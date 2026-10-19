//! The pure parts of the label tools (no GTK, no I/O): where a label or citation key occurs in a
//! text, which references and citations of a project are undefined and which labels and
//! bibliography entries nobody uses, what a label is called ("Theorem 3.2", "(1)"), where a file
//! argument may point, and bibliography fields as plain text.

use lemma_core::{LineIndex, TextEdit};
use lemma_syntax::{
    BibEntry, FileIndex, IncludeKind, Label, LineStates, MacroDef, ProjectIndex, ScanConfig,
    StrLines, Token, TokenKind,
};
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// What a name in an argument stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NameKind {
    /// A label: defined by `\label`, used by `\ref` and its relatives.
    Label,
    /// A citation key: defined by a bibliography entry, used by `\cite` and its relatives.
    Cite,
}

impl NameKind {
    /// Whether a token of `kind` holds names of this kind.
    pub fn matches(self, kind: TokenKind) -> bool {
        match self {
            Self::Label => matches!(kind, TokenKind::RefArg | TokenKind::LabelArg),
            Self::Cite => kind == TokenKind::CiteArg,
        }
    }
}

/// One place a name occurs in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// The name's bytes in the text.
    pub range: Range<usize>,
    /// Its 0-based line.
    pub line: u32,
    /// It is the defining `\label{…}`, not a reference.
    pub definition: bool,
}

/// The names of a name-list token of `line` (`a, b` in `\cref{a, b}`): their byte ranges in the
/// line and their text, blanks trimmed, empty items left out.
pub fn token_items<'a>(line: &'a str, token: &Token) -> Vec<(Range<usize>, &'a str)> {
    let (start, end) = (token.start as usize, (token.end as usize).min(line.len()));
    let Some(text) = line.get(start..end) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = start;
    for raw in text.split(',') {
        let lead = raw.len() - raw.trim_start().len();
        let name = raw.trim();
        if !name.is_empty() {
            out.push((at + lead..at + lead + name.len(), name));
        }
        at += raw.len() + 1;
    }
    out
}

/// Whether the byte before `at` (blanks aside) can open a name-list item.
fn opens_item(text: &str, at: usize) -> bool {
    text[..at]
        .bytes()
        .rev()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'{' | b',' | b'['))
}

/// Whether the byte at `at` (blanks aside) can close a name-list item.
fn closes_item(text: &str, at: usize) -> bool {
    text[at..]
        .bytes()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'}' | b',' | b']'))
}

/// The places where `name` is a whole item of an argument of `kind` in `text`: `tokens_of(line,
/// line_text, out)` gives the scanner tokens of a line (the line's text without its `\n`).
/// Comments, verbatim and other arguments are skipped because their tokens are not name lists.
pub fn find_occurrences(
    text: &str,
    index: &LineIndex,
    mut tokens_of: impl FnMut(u32, &str, &mut Vec<Token>),
    name: &str,
    kind: NameKind,
) -> Vec<Occurrence> {
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    let mut tokens = Vec::new();
    let mut tokens_line = None;
    for (hit, _) in text.match_indices(name) {
        let end = hit + name.len();
        if !opens_item(text, hit) || !closes_item(text, end) {
            continue;
        }
        let line = index.line_of(hit);
        let range = index.line_range(line);
        // A name never spans lines.
        if end > range.end {
            continue;
        }
        let line_text = &text[range.clone()];
        if tokens_line != Some(line) {
            tokens.clear();
            tokens_of(line, line_text, &mut tokens);
            tokens_line = Some(line);
        }
        let (col, col_end) = (hit - range.start, end - range.start);
        let Some(token) = tokens
            .iter()
            .find(|t| t.start as usize <= col && col_end <= t.end as usize)
        else {
            continue;
        };
        if !kind.matches(token.kind) {
            continue;
        }
        if token_items(line_text, token)
            .iter()
            .any(|(r, _)| r.start == col && r.end == col_end)
        {
            out.push(Occurrence {
                range: hit..end,
                line,
                definition: token.kind == TokenKind::LabelArg,
            });
        }
    }
    out
}

/// [`find_occurrences`] in a text without scanner states: they are built only when the text
/// holds `name` at a plausible place (for files that are not open; run it off the main thread).
pub fn scan_occurrences(
    text: &str,
    config: Arc<ScanConfig>,
    name: &str,
    kind: NameKind,
) -> Vec<Occurrence> {
    if name.is_empty()
        || !text
            .match_indices(name)
            .any(|(hit, _)| opens_item(text, hit) && closes_item(text, hit + name.len()))
    {
        return Vec::new();
    }
    let index = LineIndex::new(text);
    let mut states = LineStates::new(config);
    states.rebuild(&StrLines {
        text,
        index: &index,
    });
    find_occurrences(
        text,
        &index,
        |line, line_text, out| states.tokens(line, line_text, out),
        name,
        kind,
    )
}

/// The edits that rename every occurrence in `occurrences` to `new_name`, in descending order
/// (as `Document::apply_edits` takes them).
pub fn rename_edits(occurrences: &[Occurrence], new_name: &str) -> Vec<TextEdit> {
    let mut ranges: Vec<Range<usize>> = occurrences.iter().map(|o| o.range.clone()).collect();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.start));
    ranges.dedup();
    ranges
        .into_iter()
        .map(|range| TextEdit {
            range,
            text: new_name.to_owned(),
        })
        .collect()
}

/// Why `name` cannot be a label (`None`: it can). Characters that end an argument or a list
/// item, start a command or comment, or that TeX treats specially in a `\label` are refused,
/// and so is any blank.
pub fn label_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("Enter a name");
    }
    if name.chars().any(char::is_whitespace) {
        return Some("A label cannot contain spaces");
    }
    if let Some(c) = name.chars().find(|c| {
        matches!(
            c,
            '{' | '}' | '\\' | '%' | '#' | ',' | '~' | '$' | '^' | '&'
        )
    }) {
        return Some(match c {
            ',' => "A label cannot contain a comma",
            '{' | '}' => "A label cannot contain braces",
            '\\' => "A label cannot contain a backslash",
            _ => "A label cannot contain this character",
        });
    }
    None
}

/// Whether a name read from an argument is a literal name (not built by a macro or a macro
/// parameter), so that it can be checked against the project's definitions.
fn is_literal(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\\', '#', '{', '}'])
}

// ----- project checks -----

/// An undefined or unused name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The file it is in (the `.bib` file for an entry).
    pub path: PathBuf,
    /// Its 0-based line.
    pub line: u32,
    pub name: String,
    pub kind: NameKind,
}

/// The undefined references and citations of a project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Undefined {
    /// By path, then line, then name.
    pub findings: Vec<Finding>,
    /// Citations were checked: the project names bibliographies and every one was read (else a
    /// key may live in a file Lemma could not read, and none is reported).
    pub cites_checked: bool,
}

/// `dir` joined with `rel`, with `.` and `..` folded without touching the file system (as the
/// project index names bibliographies).
pub fn lexical_join(dir: &Path, rel: impl AsRef<Path>) -> PathBuf {
    let mut out = PathBuf::new();
    for component in dir.join(rel).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `.bib` files `file` (at `path`) names with `\bibliography`/`\addbibresource`, as the
/// project index keys them.
pub fn bibliographies(path: &Path, file: &FileIndex) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    file.includes
        .iter()
        .filter(|i| i.kind == IncludeKind::Bibliography)
        .map(|i| i.target.trim())
        .filter(|target| !target.is_empty())
        .map(|target| {
            let bib = lexical_join(dir, target);
            if bib
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("bib"))
            {
                bib
            } else {
                PathBuf::from(format!("{}.bib", bib.display()))
            }
        })
        .collect()
}

/// Whether citations of `index` can be checked: it names at least one bibliography and has
/// read every one of them.
pub fn cites_checkable(index: &ProjectIndex) -> bool {
    let named: Vec<PathBuf> = index
        .files
        .iter()
        .flat_map(|(path, file)| bibliographies(path, file))
        .collect();
    !named.is_empty() && named.iter().all(|bib| index.bibs.contains_key(bib))
}

/// The references to labels that neither the project nor the `.aux` file define, and (when
/// [`cites_checkable`]) the citations of keys no bibliography entry has.
pub fn undefined(index: &ProjectIndex) -> Undefined {
    let labels: HashSet<&str> = index
        .files
        .values()
        .flat_map(|f| f.labels.iter().map(|l| l.name.as_str()))
        .chain(index.aux.labels.keys().map(String::as_str))
        .collect();
    let cites_checked = cites_checkable(index);
    let keys: HashSet<&str> = if cites_checked {
        index
            .bibs
            .values()
            .flat_map(|entries| entries.iter().map(|e| e.key.as_str()))
            .collect()
    } else {
        HashSet::new()
    };
    let mut findings = Vec::new();
    for (path, file) in &index.files {
        for reference in &file.refs {
            for name in &reference.names {
                let name = name.trim();
                if is_literal(name) && !labels.contains(name) {
                    findings.push(Finding {
                        path: path.clone(),
                        line: reference.line,
                        name: name.to_owned(),
                        kind: NameKind::Label,
                    });
                }
            }
        }
        if cites_checked {
            for citation in &file.cites {
                for key in &citation.keys {
                    let key = key.trim();
                    if is_literal(key) && key != "*" && !keys.contains(key) {
                        findings.push(Finding {
                            path: path.clone(),
                            line: citation.line,
                            name: key.to_owned(),
                            kind: NameKind::Cite,
                        });
                    }
                }
            }
        }
    }
    findings.sort();
    findings.dedup();
    Undefined {
        findings,
        cites_checked,
    }
}

/// The labels no reference of the project uses, by path and line.
pub fn unused_labels(index: &ProjectIndex) -> Vec<Finding> {
    let used: HashSet<&str> = index
        .files
        .values()
        .flat_map(|f| f.refs.iter().flat_map(|r| r.names.iter().map(|n| n.trim())))
        .collect();
    let mut out: Vec<Finding> = index
        .files
        .iter()
        .flat_map(|(path, file)| file.labels.iter().map(move |label| (path, label)))
        .filter(|(_, label)| !used.contains(label.name.as_str()))
        .map(|(path, label)| Finding {
            path: path.clone(),
            line: label.line,
            name: label.name.clone(),
            kind: NameKind::Label,
        })
        .collect();
    out.sort();
    out
}

/// The bibliography entries no citation of the project uses (none when `cite_all`: the project
/// has `\nocite{*}`), by `.bib` path and line.
pub fn unused_entries(index: &ProjectIndex, cite_all: bool) -> Vec<Finding> {
    if cite_all {
        return Vec::new();
    }
    let cited: HashSet<&str> = index
        .files
        .values()
        .flat_map(|f| f.cites.iter().flat_map(|c| c.keys.iter().map(|k| k.trim())))
        .collect();
    let mut out: Vec<Finding> = index
        .bibs
        .iter()
        .flat_map(|(path, entries)| entries.iter().map(move |entry| (path, entry)))
        .filter(|(_, entry)| !cited.contains(entry.key.as_str()))
        .map(|(path, entry)| Finding {
            path: path.clone(),
            line: entry.line,
            name: entry.key.clone(),
            kind: NameKind::Cite,
        })
        .collect();
    out.sort();
    out
}

/// Whether `text` holds a `\nocite{*}` (which cites every entry of the bibliography).
pub fn cites_everything(text: &str) -> bool {
    text.match_indices("\\nocite").any(|(at, word)| {
        let rest = text[at + word.len()..].trim_start();
        rest.strip_prefix('{')
            .map(str::trim_start)
            .is_some_and(|r| r.starts_with('*'))
    })
}

/// The Levenshtein distance between `a` and `b` (in characters).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

// ----- what a label is called -----

/// What the project knows about a label: what it labels and its number and page from the
/// `.aux` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelInfo {
    /// "Theorem", "Equation", "Section", "Figure", …
    pub kind: String,
    /// From the last build.
    pub number: Option<String>,
    pub page: Option<String>,
    /// An equation: its number is written in parentheses.
    pub equation: bool,
}

impl LabelInfo {
    /// "Theorem 3.2", "Equation (1)", or the kind alone before a build.
    pub fn title(&self) -> String {
        match &self.number {
            Some(n) if self.equation => format!("{} ({n})", self.kind),
            Some(n) => format!("{} {n}", self.kind),
            None => self.kind.clone(),
        }
    }

    /// What the inline equation numbers show at the end of the label's line: "(1)" for an
    /// equation, "§4" for a section, the bare number ("3.2") for the rest — the line itself
    /// says what it is (`\begin{theorem}…`), and the number sits where space is short;
    /// `None` before a build.
    pub fn annotation(&self) -> Option<String> {
        let number = self.number.as_deref().filter(|n| !n.is_empty())?;
        Some(if self.equation {
            format!("({number})")
        } else if matches!(self.kind.as_str(), "Section" | "Paragraph") {
            format!("§{number}")
        } else {
            number.to_owned()
        })
    }
}

/// Environments whose labels are equation numbers (starred forms are not numbered, but a
/// `\tag` gives them one).
const EQUATION_ENVS: &[&str] = &[
    "equation",
    "align",
    "alignat",
    "flalign",
    "gather",
    "multline",
    "eqnarray",
    "xalignat",
    "xxalignat",
    "dmath",
    "dgroup",
    "subequations",
    "IEEEeqnarray",
];

/// Theorem-like environments LaTeX users commonly define, with their usual titles (for labels
/// whose `\newtheorem` the index does not know).
const COMMON_THEOREMS: &[(&str, &str)] = &[
    ("thm", "Theorem"),
    ("lem", "Lemma"),
    ("prop", "Proposition"),
    ("cor", "Corollary"),
    ("defn", "Definition"),
    ("def", "Definition"),
    ("rem", "Remark"),
    ("ex", "Example"),
    ("conj", "Conjecture"),
];

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The title of a theorem-like environment `env` from its `\newtheorem` (`theorems`, by name).
fn theorem_title(env: &str, theorems: &BTreeMap<&str, &MacroDef>) -> Option<String> {
    let def = theorems.get(env)?;
    let title = def.body.as_deref().map(detex).unwrap_or_default();
    Some(if title.trim().is_empty() {
        capitalized(env)
    } else {
        title.trim().to_owned()
    })
}

/// The kind of thing `label` (in `file`) labels: from its environment, else the section it is
/// in.
pub fn label_kind(index: &ProjectIndex, file: &FileIndex, label: &Label) -> (String, bool) {
    let Some(env) = label.env.as_deref() else {
        let level = file
            .outline
            .iter()
            .rfind(|o| o.level < 7 && o.line <= label.line)
            .map(|o| o.level);
        let kind = match level {
            Some(0) => "Part",
            Some(1) => "Chapter",
            Some(2..=4) => "Section",
            Some(_) => "Paragraph",
            None => "Label",
        };
        return (kind.to_owned(), false);
    };
    let base = env.strip_suffix('*').unwrap_or(env);
    if EQUATION_ENVS.contains(&base) {
        return ("Equation".to_owned(), true);
    }
    let named = match base {
        "figure" | "subfigure" | "wrapfigure" | "SCfigure" => Some("Figure"),
        "table" | "subtable" | "wraptable" | "longtable" => Some("Table"),
        "enumerate" => Some("Item"),
        "algorithm" => Some("Algorithm"),
        "lstlisting" | "listing" => Some("Listing"),
        "footnote" => Some("Footnote"),
        _ => None,
    };
    if let Some(named) = named {
        return (named.to_owned(), false);
    }
    let theorems: BTreeMap<&str, &MacroDef> = index
        .files
        .values()
        .flat_map(|f| f.macros.iter().filter(|m| m.is_theorem))
        .map(|m| (m.name.as_str(), m))
        .collect();
    let in_file = file
        .macros
        .iter()
        .find(|m| m.is_theorem && m.name == base)
        .and_then(|m| {
            let title = m.body.as_deref().map(detex).unwrap_or_default();
            (!title.trim().is_empty()).then(|| title.trim().to_owned())
        });
    let kind = in_file
        .or_else(|| theorem_title(base, &theorems))
        .or_else(|| {
            COMMON_THEOREMS
                .iter()
                .find(|(n, _)| *n == base)
                .map(|(_, t)| (*t).to_owned())
        })
        .unwrap_or_else(|| capitalized(base));
    (kind, false)
}

/// What the project knows about `label` (defined in `file`).
pub fn label_info(index: &ProjectIndex, file: &FileIndex, label: &Label) -> LabelInfo {
    let (kind, equation) = label_kind(index, file, label);
    let (number, page) = match index.aux.labels.get(&label.name) {
        Some((number, page)) => (
            Some(detex(number)).filter(|n| !n.is_empty()),
            Some(page.clone()).filter(|p| !p.is_empty()),
        ),
        None => (None, None),
    };
    LabelInfo {
        kind,
        number,
        page,
        equation,
    }
}

/// The definition of `name` (a label) in the project: its file, its index entry and the file's
/// index (the first by path when it is defined twice).
pub fn find_label<'a>(
    index: &'a ProjectIndex,
    name: &str,
) -> Option<(&'a Path, &'a FileIndex, &'a Label)> {
    index.files.iter().find_map(|(path, file)| {
        file.labels
            .iter()
            .find(|l| l.name == name)
            .map(|l| (path.as_path(), &**file, l))
    })
}

/// The bibliography entry of `key`, with its `.bib` file.
pub fn find_entry<'a>(index: &'a ProjectIndex, key: &str) -> Option<(&'a Path, &'a BibEntry)> {
    index.bibs.iter().find_map(|(path, entries)| {
        entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| (path.as_path(), e))
    })
}

/// The definition of the user macro `\name` (a command, not an environment): the first in the
/// project by path, preferring the files that define it with `\newcommand` over `\renewcommand`
/// order (the index keeps them in file order).
pub fn find_macro<'a>(index: &'a ProjectIndex, name: &str) -> Option<(&'a Path, &'a MacroDef)> {
    index.files.iter().find_map(|(path, file)| {
        file.macros
            .iter()
            .find(|m| !m.is_env && !m.is_theorem && m.name == name)
            .map(|m| (path.as_path(), m))
    })
}

/// The 0-based lines of the environment around `label` (from its `\begin`, at most `max`
/// lines, not past its `\end`), else the label's own line and the lines after it.
pub fn label_lines(file: &FileIndex, label: &Label, max: u32) -> Range<u32> {
    let env = label.env.as_deref().and_then(|name| {
        file.envs
            .iter()
            .filter(|e| {
                e.name == name
                    && e.begin_line <= label.line
                    && e.end_line.is_none_or(|end| label.line <= end)
            })
            .max_by_key(|e| e.begin_line)
    });
    match env {
        Some(env) => {
            let end = env.end_line.unwrap_or(env.begin_line + max - 1);
            env.begin_line..(end + 1).min(env.begin_line + max)
        }
        None => label.line..label.line + max,
    }
}

/// The 0-based lines of the environment around `label` that typeset it: for an equation-like
/// environment its body (from the line after `\begin` to the line before `\end`, at most
/// `max` lines), else what [`label_lines`] gives. The PDF crop of a reference hover looks these
/// lines up with SyncTeX (the line of `\begin{equation}` belongs to the paragraph before).
pub fn typeset_lines(file: &FileIndex, label: &Label, equation: bool, max: u32) -> Range<u32> {
    let lines = label_lines(file, label, max);
    if !equation {
        return label_lines(file, label, 3);
    }
    let env = label.env.as_deref().and_then(|name| {
        file.envs
            .iter()
            .filter(|e| {
                e.name == name
                    && e.begin_line <= label.line
                    && e.end_line.is_none_or(|end| label.line <= end)
            })
            .max_by_key(|e| e.begin_line)
    });
    match env.and_then(|e| Some((e.begin_line, e.end_line?))) {
        Some((begin, end)) if end > begin + 1 => begin + 1..end.min(begin + 1 + max),
        _ => lines,
    }
}

// ----- files -----

/// What kind of file a file or package argument of `cmd` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A TeX source (`\input`, `\include`, `\subfile`, `\import`).
    Tex,
    /// An image (`\includegraphics`, …): opened in another application.
    Graphics,
    /// A bibliography.
    Bib,
    /// A package (`\usepackage`): a local `.sty`.
    Package,
    /// A class (`\documentclass`): a local `.cls`.
    Class,
    /// Any other file argument, as written (`\lstinputlisting`, `\verbatiminput`).
    Other,
}

/// The kind of file argument `cmd` (without the backslash) takes; `None` for arguments that
/// are not files to go to.
pub fn file_kind(cmd: &str) -> Option<FileKind> {
    Some(match cmd {
        "input" | "include" | "subfile" | "subfileinclude" | "includeonly" | "import"
        | "subimport" | "inputfrom" | "includefrom" | "subinputfrom" | "subincludefrom"
        | "includestandalone" | "InputIfFileExists" | "externaldocument" => FileKind::Tex,
        "includegraphics" | "includesvg" | "includepdf" => FileKind::Graphics,
        "bibliography" | "addbibresource" | "addglobalbib" => FileKind::Bib,
        "usepackage" | "RequirePackage" => FileKind::Package,
        "documentclass" | "LoadClass" => FileKind::Class,
        "verbatiminput" | "lstinputlisting" | "inputminted" => FileKind::Other,
        _ => return None,
    })
}

/// The file names `target` of a `kind` argument may refer to, most likely first.
pub fn file_names(kind: FileKind, target: &str) -> Vec<String> {
    let target = target.trim();
    if target.is_empty() {
        return Vec::new();
    }
    let ext = Path::new(target)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match (kind, ext.as_deref()) {
        (FileKind::Tex, Some("tex")) => vec![target.to_owned()],
        (FileKind::Tex, Some(_)) => vec![target.to_owned(), format!("{target}.tex")],
        (FileKind::Tex, None) => vec![format!("{target}.tex"), target.to_owned()],
        (FileKind::Graphics, Some(_)) => vec![target.to_owned()],
        (FileKind::Graphics, None) => ["pdf", "png", "jpg", "jpeg", "eps", "svg", "PDF", "PNG"]
            .iter()
            .map(|e| format!("{target}.{e}"))
            .collect(),
        (FileKind::Bib, Some("bib")) => vec![target.to_owned()],
        (FileKind::Bib, _) => vec![format!("{target}.bib"), target.to_owned()],
        (FileKind::Package, _) => vec![format!("{target}.sty")],
        (FileKind::Class, _) => vec![format!("{target}.cls")],
        (FileKind::Other, _) => vec![target.to_owned()],
    }
}

/// The directories named by `\graphicspath{{a/}{b/}}` in `text`, in order.
pub fn graphics_paths(text: &str) -> Vec<String> {
    let Some(at) = text.find("\\graphicspath") else {
        return Vec::new();
    };
    let rest = text[at + "\\graphicspath".len()..].trim_start();
    let Some(rest) = rest.strip_prefix('{') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 1;
    let mut current = String::new();
    for c in rest.chars() {
        match c {
            '{' => {
                depth += 1;
                current.clear();
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                if !current.trim().is_empty() {
                    out.push(current.trim().to_owned());
                }
                current.clear();
            }
            '\n' if depth == 1 => {}
            c if depth == 2 => current.push(c),
            _ => {}
        }
    }
    out
}

/// The byte offset of the key of the entry `key` in a `.bib` text (`@book{key,`).
pub fn bib_entry_offset(text: &str, key: &str) -> Option<usize> {
    let b = text.as_bytes();
    let skip = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    text.match_indices('@').find_map(|(at, _)| {
        let kind = b[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if kind == 0 {
            return None;
        }
        let open = skip(at + 1 + kind);
        if !matches!(b.get(open), Some(b'{' | b'(')) {
            return None;
        }
        let start = skip(open + 1);
        let rest = text.get(start..)?;
        let after = skip(start + key.len());
        (rest.starts_with(key) && matches!(b.get(after), Some(b',' | b'}' | b')') | None))
            .then_some(start)
    })
}

/// Commands that define a macro (the definitions [`definition_offset`] looks for).
const DEFINING_COMMANDS: &[&str] = &[
    "\\newcommand",
    "\\renewcommand",
    "\\providecommand",
    "\\DeclareRobustCommand",
    "\\DeclareMathOperator",
    "\\NewDocumentCommand",
    "\\RenewDocumentCommand",
    "\\ProvideDocumentCommand",
    "\\DeclareDocumentCommand",
    "\\DeclarePairedDelimiter",
    "\\newrobustcmd",
    "\\def",
    "\\gdef",
    "\\edef",
    "\\xdef",
    "\\let",
];

/// The byte offset of `\name` in its definition in `text`: on the 0-based `line` (where the
/// project index saw it) when that line defines it, else on the first line that does.
pub fn definition_offset(text: &str, name: &str, line: u32) -> Option<usize> {
    let command = format!("\\{name}");
    let mut first = None;
    let mut line_no = 0u32;
    let mut counted = 0usize;
    for (at, _) in text.match_indices(&command) {
        let end = at + command.len();
        if text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '@')
        {
            continue;
        }
        line_no += text[counted..at].bytes().filter(|&b| b == b'\n').count() as u32;
        counted = at;
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let before = &text[line_start..at];
        let defines = DEFINING_COMMANDS.iter().any(|d| {
            before.match_indices(d).any(|(i, _)| {
                // `\def` must not be the start of `\default`.
                !before[i + d.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
            })
        });
        if !defines {
            continue;
        }
        if line_no == line {
            return Some(at);
        }
        first.get_or_insert(at);
    }
    first
}

// ----- bibliography text -----

/// A combining mark for a TeX accent command.
fn accent_mark(command: char) -> Option<char> {
    Some(match command {
        '\'' => '\u{301}',
        '`' => '\u{300}',
        '^' => '\u{302}',
        '"' => '\u{308}',
        '~' => '\u{303}',
        '=' => '\u{304}',
        '.' => '\u{307}',
        'u' => '\u{306}',
        'v' => '\u{30C}',
        'H' => '\u{30B}',
        'c' => '\u{327}',
        'k' => '\u{328}',
        'r' => '\u{30A}',
        'd' => '\u{323}',
        'b' => '\u{331}',
        _ => return None,
    })
}

/// Letters TeX writes as control words.
fn letter_word(word: &str) -> Option<&'static str> {
    Some(match word {
        "ss" => "ß",
        "o" => "ø",
        "O" => "Ø",
        "aa" => "å",
        "AA" => "Å",
        "ae" => "æ",
        "AE" => "Æ",
        "oe" => "œ",
        "OE" => "Œ",
        "l" => "ł",
        "L" => "Ł",
        "i" => "ı",
        "j" => "ȷ",
        "dh" => "ð",
        "DH" => "Ð",
        "th" => "þ",
        "TH" => "Þ",
        "LaTeX" => "LaTeX",
        "TeX" => "TeX",
        "textendash" => "–",
        "textemdash" => "—",
        "ldots" | "dots" | "textellipsis" => "…",
        _ => return None,
    })
}

/// A bibliography field or `.aux` number as plain text: TeX accents become accented letters
/// (`Rev{\^e}tements` → `Revêtements`), `--`/`---` dashes, `~` a no-break space, escaped
/// specials themselves; formatting commands (`\emph{…}`, `\textbf{…}`) keep their argument,
/// braces go, and math stays as written.
pub fn detex(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_math = false;
    while i < chars.len() {
        let c = chars[i];
        if in_math {
            out.push(c);
            if c == '$' {
                in_math = false;
            }
            i += 1;
            continue;
        }
        match c {
            '$' => {
                in_math = true;
                out.push(c);
                i += 1;
            }
            '{' | '}' => i += 1,
            '~' => {
                out.push('\u{a0}');
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                if chars.get(i + 2) == Some(&'-') {
                    out.push('—');
                    i += 3;
                } else {
                    out.push('–');
                    i += 2;
                }
            }
            '\\' => {
                let Some(&next) = chars.get(i + 1) else {
                    i += 1;
                    continue;
                };
                // A letter accent (`\c`, `\v`) is one only as a one-letter control word (not
                // `\cite`).
                if let Some(mark) = accent_mark(next)
                    && (!next.is_ascii_alphabetic()
                        || !chars.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()))
                {
                    // `\'e`, `\'{e}`, `\c c`, `\v{s}`: the accented letter (or `\i`).
                    let mut j = i + 2;
                    while chars.get(j) == Some(&' ') && next.is_ascii_alphabetic() {
                        j += 1;
                    }
                    let braced = chars.get(j) == Some(&'{');
                    if braced {
                        j += 1;
                    }
                    let mut base = String::new();
                    if chars.get(j) == Some(&'\\') {
                        let start = j + 1;
                        let mut k = start;
                        while chars.get(k).is_some_and(|c| c.is_ascii_alphabetic()) {
                            k += 1;
                        }
                        let word: String = chars[start..k].iter().collect();
                        base.push_str(letter_word(&word).unwrap_or(""));
                        j = k;
                    } else if let Some(&letter) = chars.get(j)
                        && letter != '}'
                    {
                        base.push(letter);
                        j += 1;
                    }
                    if braced && chars.get(j) == Some(&'}') {
                        j += 1;
                    }
                    if !base.is_empty() {
                        out.push_str(&base);
                        out.push(mark);
                        i = j;
                        continue;
                    }
                }
                if next.is_ascii_alphabetic() {
                    let start = i + 1;
                    let mut k = start;
                    while chars.get(k).is_some_and(|c| c.is_ascii_alphabetic()) {
                        k += 1;
                    }
                    let word: String = chars[start..k].iter().collect();
                    if let Some(letter) = letter_word(&word) {
                        out.push_str(letter);
                    }
                    // Other commands (`\emph`, `\textbf`, `\url`) go; their argument stays.
                    i = k;
                    while chars.get(i) == Some(&' ') && letter_word(&word).is_some() {
                        i += 1;
                    }
                } else if next == '\\' || next.is_whitespace() {
                    // A line break `\\` or a control space: a space.
                    if !out.ends_with(' ') && !out.is_empty() {
                        out.push(' ');
                    }
                    i += 2;
                } else {
                    // `\&`, `\%`, `\$`, `\_`, `\#`, `\{`: the character.
                    out.push(next);
                    i += 2;
                }
            }
            c if c.is_whitespace() => {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.trim().to_owned()
}

/// A BibTeX name list (`A and B and C`) for display: `A; B; C`, with "et al." after four names
/// (`and others` too).
pub fn authors(list: &str) -> String {
    let names: Vec<String> = list
        .split(" and ")
        .map(|n| detex(n.trim()))
        .filter(|n| !n.is_empty())
        .collect();
    let others = names.iter().any(|n| n == "others");
    let names: Vec<&String> = names.iter().filter(|n| *n != "others").collect();
    let mut shown: Vec<&str> = names.iter().take(4).map(|n| n.as_str()).collect();
    let more = names.len() > 4 || others;
    if names.len() > 4 {
        shown.truncate(3);
    }
    let mut out = shown.join("; ");
    if more {
        out.push_str(" et al.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_syntax::index_file;

    fn occ(text: &str, name: &str, kind: NameKind) -> Vec<(String, bool)> {
        scan_occurrences(text, Arc::new(ScanConfig::default()), name, kind)
            .into_iter()
            .map(|o| (text[o.range].to_owned(), o.definition))
            .collect()
    }

    fn lines(text: &str, name: &str, kind: NameKind) -> Vec<u32> {
        scan_occurrences(text, Arc::new(ScanConfig::default()), name, kind)
            .into_iter()
            .map(|o| o.line)
            .collect()
    }

    #[test]
    fn items_of_a_token() {
        let line = r"\cref{a, b ,,c}";
        let token = Token {
            start: 6,
            end: 14,
            kind: TokenKind::RefArg,
        };
        let items: Vec<(Range<usize>, &str)> = token_items(line, &token);
        assert_eq!(items, vec![(6..7, "a"), (9..10, "b"), (13..14, "c")]);
    }

    #[test]
    fn occurrences_of_labels_skip_comments_verbatim_and_other_text() {
        let text = "\\section{A}\\label{sec:a}\n\
                    See \\ref{sec:a} and \\cref{x, sec:a}.\n\
                    % \\ref{sec:a} in a comment\n\
                    \\verb|\\ref{sec:a}| and the word sec:a and {sec:a}\n\
                    \\crefrange{sec:a}{sec:ab} \\hyperref[sec:a]{text}\n\
                    \\begin{verbatim}\n\\ref{sec:a}\n\\end{verbatim}\n\
                    \\eqref{ sec:a } \\ref{sec:a,b}\n";
        assert_eq!(
            lines(text, "sec:a", NameKind::Label),
            vec![0, 1, 1, 4, 4, 8, 8]
        );
        let found = occ(text, "sec:a", NameKind::Label);
        assert_eq!(found[0], ("sec:a".to_owned(), true));
        assert!(found[1..].iter().all(|(t, def)| t == "sec:a" && !def));
        // Not a citation.
        assert!(occ(text, "sec:a", NameKind::Cite).is_empty());
    }

    #[test]
    fn occurrences_of_citation_keys() {
        let text = "\\cite{Stacks} \\cite[Tag 023N]{Stacks, SGA1}\n\
                    \\parencites[p.~1]{SGA1}{Stacks} \\label{Stacks}\n\
                    \\nocite{Stacksx}\n";
        assert_eq!(lines(text, "Stacks", NameKind::Cite), vec![0, 0, 1]);
        assert_eq!(lines(text, "SGA1", NameKind::Cite), vec![0, 1]);
    }

    #[test]
    fn rename_edits_descend() {
        let text = "\\label{a} \\ref{a} \\cref{b,a}";
        let found = scan_occurrences(text, Arc::new(ScanConfig::default()), "a", NameKind::Label);
        let edits = rename_edits(&found, "eq:new");
        let mut renamed = text.to_owned();
        for edit in &edits {
            edit.apply(&mut renamed);
        }
        assert_eq!(renamed, "\\label{eq:new} \\ref{eq:new} \\cref{b,eq:new}");
        assert!(edits.windows(2).all(|w| w[1].range.end <= w[0].range.start));
    }

    #[test]
    fn label_names() {
        assert_eq!(label_name_problem("eq:main-1_a.b/c"), None);
        assert!(label_name_problem("").is_some());
        assert!(label_name_problem("a b").is_some());
        assert!(label_name_problem("a,b").is_some());
        assert!(label_name_problem("a}").is_some());
        assert!(label_name_problem("a\\b").is_some());
        assert!(label_name_problem("a%").is_some());
    }

    fn project(files: &[(&str, &str)], bibs: &[(&str, &str)], aux: &str) -> ProjectIndex {
        let mut index = ProjectIndex::default();
        for (path, text) in files {
            index.update_file(PathBuf::from(path), Arc::new(index_file(text)));
        }
        for (path, text) in bibs {
            index
                .bibs
                .insert(PathBuf::from(path), Arc::new(lemma_syntax::scan_bib(text)));
        }
        index.aux = lemma_syntax::parse_aux(aux);
        index
    }

    #[test]
    fn undefined_references_and_citations() {
        let main = "\\documentclass{article}\n\\addbibresource{refs.bib}\n\\begin{document}\n\
                    \\section{A}\\label{sec:a}\n\\ref{sec:a} \\ref{sec:missing} \\eqref{eq:aux}\n\
                    \\cite{known, unknown} \\ref{\\x} \\ref{#1}\n\\input{ch}\n\\end{document}\n";
        let ch = "\\cref{sec:a,ch:missing}\n\\cite{known}\n";
        let bib = "@book{known, title={T}}\n@book{spare, title={S}}\n";
        let aux = "\\newlabel{eq:aux}{{1}{2}}\n";
        let index = project(
            &[("/p/main.tex", main), ("/p/ch.tex", ch)],
            &[("/p/refs.bib", bib)],
            aux,
        );
        let undefined = undefined(&index);
        assert!(undefined.cites_checked);
        let found: Vec<(&str, u32, &str, NameKind)> = undefined
            .findings
            .iter()
            .map(|f| (f.path.to_str().unwrap(), f.line, f.name.as_str(), f.kind))
            .collect();
        assert_eq!(
            found,
            vec![
                ("/p/ch.tex", 0, "ch:missing", NameKind::Label),
                ("/p/main.tex", 4, "sec:missing", NameKind::Label),
                ("/p/main.tex", 5, "unknown", NameKind::Cite),
            ]
        );
        // A bibliography Lemma could not read: no citation is reported.
        let mut unread = index.clone();
        unread.bibs.clear();
        let undefined = super::undefined(&unread);
        assert!(!undefined.cites_checked);
        assert!(undefined.findings.iter().all(|f| f.kind == NameKind::Label));
        // Unused.
        let labels: Vec<String> = unused_labels(&index).into_iter().map(|f| f.name).collect();
        assert!(labels.is_empty(), "{labels:?}");
        let entries: Vec<String> = unused_entries(&index, false)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(entries, ["spare"]);
        assert!(unused_entries(&index, true).is_empty());
    }

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("eq:one", "eq:one"), 0);
        assert_eq!(edit_distance("eq:onee", "eq:one"), 1);
        assert_eq!(edit_distance("sec:itnro", "sec:intro"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn nocite_star() {
        assert!(cites_everything("x \\nocite{*} y"));
        assert!(cites_everything("\\nocite { * }"));
        assert!(!cites_everything("\\nocite{a}"));
    }

    #[test]
    fn what_labels_are_called() {
        let text = "\\documentclass{amsart}\n\\newtheorem{thm}{Theorem}[section]\n\
                    \\newtheorem{lemma}[thm]{Lemma}\n\\begin{document}\n\
                    \\section{Intro}\\label{sec:i}\n\\subsection{More}\\label{sec:m}\n\
                    \\begin{thm}[Big]\\label{thm:big}\nText\n\\end{thm}\n\
                    \\begin{lemma}\\label{lem:a}\\end{lemma}\n\
                    \\begin{equation}\\label{eq:1}a\\end{equation}\n\
                    \\begin{align}a\\label{eq:2}\\end{align}\n\
                    \\begin{figure}\\caption{C}\\label{fig:c}\\end{figure}\n\
                    \\begin{mything}\\label{my:x}\\end{mything}\n\\end{document}\n";
        let aux = "\\newlabel{thm:big}{{1.1}{1}}\n\\newlabel{eq:1}{{1}{2}}\n\
                   \\newlabel{sec:m}{{1.1}{1}}\n\\newlabel{lem:a}{{1.2}{1}}\n";
        let index = project(&[("/p/main.tex", text)], &[], aux);
        let file = index.files.values().next().unwrap().clone();
        let info = |name: &str| {
            let label = file.labels.iter().find(|l| l.name == name).unwrap();
            label_info(&index, &file, label)
        };
        assert_eq!(info("thm:big").title(), "Theorem 1.1");
        assert_eq!(info("thm:big").annotation().as_deref(), Some("1.1"));
        assert_eq!(info("lem:a").annotation().as_deref(), Some("1.2"));
        assert_eq!(info("eq:1").title(), "Equation (1)");
        assert_eq!(info("eq:1").annotation().as_deref(), Some("(1)"));
        assert_eq!(info("eq:1").page.as_deref(), Some("2"));
        assert_eq!(info("eq:2").title(), "Equation");
        assert_eq!(info("eq:2").annotation(), None);
        assert_eq!(info("sec:i").kind, "Section");
        assert_eq!(info("sec:m").annotation().as_deref(), Some("§1.1"));
        assert_eq!(info("fig:c").kind, "Figure");
        assert_eq!(info("my:x").kind, "Mything");
        let thm = file.labels.iter().find(|l| l.name == "thm:big").unwrap();
        assert_eq!(label_lines(&file, thm, 3), 6..9);
        let lemma = file.labels.iter().find(|l| l.name == "lem:a").unwrap();
        assert_eq!(label_lines(&file, lemma, 3), 9..10);
        let sec = file.labels.iter().find(|l| l.name == "sec:i").unwrap();
        assert_eq!(label_lines(&file, sec, 3), 4..7);
        // The typeset lines of an equation are its body; of a theorem, its first lines.
        let eq = file.labels.iter().find(|l| l.name == "eq:2").unwrap();
        assert_eq!(
            typeset_lines(&file, eq, true, 12),
            11..12,
            "on one line: that line"
        );
        assert_eq!(typeset_lines(&file, thm, false, 12), 6..9);
        let display = index_file(
            "\\begin{document}\nText\n\\begin{align}\\label{eq:a}\na &= b \\\\\nc &= d\n\
             \\end{align}\n\\end{document}\n",
        );
        let a = display.labels.iter().find(|l| l.name == "eq:a").unwrap();
        assert_eq!(typeset_lines(&display, a, true, 12), 3..5, "the body");
        assert_eq!(
            typeset_lines(&display, a, true, 1),
            3..4,
            "at most `max` lines"
        );
    }

    #[test]
    fn file_arguments() {
        assert_eq!(file_kind("input"), Some(FileKind::Tex));
        assert_eq!(file_kind("includegraphics"), Some(FileKind::Graphics));
        assert_eq!(file_kind("usepackage"), Some(FileKind::Package));
        assert_eq!(file_kind("ref"), None);
        assert_eq!(
            file_names(FileKind::Tex, "chapters/a"),
            ["chapters/a.tex", "chapters/a"]
        );
        assert_eq!(file_names(FileKind::Tex, "a.tex"), ["a.tex"]);
        assert_eq!(file_names(FileKind::Package, "defs"), ["defs.sty"]);
        assert_eq!(file_names(FileKind::Class, "mine"), ["mine.cls"]);
        assert_eq!(file_names(FileKind::Bib, "refs"), ["refs.bib", "refs"]);
        assert_eq!(file_names(FileKind::Graphics, "f.png"), ["f.png"]);
        assert_eq!(file_names(FileKind::Graphics, "f")[0], "f.pdf");
        assert_eq!(
            graphics_paths("x\\graphicspath{{figs/}{../img/}}\n"),
            ["figs/", "../img/"]
        );
        assert!(graphics_paths("nothing").is_empty());
    }

    #[test]
    fn entry_and_definition_offsets() {
        let bib = "@string{x = {y}}\n@book{SGA1x,\n}\n@Book { SGA1 ,\n title={T}}\n";
        let at = bib_entry_offset(bib, "SGA1").unwrap();
        assert_eq!(&bib[at..at + 4], "SGA1");
        assert_eq!(bib[..at].lines().count(), 4);
        assert_eq!(bib_entry_offset(bib, "SGA"), None);
        let tex = "\\cF here\n\\newcommand{\\cFF}{x}\n\\newcommand{\\cF}{\\mathcal{F}}\n\
                   \\renewcommand{\\cF}{y}\n\\default\\cF\n";
        let at = definition_offset(tex, "cF", 2).unwrap();
        assert_eq!(&tex[at..at + 4], "\\cF}");
        assert_eq!(tex[..at].matches('\n').count(), 2);
        let at = definition_offset(tex, "cF", 3).unwrap();
        assert_eq!(tex[..at].matches('\n').count(), 3);
        // A stale line: the first definition.
        let at = definition_offset(tex, "cF", 9).unwrap();
        assert_eq!(tex[..at].matches('\n').count(), 2);
        assert_eq!(definition_offset(tex, "nothing", 0), None);
    }

    #[test]
    fn bibliography_text() {
        assert_eq!(
            detex("Rev{\\^e}tements {\\'e}tales et groupe fondamental (SGA 1)"),
            "Reve\u{302}tements e\u{301}tales et groupe fondamental (SGA 1)"
        );
        assert_eq!(
            detex("G{\\\"o}del and \\c{C}ar and \\v s"),
            "Go\u{308}del and C\u{327}ar and s\u{30c}"
        );
        assert_eq!(
            detex("pp.~1--104 \\& more---ok"),
            "pp.\u{a0}1–104 & more—ok"
        );
        assert_eq!(detex("\\emph{Stacks} {The} \\ss{}e"), "Stacks The ße");
        assert_eq!(
            detex("A  study of $\\mathbb{Z}$ \\\\ {x}"),
            "A study of $\\mathbb{Z}$ x"
        );
        assert_eq!(detex("{\\'\\i}"), "ı\u{301}");
        assert_eq!(
            authors("Grothendieck, Alexander"),
            "Grothendieck, Alexander"
        );
        assert_eq!(authors("A and B and others"), "A; B et al.");
        assert_eq!(authors("A and B and C and D and E"), "A; B; C et al.");
        assert_eq!(
            authors("{The Stacks project authors}"),
            "The Stacks project authors"
        );
    }
}
