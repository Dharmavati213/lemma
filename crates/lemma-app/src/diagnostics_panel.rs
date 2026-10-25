//! Build diagnostics (ARCHITECTURE §5): the Problems sidebar page, gutter marks and underline
//! tags in the editors.
//!
//! The [`DiagnosticsPanel`] belongs to the build controller ([`crate::build_ui`]), which replaces
//! its problems after every finished build. Each problem with a line becomes a sourceview mark
//! (an icon in the gutter, with the message as its tooltip) and a wavy underline on that line;
//! the Problems page lists them grouped by file, and activating one opens the file at its line.
//!
//! A build's line numbers describe the files as they were saved for it. Edits made since then
//! (while latexmk ran, or after the build) move lines, so each problem's line is mapped through
//! the documents' line changes: an [`EditJournal`] records them from the save until the build
//! finishes, and the panel then follows every later change. Marks, underlines and Problems-list
//! navigation all use the mapped line.
//!
//! The list's rows are built when the page first shows them, and it lists at most
//! [`MAX_SECTION_ROWS`] problems, the most severe ones, then a line saying how many more there
//! are: a long thesis's thousands of bad boxes would otherwise cost tens of thousands of widgets
//! on every build. The problems it leaves out still get their gutter marks, and
//! `win.problem-next` / `win.problem-prev` (F4 / Shift+F4, `]e` / `[e` in Vim) go from the
//! cursor to the next or previous problem in the list's order, listed or not, opening its file as
//! a click on its row does ([`DiagnosticsPanel::goto_problem`]).
//!
//! **M3 (WP3.7, additive):** problems of other sources than the build — chktex warnings
//! (`DiagSource::Lint`, [`crate::lint`]) and, for WP3.2, undefined references
//! (`DiagSource::Labels`) — are [`Finding`]s in their own section below the build's problems,
//! replaced file by file ([`DiagnosticsPanel::set_findings`]) and kept across builds. A filter
//! bar (All, Build, one toggle per section) appears while there are findings. Build errors the
//! table of [`lemma_build::explain`] knows get a plain-English explanation in their tooltip and
//! in an expander under the row, with "Ask Agent" and, where [`crate::quickfix`] has a safe one,
//! a quick fix applied as reviewable hunks.

use crate::document::{DocRef, Document, ObserverId};
use crate::editor::Editor;
use crate::window::{PAGE_PROBLEMS, WindowState};
use crate::workspace::normalize;
use adw::prelude::*;
use lemma_core::{DiagSource, Diagnostic, Severity};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};

/// Mark category of error lines.
pub const ERROR_CATEGORY: &str = "lemma-error";
/// Mark category of warning lines.
pub const WARNING_CATEGORY: &str = "lemma-warning";
/// Underline tag of error lines.
pub const ERROR_TAG: &str = "lemma-diag-error";
/// Underline tag of warning lines.
pub const WARNING_TAG: &str = "lemma-diag-warning";

/// Gutter icon of error lines (coloured: symbolic icons would take the text colour).
const ERROR_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><circle cx="8" cy="8" r="6.5" fill="#e01b24"/><rect x="4.5" y="7" width="7" height="2" rx="1" fill="#fff"/></svg>"##;
/// Gutter icon of warning lines.
const WARNING_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path d="M8 2 14.5 14H1.5Z" fill="#e5a50a" stroke="#e5a50a" stroke-width="1.5" stroke-linejoin="round"/><rect x="7.15" y="6" width="1.7" height="4.5" rx=".85" fill="#241f31"/><circle cx="8" cy="12.2" r=".95" fill="#241f31"/></svg>"##;

/// Styles of the Problems page.
pub(crate) const CSS: &str = "
.lemma-problems-summary { margin: 6px 12px 0 12px; }
.lemma-problems-header { margin: 10px 8px 2px 8px; }
.lemma-problems row { padding-top: 3px; padding-bottom: 3px; }
.lemma-problems-filter { margin: 8px 10px 2px 10px; }
.lemma-problems-section { margin: 16px 12px 0 12px; }
.lemma-problem-details { margin: 4px 0 2px 24px; }
.lemma-problem-details button { padding: 2px 10px; min-height: 24px; }
.lemma-problem-toggle { min-width: 22px; min-height: 22px; padding: 0; margin: -2px 0; }
.lemma-problems-more { margin: 4px 2px; }
";

/// Extensions of files TeX writes itself; diagnostics while reading them belong to the root.
const GENERATED_EXTENSIONS: &[&str] = &[
    "aux", "bbl", "toc", "lof", "lot", "out", "nav", "snm", "ind", "gls", "glo", "idx", "bcf",
];

/// One diagnostic as the Problems page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The file to open (canonical); diagnostics in generated or outside files are moved to
    /// the root document.
    pub path: PathBuf,
    /// 1-based line, if known.
    pub line: Option<u32>,
    pub severity: Severity,
    pub message: String,
    /// TeX's error context, or where a moved diagnostic came from.
    pub context: Option<String>,
    pub source: DiagSource,
}

/// Turns a build's diagnostics into problems, sorted for display. Files are ordered by their most
/// severe problem (files with errors first, then those with warnings, then the rest), the root
/// first among equals, then by path; within a file come errors, then warnings, then bad boxes,
/// each by line. So the first row is always an error when there is one.
///
/// Diagnostics in files TeX generated (`.aux`, `.bbl`, … or anything under `outdir`) and in
/// files outside the root's directory (packages of the TeX distribution) are attributed to the
/// root document without a line. When `outdir` is the root's directory (`"."`) or above it, the
/// sources live there too, so only the extension marks a file as generated. Paths are
/// canonicalized, so call this off the main thread.
pub fn problems_from(diagnostics: &[Diagnostic], root: &Path, outdir: &str) -> Vec<Problem> {
    let root = normalize(root);
    let root_dir = root.parent().map(Path::to_path_buf).unwrap_or_default();
    let out_dir = lexical_normalize(&root_dir.join(outdir));
    let separate_out_dir = !root_dir.starts_with(&out_dir);
    let mut problems: Vec<Problem> = diagnostics
        .iter()
        .map(|d| {
            let path = normalize(&d.path);
            let generated = (separate_out_dir && path.starts_with(&out_dir))
                || path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| GENERATED_EXTENSIONS.contains(&e));
            if generated || !path.starts_with(&root_dir) {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                Problem {
                    path: root.clone(),
                    line: None,
                    severity: d.severity,
                    message: d.message.clone(),
                    context: Some(match &d.context {
                        Some(c) => format!("in {name}: {c}"),
                        None => format!("in {name}"),
                    }),
                    source: d.source,
                }
            } else {
                Problem {
                    path,
                    line: d.line,
                    severity: d.severity,
                    message: d.message.clone(),
                    context: d.context.clone(),
                    source: d.source,
                }
            }
        })
        .collect();
    let mut worst: HashMap<PathBuf, u8> = HashMap::new();
    for p in &problems {
        let rank = worst.entry(p.path.clone()).or_insert(u8::MAX);
        *rank = (*rank).min(severity_rank(p.severity));
    }
    let key = |p: &Problem| {
        let file = worst.get(&p.path).copied().unwrap_or(u8::MAX);
        (
            file,
            p.path != root,
            p.path.clone(),
            severity_rank(p.severity),
            p.line,
        )
    };
    problems.sort_by_cached_key(key);
    problems.dedup();
    problems
}

/// `path` with `.` components dropped and each `..` folded into the component before it.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A line change `(first, removed, added)`: lines `[first, first + removed)` of the old text
/// became `[first, first + added)` (0-based, as `LineIndex::line_delta` reports it).
type LineChange = (u32, u32, u32);

/// Where the 1-based `line` is after `change`. Lines after the changed range shift; a line inside
/// it goes to the range's first line, which is also where a gutter mark at the start of that line
/// ends up.
fn moved_line(line: u32, (first, removed, added): LineChange) -> u32 {
    let l = line.saturating_sub(1);
    let moved = if l < first {
        l
    } else if l >= first.saturating_add(removed) {
        l - removed + added
    } else {
        first
    };
    moved + 1
}

/// Records the line changes of documents from the moment a build's sources were saved, so that
/// the build's line numbers can be mapped to the text in the editors when it finishes
/// ([`DiagnosticsPanel::set_from_build`]). Recording stops when it is dropped.
pub struct EditJournal {
    docs: HashMap<PathBuf, JournalDoc>,
}

struct JournalDoc {
    doc: Weak<Document>,
    observer: ObserverId,
    changes: Rc<RefCell<Vec<LineChange>>>,
}

impl EditJournal {
    /// Starts recording the changes of `documents` (every document of the workspace, right after
    /// they were saved for a build).
    pub fn start(documents: &[DocRef]) -> Self {
        let docs = documents
            .iter()
            .map(|doc| {
                let changes = Rc::new(RefCell::new(Vec::new()));
                let sink = Rc::downgrade(&changes);
                let observer = doc.connect_lines_changed(move |first, removed, added| {
                    if let Some(sink) = sink.upgrade() {
                        sink.borrow_mut().push((first, removed, added));
                    }
                });
                let entry = JournalDoc {
                    doc: Rc::downgrade(doc),
                    observer,
                    changes,
                };
                (doc.path().to_path_buf(), entry)
            })
            .collect();
        Self { docs }
    }
}

impl Drop for JournalDoc {
    fn drop(&mut self) {
        if let Some(doc) = self.doc.upgrade() {
            doc.disconnect(self.observer);
        }
    }
}

/// Keeps the problems' lines of one document current while it changes.
struct LineTracker {
    doc: Weak<Document>,
    observer: ObserverId,
}

impl Drop for LineTracker {
    fn drop(&mut self) {
        if let Some(doc) = self.doc.upgrade() {
            doc.disconnect(self.observer);
        }
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::BadBox => 2,
        Severity::Info => 3,
    }
}

/// `message` without TeX's trailing "on input line N." (the list shows the line separately).
pub fn display_message(message: &str) -> &str {
    let trimmed = message.trim_end().trim_end_matches('.');
    let without_line = trimmed.trim_end_matches(|c: char| c.is_ascii_digit());
    match without_line.strip_suffix("on input line ") {
        Some(rest) if without_line.len() < trimmed.len() => rest
            .trim_end()
            .trim_end_matches("; reported")
            .trim_end_matches([';', ',']),
        _ => message,
    }
}

/// Numbers of errors, warnings and bad boxes in `problems`.
pub fn counts(problems: &[Problem]) -> (usize, usize, usize) {
    let count = |s| problems.iter().filter(|p| p.severity == s).count();
    (
        count(Severity::Error),
        count(Severity::Warning),
        count(Severity::BadBox),
    )
}

/// "2 errors · 1 warning · 3 bad boxes"; empty when there is nothing.
pub fn summary(errors: usize, warnings: usize, bad_boxes: usize) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(plural(errors, "error", "errors"));
    }
    if warnings > 0 {
        parts.push(plural(warnings, "warning", "warnings"));
    }
    if bad_boxes > 0 {
        parts.push(plural(bad_boxes, "bad box", "bad boxes"));
    }
    parts.join(" · ")
}

/// The Problems page plus the marks and underlines in the editors.
pub struct DiagnosticsPanel {
    inner: Rc<Inner>,
}

struct Inner {
    state: Weak<WindowState>,
    /// "none" (never built), "clean" or "list".
    stack: gtk::Stack,
    list: gtk::ListBox,
    summary: gtk::Label,
    problems: Rc<RefCell<Vec<Problem>>>,
    /// Number of problems per file, for the group headers.
    file_counts: Rc<RefCell<HashMap<PathBuf, usize>>>,
    /// Each problem's 1-based line in the current text: its build line moved by the edits made
    /// since the build's sources were saved. `None` when it has no line, or when its document
    /// changed in ways that were not recorded.
    lines: Rc<RefCell<Vec<Option<u32>>>>,
    /// The observers that keep `lines` current, per document path.
    trackers: RefCell<HashMap<PathBuf, LineTracker>>,
    /// The name of each problem's gutter mark, by problem index.
    mark_names: RefCell<HashMap<usize, String>>,
    /// The root document of the build the problems come from.
    root: Rc<RefCell<PathBuf>>,
    /// Tooltip of every gutter mark, by mark name.
    tooltips: Rc<RefCell<HashMap<String, String>>>,
    next_mark: Cell<u64>,
    // ----- M3 (WP3.7): findings, the filter and explanations -----
    /// The filter bar (shown while a section has findings) and its toggles.
    filter_bar: gtk::Box,
    filter: adw::ToggleGroup,
    /// The sections of findings, in the order their sources first appeared.
    sections: RefCell<Vec<Rc<Section>>>,
    sections_box: gtk::Box,
    /// Shown instead of the build's rows when the filter shows only the build and it has none.
    build_empty: gtk::Label,
    /// A build finished (the page is past "none").
    built: Cell<bool>,
    /// The details (explanation, actions) of the build rows, by problem index.
    details: RefCell<HashMap<usize, RowDetails>>,
    /// True while [`Inner::refresh_page`] runs.
    refreshing: Cell<bool>,
    /// The error kinds whose first row was expanded (the others start collapsed).
    expanded_kinds: RefCell<std::collections::HashSet<lemma_build::explain::ErrorKind>>,
    /// `list` holds the rows of `problems` (they are built when the page is shown).
    rows_built: Cell<bool>,
    /// The problem each row of `list` shows, by row index ([`listed_problems`]).
    listed: Rc<RefCell<Vec<usize>>>,
    /// The problem gone to last (a row's click, next or previous problem): the next step goes
    /// on from it while the cursor is still on its line.
    at: Cell<Option<usize>>,
    /// True while the file of the problem gone to last is being opened.
    opening: Cell<bool>,
}

impl DiagnosticsPanel {
    /// The panel of `state`'s window; [`install`] puts its widget into the sidebar.
    pub fn new(state: &Rc<WindowState>) -> Self {
        let none = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("No Problems")
            .description("Errors and warnings of the last build appear here.")
            .css_classes(["compact", "dim-label"])
            .build();
        let clean = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .title("No Problems")
            .description("The last build finished without errors or warnings.")
            .css_classes(["compact", "dim-label"])
            .build();
        let summary = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label", "lemma-problems-summary"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .css_classes(["navigation-sidebar", "lemma-problems"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // M3: the filter bar (hidden until there are findings) above the summary; the build's
        // list, its "nothing" line and the findings' sections share the scroller.
        // Not homogeneous: equal widths cut "References" next to All, Build and Lint in the
        // sidebar's 300 px.
        let filter = adw::ToggleGroup::builder()
            .homogeneous(false)
            .hexpand(true)
            .build();
        filter.add(
            adw::Toggle::builder()
                .name(FILTER_ALL)
                .label("All")
                .tooltip("Every problem")
                .build(),
        );
        filter.add(
            adw::Toggle::builder()
                .name(FILTER_BUILD)
                .label("Build")
                .tooltip("Errors and warnings of the last build")
                .build(),
        );
        filter.set_active_name(Some(FILTER_ALL));
        let filter_bar = gtk::Box::builder()
            .css_classes(["lemma-problems-filter"])
            .visible(false)
            .build();
        filter_bar.append(&filter);
        let build_empty = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["dim-label", "lemma-problems-section"])
            .build();
        let sections_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&list);
        content.append(&build_empty);
        content.append(&sections_box);
        scroller.set_child(Some(&content));
        page.append(&filter_bar);
        page.append(&summary);
        page.append(&scroller);
        let stack = gtk::Stack::new();
        stack.add_named(&none, Some("none"));
        stack.add_named(&clean, Some("clean"));
        stack.add_named(&page, Some("list"));
        stack.set_visible_child_name("none");

        let inner = Rc::new(Inner {
            state: Rc::downgrade(state),
            stack,
            list,
            summary,
            problems: Rc::new(RefCell::new(Vec::new())),
            file_counts: Rc::new(RefCell::new(HashMap::new())),
            lines: Rc::new(RefCell::new(Vec::new())),
            trackers: RefCell::new(HashMap::new()),
            mark_names: RefCell::new(HashMap::new()),
            root: Rc::new(RefCell::new(PathBuf::new())),
            tooltips: Rc::new(RefCell::new(HashMap::new())),
            next_mark: Cell::new(0),
            filter_bar,
            filter,
            sections: RefCell::new(Vec::new()),
            sections_box,
            build_empty,
            built: Cell::new(false),
            details: RefCell::new(HashMap::new()),
            refreshing: Cell::new(false),
            expanded_kinds: RefCell::new(std::collections::HashSet::new()),
            rows_built: Cell::new(false),
            listed: Rc::new(RefCell::new(Vec::new())),
            at: Cell::new(None),
            opening: Cell::new(false),
        });
        inner.connect_list();
        inner.connect_context_menu();
        inner.connect_findings();
        Self { inner }
    }

    /// The Problems page widget.
    pub fn widget(&self) -> gtk::Widget {
        self.inner.stack.clone().upcast()
    }

    /// Replaces the problems with `problems` of a build of `root` whose lines describe the
    /// current text, in the list and in every open editor.
    pub fn set(&self, problems: Vec<Problem>, root: &Path) {
        let documents = self
            .inner
            .state
            .upgrade()
            .map(|state| state.workspace.documents())
            .unwrap_or_default();
        self.set_from_build(problems, root, EditJournal::start(&documents));
    }

    /// Replaces the problems with those of a finished build of `root`. `journal` holds the edits
    /// made since the build's sources were saved; the problems' lines are moved through them
    /// before they are marked, and follow every later edit.
    pub fn set_from_build(&self, problems: Vec<Problem>, root: &Path, journal: EditJournal) {
        let inner = &self.inner;
        inner.root.replace(root.to_path_buf());
        inner.built.set(true);
        inner.details.borrow_mut().clear();
        inner.expanded_kinds.borrow_mut().clear();
        inner.at.set(None);
        inner.opening.set(false);
        // Empty the list while there are no problems: each removed row makes the list rerun the
        // header function on the next one, which must not look at the new problems.
        inner.problems.replace(Vec::new());
        inner.listed.replace(Vec::new());
        inner.list.remove_all();
        inner.trackers.borrow_mut().clear();
        inner.mark_names.borrow_mut().clear();
        inner.tooltips.borrow_mut().clear();
        let mut counts = HashMap::new();
        for problem in &problems {
            *counts.entry(problem.path.clone()).or_insert(0) += 1;
        }
        inner.file_counts.replace(counts);
        inner
            .lines
            .replace(problems.iter().map(|p| p.line).collect());
        inner.problems.replace(problems);
        inner.follow_edits(&journal);
        drop(journal);
        inner.fill_list();
        if let Some(state) = inner.state.upgrade() {
            for editor in state.editors() {
                inner.apply(&editor);
            }
        }
    }

    /// The problems shown.
    pub fn problems(&self) -> Vec<Problem> {
        self.inner.problems.borrow().clone()
    }

    /// Numbers of errors, warnings and bad boxes shown.
    pub fn counts(&self) -> (usize, usize, usize) {
        counts(&self.inner.problems.borrow())
    }

    /// Opens problem `index` at its line, as a click on its row does (also for the problems the
    /// list does not show).
    pub fn activate(&self, index: usize) {
        self.inner.open_problem(index);
    }

    /// Goes to the next problem after the active editor's cursor (`forward`), or the previous
    /// one before it, in the list's order: `win.problem-next` / `win.problem-prev`. The same file
    /// comes first, then the files after (before) it, wrapping around; repeated steps go on from
    /// the problem gone to last while the cursor stays on its line. The problem's file is opened
    /// at the problem's current line, as a click on its row does. Problems without a line are
    /// skipped, and so are the findings (lint warnings, references). Toasts "No problems" when
    /// the build has none.
    pub fn goto_problem(&self, forward: bool) {
        self.inner.goto_problem(forward);
    }

    /// Problem `index` as "Ask agent to fix" sends it: with its line in the current text (the
    /// line its mark moved to, or its build line moved by the edits since).
    pub fn problem_for_agent(&self, index: usize) -> Option<Problem> {
        self.inner.problem_for_agent(index)
    }

    /// "Ask agent to fix" on problem `index` (the rows' context-menu item, action
    /// `problems.ask-agent-fix(u)` on the page): passes it to `agent::quick::fix_diagnostic`.
    pub fn ask_agent(&self, index: usize) {
        self.inner.ask_agent(index);
    }

    /// The context menu of problem `index` (what a right click on its row shows); `None` past
    /// the last problem.
    pub fn context_menu(&self, index: usize) -> Option<gio::Menu> {
        let index = u32::try_from(index)
            .ok()
            .filter(|&i| (i as usize) < self.inner.problems.borrow().len())?;
        let menu = context_menu(index);
        self.inner.add_fix_item(&menu, index);
        Some(menu)
    }

    /// Prepares a new editor: gutter mark icons and tooltips, then this file's problems.
    pub fn attach_editor(&self, editor: &Editor) {
        let view = editor.view();
        for (category, icon, priority) in [
            (ERROR_CATEGORY, ERROR_ICON, 20),
            (WARNING_CATEGORY, WARNING_ICON, 10),
        ] {
            let attributes = sourceview5::MarkAttributes::new();
            attributes.set_gicon(&gio::BytesIcon::new(&glib::Bytes::from_static(icon)));
            let tooltips = self.inner.tooltips.clone();
            attributes.connect_query_tooltip_text(move |_, mark| {
                mark.name()
                    .and_then(|name| tooltips.borrow().get(name.as_str()).cloned())
                    .unwrap_or_default()
            });
            view.set_mark_attributes(category, &attributes, priority);
        }
        self.inner.follow(editor.document());
        self.inner.follow_findings(editor.document());
        self.inner.apply(editor);
    }
}

impl Inner {
    /// Moves the problems' lines through the changes `journal` recorded, then follows the later
    /// changes of their documents.
    fn follow_edits(&self, journal: &EditJournal) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut by_path: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for (i, problem) in self.problems.borrow().iter().enumerate() {
            if problem.line.is_some() {
                by_path.entry(problem.path.clone()).or_default().push(i);
            }
        }
        for (path, indexes) in by_path {
            let recorded = journal.docs.get(&path);
            if let Some(recorded) = recorded {
                let changes = recorded.changes.borrow();
                let mut lines = self.lines.borrow_mut();
                for &i in &indexes {
                    lines[i] = lines[i].map(|l| changes.iter().fold(l, |l, &c| moved_line(l, c)));
                }
            }
            // A document the journal did not see was loaded since the save: it matches the
            // build's text unless it was edited before it could be followed.
            let Some(doc) = state.workspace.get(&path) else {
                continue;
            };
            let recorded_doc = recorded.and_then(|r| r.doc.upgrade());
            if recorded_doc.is_some_and(|d| Rc::ptr_eq(&d, &doc)) || doc.version() == 0 {
                let tracker = self.tracker(&doc, indexes);
                self.trackers.borrow_mut().insert(path, tracker);
            } else {
                let mut lines = self.lines.borrow_mut();
                for i in indexes {
                    lines[i] = None;
                }
            }
        }
    }

    /// Follows the changes of `doc` (a document an editor now shows) unless that already happens.
    fn follow(&self, doc: &DocRef) {
        let followed = self
            .trackers
            .borrow()
            .get(doc.path())
            .and_then(|t| t.doc.upgrade())
            .is_some_and(|d| Rc::ptr_eq(&d, doc));
        if followed {
            return;
        }
        let indexes: Vec<usize> = self
            .problems
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.line.is_some() && p.path == doc.path())
            .map(|(i, _)| i)
            .collect();
        if indexes.is_empty() {
            return;
        }
        if doc.version() != 0 {
            // Edited before anything followed it: where its lines went is unknown.
            let mut lines = self.lines.borrow_mut();
            for i in indexes {
                lines[i] = None;
            }
            return;
        }
        let tracker = self.tracker(doc, indexes);
        self.trackers
            .borrow_mut()
            .insert(doc.path().to_path_buf(), tracker);
    }

    /// An observer that moves the lines of the problems `indexes` with every change of `doc`.
    fn tracker(&self, doc: &DocRef, indexes: Vec<usize>) -> LineTracker {
        let lines = Rc::downgrade(&self.lines);
        let observer = doc.connect_lines_changed(move |first, removed, added| {
            let Some(lines) = lines.upgrade() else {
                return;
            };
            let mut lines = lines.borrow_mut();
            for &i in &indexes {
                if let Some(Some(line)) = lines.get_mut(i) {
                    *line = moved_line(*line, (first, removed, added));
                }
            }
        });
        LineTracker {
            doc: Rc::downgrade(doc),
            observer,
        }
    }

    /// The 1-based line where problem `index` (which must still be `problem`) is now in
    /// `editor`: its gutter mark's line, else its mapped line, else the build's line.
    fn current_line(&self, index: usize, problem: &Problem, editor: &Editor) -> Option<u32> {
        if self.problems.borrow().get(index) != Some(problem) {
            return problem.line;
        }
        let buffer = editor.buffer();
        let marked = self
            .mark_names
            .borrow()
            .get(&index)
            .and_then(|name| buffer.mark(name))
            .filter(|mark| !mark.is_deleted())
            .map(|mark| buffer.iter_at_mark(&mark).line());
        if let Some(line) = marked.and_then(|l| u32::try_from(l).ok()) {
            return Some(line + 1);
        }
        self.lines
            .borrow()
            .get(index)
            .copied()
            .flatten()
            .or(problem.line)
    }

    fn connect_list(self: &Rc<Self>) {
        let problems = self.problems.clone();
        let listed = self.listed.clone();
        let file_counts = self.file_counts.clone();
        let root = self.root.clone();
        self.list.set_header_func(move |row, before| {
            let problems = problems.borrow();
            let listed = listed.borrow();
            let shown = |r: &gtk::ListBoxRow| row_problem(&listed, r).and_then(|i| problems.get(i));
            let Some(current) = shown(row) else {
                row.set_header(None::<&gtk::Widget>);
                return;
            };
            let previous = before.and_then(shown);
            if previous.is_some_and(|p| p.path == current.path) {
                row.set_header(None::<&gtk::Widget>);
            } else {
                let count = file_counts
                    .borrow()
                    .get(&current.path)
                    .copied()
                    .unwrap_or(0);
                row.set_header(Some(&file_header(&current.path, &root.borrow(), count)));
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let index = row_problem(&inner.listed.borrow(), row);
            if let Some(index) = index {
                inner.open_problem(index);
            }
        });
        // The rows are built when the page first shows them (see `fill_list`).
        let weak = Rc::downgrade(self);
        self.list.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.build_rows();
            }
        });
    }

    /// Opens the file of problem `index` and puts the cursor on the problem's current line (a
    /// click on its row, next or previous problem).
    fn open_problem(self: &Rc<Self>, index: usize) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(problem) = self.problems.borrow().get(index).cloned() else {
            return;
        };
        self.at.set(Some(index));
        self.opening.set(true);
        state.sidebar.dismiss_overlay();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // The line is looked up once the file is open, so it is the marked line.
            let opened = state.open_file(&problem.path).await;
            let inner = weak.upgrade();
            if let Some(inner) = &inner
                && inner.at.get() == Some(index)
            {
                inner.opening.set(false);
            }
            match opened {
                Ok(editor) => {
                    let line = inner.map_or(problem.line, |inner| {
                        inner.current_line(index, &problem, &editor)
                    });
                    if let Some(line) = line {
                        goto_line(&editor, line);
                        scroll_again_after_first_frame(&editor);
                    }
                }
                Err(e) => state.toast(&e),
            }
        });
    }

    fn goto_problem(self: &Rc<Self>, forward: bool) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let target = {
            let problems = self.problems.borrow();
            if problems.is_empty() {
                // Lint warnings and references have rows of their own, but are no build problems.
                let findings = self.sections.borrow().iter().any(|s| s.total() > 0);
                state.toast(if findings {
                    "No build problems"
                } else {
                    "No problems"
                });
                return;
            }
            let editor = state.active_editor();
            let cursor = editor
                .as_ref()
                .map(|e| (normalize(e.document().path()), e.cursor_position().0));
            // Lines past the end are the last line, where `goto_line` put the cursor (a bad box
            // has no mark to keep it in the text).
            let line = |i: usize| match &editor {
                Some(editor) => {
                    let last = u32::try_from(editor.buffer().line_count()).unwrap_or(u32::MAX);
                    self.current_line(i, &problems[i], editor)
                        .map(|l| l.min(last))
                }
                None => problems[i].line,
            };
            // Going on from the problem gone to last while its file opens or the cursor is
            // still on its line; otherwise from the cursor.
            let from = self.at.get().filter(|&i| {
                self.opening.get()
                    || cursor.as_ref().is_some_and(|(path, cursor_line)| {
                        problems.get(i).is_some_and(|p| p.path == *path)
                            && line(i) == Some(*cursor_line)
                    })
            });
            let cursor = cursor.as_ref().map(|(path, line)| (path.as_path(), *line));
            step_target(&problems, from, cursor, line, forward)
        };
        match target {
            Some(index) => self.open_problem(index),
            None => state.toast("No problem has a line to go to"),
        }
    }

    fn problem_for_agent(&self, index: usize) -> Option<Problem> {
        let mut problem = self.problems.borrow().get(index).cloned()?;
        let editor = self.state.upgrade().and_then(|state| {
            state
                .editors()
                .into_iter()
                .find(|e| normalize(e.document().path()) == problem.path)
        });
        problem.line = match editor {
            Some(editor) => self.current_line(index, &problem, &editor),
            None => self
                .lines
                .borrow()
                .get(index)
                .copied()
                .flatten()
                .or(problem.line),
        };
        Some(problem)
    }

    fn ask_agent(&self, index: usize) {
        if let (Some(state), Some(problem)) = (self.state.upgrade(), self.problem_for_agent(index))
        {
            crate::agent::quick::fix_diagnostic(&state, &problem);
        }
    }

    /// The rows' context menu (right click, long press, Menu or Shift+F10 on a row) with "Ask
    /// Agent to Fix", backed by the `problems.ask-agent-fix(u)` action on the list.
    fn connect_context_menu(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        let action = gio::SimpleAction::new("ask-agent-fix", Some(glib::VariantTy::UINT32));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, index| {
            if let (Some(inner), Some(index)) = (weak.upgrade(), index.and_then(|v| v.get::<u32>()))
            {
                inner.ask_agent(index as usize);
            }
        });
        group.add_action(&action);
        let action = gio::SimpleAction::new("apply-fix", Some(glib::VariantTy::UINT32));
        let weak = Rc::downgrade(self);
        action.connect_activate(move |_, index| {
            if let (Some(inner), Some(index)) = (weak.upgrade(), index.and_then(|v| v.get::<u32>()))
            {
                inner.apply_fix(index as usize);
            }
        });
        group.add_action(&action);
        // On the page's root, an ancestor of the list and of the menus.
        self.stack.insert_action_group("problems", Some(&group));

        let click = gtk::GestureClick::builder().button(3).build();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(inner) = weak.upgrade()
                && let Some(row) = inner.list.row_at_y(y as i32)
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                inner.show_context_menu(&row, Some((x, y)));
            }
        });
        self.list.add_controller(click);
        let press = gtk::GestureLongPress::new();
        let weak = Rc::downgrade(self);
        press.connect_pressed(move |_, x, y| {
            if let Some(inner) = weak.upgrade()
                && let Some(row) = inner.list.row_at_y(y as i32)
            {
                inner.show_context_menu(&row, Some((x, y)));
            }
        });
        self.list.add_controller(press);
        let keys = gtk::ShortcutController::new();
        let weak = Rc::downgrade(self);
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Menu|<Shift>F10"),
            Some(gtk::CallbackAction::new(move |list, _| {
                let Some(inner) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let focused = list
                    .root()
                    .and_then(|root| root.focus())
                    .and_then(|w| w.ancestor(gtk::ListBoxRow::static_type()))
                    .and_then(|w| w.downcast::<gtk::ListBoxRow>().ok());
                match focused {
                    Some(row) => {
                        inner.show_context_menu(&row, None);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            })),
        ));
        self.list.add_controller(keys);
    }

    /// Shows the context menu of `row`, at `at` (list coordinates) or below the row.
    fn show_context_menu(&self, row: &gtk::ListBoxRow, at: Option<(f64, f64)>) {
        let index = row_problem(&self.listed.borrow(), row);
        let Some(index) = index.and_then(|i| u32::try_from(i).ok()) else {
            return;
        };
        let menu = context_menu(index);
        self.add_fix_item(&menu, index);
        // Parented to the page's box, which treats any child alike (a list box or its rows
        // would complain about a foreign child when they are refilled or go).
        let Some(host) = self.summary.parent() else {
            return;
        };
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_has_arrow(false);
        popover.set_parent(&host);
        let point = at.and_then(|(x, y)| {
            self.list
                .compute_point(&host, &gtk::graphene::Point::new(x as f32, y as f32))
        });
        let rect = match point {
            Some(p) => gtk::gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1),
            None => {
                let bounds = row.compute_bounds(&host);
                bounds.map_or(gtk::gdk::Rectangle::new(0, 0, 1, 1), |b| {
                    gtk::gdk::Rectangle::new(
                        b.x() as i32,
                        b.y() as i32,
                        b.width() as i32,
                        b.height() as i32,
                    )
                })
            }
        };
        popover.set_pointing_to(Some(&rect));
        // Unparented once closed (after its action ran).
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
        });
        popover.popup();
    }

    /// Fills the list (it is empty) and updates the summary. The rows are widgets (several per
    /// problem): they are built when the page shows them, so the problems of a build nobody
    /// looks at in the list cost no widgets.
    fn fill_list(self: &Rc<Self>) {
        self.rows_built.set(false);
        if self.list.is_mapped() {
            self.build_rows();
        }
        let (errors, _, _) = counts(&self.problems.borrow());
        self.refresh_page();
        if let Some(state) = self.state.upgrade() {
            let page = state.sidebar.stack.page(&state.sidebar.problems);
            page.set_needs_attention(errors > 0);
        }
    }

    /// Fills the list with the rows of at most [`MAX_SECTION_ROWS`] problems, the most severe
    /// ones ([`listed_problems`]), and a line counting the others, unless it holds them.
    fn build_rows(self: &Rc<Self>) {
        if self.rows_built.replace(true) {
            return;
        }
        let problems = self.problems.borrow();
        let listed = listed_problems(&problems, MAX_SECTION_ROWS);
        // Before the rows: appending one runs the header function, which looks it up.
        self.listed.replace(listed.clone());
        for &index in &listed {
            self.list.append(&self.build_row(index, &problems[index]));
        }
        if problems.len() > listed.len() {
            self.list.append(&more_row(problems.len() - listed.len()));
        }
    }

    /// Replaces the marks and underlines in `editor`'s buffer with its file's problems. Like
    /// Vim's `signcolumn=auto`, the gutter's marks column only takes space while there are
    /// marks.
    fn apply(&self, editor: &Editor) {
        let buffer = editor.buffer();
        let (start, end) = buffer.bounds();
        for category in [ERROR_CATEGORY, WARNING_CATEGORY] {
            buffer.remove_source_marks(&start, &end, Some(category));
        }
        let error_tag = ensure_tag(
            buffer,
            ERROR_TAG,
            gtk::gdk::RGBA::new(0.88, 0.11, 0.14, 1.0),
        );
        let warning_tag = ensure_tag(
            buffer,
            WARNING_TAG,
            gtk::gdk::RGBA::new(0.90, 0.65, 0.04, 1.0),
        );
        buffer.remove_tag(&error_tag, &start, &end);
        buffer.remove_tag(&warning_tag, &start, &end);
        let path = normalize(editor.document().path());
        let problems = self.problems.borrow();
        let lines = self.lines.borrow();
        let mut tooltips = self.tooltips.borrow_mut();
        let mut mark_names = self.mark_names.borrow_mut();
        let mut marked = false;
        for (i, problem) in problems.iter().enumerate().filter(|(_, p)| p.path == path) {
            if let Some(old) = mark_names.remove(&i) {
                tooltips.remove(&old);
            }
            let (category, tag) = match problem.severity {
                Severity::Error => (ERROR_CATEGORY, &error_tag),
                Severity::Warning => (WARNING_CATEGORY, &warning_tag),
                Severity::BadBox | Severity::Info => continue,
            };
            let Some(line) = lines.get(i).copied().flatten() else {
                continue;
            };
            let last = buffer.line_count().saturating_sub(1);
            let line = i32::try_from(line.saturating_sub(1)).map_or(last, |l| l.min(last));
            let Some(mut line_start) = buffer.iter_at_line(line) else {
                continue;
            };
            let id = self.next_mark.get();
            self.next_mark.set(id + 1);
            let name = format!("lemma-diag-{id}");
            buffer.create_source_mark(Some(&name), category, &line_start);
            let tooltip = match crate::quickfix::explanation(problem) {
                Some(e) => format!("{}\n{}", display_message(&problem.message), e.text),
                None => display_message(&problem.message).to_owned(),
            };
            tooltips.insert(name.clone(), tooltip);
            mark_names.insert(i, name);
            marked = true;
            // Underline the line's text, without its indentation.
            while !line_start.ends_line() && line_start.char().is_whitespace() {
                line_start.forward_char();
            }
            let mut line_end = line_start;
            if !line_end.ends_line() {
                line_end.forward_to_line_end();
            }
            buffer.apply_tag(tag, &line_start, &line_end);
        }
        editor.view().set_show_line_marks(marked);
        drop((problems, lines, tooltips, mark_names));
        self.apply_findings(editor);
    }
}

/// The problem a row of the build's list shows (`listed` is [`Inner::listed`]): its index,
/// `None` for the line after the last one the list holds.
fn row_problem(listed: &[usize], row: &gtk::ListBoxRow) -> Option<usize> {
    usize::try_from(row.index())
        .ok()
        .and_then(|i| listed.get(i).copied())
}

/// The problems the build's list shows, in its order: all of them, or when there are more than
/// `max`, the `max` most severe ones (errors, then warnings, then bad boxes, each in the list's
/// order). Its first `max` rows would not do: a file's errors come before its bad boxes, but the
/// errors and warnings of the files after it come after them.
fn listed_problems(problems: &[Problem], max: usize) -> Vec<usize> {
    let mut listed: Vec<usize> = (0..problems.len()).collect();
    if listed.len() > max {
        listed.sort_by_key(|&i| (severity_rank(problems[i].severity), i));
        listed.truncate(max);
        listed.sort_unstable();
    }
    listed
}

/// The last row of a list cut at [`MAX_SECTION_ROWS`]: how many problems it leaves out.
fn more_row(left_out: usize) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(format!("{left_out} more not listed"))
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "lemma-problems-more"])
        .build();
    gtk::ListBoxRow::builder()
        .child(&label)
        .activatable(false)
        .selectable(false)
        .build()
}

/// The problem that next problem (`forward`) or previous problem goes to: an index into
/// `problems`, which are in the Problems page's order. Problems without a line are skipped;
/// `None` when no problem has one.
///
/// With `from` (the problem gone to last, while the cursor is still on its line) it is the
/// problem after (before) it. Otherwise it is found from the `cursor` (the active editor's file
/// and 1-based line; `line(i)` is the current line of problem `i` of that file): going forward,
/// the first of that file's problems whose line is after the cursor, else the first problem of
/// the files after it; going back, the last one whose line is before the cursor, else the last
/// problem of the files before it. Both wrap around. From a file without problems (or no file)
/// they go to the first and the last problem.
fn step_target(
    problems: &[Problem],
    from: Option<usize>,
    cursor: Option<(&Path, u32)>,
    line: impl Fn(usize) -> Option<u32>,
    forward: bool,
) -> Option<usize> {
    let order: Vec<usize> = (0..problems.len())
        .filter(|&i| problems[i].line.is_some())
        .collect();
    let n = order.len();
    if n == 0 {
        return None;
    }
    let after = |k: usize| order[(k + 1) % n];
    let before = |k: usize| order[(k + n - 1) % n];
    if let Some(k) = from.and_then(|i| order.iter().position(|&j| j == i)) {
        return Some(if forward { after(k) } else { before(k) });
    }
    let ends = || Some(if forward { order[0] } else { order[n - 1] });
    let Some((path, cursor_line)) = cursor else {
        return ends();
    };
    // The file's problems are one run of the list: after its last come the next files'.
    let here: Vec<usize> = (0..n)
        .filter(|&k| problems[order[k]].path == path)
        .collect();
    let (Some(&first), Some(&last)) = (here.first(), here.last()) else {
        return ends();
    };
    let found = if forward {
        here.iter()
            .find(|&&k| line(order[k]).is_some_and(|l| l > cursor_line))
    } else {
        here.iter()
            .rev()
            .find(|&&k| line(order[k]).is_some_and(|l| l < cursor_line))
    };
    Some(match found {
        Some(&k) => order[k],
        None if forward => after(last),
        None => before(first),
    })
}

/// The context menu of the problem row `index`: "Ask Agent to Fix".
fn context_menu(index: u32) -> gio::Menu {
    let menu = gio::Menu::new();
    let item = gio::MenuItem::new(Some("Ask Agent to _Fix"), None);
    item.set_action_and_target_value(Some("problems.ask-agent-fix"), Some(&index.to_variant()));
    menu.append_item(&item);
    menu
}

fn ensure_tag(buffer: &sourceview5::Buffer, name: &str, color: gtk::gdk::RGBA) -> gtk::TextTag {
    let table = buffer.tag_table();
    table.lookup(name).unwrap_or_else(|| {
        let tag = gtk::TextTag::builder()
            .name(name)
            .underline(pango::Underline::Error)
            .underline_rgba(&color)
            .build();
        table.add(&tag);
        tag
    })
}

/// The group header of a file: its path relative to the root's directory and the count.
fn file_header(path: &Path, root: &Path, count: usize) -> gtk::Widget {
    let root_dir = root.parent().unwrap_or(Path::new("/"));
    let name = path
        .strip_prefix(root_dir)
        .unwrap_or(path)
        .display()
        .to_string();
    let label = gtk::Label::builder()
        .label(&name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(pango::EllipsizeMode::Middle)
        .css_classes(["heading"])
        .build();
    let count = gtk::Label::builder()
        .label(count.to_string())
        .css_classes(["dim-label", "numeric", "caption"])
        .build();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header.add_css_class("lemma-problems-header");
    header.append(&label);
    header.append(&count);
    header.upcast()
}

fn problem_row(problem: &Problem) -> gtk::ListBoxRow {
    let (icon, class) = match problem.severity {
        Severity::Error => ("dialog-error-symbolic", "error"),
        Severity::Warning => ("dialog-warning-symbolic", "warning"),
        Severity::BadBox => ("format-justify-fill-symbolic", "dim-label"),
        Severity::Info => ("dialog-information-symbolic", "dim-label"),
    };
    let image = gtk::Image::builder()
        .icon_name(icon)
        .valign(gtk::Align::Start)
        .css_classes([class])
        .build();
    let message = gtk::Label::builder()
        .label(display_message(&problem.message))
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .hexpand(true)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .lines(3)
        .ellipsize(pango::EllipsizeMode::End)
        .build();
    let location = match (problem.line, problem.source) {
        (Some(line), _) => line.to_string(),
        (None, DiagSource::Biber) => "biber".to_owned(),
        (None, DiagSource::Bibtex) => "bibtex".to_owned(),
        (None, _) => String::new(),
    };
    let location = gtk::Label::builder()
        .label(&location)
        .valign(gtk::Align::Start)
        .xalign(1.0)
        .width_chars(4)
        .tooltip_text("Line")
        .css_classes(["dim-label", "numeric"])
        .build();
    // Centred as a whole in the row (single-line messages sit level with their icon); within
    // it everything aligns to the first line.
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_valign(gtk::Align::Center);
    content.append(&image);
    content.append(&message);
    content.append(&location);
    let row = gtk::ListBoxRow::builder().child(&content).build();
    let tooltip = match &problem.context {
        Some(context) => format!("{}\n\n{context}", problem.message),
        None => problem.message.clone(),
    };
    row.set_tooltip_text(Some(&tooltip));
    row
}

/// Opens `path` in an editor tab (or selects its tab) and, with a 1-based `line`, puts the
/// cursor at the start of that line's text and scrolls it into view.
pub async fn open_at_line(
    state: &Rc<WindowState>,
    path: &Path,
    line: Option<u32>,
) -> Result<Rc<Editor>, String> {
    let editor = state.open_file(path).await?;
    if let Some(line) = line {
        goto_line(&editor, line);
        scroll_again_after_first_frame(&editor);
    }
    Ok(editor)
}

/// Scrolls the cursor of `editor` into view again once its view has drawn: a view scrolled
/// before its first layout (a tab opened just now) can end up showing other lines.
fn scroll_again_after_first_frame(editor: &Rc<Editor>) {
    editor.after_first_frame(|editor| {
        let buffer = editor.buffer();
        editor
            .view()
            .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.3);
    });
}

/// Puts the cursor of `editor` at the first non-blank character of the 1-based `line`
/// (clamped to the last line), scrolls it into view and focuses the view.
pub fn goto_line(editor: &Editor, line: u32) {
    let buffer = editor.buffer();
    let last = buffer.line_count().saturating_sub(1);
    let line = i32::try_from(line.saturating_sub(1)).map_or(last, |l| l.min(last));
    let Some(mut iter) = buffer.iter_at_line(line) else {
        return;
    };
    while !iter.ends_line() && iter.char().is_whitespace() {
        iter.forward_char();
    }
    buffer.place_cursor(&iter);
    let view = editor.view();
    view.scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.3);
    view.grab_focus();
}

/// Shows the Problems page of the sidebar.
pub fn reveal(state: &WindowState) {
    state.sidebar.show_page(PAGE_PROBLEMS);
}

/// The default keys of next and previous problem (`[keys]` overrides them). F8, the usual key
/// for this, shows the PDF.
const ACCELS: &[(&str, &[&str])] = &[
    ("win.problem-next", &["F4"]),
    ("win.problem-prev", &["<Shift>F4"]),
];

/// Called once per window; fills `state.sidebar.problems` with the build controller's panel and
/// adds `win.problem-next` / `win.problem-prev` ([`DiagnosticsPanel::goto_problem`]).
pub fn install(state: &Rc<WindowState>) {
    if let Some(build) = state.build.get() {
        state
            .sidebar
            .problems
            .set_child(Some(&build.diagnostics().widget()));
    }
    for (name, forward) in [("problem-next", true), ("problem-prev", false)] {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade()
                && let Some(build) = state.build.get()
            {
                build.diagnostics().goto_problem(forward);
            }
        });
        state.window.add_action(&action);
    }
    crate::app::set_default_accels(state, ACCELS);
}

/// Called after an editor tab is created: applies the current diagnostics to its buffer, and
/// lints its file (M3, WP3.7: [`crate::lint::attach_editor`]).
pub fn attach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if let Some(build) = state.build.get() {
        build.diagnostics().attach_editor(editor);
    }
    crate::lint::attach_editor(state, editor);
}

/// Called after an editor tab is closed. Marks and tags live in the buffer, which goes with it.
/// The file's lint findings go too (M3, WP3.7: [`crate::lint::detach_editor`]).
pub fn detach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    crate::lint::detach_editor(state, editor);
}

// ===== M3 (WP3.7): findings, the filter, explanations and quick fixes =====

/// The width of a row's details toggle (`.lemma-problem-toggle` in [`CSS`]).
const TOGGLE_WIDTH: i32 = 22;

/// Toggle names of the filter bar (a section's toggle is [`filter_name`]).
const FILTER_ALL: &str = "all";
const FILTER_BUILD: &str = "build";

/// Most rows one section lists: a file with thousands of warnings must not make the page slow.
/// The section's count still shows them all.
pub const MAX_SECTION_ROWS: usize = 500;

/// What the Problems page lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemFilter {
    /// The build's problems and every section.
    All,
    /// The build's problems only.
    Build,
    /// One section of findings.
    Source(DiagSource),
}

/// A problem from another source than the build (M3): chktex warnings (`DiagSource::Lint`,
/// WP3.7), undefined references and citations (`DiagSource::Labels`, WP3.2). Each source has a
/// section of its own below the build's problems; [`DiagnosticsPanel::set_findings`] replaces
/// one file's findings at a time, and builds leave them alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// `problem.source` names the section; `path` is canonical and `line` 1-based, describing
    /// the text the findings were computed from (see [`DiagnosticsPanel::set_findings`]).
    pub problem: Problem,
    /// The byte range within the line that is underlined, for sources with a
    /// [`FindingStyle`]; `None` for findings about the whole file.
    pub columns: Option<Range<u32>>,
    /// A code shown with the finding (chktex's warning number).
    pub code: Option<u32>,
}

/// An action offered on a finding (a button in its row's details and an item of its context
/// menu). `run` gets the finding with its line in the current text.
#[derive(Clone)]
pub struct FindingAction {
    pub label: String,
    pub tooltip: Option<String>,
    pub run: Rc<FindingRunFn>,
}

/// What a [`FindingAction`] does.
pub type FindingRunFn = dyn Fn(&Rc<WindowState>, &Finding);

impl std::fmt::Debug for FindingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FindingAction")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

type FindingActionsFn = dyn Fn(&Finding) -> Vec<FindingAction>;

/// How a section's findings are underlined in the editors: a wavy underline in `color` (RGBA)
/// under the finding's columns, with the text tag `tag`. Sections without a style are not
/// underlined by the panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindingStyle {
    pub tag: &'static str,
    pub color: (f32, f32, f32, f32),
}

/// One section of findings: a heading and a list grouped by file.
struct Section {
    source: DiagSource,
    widget: gtk::Box,
    count: gtk::Label,
    list: gtk::ListBox,
    files: RefCell<BTreeMap<PathBuf, SectionFile>>,
    /// The row order: (file, index of the finding in that file).
    rows: Rc<RefCell<Vec<(PathBuf, usize)>>>,
    /// Findings per file, for the group headers.
    file_counts: Rc<RefCell<HashMap<PathBuf, usize>>>,
    style: Cell<Option<FindingStyle>>,
    actions: RefCell<Option<Rc<FindingActionsFn>>>,
    next_mark: Cell<u64>,
    /// `list` holds the rows of `rows` (they are built when the page is shown).
    rows_built: Cell<bool>,
}

/// One file's findings in a section.
struct SectionFile {
    findings: Vec<Finding>,
    /// Each finding's 1-based line in the current text (`None` when unknown).
    lines: Rc<RefCell<Vec<Option<u32>>>>,
    tracker: Option<LineTracker>,
    /// The name of each finding's start mark in the open buffer.
    marks: Vec<Option<String>>,
}

impl Section {
    fn total(&self) -> usize {
        self.files.borrow().values().map(|f| f.findings.len()).sum()
    }
}

/// The details under a build row: its explanation, revealed by a toggle.
#[derive(Clone)]
struct RowDetails {
    toggle: gtk::ToggleButton,
    /// The label of the quick fix, once [`crate::quickfix::proposal`] found one (the details
    /// were shown).
    fix: Rc<RefCell<Option<String>>>,
}

/// The toggle name of `source`'s section.
fn filter_name(source: DiagSource) -> String {
    format!("source-{source:?}").to_lowercase()
}

/// The filter label, the heading, and the singular and plural nouns of `source`'s section.
fn section_names(source: DiagSource) -> (&'static str, &'static str, &'static str, &'static str) {
    match source {
        DiagSource::Lint => ("Lint", "Lint (chktex)", "lint warning", "lint warnings"),
        DiagSource::Labels => (
            "References",
            "References",
            "unresolved reference",
            "unresolved references",
        ),
        _ => ("Other", "Other", "problem", "problems"),
    }
}

impl DiagnosticsPanel {
    /// Replaces the findings of `source` in the file `path` with `findings` (all of that
    /// source and file; empty removes them). Their lines describe the text the findings were
    /// computed from: `journal` (started when that text was taken) holds the edits made since,
    /// and the lines follow them and every later edit; without a journal they describe the
    /// current text.
    pub fn set_findings(
        &self,
        source: DiagSource,
        path: &Path,
        findings: Vec<Finding>,
        journal: Option<EditJournal>,
    ) {
        self.inner.set_findings(source, path, findings, journal);
    }

    /// Removes every finding of `source`.
    pub fn clear_findings(&self, source: DiagSource) {
        let Some(section) = self.inner.section(source) else {
            return;
        };
        let paths: Vec<PathBuf> = section.files.borrow().keys().cloned().collect();
        for path in paths {
            self.inner.set_findings(source, &path, Vec::new(), None);
        }
    }

    /// The findings of `source` in row order, each with its line in the current text.
    pub fn findings(&self, source: DiagSource) -> Vec<Finding> {
        let Some(section) = self.inner.section(source) else {
            return Vec::new();
        };
        let rows = section.rows.borrow().clone();
        rows.iter()
            .filter_map(|(path, i)| self.inner.current_finding(&section, path, *i))
            .collect()
    }

    /// The files that have findings of `source`.
    pub fn finding_files(&self, source: DiagSource) -> Vec<PathBuf> {
        self.inner
            .section(source)
            .map(|s| s.files.borrow().keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Underlines the findings of `source` in the editors with `style`.
    pub fn set_finding_style(&self, source: DiagSource, style: FindingStyle) {
        let section = self.inner.section_or_create(source);
        section.style.set(Some(style));
    }

    /// Offers the actions `actions(finding)` on every finding of `source`.
    pub fn set_finding_actions(
        &self,
        source: DiagSource,
        actions: impl Fn(&Finding) -> Vec<FindingAction> + 'static,
    ) {
        let section = self.inner.section_or_create(source);
        section.actions.replace(Some(Rc::new(actions)));
    }

    /// The actions offered on finding `index` (row order) of `source`.
    pub fn finding_actions(&self, source: DiagSource, index: usize) -> Vec<FindingAction> {
        let Some(section) = self.inner.section(source) else {
            return Vec::new();
        };
        let finding = section
            .rows
            .borrow()
            .get(index)
            .cloned()
            .and_then(|(path, i)| self.inner.current_finding(&section, &path, i));
        let actions = section.actions.borrow().clone();
        match (finding, actions) {
            (Some(f), Some(actions)) => actions(&f),
            _ => Vec::new(),
        }
    }

    /// Runs action `action` of finding `index` of `source` (as its button does).
    pub fn run_finding_action(&self, source: DiagSource, index: usize, action: usize) {
        self.inner.run_finding_action(source, index, action);
    }

    /// Activates the row of finding `index` of `source` (as a click does): opens its file at
    /// its line and column.
    pub fn activate_finding(&self, source: DiagSource, index: usize) {
        let Some(section) = self.inner.section(source) else {
            return;
        };
        self.inner.build_section_rows(&section);
        if let Ok(index) = i32::try_from(index)
            && let Some(row) = section.list.row_at_index(index)
        {
            row.emit_activate();
        }
    }

    /// The context menu of finding `index` of `source`: its actions, then "Ask Agent to Fix".
    pub fn finding_context_menu(&self, source: DiagSource, index: usize) -> Option<gio::Menu> {
        let section = self.inner.section(source)?;
        (index < section.rows.borrow().len()).then(|| self.inner.finding_menu(&section, index))
    }

    /// The finding under byte `offset` of `editor`'s document (one with columns, in a
    /// section with a [`FindingStyle`]), with its current byte range.
    pub fn finding_at(&self, editor: &Editor, offset: usize) -> Option<(Finding, Range<usize>)> {
        self.inner.finding_at(editor, offset)
    }

    /// What the page lists.
    pub fn filter(&self) -> ProblemFilter {
        self.inner.current_filter()
    }

    /// Lists only `filter`'s problems (as clicking its toggle does).
    pub fn set_filter(&self, filter: ProblemFilter) {
        let name = match filter {
            ProblemFilter::All => FILTER_ALL.to_owned(),
            ProblemFilter::Build => FILTER_BUILD.to_owned(),
            ProblemFilter::Source(source) => filter_name(source),
        };
        if self.inner.filter.toggle_by_name(&name).is_some() {
            self.inner.filter.set_active_name(Some(&name));
        }
    }

    /// True while the filter bar is shown (some section has findings).
    pub fn shows_filter_bar(&self) -> bool {
        self.inner.filter_bar.is_visible()
    }

    /// The summary line above the list.
    pub fn summary_text(&self) -> String {
        self.inner.summary.text().to_string()
    }

    /// The plain-English explanation of build problem `index`, if the table knows its error.
    pub fn explanation(&self, index: usize) -> Option<String> {
        let problems = self.inner.problems.borrow();
        crate::quickfix::explanation(problems.get(index)?).map(|e| e.text)
    }

    /// The label of the quick fix offered for build problem `index`, once it is known (its row
    /// computes it when its details are first shown; rows exist once the page showed the list).
    pub fn fix_label(&self, index: usize) -> Option<String> {
        self.inner
            .details
            .borrow()
            .get(&index)
            .and_then(|d| d.fix.borrow().clone())
    }

    /// Applies the quick fix of build problem `index` (its button; reviewable hunks).
    pub fn apply_fix(&self, index: usize) {
        self.inner.apply_fix(index);
    }

    /// Whether the details (explanation and actions) of build problem `index` are shown (false
    /// while its row does not exist).
    pub fn is_expanded(&self, index: usize) -> bool {
        self.inner
            .details
            .borrow()
            .get(&index)
            .is_some_and(|d| d.toggle.is_active())
    }

    /// Shows or hides the details of build problem `index`, if the list holds its row.
    pub fn set_expanded(&self, index: usize, expanded: bool) {
        if let Some(d) = self.inner.details.borrow().get(&index) {
            d.toggle.set_active(expanded);
        }
    }
}

impl Inner {
    fn section(&self, source: DiagSource) -> Option<Rc<Section>> {
        self.sections
            .borrow()
            .iter()
            .find(|s| s.source == source)
            .cloned()
    }

    fn current_filter(&self) -> ProblemFilter {
        let name = self.filter.active_name();
        match name.as_deref() {
            None | Some(FILTER_ALL) => ProblemFilter::All,
            Some(FILTER_BUILD) => ProblemFilter::Build,
            Some(other) => self
                .sections
                .borrow()
                .iter()
                .find(|s| filter_name(s.source) == other)
                .map_or(ProblemFilter::All, |s| ProblemFilter::Source(s.source)),
        }
    }

    /// The filter bar follows its toggles.
    fn connect_findings(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.filter.connect_active_name_notify(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.refresh_page();
            }
        });
        // The findings' rows are built when the page first shows them (see `fill_section`).
        let weak = Rc::downgrade(self);
        self.sections_box.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                let sections = inner.sections.borrow().clone();
                for section in &sections {
                    inner.build_section_rows(section);
                }
            }
        });
    }

    /// The section of `source`, created (empty, hidden) on first use.
    fn section_or_create(self: &Rc<Self>, source: DiagSource) -> Rc<Section> {
        if let Some(section) = self.section(source) {
            return section;
        }
        let (_, heading, _, _) = section_names(source);
        let title = gtk::Label::builder()
            .label(heading)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let count = gtk::Label::builder()
            .css_classes(["dim-label", "numeric", "caption"])
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("lemma-problems-section");
        header.append(&title);
        header.append(&count);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .css_classes(["navigation-sidebar", "lemma-problems"])
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_visible(false);
        widget.append(&header);
        widget.append(&list);
        self.sections_box.append(&widget);
        let section = Rc::new(Section {
            source,
            widget,
            count,
            list,
            files: RefCell::new(BTreeMap::new()),
            rows: Rc::new(RefCell::new(Vec::new())),
            file_counts: Rc::new(RefCell::new(HashMap::new())),
            style: Cell::new(None),
            actions: RefCell::new(None),
            next_mark: Cell::new(0),
            rows_built: Cell::new(false),
        });
        // Group headers: the file, relative to the project root.
        let rows = section.rows.clone();
        let counts = section.file_counts.clone();
        let state = self.state.clone();
        section.list.set_header_func(move |row, before| {
            let rows = rows.borrow();
            let path_of = |r: &gtk::ListBoxRow| {
                usize::try_from(r.index())
                    .ok()
                    .and_then(|i| rows.get(i))
                    .map(|(p, _)| p.clone())
            };
            let Some(current) = path_of(row) else {
                row.set_header(None::<&gtk::Widget>);
                return;
            };
            if before.and_then(path_of).as_ref() == Some(&current) {
                row.set_header(None::<&gtk::Widget>);
                return;
            }
            let count = counts.borrow().get(&current).copied().unwrap_or(0);
            // `file_header` names files relative to the root document's directory.
            let anchor = state
                .upgrade()
                .map(|s| s.workspace.root().path().join("root"))
                .unwrap_or_default();
            row.set_header(Some(&file_header(&current, &anchor, count)));
        });
        let weak = Rc::downgrade(self);
        let source_of = source;
        section.list.connect_row_activated(move |_, row| {
            if let (Some(inner), Ok(index)) = (weak.upgrade(), usize::try_from(row.index())) {
                inner.open_finding(source_of, index);
            }
        });
        self.connect_finding_menu(&section);
        // Its filter toggle comes with its first finding (`refresh_page`).
        self.sections.borrow_mut().push(section.clone());
        section
    }

    fn set_findings(
        self: &Rc<Self>,
        source: DiagSource,
        path: &Path,
        findings: Vec<Finding>,
        journal: Option<EditJournal>,
    ) {
        let path = normalize(path);
        let section = self.section_or_create(source);
        let editor = self.state.upgrade().and_then(|state| {
            state
                .editors()
                .into_iter()
                .find(|e| normalize(e.document().path()) == path)
        });
        // The old findings' marks go with them.
        if let Some(old) = section.files.borrow_mut().remove(&path)
            && let Some(editor) = &editor
        {
            remove_marks(editor, &old.marks);
        }
        if !findings.is_empty() {
            let lines = Rc::new(RefCell::new(
                findings.iter().map(|f| f.problem.line).collect::<Vec<_>>(),
            ));
            let doc = self.state.upgrade().and_then(|s| s.workspace.get(&path));
            let mut tracker = None;
            if let Some(recorded) = journal.as_ref().and_then(|j| j.docs.get(&path)) {
                let changes = recorded.changes.borrow();
                for line in lines.borrow_mut().iter_mut() {
                    *line = line.map(|l| changes.iter().fold(l, |l, &c| moved_line(l, c)));
                }
            }
            if let Some(doc) = &doc {
                let recorded = journal
                    .as_ref()
                    .and_then(|j| j.docs.get(&path))
                    .and_then(|r| r.doc.upgrade());
                let followed = match &journal {
                    None => true,
                    Some(_) => recorded.is_some_and(|d| Rc::ptr_eq(&d, doc)) || doc.version() == 0,
                };
                if followed {
                    tracker = Some(track_lines(doc, &lines));
                } else {
                    lines.borrow_mut().iter_mut().for_each(|l| *l = None);
                }
            }
            let marks = vec![None; findings.len()];
            section.files.borrow_mut().insert(
                path.clone(),
                SectionFile {
                    findings,
                    lines,
                    tracker,
                    marks,
                },
            );
        }
        drop(journal);
        self.fill_section(&section);
        if let Some(editor) = &editor {
            self.apply_findings(editor);
        }
        self.refresh_page();
    }

    /// Follows the line changes of `doc` (a document an editor now shows) for the findings in
    /// its file, unless that already happens.
    fn follow_findings(&self, doc: &DocRef) {
        let path = normalize(doc.path());
        for section in self.sections.borrow().iter() {
            let mut files = section.files.borrow_mut();
            let Some(file) = files.get_mut(&path) else {
                continue;
            };
            let followed = file
                .tracker
                .as_ref()
                .and_then(|t| t.doc.upgrade())
                .is_some_and(|d| Rc::ptr_eq(&d, doc));
            if followed {
                continue;
            }
            if doc.version() == 0 {
                file.tracker = Some(track_lines(doc, &file.lines));
            } else {
                file.lines.borrow_mut().iter_mut().for_each(|l| *l = None);
                file.tracker = None;
            }
        }
    }

    /// Rebuilds the rows of `section`: files in path order (the root document first), each
    /// file's findings by line and column.
    fn fill_section(self: &Rc<Self>, section: &Rc<Section>) {
        section.rows.borrow_mut().clear();
        section.list.remove_all();
        let root = self.root.borrow().clone();
        let files = section.files.borrow();
        let mut paths: Vec<&PathBuf> = files.keys().collect();
        paths.sort_by_key(|p| (**p != root, (*p).clone()));
        let mut rows = Vec::new();
        let mut counts = HashMap::new();
        for path in paths {
            let file = &files[path];
            counts.insert(path.clone(), file.findings.len());
            let mut order: Vec<usize> = (0..file.findings.len()).collect();
            order.sort_by_key(|&i| {
                let f = &file.findings[i];
                (
                    f.problem.line,
                    f.columns.as_ref().map(|c| c.start),
                    severity_rank(f.problem.severity),
                )
            });
            rows.extend(order.into_iter().map(|i| (path.clone(), i)));
        }
        rows.truncate(MAX_SECTION_ROWS);
        section.file_counts.replace(counts);
        section.rows.replace(rows);
        section.rows_built.set(false);
        drop(files);
        // The rows are widgets (several per finding): they are built when the page shows them,
        // so the findings of a page nobody looks at (lint warnings of a file just opened) cost
        // no widgets.
        if self.sections_box.is_mapped() {
            self.build_section_rows(section);
        }
        let total = section.total();
        section.count.set_text(&total.to_string());
    }

    /// Fills the list of `section` with the rows of its findings, unless it holds them.
    fn build_section_rows(self: &Rc<Self>, section: &Rc<Section>) {
        if section.rows_built.replace(true) {
            return;
        }
        let rows = section.rows.borrow().clone();
        let files = section.files.borrow();
        for (index, (path, i)) in rows.iter().enumerate() {
            if let Some(finding) = files.get(path).and_then(|f| f.findings.get(*i)) {
                section
                    .list
                    .append(&self.finding_row(section, index, finding));
            }
        }
    }

    /// The row of `finding` (row `index` of `section`).
    fn finding_row(
        self: &Rc<Self>,
        section: &Rc<Section>,
        index: usize,
        finding: &Finding,
    ) -> gtk::ListBoxRow {
        let mut problem = finding.problem.clone();
        // A code (chktex's number) is what the tooltip shows after the message.
        if let Some(code) = finding.code {
            problem.context = Some(match section.source {
                DiagSource::Lint => lemma_build::chktex::code_label(code),
                _ => code.to_string(),
            });
        }
        let row = problem_row(&problem);
        if section.source == DiagSource::Lint {
            // Lint is advice, not a failure: the information icon in the accent colour.
            if let Some(image) = first_image(&row) {
                image.set_icon_name(Some("dialog-information-symbolic"));
                image.set_css_classes(&["accent"]);
            }
        }
        let caption = match (section.source, finding.code) {
            (DiagSource::Lint, Some(c)) => Some(format!(
                "chktex warning {c}. Disable it on this line with a “% chktex {c}” comment, or \
                 everywhere."
            )),
            _ => finding.problem.context.clone(),
        };
        let weak = Rc::downgrade(self);
        let source = section.source;
        let finding = finding.clone();
        // Built when first shown: a long list of warnings costs only its rows.
        add_details(&row, false, move || {
            let (details, actions) = details_box(caption.as_deref());
            let Some(inner) = weak.upgrade() else {
                return details;
            };
            let offered = inner
                .section(source)
                .and_then(|section| section.actions.borrow().clone());
            if let Some(offered) = offered {
                for (i, action) in offered(&finding).into_iter().enumerate() {
                    let button = gtk::Button::builder()
                        .label(&action.label)
                        .css_classes(["flat"])
                        .build();
                    button.set_tooltip_text(action.tooltip.as_deref());
                    let weak = Rc::downgrade(&inner);
                    button.connect_clicked(move |_| {
                        if let Some(inner) = weak.upgrade() {
                            inner.run_finding_action(source, index, i);
                        }
                    });
                    actions.append(&button);
                }
            }
            let ask = ask_agent_button();
            let weak = Rc::downgrade(&inner);
            ask.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.ask_agent_about_finding(source, index);
                }
            });
            actions.append(&ask);
            details
        });
        row
    }

    /// Finding `i` of `path` in `section`, with its line in the current text: its mark's line
    /// in an open editor, else its followed line.
    fn current_finding(&self, section: &Section, path: &Path, i: usize) -> Option<Finding> {
        let files = section.files.borrow();
        let file = files.get(path)?;
        let mut finding = file.findings.get(i)?.clone();
        let marked = file.marks.get(i).cloned().flatten().and_then(|name| {
            let state = self.state.upgrade()?;
            let editor = state
                .editors()
                .into_iter()
                .find(|e| normalize(e.document().path()) == path)?;
            let buffer = editor.buffer();
            let mark = buffer.mark(&name).filter(|m| !m.is_deleted())?;
            let iter = buffer.iter_at_mark(&mark);
            let doc = editor.document();
            let lines_match =
                u32::try_from(buffer.line_count()).ok() == Some(doc.line_index().line_count());
            if lines_match {
                let line = u32::try_from(iter.line()).ok()?;
                let column = u32::try_from(iter.line_index()).ok()?;
                return Some((line + 1, column));
            }
            let offset = doc.byte_offset(&iter);
            let (line, column) = doc.line_index().position(offset);
            Some((line + 1, column))
        });
        match marked {
            Some((line, column)) => {
                finding.problem.line = Some(line);
                if let Some(columns) = &mut finding.columns {
                    let len = columns.end - columns.start;
                    *columns = column..column + len;
                }
            }
            None => finding.problem.line = file.lines.borrow().get(i).copied().flatten(),
        }
        Some(finding)
    }

    fn finding_by_row(&self, source: DiagSource, index: usize) -> Option<Finding> {
        let section = self.section(source)?;
        let (path, i) = section.rows.borrow().get(index).cloned()?;
        self.current_finding(&section, &path, i)
    }

    /// Opens finding `index` of `source` at its line and column.
    fn open_finding(&self, source: DiagSource, index: usize) {
        let (Some(state), Some(finding)) =
            (self.state.upgrade(), self.finding_by_row(source, index))
        else {
            return;
        };
        state.sidebar.dismiss_overlay();
        glib::spawn_future_local(async move {
            match state.open_file(&finding.problem.path).await {
                Ok(editor) => {
                    let Some(line) = finding.problem.line else {
                        return;
                    };
                    goto_line(&editor, line);
                    if let Some(columns) = &finding.columns {
                        let doc = editor.document();
                        let start = doc.line_index().line_start(line - 1);
                        if let Some(iter) = doc.iter_at_byte(start + columns.start as usize) {
                            editor.buffer().place_cursor(&iter);
                        }
                    }
                    scroll_again_after_first_frame(&editor);
                }
                Err(e) => state.toast(&e),
            }
        });
    }

    fn run_finding_action(&self, source: DiagSource, index: usize, action: usize) {
        let (Some(state), Some(section)) = (self.state.upgrade(), self.section(source)) else {
            return;
        };
        let Some(finding) = self.finding_by_row(source, index) else {
            return;
        };
        let actions = section.actions.borrow().clone();
        if let Some(action) = actions.and_then(|a| a(&finding).into_iter().nth(action)) {
            (action.run)(&state, &finding);
        }
    }

    fn ask_agent_about_finding(&self, source: DiagSource, index: usize) {
        if let (Some(state), Some(finding)) =
            (self.state.upgrade(), self.finding_by_row(source, index))
        {
            let mut problem = finding.problem;
            if let (DiagSource::Lint, Some(code)) = (source, finding.code) {
                problem.message = format!("chktex warning {code}: {}", problem.message);
            }
            crate::agent::quick::fix_diagnostic(&state, &problem);
        }
    }

    /// The context menu of finding row `index`: the section's actions, then "Ask Agent to Fix"
    /// (actions `finding.run((uu))` and `finding.ask-agent(u)` on the section).
    fn finding_menu(&self, section: &Section, index: usize) -> gio::Menu {
        let menu = gio::Menu::new();
        let row = u32::try_from(index).unwrap_or(u32::MAX);
        let finding = self.finding_by_row(section.source, index);
        let actions = section.actions.borrow().clone();
        if let (Some(finding), Some(actions)) = (finding, actions) {
            for (i, action) in actions(&finding).into_iter().enumerate() {
                let item = gio::MenuItem::new(Some(&action.label), None);
                item.set_action_and_target_value(
                    Some("finding.run"),
                    Some(&(row, u32::try_from(i).unwrap_or(u32::MAX)).to_variant()),
                );
                menu.append_item(&item);
            }
        }
        let item = gio::MenuItem::new(Some("Ask Agent to _Fix"), None);
        item.set_action_and_target_value(Some("finding.ask-agent"), Some(&row.to_variant()));
        menu.append_item(&item);
        menu
    }

    /// The section's context menu: right click, long press, Menu or Shift+F10 on a row.
    fn connect_finding_menu(self: &Rc<Self>, section: &Rc<Section>) {
        let group = gio::SimpleActionGroup::new();
        let run = gio::SimpleAction::new("run", Some(glib::VariantTy::new("(uu)").unwrap()));
        let weak = Rc::downgrade(self);
        let source = section.source;
        run.connect_activate(move |_, target| {
            if let (Some(inner), Some((row, action))) =
                (weak.upgrade(), target.and_then(|v| v.get::<(u32, u32)>()))
            {
                inner.run_finding_action(source, row as usize, action as usize);
            }
        });
        group.add_action(&run);
        let ask = gio::SimpleAction::new("ask-agent", Some(glib::VariantTy::UINT32));
        let weak = Rc::downgrade(self);
        ask.connect_activate(move |_, target| {
            if let (Some(inner), Some(row)) = (weak.upgrade(), target.and_then(|v| v.get::<u32>()))
            {
                inner.ask_agent_about_finding(source, row as usize);
            }
        });
        group.add_action(&ask);
        section.widget.insert_action_group("finding", Some(&group));

        let show = {
            let weak = Rc::downgrade(self);
            let section = Rc::downgrade(section);
            move |row: &gtk::ListBoxRow, at: Option<(f64, f64)>| {
                let (Some(inner), Some(section)) = (weak.upgrade(), section.upgrade()) else {
                    return;
                };
                let Ok(index) = usize::try_from(row.index()) else {
                    return;
                };
                let menu = inner.finding_menu(&section, index);
                popup_menu(&menu, &section.widget, &section.list, row, at);
            }
        };
        let click = gtk::GestureClick::builder().button(3).build();
        let list = section.list.clone();
        let on_click = show.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(row) = list.row_at_y(y as i32) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                on_click(&row, Some((x, y)));
            }
        });
        section.list.add_controller(click);
        let press = gtk::GestureLongPress::new();
        let list = section.list.clone();
        let on_press = show.clone();
        press.connect_pressed(move |_, x, y| {
            if let Some(row) = list.row_at_y(y as i32) {
                on_press(&row, Some((x, y)));
            }
        });
        section.list.add_controller(press);
        let keys = gtk::ShortcutController::new();
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Menu|<Shift>F10"),
            Some(gtk::CallbackAction::new(move |list, _| {
                let focused = list
                    .root()
                    .and_then(|root| root.focus())
                    .and_then(|w| w.ancestor(gtk::ListBoxRow::static_type()))
                    .and_then(|w| w.downcast::<gtk::ListBoxRow>().ok());
                match focused {
                    Some(row) => {
                        show(&row, None);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            })),
        ));
        section.list.add_controller(keys);
    }

    /// Underlines the findings of `editor`'s file (sections with a style) and puts a mark at
    /// the start of each, so that they follow edits.
    fn apply_findings(&self, editor: &Editor) {
        let path = normalize(editor.document().path());
        let buffer = editor.buffer();
        let doc = editor.document();
        for section in self.sections.borrow().iter() {
            let Some(style) = section.style.get() else {
                continue;
            };
            let table = buffer.tag_table();
            let mut files = section.files.borrow_mut();
            let file = files.get_mut(&path);
            let tag = match table.lookup(style.tag) {
                Some(tag) => tag,
                None if file.is_none() => continue,
                None => {
                    let (r, g, b, a) = style.color;
                    let tag = gtk::TextTag::builder()
                        .name(style.tag)
                        .underline(pango::Underline::Error)
                        .underline_rgba(&gtk::gdk::RGBA::new(r, g, b, a))
                        .build();
                    table.add(&tag);
                    tag
                }
            };
            let (start, end) = buffer.bounds();
            buffer.remove_tag(&tag, &start, &end);
            let Some(file) = file else {
                continue;
            };
            remove_marks(editor, &file.marks);
            let lines = file.lines.borrow();
            let index = doc.line_index();
            let line_count = index.line_count();
            let mut placed = Vec::with_capacity(file.findings.len());
            for (i, finding) in file.findings.iter().enumerate() {
                let (Some(line), Some(columns)) =
                    (lines.get(i).copied().flatten(), finding.columns.as_ref())
                else {
                    placed.push((i, None));
                    continue;
                };
                if line == 0 || line > line_count {
                    placed.push((i, None));
                    continue;
                }
                let line_start = index.line_start(line - 1);
                let line_end = if line < line_count {
                    index.line_start(line) - 1
                } else {
                    index.text_len()
                };
                let from = (line_start + columns.start as usize).min(line_end);
                let to = (line_start + columns.end as usize).clamp(from, line_end);
                placed.push((i, Some((from, to))));
            }
            drop(index);
            drop(lines);
            let offsets: Vec<usize> = placed
                .iter()
                .filter_map(|(_, r)| *r)
                .flat_map(|(a, b)| [a, b])
                .collect();
            let iters = iters_at_bytes(doc, buffer, &offsets);
            for (i, range) in placed {
                let Some((from, to)) = range else {
                    continue;
                };
                let (Some(a), Some(b)) = (iters.get(&from), iters.get(&to)) else {
                    continue;
                };
                buffer.apply_tag(&tag, a, b);
                let id = section.next_mark.get();
                section.next_mark.set(id + 1);
                let name = format!("lemma-finding-{id}");
                buffer.create_mark(Some(&name), a, false);
                file.marks[i] = Some(name);
            }
        }
    }

    fn finding_at(&self, editor: &Editor, offset: usize) -> Option<(Finding, Range<usize>)> {
        let path = normalize(editor.document().path());
        let buffer = editor.buffer();
        let doc = editor.document();
        let mut at: Option<gtk::TextIter> = None;
        for section in self.sections.borrow().iter() {
            let Some(style) = section.style.get() else {
                continue;
            };
            let Some(tag) = buffer.tag_table().lookup(style.tag) else {
                continue;
            };
            if at.is_none() {
                at = Some(doc.iter_at_byte(offset)?);
            }
            let iter = at.as_ref()?;
            // Most of the text has no finding: one tag lookup answers.
            if !iter.has_tag(&tag) {
                continue;
            }
            let mut start = *iter;
            if !start.starts_tag(Some(&tag)) {
                start.backward_to_tag_toggle(Some(&tag));
            }
            let mut end = *iter;
            end.forward_to_tag_toggle(Some(&tag));
            let index = {
                let files = section.files.borrow();
                let file = files.get(&path)?;
                // The finding whose mark starts this underline (adjacent findings can share
                // one tag run: the last mark at or before the offset).
                file.marks
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| {
                        let mark = buffer.mark(m.as_deref()?).filter(|m| !m.is_deleted())?;
                        let it = buffer.iter_at_mark(&mark);
                        (it.offset() >= start.offset() && it.offset() <= iter.offset())
                            .then_some((it.offset(), i))
                    })
                    .max()
                    .map(|(_, i)| i)
            };
            let Some(i) = index else {
                continue;
            };
            let finding = self.current_finding(section, &path, i)?;
            let range = doc.byte_offset(&start)..doc.byte_offset(&end);
            return Some((finding, range));
        }
        None
    }

    /// The build row of problem `index`: [`problem_row`] plus, for errors the explanation table
    /// knows, the explanation (tooltip and details), "Ask Agent" and the quick fix once
    /// [`crate::quickfix::proposal`] found one.
    fn build_row(self: &Rc<Self>, index: usize, problem: &Problem) -> gtk::ListBoxRow {
        let row = problem_row(problem);
        let Some(explanation) = crate::quickfix::explanation(problem) else {
            // As wide as the details toggle of the other rows, so the line numbers align.
            if let Some(content) = row.child().and_then(|c| c.downcast::<gtk::Box>().ok()) {
                content.append(&gtk::Box::builder().width_request(TOGGLE_WIDTH).build());
            }
            return row;
        };
        let tooltip = row
            .tooltip_text()
            .map(|t| t.to_string())
            .unwrap_or_default();
        row.set_tooltip_text(Some(&format!("{tooltip}\n\n{}", explanation.text)));
        // The first error of each kind shows its explanation; repeats and warnings start
        // collapsed (and compute their fix when first opened).
        let expanded = problem.severity == Severity::Error
            && self.expanded_kinds.borrow_mut().insert(explanation.kind);
        let fix: Rc<RefCell<Option<String>>> = Rc::default();
        let weak = Rc::downgrade(self);
        let text = explanation.text.clone();
        let has_fix = explanation.fix.is_some();
        let problem = problem.clone();
        let ready = fix.clone();
        let toggle = add_details(&row, expanded, move || {
            let (details, actions) = details_box(Some(&text));
            let Some(inner) = weak.upgrade() else {
                return details;
            };
            let ask = ask_agent_button();
            let weak = Rc::downgrade(&inner);
            ask.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.ask_agent(index);
                }
            });
            actions.append(&ask);
            if has_fix && let Some(state) = inner.state.upgrade() {
                // With its line in the current text, while the list still shows it.
                let current = inner
                    .problem_for_agent(index)
                    .filter(|p| p.message == problem.message)
                    .unwrap_or_else(|| problem.clone());
                let proposal = crate::quickfix::proposal(&state, &current);
                let actions = actions.downgrade();
                let ready = ready.clone();
                let weak = Rc::downgrade(&inner);
                glib::spawn_future_local(async move {
                    let Some(proposal) = proposal.await else {
                        return;
                    };
                    let (Some(actions), Some(_)) = (actions.upgrade(), weak.upgrade()) else {
                        return;
                    };
                    let label = gtk::Label::builder()
                        .label(&proposal.label)
                        .ellipsize(pango::EllipsizeMode::End)
                        .max_width_chars(30)
                        .build();
                    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                    content.append(&gtk::Image::from_icon_name("error-correct-symbolic"));
                    content.append(&label);
                    let button = gtk::Button::builder()
                        .child(&content)
                        .tooltip_text(&proposal.description)
                        .css_classes(["suggested-action"])
                        .build();
                    button.connect_clicked(move |_| {
                        if let Some(inner) = weak.upgrade() {
                            inner.apply_fix(index);
                        }
                    });
                    actions.prepend(&button);
                    ready.replace(Some(proposal.label));
                });
            }
            details
        });
        self.details
            .borrow_mut()
            .insert(index, RowDetails { toggle, fix });
        row
    }

    fn apply_fix(&self, index: usize) {
        let (Some(state), Some(problem)) = (self.state.upgrade(), self.problem_for_agent(index))
        else {
            return;
        };
        glib::spawn_future_local(async move {
            if let Err(e) = crate::quickfix::apply(&state, &problem).await {
                state.toast(&e);
            }
        });
    }

    /// Adds "Apply Fix: …" to the context menu of build row `index` when it has a fix.
    fn add_fix_item(&self, menu: &gio::Menu, index: u32) {
        let label = self
            .details
            .borrow()
            .get(&(index as usize))
            .and_then(|d| d.fix.borrow().clone());
        if let Some(label) = label {
            let item = gio::MenuItem::new(Some(&format!("Apply Fix: {label}")), None);
            item.set_action_and_target_value(Some("problems.apply-fix"), Some(&index.to_variant()));
            menu.append_item(&item);
        }
    }

    /// Shows what the filter selects and updates the summary, the filter bar and the page.
    fn refresh_page(&self) {
        // Adding and removing toggles notifies the filter, which calls this again.
        if self.refreshing.replace(true) {
            return;
        }
        self.refresh_page_now();
        self.refreshing.set(false);
    }

    fn refresh_page_now(&self) {
        let problems = self.problems.borrow();
        let sections = self.sections.borrow();
        // A section without findings has no toggle.
        for section in sections.iter() {
            let name = filter_name(section.source);
            let has = section.total() > 0;
            let toggle = self.filter.toggle_by_name(&name);
            match (has, toggle) {
                (false, Some(toggle)) => {
                    if self.filter.active_name().as_deref() == Some(name.as_str()) {
                        self.filter.set_active_name(Some(FILTER_ALL));
                    }
                    self.filter.remove(&toggle);
                }
                (true, None) => {
                    let (label, _, _, many) = section_names(section.source);
                    self.filter.add(
                        adw::Toggle::builder()
                            .name(&name)
                            .label(label)
                            .tooltip(format!("Only {many}"))
                            .build(),
                    );
                }
                _ => {}
            }
        }
        let any_findings = sections.iter().any(|s| s.total() > 0);
        self.filter_bar.set_visible(any_findings);
        let filter = self.current_filter();
        let shows_build = matches!(filter, ProblemFilter::All | ProblemFilter::Build);
        self.list.set_visible(shows_build && !problems.is_empty());
        self.build_empty
            .set_visible(any_findings && problems.is_empty() && filter == ProblemFilter::Build);
        self.build_empty.set_text(if self.built.get() {
            "The last build finished without errors or warnings."
        } else {
            "Not built yet."
        });
        let mut parts = Vec::new();
        if shows_build {
            let (errors, warnings, bad_boxes) = counts(&problems);
            let build = summary(errors, warnings, bad_boxes);
            if !build.is_empty() {
                parts.push(build);
            }
        }
        for section in sections.iter() {
            let total = section.total();
            let shown = total > 0
                && (filter == ProblemFilter::All
                    || filter == ProblemFilter::Source(section.source));
            section.widget.set_visible(shown);
            if shown {
                let (_, _, one, many) = section_names(section.source);
                parts.push(format!("{total} {}", if total == 1 { one } else { many }));
            }
        }
        self.summary.set_text(&parts.join(" · "));
        self.summary.set_visible(!parts.is_empty());
        let page = if !problems.is_empty() || any_findings {
            "list"
        } else if self.built.get() {
            "clean"
        } else {
            "none"
        };
        self.stack.set_visible_child_name(page);
    }
}

/// The buffer iters at the byte `offsets` of `doc`'s open buffer (offsets inside a character or
/// past the end are left out). Normally each is O(1); while GTK counts more lines than the
/// document (a lone `\r` or U+2029), where every `Document::iter_at_byte` copies the text,
/// they are found in one pass over one copy.
fn iters_at_bytes(
    doc: &Document,
    buffer: &sourceview5::Buffer,
    offsets: &[usize],
) -> HashMap<usize, gtk::TextIter> {
    let lines_match =
        u32::try_from(buffer.line_count()).ok() == Some(doc.line_index().line_count());
    if lines_match {
        return offsets
            .iter()
            .filter_map(|&o| doc.iter_at_byte(o).map(|it| (o, it)))
            .collect();
    }
    let mut sorted: Vec<usize> = offsets.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let text = crate::document::buffer_text(buffer);
    let mut out = HashMap::new();
    let (mut byte, mut chars) = (0usize, 0i32);
    for target in sorted {
        if target > text.len() || !text.is_char_boundary(target) {
            continue;
        }
        chars += i32::try_from(text[byte..target].chars().count()).unwrap_or(i32::MAX);
        byte = target;
        out.insert(target, buffer.iter_at_offset(chars));
    }
    out
}

/// An observer that moves `lines` with every line change of `doc`.
fn track_lines(doc: &DocRef, lines: &Rc<RefCell<Vec<Option<u32>>>>) -> LineTracker {
    let weak = Rc::downgrade(lines);
    let observer = doc.connect_lines_changed(move |first, removed, added| {
        if let Some(lines) = weak.upgrade() {
            for line in lines.borrow_mut().iter_mut().flatten() {
                *line = moved_line(*line, (first, removed, added));
            }
        }
    });
    LineTracker {
        doc: Rc::downgrade(doc),
        observer,
    }
}

fn remove_marks(editor: &Editor, marks: &[Option<String>]) {
    let buffer = editor.buffer();
    for name in marks.iter().flatten() {
        if let Some(mark) = buffer.mark(name)
            && !mark.is_deleted()
        {
            buffer.delete_mark(&mark);
        }
    }
}

/// The first image in `row` (its severity icon).
fn first_image(row: &gtk::ListBoxRow) -> Option<gtk::Image> {
    let mut stack = vec![row.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Ok(image) = widget.clone().downcast::<gtk::Image>() {
            return Some(image);
        }
        let mut child = widget.last_child();
        while let Some(c) = child {
            child = c.prev_sibling();
            stack.push(c);
        }
    }
    None
}

fn ask_agent_button() -> gtk::Button {
    gtk::Button::builder()
        .label("Ask Agent")
        .tooltip_text("Ask the agent to fix this")
        .css_classes(["flat"])
        .build()
}

/// The details' content: `text` (an explanation, dim) above a box of buttons that wraps onto
/// more lines when the sidebar is narrow.
fn details_box(text: Option<&str>) -> (gtk::Widget, adw::WrapBox) {
    let details = gtk::Box::new(gtk::Orientation::Vertical, 6);
    details.add_css_class("lemma-problem-details");
    if let Some(text) = text {
        details.append(
            &gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .css_classes(["dim-label"])
                .build(),
        );
    }
    let actions = adw::WrapBox::builder()
        .child_spacing(6)
        .line_spacing(6)
        .build();
    details.append(&actions);
    (details.upcast(), actions)
}

/// Gives `row` (a [`problem_row`]) a toggle at its end that reveals details below it, built by
/// `make` when they are first shown (at once when `expanded`). Returns the toggle.
fn add_details(
    row: &gtk::ListBoxRow,
    expanded: bool,
    make: impl Fn() -> gtk::Widget + 'static,
) -> gtk::ToggleButton {
    let content = row.child().expect("a problem row has content");
    row.set_child(None::<&gtk::Widget>);
    let toggle = gtk::ToggleButton::builder()
        .icon_name("pan-end-symbolic")
        .valign(gtk::Align::Start)
        .tooltip_text("Explain and Fix")
        .css_classes(["flat", "circular", "lemma-problem-toggle"])
        .build();
    if let Ok(top) = content.clone().downcast::<gtk::Box>() {
        top.append(&toggle);
    }
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_valign(gtk::Align::Center);
    column.append(&content);
    row.set_child(Some(&column));
    let built = Cell::new(false);
    let host = column.downgrade();
    toggle.connect_toggled(move |t| {
        t.set_icon_name(if t.is_active() {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
        if t.is_active()
            && !built.replace(true)
            && let Some(column) = host.upgrade()
        {
            let revealer = gtk::Revealer::builder()
                .child(&make())
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .build();
            column.append(&revealer);
            t.bind_property("active", &revealer, "reveal-child")
                .sync_create()
                .build();
        }
    });
    toggle.set_active(expanded);
    toggle
}

/// Pops `menu` up for `row` of `list`, parented to `host` (an ancestor that holds the menu's
/// actions), at `at` (list coordinates) or below the row; it is unparented once closed.
fn popup_menu(
    menu: &gio::Menu,
    host: &gtk::Box,
    list: &gtk::ListBox,
    row: &gtk::ListBoxRow,
    at: Option<(f64, f64)>,
) {
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_has_arrow(false);
    popover.set_parent(host);
    let point = at.and_then(|(x, y)| {
        list.compute_point(host, &gtk::graphene::Point::new(x as f32, y as f32))
    });
    let rect = match point {
        Some(p) => gtk::gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1),
        None => row
            .compute_bounds(host)
            .map_or(gtk::gdk::Rectangle::new(0, 0, 1, 1), |b| {
                gtk::gdk::Rectangle::new(
                    b.x() as i32,
                    b.y() as i32,
                    b.width() as i32,
                    b.height() as i32,
                )
            }),
    };
    popover.set_pointing_to(Some(&rect));
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || {
            if popover.parent().is_some() {
                popover.unparent();
            }
        });
    });
    popover.popup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(path: &Path, line: Option<u32>, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            path: path.to_path_buf(),
            line,
            severity,
            message: message.into(),
            context: None,
            source: DiagSource::Tex,
        }
    }

    #[test]
    fn problems_are_grouped_sorted_and_moved_to_the_root() {
        let dir = std::env::temp_dir().join(format!("lemma-diag-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("chapters")).unwrap();
        let dir = lemma_core::canonicalize(&dir).unwrap();
        let root = dir.join("main.tex");
        let chapter = dir.join("chapters/a.tex");
        let diagnostics = vec![
            diag(&chapter, Some(3), Severity::Warning, "w"),
            diag(&root, Some(9), Severity::Warning, "late"),
            diag(&root, Some(2), Severity::Warning, "same line, warning"),
            diag(&root, Some(2), Severity::Error, "same line, error"),
            diag(
                &dir.join(".build/main.aux"),
                None,
                Severity::Warning,
                "multiply",
            ),
            diag(
                Path::new("/usr/share/texmf/x.sty"),
                Some(4),
                Severity::Warning,
                "pkg",
            ),
            diag(&chapter, Some(3), Severity::Warning, "w"),
        ];
        let problems = problems_from(&diagnostics, &root, ".build");
        let summary: Vec<(String, Option<u32>, &str)> = problems
            .iter()
            .map(|p| {
                let name = p.path.strip_prefix(&dir).unwrap().to_string_lossy();
                // `/` on Windows too, as below.
                (name.replace('\\', "/"), p.line, p.message.as_str())
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("main.tex".into(), Some(2), "same line, error"),
                ("main.tex".into(), None, "multiply"),
                ("main.tex".into(), None, "pkg"),
                ("main.tex".into(), Some(2), "same line, warning"),
                ("main.tex".into(), Some(9), "late"),
                ("chapters/a.tex".into(), Some(3), "w"),
            ]
        );
        assert_eq!(problems[1].context.as_deref(), Some("in main.aux"));
        assert_eq!(problems[2].context.as_deref(), Some("in x.sty"));
        assert_eq!(counts(&problems), (1, 5, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_directories_holding_the_sources_and_file_order() {
        let dir = std::env::temp_dir().join(format!("lemma-diag-out-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("chapters")).unwrap();
        let dir = lemma_core::canonicalize(&dir).unwrap();
        let root = dir.join("main.tex");
        let chapter = dir.join("chapters/a.tex");
        let other = dir.join("chapters/b.tex");
        let diagnostics = vec![
            diag(&root, Some(3), Severity::BadBox, "box"),
            diag(&other, Some(1), Severity::Warning, "w"),
            diag(&chapter, Some(5), Severity::Error, "e"),
            diag(&dir.join("main.aux"), None, Severity::Warning, "aux"),
        ];
        for outdir in [".", "./", "", "..", "../.."] {
            let problems = problems_from(&diagnostics, &root, outdir);
            let got: Vec<(&str, Option<u32>)> = problems
                .iter()
                .map(|p| (p.message.as_str(), p.line))
                .collect();
            // Files by their worst problem: the chapter's error, then the warnings (the root
            // first among equals: its generated .aux warning), then the root's bad box.
            assert_eq!(
                got,
                vec![
                    ("e", Some(5)),
                    ("aux", None),
                    ("box", Some(3)),
                    ("w", Some(1)),
                ],
                "outdir {outdir:?}"
            );
        }
        // A separate output directory still marks everything in it as generated.
        let built = diag(
            &dir.join("out/chapters/a.tex"),
            Some(2),
            Severity::Warning,
            "x",
        );
        let problems = problems_from(&[built], &root, "./out/../out");
        assert_eq!(
            (problems[0].path.as_path(), problems[0].line),
            (root.as_path(), None)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_move_with_line_changes() {
        // Three lines inserted at the top: (0, 1, 4).
        assert_eq!(moved_line(66, (0, 1, 4)), 69);
        assert_eq!(moved_line(1, (0, 1, 4)), 1);
        // A character typed on the line itself, and a line before it split in two.
        assert_eq!(moved_line(10, (9, 1, 1)), 10);
        assert_eq!(moved_line(10, (4, 1, 2)), 11);
        // Lines 5..=9 (0-based 4..9) joined into one: later lines move up, the joined ones go to
        // the start of the change.
        assert_eq!(moved_line(20, (4, 5, 1)), 16);
        assert_eq!(moved_line(7, (4, 5, 1)), 5);
        assert_eq!(moved_line(4, (4, 5, 1)), 4);
    }

    #[test]
    fn next_and_previous_problem_follow_the_list() {
        let (a, b, c) = (
            Path::new("/p/a.tex"),
            Path::new("/p/b.tex"),
            Path::new("/p/c.tex"),
        );
        let problem = |path: &Path, line| Problem {
            path: path.to_path_buf(),
            line,
            severity: Severity::Error,
            message: "e".into(),
            context: None,
            source: DiagSource::Tex,
        };
        // The list's order: a.tex's problems (lines 8, none, 3), then b.tex's (5, 2).
        let problems = vec![
            problem(a, Some(8)),
            problem(a, None),
            problem(a, Some(3)),
            problem(b, Some(5)),
            problem(b, Some(2)),
        ];
        let step = |from, cursor, forward| {
            step_target(&problems, from, cursor, |i| problems[i].line, forward)
        };
        // On from the problem gone to last, past the one without a line, around the ends.
        assert_eq!(step(Some(0), None, true), Some(2));
        assert_eq!(step(Some(2), None, true), Some(3));
        assert_eq!(step(Some(4), None, true), Some(0));
        assert_eq!(step(Some(0), None, false), Some(4));
        assert_eq!(step(Some(3), None, false), Some(2));
        // From the cursor: the first (last) of its file's problems in the list after (before)
        // it, else the first problem of the next file (the last of the previous one).
        assert_eq!(step(None, Some((a, 1)), true), Some(0));
        assert_eq!(step(None, Some((a, 5)), true), Some(0));
        assert_eq!(step(None, Some((a, 5)), false), Some(2));
        assert_eq!(step(None, Some((a, 8)), true), Some(3));
        assert_eq!(step(None, Some((a, 8)), false), Some(2));
        assert_eq!(step(None, Some((a, 1)), false), Some(4));
        assert_eq!(step(None, Some((b, 3)), true), Some(3));
        assert_eq!(step(None, Some((b, 6)), true), Some(0));
        assert_eq!(step(None, Some((b, 1)), false), Some(2));
        // A file without problems, no editor, or a last problem without a line: the ends.
        assert_eq!(step(None, Some((c, 4)), true), Some(0));
        assert_eq!(step(None, Some((c, 4)), false), Some(4));
        assert_eq!(step(None, None, true), Some(0));
        assert_eq!(step(Some(1), Some((b, 3)), false), Some(4));
        // Nothing to go to.
        let lineless = vec![problem(a, None)];
        assert_eq!(step_target(&lineless, None, None, |_| None, true), None);
        assert_eq!(step_target(&[], Some(0), None, |_| None, false), None);
    }

    #[test]
    fn a_long_list_shows_the_most_severe_problems() {
        let problem = |path: &str, severity| Problem {
            path: PathBuf::from(path),
            line: Some(1),
            severity,
            message: "m".into(),
            context: None,
            source: DiagSource::Tex,
        };
        use Severity::{BadBox, Error, Warning};
        // As `problems_from` sorts them: a.tex's error and bad boxes come before b.tex's error.
        let problems = vec![
            problem("/p/a.tex", Error),
            problem("/p/a.tex", BadBox),
            problem("/p/a.tex", BadBox),
            problem("/p/b.tex", Error),
            problem("/p/c.tex", Warning),
            problem("/p/c.tex", BadBox),
        ];
        assert_eq!(listed_problems(&problems, 6), [0, 1, 2, 3, 4, 5]);
        assert_eq!(listed_problems(&problems, 4), [0, 1, 3, 4]);
        assert_eq!(listed_problems(&problems, 3), [0, 3, 4]);
        assert_eq!(listed_problems(&problems, 1), [0]);
        assert!(listed_problems(&[], 2).is_empty());
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(
            lexical_normalize(Path::new("/p/./.build")),
            Path::new("/p/.build")
        );
        assert_eq!(
            lexical_normalize(Path::new("/p/a/../out")),
            Path::new("/p/out")
        );
        assert_eq!(lexical_normalize(Path::new("/p/..")), Path::new("/"));
        assert_eq!(lexical_normalize(Path::new("/..")), Path::new("/"));
        assert_eq!(lexical_normalize(Path::new("a/../../b")), Path::new("../b"));
    }

    #[test]
    fn messages_lose_the_repeated_line() {
        assert_eq!(
            display_message("Citation 'SGA1' on page 1 undefined on input line 41."),
            "Citation 'SGA1' on page 1 undefined"
        );
        assert_eq!(
            display_message(
                "Class amsart: Abstract should precede \\maketitle in AMS document classes; \
                 reported on input line 32."
            ),
            "Class amsart: Abstract should precede \\maketitle in AMS document classes"
        );
        assert_eq!(
            display_message("Undefined control sequence."),
            "Undefined control sequence."
        );
        assert_eq!(display_message("on input line"), "on input line");
    }

    #[test]
    fn summaries() {
        assert_eq!(summary(0, 0, 0), "");
        assert_eq!(summary(1, 0, 0), "1 error");
        assert_eq!(summary(2, 1, 3), "2 errors · 1 warning · 3 bad boxes");
        assert_eq!(summary(0, 2, 1), "2 warnings · 1 bad box");
    }
}
