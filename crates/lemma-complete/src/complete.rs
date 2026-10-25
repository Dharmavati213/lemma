//! [`Completer`]: the sources for each context and their ranking (ARCHITECTURE §5 "Completion").

use crate::db::{DbCmd, DbEnv};
use crate::request::{self, ModeCtx, Target};
use crate::tables::{self, GenCmd, GenEnv};
use crate::text::{detex, short_authors, truncate};
use crate::{CompletionDb, CompletionRequest, Insert, Item, ItemKind, Mode, WordList};
use frizbee::{CaseMatching, Config, Matcher, Matching, SortStrategy};
use lemma_snippets::{Chunk, SnippetSet, parse_body};
use lemma_syntax::{BibEntry, Label, MacroDef, ProjectIndex};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// At most this many items are proposed.
pub const MAX_ITEMS: usize = 50;

/// At most this many dictionary or project words are proposed (blink-cmp-dictionary's limit).
const MAX_WORDS: usize = 12;
/// Words are proposed for prefixes of at least this many characters.
const MIN_WORD_PREFIX: usize = 2;

// Ranking bonuses, added to the frizbee score (ARCHITECTURE §5).
const SNIPPET_BONUS: i32 = 20;
const USER_BONUS: i32 = 12;
const PROJECT_BONUS: i32 = 8;
const L1_BONUS: i32 = 4;
const WORD_PENALTY: i32 = -8;
/// Commands the word lists mark unusual (`#*`) come after the usual ones; they are not offered
/// before a letter is typed.
const UNUSUAL_PENALTY: i32 = -10;
/// `\eqref` prefers labels of equations.
const EQREF_BONUS: i32 = 4;
/// In math, commands and environments of no particular mode that belong in text (`\frame`,
/// `\footref`, `itemize`) come after the ones that belong in formulas (a two-letter prefix
/// match scores about 44, so `\fr` lists them after `\mathfrak`; a name typed in full still
/// comes first).
const TEXT_IN_MATH_PENALTY: i32 = -40;

/// How many accepted labels the recency table remembers.
const RECENCY_CAP: usize = 512;

/// Environments whose labels number equations.
const EQUATION_ENVS: &[&str] = &[
    "equation",
    "align",
    "gather",
    "multline",
    "flalign",
    "alignat",
    "eqnarray",
    "subequations",
];

/// Image extensions offered after `\includegraphics`.
const IMAGE_EXTENSIONS: &[&str] = &["pdf", "png", "jpg", "jpeg", "eps", "svg"];

/// How often and how recently a label was accepted.
#[derive(Clone, Copy, Debug)]
struct Recent {
    count: u32,
    last: u64,
}

/// Ranks proposals (frizbee matcher config and recency counts).
///
/// Holds what is remembered between requests: the labels accepted recently
/// ([`Self::record_accepted`]) and the project's files ([`Self::set_files`]).
#[derive(Clone, Debug)]
pub struct Completer {
    /// Recency of accepted commands, keyed by name without the backslash.
    commands: HashMap<Box<str>, Recent>,
    /// Recency of every other accepted label.
    others: HashMap<Box<str>, Recent>,
    tick: u64,
    /// Project files relative to the root directory, `/`-separated.
    files: Vec<String>,
}

impl Default for Completer {
    fn default() -> Self {
        Self::new()
    }
}

/// A candidate before it becomes an [`Item`].
struct Cand<'a> {
    score: i32,
    /// First tie-breaker: the `common.txt` rank (`u32::MAX` when not listed).
    rank: u32,
    /// Next tie-breaker: shorter first, then bytewise. Empty for the sources listed in document
    /// order (labels, citations, files).
    key: &'a str,
    /// Push order: the final tie-breaker, which keeps document order.
    order: u32,
    src: Src<'a>,
}

enum Src<'a> {
    Cmd(&'static GenCmd),
    UserMacro(&'a MacroDef),
    Env(&'static GenEnv),
    /// A `\newenvironment` or `\newtheorem` of the project.
    UserEnv(&'a MacroDef),
    /// An environment the project uses that no table or definition knows.
    ProjectEnv(&'a str),
    Snippet {
        trigger: &'a str,
        chunks: &'a [Chunk],
        description: Option<&'a str>,
    },
    Label {
        name: &'a str,
        def: Option<&'a Label>,
    },
    Cite(&'a BibEntry),
    File(String),
    Name {
        name: &'static str,
        detail: &'static str,
    },
    Word(&'a str),
}

/// Whether a command of `mode` may be offered where the mode is `ctx`.
fn mode_allows(mode: Mode, ctx: ModeCtx) -> bool {
    !matches!(
        (mode, ctx),
        (Mode::MathOnly, ModeCtx::Text) | (Mode::TextOnly, ModeCtx::Math)
    )
}

/// blink.cmp's fuzzy matching: smart case, one typo per four typed characters.
fn fuzzy_config(needle: &str) -> Config {
    Config {
        max_typos: Some((needle.chars().count() / 4) as u16),
        sort: SortStrategy::IndexAsc,
        ..Config::default()
    }
}

/// Case-insensitive prefix matching (blink-cmp-dictionary's `rg --ignore-case ^prefix`).
fn prefix_config() -> Config {
    Config {
        max_typos: Some(0),
        casing: CaseMatching::Ignore,
        matching: Matching::Prefix,
        sort: SortStrategy::IndexAsc,
        ..Config::default()
    }
}

/// `(index, score)` of the haystacks matching `needle`; all of them, scored 0, for an empty
/// needle.
fn matches<S: AsRef<str>>(needle: &str, haystacks: &[S], config: &Config) -> Vec<(usize, i32)> {
    if needle.is_empty() {
        return (0..haystacks.len()).map(|i| (i, 0)).collect();
    }
    if haystacks.is_empty() {
        return Vec::new();
    }
    Matcher::new(needle, config)
        .match_list(haystacks)
        .into_iter()
        .map(|m| (m.index as usize, i32::from(m.score)))
        .collect()
}

impl Completer {
    /// A completer with no recency history.
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            others: HashMap::new(),
            tick: 0,
            files: Vec::new(),
        }
    }

    /// The project's files, relative to the root directory and `/`-separated
    /// (`chapters/intro.tex`, `figures/plot.pdf`, `refs.bib`), offered in file arguments.
    ///
    /// Until it is set, the `.tex` and `.bib` files of the [`ProjectIndex`] are offered.
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = files;
    }

    /// The files [`Self::set_files`] gave (empty until it is called).
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// Clears `out` and fills it with at most [`MAX_ITEMS`] items, best first.
    ///
    /// Score = frizbee score + snippet 20 + user macro 12 + used-in-project 8 + L1 4
    /// − dictionary 8 + recency. An exact line-begin snippet trigger hides everything else.
    /// Dictionary words only in Text context, prefix >= 2 chars, max 12.
    ///
    /// The sources by context ([`CompletionRequest`] says which context to pass):
    ///
    /// - **command names** (after `\`): the loaded tables, filtered by math or text mode (with
    ///   glyphs; in math, commands of no particular mode that belong in text, such as `\frame`,
    ///   come −40 after the others), and the project's macros (with the glyph of their
    ///   definition, `\Z` → ℤ).
    ///   Commands the project uses get the used-in-project bonus (see [`WordList`]), the most
    ///   common ones (`data/completion/common.txt`) +1 to +4, and commands the word lists mark
    ///   unusual −10;
    /// - **`\begin{` and `\end{`**: environments of the tables, the project's
    ///   `\newenvironment`/`\newtheorem`s (+12) and the environments it uses (+8), filtered by
    ///   mode when it is known;
    /// - **references** (`\ref{`, `\eqref{`, `\cref{a,` …): labels in document order with their
    ///   `.aux` number, environment and title; referenced labels +8, equations +4 after `\eqref`;
    /// - **citations**: `.bib` keys with author, year and title; cited keys +8;
    /// - **files and packages**: [`Self::set_files`] by extension (`.tex` without the extension
    ///   for `\input`), and package, class or TikZ library names;
    /// - **anything else** (a word): the manual snippets whose trigger starts with the prefix
    ///   (+20) and, in text, dictionary and project words (−8).
    pub fn complete(
        &mut self,
        db: &CompletionDb,
        snippets: &SnippetSet,
        idx: &ProjectIndex,
        words: &WordList,
        req: &CompletionRequest,
        out: &mut Vec<Item>,
    ) {
        out.clear();
        let mut cands = Vec::new();
        let needle = req.prefix;
        match request::target(db, req) {
            Target::None => {}
            Target::Command(mode) => self.commands(db, idx, words, needle, mode, &mut cands),
            Target::Environment(mode) => self.environments(db, idx, needle, mode, &mut cands),
            Target::Ref(cmd) => self.labels(idx, cmd, needle, &mut cands),
            Target::Cite => self.citations(idx, needle, &mut cands),
            Target::File(cmd) => self.file_names(idx, cmd, needle, &mut cands),
            Target::Package(cmd) => self.package_names(cmd, needle, &mut cands),
            Target::Word(mode) => self.words(snippets, idx, words, req, mode, &mut cands),
        }
        select_best(&mut cands, MAX_ITEMS);
        out.extend(cands.into_iter().map(|c| c.into_item(idx)));
    }

    /// Boosts `label` in later rankings.
    ///
    /// `label` is the accepted [`Item::label`]. Each acceptance adds 2 (up to 10), and the last
    /// 8 acceptances get 4 more (the last 32: 2 more).
    pub fn record_accepted(&mut self, label: &str) {
        self.tick += 1;
        let tick = self.tick;
        let (map, key) = match label.strip_prefix('\\') {
            Some(name) => (&mut self.commands, name),
            None => (&mut self.others, label),
        };
        match map.get_mut(key) {
            Some(r) => {
                r.count = r.count.saturating_add(1);
                r.last = tick;
            }
            None => {
                map.insert(
                    Box::from(key),
                    Recent {
                        count: 1,
                        last: tick,
                    },
                );
            }
        }
        if map.len() > RECENCY_CAP
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, r)| r.last)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }

    fn recency(&self, recent: Option<&Recent>) -> i32 {
        let Some(r) = recent else {
            return 0;
        };
        let frequency = r.count.min(5) as i32 * 2;
        let age = self.tick.saturating_sub(r.last);
        let fresh = match age {
            0..8 => 4,
            8..32 => 2,
            _ => 0,
        };
        frequency + fresh
    }

    fn command_recency(&self, name: &str) -> i32 {
        self.recency(self.commands.get(name))
    }

    fn other_recency(&self, label: &str) -> i32 {
        self.recency(self.others.get(label))
    }

    fn commands<'a>(
        &self,
        db: &'a CompletionDb,
        idx: &'a ProjectIndex,
        words: &WordList,
        needle: &str,
        mode: ModeCtx,
        out: &mut Vec<Cand<'a>>,
    ) {
        let config = fuzzy_config(needle);
        let user = user_macros(idx);
        let user_names: Vec<&str> = user.iter().map(|m| m.name.as_str()).collect();
        for (i, score) in matches(needle, &user_names, &config) {
            let m = user[i];
            let macro_mode = if m.is_math_operator {
                Mode::MathOnly
            } else {
                Mode::Any
            };
            if !mode_allows(macro_mode, mode) {
                continue;
            }
            let common = tables::common_command(&m.name);
            let mut score = score + USER_BONUS + common.bonus + self.command_recency(&m.name);
            if words.uses_command(&m.name) {
                score += PROJECT_BONUS;
            }
            push_ranked(out, score, common.rank, &m.name, Src::UserMacro(m));
        }
        let user_set: HashSet<&str> = user_names.into_iter().collect();
        for (i, score) in matches(needle, &db.command_names, &config) {
            let DbCmd { cmd, builtin } = db.commands[i];
            let name = cmd.entry.name;
            if user_set.contains(name)
                || !mode_allows(cmd.entry.mode, mode)
                || (cmd.unusual && needle.is_empty())
            {
                continue;
            }
            let common = tables::common_command(name);
            let mut score = score + self.command_recency(name) + common.bonus;
            if builtin {
                score += L1_BONUS;
            }
            if cmd.unusual {
                score += UNUSUAL_PENALTY;
            }
            if cmd.text_leaning && mode == ModeCtx::Math {
                score += TEXT_IN_MATH_PENALTY;
            }
            if words.uses_command(name) {
                score += PROJECT_BONUS;
            }
            push_ranked(out, score, common.rank, name, Src::Cmd(cmd));
        }
    }

    fn environments<'a>(
        &self,
        db: &'a CompletionDb,
        idx: &'a ProjectIndex,
        needle: &str,
        mode: ModeCtx,
        out: &mut Vec<Cand<'a>>,
    ) {
        let config = fuzzy_config(needle);
        let used: HashSet<&str> = idx
            .files
            .values()
            .flat_map(|f| f.envs.iter().map(|e| e.name.as_str()))
            .collect();
        // The bonuses every source shares, and the common.txt rank.
        let shared = |name: &str| {
            let common = tables::common_environment(name);
            let used = if used.contains(name) {
                PROJECT_BONUS
            } else {
                0
            };
            (common.bonus + self.other_recency(name) + used, common.rank)
        };
        let user = user_environments(idx);
        let user_names: Vec<&str> = user.iter().map(|m| m.name.as_str()).collect();
        for (i, score) in matches(needle, &user_names, &config) {
            let m = user[i];
            // A theorem is a paragraph of text.
            let env_mode = if m.is_theorem {
                Mode::TextOnly
            } else {
                Mode::Any
            };
            if mode_allows(env_mode, mode) {
                let (bonus, rank) = shared(&m.name);
                let score = score + USER_BONUS + bonus;
                push_ranked(out, score, rank, &m.name, Src::UserEnv(m));
            }
        }
        let user_set: HashSet<&str> = user_names.into_iter().collect();
        for (i, score) in matches(needle, &db.environment_names, &config) {
            let DbEnv { env, builtin } = db.environments[i];
            let name = env.entry.name;
            if user_set.contains(name)
                || !mode_allows(env.mode, mode)
                || (env.unusual && needle.is_empty())
            {
                continue;
            }
            let (bonus, rank) = shared(name);
            let mut score = score + bonus;
            if builtin {
                score += L1_BONUS;
            }
            if env.unusual {
                score += UNUSUAL_PENALTY;
            }
            if env.text_leaning && mode == ModeCtx::Math {
                score += TEXT_IN_MATH_PENALTY;
            }
            push_ranked(out, score, rank, name, Src::Env(env));
        }
        let unknown: Vec<&str> = used
            .iter()
            .copied()
            .filter(|n| !user_set.contains(n) && db.environment(n).is_none())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for (i, score) in matches(needle, &unknown, &config) {
            let name = unknown[i];
            let (bonus, rank) = shared(name);
            push_ranked(out, score + bonus, rank, name, Src::ProjectEnv(name));
        }
    }

    fn labels<'a>(&self, idx: &'a ProjectIndex, cmd: &str, needle: &str, out: &mut Vec<Cand<'a>>) {
        let mut seen = HashSet::new();
        let mut names: Vec<&str> = Vec::new();
        let mut defs: Vec<Option<&Label>> = Vec::new();
        for (_, label) in idx.all_labels() {
            if seen.insert(label.name.as_str()) {
                names.push(&label.name);
                defs.push(Some(label));
            }
        }
        // Labels known only from the last build (files that are not indexed).
        for name in idx.aux.labels.keys() {
            if seen.insert(name.as_str()) {
                names.push(name);
                defs.push(None);
            }
        }
        let referenced: HashSet<&str> = idx
            .files
            .values()
            .flat_map(|f| {
                f.refs
                    .iter()
                    .flat_map(|r| r.names.iter().map(String::as_str))
            })
            .collect();
        let eqref = cmd.trim_end_matches('*').eq_ignore_ascii_case("eqref");
        for (i, score) in matches(needle, &names, &fuzzy_config(needle)) {
            let (name, def) = (names[i], defs[i]);
            let mut score = score + self.other_recency(name);
            if referenced.contains(name) {
                score += PROJECT_BONUS;
            }
            if eqref && def.is_some_and(|d| is_equation_env(d.env.as_deref())) {
                score += EQREF_BONUS;
            }
            push(out, score, "", Src::Label { name, def });
        }
    }

    fn citations<'a>(&self, idx: &'a ProjectIndex, needle: &str, out: &mut Vec<Cand<'a>>) {
        let mut seen = HashSet::new();
        let entries: Vec<&BibEntry> = idx
            .all_bib()
            .into_iter()
            .filter(|e| seen.insert(e.key.as_str()))
            .collect();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        let cited: HashSet<&str> = idx
            .files
            .values()
            .flat_map(|f| {
                f.cites
                    .iter()
                    .flat_map(|c| c.keys.iter().map(String::as_str))
            })
            .collect();
        for (i, score) in matches(needle, &keys, &fuzzy_config(needle)) {
            let mut score = score + self.other_recency(keys[i]);
            if cited.contains(keys[i]) {
                score += PROJECT_BONUS;
            }
            push(out, score, "", Src::Cite(entries[i]));
        }
    }

    fn file_names<'a>(&self, idx: &ProjectIndex, cmd: &str, needle: &str, out: &mut Vec<Cand<'a>>) {
        let derived;
        let files: &[String] = if self.files.is_empty() {
            derived = index_files(idx);
            &derived
        } else {
            &self.files
        };
        let mut seen = HashSet::new();
        let inserts: Vec<String> = files
            .iter()
            .filter_map(|f| file_insert(cmd, f))
            .filter(|f| seen.insert(f.clone()))
            .collect();
        for (i, score) in matches(needle, &inserts, &fuzzy_config(needle)) {
            let score = score + self.other_recency(&inserts[i]);
            push(out, score, "", Src::File(inserts[i].clone()));
        }
    }

    fn package_names<'a>(&self, cmd: &str, needle: &str, out: &mut Vec<Cand<'a>>) {
        let (list, detail): (&'static [&'static str], &'static str) = match cmd {
            "documentclass" | "LoadClass" => (tables::class_names(), "class"),
            "usetikzlibrary" => (tables::tikz_libraries(), "TikZ library"),
            "usepgfplotslibrary" | "tcbuselibrary" => (&[], ""),
            _ => (tables::package_names(), "package"),
        };
        for (i, score) in matches(needle, list, &fuzzy_config(needle)) {
            let name = list[i];
            let score = score + self.other_recency(name);
            push(out, score, name, Src::Name { name, detail });
        }
    }

    fn words<'a>(
        &self,
        snippets: &'a SnippetSet,
        idx: &ProjectIndex,
        words: &'a WordList,
        req: &CompletionRequest,
        mode: ModeCtx,
        out: &mut Vec<Cand<'a>>,
    ) {
        let prefix = req.prefix;
        let manual = snippets.manual(prefix, req.line_before, req.ctx);
        let chunks_of = |m: &lemma_snippets::SnippetMatch<'a>| -> &'a [Chunk] {
            match (m.def.required_macro(), m.alt_chunks) {
                (Some(name), Some(alt)) if !macro_defined(idx, name) => alt,
                _ => m.chunks,
            }
        };
        let snippet = |m: &lemma_snippets::SnippetMatch<'a>| Src::Snippet {
            trigger: &m.def.trigger,
            chunks: chunks_of(m),
            description: m.def.description.as_deref(),
        };
        // The user's blink rule: an exact line-begin trigger hides everything else.
        if snippets.is_exact_line_begin_trigger(prefix, req.line_before) {
            let exact: Vec<_> = manual
                .iter()
                .filter(|m| m.def.line_begin && m.def.trigger == prefix)
                .collect();
            if !exact.is_empty() {
                for m in exact {
                    let score = SNIPPET_BONUS + self.other_recency(&m.def.trigger);
                    push(out, score, &m.def.trigger, snippet(m));
                }
                return;
            }
        }
        let triggers: Vec<&str> = manual.iter().map(|m| m.def.trigger.as_str()).collect();
        let scores = matches(prefix, &triggers, &prefix_config());
        for (i, score) in scores {
            let m = &manual[i];
            let score = score + SNIPPET_BONUS + self.other_recency(&m.def.trigger);
            push(out, score, &m.def.trigger, snippet(m));
        }
        if mode != ModeCtx::Text || prefix.chars().count() < MIN_WORD_PREFIX {
            return;
        }
        let list = words.words();
        let mut found: Vec<Cand<'a>> = Vec::new();
        for (i, score) in matches(prefix, list, &prefix_config()) {
            let word: &'a str = &list[i];
            if word == prefix {
                continue;
            }
            let score = score + WORD_PENALTY + self.other_recency(word);
            push(&mut found, score, word, Src::Word(word));
        }
        select_best(&mut found, MAX_WORDS);
        let base = out.len() as u32;
        out.extend(found.into_iter().enumerate().map(|(i, mut c)| {
            c.order = base + i as u32;
            c
        }));
    }
}

fn push<'a>(out: &mut Vec<Cand<'a>>, score: i32, key: &'a str, src: Src<'a>) {
    push_ranked(out, score, u32::MAX, key, src);
}

fn push_ranked<'a>(out: &mut Vec<Cand<'a>>, score: i32, rank: u32, key: &'a str, src: Src<'a>) {
    let order = out.len() as u32;
    out.push(Cand {
        score,
        rank,
        key,
        order,
        src,
    });
}

/// Best first: higher score, then more common, then shorter key, then key bytewise, then push
/// order.
fn rank(a: &Cand, b: &Cand) -> Ordering {
    (Reverse(a.score), a.rank, a.key.len(), a.key, a.order).cmp(&(
        Reverse(b.score),
        b.rank,
        b.key.len(),
        b.key,
        b.order,
    ))
}

/// Keeps the best `n` candidates, sorted.
fn select_best(cands: &mut Vec<Cand>, n: usize) {
    if cands.len() > n {
        cands.select_nth_unstable_by(n - 1, rank);
        cands.truncate(n);
    }
    cands.sort_unstable_by(rank);
}

/// The project's macros (not environments or theorems), first definition of each name.
fn user_macros(idx: &ProjectIndex) -> Vec<&MacroDef> {
    let mut seen = HashSet::new();
    idx.files
        .values()
        .flat_map(|f| f.macros.iter())
        .filter(|m| !m.is_env && !m.is_theorem && !m.name.is_empty())
        .filter(|m| seen.insert(m.name.as_str()))
        .collect()
}

/// The project's environments and theorems, first definition of each name.
fn user_environments(idx: &ProjectIndex) -> Vec<&MacroDef> {
    let mut seen = HashSet::new();
    idx.files
        .values()
        .flat_map(|f| f.macros.iter())
        .filter(|m| (m.is_env || m.is_theorem) && !m.name.is_empty())
        .filter(|m| seen.insert(m.name.as_str()))
        .collect()
}

/// Whether the project defines macro `name` (for a snippet's `if_undefined` body).
fn macro_defined(idx: &ProjectIndex, name: &str) -> bool {
    idx.files.values().any(|f| {
        f.macros
            .iter()
            .any(|m| !m.is_env && !m.is_theorem && m.name == name)
    })
}

fn is_equation_env(env: Option<&str>) -> bool {
    env.is_some_and(|e| EQUATION_ENVS.contains(&e.trim_end_matches('*')))
}

/// The `.tex` and `.bib` files of the index, relative to their common directory.
fn index_files(idx: &ProjectIndex) -> Vec<String> {
    let paths: Vec<&Path> = idx
        .files
        .keys()
        .chain(idx.bibs.keys())
        .map(PathBuf::as_path)
        .collect();
    let Some(first) = paths.first() else {
        return Vec::new();
    };
    let mut base = first.parent().unwrap_or(Path::new("")).to_path_buf();
    for p in &paths[1..] {
        while !p.starts_with(&base) {
            if !base.pop() {
                break;
            }
        }
    }
    let mut out: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(&base).ok())
        .filter_map(Path::to_str)
        .map(|s| s.replace('\\', "/"))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What a file argument of `cmd` inserts for `file` (a `/`-separated path relative to the
/// root), or `None` when the file does not fit the command.
fn file_insert(cmd: &str, file: &str) -> Option<String> {
    let (stem, ext) = match file.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') && !stem.is_empty() => {
            (stem, ext.to_ascii_lowercase())
        }
        _ => (file, String::new()),
    };
    let cmd = cmd.trim_end_matches('*');
    match cmd {
        "input" | "include" | "includeonly" | "subfile" | "subfileinclude" | "includefrom"
        | "inputfrom" | "subincludefrom" | "subinputfrom" | "externaldocument"
        | "includestandalone" => (ext == "tex").then(|| stem.to_owned()),
        "includegraphics" => IMAGE_EXTENSIONS
            .contains(&ext.as_str())
            .then(|| file.to_owned()),
        "includepdf" => (ext == "pdf").then(|| file.to_owned()),
        "includesvg" => (ext == "svg").then(|| stem.to_owned()),
        "bibliography" => (ext == "bib").then(|| stem.to_owned()),
        "addbibresource" | "addglobalbib" => (ext == "bib").then(|| file.to_owned()),
        _ => Some(file.to_owned()),
    }
}

impl Cand<'_> {
    fn into_item(self, idx: &ProjectIndex) -> Item {
        let score = self.score;
        let item = |label: String, insert: Insert, kind, detail, glyph| Item {
            label,
            insert,
            kind,
            detail,
            glyph,
            score,
        };
        match self.src {
            Src::Cmd(cmd) => {
                let e = &cmd.entry;
                let package = tables::package_label(e.package);
                let detail = if cmd.sig.is_empty() {
                    package.to_owned()
                } else {
                    format!("{} · {package}", cmd.sig)
                };
                item(
                    format!("\\{}", e.name),
                    insert_of(e.snippet),
                    ItemKind::Command,
                    Some(detail),
                    e.glyph,
                )
            }
            Src::UserMacro(m) => {
                // Only the mandatory arguments, in braces: an optional one left out takes its
                // default (`\norm{x}` for `\newcommand{\norm}[2][]{…}`), and a braced group in
                // its place would be taken for the next argument.
                let mut body = format!("\\{}", m.name);
                for n in 1..=m.nargs.saturating_sub(m.optional_args).min(9) {
                    body.push_str(&format!("{{${n}}}"));
                }
                let glyph = (m.nargs == 0)
                    .then(|| m.body.as_deref().and_then(crate::glyph_for))
                    .flatten();
                let detail = match &m.body {
                    Some(b) if !b.is_empty() => truncate(b, 40),
                    _ => "user macro".to_owned(),
                };
                item(
                    format!("\\{}", m.name),
                    insert_of(&body),
                    ItemKind::UserMacro,
                    Some(detail),
                    glyph,
                )
            }
            Src::Env(env) => item(
                env.entry.name.to_owned(),
                Insert::Text(env.entry.name.to_owned()),
                ItemKind::Environment,
                Some(tables::package_label(env.entry.package).to_owned()),
                None,
            ),
            Src::UserEnv(m) => {
                let detail = if m.is_theorem {
                    match m.body.as_deref().map(detex) {
                        Some(title) if !title.is_empty() => format!("theorem: {title}"),
                        _ => "theorem".to_owned(),
                    }
                } else {
                    "user environment".to_owned()
                };
                item(
                    m.name.clone(),
                    Insert::Text(m.name.clone()),
                    ItemKind::Environment,
                    Some(detail),
                    None,
                )
            }
            Src::ProjectEnv(name) => item(
                name.to_owned(),
                Insert::Text(name.to_owned()),
                ItemKind::Environment,
                Some("used in the project".to_owned()),
                None,
            ),
            Src::Snippet {
                trigger,
                chunks,
                description,
            } => item(
                trigger.to_owned(),
                Insert::Snippet(chunks.to_vec()),
                ItemKind::Snippet,
                description.map(str::to_owned),
                None,
            ),
            Src::Label { name, def } => item(
                name.to_owned(),
                Insert::Text(name.to_owned()),
                ItemKind::Label,
                label_detail(idx, name, def),
                None,
            ),
            Src::Cite(entry) => item(
                entry.key.clone(),
                Insert::Text(entry.key.clone()),
                ItemKind::Citation,
                cite_detail(entry),
                None,
            ),
            Src::File(path) => item(path.clone(), Insert::Text(path), ItemKind::File, None, None),
            Src::Name { name, detail } => item(
                name.to_owned(),
                Insert::Text(name.to_owned()),
                ItemKind::Package,
                (!detail.is_empty()).then(|| detail.to_owned()),
                None,
            ),
            Src::Word(word) => item(
                word.to_owned(),
                Insert::Text(word.to_owned()),
                ItemKind::Word,
                None,
                None,
            ),
        }
    }
}

/// A snippet body as an [`Insert`]: plain text when it has no tab stops.
fn insert_of(body: &str) -> Insert {
    if !body.contains('$') {
        return Insert::Text(body.to_owned());
    }
    match parse_body(body) {
        Ok(chunks) => Insert::Snippet(chunks),
        Err(_) => Insert::Text(body.to_owned()),
    }
}

/// `3.2 · theorem · Main result`, `(1) · equation`: the `.aux` number (in parentheses for
/// equations), the environment and the title.
fn label_detail(idx: &ProjectIndex, name: &str, def: Option<&Label>) -> Option<String> {
    let env = def.and_then(|d| d.env.as_deref());
    let mut parts = Vec::new();
    if let Some((number, _)) = idx.aux.labels.get(name)
        && !number.is_empty()
    {
        parts.push(if is_equation_env(env) {
            format!("({number})")
        } else {
            number.clone()
        });
    }
    if let Some(env) = env {
        parts.push(env.to_owned());
    }
    if let Some(title) = def.and_then(|d| d.title.as_deref()) {
        let title = detex(title);
        if !title.is_empty() {
            parts.push(truncate(&title, 40));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `Mac Lane 1998 · Categories for the Working Mathematician`.
fn cite_detail(e: &BibEntry) -> Option<String> {
    let authors = e.author.as_deref().map(short_authors).unwrap_or_default();
    let head = [authors.as_str(), e.year.as_deref().unwrap_or("")]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let title = e
        .title
        .as_deref()
        .map(|t| truncate(&detex(t), 60))
        .unwrap_or_default();
    let parts: Vec<&str> = [head.as_str(), title.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_inserts() {
        assert_eq!(
            file_insert("input", "chapters/a.tex").as_deref(),
            Some("chapters/a")
        );
        assert_eq!(file_insert("include", "refs.bib"), None);
        assert_eq!(
            file_insert("includegraphics", "fig/plot.PDF").as_deref(),
            Some("fig/plot.PDF")
        );
        assert_eq!(file_insert("includegraphics", "main.tex"), None);
        assert_eq!(
            file_insert("bibliography", "refs.bib").as_deref(),
            Some("refs")
        );
        assert_eq!(
            file_insert("addbibresource", "refs.bib").as_deref(),
            Some("refs.bib")
        );
        assert_eq!(
            file_insert("lstinputlisting", "src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(file_insert("input", "Makefile"), None);
        assert_eq!(
            file_insert("input", ".hidden.tex").as_deref(),
            Some(".hidden")
        );
    }

    #[test]
    fn recency_rewards_frequency_and_freshness() {
        let mut c = Completer::new();
        assert_eq!(c.command_recency("frac"), 0);
        c.record_accepted(r"\frac");
        assert_eq!(c.command_recency("frac"), 2 + 4);
        c.record_accepted(r"\frac");
        assert_eq!(c.command_recency("frac"), 4 + 4);
        for _ in 0..40 {
            c.record_accepted("other");
        }
        assert_eq!(c.command_recency("frac"), 4);
        assert_eq!(c.other_recency("other"), 10 + 4);
        assert_eq!(c.other_recency("frac"), 0);
    }

    #[test]
    fn recency_table_is_capped() {
        let mut c = Completer::new();
        for i in 0..RECENCY_CAP + 10 {
            c.record_accepted(&format!("label{i}"));
        }
        assert_eq!(c.others.len(), RECENCY_CAP);
        assert!(!c.others.contains_key("label0"));
        assert!(
            c.others
                .contains_key(format!("label{}", RECENCY_CAP + 9).as_str())
        );
    }
}
