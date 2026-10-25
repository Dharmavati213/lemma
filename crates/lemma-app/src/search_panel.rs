//! The Search page of the sidebar: project search and replace. The
//! search itself is the GUI-free `lemma-search` crate.
//!
//! # The page
//!
//! A query entry (search as you type, 150 ms after the last key; Enter searches at once), the
//! options Match Case (Alt+C), Whole Word (Alt+W) and Regular Expression (Alt+R) as toggles
//! below it, and a Replace toggle (Ctrl+H) that reveals the replace entry and **Replace All**.
//! Results stream in while the search runs and are grouped by file (name, folder and count; a
//! dot marks a file with unsaved changes); each row is one line with matches, the matches
//! highlighted and the line number on the right. Activating a file row folds it; activating a
//! line goes there (`editor::goto::navigate`, so Alt+Left comes back), selects the match when
//! Vim is off, and hides the sidebar when it is an overlay. Down in the query entry moves into
//! the list, Escape goes back to the editor.
//!
//! Open documents are searched in their live text (cached per document version, dropped
//! [`LIVE_CACHE_LIFETIME`] after the last search), the rest on disk; the project graph comes
//! first, in include order from the root document, then the other files under the project
//! directory (see `lemma_search::Scope`). The search runs on a thread of its own and its batches
//! go into a lazy list model (positions are computed, row objects made when the list asks).
//! GtkListView keeps a widget for each of its first ~200 rows, so those rows are added a few per
//! frame within a time budget, the rest a batch at once: streaming 10 000 matches stays within
//! the 4 ms frame budget in a release build ([`Panel::frame_stats`],
//! `perf_search_10k_frame_budget`). At most
//! [`lemma_search::Limits::max_matches`] matches are listed. A row whose document changed since
//! the search goes to the nearest occurrence of its match ([`relocate`]).
//!
//! # Replace
//!
//! While the replace entry is revealed every row previews the replaced line: the match struck
//! through in red, its replacement in green (capture groups `$1`, `${name}` with a regular
//! expression). **Replace All** (Ctrl+Enter in the replace entry, Ctrl+Alt+Enter) replaces every
//! match of the current text of the listed files and shows the result **for review**: the edits
//! are a named review turn ("Replace “a” → “b”", `review_ui::begin_named_turn`), so each hunk
//! can be accepted or rejected, Reject Turn in the Changes panel or the toast's Undo takes the
//! whole replace back, and files that were not open are changed and saved like an agent's
//! edits. A search cut at the match limit cannot be replaced at once.
//!
//! Hooks: [`install`] once per window puts the page into `state.sidebar.search` the first time
//! it is shown (`window::PAGE_SEARCH`, `--sidebar search`); [`show`] is `win.project-search`
//! (Ctrl+Shift+F: shows the page and focuses the query, prefilled with the active editor's
//! one-line selection); [`run_demo`] is `--demo search` (for `n-search.png`; the environment
//! variables `LEMMA_DEMO_SEARCH` and `LEMMA_DEMO_REPLACE` change its query and replacement).

use crate::document::DocRef;
use crate::editor::goto::{self, GotoTarget};
use crate::review_ui;
use crate::window::{PAGE_SEARCH, WindowState};
use crate::workspace::normalize;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use lemma_search::{
    Event, FileMatches, Limits, LineMatch, Matcher, Query, QueryError, Scope, Searcher, Summary,
    Template,
};
use lemma_syntax::{IncludeKind, ProjectIndex};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long open documents' text copies are kept after the last search.
pub const LIVE_CACHE_LIFETIME: Duration = Duration::from_secs(30);
/// Delay of search-as-you-type after the last change of the query.
const SEARCH_DELAY_MS: u32 = 150;
/// Characters of a line shown before its first match at most (the rest of the line is
/// ellipsized; the sidebar shows about 30): from a word start when there is one.
const CONTEXT_CHARS: usize = 12;
/// Longest part of the pattern or replacement quoted in the review turn's name.
const NAME_QUOTE_CHARS: usize = 24;
/// The first rows of the results are listed a few per frame (see [`Panel::feed`]):
/// GtkListView keeps a widget for each of its first 200 or so rows, and creating them is the
/// costly part. Rows past this many are added a whole batch at a time (no widgets).
const FILL_ROWS: u32 = 210;
/// Main-loop time a step of the first screenful may take; its size follows the measured cost.
const FILL_BUDGET: Duration = Duration::from_micros(2000);
/// The first step's rows, and the most rows of a step.
const FILL_FIRST_ROWS: usize = 3;
const FILL_MAX_ROWS: usize = 64;
/// A step waits for the next frame, or this long when no frame comes (an unmapped list).
const FILL_FRAME_WAIT: Duration = Duration::from_millis(30);

const CSS: &str = "
.lemma-search-controls { padding: 8px 8px 4px 8px; }
.lemma-search-replace { margin-top: 6px; }
button.lemma-search-option { min-width: 30px; min-height: 26px; padding: 0 5px; }
button.lemma-search-option label { font-weight: 700; font-size: 0.9em; }
.lemma-search-status { margin: 2px 12px 4px 12px; }
listview.lemma-search-results > row { padding: 2px 8px; }
.lemma-search-file { margin-top: 4px; }
.lemma-search-current { box-shadow: inset 2px 0 @accent_color; }
.lemma-search-file-name { font-weight: 700; }
";

// ----- results model -----

/// One file of the results.
#[derive(Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    /// The file name and its folder relative to the project directory ("" at its top).
    pub name: String,
    pub dir: String,
    /// The lines with matches.
    pub lines: Vec<LineMatch>,
    pub matches: usize,
    /// All of the file was searched (the match limit did not cut it).
    pub complete: bool,
    /// The document had unsaved changes when it was searched.
    pub dirty: bool,
    /// The document version the results describe (live documents), for exact navigation.
    pub version: Option<u64>,
    /// Folded: only its header row is listed.
    collapsed: Cell<bool>,
    /// Lines in the list so far (the first screenful of results is added a few rows at a
    /// time, see [`Panel::feed`]).
    shown: Cell<usize>,
}

impl FileEntry {
    /// Its path relative to the project directory.
    pub fn relative(&self) -> String {
        if self.dir.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.dir, self.name)
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.collapsed.get()
    }
}

/// One row of the list: a file header (`line == None`) or one of its lines.
#[derive(Clone, Debug)]
pub struct Row {
    pub file: Rc<FileEntry>,
    /// Index into `file.lines`.
    pub line: Option<usize>,
}

impl Row {
    pub fn line_match(&self) -> Option<&LineMatch> {
        self.line.and_then(|i| self.file.lines.get(i))
    }
}

/// The rows of the list: per file its header, then its lines unless it is folded. Positions
/// are computed, never stored per row, so appending a file costs as much for 10 000 lines as
/// for one.
#[derive(Default)]
struct Rows {
    files: Vec<Rc<FileEntry>>,
    /// The position of each file's header.
    starts: Vec<u32>,
    total: u32,
}

impl Rows {
    fn rows_of(file: &FileEntry) -> u32 {
        if file.collapsed.get() {
            1
        } else {
            1 + file.shown.get() as u32
        }
    }

    fn row(&self, position: u32) -> Option<Row> {
        if position >= self.total {
            return None;
        }
        let f = self.starts.partition_point(|s| *s <= position) - 1;
        let offset = position - self.starts[f];
        Some(Row {
            file: self.files[f].clone(),
            line: (offset > 0).then(|| offset as usize - 1),
        })
    }

    /// Appends `file` with its [`FileEntry::shown`] lines.
    fn push(&mut self, file: Rc<FileEntry>) {
        self.starts.push(self.total);
        self.total += Self::rows_of(&file);
        self.files.push(file);
    }

    /// Lists `n` more lines of the last file (rows are only ever appended at the end).
    fn grow_last(&mut self, n: usize) {
        if let Some(file) = self.files.last() {
            let n = n.min(file.lines.len() - file.shown.get());
            file.shown.set(file.shown.get() + n);
            if !file.collapsed.get() {
                self.total += n as u32;
            }
        }
    }

    /// Lines of the last file not listed yet.
    fn last_missing(&self) -> usize {
        self.files
            .last()
            .map_or(0, |f| f.lines.len() - f.shown.get())
    }

    /// Removes everything; returns the number of rows removed.
    fn clear(&mut self) -> u32 {
        let removed = self.total;
        *self = Self::default();
        removed
    }

    /// Folds or unfolds file `f`; returns (position, removed, added) of the change.
    fn set_collapsed(&mut self, f: usize, collapsed: bool) -> Option<(u32, u32, u32)> {
        let file = self.files.get(f)?;
        if file.collapsed.get() == collapsed || file.shown.get() == 0 {
            return None;
        }
        let lines = file.shown.get() as u32;
        file.collapsed.set(collapsed);
        let (removed, added) = if collapsed { (lines, 0) } else { (0, lines) };
        for start in &mut self.starts[f + 1..] {
            *start = *start + added - removed;
        }
        self.total = self.total + added - removed;
        Some((self.starts[f] + 1, removed, added))
    }

    fn position_of(&self, file: &Rc<FileEntry>, line: Option<usize>) -> Option<u32> {
        let f = self.files.iter().position(|e| Rc::ptr_eq(e, file))?;
        match line {
            None => Some(self.starts[f]),
            Some(l) if file.collapsed.get() || l >= file.shown.get() => None,
            Some(l) => Some(self.starts[f] + 1 + l as u32),
        }
    }
}

mod model {
    use super::{Row, Rows};
    use gtk::gio;
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::RefCell;

    glib::wrapper! {
        /// The list model of the results: [`Row`]s in `glib::BoxedAnyObject`s, created when the
        /// list asks for them.
        pub struct ResultsModel(ObjectSubclass<imp::ResultsModel>) @implements gio::ListModel;
    }

    impl Default for ResultsModel {
        fn default() -> Self {
            glib::Object::new()
        }
    }

    impl ResultsModel {
        pub(super) fn with_rows<R>(&self, f: impl FnOnce(&mut Rows) -> R) -> R {
            f(&mut self.imp().rows.borrow_mut())
        }

        pub fn row(&self, position: u32) -> Option<Row> {
            self.imp().rows.borrow().row(position)
        }
    }

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct ResultsModel {
            pub(in super::super) rows: RefCell<Rows>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for ResultsModel {
            const NAME: &'static str = "LemmaSearchResults";
            type Type = super::ResultsModel;
            type Interfaces = (gio::ListModel,);
        }

        impl ObjectImpl for ResultsModel {}

        impl ListModelImpl for ResultsModel {
            fn item_type(&self) -> glib::Type {
                glib::BoxedAnyObject::static_type()
            }

            fn n_items(&self) -> u32 {
                self.rows.borrow().total
            }

            fn item(&self, position: u32) -> Option<glib::Object> {
                let row = self.rows.borrow().row(position)?;
                Some(glib::BoxedAnyObject::new(row).upcast())
            }
        }
    }
}

pub use model::ResultsModel;

// ----- markup -----

/// Colours of the highlights, for the light or the dark style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub matched: (&'static str, &'static str),
    pub removed: (&'static str, &'static str),
    pub added: (&'static str, &'static str),
}

impl Colors {
    /// The colours for the current style (those of the review overlay for removed and added
    /// text, and a search yellow).
    pub fn current() -> Self {
        Self::for_dark(adw::StyleManager::default().is_dark())
    }

    pub fn for_dark(dark: bool) -> Self {
        if dark {
            Self {
                matched: ("#f6d32d", "38%"),
                removed: ("#f66151", "30%"),
                added: ("#57e389", "26%"),
            }
        } else {
            Self {
                matched: ("#f6d32d", "62%"),
                removed: ("#e01b24", "20%"),
                added: ("#2ec27e", "30%"),
            }
        }
    }
}

/// Pango markup of a result line: `text` with the matches `ranges` highlighted, or, with
/// `replacements` (one per range), each match struck through and followed by its replacement.
/// Leading blanks are left out, as is the start of a line whose first match comes late ("…"
/// and [`CONTEXT_CHARS`] characters before it); tabs become spaces. `cut_before` marks text that
/// does not start at the line's start.
pub fn line_markup(
    text: &str,
    ranges: &[Range<usize>],
    replacements: Option<&[String]>,
    colors: Colors,
    cut_before: bool,
) -> String {
    let first = ranges.first().map_or(0, |r| r.start.min(text.len()));
    let blank = text.len() - text.trim_start().len();
    let mut context = text[..first]
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS - 1)
        .map_or(0, |(i, _)| i);
    // Not in the middle of a word: from the next one, unless that leaves no context.
    if context > 0
        && let Some(space) = text[context..first].find(' ')
        && context + space + 1 < first
    {
        context += space + 1;
    }
    let (start, ellipsis) = if context > blank {
        (context, true)
    } else {
        (blank.min(first), cut_before)
    };
    let escape = |s: &str| glib::markup_escape_text(&s.replace('\t', " ")).to_string();
    let span = |(color, alpha): (&str, &str), extra: &str, s: &str| {
        format!(
            r#"<span background="{color}" bgalpha="{alpha}"{extra}>{}</span>"#,
            escape(s)
        )
    };
    let mut out = String::new();
    if ellipsis {
        out.push('…');
    }
    let mut at = start;
    for (i, r) in ranges.iter().enumerate() {
        let (a, b) = (r.start.max(at).min(text.len()), r.end.min(text.len()));
        if b < a {
            continue;
        }
        out.push_str(&escape(&text[at..a]));
        match replacements.and_then(|rs| rs.get(i)) {
            Some(new) => {
                if a < b {
                    out.push_str(&span(
                        colors.removed,
                        r#" strikethrough="true""#,
                        &text[a..b],
                    ));
                }
                if !new.is_empty() {
                    out.push_str(&span(colors.added, "", new));
                }
            }
            None => out.push_str(&span(colors.matched, "", &text[a..b])),
        }
        at = b;
    }
    out.push_str(&escape(&text[at..]));
    out
}

/// `n` with thin spaces between groups of three digits ("20 000").
pub(crate) fn digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

/// `n` and the noun for it: "1 match", "20 000 matches" (see [`digits`]).
pub(crate) fn count(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", digits(n), if n == 1 { one } else { many })
}

/// `s` quoted for a review turn's name, shortened to [`NAME_QUOTE_CHARS`].
fn quoted(s: &str) -> String {
    let mut chars = s.chars();
    let short: String = chars.by_ref().take(NAME_QUOTE_CHARS).collect();
    let more = if chars.next().is_some() { "…" } else { "" };
    format!("“{short}{more}”")
}

/// The name of the review turn of a replace of `pattern` by `replacement`.
pub fn replace_name(pattern: &str, replacement: &str) -> String {
    if replacement.is_empty() {
        format!("Delete {}", quoted(pattern))
    } else {
        format!("Replace {} → {}", quoted(pattern), quoted(replacement))
    }
}

// ----- the panel -----

/// Open documents' text for a search, and the versions it is of.
type LiveTexts = (Vec<(PathBuf, Arc<str>)>, HashMap<PathBuf, u64>);

/// What a Replace All did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplaceOutcome {
    /// The review turn holding the edits (0 when nothing was replaced).
    pub turn: u32,
    pub files: usize,
    pub matches: usize,
    /// Files that could not be read, changed or saved, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

/// Main-loop time the panel spends on results: what a frame can be held up by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// The most spent at once: appending one batch (with the rows the list binds right away),
    /// or binding rows in one frame's layout.
    pub max: Duration,
    /// The longest batch, and the most bind time in one frame.
    pub ingest: Duration,
    pub bind: Duration,
    /// Batches of results received.
    pub batches: usize,
    /// Rows bound.
    pub binds: usize,
    /// The most rows bound by one step that added rows (the list creates their widgets then).
    pub step_binds: usize,
}

/// The search that fills the list.
struct Current {
    query: Query,
    matcher: Matcher,
    /// The first batch arrived (its rows replace the previous search's).
    started: bool,
    done: Option<Summary>,
    /// Lines and matches received.
    files: usize,
    matches: usize,
    /// Live document versions searched.
    versions: HashMap<PathBuf, u64>,
    root_dir: PathBuf,
}

/// The Search page of one window.
pub struct Panel {
    state: Weak<WindowState>,
    root: gtk::Box,
    query: gtk::SearchEntry,
    case: gtk::ToggleButton,
    word: gtk::ToggleButton,
    regex: gtk::ToggleButton,
    replace_toggle: gtk::ToggleButton,
    replace_revealer: gtk::Revealer,
    replace: gtk::Entry,
    replace_button: gtk::Button,
    status: gtk::Label,
    /// The results; under `covers` while it shows a status page (the list stays allocated,
    /// so it creates widgets only for the rows on screen).
    results: gtk::Box,
    covers: gtk::Stack,
    none_page: adw::StatusPage,
    error_page: adw::StatusPage,
    list: gtk::ListView,
    model: ResultsModel,
    searcher: Searcher,
    generation: Cell<u64>,
    current: RefCell<Option<Current>>,
    /// The rows listed are an earlier search's: the next rows replace them (in one change, so
    /// the list reuses their widgets).
    stale_rows: Cell<bool>,
    /// Results received but not listed yet (see [`Panel::feed`]).
    pending: RefCell<std::collections::VecDeque<Rc<FileEntry>>>,
    /// The next feed step: a timeout, and the list's tick callback (the next frame).
    feed_idle: RefCell<Option<glib::SourceId>>,
    feed_tick: RefCell<Option<gtk::TickCallbackId>>,
    /// Rows per step of the first rows.
    fill_rows: Cell<usize>,
    /// The replacement of the preview (`None` while the replace entry is hidden).
    template: RefCell<Option<Template>>,
    /// The row activated last (outlined).
    active: RefCell<Option<(Rc<FileEntry>, usize)>>,
    /// Rows on screen (to restyle them).
    bound: RefCell<Vec<glib::WeakRef<gtk::ListItem>>>,
    live_cache: RefCell<HashMap<PathBuf, (u64, Arc<str>)>>,
    live_cache_timer: RefCell<Option<glib::SourceId>>,
    replacing: Cell<bool>,
    /// The replace toggle is set from code: the focus stays.
    quiet_toggle: Cell<bool>,
    /// A batch is being appended (binds then count as its time).
    ingesting: Cell<bool>,
    /// Bind time in the current frame.
    frame_work: Cell<Duration>,
    stats: Cell<FrameStats>,
}

thread_local! {
    static PANELS: RefCell<Vec<Rc<Panel>>> = const { RefCell::new(Vec::new()) };
}

/// The Search page of `state`'s window, once it was built.
pub fn panel(state: &WindowState) -> Option<Rc<Panel>> {
    PANELS.with_borrow_mut(|panels| {
        panels.retain(|p| p.state.strong_count() > 0);
        panels
            .iter()
            .find(|p| std::ptr::eq(p.state.as_ptr(), state))
            .cloned()
    })
}

/// The Search page of `state`'s window, built (and put into the sidebar) if it was not yet.
pub fn ensure(state: &Rc<WindowState>) -> Rc<Panel> {
    if let Some(panel) = panel(state) {
        return panel;
    }
    crate::build_ui::install_css_once("search", CSS);
    let panel = Panel::new(state);
    state.sidebar.search.set_child(Some(&panel.root));
    PANELS.with_borrow_mut(|panels| panels.push(panel.clone()));
    panel
}

/// Called once per window after the M2 install hooks: the page is built the first time it is
/// shown (nothing is created for windows that never search).
pub fn install(state: &Rc<WindowState>) {
    let stack = &state.sidebar.stack;
    if stack.visible_child_name().as_deref() == Some(PAGE_SEARCH) {
        ensure(state);
        return;
    }
    let weak = Rc::downgrade(state);
    stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() == Some(PAGE_SEARCH)
            && let Some(state) = weak.upgrade()
        {
            ensure(&state);
        }
    });
}

/// `win.project-search` (Ctrl+Shift+F): shows the page and focuses the query, prefilled with
/// the active editor's selection when it is one line.
pub fn show(state: &Rc<WindowState>) {
    let panel = ensure(state);
    state.sidebar.show_page(PAGE_SEARCH);
    if let Some(selection) = selection_text(state)
        && selection != panel.query.text().as_str()
    {
        panel.query.set_text(&selection);
    }
    panel.query.grab_focus();
    panel.query.select_region(0, -1);
}

/// The active editor's selection, if it is non-empty, on one line and not too long (the find
/// bar prefills with it too).
pub(crate) fn selection_text(state: &WindowState) -> Option<String> {
    let editor = state.active_editor()?;
    let (start, end) = editor.buffer().selection_bounds()?;
    let text = editor.buffer().text(&start, &end, false).to_string();
    (!text.trim().is_empty() && !text.contains('\n') && text.len() <= 200).then_some(text)
}

/// `--demo search`: searches the project of the FILE argument (`LEMMA_DEMO_SEARCH`, default
/// "sheaf") with the replace preview (`LEMMA_DEMO_REPLACE`, default "faisceau"; empty: no
/// preview) and resolves when the results are shown, the first one active.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    let pattern = std::env::var("LEMMA_DEMO_SEARCH").unwrap_or_else(|_| "sheaf".into());
    let replacement = std::env::var("LEMMA_DEMO_REPLACE").unwrap_or_else(|_| "faisceau".into());
    show(&state);
    let panel = ensure(&state);
    panel.query.set_text(&pattern);
    if !replacement.is_empty() {
        panel.set_replacement(Some(&replacement));
    }
    panel.search_now();
    crate::testmode::wait_until(Duration::from_secs(20), "the search results", || {
        !panel.is_searching()
    })
    .await?;
    if panel.files().is_empty() {
        return Err(format!("nothing matches {pattern:?}"));
    }
    // The second line row (the first is often a title) is the active one, as after a click.
    if let Some(row) = (0..panel.row_count())
        .filter_map(|p| panel.model.row(p))
        .filter(|r| r.line.is_some())
        .nth(1)
    {
        panel
            .active
            .replace(Some((row.file.clone(), row.line.unwrap_or(0))));
        panel.restyle();
    }
    // The focus stays in the query, as after typing it (in a narrow window the sidebar is an
    // overlay that closes when the focus leaves it).
    panel.query.grab_focus();
    panel.query.set_position(-1);
    crate::testmode::wait_for_frames(&state.window, 1).await;
    Ok(())
}

impl Panel {
    fn new(state: &Rc<WindowState>) -> Rc<Self> {
        let query = gtk::SearchEntry::builder()
            .placeholder_text("Search in Project")
            .search_delay(SEARCH_DELAY_MS)
            .hexpand(true)
            .build();
        query.update_property(&[gtk::accessible::Property::Label("Search in project")]);
        let option = |markup: &str, tooltip: &str| {
            let label = gtk::Label::builder().use_markup(true).label(markup).build();
            gtk::ToggleButton::builder()
                .child(&label)
                .tooltip_text(tooltip)
                .css_classes(["flat", "lemma-search-option"])
                .valign(gtk::Align::Center)
                .build()
        };
        let case = option("Aa", "Match Case (Alt+C)");
        let word = option("<u>ab</u>", "Match Whole Word (Alt+W)");
        let regex = option(".*", "Use Regular Expression (Alt+R)");
        let replace_toggle = gtk::ToggleButton::builder()
            .icon_name("edit-find-replace-symbolic")
            .tooltip_text("Replace (Ctrl+H)")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let options = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .margin_top(4)
            .build();
        options.append(&case);
        options.append(&word);
        options.append(&regex);
        let spacer = gtk::Box::builder().hexpand(true).build();
        options.append(&spacer);
        options.append(&replace_toggle);

        let replace = gtk::Entry::builder()
            .placeholder_text("Replace")
            .hexpand(true)
            .build();
        replace.update_property(&[gtk::accessible::Property::Label("Replace with")]);
        let replace_button = gtk::Button::builder()
            .label("Replace All")
            .tooltip_text(
                "Replace every match in the listed files and show the changes for review \
                 (Ctrl+Enter)",
            )
            .sensitive(false)
            .build();
        let replace_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["lemma-search-replace"])
            .build();
        replace_row.append(&replace);
        replace_row.append(&replace_button);
        let replace_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .child(&replace_row)
            .build();

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["lemma-search-controls"])
            .build();
        controls.append(&query);
        controls.append(&options);
        controls.append(&replace_revealer);

        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label", "lemma-search-status"])
            .build();
        let model = ResultsModel::default();
        let selection = gtk::NoSelection::new(Some(model.clone()));
        let factory = gtk::SignalListItemFactory::new();
        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(true)
            .css_classes(["navigation-sidebar", "lemma-search-results"])
            .build();
        list.update_property(&[gtk::accessible::Property::Label("Search results")]);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let results = gtk::Box::new(gtk::Orientation::Vertical, 0);
        results.append(&status);
        results.append(&scroller);

        let status_page = |icon: &str, title: &str, description: &str| {
            adw::StatusPage::builder()
                .icon_name(icon)
                .title(title)
                .description(description)
                .css_classes(["compact", "dim-label"])
                .vexpand(true)
                .build()
        };
        let empty_page = status_page(
            "edit-find-symbolic",
            "Search the Project",
            "Find and replace text in every file of the project.",
        );
        let none_page = status_page("edit-find-symbolic", "No Results", "");
        let error_page = status_page("dialog-warning-symbolic", "Invalid Regular Expression", "");
        let covers = gtk::Stack::builder()
            .vexpand(true)
            .vhomogeneous(false)
            .build();
        covers.add_named(&empty_page, Some("empty"));
        covers.add_named(&none_page, Some("none"));
        covers.add_named(&error_page, Some("error"));
        let overlay = gtk::Overlay::builder()
            .child(&results)
            .vexpand(true)
            .build();
        overlay.add_overlay(&covers);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&controls);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&overlay);

        let panel = Rc::new(Self {
            state: Rc::downgrade(state),
            root,
            query,
            case,
            word,
            regex,
            replace_toggle,
            replace_revealer,
            replace,
            replace_button,
            status,
            results,
            covers,
            none_page,
            error_page,
            list,
            model,
            searcher: Searcher::new(),
            generation: Cell::new(0),
            current: RefCell::new(None),
            stale_rows: Cell::new(false),
            pending: RefCell::new(std::collections::VecDeque::new()),
            feed_idle: RefCell::new(None),
            feed_tick: RefCell::new(None),
            fill_rows: Cell::new(FILL_FIRST_ROWS),
            template: RefCell::new(None),
            active: RefCell::new(None),
            bound: RefCell::new(Vec::new()),
            live_cache: RefCell::new(HashMap::new()),
            live_cache_timer: RefCell::new(None),
            replacing: Cell::new(false),
            quiet_toggle: Cell::new(false),
            ingesting: Cell::new(false),
            frame_work: Cell::new(Duration::ZERO),
            stats: Cell::new(FrameStats::default()),
        });
        panel.show_page("empty");
        panel.connect_factory(&factory);
        panel.connect_signals(state);
        panel
    }

    fn connect_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&row_widget()));
            }
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let (Some(panel), Some(item)) = (weak.upgrade(), item.downcast_ref::<gtk::ListItem>())
            else {
                return;
            };
            let started = Instant::now();
            panel.bind(item);
            panel.bound.borrow_mut().push(item.downgrade());
            let spent = started.elapsed();
            let mut stats = panel.stats.get();
            stats.binds += 1;
            panel.stats.set(stats);
            if !panel.ingesting.get() {
                panel.add_bind_work(spent);
            }
        });
        let weak = Rc::downgrade(self);
        factory.connect_unbind(move |_, item| {
            if let (Some(panel), Some(item)) =
                (weak.upgrade(), item.downcast_ref::<gtk::ListItem>())
            {
                panel
                    .bound
                    .borrow_mut()
                    .retain(|w| w.upgrade().is_some_and(|i| &i != item));
            }
        });
    }

    fn connect_signals(self: &Rc<Self>, state: &Rc<WindowState>) {
        let weak = Rc::downgrade(self);
        self.query.connect_search_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                // Text set from code was searched already (Enter always searches again).
                let unchanged = panel
                    .current
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.query == panel.query());
                if !unchanged {
                    panel.search_now();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.query.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.search_now();
            }
        });
        let weak = Rc::downgrade(self);
        self.query.connect_stop_search(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.focus_editor();
            }
        });
        // Down moves from the query into the results.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(panel) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Down && mods.is_empty() && panel.model.n_items() > 0 {
                panel.focus_results();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.query.add_controller(keys);

        for toggle in [&self.case, &self.word, &self.regex] {
            let weak = Rc::downgrade(self);
            toggle.connect_toggled(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.search_now();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.replace_toggle.connect_toggled(move |toggle| {
            if let Some(panel) = weak.upgrade() {
                panel.replace_revealer.set_reveal_child(toggle.is_active());
                panel.update_template();
                if toggle.is_active() && !panel.quiet_toggle.get() {
                    panel.replace.grab_focus();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.replace.connect_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.update_template();
            }
        });
        let weak = Rc::downgrade(self);
        self.replace_button.connect_clicked(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.start_replace_all();
            }
        });
        // Escape in the replace entry goes back to the editor; Ctrl+Enter replaces.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(panel) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape if mods.is_empty() => {
                    panel.focus_editor();
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter
                    if mods.contains(gdk::ModifierType::CONTROL_MASK) =>
                {
                    panel.start_replace_all();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.replace.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.list.connect_activate(move |_, position| {
            if let Some(panel) = weak.upgrade() {
                panel.activate(position);
            }
        });
        // Escape in the list goes back to the query.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(panel) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Escape && mods.is_empty() {
                panel.query.grab_focus();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.list.add_controller(keys);

        self.add_shortcuts();

        // Per-frame accounting (tests read it through `frame_stats`).
        let weak = Rc::downgrade(self);
        self.list.connect_realize(move |list| {
            let Some(clock) = list.frame_clock() else {
                return;
            };
            let weak = weak.clone();
            clock.connect_after_paint(move |_| {
                if let Some(panel) = weak.upgrade() {
                    // Rows are bound in a frame's layout: one frame's binds end here.
                    panel.frame_work.set(Duration::ZERO);
                }
            });
        });

        // The highlight colours follow the style.
        let weak = Rc::downgrade(self);
        let style = adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.restyle();
            }
        });
        let style = RefCell::new(Some(style));
        state.window.connect_destroy(move |_| {
            if let Some(id) = style.take() {
                adw::StyleManager::default().disconnect(id);
            }
        });
    }

    fn add_shortcuts(self: &Rc<Self>) {
        let controller = gtk::ShortcutController::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let add = |trigger: &str, run: fn(&Rc<Panel>)| {
            let weak = Rc::downgrade(self);
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(panel) = weak.upgrade() {
                    run(&panel);
                }
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(action),
            ));
        };
        add("<Alt>c", |p| p.case.set_active(!p.case.is_active()));
        add("<Alt>w", |p| p.word.set_active(!p.word.is_active()));
        add("<Alt>r", |p| p.regex.set_active(!p.regex.is_active()));
        add("<Control>h", |p| {
            p.replace_toggle.set_active(!p.replace_toggle.is_active())
        });
        add("<Control><Alt>Return|<Control><Alt>KP_Enter", |p| {
            p.start_replace_all()
        });
        self.root.add_controller(controller);
    }

    // ----- for tests and other modules -----

    /// The page's widget.
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn query_entry(&self) -> &gtk::SearchEntry {
        &self.query
    }

    pub fn replace_entry(&self) -> &gtk::Entry {
        &self.replace
    }

    pub fn replace_button(&self) -> &gtk::Button {
        &self.replace_button
    }

    pub fn list_view(&self) -> &gtk::ListView {
        &self.list
    }

    pub fn model(&self) -> &ResultsModel {
        &self.model
    }

    /// The status line above the results ("12 results in 3 files").
    pub fn status_text(&self) -> String {
        self.status.text().to_string()
    }

    /// The visible page: "empty", "none", "error" or "results".
    pub fn page(&self) -> String {
        if !self.covers.is_visible() {
            return "results".into();
        }
        self.covers
            .visible_child_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
    }

    /// Shows the results, or the status page `name` over them.
    fn show_page(&self, name: &str) {
        let results = name == "results";
        if !results {
            self.covers.set_visible_child_name(name);
        }
        self.covers.set_visible(!results);
        // Hidden but allocated: not seen, not focusable, not clickable.
        self.results.set_opacity(if results { 1.0 } else { 0.0 });
        self.results.set_sensitive(results);
    }

    /// The description of the "No Results" or "Invalid Regular Expression" page.
    pub fn message(&self) -> String {
        match self.page().as_str() {
            "none" => self.none_page.description().map(|d| d.to_string()),
            "error" => self.error_page.description().map(|d| d.to_string()),
            _ => None,
        }
        .unwrap_or_default()
    }

    /// Sets the options (they search again, as clicking them does).
    pub fn set_options(&self, regex: bool, case_sensitive: bool, whole_word: bool) {
        self.regex.set_active(regex);
        self.case.set_active(case_sensitive);
        self.word.set_active(whole_word);
    }

    /// The query as the entry and the toggles describe it.
    pub fn query(&self) -> Query {
        Query {
            pattern: self.query.text().to_string(),
            regex: self.regex.is_active(),
            case_sensitive: self.case.is_active(),
            whole_word: self.word.is_active(),
        }
    }

    /// Shows the replace entry with `text`, or hides it (`None`); the focus stays where it is.
    pub fn set_replacement(&self, text: Option<&str>) {
        if let Some(text) = text {
            self.replace.set_text(text);
        }
        self.quiet_toggle.set(true);
        self.replace_toggle.set_active(text.is_some());
        self.quiet_toggle.set(false);
        self.update_template();
    }

    /// True while a search runs.
    pub fn is_searching(&self) -> bool {
        self.current
            .borrow()
            .as_ref()
            .is_some_and(|c| c.done.is_none())
            || !self.pending.borrow().is_empty()
            || self.model.with_rows(|rows| rows.last_missing() > 0)
    }

    /// The finished search's summary.
    pub fn summary(&self) -> Option<Summary> {
        self.current.borrow().as_ref().and_then(|c| c.done.clone())
    }

    /// The files listed, in order.
    pub fn files(&self) -> Vec<Rc<FileEntry>> {
        self.model.with_rows(|rows| rows.files.clone())
    }

    pub fn row_count(&self) -> u32 {
        self.model.n_items()
    }

    pub fn row(&self, position: u32) -> Option<Row> {
        self.model.row(position)
    }

    /// The markup row `position` shows (empty for a file header).
    pub fn row_markup(&self, position: u32) -> String {
        self.model
            .row(position)
            .map(|row| self.markup_of(&row))
            .unwrap_or_default()
    }

    /// The position of `file`'s line `line` (its header with `None`), if listed.
    pub fn position_of(&self, file: &Rc<FileEntry>, line: Option<usize>) -> Option<u32> {
        self.model.with_rows(|rows| rows.position_of(file, line))
    }

    /// Main-loop time spent on results per frame (see [`FrameStats`]); `reset` starts again.
    pub fn frame_stats(&self, reset: bool) -> FrameStats {
        let stats = self.stats.get();
        if reset {
            self.stats.set(FrameStats::default());
            self.frame_work.set(Duration::ZERO);
        }
        stats
    }

    /// Searches now with the current query and options (search as you type does this 150 ms
    /// after the last key).
    pub fn search_now(self: &Rc<Self>) {
        self.start_search();
    }

    /// Activates row `position` as a click does: folds a file, goes to a line.
    pub fn activate(self: &Rc<Self>, position: u32) {
        let Some(row) = self.model.row(position) else {
            return;
        };
        match row.line {
            None => {
                let collapsed = !row.file.is_collapsed();
                self.set_collapsed(&row.file, collapsed);
            }
            Some(line) => {
                self.active.replace(Some((row.file.clone(), line)));
                self.restyle();
                if let Some(state) = self.state.upgrade() {
                    glib::spawn_future_local(go_to(state, row));
                }
            }
        }
    }

    /// Folds or unfolds `file`.
    pub fn set_collapsed(&self, file: &Rc<FileEntry>, collapsed: bool) {
        let change = self.model.with_rows(|rows| {
            let f = rows.files.iter().position(|e| Rc::ptr_eq(e, file))?;
            rows.set_collapsed(f, collapsed)
        });
        if let Some((position, removed, added)) = change {
            self.model.items_changed(position, removed, added);
            // The header's chevron.
            if let Some(header) = self.position_of(file, None) {
                self.model.items_changed(header, 1, 1);
            }
        }
    }

    /// Replace All (see the module docs).
    pub fn replace_all(self: &Rc<Self>) -> impl Future<Output = ReplaceOutcome> + use<> {
        let panel = self.clone();
        async move { panel.do_replace_all().await }
    }

    // ----- searching -----

    fn start_search(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let query = self.query();
        let matcher = match query.matcher() {
            Ok(matcher) => matcher,
            Err(e) => {
                self.searcher.cancel();
                self.clear_results();
                self.current.replace(None);
                match e {
                    QueryError::Empty => self.show_page("empty"),
                    QueryError::Invalid(message) => {
                        self.error_page.set_description(Some(&message));
                        self.show_page("error");
                    }
                }
                self.query.remove_css_class("error");
                if self.page() == "error" {
                    self.query.add_css_class("error");
                }
                self.update_replace_button();
                return;
            }
        };
        self.query.remove_css_class("error");
        let root_dir = state.workspace.root().path().to_path_buf();
        let (live, versions) = self.live_texts(&state);
        let scope = Scope {
            root: Some(root_dir.clone()),
            files: graph_order(&state),
            live,
            skip_dirs: skip_dirs(&state),
        };
        let search = self
            .searcher
            .start(matcher.clone(), scope, Limits::default());
        // The previous results stay until the first new ones replace them (no flicker), unless
        // they were still being listed.
        self.pending.borrow_mut().clear();
        self.cancel_feed();
        if self.model.with_rows(|rows| rows.last_missing() > 0) {
            self.clear_results();
        }
        self.stale_rows.set(self.model.n_items() > 0);
        self.current.replace(Some(Current {
            query,
            matcher,
            started: false,
            done: None,
            files: 0,
            matches: 0,
            versions,
            root_dir,
        }));
        self.update_template();
        self.status.set_text("Searching…");
        self.update_replace_button();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let events = search.events().clone();
            while let Ok(event) = events.recv().await {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.generation.get() != generation {
                    return;
                }
                match event {
                    Event::Files(files) => panel.ingest(files),
                    Event::Done(summary) => panel.finish(summary),
                }
            }
            drop(search);
        });
    }

    /// Takes a batch of results: they are listed by [`Panel::feed`].
    fn ingest(self: &Rc<Self>, files: Vec<FileMatches>) {
        let started = Instant::now();
        let Some(state) = self.state.upgrade() else {
            return;
        };
        // No borrow of `current` is held while the list changes: binding rows reads it.
        let first = match self.current.borrow_mut().as_mut() {
            Some(current) => !std::mem::replace(&mut current.started, true),
            None => return,
        };
        if first {
            self.show_page("results");
        }
        let (files_total, matches_total) = {
            let mut guard = self.current.borrow_mut();
            let Some(current) = guard.as_mut() else {
                return;
            };
            let mut pending = self.pending.borrow_mut();
            for f in files {
                current.files += 1;
                current.matches += f.matches;
                let doc = state.workspace.get(&f.path);
                let (name, dir) = name_and_dir(&f.path, &current.root_dir);
                pending.push_back(Rc::new(FileEntry {
                    version: f
                        .live
                        .then(|| current.versions.get(&f.path).copied())
                        .flatten(),
                    dirty: f.live && doc.is_some_and(|d| d.is_dirty()),
                    path: f.path,
                    name,
                    dir,
                    lines: f.lines,
                    matches: f.matches,
                    complete: f.complete,
                    collapsed: Cell::new(false),
                    shown: Cell::new(0),
                }));
            }
            (current.files, current.matches)
        };
        self.status.set_text(&format!(
            "Searching… {} in {}",
            count(matches_total, "result", "results"),
            count(files_total, "file", "files")
        ));
        let mut stats = self.stats.get();
        stats.batches += 1;
        self.stats.set(stats);
        self.add_ingest_work(started.elapsed());
        self.feed();
    }

    /// Lists pending results. The first [`FILL_ROWS`] go a few per frame, as many as
    /// [`FILL_BUDGET`] allows at the measured cost per row; the rest a whole batch at a time
    /// (the list creates no widgets for them).
    fn feed(self: &Rc<Self>) {
        self.cancel_feed();
        let started = Instant::now();
        let step = self.fill_rows.get();
        let stale = self.stale_rows.replace(false);
        if stale {
            self.active.replace(None);
        }
        let (position, removed, added, gradual) = self.model.with_rows(|rows| {
            let removed = if stale { rows.clear() } else { 0 };
            let position = rows.total;
            let mut pending = self.pending.borrow_mut();
            let gradual = rows.total < FILL_ROWS;
            if gradual {
                let mut budget = step;
                while budget > 0 {
                    let missing = rows.last_missing();
                    if missing > 0 {
                        let n = missing.min(budget);
                        rows.grow_last(n);
                        budget -= n;
                    } else if let Some(file) = pending.pop_front() {
                        rows.push(file);
                        budget -= 1;
                    } else {
                        break;
                    }
                }
            } else {
                rows.grow_last(usize::MAX);
                while let Some(file) = pending.pop_front() {
                    file.shown.set(file.lines.len());
                    rows.push(file);
                }
            }
            (position, removed, rows.total - position, gradual)
        });
        if added > 0 || removed > 0 {
            let binds = self.stats.get().binds;
            self.ingesting.set(true);
            self.model.items_changed(position, removed, added);
            self.ingesting.set(false);
            let mut stats = self.stats.get();
            stats.step_binds = stats.step_binds.max(stats.binds - binds);
            self.stats.set(stats);
            let spent = started.elapsed();
            self.add_ingest_work(spent);
            if gradual && added > 0 {
                let per_row = spent / added;
                let rows = FILL_BUDGET
                    .as_nanos()
                    .checked_div(per_row.as_nanos())
                    .unwrap_or(FILL_MAX_ROWS as u128);
                self.fill_rows.set((rows as usize).clamp(1, FILL_MAX_ROWS));
            }
        }
        let more = !self.pending.borrow().is_empty()
            || self.model.with_rows(|rows| rows.last_missing() > 0);
        if !more {
            self.update_replace_button();
            return;
        }
        // The next step: after the next frame while filling the screen (the list lays the new
        // rows out and learns how many fit), else at once.
        let weak = Rc::downgrade(self);
        let wait = if gradual {
            FILL_FRAME_WAIT
        } else {
            Duration::ZERO
        };
        let id = glib::timeout_add_local_full(wait, glib::Priority::DEFAULT_IDLE, move || {
            if let Some(panel) = weak.upgrade() {
                panel.feed_idle.replace(None);
                panel.feed();
            }
            glib::ControlFlow::Break
        });
        self.feed_idle.replace(Some(id));
        if gradual && self.list.is_mapped() {
            let weak = Rc::downgrade(self);
            let frames = Cell::new(0);
            let tick = self.list.add_tick_callback(move |_, _| {
                // The first tick is this frame's; the step goes in the next one.
                frames.set(frames.get() + 1);
                if frames.get() < 2 {
                    return glib::ControlFlow::Continue;
                }
                if let Some(panel) = weak.upgrade() {
                    panel.feed_tick.replace(None);
                    panel.feed();
                }
                glib::ControlFlow::Break
            });
            self.feed_tick.replace(Some(tick));
        }
    }

    fn cancel_feed(&self) {
        if let Some(id) = self.feed_idle.take() {
            id.remove();
        }
        if let Some(tick) = self.feed_tick.take() {
            tick.remove();
        }
    }

    fn finish(&self, summary: Summary) {
        let (started, pattern) = match self.current.borrow_mut().as_mut() {
            Some(current) => {
                current.done = Some(summary.clone());
                (current.started, current.query.pattern.clone())
            }
            None => return,
        };
        if !started {
            self.clear_results();
        }
        if summary.files_with_matches == 0 {
            let searched = count(summary.files_searched, "file", "files");
            self.none_page
                .set_description(Some(&format!("Nothing matches “{pattern}” in {searched}")));
            self.show_page("none");
            self.status.set_text("");
        } else {
            self.show_page("results");
            let mut text = if summary.capped {
                format!(
                    "{}+ results in {} · refine the search",
                    digits(summary.matches),
                    count(summary.files_with_matches, "file", "files")
                )
            } else {
                format!(
                    "{} in {}",
                    count(summary.matches, "result", "results"),
                    count(summary.files_with_matches, "file", "files")
                )
            };
            let skipped = summary.skipped_large + summary.skipped_binary + summary.unreadable;
            if skipped > 0 {
                text.push_str(&format!(" · {} skipped", count(skipped, "file", "files")));
            }
            self.status.set_text(&text);
            self.status.set_tooltip_text(Some(&format!(
                "{} searched{}{}",
                count(summary.files_searched, "file", "files"),
                if summary.skipped_large > 0 {
                    format!(
                        ", {} over 8 MiB skipped",
                        count(summary.skipped_large, "file", "files")
                    )
                } else {
                    String::new()
                },
                if summary.skipped_binary > 0 {
                    format!(
                        ", {} skipped",
                        count(summary.skipped_binary, "binary file", "binary files")
                    )
                } else {
                    String::new()
                },
            )));
        }
        self.update_replace_button();
    }

    fn clear_results(&self) {
        self.pending.borrow_mut().clear();
        self.cancel_feed();
        self.stale_rows.set(false);
        let removed = self.model.with_rows(Rows::clear);
        self.active.replace(None);
        if removed > 0 {
            self.model.items_changed(0, removed, 0);
        }
        self.status.set_text("");
        self.status.set_tooltip_text(None);
    }

    /// The text of the workspace's open (or unsaved) documents, copied once per version.
    fn live_texts(&self, state: &WindowState) -> LiveTexts {
        let mut cache = self.live_cache.borrow_mut();
        let mut live = Vec::new();
        let mut versions = HashMap::new();
        let mut keep = HashSet::new();
        for doc in state.workspace.documents() {
            if !doc.is_open() && !doc.is_dirty() {
                continue;
            }
            let path = doc.path().to_path_buf();
            let version = doc.version();
            let text = match cache.get(&path) {
                Some((v, text)) if *v == version => text.clone(),
                _ => {
                    let text: Arc<str> = doc.with_text(|t| Arc::from(t));
                    cache.insert(path.clone(), (version, text.clone()));
                    text
                }
            };
            keep.insert(path.clone());
            versions.insert(path.clone(), version);
            live.push((path, text));
        }
        cache.retain(|p, _| keep.contains(p));
        drop(cache);
        // The copies go a while after the last search.
        if let Some(timer) = self.live_cache_timer.take() {
            timer.remove();
        }
        let weak = self.state.clone();
        let timer = glib::timeout_add_local_once(LIVE_CACHE_LIFETIME, move || {
            if let Some(state) = weak.upgrade()
                && let Some(panel) = panel(&state)
            {
                panel.live_cache_timer.replace(None);
                panel.live_cache.borrow_mut().clear();
            }
        });
        self.live_cache_timer.replace(Some(timer));
        (live, versions)
    }

    /// Counts `spent` binding rows in the current frame.
    fn add_bind_work(&self, spent: Duration) {
        let work = self.frame_work.get() + spent;
        self.frame_work.set(work);
        let mut stats = self.stats.get();
        stats.bind = stats.bind.max(work);
        stats.max = stats.max.max(work);
        self.stats.set(stats);
    }

    /// Counts one batch that took `spent` (with the binds the list did meanwhile).
    fn add_ingest_work(&self, spent: Duration) {
        let mut stats = self.stats.get();
        stats.ingest = stats.ingest.max(spent);
        stats.max = stats.max.max(spent);
        self.stats.set(stats);
    }

    // ----- rows -----

    fn bind(&self, item: &gtk::ListItem) {
        let Some(row) = item
            .item()
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|b| b.borrow::<Row>().clone())
        else {
            return;
        };
        let Some(widget) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let parts = RowParts::of(&widget);
        let file = &row.file;
        match row.line_match() {
            None => {
                parts.chevron.set_icon_name(Some(if file.is_collapsed() {
                    "pan-end-symbolic"
                } else {
                    "pan-down-symbolic"
                }));
                let name = if file.dirty {
                    format!("{} •", file.name)
                } else {
                    file.name.clone()
                };
                parts.main.set_text(&name);
                parts.main.add_css_class("lemma-search-file-name");
                parts.main.set_hexpand(false);
                parts.main.set_ellipsize(pango::EllipsizeMode::Middle);
                parts.dir.set_text(&file.dir);
                parts.dir.set_visible(true);
                parts.right.set_text(&if file.complete {
                    digits(file.matches)
                } else {
                    format!("{}+", digits(file.matches))
                });
                parts.right.set_tooltip_text(None);
                widget.set_tooltip_text(Some(&format!(
                    "{} · {}{}",
                    file.relative(),
                    count(file.matches, "match", "matches"),
                    if file.dirty {
                        " · unsaved changes"
                    } else {
                        ""
                    }
                )));
                widget.add_css_class("lemma-search-file");
                widget.remove_css_class("lemma-search-current");
                item.set_accessible_label(&format!(
                    "{}, {}",
                    file.relative(),
                    count(file.matches, "match", "matches")
                ));
            }
            Some(line) => {
                parts.chevron.set_icon_name(None);
                parts.main.set_markup(&self.markup_of(&row));
                parts.main.remove_css_class("lemma-search-file-name");
                parts.main.set_hexpand(true);
                parts.main.set_ellipsize(pango::EllipsizeMode::End);
                parts.dir.set_visible(false);
                parts.right.set_text(&(line.line + 1).to_string());
                widget.set_tooltip_text(Some(&format!(
                    "{}:{}\n{}",
                    file.relative(),
                    line.line + 1,
                    line.text.trim()
                )));
                widget.remove_css_class("lemma-search-file");
                let active = self
                    .active
                    .borrow()
                    .as_ref()
                    .is_some_and(|(f, l)| Rc::ptr_eq(f, file) && Some(*l) == row.line);
                if active {
                    widget.add_css_class("lemma-search-current");
                } else {
                    widget.remove_css_class("lemma-search-current");
                }
                item.set_accessible_label(&format!("Line {}: {}", line.line + 1, line.text.trim()));
            }
        }
    }

    fn markup_of(&self, row: &Row) -> String {
        let Some(line) = row.line_match() else {
            return String::new();
        };
        let current = self.current.borrow();
        let template = self.template.borrow();
        // An earlier search's rows (until the new ones replace them) get no preview: the
        // template is the new search's.
        let template = template.as_ref().filter(|_| !self.stale_rows.get());
        let replacements = match (template, current.as_ref()) {
            (Some(template), Some(current)) => Some(lemma_search::preview(
                &current.matcher,
                template,
                &line.text,
                &line.ranges,
            )),
            _ => None,
        };
        line_markup(
            &line.text,
            &line.ranges,
            replacements.as_deref(),
            Colors::current(),
            line.text_start > 0,
        )
    }

    /// Binds the rows on screen again (style, preview or active row changed).
    fn restyle(&self) {
        let items: Vec<gtk::ListItem> = self
            .bound
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect();
        for item in items {
            if item.item().is_some() {
                self.bind(&item);
            }
        }
    }

    /// The preview's replacement follows the replace entry while it is shown.
    fn update_template(&self) {
        let template = match (
            self.replace_toggle.is_active(),
            self.current.borrow().as_ref(),
        ) {
            (true, Some(current)) => Some(Template::new(&current.matcher, &self.replace.text())),
            _ => None,
        };
        let changed = *self.template.borrow() != template;
        self.template.replace(template);
        if changed {
            self.restyle();
        }
        self.update_replace_button();
    }

    fn update_replace_button(&self) {
        let current = self.current.borrow();
        let listing = !self.pending.borrow().is_empty()
            || self.model.with_rows(|rows| rows.last_missing() > 0);
        let (ready, tooltip) = match current.as_ref() {
            _ if self.replacing.get() => (false, "Replacing…"),
            _ if listing => (false, "Wait for the search to finish"),
            _ if !self.replace_toggle.is_active() => (false, "Show the replace entry first"),
            None => (false, "Search for something to replace first"),
            Some(c) => match &c.done {
                None => (false, "Wait for the search to finish"),
                Some(s) if s.capped => (
                    false,
                    "Too many matches to replace at once; refine the search",
                ),
                Some(s) if s.matches == 0 => (false, "Nothing to replace"),
                Some(_) => (
                    true,
                    "Replace every match in the listed files and show the changes for review \
                     (Ctrl+Enter)",
                ),
            },
        };
        self.replace_button.set_sensitive(ready);
        self.replace_button.set_tooltip_text(Some(tooltip));
    }

    fn focus_editor(&self) {
        if let Some(editor) = self.state.upgrade().and_then(|s| s.active_editor()) {
            editor.view().grab_focus();
        }
    }

    fn focus_results(&self) {
        self.list
            .scroll_to(0, gtk::ListScrollFlags::FOCUS, None::<gtk::ScrollInfo>);
    }

    // ----- replacing -----

    fn start_replace_all(self: &Rc<Self>) {
        if !self.replace_button.is_sensitive() || !self.replace_toggle.is_active() {
            return;
        }
        let future = self.replace_all();
        glib::spawn_future_local(async move {
            future.await;
        });
    }

    async fn do_replace_all(self: Rc<Self>) -> ReplaceOutcome {
        let mut outcome = ReplaceOutcome::default();
        let Some(state) = self.state.upgrade() else {
            return outcome;
        };
        let (query, matcher, paths, capped) = {
            let current = self.current.borrow();
            let Some(current) = current.as_ref() else {
                return outcome;
            };
            let capped = current.done.as_ref().is_none_or(|s| s.capped);
            let paths: Vec<PathBuf> = self.files().iter().map(|f| f.path.clone()).collect();
            (
                current.query.clone(),
                current.matcher.clone(),
                paths,
                capped,
            )
        };
        if capped || paths.is_empty() || self.replacing.get() {
            return outcome;
        }
        let replacement = self.replace.text().to_string();
        let template = Template::new(&matcher, &replacement);
        self.replacing.set(true);
        self.replace_button.set_label("Replacing…");
        self.update_replace_button();

        // The documents, loaded (closed) when they are not open.
        let mut docs: Vec<DocRef> = Vec::new();
        let mut loaded: Vec<DocRef> = Vec::new();
        for path in &paths {
            let known = state.workspace.get(path).is_some();
            match state.workspace.get_or_load(path).await {
                Ok(doc) => {
                    if !known {
                        loaded.push(doc.clone());
                    }
                    docs.push(doc);
                }
                Err(e) => outcome.failed.push((path.clone(), e)),
            }
        }
        // Planned and applied without yielding, so no text changes in between.
        let plans: Vec<(DocRef, Vec<TextEdit>, usize)> = docs
            .into_iter()
            .filter_map(|doc| {
                let planned = doc.with_text(|text| {
                    lemma_search::plan_replace(&matcher, &template, text, |_| true)
                });
                let matches = planned.iter().map(|r| r.matches).sum();
                let edits: Vec<TextEdit> = planned
                    .into_iter()
                    .map(|r| TextEdit {
                        range: r.range,
                        text: r.text,
                    })
                    .collect();
                (!edits.is_empty()).then_some((doc, edits, matches))
            })
            .collect();
        // Files loaded for nothing (their text no longer matches) leave the workspace again.
        for doc in &loaded {
            if !doc.is_open()
                && doc.review.borrow().is_none()
                && !plans.iter().any(|(d, _, _)| Rc::ptr_eq(d, doc))
            {
                state.workspace.release(doc);
            }
        }
        let mut changed: Vec<DocRef> = Vec::new();
        if !plans.is_empty() {
            let name = replace_name(&query.pattern, &replacement);
            let turn = review_ui::begin_named_turn(&state, &name);
            outcome.turn = turn;
            for (doc, edits, matches) in plans {
                review_ui::start_review(&doc, false);
                let applied = review_ui::in_turn(&doc, turn, || {
                    doc.apply_edits(&edits, Origin::Agent { call: 0 })
                });
                match applied {
                    Ok(()) => {
                        review_ui::recompute(&doc);
                        outcome.files += 1;
                        outcome.matches += matches;
                        changed.push(doc);
                    }
                    Err(e) => outcome
                        .failed
                        .push((doc.path().to_path_buf(), e.to_string())),
                }
            }
        }
        // Disk holds the proposed text, as after an agent's edit (ARCHITECTURE §4.5).
        for doc in &changed {
            if let Err(e) = doc.save().await {
                outcome
                    .failed
                    .push((doc.path().to_path_buf(), e.to_string()));
            }
        }
        self.replacing.set(false);
        self.replace_button.set_label("Replace All");
        self.report(&state, &outcome);
        // The results now describe the replaced text.
        self.search_now();
        outcome
    }

    fn report(&self, state: &Rc<WindowState>, outcome: &ReplaceOutcome) {
        let failed = match outcome.failed.as_slice() {
            [] => String::new(),
            [(path, e)] => format!(
                " · {} failed: {e}",
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into()
                )
            ),
            more => format!(" · {} failed", count(more.len(), "file", "files")),
        };
        if outcome.turn == 0 {
            state.toast(&format!("Nothing to replace{failed}"));
            return;
        }
        let toast = adw::Toast::builder()
            .title(format!(
                "Replaced {} in {}{failed}",
                count(outcome.matches, "match", "matches"),
                count(outcome.files, "file", "files")
            ))
            .use_markup(false)
            .button_label("_Undo")
            .timeout(8)
            .build();
        let (weak, turn) = (Rc::downgrade(state), outcome.turn);
        toast.connect_button_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                review_ui::decide_turn(&state, turn, review_ui::Decision::Reject);
            }
        });
        state.toasts.add_toast(toast);
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.searcher.cancel();
        if let Some(id) = self.feed_idle.take() {
            id.remove();
        }
        // The tick callback goes with the list.
        if let Some(timer) = self.live_cache_timer.take() {
            timer.remove();
        }
    }
}

// ----- rows -----

/// The widgets of a row, for both kinds (few: the list creates them for every row on screen).
struct RowParts {
    /// The fold chevron of a file; empty (keeping the indent) on a line.
    chevron: gtk::Image,
    /// The file name, or the line's text.
    main: gtk::Label,
    /// The file's folder (hidden on a line).
    dir: gtk::Label,
    /// The file's count, or the line number.
    right: gtk::Label,
}

impl RowParts {
    fn of(row: &gtk::Box) -> Self {
        let mut children = std::iter::successors(row.first_child(), |w| w.next_sibling());
        let mut next = || children.next().expect("a result row's widgets");
        Self {
            chevron: next().downcast().expect("chevron"),
            main: next().downcast().expect("main label"),
            dir: next().downcast().expect("folder"),
            right: next().downcast().expect("count or line"),
        }
    }
}

fn row_widget() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::builder().pixel_size(16).build());
    row.append(
        &gtk::Label::builder()
            .xalign(0.0)
            .single_line_mode(true)
            .build(),
    );
    row.append(
        &gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Start)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    row.append(
        &gtk::Label::builder()
            .xalign(1.0)
            .width_chars(3)
            .css_classes(["caption", "dim-label", "numeric"])
            .build(),
    );
    row
}

/// The file name of `path` and its folder relative to `root_dir` ("" at its top; the absolute
/// folder outside it).
fn name_and_dir(path: &Path, root_dir: &Path) -> (String, String) {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    let dir =
        path.parent()
            .map_or_else(String::new, |parent| match parent.strip_prefix(root_dir) {
                Ok(rel) => rel.display().to_string(),
                Err(_) => parent.display().to_string(),
            });
    (name, dir)
}

/// Goes to the line of `row` (its first match), selecting the match when Vim is off.
async fn go_to(state: Rc<WindowState>, row: Row) {
    let Some(line) = row.line_match().cloned() else {
        return;
    };
    let path = row.file.path.clone();
    let column = line.first.start;
    let matched_len = line.first.len();
    let matched = line
        .text
        .get(column.saturating_sub(line.text_start)..)
        .and_then(|rest| rest.get(..matched_len))
        .map(str::to_owned);
    // The exact place while the document still has the text that was searched; after edits,
    // the same text nearest to where it was; for a file that is not open, the match on that
    // line (the goto module finds it when the file opens), else the line.
    let exact = state.workspace.get(&path).and_then(|doc| {
        if row.file.version.is_some_and(|v| v == doc.version()) {
            Some(line.offset + column)
        } else if doc.is_open() || doc.is_dirty() {
            let matched = matched.as_deref().filter(|m| !m.is_empty())?;
            doc.with_text(|text| relocate(text, line.line, column, matched))
        } else {
            None
        }
    });
    let target = match exact {
        Some(offset) => GotoTarget::File {
            path: path.clone(),
            offset,
        },
        None => GotoTarget::Line {
            path: path.clone(),
            line: line.line,
            find: matched.clone().filter(|m| !m.is_empty()),
        },
    };
    if let Err(e) = goto::navigate(&state, target).await {
        state.toast(&e);
        return;
    }
    state.sidebar.dismiss_overlay();
    if state.vim_enabled() || matched_len == 0 {
        return;
    }
    let Some(editor) = state.active_editor() else {
        return;
    };
    let doc = editor.document();
    if doc.path() != normalize(&path) {
        return;
    }
    let start = editor.cursor_offset();
    let end = start + matched_len;
    let fits = doc.with_text(|text| {
        text.get(start..end)
            .is_some_and(|t| matched.as_deref().is_some_and(|m| t == m))
    });
    if fits && let (Some(a), Some(b)) = (doc.iter_at_byte(start), doc.iter_at_byte(end)) {
        editor.buffer().select_range(&b, &a);
    }
}

/// Lines around a result where [`relocate`] looks for its text.
const RELOCATE_LINES: u32 = 200;

/// The byte offset of the occurrence of `matched` in `text` nearest to line `line` (0-based)
/// and byte column `column`: where a result moved to after edits. Lines within
/// [`RELOCATE_LINES`] of `line` are searched; `None` when none has it.
pub fn relocate(text: &str, line: u32, column: usize, matched: &str) -> Option<usize> {
    if matched.is_empty() {
        return None;
    }
    let first = line.saturating_sub(RELOCATE_LINES);
    let last = line.saturating_add(RELOCATE_LINES);
    let mut best: Option<((u32, usize), usize)> = None;
    let mut at = 0;
    for (n, l) in (0u32..).zip(text.split_inclusive('\n')) {
        if n > last {
            break;
        }
        if n >= first {
            let content = l.strip_suffix('\n').unwrap_or(l);
            for (i, _) in content.match_indices(matched) {
                let key = (n.abs_diff(line), i.abs_diff(column));
                if best.is_none_or(|(k, _)| key < k) {
                    best = Some((key, at + i));
                }
            }
        }
        at += l.len();
    }
    best.map(|(_, offset)| offset)
}

// ----- scope -----

/// The project graph in include order: the root document, then what it includes, depth first
/// (`\input`, `\include`, `\subfile`, `\import`, bibliographies), then the index's other files.
fn graph_order(state: &WindowState) -> Vec<PathBuf> {
    let index = state.workspace.index.borrow();
    let root = state
        .build
        .get()
        .and_then(|b| b.root())
        .map(|r| normalize(&r))
        .filter(|r| index.files.contains_key(r))
        .or_else(|| {
            index
                .files
                .iter()
                .find(|(_, f)| f.has_documentclass && f.has_begin_document)
                .map(|(p, _)| p.clone())
        });
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(root) = root {
        let root_dir = root.parent().unwrap_or(Path::new("/")).to_path_buf();
        visit_includes(&index, &root, &root_dir, &mut seen, &mut out, 0);
    }
    for path in index.files.keys().chain(index.bibs.keys()) {
        if seen.insert(path.clone()) {
            out.push(path.clone());
        }
    }
    out
}

fn visit_includes(
    index: &ProjectIndex,
    file: &Path,
    root_dir: &Path,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
    depth: usize,
) {
    if depth > 32 || !seen.insert(file.to_path_buf()) {
        return;
    }
    out.push(file.to_path_buf());
    let Some(idx) = index.files.get(file) else {
        return;
    };
    let dir = file.parent().unwrap_or(Path::new("/"));
    for inc in &idx.includes {
        let target = inc.target.trim();
        let names: Vec<String> = match inc.kind {
            IncludeKind::Input
            | IncludeKind::Include
            | IncludeKind::Subfile
            | IncludeKind::Import => match Path::new(target).extension().and_then(|e| e.to_str()) {
                Some(e) if e.eq_ignore_ascii_case("tex") => vec![target.to_owned()],
                Some(_) if inc.kind != IncludeKind::Include => {
                    vec![target.to_owned(), format!("{target}.tex")]
                }
                _ => vec![format!("{target}.tex")],
            },
            IncludeKind::Bibliography => {
                if target.ends_with(".bib") {
                    vec![target.to_owned()]
                } else {
                    vec![format!("{target}.bib")]
                }
            }
            IncludeKind::Graphics | IncludeKind::Package => continue,
        };
        let found = names
            .iter()
            .flat_map(|n| [root_dir.join(n), dir.join(n)])
            .map(|p| lexical(&p))
            .find(|p| index.files.contains_key(p) || index.bibs.contains_key(p));
        if let Some(found) = found {
            if index.bibs.contains_key(&found) {
                if seen.insert(found.clone()) {
                    out.push(found);
                }
            } else {
                visit_includes(index, &found, root_dir, seen, out, depth + 1);
            }
        }
    }
}

/// `path` with `.` and `..` folded (no file system access; index keys are canonical).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The build output directory, when it is not hidden (a hidden one is skipped anyway).
fn skip_dirs(state: &WindowState) -> Vec<PathBuf> {
    let outdir = state.config.build.outdir.trim().to_owned();
    if outdir.is_empty() || outdir == "." || outdir.starts_with('.') {
        return Vec::new();
    }
    let base = state
        .build
        .get()
        .and_then(|b| b.root())
        .and_then(|r| r.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| state.workspace.root().path().to_path_buf());
    vec![lexical(&base.join(outdir))]
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // match ranges are lists
mod tests {
    use super::*;

    #[test]
    fn markup_highlights_and_previews() {
        let c = Colors::for_dark(false);
        let m = line_markup("  a <b> a", &[2..3, 8..9], None, c, false);
        assert_eq!(
            m,
            "<span background=\"#f6d32d\" bgalpha=\"62%\">a</span> &lt;b&gt; \
             <span background=\"#f6d32d\" bgalpha=\"62%\">a</span>"
        );
        let r = vec!["x&".to_owned(), String::new()];
        let m = line_markup("a b a", &[0..1, 4..5], Some(&r), c, false);
        assert_eq!(
            m,
            "<span background=\"#e01b24\" bgalpha=\"20%\" strikethrough=\"true\">a</span>\
             <span background=\"#2ec27e\" bgalpha=\"30%\">x&amp;</span> b \
             <span background=\"#e01b24\" bgalpha=\"20%\" strikethrough=\"true\">a</span>"
        );
        // A late first match: the line starts shortly before it.
        let text = format!("{}needle", "x".repeat(100));
        let m = line_markup(&text, &[100..106], None, c, false);
        assert!(
            m.starts_with(&format!("…{}<span", "x".repeat(CONTEXT_CHARS))),
            "{m}"
        );
        // From a word start when there is one.
        let text = format!("{} one two three needle", "x".repeat(40));
        let m = line_markup(&text, &[55..61], None, c, false);
        assert!(m.starts_with("…two three <span"), "{m}");
        // Text cut before by the search is marked too.
        assert!(line_markup("ab", &[0..1], None, c, true).starts_with('…'));
        // Tabs are spaces; an empty match inserts its replacement.
        let r = vec!["% ".to_owned()];
        assert_eq!(
            line_markup("\tx", &[0..0], Some(&r), c, false),
            "<span background=\"#2ec27e\" bgalpha=\"30%\">% </span> x"
        );
    }

    #[test]
    fn rows_fold_grow_and_map_positions() {
        let entry = |lines: usize, shown: usize| {
            Rc::new(FileEntry {
                path: PathBuf::from("/p/a.tex"),
                name: "a.tex".into(),
                dir: String::new(),
                lines: (0..lines)
                    .map(|i| LineMatch {
                        line: i as u32,
                        offset: 0,
                        text: String::new(),
                        text_start: 0,
                        cut_end: false,
                        ranges: Vec::new(),
                        matches: 1,
                        first: 0..0,
                    })
                    .collect(),
                matches: lines,
                complete: true,
                dirty: false,
                version: None,
                collapsed: Cell::new(false),
                shown: Cell::new(shown),
            })
        };
        let mut rows = Rows::default();
        let (a, b) = (entry(3, 3), entry(2, 0));
        rows.push(a.clone());
        assert_eq!(rows.total, 4);
        // A file listed a few lines at a time.
        rows.push(b.clone());
        assert_eq!((rows.total, rows.last_missing()), (5, 2));
        rows.grow_last(1);
        assert_eq!((rows.total, rows.last_missing()), (6, 1));
        assert_eq!(rows.position_of(&b, Some(1)), None);
        rows.grow_last(usize::MAX);
        assert_eq!((rows.total, rows.last_missing()), (7, 0));
        assert_eq!(rows.row(4).unwrap().line, None);
        assert_eq!(rows.row(6).unwrap().line, Some(1));
        assert!(rows.row(7).is_none());
        assert_eq!(rows.set_collapsed(0, true), Some((1, 3, 0)));
        assert_eq!(rows.total, 4);
        assert!(Rc::ptr_eq(&rows.row(1).unwrap().file, &b));
        assert_eq!(rows.position_of(&b, Some(1)), Some(3));
        assert_eq!(rows.position_of(&a, Some(0)), None);
        assert_eq!(rows.set_collapsed(0, true), None);
        assert_eq!(rows.set_collapsed(0, false), Some((1, 0, 3)));
        assert_eq!(rows.clear(), 7);
    }

    #[test]
    fn relocation_finds_the_nearest_occurrence() {
        let text = "a x\nb\nx x\nc\n";
        // Unchanged: the same place.
        assert_eq!(relocate(text, 2, 2, "x"), Some(8));
        // The nearest line wins, then the nearest column: from line 1, lines 0 and 2 are as
        // near, and line 2's x is in column 0.
        assert_eq!(relocate(text, 1, 0, "x"), Some(6));
        assert_eq!(relocate(text, 1, 2, "x"), Some(2));
        assert_eq!(relocate(text, 3, 2, "x"), Some(8));
        assert_eq!(relocate(text, 0, 0, "y"), None);
        assert_eq!(relocate(text, 0, 0, ""), None);
        // Too far away.
        let far = format!("{}x\n", "\n".repeat(500));
        assert_eq!(relocate(&far, 0, 0, "x"), None);
        assert_eq!(relocate(&far, 450, 0, "x"), Some(500));
    }

    #[test]
    fn names_and_counts() {
        assert_eq!(digits(20000), "20\u{202f}000");
        assert_eq!(digits(999), "999");
        assert_eq!(digits(1234567), "1\u{202f}234\u{202f}567");
        assert_eq!(replace_name("a", "b"), "Replace “a” → “b”");
        assert_eq!(replace_name("x", ""), "Delete “x”");
        assert_eq!(
            replace_name(&"y".repeat(30), "z"),
            format!("Replace “{}…” → “z”", "y".repeat(24))
        );
        assert_eq!(
            name_and_dir(Path::new("/r/ch/a.tex"), Path::new("/r")),
            ("a.tex".to_owned(), "ch".to_owned())
        );
        assert_eq!(
            name_and_dir(Path::new("/r/a.tex"), Path::new("/r")),
            ("a.tex".to_owned(), String::new())
        );
        assert_eq!(
            lexical(Path::new("/a/b/../c/./d.tex")),
            PathBuf::from("/a/c/d.tex")
        );
    }
}
