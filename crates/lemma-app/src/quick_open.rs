//! Quick open.
//!
//! `win.quick-open` (Ctrl+P) opens the picker dialog of the command palette (`palette.rs`)
//! over the project's files ([`candidates`]), in this order:
//!
//! 1. the open tabs, in tab order, the active one last (switching to it is pointless);
//! 2. the files of the project index (`workspace.index`): each root document followed by its
//!    `\input`s and `\include`s in document order, the other indexed files, the
//!    bibliographies;
//! 3. the other source files under the project root (`workspace::is_source_file`: `.tex`,
//!    `.bib`, `.sty`, `.cls`, `.md`, …, and `latexmkrc`), from a walk of at most
//!    [`MAX_WALK_FILES`] files and [`MAX_WALK_DIRS`] directories on the jobs thread
//!    (`workspace::walk_project`: hidden directories such as `.git` and `.build`, symbolic
//!    links and the build output directory are skipped). The list grows when the walk is done;
//!    the selected row stays selected.
//!
//! A query is matched fuzzily (frizbee, as in the palette) against the path relative to the
//! root and, with a bonus, against the file name alone, so `yon` finds `chapters/yoneda.tex`
//! before a directory that happens to contain the letters ([`rank_files`]). Open tabs and index
//! files get a small bonus, so among equal matches they come first; then shorter paths, then
//! the list order. At most [`MAX_ROWS`] rows are shown. Rows show the file name (matched
//! characters in bold), its directory, and "Open" or "Unsaved" for open tabs. Choosing a row
//! opens the file (`state.open_file`), or selects its tab. Opening quick open again while it
//! is open selects its query (no new walk).
//!
//! # Go to Line
//!
//! `win.goto-line` (Ctrl+G, [`show_goto_line`]) opens quick open with `:` typed. A query that
//! ends in a line number goes to that line ([`parse_query`]; numbers are 1-based, as the status
//! bar counts lines and columns):
//!
//! - `:12` goes to line 12 of the active document, `:12:5` to its column 5: one row, "Go to
//!   line 12", with the document's number of lines; a line past the end is the last one
//!   (`goto::last_line`). `:` alone, or `:` and something that is no line number, shows a row
//!   asking for one.
//! - `main:12` and `chapters/a.tex:12:5` (a trailing `:` and a leading `./` are allowed, as
//!   TeX writes `./main.tex:12:`) list the files that match the text before the number, as that
//!   query would (a path that names one of them exactly, absolute or relative to the root, comes
//!   first), each going to line 12 of its file ("Line 12" at the end of the row).
//!
//! Choosing such a row goes there through `goto::navigate`, so Alt+Left comes back.
//!
//! Hooks: [`show`] and [`show_goto_line`] are the actions. Tests use [`candidates`],
//! [`walk_files`], [`rank_files`], [`parse_query`], [`line_key`] and the picker functions of
//! `palette.rs` ([`crate::palette::visible`], …).

use crate::document::DocRef;
use crate::editor::goto::{self, GotoTarget};
use crate::palette::{Picker, PickerRow, PickerSpec, Ranked, char_positions, matcher};
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// The walk stops after this many files…
pub const MAX_WALK_FILES: usize = 20_000;
/// … or this many directories.
pub const MAX_WALK_DIRS: usize = 2_000;
/// At most this many rows are shown.
pub const MAX_ROWS: usize = 200;
/// A match of the file name alone scores this much more than a match of the whole path.
const NAME_BONUS: i32 = 6;
/// Bonuses of open tabs and of the project index's files.
const TAB_BONUS: i32 = 8;
const INDEX_BONUS: i32 = 4;

/// Where a file in the list comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// An open tab.
    Tab,
    /// The project index (the root and what it includes, bibliographies).
    Index,
    /// The walk of the project directory.
    Walk,
}

/// One file quick open can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCandidate {
    /// Absolute, normalized.
    pub path: PathBuf,
    /// What is shown and matched: the path relative to the root, `~/…` or absolute outside it.
    pub display: String,
    pub source: Source,
    /// An open tab with unsaved changes.
    pub dirty: bool,
}

/// `path` as shown: relative to `root` when inside it, else with the home directory as `~`.
fn display_path(path: &Path, root: &Path, home: Option<&Path>) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().into_owned();
    }
    if let Some(home) = home
        && let Ok(rel) = path.strip_prefix(home)
    {
        return format!("~/{}", rel.to_string_lossy());
    }
    path.to_string_lossy().into_owned()
}

/// The user's home directory (for `~/…` paths).
fn home_dir(state: &WindowState) -> Option<PathBuf> {
    state
        .paths
        .as_ref()
        .map(|p| p.home.clone())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

/// True for the files the walk lists: project sources and `latexmkrc`.
fn listed(path: &Path) -> bool {
    crate::workspace::is_source_file(path) || path.file_name().is_some_and(|n| n == "latexmkrc")
}

/// The open tabs and the files of the project index, in quick open's order (see the module
/// docs); [`walk_files`] adds the rest.
pub fn candidates(state: &WindowState) -> Vec<FileCandidate> {
    let root = state.workspace.root().path().to_path_buf();
    let home = home_dir(state);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let active = state
        .active_editor()
        .map(|e| e.document().path().to_path_buf());
    let mut tabs: Vec<(PathBuf, bool)> = Vec::new();
    for i in 0..state.tab_view.n_pages() {
        let page = state.tab_view.nth_page(i);
        if let Some(editor) = state
            .editors()
            .into_iter()
            .find(|e| e.widget().upcast_ref::<gtk::Widget>() == &page.child())
        {
            let doc = editor.document();
            tabs.push((doc.path().to_path_buf(), doc.is_dirty()));
        }
    }
    // The active tab last.
    tabs.sort_by_key(|(path, _)| Some(path) == active.as_ref());
    for (path, dirty) in tabs {
        if seen.insert(path.clone()) {
            out.push(FileCandidate {
                display: display_path(&path, &root, home.as_deref()),
                path,
                source: Source::Tab,
                dirty,
            });
        }
    }
    let indexed = index_order(&state.workspace.index.borrow());
    for path in indexed {
        if seen.insert(path.clone()) {
            out.push(FileCandidate {
                display: display_path(&path, &root, home.as_deref()),
                path,
                source: Source::Index,
                dirty: false,
            });
        }
    }
    out
}

/// The files of the project index in project order: each root document (a file with
/// `\documentclass`) followed by what it includes, in document order; then the other indexed
/// files by depth and path; then the bibliographies.
fn index_order(index: &lemma_syntax::ProjectIndex) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |path: &Path, out: &mut Vec<PathBuf>| {
        let path = crate::workspace::normalize(path);
        if seen.insert(path.clone()) {
            out.push(path);
        }
    };
    for (root, file) in &index.files {
        if file.has_documentclass {
            for path in index.project_files(root) {
                // Included files not indexed yet may not exist.
                if index.files.contains_key(&path) || path.is_file() {
                    push(&path, &mut out);
                }
            }
        }
    }
    let mut rest: Vec<&PathBuf> = index.files.keys().collect();
    rest.sort_by(|a, b| {
        let depth = |p: &Path| p.components().count();
        depth(a).cmp(&depth(b)).then_with(|| a.cmp(b))
    });
    for path in rest {
        push(path, &mut out);
    }
    for path in index.bibs.keys() {
        push(path, &mut out);
    }
    out
}

/// The project's other source files (see the module docs). **Blocking**: run it on the jobs
/// thread.
pub fn walk_files(root: &Path, skip: &[PathBuf]) -> Vec<PathBuf> {
    crate::workspace::walk_project(root, skip, MAX_WALK_FILES, MAX_WALK_DIRS)
        .files
        .into_iter()
        .filter(|p| listed(p))
        .collect()
}

/// The order of `files` for `query`: everything in list order without a query; otherwise the
/// matches by score (see the module docs), then list order. `positions` are byte offsets into
/// `display`.
pub fn rank_files(files: &[FileCandidate], query: &str) -> Vec<Ranked> {
    let Some(mut matcher) = matcher(query) else {
        return (0..files.len())
            .map(|index| Ranked {
                index,
                score: 0,
                positions: Vec::new(),
            })
            .collect();
    };
    let paths: Vec<&str> = files.iter().map(|f| f.display.as_str()).collect();
    let names: Vec<&str> = files.iter().map(|f| file_name(&f.display)).collect();
    let mut best: Vec<Option<Ranked>> = vec![None; files.len()];
    for m in matcher.match_list_indices(&paths) {
        let index = m.index as usize;
        best[index] = Some(Ranked {
            index,
            score: i32::from(m.score),
            positions: char_positions(paths[index], &m.indices),
        });
    }
    for m in matcher.match_list_indices(&names) {
        let index = m.index as usize;
        let score = i32::from(m.score) + NAME_BONUS;
        if best[index].as_ref().is_none_or(|b| b.score < score) {
            let offset = paths[index].len() - names[index].len();
            best[index] = Some(Ranked {
                index,
                score,
                positions: char_positions(names[index], &m.indices)
                    .into_iter()
                    .map(|p| p + offset)
                    .collect(),
            });
        }
    }
    let mut out: Vec<Ranked> = best
        .into_iter()
        .flatten()
        .map(|mut r| {
            r.score += match files[r.index].source {
                Source::Tab => TAB_BONUS,
                Source::Index => INDEX_BONUS,
                Source::Walk => 0,
            };
            r
        })
        .collect();
    // Equal scores: the shorter path, then list order.
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(
                files[a.index]
                    .display
                    .len()
                    .cmp(&files[b.index].display.len()),
            )
            .then(a.index.cmp(&b.index))
    });
    out
}

/// The last component of a displayed path.
fn file_name(display: &str) -> &str {
    display.rsplit('/').next().unwrap_or(display)
}

/// The picker row of `file`; `positions` are the matched byte offsets of its display path.
fn file_row(file: &FileCandidate, positions: &[usize]) -> PickerRow {
    let name = file_name(&file.display);
    let dir_len = file.display.len() - name.len();
    let dir = file.display[..dir_len].trim_end_matches('/');
    let (dir_positions, name_positions): (Vec<usize>, Vec<usize>) =
        positions.iter().partition(|&&p| p < dir_len);
    PickerRow {
        key: file.path.to_string_lossy().into_owned(),
        title: name.to_owned(),
        title_positions: name_positions.into_iter().map(|p| p - dir_len).collect(),
        subtitle: dir.to_owned(),
        subtitle_positions: dir_positions
            .into_iter()
            .filter(|&p| p < dir.len())
            .collect(),
        accel: String::new(),
        badge: (file.source == Source::Tab)
            .then(|| if file.dirty { "Unsaved" } else { "Open" }.to_owned()),
        checked: false,
        enabled: true,
    }
}

/// The picker rows of `files` for `query`.
fn file_rows(files: &[FileCandidate], query: &str) -> Vec<PickerRow> {
    rank_files(files, query)
        .into_iter()
        .take(MAX_ROWS)
        .map(|r| file_row(&files[r.index], &r.positions))
        .collect()
}

// ───────────── Go to Line ─────────────

/// What a query of quick open asks for ([`parse_query`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query<'a> {
    /// The files that match the text.
    Files(&'a str),
    /// A 1-based line, and column, of the active document (`file: None`) or of the files that
    /// match `file`.
    Line {
        file: Option<&'a str>,
        line: u32,
        column: Option<u32>,
    },
    /// `:` without a line number (yet).
    LineHint,
}

/// A run of ASCII digits as a 1-based number: 0 is 1, and a number too large for `u32` is its
/// largest value (both are clamped to the document later).
fn number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.parse::<u32>().unwrap_or(u32::MAX).max(1))
}

/// `N` or `N:C`, with an optional trailing `:`.
fn line_and_column(s: &str) -> Option<(u32, Option<u32>)> {
    let s = s.strip_suffix(':').unwrap_or(s);
    match s.split_once(':') {
        Some((line, column)) => Some((number(line)?, Some(number(column)?))),
        None => Some((number(s)?, None)),
    }
}

/// Reads a query of quick open (see the module docs): `:N` and `:N:C` are a line of the active
/// document, `path:N` and `path:N:C` (a trailing `:` and a leading `./` allowed) a line of the
/// files matching `path`, `:` with anything else asks for a line number, and the rest are file
/// queries (`main.tex:`, while the number is still to come, is `main.tex`).
pub fn parse_query(query: &str) -> Query<'_> {
    let query = query.trim();
    if let Some(rest) = query.strip_prefix(':') {
        return match line_and_column(rest.trim_start()) {
            Some((line, column)) => Query::Line {
                file: None,
                line,
                column,
            },
            None => Query::LineHint,
        };
    }
    let body = query.strip_suffix(':').unwrap_or(query);
    if let Some((head, last)) = body.rsplit_once(':')
        && let Some(last) = number(last)
    {
        // `path:N:C`, else `path:N` (a Windows path's drive is no line number: `C:\a.tex:3`).
        let path_and_line = head
            .rsplit_once(':')
            .and_then(|(file, line)| Some((file, number(line)?)));
        let (file, line, column) = match path_and_line {
            Some((file, line)) if !file.is_empty() => (file, line, Some(last)),
            _ => (head, last, None),
        };
        let file = file
            .strip_prefix("./")
            .or_else(|| file.strip_prefix(".\\"))
            .unwrap_or(file);
        if !file.trim().is_empty() {
            return Query::Line {
                file: Some(file),
                line,
                column,
            };
        }
    }
    Query::Files(body)
}

/// The key of a row that goes to the 1-based `line` and `column` of `path` (tests compare
/// [`crate::palette::visible`] with it). File rows' keys are their paths, which never start
/// with `line:`.
pub fn line_key(path: &Path, line: u32, column: Option<u32>) -> String {
    format!(
        "line:{line}:{}:{}",
        column.unwrap_or(0),
        path.to_string_lossy()
    )
}

/// The target of a key made by [`line_key`].
fn line_target(key: &str) -> Option<GotoTarget> {
    let mut parts = key.strip_prefix("line:")?.splitn(3, ':');
    let line: u32 = parts.next()?.parse().ok()?;
    let column: u32 = parts.next()?.parse().ok()?;
    Some(GotoTarget::LineColumn {
        path: PathBuf::from(parts.next()?),
        line: line.saturating_sub(1),
        column: column.checked_sub(1),
    })
}

/// `doc`'s number of lines as Vim counts them (see `goto::last_line`).
fn line_count(doc: &DocRef) -> u32 {
    goto::last_line(doc) + 1
}

/// "Line 12" or "Line 12, column 5".
fn line_label(line: u32, column: Option<u32>) -> String {
    match column {
        Some(column) => format!("Line {line}, column {column}"),
        None => format!("Line {line}"),
    }
}

/// "main.tex, 342 lines".
fn lines_subtitle(display: &str, lines: u32) -> String {
    let unit = if lines == 1 { "line" } else { "lines" };
    format!("{display}, {lines} {unit}")
}

/// A row that cannot be chosen: a hint about what to type.
fn hint_row(title: &str, subtitle: String) -> PickerRow {
    PickerRow {
        key: String::new(),
        title: title.to_owned(),
        subtitle,
        enabled: false,
        ..PickerRow::default()
    }
}

/// The rows of a line of the active document (`:N`, `:N:C`) or of `:` alone (`line: None`).
fn active_line_rows(state: &WindowState, line: Option<(u32, Option<u32>)>) -> Vec<PickerRow> {
    let Some(editor) = state.active_editor() else {
        return vec![hint_row(
            "No document is open",
            "Type a file name before the line number, as in main.tex:12".to_owned(),
        )];
    };
    let doc = editor.document();
    let root = state.workspace.root().path().to_path_buf();
    let display = display_path(doc.path(), &root, home_dir(state).as_deref());
    let lines = line_count(doc);
    let subtitle = lines_subtitle(&display, lines);
    let Some((line, column)) = line else {
        return vec![hint_row("Type a line number", subtitle)];
    };
    let line = line.min(lines);
    vec![PickerRow {
        key: line_key(doc.path(), line, column),
        title: format!("Go to {}", line_label(line, column).to_lowercase()),
        subtitle,
        enabled: true,
        ..PickerRow::default()
    }]
}

/// The rows of `file:N` (`file:N:C`): the files that match `file`, the one it names exactly
/// first, each going to that line of its file (clamped when the workspace has it).
fn file_line_rows(
    state: &WindowState,
    files: &[FileCandidate],
    file: &str,
    line: u32,
    column: Option<u32>,
) -> Vec<PickerRow> {
    let mut ranked = rank_files(files, file);
    let named = state.workspace.root().path().join(file);
    if let Some(exact) = files
        .iter()
        .position(|f| f.path == named || f.display == file)
    {
        let at = ranked.iter().position(|r| r.index == exact);
        let first = at.map_or(
            Ranked {
                index: exact,
                score: 0,
                positions: Vec::new(),
            },
            |at| ranked.remove(at),
        );
        ranked.insert(0, first);
    }
    // The documents the workspace has, by their (normalized) path: no file is looked at.
    let docs: HashMap<PathBuf, DocRef> = state
        .workspace
        .documents()
        .into_iter()
        .map(|d| (d.path().to_path_buf(), d))
        .collect();
    ranked
        .into_iter()
        .take(MAX_ROWS)
        .map(|r| {
            let candidate = &files[r.index];
            let line = docs
                .get(&candidate.path)
                .map_or(line, |doc| line.min(line_count(doc)));
            PickerRow {
                key: line_key(&candidate.path, line, column),
                badge: Some(line_label(line, column)),
                ..file_row(candidate, &r.positions)
            }
        })
        .collect()
}

/// The picker rows of `query` (see the module docs): files, or lines.
fn rows(state: &WindowState, files: &[FileCandidate], query: &str) -> Vec<PickerRow> {
    match parse_query(query) {
        Query::Files(text) => file_rows(files, text),
        Query::LineHint => active_line_rows(state, None),
        Query::Line {
            file: None,
            line,
            column,
        } => active_line_rows(state, Some((line, column))),
        Query::Line {
            file: Some(file),
            line,
            column,
        } => file_line_rows(state, files, file, line, column),
    }
}

/// The picker kind of quick open.
const KIND: &str = "quick-open";

/// `win.quick-open` (Ctrl+P).
pub fn show(state: &Rc<WindowState>) {
    // Open already: it selects its query and keeps its files.
    let reopened = is_open(state);
    let files = Rc::new(RefCell::new(if reopened {
        Vec::new()
    } else {
        candidates(state)
    }));
    let shown = files.clone();
    let window: Weak<WindowState> = Rc::downgrade(state);
    let picker = Picker::open(
        state,
        PickerSpec {
            kind: KIND,
            placeholder: "Go to file, or :line",
            empty_title: "No Matching Files",
            accessible_label: "Files",
            filter: Box::new(move |query| match window.upgrade() {
                Some(state) => rows(&state, &shown.borrow(), query),
                None => Vec::new(),
            }),
            choose: Box::new(|state, row| {
                let state = state.clone();
                let line = line_target(&row.key);
                let path = PathBuf::from(&row.key);
                glib::spawn_future_local(async move {
                    let result = match line {
                        Some(target) => goto::navigate(&state, target).await,
                        None => state.open_file(&path).await.map(|_| ()),
                    };
                    if let Err(e) = result {
                        state.toast(&e);
                    }
                });
            }),
        },
    );
    if reopened {
        return;
    }
    // The rest of the project, from the jobs thread.
    let root = state.workspace.root().path().to_path_buf();
    if root.parent().is_none() {
        return;
    }
    let home = home_dir(state);
    let skip = crate::agent::tools::output_dirs(state);
    let weak = Rc::downgrade(&picker);
    glib::spawn_future_local(async move {
        let walk_root = root.clone();
        let walk = move || {
            walk_files(&walk_root, &skip)
                .iter()
                .map(|p| crate::workspace::normalize(p))
                .collect::<Vec<PathBuf>>()
        };
        let Ok(walked) = gio::spawn_blocking(walk).await else {
            return;
        };
        let Some(picker) = weak.upgrade() else {
            return;
        };
        {
            let mut files = files.borrow_mut();
            let mut known: HashSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
            for path in walked {
                if known.insert(path.clone()) {
                    files.push(FileCandidate {
                        display: display_path(&path, &root, home.as_deref()),
                        path,
                        source: Source::Walk,
                        dirty: false,
                    });
                }
            }
        }
        picker.refresh();
    });
}

/// `win.goto-line` (Ctrl+G): quick open with `:` typed, the cursor after it, for a line
/// number (see the module docs). Quick open that is open already gets the `:` instead of its
/// query.
pub fn show_goto_line(state: &Rc<WindowState>) {
    show(state);
    if let Some(picker) = Picker::current(state).filter(|p| p.kind == KIND) {
        picker.type_query(":");
    }
}

/// True while quick open is the open picker of `state`'s window.
pub fn is_open(state: &WindowState) -> bool {
    Picker::current(state).is_some_and(|p| p.kind == KIND)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(display: &str, source: Source) -> FileCandidate {
        FileCandidate {
            path: PathBuf::from("/p").join(display),
            display: display.into(),
            source,
            dirty: false,
        }
    }

    #[test]
    fn display_paths() {
        let root = Path::new("/home/u/proj");
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            display_path(Path::new("/home/u/proj/ch/a.tex"), root, home),
            "ch/a.tex"
        );
        assert_eq!(
            display_path(Path::new("/home/u/texmf/x.bib"), root, home),
            "~/texmf/x.bib"
        );
        assert_eq!(
            display_path(Path::new("/usr/x.bib"), root, home),
            "/usr/x.bib"
        );
        assert!(listed(Path::new("/p/a.tex")) && listed(Path::new("/p/latexmkrc")));
        assert!(!listed(Path::new("/p/fig.png")) && !listed(Path::new("/p/main.pdf")));
    }

    #[test]
    fn file_names_win_and_tabs_come_first() {
        let files = [
            file("yoneda/notes.md", Source::Walk),
            file("chapters/yoneda.tex", Source::Walk),
            file("main.tex", Source::Index),
            file("appendix.tex", Source::Tab),
        ];
        let order = |q: &str| -> Vec<&str> {
            rank_files(&files, q)
                .iter()
                .map(|r| files[r.index].display.as_str())
                .collect()
        };
        assert_eq!(order("yon")[0], "chapters/yoneda.tex");
        assert_eq!(order(".tex")[0], "appendix.tex");
        assert_eq!(
            order("")[0],
            "yoneda/notes.md",
            "list order without a query"
        );
        let r = &rank_files(&files, "yon")[0];
        assert_eq!(r.positions, [9, 10, 11], "on the file name");
        let rows = file_rows(&files, "yon");
        assert_eq!(rows[0].title, "yoneda.tex");
        assert_eq!(rows[0].title_positions, [0, 1, 2]);
        assert_eq!(rows[0].subtitle, "chapters");
        assert_eq!(file_rows(&files, "")[3].badge.as_deref(), Some("Open"));
    }

    #[test]
    fn line_queries() {
        use Query::{Files, Line, LineHint};
        let line = |file, line, column| Line { file, line, column };
        // The active document.
        assert_eq!(parse_query(":12"), line(None, 12, None));
        assert_eq!(parse_query(" :12:5 "), line(None, 12, Some(5)));
        assert_eq!(parse_query(":12:"), line(None, 12, None));
        assert_eq!(parse_query(":0"), line(None, 1, None), "the first line");
        assert_eq!(parse_query(":3:0"), line(None, 3, Some(1)));
        assert_eq!(
            parse_query(":99999999999"),
            line(None, u32::MAX, None),
            "clamped to the document later"
        );
        assert_eq!(parse_query(": 12"), line(None, 12, None));
        for hint in [":", ":x", ":12x", ":12:x", "::"] {
            assert_eq!(parse_query(hint), LineHint, "{hint}");
        }
        // A file.
        assert_eq!(
            parse_query("ch/intro.tex:7"),
            line(Some("ch/intro.tex"), 7, None)
        );
        assert_eq!(parse_query("yon:3"), line(Some("yon"), 3, None));
        assert_eq!(
            parse_query("main.tex:12:5"),
            line(Some("main.tex"), 12, Some(5))
        );
        assert_eq!(
            parse_query("./main.tex:12:"),
            line(Some("main.tex"), 12, None),
            "as TeX writes it"
        );
        assert_eq!(
            parse_query(r"C:\proj\a.tex:3"),
            line(Some(r"C:\proj\a.tex"), 3, None),
            "a drive is no line"
        );
        // Files.
        assert_eq!(parse_query("main.tex:"), Files("main.tex"));
        assert_eq!(parse_query("chapters/yon"), Files("chapters/yon"));
        assert_eq!(parse_query("a:b"), Files("a:b"));
        assert_eq!(parse_query(""), Files(""));

        // Row keys: 1-based in the key, 0-based in the target; file keys are not line keys.
        assert_eq!(
            line_target(&line_key(Path::new("/p/a.tex"), 12, Some(5))),
            Some(GotoTarget::LineColumn {
                path: "/p/a.tex".into(),
                line: 11,
                column: Some(4)
            })
        );
        assert_eq!(
            line_target(&line_key(Path::new("/p/a:1.tex"), 1, None)),
            Some(GotoTarget::LineColumn {
                path: "/p/a:1.tex".into(),
                line: 0,
                column: None
            })
        );
        assert_eq!(line_target("/p/line:1:2.tex"), None);
        assert_eq!(line_label(3, Some(4)), "Line 3, column 4");
        assert_eq!(lines_subtitle("a.tex", 1), "a.tex, 1 line");
    }
}
