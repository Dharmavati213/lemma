//! The Outline page of the sidebar (ARCHITECTURE §5 "Navigation").
//!
//! # What it shows
//!
//! The structure of the **whole project** of the active document, in include order: the root
//! document's rows first, and every file it pulls in with `\input`, `\include`, `\subfile` or
//! `\import` as a row of its own, at the place where it is included, with the file's rows
//! nested under it. A file row sits under the deepest open heading that is above the file's
//! own headings (a chapter file included inside the introduction's section is a sibling of that
//! section, not its child). Files included in the preamble or inside a float appear only when
//! they hold rows themselves.
//!
//! Rows: parts, chapters, sections down to subparagraphs (KOMA-Script's `\addpart`, `\addchap`,
//! `\addsec` and `\minisec` too), theorem-like environments as children of their heading (named
//! after the project's `\newtheorem` / `\declaretheorem`: `Lemma`, not `lem`), figures, tables
//! and algorithms with their captions, and included files. Each row shows the number the last
//! build gave it when it is known (`3.2`, `Part I`, `Theorem 2.1`, `Figure 1`), then the title;
//! an unnumbered (starred) heading shows `∗` instead of a number.
//!
//! **Numbers** come from the `.aux` of the root's last build (`<root dir>/<build.outdir>/` or
//! next to the root, plus the `.aux` files of `\include`d files), read on the jobs thread when the
//! root changes and after every build ([`read_aux`]):
//! - a `\label` right after a heading (on its line or the next), or inside a theorem or float,
//!   gives that label's number;
//! - otherwise a heading takes the number of its table-of-contents line (`\@writefile{toc}`),
//!   matched in document order by level and title (`align`): unlabelled sections are numbered
//!   too, a heading whose title was edited since the build keeps its number, and a heading added
//!   since gets none until the next build;
//! - figures and tables take theirs from the lists of figures and tables the same way.
//!
//! **Captions** come from the live text of open documents ([`caption_in`]), else from the
//! title of a `\label` in the float, else from the list of figures or tables.
//!
//! # Behaviour
//!
//! - **Click** a row (or press **Enter** on it) to go to its line, opening the file if needed,
//!   through [`goto::navigate`], so Alt+Left (Ctrl+O in Vim) comes back; the sidebar overlay of
//!   a narrow window is dismissed afterwards. Clicking a row's arrow only expands or collapses
//!   it; **Left** and **Right** do the same from the keyboard.
//! - **Follow cursor** (`[outline] follow_cursor`, also a check item of the page's menu): the row
//!   of the innermost heading, theorem or float holding the cursor is selected (the deepest
//!   *visible* one when its parents are collapsed) and, once the cursor rests for 120 ms,
//!   scrolled into view in the outline. The editor is never scrolled by it.
//! - **Filter**: typing in the entry (or anywhere on the list) shows the rows matching the text
//!   fuzzily (frizbee), best first, with the matched characters in bold in the accent colour
//!   (the number and the theorem name match too: `Prop`, `3.2`); **Up**/**Down** move
//!   the selection, **Enter** goes to it, **Escape** clears the filter (and then gives the focus
//!   back to the editor).
//! - **Collapse state** is kept per row (file, kind, level and title) across updates and when
//!   the rows are rebuilt; the menu has *Expand All* and *Collapse All*.
//!
//! # Updates and cost
//!
//! Nothing at all happens while the page is not shown (the sidebar is hidden or shows another
//! page): the page is brought up to date when it is mapped. While it is shown:
//! - a keystroke costs a few microseconds: a document change only (re)arms a 100 ms watch, a
//!   change of the line count shifts the lines of that file's rows (so a click lands on the right
//!   line before the project index catches up), and a cursor move queues one idle `follow`;
//! - the watch compares the project index's per-file `Arc`s (`state.workspace.index`, kept by
//!   completion's project feed on the jobs thread 300 ms after typing stops) and the `.aux`
//!   generation with what the page shows, and only on a change recomputes the outline (no GTK)
//!   and **patches** the list models: unchanged rows keep their objects (and so their expanded
//!   state and selection), changed rows are updated in place, and only rows that appeared are
//!   created. The watch stops when the feed is idle and nothing changed for two ticks.
//!
//! The project index has no change signal, so the watch is armed by the events that change it:
//! document changes, tabs opened, closed and selected, the project root moving, and finished
//! builds.
//!
//! # Hooks
//!
//! [`install`] (once per window) puts the page into `state.sidebar.outline`; [`run_demo`] is
//! `--demo outline` (`i-outline.png`; `LEMMA_DEMO_FILTER=text` shows the filter instead). The
//! rest of the public functions are for tests and other modules: [`rows`], [`tree_text`],
//! [`selected_row`], [`activate_row`], [`activate_position`], [`focus_row`], [`set_expanded`],
//! [`is_expanded`], [`set_filter`], [`filter_results`], [`activate_filter`], [`page_state`],
//! [`refresh`], [`is_settled`], [`stats`], [`root`], [`page`], [`row_line`]; the model is
//! GUI-free and public too ([`build_outline`], [`parse_aux_text`], [`read_aux`],
//! [`aux_candidates`], [`caption_in`], [`pretty_title`], [`tree_text_of`]).
//!
//! **Known limit:** rows that follow an `\input` in the including file nest under the including
//! file's open headings, not under a section the included file opened (which LaTeX continues):
//! a row belongs to the file it is written in.

use crate::document::{DocRef, ObserverId};
use crate::editor::completion;
use crate::editor::goto::{self, GotoTarget};
use crate::window::{PAGE_OUTLINE, WindowState};
use adw::prelude::*;
use lemma_syntax::{FileIndex, Include, IncludeKind, OutlineItem, ProjectIndex, parse_aux};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ───────────────────────── The model (no GTK) ─────────────────────────

/// [`Entry::level`] of theorem-like environments (the structure pass's level 7).
pub const LEVEL_THEOREM: u8 = 7;
/// [`Entry::level`] of floats.
pub const LEVEL_FLOAT: u8 = 8;
/// [`Entry::level`] of included files.
pub const LEVEL_FILE: u8 = 9;

/// The sectioning commands by level (0 part … 6 subparagraph): what a heading row's
/// navigation looks for on its line.
const HEADING_COMMANDS: [&str; 7] = [
    "\\part",
    "\\chapter",
    "\\section",
    "\\subsection",
    "\\subsubsection",
    "\\paragraph",
    "\\subparagraph",
];

/// The table-of-contents entry kinds by level.
const TOC_KINDS: [&str; 7] = [
    "part",
    "chapter",
    "section",
    "subsection",
    "subsubsection",
    "paragraph",
    "subparagraph",
];

/// How many lines after a float's `\begin` are searched for its caption.
const CAPTION_LINES: u32 = 40;

/// How many entries ahead [`align`] looks for a title.
const ALIGN_WINDOW: usize = 8;

/// The largest `.aux` read, and how many `\@input` files are followed.
const MAX_AUX_BYTES: u64 = 16 << 20;
const MAX_AUX_INPUTS: usize = 256;

/// What a row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// `\part` … `\subparagraph`, KOMA-Script's `\add…` and `\minisec`.
    Heading,
    /// A theorem-like environment.
    Theorem,
    /// A figure, table or algorithm.
    Float,
    /// An included file.
    File,
}

/// One row of the outline and the rows under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// Headings: 0 part … 6 subparagraph; [`LEVEL_THEOREM`], [`LEVEL_FLOAT`], [`LEVEL_FILE`].
    pub level: u8,
    /// The number of the last build (`"3.2"`, `"I"`, `"A"`), when known.
    pub number: Option<String>,
    /// What a theorem or float is called (`"Lemma"`, `"Figure"`); `None` otherwise.
    pub name: Option<String>,
    /// As shown: a heading's title, a theorem's optional title, a float's caption (TeX markup
    /// made readable, math kept), a file's path relative to the root's directory.
    pub title: String,
    /// An unnumbered heading or environment.
    pub starred: bool,
    /// The file the row is in (for a file row: the included file).
    pub path: PathBuf,
    /// The 0-based line of the command (a file row: 0).
    pub line: u32,
    /// The line of the environment's `\end` (theorems and floats), when it has one.
    pub end_line: Option<u32>,
    /// What navigation looks for on `line` (`\section`, `\begin{lemma}`).
    pub find: Option<String>,
    /// File rows: false while the file is not in the project index (not read yet, or missing).
    pub indexed: bool,
    pub children: Vec<Entry>,
}

impl Entry {
    /// The number column: `3.2`, `Part I`, `Lemma 2.1`, `Figure`, `∗` (unnumbered heading).
    pub fn number_text(&self) -> String {
        number_text(
            self.kind,
            self.level,
            self.number.as_deref(),
            self.name.as_deref(),
            self.starred,
        )
    }

    /// The title column: a theorem's title in parentheses, as amsthm prints it.
    pub fn title_text(&self) -> String {
        title_text(self.kind, &self.title)
    }

    /// The row as one line of text: `2.1 Sheaves`, `Lemma 2.1 (Yoneda)`,
    /// `[chapters/yoneda.tex]` (the golden format).
    pub fn text(&self) -> String {
        row_text(self.kind, &self.number_text(), &self.title_text())
    }
}

fn number_text(
    kind: EntryKind,
    level: u8,
    number: Option<&str>,
    name: Option<&str>,
    starred: bool,
) -> String {
    match kind {
        EntryKind::Heading if starred => "∗".to_owned(),
        EntryKind::Heading if level == 0 => match number {
            Some(n) => format!("Part {n}"),
            None => "Part".to_owned(),
        },
        EntryKind::Heading => number.unwrap_or_default().to_owned(),
        EntryKind::Theorem | EntryKind::Float => {
            let name = name.unwrap_or_default();
            match number {
                Some(n) if !starred => format!("{name} {n}"),
                _ => name.to_owned(),
            }
        }
        EntryKind::File => String::new(),
    }
}

fn title_text(kind: EntryKind, title: &str) -> String {
    match kind {
        EntryKind::Theorem if !title.is_empty() => format!("({title})"),
        _ => title.to_owned(),
    }
}

fn row_text(kind: EntryKind, number: &str, title: &str) -> String {
    match kind {
        EntryKind::File => format!("[{title}]"),
        _ if number.is_empty() => title.to_owned(),
        _ if title.is_empty() => number.to_owned(),
        _ => format!("{number} {title}"),
    }
}

/// The outline as indented text, two spaces per level (the golden format of the tests).
pub fn tree_text_of(entries: &[Entry]) -> String {
    fn walk(entries: &[Entry], depth: usize, out: &mut String) {
        for e in entries {
            out.push_str(&"  ".repeat(depth));
            out.push_str(&e.text());
            out.push('\n');
            walk(&e.children, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(entries, 0, &mut out);
    out
}

/// What the `.aux` files of a build say: label numbers, and the lines of the table of contents
/// and the lists of figures and tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuxData {
    /// label → (number, page), from `\newlabel`.
    pub labels: BTreeMap<String, (String, String)>,
    pub toc: Vec<ContentsLine>,
    pub lof: Vec<ContentsLine>,
    pub lot: Vec<ContentsLine>,
}

/// One `\contentsline` of a `\@writefile{toc|lof|lot}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentsLine {
    /// `part`, `chapter`, `section`, …, `figure`, `table`.
    pub kind: String,
    /// The number (`1.2`, `I`, `A`); `None` for an unnumbered entry.
    pub number: Option<String>,
    /// The title as the `.aux` has it (TeX source, partly expanded).
    pub title: String,
}

/// Reads `\newlabel`s and the `toc`, `lof` and `lot` lines of an `.aux` text, in order,
/// following `\@input{file.aux}` (the `.aux` of an `\include`d file) at its place through
/// `read_input` (given the name as written; `None`: not readable).
pub fn parse_aux_text(
    text: &str,
    out: &mut AuxData,
    read_input: &mut dyn FnMut(&str) -> Option<String>,
) {
    parse_aux_into(text, out, read_input, 0);
}

fn parse_aux_into(
    text: &str,
    out: &mut AuxData,
    read_input: &mut dyn FnMut(&str) -> Option<String>,
    depth: u32,
) {
    out.labels.extend(parse_aux(text).labels);
    let mut i = 0;
    while let Some(off) = text[i..].find("\\@") {
        let at = i + off;
        let after = at + 2;
        if let Some(rest) = text[after..].strip_prefix("writefile")
            && !starts_with_letter(rest)
            && let Some((ext, e1)) = group(text, after + "writefile".len())
            && let Some((content, e2)) = group(text, e1)
        {
            let list = match text[ext].trim() {
                "toc" => Some(&mut out.toc),
                "lof" => Some(&mut out.lof),
                "lot" => Some(&mut out.lot),
                _ => None,
            };
            if let Some(list) = list
                && let Some(line) = parse_contents_line(&text[content])
            {
                list.push(line);
            }
            i = e2;
            continue;
        }
        if let Some(rest) = text[after..].strip_prefix("input")
            && !starts_with_letter(rest)
            && let Some((name, end)) = group(text, after + "input".len())
        {
            if depth < 8
                && let Some(input) = read_input(text[name].trim())
            {
                parse_aux_into(&input, out, read_input, depth + 1);
            }
            i = end;
            continue;
        }
        i = after;
    }
}

fn parse_contents_line(content: &str) -> Option<ContentsLine> {
    let c = content.trim_start();
    let rest = c.strip_prefix("\\contentsline")?;
    if starts_with_letter(rest) {
        return None;
    }
    let (kind, e1) = group(c, c.len() - rest.len())?;
    let (entry, _) = group(c, e1)?;
    let (number, title) = split_contents_entry(&c[entry]);
    Some(ContentsLine {
        kind: c[kind].trim().to_owned(),
        number,
        title,
    })
}

/// Splits the second argument of a `\contentsline` into its number and title, for the forms
/// the standard classes, KOMA-Script, memoir and the AMS classes write.
fn split_contents_entry(entry: &str) -> (Option<String>, String) {
    let e = entry.trim();
    for cmd in ["\\numberline", "\\partnumberline", "\\chapternumberline"] {
        if let Some(rest) = e.strip_prefix(cmd)
            && !starts_with_letter(rest)
            && let Some((number, end)) = group(rest, 0)
        {
            return (non_empty(&rest[number]), clean_aux_title(&rest[end..]));
        }
    }
    if let Some(rest) = e.strip_prefix("\\nonumberline")
        && !starts_with_letter(rest)
    {
        return (None, clean_aux_title(rest));
    }
    // amsart, amsbook: \tocsection {Appendix}{A}{Title}.
    if let Some(rest) = e.strip_prefix("\\toc") {
        let name = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if name > 0
            && let Some((_, e1)) = group(rest, name)
            && let Some((number, e2)) = group(rest, e1)
            && let Some((title, _)) = group(rest, e2)
        {
            return (non_empty(&rest[number]), clean_aux_title(&rest[title]));
        }
    }
    // Parts of the standard classes (`I\hspace {1em}Title`) and of amsbook
    // (`Part\ 1. \enspace \noindent Title`).
    for sep in ["\\hspace", "\\enspace", "\\quad"] {
        if let Some(p) = e.find(sep) {
            let number = e[..p]
                .split_whitespace()
                .last()
                .map(|n| n.trim_end_matches('.'))
                .and_then(non_empty);
            let mut rest = &e[p + sep.len()..];
            if sep == "\\hspace" {
                rest = rest.trim_start().strip_prefix('*').unwrap_or(rest);
                if let Some((_, end)) = group(rest, 0) {
                    rest = &rest[end..];
                }
            }
            return (number, clean_aux_title(rest));
        }
    }
    (None, clean_aux_title(e))
}

/// Removes the spacing and protection commands of an `.aux` title and its outer braces.
fn clean_aux_title(title: &str) -> String {
    let mut t = title.to_owned();
    for cmd in [
        "\\ignorespaces",
        "\\relax",
        "\\noindent",
        "\\protect",
        "\\nobreakspace {}",
    ] {
        t = t.replace(cmd, " ");
    }
    let t = collapse_spaces(&t);
    let mut t = t.as_str();
    while t.starts_with('{')
        && let Some((inner, end)) = group(t, 0)
        && end == t.len()
    {
        t = t[inner].trim();
    }
    t.to_owned()
}

/// Reads the `.aux` of a build (blocking: run it on the jobs thread): the first readable of
/// `candidates`, and the `\@input` files it names (resolved against its directory, then
/// `fallback_dir`). Returns the file read and what it says.
pub fn read_aux(candidates: &[PathBuf], fallback_dir: &Path) -> Option<(PathBuf, AuxData)> {
    let read = |path: &Path| -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_AUX_BYTES {
            return None;
        }
        std::fs::read(path)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    for path in candidates {
        let Some(text) = read(path) else {
            continue;
        };
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let mut seen = HashSet::new();
        let mut read_input = |name: &str| -> Option<String> {
            if seen.len() >= MAX_AUX_INPUTS {
                return None;
            }
            [&dir, fallback_dir].iter().find_map(|d| {
                let p = lexical_join(d, name);
                if !seen.insert(p.clone()) {
                    return None;
                }
                read(&p)
            })
        };
        let mut aux = AuxData::default();
        parse_aux_text(&text, &mut aux, &mut read_input);
        return Some((path.clone(), aux));
    }
    None
}

/// Where the `.aux` of `root`'s build can be: `hint` (beside the last build's PDF), the build
/// output directory `outdir` (relative to the root's directory), then next to the root.
pub fn aux_candidates(root: &Path, outdir: &str, hint: Option<&Path>) -> Vec<PathBuf> {
    let dir = root.parent().unwrap_or(Path::new("/"));
    let mut name = PathBuf::from(root.file_stem().unwrap_or_default());
    name.set_extension("aux");
    let mut out: Vec<PathBuf> = hint.map(Path::to_path_buf).into_iter().collect();
    if !outdir.is_empty() {
        out.push(lexical_join(dir, outdir).join(&name));
    }
    out.push(dir.join(&name));
    out.dedup();
    out
}

/// What a float environment is called and which list holds its caption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatList {
    Figures,
    Tables,
    Other,
}

fn float_kind(env: &str) -> Option<(&'static str, FloatList)> {
    let base = env.strip_suffix('*').unwrap_or(env);
    match base {
        "figure" | "wrapfigure" | "sidewaysfigure" | "SCfigure" | "figwindow" => {
            Some(("Figure", FloatList::Figures))
        }
        "table" | "wraptable" | "sidewaystable" | "SCtable" | "longtable" | "tabwindow" => {
            Some(("Table", FloatList::Tables))
        }
        "algorithm" => Some(("Algorithm", FloatList::Other)),
        _ => None,
    }
}

/// The list of figures or tables a float's caption goes to, by its name.
fn list_of(name: Option<&str>) -> FloatList {
    match name {
        Some("Figure") => FloatList::Figures,
        Some("Table") => FloatList::Tables,
        _ => FloatList::Other,
    }
}

/// The caption of a float whose source from its `\begin` on is `text`: the short form of the
/// first `\caption[short]{long}` that is not inside a `sub…` environment (a subfigure), else
/// its long form; readable ([`pretty_title`]).
pub fn caption_in(text: &str) -> Option<String> {
    let clean: Vec<&str> = text.lines().map(strip_comment).collect();
    let s = clean.join("\n");
    let mut sub = 0usize;
    let mut i = 0;
    while let Some(off) = s[i..].find('\\') {
        let at = i + off;
        let name_end = s[at + 1..]
            .find(|c: char| !c.is_ascii_alphabetic())
            .map_or(s.len(), |n| at + 1 + n);
        let name = &s[at + 1..name_end];
        match name {
            "" => {
                // A control symbol (`\\`, `\{`): skip it whole.
                i = at + 1 + s[at + 1..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            "begin" | "end" => {
                if let Some((env, end)) = group(&s, name_end) {
                    if s[env].trim().starts_with("sub") {
                        if name == "begin" {
                            sub += 1;
                        } else {
                            sub = sub.saturating_sub(1);
                        }
                    }
                    i = end;
                    continue;
                }
            }
            "caption" if sub == 0 => {
                let mut j = name_end;
                if s[j..].starts_with('*') {
                    j += 1;
                }
                let short = bracket(&s, j);
                if let Some((_, end)) = &short {
                    j = *end;
                }
                let long = group(&s, j);
                let text = short
                    .map(|(r, _)| &s[r])
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| long.map(|(r, _)| &s[r]))?;
                let title = pretty_title(&remove_commands(text, &["label", "index"]));
                return (!title.is_empty()).then_some(title);
            }
            _ => {}
        }
        i = name_end;
    }
    None
}

/// The line without its `%` comment (an unescaped `%` and what follows).
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'%' => return &line[..i],
            _ => i += 1,
        }
    }
    line
}

/// `text` without the commands `names` and their braced argument.
fn remove_commands(text: &str, names: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(off) = text[i..].find('\\') {
        let at = i + off;
        out.push_str(&text[i..at]);
        let name_end = text[at + 1..]
            .find(|c: char| !c.is_ascii_alphabetic())
            .map_or(text.len(), |n| at + 1 + n);
        let name = &text[at + 1..name_end];
        if names.contains(&name)
            && let Some((_, end)) = group(text, name_end)
        {
            i = end;
            continue;
        }
        let end = if name.is_empty() {
            (at + 1 + text[at + 1..].chars().next().map_or(0, char::len_utf8)).min(text.len())
        } else {
            name_end
        };
        out.push_str(&text[at..end]);
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

/// A heading, theorem or float title made readable: font commands (`\emph{x}`, `\textbf{x}`)
/// give their argument, `\texorpdfstring{tex}{text}` its text, accents and `~`, `--`, quotes
/// their characters, `\&` and friends their symbol; math (`$…$`) and other commands stay as
/// written.
pub fn pretty_title(raw: &str) -> String {
    // Most titles are plain words: nothing to do but collapse spaces.
    if !raw.contains(['\\', '~', '{', '}', '$', '-', '`', '\'']) {
        return if raw.contains("  ") || raw.starts_with(' ') || raw.ends_with(' ') {
            collapse_spaces(raw)
        } else {
            raw.to_owned()
        };
    }
    let mut out = String::with_capacity(raw.len());
    pretty_into(raw, &mut out);
    collapse_spaces(&out)
}

fn pretty_into(s: &str, out: &mut String) {
    let mut i = 0;
    let mut math = false;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap_or(' ');
        if math {
            if c == '$' {
                math = false;
            } else if c == '\\' && rest.len() > 1 {
                // `\$` inside math must not end it.
                let next = rest[1..].chars().next().unwrap_or(' ');
                out.push('\\');
                out.push(next);
                i += 1 + next.len_utf8();
                continue;
            }
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        match c {
            '$' => {
                math = true;
                out.push('$');
                i += 1;
            }
            '\\' => i = pretty_command(s, i, out),
            '~' => {
                out.push(' ');
                i += 1;
            }
            '{' | '}' => i += 1,
            '-' if rest.starts_with("---") => {
                out.push('—');
                i += 3;
            }
            '-' if rest.starts_with("--") => {
                out.push('–');
                i += 2;
            }
            '`' if rest.starts_with("``") => {
                out.push('“');
                i += 2;
            }
            '`' => {
                out.push('‘');
                i += 1;
            }
            '\'' if rest.starts_with("''") => {
                out.push('”');
                i += 2;
            }
            _ => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
}

/// Handles the control sequence at `s[i..]` (a backslash) for [`pretty_title`]; returns where
/// to go on.
fn pretty_command(s: &str, i: usize, out: &mut String) -> usize {
    let rest = &s[i + 1..];
    let name_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if name_len == 0 {
        let Some(sym) = rest.chars().next() else {
            return i + 1;
        };
        let after = i + 1 + sym.len_utf8();
        if let Some(mark) = accent_mark(&sym.to_string()) {
            return accent(s, after, mark, out);
        }
        match sym {
            '&' | '%' | '$' | '#' | '_' | '{' | '}' => out.push(sym),
            ',' | ';' | ':' | ' ' | '!' | '\\' | '/' | '@' => out.push(' '),
            '-' => {}
            _ => {
                out.push('\\');
                out.push(sym);
            }
        }
        return after;
    }
    let name = &rest[..name_len];
    let after = i + 1 + name_len;
    // The space after a control word belongs to it.
    let skip_space = |at: usize| at + s[at..].len() - s[at..].trim_start().len();
    if let Some(mark) = accent_mark(name) {
        return accent(s, skip_space(after), mark, out);
    }
    let symbol = match name {
        "LaTeX" => Some("LaTeX"),
        "LaTeXe" => Some("LaTeX2ε"),
        "TeX" => Some("TeX"),
        "ldots" | "dots" | "textellipsis" => Some("…"),
        "S" => Some("§"),
        "P" => Some("¶"),
        "ss" => Some("ß"),
        "ae" => Some("æ"),
        "AE" => Some("Æ"),
        "oe" => Some("œ"),
        "OE" => Some("Œ"),
        "aa" => Some("å"),
        "AA" => Some("Å"),
        "o" => Some("ø"),
        "O" => Some("Ø"),
        "l" => Some("ł"),
        "L" => Some("Ł"),
        "i" => Some("ı"),
        "textendash" => Some("–"),
        "textemdash" => Some("—"),
        "newline" | "linebreak" | "quad" | "qquad" | "enspace" | "thinspace" => Some(" "),
        _ => None,
    };
    if let Some(symbol) = symbol {
        out.push_str(symbol);
        return skip_space(after);
    }
    match name {
        "emph" | "textbf" | "textit" | "textsc" | "texttt" | "textrm" | "textsf" | "textup"
        | "textsl" | "textmd" | "textnormal" | "underline" | "mbox" | "hbox" | "text" => {
            match group(s, after) {
                Some((arg, end)) => {
                    pretty_into(&s[arg], out);
                    end
                }
                None => skip_space(after),
            }
        }
        "texorpdfstring" => match group(s, after).and_then(|(_, e1)| group(s, e1)) {
            Some((text, end)) => {
                pretty_into(&s[text], out);
                end
            }
            None => after,
        },
        "bf" | "it" | "em" | "sc" | "rm" | "sf" | "tt" | "sl" | "normalfont" | "bfseries"
        | "itshape" | "scshape" | "upshape" | "mdseries" | "slshape" | "relax" | "protect"
        | "noindent" | "ignorespaces" | "centering" | "small" | "footnotesize" | "large"
        | "Large" | "LARGE" | "huge" | "Huge" | "normalsize" | "boldmath" | "unboldmath" => {
            skip_space(after)
        }
        _ => {
            out.push('\\');
            out.push_str(name);
            after
        }
    }
}

/// The combining mark of an accent command (`\'`, `\"`, `\c`, …).
fn accent_mark(name: &str) -> Option<char> {
    Some(match name {
        "'" => '\u{301}',
        "`" => '\u{300}',
        "^" => '\u{302}',
        "\"" => '\u{308}',
        "~" => '\u{303}',
        "=" => '\u{304}',
        "." => '\u{307}',
        "c" => '\u{327}',
        "v" => '\u{30c}',
        "u" => '\u{306}',
        "H" => '\u{30b}',
        "r" => '\u{30a}',
        "k" => '\u{328}',
        _ => return None,
    })
}

/// Puts the letter at `s[at..]` (or the first letter of a group there) with `mark` into `out`.
fn accent(s: &str, at: usize, mark: char, out: &mut String) -> usize {
    let (base, end) = match group(s, at) {
        Some((arg, end)) => (s[arg].trim().to_owned(), end),
        None => match s[at..].chars().next() {
            Some(c) => (c.to_string(), at + c.len_utf8()),
            None => (String::new(), at),
        },
    };
    let base = match base.as_str() {
        "\\i" => "ı".to_owned(),
        "\\j" => "ȷ".to_owned(),
        _ => base,
    };
    let mut chars = base.chars();
    if let Some(first) = chars.next() {
        out.push(first);
        out.push(mark);
        out.extend(chars);
    }
    end
}

/// The words of a title for comparing a source title with its `.aux` copy: lower-case letters
/// and digits outside commands and math, separated by single spaces.
fn norm_words(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut math = false;
    let mut chars = title.chars().peekable();
    let mut gap = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if chars.peek().is_some_and(char::is_ascii_alphabetic) {
                    while chars.peek().is_some_and(char::is_ascii_alphabetic) {
                        chars.next();
                    }
                } else {
                    chars.next();
                }
                gap = true;
            }
            '$' => {
                math = !math;
                gap = true;
            }
            c if c.is_alphanumeric() && !math => {
                if gap && !out.is_empty() {
                    out.push(' ');
                }
                gap = false;
                out.extend(c.to_lowercase());
            }
            _ => gap = true,
        }
    }
    out
}

/// A row or contents line as [`align`] compares them.
#[derive(Clone, Debug)]
struct AlignKey {
    level: u8,
    words: String,
    /// Rows: unnumbered. Contents lines: without a number.
    unnumbered: bool,
}

/// Matches outline rows with the contents lines of a build, in order: for each row, the next
/// line (within [`ALIGN_WINDOW`]) of the same level and title; else, for a numbered row, the
/// next line of its level when that line's title is not about to come (the row's title was
/// edited since the build); else none (the row is new). Returns the line index per row.
fn align(rows: &[AlignKey], lines: &[AlignKey]) -> Vec<Option<usize>> {
    let mut out = vec![None; rows.len()];
    let mut t = 0;
    for (i, row) in rows.iter().enumerate() {
        let window = t..lines.len().min(t + ALIGN_WINDOW);
        if let Some(k) = window
            .clone()
            .find(|&k| lines[k].level == row.level && lines[k].words == row.words)
        {
            out[i] = Some(k);
            t = k + 1;
            continue;
        }
        if row.unnumbered || t >= lines.len() {
            continue;
        }
        let line = &lines[t];
        let upcoming = rows[i + 1..rows.len().min(i + 1 + ALIGN_WINDOW)]
            .iter()
            .any(|r| r.level == line.level && r.words == line.words);
        if line.level == row.level && !line.unnumbered && !upcoming {
            out[i] = Some(t);
            t += 1;
        }
    }
    out
}

/// One step of the document-order walk that [`build_outline`] makes.
enum Ev {
    Item(Entry),
    /// An included file: its row and the lowest heading level inside it.
    Enter(Entry, u8),
    Leave,
}

/// Builds the outline of `root`'s project from the project index, the `.aux` of the last build
/// and `caption(path, begin_line, end_line)`, which gives a float's caption from the live text
/// of an open document (or `None`).
pub fn build_outline(
    index: &ProjectIndex,
    root: &Path,
    aux: &AuxData,
    caption: &dyn Fn(&Path, u32, u32) -> Option<String>,
) -> Vec<Entry> {
    let root_dir = root.parent().map(lexical_normalize).unwrap_or_default();
    let mut builder = Builder {
        index,
        aux,
        caption,
        theorem_names: theorem_names(index),
        root_dir: root_dir.clone(),
        seen: HashSet::from([root.to_path_buf(), lexical_normalize(root)]),
    };
    let mut evs = builder.file_events(root, &root_dir, true, true, 0);
    number_events(&mut evs, aux);
    assemble(evs)
}

struct Builder<'a> {
    index: &'a ProjectIndex,
    aux: &'a AuxData,
    caption: &'a dyn Fn(&Path, u32, u32) -> Option<String>,
    theorem_names: HashMap<String, String>,
    root_dir: PathBuf,
    seen: HashSet<PathBuf>,
}

impl Builder<'_> {
    /// The events of `path`'s rows and those of the files it includes. `base` is the directory
    /// its includes are relative to; `in_body`: the file is read inside the document body.
    fn file_events(
        &mut self,
        path: &Path,
        base: &Path,
        in_body: bool,
        is_root: bool,
        depth: usize,
    ) -> Vec<Ev> {
        let Some(idx) = self.index.files.get(path) else {
            return Vec::new();
        };
        let idx = idx.clone();
        let body_from = if is_root {
            idx.envs
                .iter()
                .find(|e| e.name == "document")
                .map(|e| e.begin_line)
        } else {
            None
        };
        let in_preamble = |line: u32| body_from.is_some_and(|b| line < b);
        // Three runs in line order, merged at the end: headings and theorems, floats, files.
        let mut items: Vec<(u32, Ev)> = Vec::with_capacity(idx.outline.len());
        let mut float_items: Vec<(u32, Ev)> = Vec::new();
        let mut files: Vec<(u32, Vec<Ev>)> = Vec::new();
        // Headings and theorems.
        for (k, item) in idx.outline.iter().enumerate() {
            if in_preamble(item.line) {
                continue;
            }
            let next_line = idx.outline.get(k + 1).map_or(u32::MAX, |n| n.line);
            let entry = if item.level <= 6 {
                self.heading(path, &idx, item, next_line)
            } else {
                self.theorem(path, &idx, item)
            };
            items.push((item.line, Ev::Item(entry)));
        }
        // Floats.
        let floats: Vec<(u32, Option<u32>)> = idx
            .envs
            .iter()
            .filter(|e| float_kind(&e.name).is_some())
            .map(|e| (e.begin_line, e.end_line))
            .collect();
        for env in &idx.envs {
            let Some((name, _)) = float_kind(&env.name) else {
                continue;
            };
            if in_preamble(env.begin_line) {
                continue;
            }
            let entry = self.float(path, &idx, &env.name, name, env.begin_line, env.end_line);
            float_items.push((env.begin_line, Ev::Item(entry)));
        }
        // Included files.
        if depth < 64 {
            let root_dir = self.root_dir.clone();
            for inc in idx.includes.iter().filter(|i| is_tex_include(i.kind)) {
                let child = self.resolve(inc, &[base, &root_dir]);
                if !self.seen.insert(child.clone()) {
                    continue;
                }
                let in_float = floats
                    .iter()
                    .any(|&(b, e)| inc.line >= b && e.is_none_or(|e| inc.line <= e));
                let child_in_body = in_body && !in_preamble(inc.line) && !in_float;
                let child_base = match inc.kind {
                    IncludeKind::Subfile | IncludeKind::Import => {
                        child.parent().map(Path::to_path_buf).unwrap_or_default()
                    }
                    _ => base.to_path_buf(),
                };
                let indexed = self.index.files.contains_key(&child);
                let inner = self.file_events(&child, &child_base, child_in_body, false, depth + 1);
                let has_rows = inner.iter().any(|e| matches!(e, Ev::Item(_)));
                if !has_rows && !child_in_body {
                    continue;
                }
                let min_level = inner
                    .iter()
                    .filter_map(|e| match e {
                        Ev::Item(x) if x.kind == EntryKind::Heading => Some(x.level),
                        _ => None,
                    })
                    .min()
                    .unwrap_or(u8::MAX);
                let title = child
                    .strip_prefix(&self.root_dir)
                    .unwrap_or(&child)
                    .display()
                    .to_string();
                let file = Entry {
                    kind: EntryKind::File,
                    level: LEVEL_FILE,
                    number: None,
                    name: None,
                    title,
                    starred: false,
                    path: child.clone(),
                    line: 0,
                    end_line: None,
                    find: None,
                    indexed,
                    children: Vec::new(),
                };
                let mut events = Vec::with_capacity(inner.len() + 2);
                events.push(Ev::Enter(file, min_level));
                events.extend(inner);
                events.push(Ev::Leave);
                files.push((inc.line, events));
            }
        }
        // Each run is in line order already; on the same line, headings come first, then
        // floats, then files.
        let mut out = Vec::with_capacity(items.len() + float_items.len() + files.len());
        let mut items = items.into_iter().peekable();
        let mut float_items = float_items.into_iter().peekable();
        let mut files = files.into_iter().peekable();
        loop {
            let next = [
                items.peek().map(|(l, _)| (*l, 0)),
                float_items.peek().map(|(l, _)| (*l, 1)),
                files.peek().map(|(l, _)| (*l, 2)),
            ]
            .into_iter()
            .flatten()
            .min();
            match next {
                Some((_, 0)) => out.extend(items.next().map(|(_, e)| e)),
                Some((_, 1)) => out.extend(float_items.next().map(|(_, e)| e)),
                Some(_) => out.extend(files.next().into_iter().flat_map(|(_, e)| e)),
                None => return out,
            }
        }
    }

    fn heading(&self, path: &Path, idx: &FileIndex, item: &OutlineItem, next_line: u32) -> Entry {
        // A label right after the heading (its line or the next, before the next row).
        let number = if item.starred {
            None
        } else {
            let from = idx.labels.partition_point(|l| l.line < item.line);
            let last = (item.line + 1).min(next_line.saturating_sub(1).max(item.line));
            idx.labels[from..]
                .iter()
                .take_while(|l| l.line <= last)
                .find(|l| l.env.is_none() && l.title.as_deref() == Some(item.title.as_str()))
                .and_then(|l| self.label_number(&l.name))
        };
        Entry {
            kind: EntryKind::Heading,
            level: item.level,
            number,
            name: None,
            title: pretty_title(&item.title),
            starred: item.starred,
            path: path.to_path_buf(),
            line: item.line,
            end_line: None,
            find: Some(HEADING_COMMANDS[usize::from(item.level.min(6))].to_owned()),
            indexed: true,
            children: Vec::new(),
        }
    }

    fn theorem(&self, path: &Path, idx: &FileIndex, item: &OutlineItem) -> Entry {
        let env = item.env.clone().unwrap_or_default();
        let end_line = env_end(idx, &env, item.line);
        let from = idx.labels.partition_point(|l| l.line < item.line);
        let number = (!item.starred)
            .then(|| {
                idx.labels[from..]
                    .iter()
                    .take_while(|l| end_line.is_none_or(|e| l.line <= e))
                    .find(|l| l.env.as_deref() == Some(env.as_str()))
                    .and_then(|l| self.label_number(&l.name))
            })
            .flatten();
        Entry {
            kind: EntryKind::Theorem,
            level: LEVEL_THEOREM,
            number,
            name: Some(theorem_name(&self.theorem_names, &env)),
            title: pretty_title(&item.title),
            starred: item.starred,
            path: path.to_path_buf(),
            line: item.line,
            end_line,
            find: Some(format!("\\begin{{{env}}}")),
            indexed: true,
            children: Vec::new(),
        }
    }

    fn float(
        &self,
        path: &Path,
        idx: &FileIndex,
        env: &str,
        name: &str,
        begin: u32,
        end: Option<u32>,
    ) -> Entry {
        let from = idx.labels.partition_point(|l| l.line < begin);
        let label = idx.labels[from..]
            .iter()
            .take_while(|l| end.is_none_or(|e| l.line <= e))
            .find(|l| {
                l.env.as_deref().is_some_and(|e| {
                    e == env
                        || matches!(
                            e,
                            "center" | "centering" | "flushleft" | "flushright" | "minipage"
                        )
                })
            });
        let number = label.and_then(|l| self.label_number(&l.name));
        let last = end.unwrap_or(begin).min(begin + CAPTION_LINES);
        let title = (self.caption)(path, begin, last)
            .or_else(|| {
                // A label's title is the caption when the caption came first.
                let title = label?.title.as_deref()?;
                let heading = idx.outline.iter().any(|o| o.title == title);
                (!heading).then(|| pretty_title(title))
            })
            .unwrap_or_default();
        Entry {
            kind: EntryKind::Float,
            level: LEVEL_FLOAT,
            number,
            name: Some(name.to_owned()),
            title,
            starred: false,
            path: path.to_path_buf(),
            line: begin,
            end_line: end,
            find: Some(format!("\\begin{{{env}}}")),
            indexed: true,
            children: Vec::new(),
        }
    }

    fn label_number(&self, label: &str) -> Option<String> {
        self.aux
            .labels
            .get(label)
            .map(|(number, _)| number.trim().to_owned())
            .filter(|n| !n.is_empty())
    }

    /// The file `inc` names: the first candidate that is indexed in one of `dirs`, else the
    /// first candidate in the first directory (as `ProjectIndex::project_files` resolves).
    fn resolve(&self, inc: &Include, dirs: &[&Path]) -> PathBuf {
        let names = candidate_names(inc);
        let mut first = None;
        for dir in dirs {
            for name in &names {
                let path = lexical_join(dir, name);
                if self.index.files.contains_key(&path) {
                    return path;
                }
                first.get_or_insert(path);
            }
        }
        first.unwrap_or_default()
    }
}

/// The `\end` line of the environment `env` that begins on `line`.
fn env_end(idx: &FileIndex, env: &str, line: u32) -> Option<u32> {
    let from = idx.envs.partition_point(|e| e.begin_line < line);
    idx.envs[from..]
        .iter()
        .take_while(|e| e.begin_line == line)
        .find(|e| e.name == env)
        .and_then(|e| e.end_line)
}

/// The display names of the project's theorem-like environments (`\newtheorem{lem}{Lemma}`).
fn theorem_names(index: &ProjectIndex) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for file in index.files.values() {
        for m in file.macros.iter().filter(|m| m.is_theorem) {
            let title = m
                .body
                .as_deref()
                .map(pretty_title)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| default_theorem_name(&m.name));
            names.insert(m.name.clone(), title);
        }
    }
    names
}

fn theorem_name(names: &HashMap<String, String>, env: &str) -> String {
    let base = env.strip_suffix('*').unwrap_or(env);
    names
        .get(env)
        .or_else(|| names.get(base))
        .cloned()
        .unwrap_or_else(|| default_theorem_name(base))
}

/// What an environment that the project does not declare is called: the usual abbreviations
/// spelled out, else its name capitalized.
fn default_theorem_name(env: &str) -> String {
    let name = match env {
        "thm" => "Theorem",
        "lem" => "Lemma",
        "prop" => "Proposition",
        "cor" => "Corollary",
        "conj" => "Conjecture",
        "def" | "defi" | "defn" | "dfn" => "Definition",
        "rem" | "rmk" => "Remark",
        "ex" | "exa" | "expl" => "Example",
        "exer" | "exc" => "Exercise",
        "obs" => "Observation",
        "nota" => "Notation",
        "ques" | "qn" => "Question",
        "prob" => "Problem",
        "hyp" => "Hypothesis",
        "ass" | "assum" => "Assumption",
        "constr" => "Construction",
        "conv" => "Convention",
        _ => {
            let mut chars = env.chars();
            return match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => String::new(),
            };
        }
    };
    name.to_owned()
}

/// Gives the headings and floats without a label number the number of their contents line.
fn number_events(evs: &mut [Ev], aux: &AuxData) {
    let toc_level = |kind: &str| TOC_KINDS.iter().position(|k| *k == kind);
    // Headings.
    let heads: Vec<usize> = (0..evs.len())
        .filter(|&i| matches!(&evs[i], Ev::Item(e) if e.kind == EntryKind::Heading))
        .collect();
    fn item(ev: &Ev) -> Option<&Entry> {
        match ev {
            Ev::Item(e) => Some(e),
            _ => None,
        }
    }
    let rows: Vec<AlignKey> = heads
        .iter()
        .filter_map(|&i| item(&evs[i]))
        .map(|e| AlignKey {
            level: e.level,
            words: norm_words(&e.title),
            unnumbered: e.starred,
        })
        .collect();
    let lines: Vec<(AlignKey, &ContentsLine)> = aux
        .toc
        .iter()
        .filter_map(|c| {
            let level = toc_level(&c.kind)?;
            Some((
                AlignKey {
                    level: u8::try_from(level).ok()?,
                    words: norm_words(&c.title),
                    unnumbered: c.number.is_none(),
                },
                c,
            ))
        })
        .collect();
    let keys: Vec<AlignKey> = lines.iter().map(|(k, _)| k.clone()).collect();
    for (row, matched) in align(&rows, &keys).into_iter().enumerate() {
        if let Some(m) = matched
            && let Ev::Item(e) = &mut evs[heads[row]]
            && e.number.is_none()
            && !e.starred
        {
            e.number.clone_from(&lines[m].1.number);
        }
    }
    // Floats, per list.
    for (list, contents) in [
        (FloatList::Figures, &aux.lof),
        (FloatList::Tables, &aux.lot),
    ] {
        let floats: Vec<usize> = (0..evs.len())
            .filter(|&i| {
                matches!(&evs[i], Ev::Item(e) if e.kind == EntryKind::Float
                    && list_of(e.name.as_deref()) == list)
            })
            .collect();
        if floats.is_empty() {
            continue;
        }
        let rows: Vec<AlignKey> = floats
            .iter()
            .filter_map(|&i| item(&evs[i]))
            .map(|e| AlignKey {
                level: 0,
                words: norm_words(&e.title),
                unnumbered: false,
            })
            .collect();
        let entries: Vec<&ContentsLine> = contents
            .iter()
            .filter(|c| matches!(c.kind.as_str(), "figure" | "table"))
            .collect();
        let keys: Vec<AlignKey> = entries
            .iter()
            .map(|c| AlignKey {
                level: 0,
                words: norm_words(&c.title),
                unnumbered: c.number.is_none(),
            })
            .collect();
        for (row, matched) in align(&rows, &keys).into_iter().enumerate() {
            if let Some(m) = matched
                && let Ev::Item(e) = &mut evs[floats[row]]
            {
                if e.number.is_none() {
                    e.number.clone_from(&entries[m].number);
                }
                if e.title.is_empty() {
                    e.title = pretty_title(&entries[m].title);
                }
            }
        }
    }
}

/// Nests the events into a tree: headings by level, theorems and floats under the open
/// heading, files under the deepest open heading above their own headings.
fn assemble(evs: Vec<Ev>) -> Vec<Entry> {
    enum Frame {
        Heading(u8),
        File,
    }
    fn pop(stack: &mut Vec<(Frame, Entry)>, roots: &mut Vec<Entry>) {
        if let Some((_, entry)) = stack.pop() {
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(entry),
                None => roots.push(entry),
            }
        }
    }
    let mut roots = Vec::new();
    let mut stack: Vec<(Frame, Entry)> = Vec::new();
    for ev in evs {
        match ev {
            Ev::Item(entry) if entry.kind == EntryKind::Heading => {
                while matches!(stack.last(), Some((Frame::Heading(l), _)) if *l >= entry.level) {
                    pop(&mut stack, &mut roots);
                }
                stack.push((Frame::Heading(entry.level), entry));
            }
            Ev::Item(entry) => match stack.last_mut() {
                Some((_, parent)) => parent.children.push(entry),
                None => roots.push(entry),
            },
            Ev::Enter(file, min_level) => {
                while matches!(stack.last(), Some((Frame::Heading(l), _)) if *l >= min_level) {
                    pop(&mut stack, &mut roots);
                }
                stack.push((Frame::File, file));
            }
            Ev::Leave => {
                while let Some((frame, _)) = stack.last() {
                    let file = matches!(frame, Frame::File);
                    pop(&mut stack, &mut roots);
                    if file {
                        break;
                    }
                }
            }
        }
    }
    while !stack.is_empty() {
        pop(&mut stack, &mut roots);
    }
    roots
}

fn is_tex_include(kind: IncludeKind) -> bool {
    matches!(
        kind,
        IncludeKind::Input | IncludeKind::Include | IncludeKind::Subfile | IncludeKind::Import
    )
}

/// The file names a TeX include may refer to, most likely first (as `lemma_syntax`'s project
/// walk tries them).
fn candidate_names(inc: &Include) -> Vec<String> {
    let t = inc.target.as_str();
    let ext = Path::new(t)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match (inc.kind, ext.as_deref()) {
        (_, Some("tex")) => vec![t.to_owned()],
        (_, None) | (IncludeKind::Include, _) => vec![format!("{t}.tex")],
        (_, Some(_)) => vec![t.to_owned(), format!("{t}.tex")],
    }
}

fn lexical_join(dir: &Path, name: &str) -> PathBuf {
    lexical_normalize(&dir.join(name))
}

/// Folds `.` and `..` without touching the file system.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The braced group at `s[from..]` after blanks: its contents and where it ends.
fn group(s: &str, from: usize) -> Option<(Range<usize>, usize)> {
    delimited(s, from, b'{', b'}')
}

/// The bracketed optional argument at `s[from..]` after blanks (braces nest inside it).
fn bracket(s: &str, from: usize) -> Option<(Range<usize>, usize)> {
    delimited(s, from, b'[', b']')
}

fn delimited(s: &str, from: usize, open: u8, close: u8) -> Option<(Range<usize>, usize)> {
    let b = s.as_bytes();
    let mut i = from;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&open) {
        return None;
    }
    let start = i + 1;
    let mut depth = 0usize;
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            c if c == close && depth == 0 => return Some((start..j, j + 1)),
            _ => {}
        }
        j += 1;
    }
    None
}

fn starts_with_letter(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ───────────────────────── The page (GTK) ─────────────────────────

/// How often the watch looks at the project index while something may change it.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// Ticks without a change (and a quiet project feed) after which the watch stops.
const QUIET_TICKS: u32 = 2;
/// How long the cursor rests before the outline scrolls to its row.
const SCROLL_DELAY: Duration = Duration::from_millis(120);
/// Most rows the filter shows.
const MAX_RESULTS: usize = 300;
/// Most remembered collapsed rows.
const MAX_COLLAPSED: usize = 4096;

const CSS: &str = "
.lemma-outline-header { margin: 6px 6px 2px 6px; }
.lemma-outline listview.navigation-sidebar > row { min-height: 28px; padding-top: 0; padding-bottom: 0; }
.lemma-outline-row { padding-top: 3px; padding-bottom: 3px; }
.lemma-outline-row .number { font-feature-settings: 'tnum'; }
.lemma-outline-row.kind-heading .number { color: alpha(currentColor, 0.55); }
.lemma-outline-row.top label { font-weight: bold; }
.lemma-outline-row.top .number { color: alpha(currentColor, 0.8); }
.lemma-outline-row.starred .number { color: alpha(currentColor, 0.4); }
.lemma-outline-row.kind-theorem, .lemma-outline-row.kind-float { font-size: 0.94em; }
.lemma-outline-row.kind-theorem .title, .lemma-outline-row.kind-float .title { color: alpha(currentColor, 0.62); }
.lemma-outline-row.kind-float image, .lemma-outline-row.kind-file image { color: alpha(currentColor, 0.62); }
.lemma-outline-row.kind-file .title { color: alpha(currentColor, 0.75); }
.lemma-outline-row.kind-file.unread .title { font-style: italic; color: alpha(currentColor, 0.5); }
";

/// A row of the tree: an [`Entry`] without its children, plus the model of its children.
struct Node {
    kind: EntryKind,
    level: u8,
    path: PathBuf,
    data: RefCell<NodeData>,
    line: Cell<u32>,
    end_line: Cell<Option<u32>>,
    /// Headings and files: the tree's child model (`BoxedAnyObject`s of `Rc<Node>`).
    children: Option<gio::ListStore>,
    child_nodes: RefCell<Vec<Rc<Node>>>,
    parent: RefCell<Weak<Node>>,
    /// The row widgets showing this node (refreshed when it changes).
    rows: RefCell<Vec<Weak<RowWidgets>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeData {
    number: Option<String>,
    name: Option<String>,
    title: String,
    starred: bool,
    indexed: bool,
    find: Option<String>,
}

/// What collapse states are remembered by: the row's file, kind, level and title.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodeKey {
    path: PathBuf,
    kind: EntryKind,
    level: u8,
    title: String,
}

impl Node {
    fn new(entry: &mut Entry, parent: Weak<Node>, created: &mut Vec<Rc<Node>>) -> Rc<Node> {
        let has_children = matches!(entry.kind, EntryKind::Heading | EntryKind::File);
        let node = Rc::new(Node {
            kind: entry.kind,
            level: entry.level,
            path: entry.path.clone(),
            data: RefCell::new(NodeData::of(entry)),
            line: Cell::new(entry.line),
            end_line: Cell::new(entry.end_line),
            children: has_children.then(gio::ListStore::new::<glib::BoxedAnyObject>),
            child_nodes: RefCell::new(Vec::new()),
            parent: RefCell::new(parent),
            rows: RefCell::new(Vec::new()),
        });
        created.push(node.clone());
        if let Some(store) = &node.children {
            let kids: Vec<Rc<Node>> = std::mem::take(&mut entry.children)
                .iter_mut()
                .map(|child| Node::new(child, Rc::downgrade(&node), created))
                .collect();
            let objects: Vec<glib::BoxedAnyObject> = kids.iter().map(Node::object).collect();
            store.splice(0, 0, &objects);
            node.child_nodes.replace(kids);
        }
        node
    }

    fn object(node: &Rc<Node>) -> glib::BoxedAnyObject {
        glib::BoxedAnyObject::new(node.clone())
    }

    fn key(&self) -> NodeKey {
        NodeKey {
            path: self.path.clone(),
            kind: self.kind,
            level: self.level,
            title: self.data.borrow().title.clone(),
        }
    }

    /// Same row: kind, level, file and title.
    fn same(&self, entry: &Entry) -> bool {
        self.same_shape(entry) && self.data.borrow().title == entry.title
    }

    /// Same kind of row in the same file (an edited title).
    fn same_shape(&self, entry: &Entry) -> bool {
        self.kind == entry.kind && self.level == entry.level && self.path == entry.path
    }

    fn parent(&self) -> Option<Rc<Node>> {
        self.parent.borrow().upgrade()
    }

    fn number_text(&self) -> String {
        let d = self.data.borrow();
        number_text(
            self.kind,
            self.level,
            d.number.as_deref(),
            d.name.as_deref(),
            d.starred,
        )
    }

    fn title_text(&self) -> String {
        title_text(self.kind, &self.data.borrow().title)
    }

    fn text(&self) -> String {
        row_text(self.kind, &self.number_text(), &self.title_text())
    }

    /// Re-renders the rows showing this node.
    fn refresh_rows(&self) {
        let rows: Vec<Rc<RowWidgets>> = {
            let mut rows = self.rows.borrow_mut();
            rows.retain(|r| r.strong_count() > 0);
            rows.iter().filter_map(Weak::upgrade).collect()
        };
        for row in rows {
            row.render();
        }
    }

    /// Every node of the subtree, depth first (`self` first).
    fn descendants(self: &Rc<Self>, out: &mut Vec<Rc<Node>>) {
        out.push(self.clone());
        for child in self.child_nodes.borrow().iter() {
            child.descendants(out);
        }
    }
}

impl NodeData {
    /// Whether this is what `entry` shows (without copying it).
    fn equals(&self, entry: &Entry) -> bool {
        self.number == entry.number
            && self.name == entry.name
            && self.title == entry.title
            && self.starred == entry.starred
            && self.indexed == entry.indexed
            && self.find == entry.find
    }

    fn of(entry: &Entry) -> Self {
        Self {
            number: entry.number.clone(),
            name: entry.name.clone(),
            title: entry.title.clone(),
            starred: entry.starred,
            indexed: entry.indexed,
            find: entry.find.clone(),
        }
    }
}

fn node_of(object: &glib::Object) -> Option<Rc<Node>> {
    let boxed = object.downcast_ref::<glib::BoxedAnyObject>()?;
    boxed.try_borrow::<Rc<Node>>().ok().map(|n| n.clone())
}

/// The widgets of one list row.
struct RowWidgets {
    expander: gtk::TreeExpander,
    content: gtk::Box,
    icon: gtk::Image,
    number: gtk::Label,
    title: gtk::Label,
    /// A row of the filter results (matches in bold, no expander).
    results: bool,
    panel: Weak<Panel>,
    node: RefCell<Option<Rc<Node>>>,
    tree_row: RefCell<Option<(gtk::TreeListRow, glib::SignalHandlerId)>>,
    store_handler: RefCell<Option<(gio::ListStore, glib::SignalHandlerId)>>,
}

impl RowWidgets {
    fn new(panel: Weak<Panel>, results: bool) -> Rc<Self> {
        let icon = gtk::Image::builder().pixel_size(16).visible(false).build();
        let number = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["number"])
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["title"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.add_css_class("lemma-outline-row");
        content.append(&icon);
        content.append(&number);
        content.append(&title);
        let expander = gtk::TreeExpander::builder()
            .child(&content)
            .indent_for_icon(!results)
            .indent_for_depth(!results)
            .build();
        let row = Rc::new(Self {
            expander,
            content,
            icon,
            number,
            title,
            results,
            panel,
            node: RefCell::new(None),
            tree_row: RefCell::new(None),
            store_handler: RefCell::new(None),
        });
        // A click on the row's text (not on its arrow) goes to its line.
        let click = gtk::GestureClick::builder().button(1).build();
        let pressed = Rc::new(Cell::new(false));
        let p = pressed.clone();
        click.connect_pressed(move |_, n, _, _| p.set(n == 1));
        let p = pressed.clone();
        click.connect_stopped(move |_| p.set(false));
        let weak = Rc::downgrade(&row);
        click.connect_released(move |gesture, n, x, y| {
            let inside = gesture.widget().is_some_and(|w| {
                x >= 0.0 && y >= 0.0 && x < w.width().into() && y < w.height().into()
            });
            if n == 1
                && pressed.replace(false)
                && inside
                && let Some(row) = weak.upgrade()
                && let Some(node) = row.node.borrow().clone()
                && let Some(panel) = row.panel.upgrade()
            {
                panel.activate_node(&node);
            }
        });
        row.content.add_controller(click);
        row.content.set_has_tooltip(true);
        let weak = Rc::downgrade(&row);
        row.content
            .connect_query_tooltip(move |_, _, _, _, tooltip| {
                let Some(row) = weak.upgrade() else {
                    return false;
                };
                let Some(node) = row.node.borrow().clone() else {
                    return false;
                };
                let Some(panel) = row.panel.upgrade() else {
                    return false;
                };
                tooltip.set_text(Some(&panel.describe_location(&node)));
                true
            });
        row
    }

    /// Shows `item` (a `TreeListRow` of the tree, or a node object of the results), or nothing.
    fn bind(self: &Rc<Self>, item: Option<glib::Object>) {
        self.unbind();
        let Some(item) = item else {
            return;
        };
        let (node, tree_row) = match item.downcast::<gtk::TreeListRow>() {
            Ok(row) => (row.item().as_ref().and_then(node_of), Some(row)),
            Err(item) => (node_of(&item), None),
        };
        let Some(node) = node else {
            return;
        };
        self.expander.set_list_row(tree_row.as_ref());
        if let Some(row) = tree_row {
            let weak = Rc::downgrade(self);
            let id = row.connect_expanded_notify(move |row| {
                // Rows are destroyed (and notify) while the model changes: look later.
                if let Some(me) = weak.upgrade()
                    && let Some(node) = me.node.borrow().as_ref()
                    && let Some(panel) = me.panel.upgrade()
                {
                    panel.expansion_notified(row, node);
                }
            });
            self.tree_row.replace(Some((row, id)));
        }
        if !self.results
            && let Some(store) = &node.children
        {
            let expander = self.expander.downgrade();
            let id = store.connect_items_changed(move |store, _, _, _| {
                if let Some(expander) = expander.upgrade() {
                    expander.set_hide_expander(store.n_items() == 0);
                }
            });
            self.store_handler.replace(Some((store.clone(), id)));
        }
        node.rows.borrow_mut().push(Rc::downgrade(self));
        self.node.replace(Some(node));
        self.render();
    }

    fn unbind(self: &Rc<Self>) {
        if let Some((row, id)) = self.tree_row.take() {
            row.disconnect(id);
        }
        if let Some((store, id)) = self.store_handler.take() {
            store.disconnect(id);
        }
        if let Some(node) = self.node.take() {
            let me = Rc::downgrade(self);
            node.rows.borrow_mut().retain(|r| !r.ptr_eq(&me));
        }
        self.expander.set_list_row(None);
    }

    fn render(&self) {
        let Some(node) = self.node.borrow().clone() else {
            return;
        };
        let number = node.number_text();
        let title = node.title_text();
        let marks = if self.results {
            self.panel
                .upgrade()
                .and_then(|p| p.marks.borrow().get(&Rc::as_ptr(&node)).cloned())
        } else {
            None
        };
        match marks {
            Some(marks) => {
                // Marks are byte offsets into "number title" (or the title alone).
                let offset = if number.is_empty() {
                    0
                } else {
                    number.len() + 1
                };
                let color = self
                    .panel
                    .upgrade()
                    .map(|p| p.match_color.borrow().clone())
                    .unwrap_or_default();
                self.number
                    .set_markup(&highlight(&number, 0, &marks, &color));
                self.title
                    .set_markup(&highlight(&title, offset, &marks, &color));
            }
            None => {
                self.number.set_text(&number);
                self.title.set_text(&title);
            }
        }
        self.number.set_visible(!number.is_empty());
        let data = node.data.borrow();
        let icon = match node.kind {
            EntryKind::File => Some("text-x-generic-symbolic"),
            EntryKind::Float => Some(match data.name.as_deref() {
                Some("Table") => "x-office-spreadsheet-symbolic",
                Some("Figure") => "image-x-generic-symbolic",
                _ => "view-list-ordered-symbolic",
            }),
            _ => None,
        };
        self.icon.set_visible(icon.is_some());
        if let Some(icon) = icon {
            self.icon.set_icon_name(Some(icon));
        }
        let mut classes = vec!["lemma-outline-row"];
        let level_class;
        match node.kind {
            EntryKind::Heading => {
                classes.push("kind-heading");
                level_class = format!("level-{}", node.level);
                classes.push(&level_class);
                // Parts and chapters, and the top level of the document (sections of an
                // article) stand out.
                let top = self.panel.upgrade().map_or(2, |p| p.top_level.get());
                if node.level <= 1 || node.level == top {
                    classes.push("top");
                }
            }
            EntryKind::Theorem => classes.push("kind-theorem"),
            EntryKind::Float => classes.push("kind-float"),
            EntryKind::File => {
                classes.push("kind-file");
                if !data.indexed {
                    classes.push("unread");
                }
            }
        }
        if data.starred {
            classes.push("starred");
        }
        self.content.set_css_classes(&classes);
        if !self.results {
            self.expander.set_hide_expander(
                node.children
                    .as_ref()
                    .is_none_or(|store| store.n_items() == 0),
            );
        }
    }
}

/// `text` as Pango markup with the characters at byte `marks - offset` in bold, in `color`
/// (`#rrggbb`; empty: the text's colour).
fn highlight(text: &str, offset: usize, marks: &[usize], color: &str) -> String {
    let open = if color.is_empty() {
        "<b>".to_owned()
    } else {
        format!("<span weight=\"bold\" foreground=\"{color}\">")
    };
    let close = if color.is_empty() { "</b>" } else { "</span>" };
    let mut out = String::new();
    let mut on = false;
    for (i, c) in text.char_indices() {
        let mark = marks.contains(&(i + offset));
        if mark != on {
            out.push_str(if mark { &open } else { close });
            on = mark;
        }
        out.push_str(&glib::markup_escape_text(c.encode_utf8(&mut [0; 4])));
    }
    if on {
        out.push_str(close);
    }
    out
}

/// Counters for tests and the performance test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutlineStats {
    /// Outline recomputations (each followed by a patch of the models).
    pub syncs: u64,
    /// Row objects created in total, and updated in place.
    pub nodes_created: u64,
    pub nodes_updated: u64,
    /// The cost of the last and the slowest recomputation plus patch.
    pub last_sync: Duration,
    pub max_sync: Duration,
    /// The part of the last recomputation spent building the outline (no GTK).
    pub last_build: Duration,
    /// Work done for keystrokes: document-change and line-shift observers, cursor moves and the
    /// `follow` they queue (count, total and slowest single call).
    pub key_calls: u64,
    pub key_time: Duration,
    pub max_key_call: Duration,
    /// `.aux` reads finished.
    pub aux_reads: u64,
    /// Scrolls of the tree to the followed row (after the cursor rested), and the slowest.
    pub scrolls: u64,
    pub max_scroll: Duration,
}

/// The Outline page of one window.
struct Panel {
    state: Weak<WindowState>,
    page: gtk::Box,
    entry: gtk::SearchEntry,
    stack: gtk::Stack,
    tree_view: gtk::ListView,
    tree_model: gtk::TreeListModel,
    tree_selection: gtk::SingleSelection,
    root_store: gio::ListStore,
    root_nodes: RefCell<Vec<Rc<Node>>>,
    results_view: gtk::ListView,
    results_store: gio::ListStore,
    results_selection: gtk::SingleSelection,
    follow_action: gio::SimpleAction,
    /// The highest heading level of the outline (its rows are emphasized).
    top_level: Cell<u8>,
    /// Match positions of the filter results, by node, and their colour (the accent colour).
    marks: RefCell<HashMap<*const Node, Vec<usize>>>,
    match_color: RefCell<String>,
    /// The project root shown, and what the shown outline was computed from.
    root: RefCell<Option<PathBuf>>,
    inputs: RefCell<Option<Inputs>>,
    /// The `.aux` of the root (`aux_root`), bumped `aux_generation` at every read.
    aux: RefCell<Rc<AuxData>>,
    aux_root: RefCell<Option<PathBuf>>,
    aux_generation: Cell<u64>,
    aux_reading: Cell<bool>,
    aux_stale: Cell<bool>,
    aux_hint: RefCell<Option<PathBuf>>,
    /// Rows per file in line order (headings, theorems, floats), and file rows by path.
    by_file: RefCell<HashMap<PathBuf, Vec<Rc<Node>>>>,
    file_nodes: RefCell<HashMap<PathBuf, Rc<Node>>>,
    collapsed: RefCell<HashSet<NodeKey>>,
    /// The watch timer and its quiet ticks.
    watch: RefCell<Option<glib::SourceId>>,
    quiet: Cell<u32>,
    follow_idle: RefCell<Option<glib::SourceId>>,
    scroll_timer: RefCell<Option<glib::SourceId>>,
    reapply_idle: RefCell<Option<glib::SourceId>>,
    reapply: RefCell<Vec<Weak<Node>>>,
    /// Rows whose `expanded` changed, checked from an idle (see `expansion_notified`).
    expansions: RefCell<Vec<(gtk::TreeListRow, Weak<Node>)>>,
    expansion_idle: RefCell<Option<glib::SourceId>>,
    observed: RefCell<Vec<Observed>>,
    cursor: RefCell<Option<(glib::WeakRef<sourceview5::Buffer>, glib::SignalHandlerId)>>,
    stats: Cell<OutlineStats>,
}

/// An open document the page observes: the document, its path and the observers.
type Observed = (Weak<crate::document::Document>, PathBuf, Vec<ObserverId>);

/// What the shown outline was computed from.
struct Inputs {
    /// Weak, so an index that was replaced is freed (only its allocation stays, which keeps its
    /// address from being reused by a newer one).
    files: Vec<(PathBuf, std::sync::Weak<FileIndex>)>,
    active: Option<PathBuf>,
    build_root: Option<PathBuf>,
    aux_generation: u64,
}

impl Inputs {
    fn same(
        &self,
        index: &ProjectIndex,
        active: Option<&Path>,
        build_root: Option<&Path>,
        aux_generation: u64,
    ) -> bool {
        self.aux_generation == aux_generation
            && self.active.as_deref() == active
            && self.build_root.as_deref() == build_root
            && self.files.len() == index.files.len()
            && self
                .files
                .iter()
                .zip(&index.files)
                .all(|((p, a), (q, b))| p == q && std::ptr::eq(a.as_ptr(), Arc::as_ptr(b)))
    }
}

thread_local! {
    static PANELS: RefCell<Vec<Rc<Panel>>> = const { RefCell::new(Vec::new()) };
}

fn panel_of(state: &WindowState) -> Option<Rc<Panel>> {
    PANELS.with_borrow_mut(|panels| {
        panels.retain(|p| p.state.strong_count() > 0);
        panels
            .iter()
            .find(|p| std::ptr::eq(p.state.as_ptr(), state))
            .cloned()
    })
}

impl Panel {
    fn new(state: &Rc<WindowState>) -> Rc<Panel> {
        Rc::new_cyclic(|weak: &Weak<Panel>| {
            let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
            let tree_model = gtk::TreeListModel::new(root_store.clone(), false, true, |object| {
                node_of(object)
                    .and_then(|n| n.children.clone())
                    .map(|s| s.upcast())
            });
            let tree_selection = gtk::SingleSelection::builder()
                .model(&tree_model)
                .autoselect(false)
                .can_unselect(true)
                .build();
            let tree_view = gtk::ListView::builder()
                .model(&tree_selection)
                .factory(&row_factory(weak.clone(), false))
                .css_classes(["navigation-sidebar"])
                .build();
            tree_view.update_property(&[gtk::accessible::Property::Label("Outline")]);
            let tree_scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&tree_view)
                .build();
            let results_store = gio::ListStore::new::<glib::BoxedAnyObject>();
            let results_selection = gtk::SingleSelection::builder()
                .model(&results_store)
                .autoselect(true)
                .build();
            let results_view = gtk::ListView::builder()
                .model(&results_selection)
                .factory(&row_factory(weak.clone(), true))
                .css_classes(["navigation-sidebar"])
                .build();
            results_view.update_property(&[gtk::accessible::Property::Label("Matching Rows")]);
            let results_scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&results_view)
                .build();
            let empty = adw::StatusPage::builder()
                .icon_name("view-list-bullet-symbolic")
                .title("No Outline")
                .description("Sections, theorems and figures of the project appear here.")
                .css_classes(["compact", "dim-label"])
                .vexpand(true)
                .build();
            let none = adw::StatusPage::builder()
                .icon_name("edit-find-symbolic")
                .title("No Matches")
                .description("No section, theorem, figure or file matches the filter.")
                .css_classes(["compact", "dim-label"])
                .vexpand(true)
                .build();
            let stack = gtk::Stack::new();
            stack.add_named(&empty, Some("empty"));
            stack.add_named(&tree_scroller, Some("tree"));
            stack.add_named(&results_scroller, Some("results"));
            stack.add_named(&none, Some("none"));
            stack.set_visible_child_name("empty");

            let entry = gtk::SearchEntry::builder()
                .placeholder_text("Filter Outline")
                .search_delay(60)
                .hexpand(true)
                .build();
            entry.update_property(&[gtk::accessible::Property::Label("Filter the outline")]);
            let menu = gio::Menu::new();
            let all = gio::Menu::new();
            all.append(Some("_Expand All"), Some("outline.expand-all"));
            all.append(Some("_Collapse All"), Some("outline.collapse-all"));
            menu.append_section(None, &all);
            let follow = gio::Menu::new();
            follow.append(Some("_Follow Cursor"), Some("outline.follow-cursor"));
            menu.append_section(None, &follow);
            let menu_button = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .menu_model(&menu)
                .tooltip_text("Outline Options")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .build();
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            header.add_css_class("lemma-outline-header");
            header.append(&entry);
            header.append(&menu_button);
            let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
            page.add_css_class("lemma-outline");
            page.append(&header);
            page.append(&stack);
            entry.set_key_capture_widget(Some(&tree_view));

            let follow_action = gio::SimpleAction::new_stateful(
                "follow-cursor",
                None,
                &state.config.outline.follow_cursor.to_variant(),
            );
            Panel {
                state: Rc::downgrade(state),
                page,
                entry,
                stack,
                tree_view,
                tree_model,
                tree_selection,
                root_store,
                root_nodes: RefCell::new(Vec::new()),
                results_view,
                results_store,
                results_selection,
                follow_action,
                top_level: Cell::new(2),
                marks: RefCell::new(HashMap::new()),
                match_color: RefCell::new(String::new()),
                root: RefCell::new(None),
                inputs: RefCell::new(None),
                aux: RefCell::new(Rc::new(AuxData::default())),
                aux_root: RefCell::new(None),
                aux_generation: Cell::new(0),
                aux_reading: Cell::new(false),
                aux_stale: Cell::new(false),
                aux_hint: RefCell::new(None),
                by_file: RefCell::new(HashMap::new()),
                file_nodes: RefCell::new(HashMap::new()),
                collapsed: RefCell::new(HashSet::new()),
                watch: RefCell::new(None),
                quiet: Cell::new(0),
                follow_idle: RefCell::new(None),
                scroll_timer: RefCell::new(None),
                reapply_idle: RefCell::new(None),
                reapply: RefCell::new(Vec::new()),
                expansions: RefCell::new(Vec::new()),
                expansion_idle: RefCell::new(None),
                observed: RefCell::new(Vec::new()),
                cursor: RefCell::new(None),
                stats: Cell::new(OutlineStats::default()),
            }
        })
    }

    fn state(&self) -> Option<Rc<WindowState>> {
        self.state.upgrade()
    }

    fn shown(&self) -> bool {
        self.page.is_mapped()
    }

    fn update_stats(&self, f: impl FnOnce(&mut OutlineStats)) {
        let mut stats = self.stats.get();
        f(&mut stats);
        self.stats.set(stats);
    }

    /// Accounts the time since `started` as keystroke work.
    fn key_work(&self, started: Instant) {
        let spent = started.elapsed();
        self.update_stats(|s| {
            s.key_calls += 1;
            s.key_time += spent;
            s.max_key_call = s.max_key_call.max(spent);
        });
    }

    // ----- wiring -----

    fn connect(self: &Rc<Self>, state: &Rc<WindowState>) {
        let weak = Rc::downgrade(self);
        self.page.connect_map(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.poke();
                panel.check();
                panel.follow_soon();
            }
        });
        let weak = Rc::downgrade(self);
        state.tab_view.connect_selected_page_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.track_active();
                panel.poke();
            }
        });
        let weak = Rc::downgrade(self);
        state.tab_view.connect_page_attached(move |_, _, _| {
            if let Some(panel) = weak.upgrade() {
                panel.observe_documents();
                panel.poke();
            }
        });
        let weak = Rc::downgrade(self);
        state.tab_view.connect_page_detached(move |_, _, _| {
            if let Some(panel) = weak.upgrade() {
                panel.observe_documents();
                panel.poke();
            }
        });
        let weak = Rc::downgrade(self);
        state.connect_root_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.poke();
            }
        });
        let weak = Rc::downgrade(self);
        state.connect_config_changed(move |state, _| {
            if let Some(panel) = weak.upgrade() {
                let on = state.config.outline.follow_cursor;
                panel.follow_action.set_state(&on.to_variant());
                panel.follow_soon();
            }
        });
        if let Some(build) = state.build.get() {
            let weak = Rc::downgrade(self);
            build.connect_finished(move |outcome| {
                if let Some(panel) = weak.upgrade() {
                    panel.aux_stale.set(true);
                    panel
                        .aux_hint
                        .replace(outcome.pdf.as_ref().map(|p| p.with_extension("aux")));
                    panel.poke();
                }
            });
        }
        // Rows.
        let weak = Rc::downgrade(self);
        self.tree_view.connect_activate(move |_, position| {
            if let Some(panel) = weak.upgrade()
                && let Some(node) = panel.tree_node_at(position)
            {
                panel.activate_node(&node);
            }
        });
        let weak = Rc::downgrade(self);
        self.results_view.connect_activate(move |_, position| {
            if let Some(panel) = weak.upgrade()
                && let Some(node) = panel
                    .results_store
                    .item(position)
                    .as_ref()
                    .and_then(node_of)
            {
                panel.activate_node(&node);
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let plain = !modifiers.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK,
            );
            match weak.upgrade() {
                Some(panel) if plain => panel.tree_key(key),
                _ => glib::Propagation::Proceed,
            }
        });
        self.tree_view.add_controller(keys);
        // The filter.
        let weak = Rc::downgrade(self);
        self.entry.connect_search_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.refilter();
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade()
                && let Some(node) = panel
                    .results_selection
                    .selected_item()
                    .as_ref()
                    .and_then(node_of)
            {
                panel.activate_node(&node);
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_stop_search(move |entry| {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            if entry.text().is_empty() {
                if let Some(editor) = panel.state().and_then(|s| s.active_editor()) {
                    editor.view().grab_focus();
                }
            } else {
                entry.set_text("");
            }
        });
        let entry_keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        entry_keys.connect_key_pressed(move |_, key, _, _| {
            let Some(panel) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let step: i64 = match key {
                gtk::gdk::Key::Down | gtk::gdk::Key::KP_Down => 1,
                gtk::gdk::Key::Up | gtk::gdk::Key::KP_Up => -1,
                _ => return glib::Propagation::Proceed,
            };
            panel.move_result(step);
            glib::Propagation::Stop
        });
        self.entry.add_controller(entry_keys);
        // The menu.
        let group = gio::SimpleActionGroup::new();
        let expand = gio::SimpleAction::new("expand-all", None);
        let weak = Rc::downgrade(self);
        expand.connect_activate(move |_, _| {
            if let Some(panel) = weak.upgrade() {
                panel.expand_all();
            }
        });
        let collapse = gio::SimpleAction::new("collapse-all", None);
        let weak = Rc::downgrade(self);
        collapse.connect_activate(move |_, _| {
            if let Some(panel) = weak.upgrade() {
                panel.collapse_all();
            }
        });
        let weak = Rc::downgrade(self);
        self.follow_action.connect_activate(move |action, _| {
            let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(true);
            if let Some(panel) = weak.upgrade() {
                panel.set_follow_cursor(on);
            }
        });
        group.add_action(&expand);
        group.add_action(&collapse);
        group.add_action(&self.follow_action);
        self.page.insert_action_group("outline", Some(&group));
        // Teardown.
        let weak = Rc::downgrade(self);
        state.window.connect_destroy(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.teardown();
            }
        });
        self.track_active();
        self.observe_documents();
        self.poke();
    }

    fn teardown(self: &Rc<Self>) {
        for timer in [
            self.watch.take(),
            self.follow_idle.take(),
            self.scroll_timer.take(),
            self.reapply_idle.take(),
            self.expansion_idle.take(),
        ]
        .into_iter()
        .flatten()
        {
            timer.remove();
        }
        for (doc, _, ids) in self.observed.take() {
            if let Some(doc) = doc.upgrade() {
                for id in ids {
                    doc.disconnect(id);
                }
            }
        }
        if let Some((buffer, id)) = self.cursor.take()
            && let Some(buffer) = buffer.upgrade()
        {
            buffer.disconnect(id);
        }
        PANELS.with_borrow_mut(|panels| panels.retain(|p| !Rc::ptr_eq(p, self)));
    }

    /// Follows the cursor of the selected tab's buffer.
    fn track_active(self: &Rc<Self>) {
        if let Some((buffer, id)) = self.cursor.take()
            && let Some(buffer) = buffer.upgrade()
        {
            buffer.disconnect(id);
        }
        if let Some(editor) = self.state().and_then(|s| s.active_editor()) {
            let buffer = editor.buffer().clone();
            let weak = Rc::downgrade(self);
            let id = buffer.connect_cursor_position_notify(move |_| {
                if let Some(panel) = weak.upgrade()
                    && panel.shown()
                {
                    let started = Instant::now();
                    panel.follow_soon();
                    panel.key_work(started);
                }
            });
            self.cursor.replace(Some((buffer.downgrade(), id)));
        }
        self.follow_soon();
    }

    /// Observes the open documents: a change arms the watch, a change of the line count shifts
    /// the file's rows.
    fn observe_documents(self: &Rc<Self>) {
        let Some(state) = self.state() else {
            return;
        };
        let open: Vec<DocRef> = state
            .editors()
            .iter()
            .map(|e| e.document().clone())
            .collect();
        let mut observed = self.observed.borrow_mut();
        observed.retain(|(doc, _, ids)| match doc.upgrade() {
            Some(doc) if open.iter().any(|o| Rc::ptr_eq(o, &doc)) => true,
            Some(doc) => {
                for id in ids {
                    doc.disconnect(*id);
                }
                false
            }
            None => false,
        });
        for doc in open {
            if observed
                .iter()
                .any(|(d, _, _)| d.upgrade().is_some_and(|d| Rc::ptr_eq(&d, &doc)))
            {
                continue;
            }
            let path = doc.path().to_path_buf();
            let weak = Rc::downgrade(self);
            let changed = doc.connect_changed(move |_, _, _| {
                if let Some(panel) = weak.upgrade() {
                    let started = Instant::now();
                    panel.poke();
                    panel.key_work(started);
                }
            });
            let weak = Rc::downgrade(self);
            let key = path.clone();
            let lines = doc.connect_lines_changed(move |first, removed, added| {
                if let Some(panel) = weak.upgrade()
                    && removed != added
                {
                    let started = Instant::now();
                    panel.shift_lines(&key, first, removed, added);
                    panel.key_work(started);
                }
            });
            observed.push((Rc::downgrade(&doc), path, vec![changed, lines]));
        }
    }

    /// Lines `[first, first + removed)` of `path` became `[first, first + added)`: moves the
    /// rows after them (the project index catches up 300 ms after typing stops).
    fn shift_lines(&self, path: &Path, first: u32, removed: u32, added: u32) {
        let by_file = self.by_file.borrow();
        let Some(nodes) = by_file.get(path) else {
            return;
        };
        let shift = |line: u32| -> u32 {
            if line < first {
                line
            } else if line >= first + removed {
                line + added - removed
            } else {
                line.min(first + added.saturating_sub(1))
            }
        };
        for node in nodes {
            node.line.set(shift(node.line.get()));
            node.end_line.set(node.end_line.get().map(shift));
        }
    }

    // ----- the watch -----

    /// Something may have changed the project index: watch it until things are quiet.
    fn poke(self: &Rc<Self>) {
        self.quiet.set(0);
        if self.watch.borrow().is_some() || !self.shown() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(WATCH_INTERVAL, move || {
            let Some(panel) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if panel.tick() {
                glib::ControlFlow::Continue
            } else {
                panel.watch.replace(None);
                glib::ControlFlow::Break
            }
        });
        self.watch.replace(Some(id));
    }

    /// One look at the index; false when the watch can stop.
    fn tick(self: &Rc<Self>) -> bool {
        let Some(state) = self.state() else {
            return false;
        };
        if !self.shown() {
            return false;
        }
        if self.check() {
            self.quiet.set(0);
        }
        if completion::project_busy(&state) || self.aux_reading.get() {
            self.quiet.set(0);
            return true;
        }
        let quiet = self.quiet.get() + 1;
        self.quiet.set(quiet);
        quiet < QUIET_TICKS
    }

    /// Recomputes the outline when what it is computed from changed; true when it did.
    fn check(self: &Rc<Self>) -> bool {
        let Some(state) = self.state() else {
            return false;
        };
        if !self.shown() {
            return false;
        }
        let active = state
            .active_editor()
            .map(|e| e.document().path().to_path_buf());
        let build_root = state.build.get().and_then(|b| b.root());
        if self.aux_stale.get() {
            self.read_aux_now();
        }
        let unchanged = self.inputs.borrow().as_ref().is_some_and(|inputs| {
            inputs.same(
                &state.workspace.index.borrow(),
                active.as_deref(),
                build_root.as_deref(),
                self.aux_generation.get(),
            )
        });
        if unchanged {
            return false;
        }
        let index = state.workspace.index.borrow();
        let current = self.root.borrow().clone();
        let root = find_root(
            &index,
            active.as_deref(),
            build_root.as_deref(),
            current.as_deref(),
        );
        let inputs = Inputs {
            files: index
                .files
                .iter()
                .map(|(p, f)| (p.clone(), Arc::downgrade(f)))
                .collect(),
            active,
            build_root,
            aux_generation: self.aux_generation.get(),
        };
        drop(index);
        self.inputs.replace(Some(inputs));
        if root != *self.aux_root.borrow() {
            self.aux_root.replace(root.clone());
            self.aux.replace(Rc::new(AuxData::default()));
            self.read_aux_now();
        }
        self.root.replace(root);
        self.sync(&state);
        true
    }

    /// Reads the root's `.aux` on the jobs thread; the watch picks the result up.
    fn read_aux_now(self: &Rc<Self>) {
        let Some(state) = self.state() else {
            return;
        };
        if self.aux_reading.get() {
            self.aux_stale.set(true);
            return;
        }
        self.aux_stale.set(false);
        let Some(root) = self.aux_root.borrow().clone() else {
            return;
        };
        let hint = self
            .aux_hint
            .borrow()
            .clone()
            .filter(|h| h.file_stem() == root.file_stem());
        let candidates = aux_candidates(&root, &state.config.build.outdir, hint.as_deref());
        let fallback = root.parent().unwrap_or(Path::new("/")).to_path_buf();
        self.aux_reading.set(true);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let read = gio::spawn_blocking(move || read_aux(&candidates, &fallback)).await;
            let Some(panel) = weak.upgrade() else {
                return;
            };
            panel.aux_reading.set(false);
            if panel.aux_root.borrow().as_ref() == Some(&root) {
                let aux = read.ok().flatten().map(|(_, aux)| aux).unwrap_or_default();
                if **panel.aux.borrow() != aux || panel.aux_generation.get() == 0 {
                    panel.aux.replace(Rc::new(aux));
                    panel.aux_generation.set(panel.aux_generation.get() + 1);
                }
                panel.update_stats(|s| s.aux_reads += 1);
            }
            if panel.aux_stale.get() {
                panel.read_aux_now();
            }
            panel.poke();
        });
    }

    // ----- patching the models -----

    fn sync(self: &Rc<Self>, state: &Rc<WindowState>) {
        let started = Instant::now();
        let entries = match self.root.borrow().clone() {
            Some(root) => {
                let aux = self.aux.borrow().clone();
                let caption =
                    |path: &Path, begin: u32, end: u32| live_caption(state, path, begin, end);
                build_outline(&state.workspace.index.borrow(), &root, &aux, &caption)
            }
            None => Vec::new(),
        };
        let built = started.elapsed();
        let top = top_heading_level(&entries);
        let top_changed = top != self.top_level.replace(top);
        let mut created = Vec::new();
        let mut updated = 0u64;
        self.sync_children(
            None,
            &self.root_store,
            &self.root_nodes,
            entries,
            &mut created,
            &mut updated,
        );
        self.index_nodes();
        if top_changed {
            let mut all = Vec::new();
            for node in self.root_nodes.borrow().iter() {
                node.descendants(&mut all);
            }
            for node in all {
                node.refresh_rows();
            }
        }
        // Rows that appear collapsed as they were before.
        self.collapse_remembered(&created);
        let empty = self.root_nodes.borrow().is_empty();
        if self.entry.text().trim().is_empty() {
            self.stack
                .set_visible_child_name(if empty { "empty" } else { "tree" });
        } else {
            self.refilter();
        }
        let spent = started.elapsed();
        self.update_stats(|s| {
            s.syncs += 1;
            s.nodes_created += created.len() as u64;
            s.nodes_updated += updated;
            s.last_sync = spent;
            s.last_build = built;
            s.max_sync = s.max_sync.max(spent);
        });
        self.follow_soon();
    }

    /// Makes `nodes` (the children of `parent`, mirrored in `store`) show `new`: rows equal at
    /// both ends are kept, a middle of rows of the same shape is updated in place (edited
    /// titles), anything else in the middle is replaced.
    fn sync_children(
        &self,
        parent: Option<&Rc<Node>>,
        store: &gio::ListStore,
        nodes: &RefCell<Vec<Rc<Node>>>,
        mut new: Vec<Entry>,
        created: &mut Vec<Rc<Node>>,
        updated: &mut u64,
    ) {
        let old = nodes.borrow().clone();
        let mut prefix = 0;
        while prefix < old.len() && prefix < new.len() && old[prefix].same(&new[prefix]) {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old.len() - prefix
            && suffix < new.len() - prefix
            && old[old.len() - 1 - suffix].same(&new[new.len() - 1 - suffix])
        {
            suffix += 1;
        }
        let old_mid = prefix..old.len() - suffix;
        let new_mid = prefix..new.len() - suffix;
        let in_place = old_mid.len() == new_mid.len()
            && old[old_mid.clone()]
                .iter()
                .zip(&new[new_mid.clone()])
                .all(|(o, n)| o.same_shape(n));
        // Kept and updated rows.
        let mut kept: Vec<(Rc<Node>, Entry)> = Vec::new();
        let mut fresh: Vec<Rc<Node>> = Vec::new();
        let tail = new.split_off(new_mid.end);
        let middle = new.split_off(prefix);
        for (node, entry) in old[..prefix].iter().zip(new) {
            kept.push((node.clone(), entry));
        }
        if in_place {
            for (node, entry) in old[old_mid.clone()].iter().zip(middle) {
                kept.push((node.clone(), entry));
            }
        } else {
            let weak_parent = parent.map_or_else(Weak::new, Rc::downgrade);
            for mut entry in middle {
                fresh.push(Node::new(&mut entry, weak_parent.clone(), created));
            }
        }
        for (node, entry) in old[old.len() - suffix..].iter().zip(tail) {
            kept.push((node.clone(), entry));
        }
        if !in_place {
            let mut list = old[..prefix].to_vec();
            list.extend(fresh.iter().cloned());
            list.extend(old[old.len() - suffix..].iter().cloned());
            nodes.replace(list);
            let objects: Vec<glib::BoxedAnyObject> = fresh.iter().map(Node::object).collect();
            store.splice(
                u32::try_from(prefix).unwrap_or(u32::MAX),
                u32::try_from(old_mid.len()).unwrap_or(u32::MAX),
                &objects,
            );
        }
        for (node, mut entry) in kept {
            self.update_node(&node, &mut entry, updated);
            if let Some(store) = &node.children {
                let children = std::mem::take(&mut entry.children);
                self.sync_children(
                    Some(&node),
                    store,
                    &node.child_nodes,
                    children,
                    created,
                    updated,
                );
            }
        }
    }

    fn update_node(&self, node: &Rc<Node>, entry: &mut Entry, updated: &mut u64) {
        node.line.set(entry.line);
        node.end_line.set(entry.end_line);
        if node.data.borrow().equals(entry) {
            return;
        }
        let old_key = node.key();
        node.data.replace(NodeData::of(entry));
        *updated += 1;
        let new_key = node.key();
        if old_key != new_key {
            let mut collapsed = self.collapsed.borrow_mut();
            if collapsed.remove(&old_key) {
                collapsed.insert(new_key);
            }
        }
        node.refresh_rows();
        if self.marks.borrow().contains_key(&Rc::as_ptr(node)) {
            // The filter's highlight positions are stale.
            self.marks.borrow_mut().remove(&Rc::as_ptr(node));
        }
    }

    /// Rebuilds the per-file row lists (for following the cursor and shifting lines).
    fn index_nodes(&self) {
        let mut by_file: HashMap<PathBuf, Vec<Rc<Node>>> = HashMap::new();
        let mut files = HashMap::new();
        let mut all = Vec::new();
        for node in self.root_nodes.borrow().iter() {
            node.descendants(&mut all);
        }
        for node in all {
            if node.kind == EntryKind::File {
                files.insert(node.path.clone(), node);
            } else {
                by_file.entry(node.path.clone()).or_default().push(node);
            }
        }
        for nodes in by_file.values_mut() {
            nodes.sort_by_key(|n| n.line.get());
        }
        self.by_file.replace(by_file);
        self.file_nodes.replace(files);
    }

    // ----- expansion -----

    /// The tree row of `node`, or of its deepest ancestor that is shown (all the others above
    /// it are expanded).
    fn visible_row(&self, node: &Rc<Node>) -> Option<(gtk::TreeListRow, Rc<Node>)> {
        let mut chain = vec![node.clone()];
        while let Some(parent) = chain.last().and_then(|n| n.parent()) {
            chain.push(parent);
        }
        chain.reverse();
        let mut row: Option<gtk::TreeListRow> = None;
        let mut shown: Option<Rc<Node>> = None;
        for (depth, n) in chain.iter().enumerate() {
            let i = if depth == 0 {
                self.root_nodes
                    .borrow()
                    .iter()
                    .position(|s| Rc::ptr_eq(s, n))
            } else {
                chain[depth - 1]
                    .child_nodes
                    .borrow()
                    .iter()
                    .position(|s| Rc::ptr_eq(s, n))
            }?;
            let i = u32::try_from(i).ok()?;
            let next = match &row {
                None => self.tree_model.child_row(i),
                Some(parent) if parent.is_expanded() => parent.child_row(i),
                Some(_) => break,
            };
            let Some(next) = next else {
                break;
            };
            row = Some(next);
            shown = Some(n.clone());
        }
        Some((row?, shown?))
    }

    /// The row of `node` itself, when it is shown.
    fn row_of(&self, node: &Rc<Node>) -> Option<gtk::TreeListRow> {
        self.visible_row(node)
            .filter(|(_, shown)| Rc::ptr_eq(shown, node))
            .map(|(row, _)| row)
    }

    fn tree_node_at(&self, position: u32) -> Option<Rc<Node>> {
        let row = self.tree_model.row(position)?;
        node_of(&row.item()?)
    }

    /// `row` (showing `node`) notified a change of `expanded`: the user's arrow or keys, our
    /// own collapsing, or the row being destroyed while the model changes (its parent collapsed,
    /// it was removed). Which one is decided from an idle, once the model is consistent: a row
    /// that is still the model's row at its position changed for real.
    fn expansion_notified(self: &Rc<Self>, row: &gtk::TreeListRow, node: &Rc<Node>) {
        self.expansions
            .borrow_mut()
            .push((row.clone(), Rc::downgrade(node)));
        if self.expansion_idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(panel) = weak.upgrade() {
                panel.expansion_idle.replace(None);
                let changes = panel.expansions.take();
                for (row, node) in changes {
                    let alive = panel
                        .tree_model
                        .row(row.position())
                        .is_some_and(|r| r == row);
                    if alive && let Some(node) = node.upgrade() {
                        panel.expanded_changed(&node, row.is_expanded());
                    }
                }
            }
            glib::ControlFlow::Break
        });
        self.expansion_idle.replace(Some(id));
    }

    /// The user expanded or collapsed `node`'s row.
    fn expanded_changed(self: &Rc<Self>, node: &Rc<Node>, expanded: bool) {
        let key = node.key();
        if expanded {
            self.collapsed.borrow_mut().remove(&key);
            // Its children were made again (expanded): collapse the remembered ones.
            self.reapply.borrow_mut().push(Rc::downgrade(node));
            if self.reapply_idle.borrow().is_none() {
                let weak = Rc::downgrade(self);
                let id = glib::idle_add_local_full(glib::Priority::HIGH, move || {
                    if let Some(panel) = weak.upgrade() {
                        panel.reapply_idle.replace(None);
                        let nodes: Vec<Rc<Node>> = panel
                            .reapply
                            .take()
                            .iter()
                            .filter_map(Weak::upgrade)
                            .collect();
                        for node in nodes {
                            let mut below = Vec::new();
                            for child in node.child_nodes.borrow().iter() {
                                child.descendants(&mut below);
                            }
                            panel.collapse_remembered(&below);
                        }
                    }
                    glib::ControlFlow::Break
                });
                self.reapply_idle.replace(Some(id));
            }
        } else {
            let mut collapsed = self.collapsed.borrow_mut();
            if collapsed.len() >= MAX_COLLAPSED {
                collapsed.clear();
            }
            collapsed.insert(key);
        }
    }

    /// Collapses the rows of `nodes` (in tree order) that are remembered as collapsed.
    fn collapse_remembered(&self, nodes: &[Rc<Node>]) {
        if self.collapsed.borrow().is_empty() {
            return;
        }
        for node in nodes {
            if node.child_nodes.borrow().is_empty()
                || !self.collapsed.borrow().contains(&node.key())
            {
                continue;
            }
            if let Some(row) = self.row_of(node)
                && row.is_expanded()
            {
                row.set_expanded(false);
            }
        }
    }

    fn expand_all(&self) {
        self.collapsed.borrow_mut().clear();
        let mut position = 0;
        while let Some(row) = self.tree_model.row(position) {
            if row.is_expandable() && !row.is_expanded() {
                row.set_expanded(true);
            }
            position += 1;
        }
    }

    fn collapse_all(&self) {
        let mut all = Vec::new();
        for node in self.root_nodes.borrow().iter() {
            node.descendants(&mut all);
        }
        {
            let mut collapsed = self.collapsed.borrow_mut();
            for node in all.iter().filter(|n| !n.child_nodes.borrow().is_empty()) {
                if collapsed.len() < MAX_COLLAPSED {
                    collapsed.insert(node.key());
                }
            }
        }
        let count = self.root_nodes.borrow().len();
        for i in 0..count {
            if let Some(row) = self
                .tree_model
                .child_row(u32::try_from(i).unwrap_or(u32::MAX))
                && row.is_expanded()
            {
                row.set_expanded(false);
            }
        }
    }

    /// Left and Right on the tree: collapse (else go to the parent) and expand (else go to the
    /// first child).
    fn tree_key(&self, key: gtk::gdk::Key) -> glib::Propagation {
        let position = self.tree_selection.selected();
        let Some(row) = self.tree_model.row(position) else {
            return glib::Propagation::Proceed;
        };
        let target = match key {
            gtk::gdk::Key::Left | gtk::gdk::Key::KP_Left => {
                if row.is_expanded()
                    && row.is_expandable()
                    && row.children().is_some_and(|c| c.n_items() > 0)
                {
                    row.set_expanded(false);
                    None
                } else {
                    row.parent().map(|p| p.position())
                }
            }
            gtk::gdk::Key::Right | gtk::gdk::Key::KP_Right => {
                if !row.is_expanded() && row.is_expandable() {
                    row.set_expanded(true);
                    None
                } else {
                    row.child_row(0).map(|c| c.position())
                }
            }
            _ => return glib::Propagation::Proceed,
        };
        if let Some(target) = target {
            self.tree_view.scroll_to(
                target,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
        }
        glib::Propagation::Stop
    }

    // ----- following the cursor -----

    fn follow_soon(self: &Rc<Self>) {
        if self.follow_idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
            if let Some(panel) = weak.upgrade() {
                panel.follow_idle.replace(None);
                let started = Instant::now();
                panel.follow();
                panel.key_work(started);
            }
            glib::ControlFlow::Break
        });
        self.follow_idle.replace(Some(id));
    }

    /// Selects the row of the innermost heading, theorem or float holding the active editor's
    /// cursor (the deepest one shown), without touching the editor.
    fn follow(self: &Rc<Self>) {
        let Some(state) = self.state() else {
            return;
        };
        if !state.config.outline.follow_cursor || !self.shown() {
            return;
        }
        let Some(editor) = state.active_editor() else {
            return;
        };
        let doc = editor.document();
        let line = doc.line_index().line_of(editor.cursor_offset());
        let shown = self
            .node_at(doc.path(), line)
            .and_then(|node| self.visible_row(&node));
        let selected = self.tree_selection.selected();
        match shown {
            Some((row, _)) => {
                let position = row.position();
                if position != selected {
                    self.tree_selection.set_selected(position);
                    self.scroll_soon();
                }
            }
            None if selected != gtk::INVALID_LIST_POSITION => {
                self.tree_selection.set_selected(gtk::INVALID_LIST_POSITION);
            }
            None => {}
        }
    }

    /// Scrolls the selected row into view once the cursor rests for [`SCROLL_DELAY`]: scrolling
    /// binds a screenful of rows, which a held key or a run of jumps must not pay each time.
    fn scroll_soon(self: &Rc<Self>) {
        if let Some(old) = self.scroll_timer.take() {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(SCROLL_DELAY, move || {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            panel.scroll_timer.replace(None);
            let position = panel.tree_selection.selected();
            if position != gtk::INVALID_LIST_POSITION
                && panel.shown()
                && panel.stack.visible_child_name().as_deref() == Some("tree")
            {
                let started = Instant::now();
                panel
                    .tree_view
                    .scroll_to(position, gtk::ListScrollFlags::NONE, None);
                let spent = started.elapsed();
                panel.update_stats(|s| {
                    s.scrolls += 1;
                    s.max_scroll = s.max_scroll.max(spent);
                });
            }
        });
        self.scroll_timer.replace(Some(id));
    }

    /// The innermost row holding 0-based `line` of `path`.
    fn node_at(&self, path: &Path, line: u32) -> Option<Rc<Node>> {
        let by_file = self.by_file.borrow();
        let found = by_file.get(path).and_then(|nodes| {
            let i = nodes.partition_point(|n| n.line.get() <= line);
            i.checked_sub(1).map(|i| nodes[i].clone())
        });
        let mut node = match found {
            Some(node) => node,
            None => return self.file_nodes.borrow().get(path).cloned(),
        };
        // A theorem or float that ended before the line: the heading (or file) around it.
        while matches!(node.kind, EntryKind::Theorem | EntryKind::Float)
            && node.end_line.get().is_some_and(|end| end < line)
        {
            node = node.parent()?;
        }
        Some(node)
    }

    fn set_follow_cursor(&self, on: bool) {
        let Some(state) = self.state() else {
            return;
        };
        self.follow_action.set_state(&on.to_variant());
        let saved = state.edit_config_file(|text| {
            lemma_config::set_value(
                text,
                "outline",
                "follow_cursor",
                lemma_config::ConfigValue::Bool(on),
            )
        });
        if let Err(e) = saved {
            // No writable configuration file (LEMMA_CONFIG=/dev/null): this session only.
            tracing::info!("outline.follow_cursor not saved: {e}");
            let mut config = (*state.config.get()).clone();
            config.outline.follow_cursor = on;
            state.set_config(Rc::new(config));
        }
    }

    // ----- navigation -----

    /// Goes to `node`'s line (what a click or Enter on its row does).
    fn activate_node(self: &Rc<Self>, node: &Rc<Node>) {
        let Some(state) = self.state() else {
            return;
        };
        let target = GotoTarget::Line {
            path: node.path.clone(),
            line: node.line.get(),
            find: node.data.borrow().find.clone(),
        };
        glib::spawn_future_local(async move {
            match goto::navigate(&state, target).await {
                Ok(()) => state.sidebar.dismiss_overlay(),
                Err(e) => state.toast(&e),
            }
        });
    }

    fn describe_location(&self, node: &Node) -> String {
        let root_dir = self
            .root
            .borrow()
            .as_ref()
            .and_then(|r| r.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let file = node
            .path
            .strip_prefix(&root_dir)
            .unwrap_or(&node.path)
            .display()
            .to_string();
        match node.kind {
            EntryKind::File if !node.data.borrow().indexed => format!("{file} (not read yet)"),
            EntryKind::File => file,
            _ => format!("{file}, line {}", node.line.get() + 1),
        }
    }

    // ----- the filter -----

    fn refilter(self: &Rc<Self>) {
        let query = self.entry.text().trim().to_owned();
        self.marks.borrow_mut().clear();
        if query.is_empty() {
            self.results_store.remove_all();
            let empty = self.root_nodes.borrow().is_empty();
            self.stack
                .set_visible_child_name(if empty { "empty" } else { "tree" });
            return;
        }
        let mut nodes = Vec::new();
        for node in self.root_nodes.borrow().iter() {
            node.descendants(&mut nodes);
        }
        let haystacks: Vec<String> = nodes
            .iter()
            .map(|n| {
                let number = n.number_text();
                let title = n.title_text();
                if number.is_empty() {
                    title
                } else {
                    format!("{number} {title}")
                }
            })
            .collect();
        let config = frizbee::Config {
            max_typos: Some(0),
            ..frizbee::Config::default()
        };
        let mut matcher = frizbee::Matcher::new(&query, &config);
        let matches = matcher.match_list_indices(&haystacks);
        let ascii = query.is_ascii();
        let style = adw::StyleManager::default();
        let rgba = style.accent_color().to_standalone_rgba(style.is_dark());
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.match_color.replace(format!(
            "#{:02x}{:02x}{:02x}",
            byte(rgba.red()),
            byte(rgba.green()),
            byte(rgba.blue())
        ));
        let mut objects = Vec::new();
        {
            let mut marks = self.marks.borrow_mut();
            for m in matches.into_iter().take(MAX_RESULTS) {
                let Some(node) = nodes.get(m.index as usize) else {
                    continue;
                };
                let hay = &haystacks[m.index as usize];
                let positions: Vec<usize> = if ascii {
                    m.indices.iter().map(|&i| i as usize).collect()
                } else {
                    let chars: Vec<usize> = hay.char_indices().map(|(b, _)| b).collect();
                    m.indices
                        .iter()
                        .filter_map(|&i| chars.get(i as usize).copied())
                        .collect()
                };
                marks.insert(Rc::as_ptr(node), positions);
                objects.push(Node::object(node));
            }
        }
        let count = self.results_store.n_items();
        self.results_store.splice(0, count, &objects);
        if !objects.is_empty() {
            self.results_selection.set_selected(0);
            self.results_view
                .scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }
        self.stack.set_visible_child_name(if objects.is_empty() {
            "none"
        } else {
            "results"
        });
    }

    fn move_result(&self, step: i64) {
        let count = i64::from(self.results_store.n_items());
        if count == 0 {
            return;
        }
        let current = i64::from(self.results_selection.selected());
        let next = if current >= count {
            0
        } else {
            (current + step).clamp(0, count - 1)
        };
        let next = u32::try_from(next).unwrap_or(0);
        self.results_selection.set_selected(next);
        self.results_view
            .scroll_to(next, gtk::ListScrollFlags::NONE, None);
    }
}

/// The factory of the tree's (`results` false) or the filter's rows.
fn row_factory(panel: Weak<Panel>, results: bool) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = RowWidgets::new(panel.clone(), results);
        item.set_child(Some(&row.expander));
        let r = row.clone();
        item.connect_item_notify(move |item| r.bind(item.item()));
        row.bind(item.item());
    });
    factory.connect_teardown(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(None::<&gtk::Widget>);
        }
    });
    factory
}

/// The highest (smallest) heading level in `entries`, 2 (section) when there is none.
fn top_heading_level(entries: &[Entry]) -> u8 {
    fn walk(entries: &[Entry], top: &mut u8) {
        for e in entries {
            if e.kind == EntryKind::Heading {
                *top = (*top).min(e.level);
            }
            walk(&e.children, top);
        }
    }
    let mut top = u8::MAX;
    walk(entries, &mut top);
    if top == u8::MAX { 2 } else { top }
}

/// The caption of the float on lines `begin..=end` of `path`, read from its open document.
fn live_caption(state: &WindowState, path: &Path, begin: u32, end: u32) -> Option<String> {
    let doc = state.workspace.get(path).filter(|d| d.is_open())?;
    let mut text = String::new();
    for line in begin..=end {
        text.push_str(&doc.line_text(line)?);
        text.push('\n');
    }
    caption_in(&text)
}

/// The root of the active document's project: the build's root when its project holds the
/// document, else the outermost indexed root that does, else the LaTeX document itself (a
/// chapter whose root is not known yet, a document of its own). For another kind of file (a
/// `.bib`, a `.sty`) or no tab: the build's root, else the root shown so far (`current`).
fn find_root(
    index: &ProjectIndex,
    active: Option<&Path>,
    build_root: Option<&Path>,
    current: Option<&Path>,
) -> Option<PathBuf> {
    let roots: Vec<(&PathBuf, Vec<PathBuf>)> = index
        .files
        .iter()
        .filter(|(_, f)| f.has_documentclass && f.has_begin_document)
        .map(|(p, _)| (p, index.project_files(p)))
        .collect();
    if let Some(active) = active {
        let holding: Vec<&(&PathBuf, Vec<PathBuf>)> = roots
            .iter()
            .filter(|(_, files)| files.iter().any(|f| f == active))
            .collect();
        if let Some(build) = build_root
            && holding.iter().any(|(r, _)| r.as_path() == build)
        {
            return Some(build.to_path_buf());
        }
        let outermost = holding.iter().find(|(r, _)| {
            !holding
                .iter()
                .any(|(o, files)| o != r && files.iter().any(|f| f == *r))
        });
        if let Some((root, _)) = outermost.or(holding.first()) {
            return Some((*root).clone());
        }
        if index.files.contains_key(active) || completion::is_latex(active) {
            return Some(active.to_path_buf());
        }
    }
    [build_root, current]
        .into_iter()
        .flatten()
        .find(|root| index.files.contains_key(*root))
        .map(Path::to_path_buf)
}

// ───────────────────────── Hooks and the public API ─────────────────────────

/// Called once per window after the M2 install hooks; fills `state.sidebar.outline`.
pub fn install(state: &Rc<WindowState>) {
    crate::build_ui::install_css_once("outline", CSS);
    let panel = Panel::new(state);
    state.sidebar.outline.set_child(Some(&panel.page));
    PANELS.with_borrow_mut(|panels| {
        panels.retain(|p| p.state.strong_count() > 0);
        panels.push(panel.clone());
    });
    panel.connect(state);
}

/// `--demo outline` (run with FILE = a copy of `tests/fixtures/proj/main.tex`): shows the
/// Outline page with the whole project and its `.aux` numbers, puts the cursor into the first
/// numbered theorem (so following the cursor selects its row), and resolves when the page is
/// settled, for `i-outline.png`. With `LEMMA_DEMO_FILTER=text` the filter shows that text's
/// matches.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    use crate::testmode::wait_until;
    state.sidebar.show_page(PAGE_OUTLINE);
    completion::refresh_project(&state);
    let timeout = Duration::from_secs(15);
    wait_until(timeout, "the outline", || {
        is_settled(&state) && !rows(&state).is_empty()
    })
    .await?;
    let panel = panel_of(&state).ok_or("the outline page is not installed")?;
    let mut all = Vec::new();
    for node in panel.root_nodes.borrow().iter() {
        node.descendants(&mut all);
    }
    let root = panel.root.borrow().clone();
    let theorem = all
        .iter()
        .filter(|n| n.kind == EntryKind::Theorem && Some(&n.path) == root.as_ref())
        .find(|n| {
            let data = n.data.borrow();
            data.number.is_some() && !data.title.is_empty()
        })
        .or_else(|| all.iter().find(|n| n.kind == EntryKind::Theorem))
        .cloned();
    if let Some(theorem) = theorem {
        let target = GotoTarget::Line {
            path: theorem.path.clone(),
            line: theorem.line.get() + 1,
            find: None,
        };
        goto::navigate(&state, target).await?;
    }
    // Navigating dismisses the overlay of a narrow window: show the page again.
    state.sidebar.show_page(PAGE_OUTLINE);
    // `LEMMA_DEMO_FILTER=text` shows the filter's results instead of the tree.
    if let Ok(filter) = std::env::var("LEMMA_DEMO_FILTER") {
        set_filter(&state, &filter);
    }
    wait_until(timeout, "the outline to follow the cursor", || {
        is_settled(&state) && selected_row(&state).is_some()
    })
    .await?;
    Ok(())
}

/// The rows shown in the tree (collapsed rows' children are not), indented by two spaces per
/// level, in the golden format of [`Entry::text`].
pub fn rows(state: &WindowState) -> Vec<String> {
    let Some(panel) = panel_of(state) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut position = 0;
    while let Some(row) = panel.tree_model.row(position) {
        if let Some(node) = row.item().as_ref().and_then(node_of) {
            out.push(format!(
                "{}{}",
                "  ".repeat(row.depth() as usize),
                node.text()
            ));
        }
        position += 1;
    }
    out
}

/// The whole outline (collapsed or not) in the golden format ([`tree_text_of`]).
pub fn tree_text(state: &WindowState) -> String {
    fn walk(nodes: &[Rc<Node>], depth: usize, out: &mut String) {
        for node in nodes {
            out.push_str(&"  ".repeat(depth));
            out.push_str(&node.text());
            out.push('\n');
            walk(&node.child_nodes.borrow(), depth + 1, out);
        }
    }
    let mut out = String::new();
    if let Some(panel) = panel_of(state) {
        walk(&panel.root_nodes.borrow(), 0, &mut out);
    }
    out
}

/// The text of the selected tree row (what following the cursor selected), without indent.
pub fn selected_row(state: &WindowState) -> Option<String> {
    let panel = panel_of(state)?;
    let position = panel.tree_selection.selected();
    panel.tree_node_at(position).map(|n| n.text())
}

fn find_node(state: &WindowState, text: &str) -> Option<(Rc<Panel>, Rc<Node>)> {
    let panel = panel_of(state)?;
    let mut all = Vec::new();
    for node in panel.root_nodes.borrow().iter() {
        node.descendants(&mut all);
    }
    let node = all.into_iter().find(|n| n.text() == text)?;
    Some((panel, node))
}

/// What a click on the row whose text ([`Entry::text`]) is `text` does: goes to its line
/// (asynchronously). False when there is no such row.
pub fn activate_row(state: &WindowState, text: &str) -> bool {
    match find_node(state, text) {
        Some((panel, node)) => {
            panel.activate_node(&node);
            true
        }
        None => false,
    }
}

/// What pressing Enter on tree row `position` does (the list view's `activate`).
pub fn activate_position(state: &WindowState, position: u32) -> bool {
    let Some(panel) = panel_of(state) else {
        return false;
    };
    panel.tree_view.emit_by_name::<()>("activate", &[&position]);
    true
}

/// Selects and focuses the tree row whose text is `text` (as clicking it would, without going
/// to its line). False when it is not shown.
pub fn focus_row(state: &WindowState, text: &str) -> bool {
    let Some((panel, node)) = find_node(state, text) else {
        return false;
    };
    let Some(row) = panel.row_of(&node) else {
        return false;
    };
    panel.tree_view.scroll_to(
        row.position(),
        gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
        None,
    );
    true
}

/// Expands or collapses the row whose text is `text` (as its arrow does). False when it is not
/// shown or cannot be expanded.
pub fn set_expanded(state: &WindowState, text: &str, expanded: bool) -> bool {
    let Some((panel, node)) = find_node(state, text) else {
        return false;
    };
    let Some(row) = panel.row_of(&node) else {
        return false;
    };
    if !row.is_expandable() {
        return false;
    }
    row.set_expanded(expanded);
    // Rows that are not on screen have no widget to report the change.
    panel.expanded_changed(&node, expanded);
    true
}

/// Whether the row whose text is `text` is shown expanded (`None`: not shown).
pub fn is_expanded(state: &WindowState, text: &str) -> Option<bool> {
    let (panel, node) = find_node(state, text)?;
    panel.row_of(&node).map(|row| row.is_expanded())
}

/// Sets the filter text (as typing in the entry does; the filter runs at once).
pub fn set_filter(state: &WindowState, text: &str) {
    if let Some(panel) = panel_of(state) {
        panel.entry.set_text(text);
        panel.refilter();
    }
}

/// The rows the filter shows, best first, and which of them is selected.
pub fn filter_results(state: &WindowState) -> (Vec<String>, Option<usize>) {
    let Some(panel) = panel_of(state) else {
        return (Vec::new(), None);
    };
    let rows = (0..panel.results_store.n_items())
        .filter_map(|i| panel.results_store.item(i).as_ref().and_then(node_of))
        .map(|n| n.text())
        .collect();
    let selected = panel.results_selection.selected();
    let selected = (selected != gtk::INVALID_LIST_POSITION).then_some(selected as usize);
    (rows, selected)
}

/// What Enter in the filter entry does: goes to the selected result.
pub fn activate_filter(state: &WindowState) {
    if let Some(panel) = panel_of(state) {
        panel.entry.emit_by_name::<()>("activate", &[]);
    }
}

/// The name of the page's visible part: `empty`, `tree`, `results` or `none` (no matches).
pub fn page_state(state: &WindowState) -> Option<String> {
    let panel = panel_of(state)?;
    panel.stack.visible_child_name().map(|n| n.to_string())
}

/// Looks at the project index now (the watch does it every 100 ms while things change).
pub fn refresh(state: &WindowState) {
    if let Some(panel) = panel_of(state) {
        panel.poke();
        panel.check();
    }
}

/// True when the page shows what the project index and the `.aux` say: nothing is queued in the
/// project feed, the `.aux` is read, the outline is current and the cursor followed.
pub fn is_settled(state: &WindowState) -> bool {
    let Some(panel) = panel_of(state) else {
        return false;
    };
    if completion::project_busy(state) || panel.aux_reading.get() || panel.aux_stale.get() {
        return false;
    }
    let active = state
        .active_editor()
        .map(|e| e.document().path().to_path_buf());
    let build_root = state.build.get().and_then(|b| b.root());
    let current = panel.inputs.borrow().as_ref().is_some_and(|inputs| {
        inputs.same(
            &state.workspace.index.borrow(),
            active.as_deref(),
            build_root.as_deref(),
            panel.aux_generation.get(),
        )
    });
    if !current {
        // Something changed the index without an event the page watches (a test's
        // `completion::refresh_project`): look again.
        panel.poke();
        return false;
    }
    panel.follow_idle.borrow().is_none()
        && panel.scroll_timer.borrow().is_none()
        && panel.reapply_idle.borrow().is_none()
        && panel.expansion_idle.borrow().is_none()
}

/// The page's counters (see [`OutlineStats`]).
pub fn stats(state: &WindowState) -> OutlineStats {
    panel_of(state).map(|p| p.stats.get()).unwrap_or_default()
}

/// The project root whose outline is shown.
pub fn root(state: &WindowState) -> Option<PathBuf> {
    panel_of(state).and_then(|p| p.root.borrow().clone())
}

/// The page widget (tests look inside it).
pub fn page(state: &WindowState) -> Option<gtk::Widget> {
    panel_of(state).map(|p| p.page.clone().upcast())
}

/// The line the row whose text is `text` goes to now (0-based; it follows edits before the
/// project index catches up).
pub fn row_line(state: &WindowState, text: &str) -> Option<u32> {
    find_node(state, text).map(|(_, node)| node.line.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_syntax::index_file;

    fn aux_of(text: &str) -> AuxData {
        let mut aux = AuxData::default();
        parse_aux_text(text, &mut aux, &mut |_| None);
        aux
    }

    fn line(kind: &str, number: Option<&str>, title: &str) -> ContentsLine {
        ContentsLine {
            kind: kind.to_owned(),
            number: number.map(str::to_owned),
            title: title.to_owned(),
        }
    }

    #[test]
    fn contents_lines_of_every_class() {
        // From real pdflatex runs (article, book, scrartcl, amsart, amsbook, memoir).
        let aux = aux_of(
            r"\@writefile{toc}{\contentsline {part}{I\hspace  {1em}Foo}{1}{part.1}\protected@file@percent }
\@writefile{toc}{\contentsline {chapter}{\numberline {1}Chap \emph  {one}}{3}{chapter.1}\protected@file@percent }
\@writefile{lof}{\addvspace {10\p@ }}
\@writefile{toc}{\contentsline {section}{\numberline {1.1}Intro $x$}{3}{section.1.1}\protected@file@percent }
\newlabel{sec:i}{{1.1}{3}{Intro $x$}{section.1.1}{}}
\@writefile{toc}{\contentsline {section}{\nonumberline Added}{1}{section*.1}\protected@file@percent }
\@writefile{toc}{\contentsline {section}{\tocsection {}{1}{Intro $x$}}{1}{section.1}\protected@file@percent }
\@writefile{toc}{\contentsline {section}{\tocsection {Appendix}{A}{App}}{1}{appendix.A}\protected@file@percent }
\@writefile{toc}{\contentsline {section}{\tocsection {}{}{Starred}}{1}{section*.1}\protected@file@percent }
\@writefile{toc}{\contentsline {part}{Part\ 1. \enspace \noindent Foo}{1}{part.1}\protected@file@percent }
\@writefile{toc}{\contentsline {chapter}{\tocchapter {Chapter}{1}{Chap \emph  {one}}}{3}{chapter.1}\protected@file@percent }
\@writefile{toc}{\contentsline {part}{\partnumberline {I}Foo}{1}{part.1}\protected@file@percent }
\@writefile{toc}{\contentsline {chapter}{\chapternumberline {1}Chap \emph  {one}}{3}{chapter.1}\protected@file@percent }
\@writefile{toc}{\contentsline {subsection}{Sub}{3}{section*.2}\protected@file@percent }
\@writefile{toc}{\def \@chapapp {\appendixname }}
\@writefile{lof}{\contentsline {figure}{\numberline {1.1}{\ignorespaces Short}}{3}{figure.1.1}\protected@file@percent }
\@writefile{lot}{\contentsline {table}{\numberline {1}{\ignorespaces A table}}{1}{table.1}\protected@file@percent }
",
        );
        assert_eq!(
            aux.toc,
            vec![
                line("part", Some("I"), "Foo"),
                line("chapter", Some("1"), r"Chap \emph {one}"),
                line("section", Some("1.1"), "Intro $x$"),
                line("section", None, "Added"),
                line("section", Some("1"), "Intro $x$"),
                line("section", Some("A"), "App"),
                line("section", None, "Starred"),
                line("part", Some("1"), "Foo"),
                line("chapter", Some("1"), r"Chap \emph {one}"),
                line("part", Some("I"), "Foo"),
                line("chapter", Some("1"), r"Chap \emph {one}"),
                line("subsection", None, "Sub"),
            ]
        );
        assert_eq!(aux.lof, vec![line("figure", Some("1.1"), "Short")]);
        assert_eq!(aux.lot, vec![line("table", Some("1"), "A table")]);
        assert_eq!(aux.labels["sec:i"].0, "1.1");
    }

    #[test]
    fn aux_inputs_are_read_in_place() {
        let mut aux = AuxData::default();
        let mut read = |name: &str| {
            (name == "chapters/a.aux").then(|| {
                r"\@writefile{toc}{\contentsline {section}{\numberline {2}B}{1}{}}
\newlabel{lem}{{2.1}{1}}"
                    .to_owned()
            })
        };
        parse_aux_text(
            r"\@writefile{toc}{\contentsline {section}{\numberline {1}A}{1}{}}
\@input{chapters/a.aux}
\@input{missing.aux}
\@writefile{toc}{\contentsline {section}{\numberline {3}C}{1}{}}",
            &mut aux,
            &mut read,
        );
        let titles: Vec<&str> = aux.toc.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["A", "B", "C"]);
        assert_eq!(aux.labels["lem"].0, "2.1");
    }

    #[test]
    fn titles_are_made_readable() {
        assert_eq!(
            pretty_title(r"Poincar\'e duality"),
            "Poincare\u{301} duality"
        );
        assert_eq!(pretty_title(r"The \emph{Yoneda} lemma"), "The Yoneda lemma");
        assert_eq!(
            pretty_title(r"Riemann--Roch for $\mathcal{O}_X$-modules"),
            r"Riemann–Roch for $\mathcal{O}_X$-modules"
        );
        assert_eq!(
            pretty_title(r"\texorpdfstring{$\pi_1$}{pi1} and ``cats''~\&~dogs"),
            "pi1 and “cats” & dogs"
        );
        assert_eq!(
            pretty_title(r#"G\"odel and Erd\H{o}s"#),
            "Go\u{308}del and Erdo\u{30b}s"
        );
        assert_eq!(pretty_title(r"\textbf{Bold}\,text \cF"), r"Bold text \cF");
        assert_eq!(pretty_title(r"$\{x\}$ sets"), r"$\{x\}$ sets");
    }

    #[test]
    fn title_words_ignore_markup() {
        assert_eq!(norm_words(r"The $\cF$ \emph{functor}"), "the functor");
        assert_eq!(
            norm_words(r"The $\mathcal {F}$ \emph  {functor}"),
            "the functor"
        );
        assert_eq!(norm_words("Filler subsection 1"), "filler subsection 1");
    }

    #[test]
    fn captions_from_source() {
        let text = "\\begin{figure}[ht]\n\\centering % \\caption{no}\n\
                    \\begin{subfigure}{.4\\textwidth}\\caption{sub}\\end{subfigure}\n\
                    \\caption[Short form]{Long \\emph{form}\n with a break}\\label{fig:a}\n\\end{figure}";
        assert_eq!(caption_in(text).as_deref(), Some("Short form"));
        let text = "\\begin{table}\n\\caption{A \\textbf{bold}\n table\\label{t}}\n\\end{table}";
        assert_eq!(caption_in(text).as_deref(), Some("A bold table"));
        assert_eq!(caption_in("\\begin{figure}\n\\end{figure}"), None);
    }

    fn key(level: u8, words: &str, unnumbered: bool) -> AlignKey {
        AlignKey {
            level,
            words: words.to_owned(),
            unnumbered,
        }
    }

    #[test]
    fn alignment_survives_edits_since_the_build() {
        let lines = [
            key(2, "a", false),
            key(2, "b", false),
            key(2, "references", true),
            key(2, "c", false),
        ];
        // Unchanged.
        let rows = [key(2, "a", false), key(2, "b", false), key(2, "c", false)];
        assert_eq!(align(&rows, &lines), [Some(0), Some(1), Some(3)]);
        // A new section before B: no number for it, B and C keep theirs.
        let rows = [
            key(2, "a", false),
            key(2, "new", false),
            key(2, "b", false),
            key(2, "c", false),
        ];
        assert_eq!(align(&rows, &lines), [Some(0), None, Some(1), Some(3)]);
        // B retitled: keeps its number.
        let rows = [key(2, "a", false), key(2, "bee", false), key(2, "c", false)];
        assert_eq!(align(&rows, &lines), [Some(0), Some(1), Some(3)]);
        // B deleted.
        let rows = [key(2, "a", false), key(2, "c", false)];
        assert_eq!(align(&rows, &lines), [Some(0), Some(3)]);
        // A starred section takes no number from a numbered line.
        let rows = [key(2, "x", true), key(2, "a", false)];
        assert_eq!(align(&rows, &lines), [None, Some(0)]);
    }

    fn project(files: &[(&str, &str)]) -> (ProjectIndex, PathBuf) {
        let mut index = ProjectIndex::default();
        for (path, text) in files {
            index.update_file(PathBuf::from(path), Arc::new(index_file(text)));
        }
        (index, PathBuf::from(files[0].0))
    }

    #[test]
    fn files_nest_under_the_heading_above_their_headings() {
        let (index, root) = project(&[
            (
                "/p/main.tex",
                "\\documentclass{book}\n\\input{preamble}\n\\begin{document}\n\
                 \\chapter{One}\n\\section{Intro}\n\\input{sec}\n\\include{ch2}\n\
                 \\input{empty}\n\\begin{figure}\\input{plot}\\caption{P}\\end{figure}\n\
                 \\end{document}\n",
            ),
            ("/p/preamble.tex", "\\newtheorem{lem}{Lemma}\n"),
            ("/p/sec.tex", "\\section{Second}\n\\begin{lem}x\\end{lem}\n"),
            ("/p/ch2.tex", "\\chapter{Two}\n\\section*{Notes}\n"),
            ("/p/empty.tex", "Just text.\n"),
            ("/p/plot.tex", "\\draw (0,0);\n"),
        ]);
        let tree = build_outline(&index, &root, &AuxData::default(), &|_, _, _| None);
        assert_eq!(
            tree_text_of(&tree),
            "One\n  Intro\n  [sec.tex]\n    Second\n      Lemma\n[ch2.tex]\n  Two\n    ∗ Notes\n\
             [empty.tex]\nFigure\n"
        );
    }

    #[test]
    fn numbers_from_labels_and_contents_lines() {
        let (index, root) = project(&[(
            "/p/main.tex",
            "\\documentclass{amsart}\n\\newtheorem{thm}{Theorem}[section]\n\
             \\newtheorem*{rem}{Remark}\n\\begin{document}\n\
             \\section{First}\\label{s1}\n\\begin{thm}[Big]\\label{t}\\end{thm}\n\
             \\section{Second}\n\\subsection{Deep}\n\\begin{rem}\\end{rem}\n\
             \\begin{figure}\\caption{Cap}\\end{figure}\n\\section{Added later}\n\
             \\end{document}\n",
        )]);
        let mut aux = AuxData::default();
        aux.labels.insert("s1".into(), ("1".into(), "1".into()));
        aux.labels.insert("t".into(), ("1.1".into(), "1".into()));
        aux.toc = vec![
            line("section", Some("1"), "First"),
            line("section", Some("2"), "Second"),
            line("subsection", Some("2.1"), "Deep"),
        ];
        aux.lof = vec![line("figure", Some("1"), r"{\ignorespaces Cap}")];
        let tree = build_outline(&index, &root, &aux, &|_, _, _| None);
        assert_eq!(
            tree_text_of(&tree),
            "1 First\n  Theorem 1.1 (Big)\n2 Second\n  2.1 Deep\n    Remark\n    Figure 1 Cap\n\
             Added later\n"
        );
    }

    #[test]
    fn koma_and_parts() {
        let (index, root) = project(&[(
            "/p/main.tex",
            "\\documentclass{scrbook}\n\\begin{document}\n\\part{Base}\n\\addchap{Preface}\n\
             \\chapter{Start}\n\\addsec{Unnumbered}\n\\minisec{Mini}\n\\end{document}\n",
        )]);
        let aux = AuxData {
            toc: vec![
                line("part", Some("I"), "Base"),
                line("chapter", None, "Preface"),
                line("chapter", Some("1"), "Start"),
                line("section", None, "Unnumbered"),
            ],
            ..AuxData::default()
        };
        let tree = build_outline(&index, &root, &aux, &|_, _, _| None);
        assert_eq!(
            tree_text_of(&tree),
            "Part I Base\n  ∗ Preface\n  1 Start\n    ∗ Unnumbered\n      ∗ Mini\n"
        );
    }

    #[test]
    fn aux_candidates_order() {
        let c = aux_candidates(Path::new("/p/main.tex"), ".build", None);
        assert_eq!(
            c,
            [
                PathBuf::from("/p/.build/main.aux"),
                PathBuf::from("/p/main.aux")
            ]
        );
        let c = aux_candidates(Path::new("/p/main.tex"), "", Some(Path::new("/o/main.aux")));
        assert_eq!(
            c,
            [PathBuf::from("/o/main.aux"), PathBuf::from("/p/main.aux")]
        );
    }

    #[test]
    fn highlight_marks_bytes() {
        assert_eq!(highlight("Lemma 2.1", 0, &[0, 1], ""), "<b>Le</b>mma 2.1");
        assert_eq!(highlight("(a<b)", 10, &[12], ""), "(a<b>&lt;</b>b)");
        assert_eq!(
            highlight("ab", 0, &[1], "#3584e4"),
            "a<span weight=\"bold\" foreground=\"#3584e4\">b</span>"
        );
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use lemma_syntax::index_file;

    #[test]
    fn proj_theorems_know_their_end() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proj");
        let dir = lexical_normalize(&dir);
        let mut index = ProjectIndex::default();
        for file in ["main.tex", "chapters/yoneda.tex"] {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            index.update_file(dir.join(file), Arc::new(index_file(&text)));
        }
        let tree = build_outline(
            &index,
            &dir.join("main.tex"),
            &AuxData::default(),
            &|_, _, _| None,
        );
        fn find<'a>(entries: &'a [Entry], title: &str) -> Option<&'a Entry> {
            entries.iter().find_map(|e| {
                (e.title == title)
                    .then_some(e)
                    .or_else(|| find(&e.children, title))
            })
        }
        let thm = find(&tree, "fpqc descent").expect("the theorem");
        assert_eq!((thm.line, thm.end_line), (59, Some(66)));
    }
}
