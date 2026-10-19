//! The git gutter.
//!
//! When git tracks a document, a thin strip at the left edge of the gutter (a renderer of its
//! own at [`POSITION`], left of the review gutter's line numbers and agent markers) marks the
//! lines added and changed relative to `HEAD`, and a small wedge marks where lines were
//! deleted. Its colours are muted (translucent green, blue and red), so it never competes with
//! the agent's hunks, which are opaque and sit right of the numbers. Files git does not track
//! get no strip, so the gutter keeps its width; a file in a directory already known to be in a
//! work tree gets it from its first frame, so the text does not move when git answers.
//!
//! - **The base** comes from `git cat-file blob <HEAD>:<path>` on the jobs thread, decoded in
//!   the document's charset, and is kept per tab as the hashes of its lines (8 bytes a line)
//!   with the `HEAD` commit it belongs to: it is read again only when `HEAD` names another
//!   commit. A file git does not track (untracked or ignored) gets no marks; one that is
//!   tracked but not in `HEAD` (added to the index, or in a repository without commits) is all
//!   added; one outside any repository gets nothing, and no `git` runs for its directory again
//!   (the answer is cached per directory).
//! - **The diff** ([`compute_marks`], [`diff_hashes`]: `similar`'s Myers line diff with a
//!   deadline) compares line hashes, lines taken without their terminators, so CRLF against LF
//!   is no change; a replaced block marks as many lines changed as it replaced, the rest added,
//!   or a wedge where it lost lines, as gitsigns does. The document's line hashes are kept
//!   exact through every change (only the changed lines are hashed again), so a diff copies 8
//!   bytes a line, never the text. It runs on the jobs thread [`DIFF_DELAY`] after the last
//!   edit: git never runs per keystroke. Meanwhile the marks move with inserted and deleted
//!   lines.
//! - **Refreshing:** on save, when the window becomes active, and when the repository's `HEAD`,
//!   index, packed refs or branch refs change (`gio::FileMonitor`s on the git directory and
//!   `refs/heads`, [`REFRESH_DELAY`] debounce), `HEAD` is resolved again (one `git rev-parse`);
//!   a new commit re-reads the bases, and the files without a `HEAD` text are checked again
//!   against the index.
//! - `[git] gutter = false` removes the renderers and stops all git work; turning it on brings
//!   them back. A missing `git` binary turns the gutter off quietly. git runs with
//!   `GIT_OPTIONAL_LOCKS=0`, no fsmonitor, literal pathspecs and no terminal prompt.
//!
//! Hooks: [`install`] once per window; [`attach`] / [`detach`] per editor tab (the renderer is
//! removed and every handler disconnected in `detach`; the view is held weakly).
//!
//! Tests: [`marks`], [`status`], [`has_renderer`], [`is_idle`], [`refresh`], [`git_runs`],
//! [`last_diff_cost`], [`compute_marks`], [`diff_hashes`], [`line_hashes`].

use crate::document::{Document, ObserverId};
use crate::editor::review_overlay::Palette;
use crate::editor::{Editor, TexView};
use crate::hooks::HookId;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use lemma_core::fnv1a64;
use sourceview5::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Gutter position of the renderer: left of the review gutter (line numbers, -30).
pub const POSITION: i32 = -40;
/// Quiet time after an edit before the diff is computed again.
pub const DIFF_DELAY: Duration = Duration::from_millis(150);
/// Quiet time after a change in the git directory before `HEAD` is resolved again.
pub const REFRESH_DELAY: Duration = Duration::from_millis(250);
/// The longest a diff may take; a slower one ends with a coarser (still correct) result.
const DIFF_DEADLINE: Duration = Duration::from_millis(500);

/// Width of the renderer, its bar and the deleted-lines wedge (pixels).
const WIDTH: i32 = 6;
const BAR_X: f32 = 1.0;
const BAR_WIDTH: f32 = 3.0;
const WEDGE_WIDTH: f32 = 5.0;
const WEDGE_HEIGHT: f32 = 8.0;

/// How a line differs from `HEAD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Changed,
}

/// The marks of a document: 0-based document lines (`\n` lines, as `LineIndex` counts them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// Runs of lines that were added or changed, ascending and disjoint.
    pub lines: Vec<(Range<u32>, MarkKind)>,
    /// Lines before which `HEAD` had lines that are gone, ascending (the line count: after the
    /// last line).
    pub deleted: Vec<u32>,
}

impl Marks {
    /// True when nothing differs.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.deleted.is_empty()
    }

    /// How `line` differs, if it does.
    pub fn kind_of(&self, line: u32) -> Option<MarkKind> {
        let i = self.lines.partition_point(|(r, _)| r.end <= line);
        self.lines
            .get(i)
            .filter(|(r, _)| r.start <= line)
            .map(|(_, k)| *k)
    }

    /// True when lines were deleted just before `line`.
    pub fn deleted_before(&self, line: u32) -> bool {
        self.deleted.binary_search(&line).is_ok()
    }

    /// Follows a change of the text: lines `[first, first + removed)` became `[first, first +
    /// added)`. Marks after it move; marks it touched stay roughly in place until the next diff.
    fn shift(&mut self, first: u32, removed: u32, added: u32) {
        let end = first + removed;
        let moved = |l: u32| (l + added).saturating_sub(removed);
        for (range, _) in &mut self.lines {
            if range.start >= end {
                *range = moved(range.start)..moved(range.end);
            } else if range.end > first {
                // A run the change touched: what is left of it ends with the change.
                if range.start >= first {
                    range.start = range.start.min(first + added.max(1) - 1);
                }
                range.end = moved(range.end.max(end)).max(range.start + 1);
            }
        }
        // Runs that now overlap merge into the first.
        let mut merged: Vec<(Range<u32>, MarkKind)> = Vec::with_capacity(self.lines.len());
        for (range, kind) in self.lines.drain(..) {
            match merged.last_mut() {
                Some((last, _)) if range.start < last.end => last.end = last.end.max(range.end),
                _ => merged.push((range, kind)),
            }
        }
        self.lines = merged;
        for line in &mut self.deleted {
            if *line >= end {
                *line = moved(*line);
            } else if *line > first {
                *line = first;
            }
        }
        self.deleted.dedup();
    }
}

/// The hashes of `text`'s lines (`\n` lines, a `\r` before the `\n` left out): what a base is
/// kept as (a 60k-line file's `HEAD` text costs 470 KiB this way instead of 2.2 MiB).
pub fn line_hashes(text: &str) -> Vec<u64> {
    text.split('\n').map(line_hash).collect()
}

/// The marks of `new` against `old` (see the module docs).
pub fn compute_marks(old: &str, new: &str) -> Marks {
    diff_hashes(&line_hashes(old), &line_hashes(new))
}

/// The hash of one line as [`line_hashes`] computes it (`line` without its `\n`).
fn line_hash(line: &str) -> u64 {
    fnv1a64(&[line.strip_suffix('\r').unwrap_or(line).as_bytes()])
}

/// [`compute_marks`] from the [`line_hashes`] of both texts.
pub fn diff_hashes(old: &[u64], new: &[u64]) -> Marks {
    let deadline = Instant::now() + DIFF_DEADLINE;
    let ops =
        similar::capture_diff_slices_deadline(similar::Algorithm::Myers, old, new, Some(deadline));
    let mut marks = Marks::default();
    let line = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    for op in ops {
        match op {
            similar::DiffOp::Equal { .. } => {}
            similar::DiffOp::Insert {
                new_index, new_len, ..
            } => marks
                .lines
                .push((line(new_index)..line(new_index + new_len), MarkKind::Added)),
            // As gitsigns does: the first lines changed, more lines added, fewer deleted.
            similar::DiffOp::Replace {
                old_len,
                new_index,
                new_len,
                ..
            } => {
                let changed = old_len.min(new_len);
                marks.lines.push((
                    line(new_index)..line(new_index + changed),
                    MarkKind::Changed,
                ));
                if new_len > changed {
                    marks.lines.push((
                        line(new_index + changed)..line(new_index + new_len),
                        MarkKind::Added,
                    ));
                }
                if old_len > changed {
                    marks.deleted.push(line(new_index + new_len));
                }
            }
            similar::DiffOp::Delete { new_index, .. } => marks.deleted.push(line(new_index)),
        }
    }
    marks.deleted.dedup();
    marks
}

/// How many `git` processes Lemma started (tests check that typing starts none).
pub fn git_runs() -> u64 {
    GIT_RUNS.load(Ordering::SeqCst)
}

static GIT_RUNS: AtomicU64 = AtomicU64::new(0);

/// What the gutter knows about a tab's file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// `[git] gutter` is off, or git is missing.
    Off,
    /// Looking for its repository or reading its `HEAD` text.
    Pending,
    /// Not inside a git work tree.
    OutsideRepository,
    /// Inside one, but git does not track it.
    Untracked,
    /// Tracked; `in_head` is false for a file added since `HEAD` (all its lines are added).
    Tracked { in_head: bool },
}

// ----- the renderer -----

/// Colours fetched once per gutter frame.
#[derive(Clone, Copy, Debug)]
struct Colors {
    added: gdk::RGBA,
    changed: gdk::RGBA,
    deleted: gdk::RGBA,
}

impl Colors {
    fn new(dark: bool) -> Self {
        let rgba = |r: u8, g: u8, b: u8, a: f32| {
            gdk::RGBA::new(
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                a,
            )
        };
        if dark {
            Self {
                added: rgba(0x57, 0xe3, 0x89, 0.5),
                changed: rgba(0x62, 0xa0, 0xea, 0.7),
                deleted: rgba(0xf6, 0x61, 0x51, 0.7),
            }
        } else {
            Self {
                added: rgba(0x2e, 0xc2, 0x7e, 0.55),
                changed: rgba(0x35, 0x84, 0xe4, 0.55),
                deleted: rgba(0xe0, 0x1b, 0x24, 0.5),
            }
        }
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GitGutterRenderer {
        pub marks: RefCell<Marks>,
        pub doc: RefCell<Weak<Document>>,
        colors: Cell<Option<Colors>>,
        /// The last line drawn in this frame: (GTK line, document line).
        last_line: Cell<Option<(u32, u32)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GitGutterRenderer {
        const NAME: &'static str = "LemmaGitGutter";
        type Type = super::GitGutterRenderer;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for GitGutterRenderer {}

    impl WidgetImpl for GitGutterRenderer {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                (WIDTH, WIDTH, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for GitGutterRenderer {
        fn begin(&self, lines: &sourceview5::GutterLines) {
            let view = lines.view();
            let dark = lines
                .buffer()
                .downcast::<sourceview5::Buffer>()
                .ok()
                .map_or_else(
                    || adw::StyleManager::default().is_dark(),
                    |b| Palette::new(view.downcast_ref::<TexView>(), &b).dark,
                );
            self.colors.set(Some(Colors::new(dark)));
            self.last_line.set(None);
        }

        fn query_data(&self, _lines: &sourceview5::GutterLines, _line: u32) {}

        fn snapshot_line(
            &self,
            snapshot: &gtk::Snapshot,
            lines: &sourceview5::GutterLines,
            line: u32,
        ) {
            self.draw_line(snapshot, lines, line);
        }
    }

    impl GitGutterRenderer {
        /// The document line of GTK line `line` (which starts at `iter`); see the review
        /// gutter's `document_line` (a lone `\r` or U+2029 also breaks GTK lines).
        fn document_line(&self, doc: &Document, iter: &gtk::TextIter, line: u32) -> u32 {
            let doc_line = match self.last_line.get() {
                Some((gtk_line, doc_line)) if gtk_line == line => doc_line,
                Some((gtk_line, doc_line)) if gtk_line + 1 == line => {
                    let mut previous = *iter;
                    let newline = previous.backward_char() && previous.char() == '\n';
                    doc_line + u32::from(newline)
                }
                _ => doc.line_index().line_of(doc.byte_offset(iter)),
            };
            self.last_line.set(Some((line, doc_line)));
            doc_line
        }

        fn draw_line(&self, snapshot: &gtk::Snapshot, lines: &sourceview5::GutterLines, line: u32) {
            let marks = self.marks.borrow();
            if marks.is_empty() {
                return;
            }
            let (Some(colors), Some(doc)) = (self.colors.get(), self.doc.borrow().upgrade()) else {
                return;
            };
            let view = lines.view();
            let iter = lines.iter_at_line(line);
            let doc_line = self.document_line(&doc, &iter, line);
            let kind = marks.kind_of(doc_line);
            // The first GTK line of the document line (a lone `\r` also starts GTK lines).
            let starts_doc_line = {
                let mut previous = iter;
                !previous.backward_char() || previous.char() == '\n'
            };
            let deleted_above = marks.deleted_before(doc_line) && starts_doc_line;
            let mut end = iter;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            let last = doc.line_index().line_count().saturating_sub(1);
            let deleted_below = doc_line == last && end.is_end() && marks.deleted_before(last + 1);
            if kind.is_none() && !deleted_above && !deleted_below {
                return;
            }
            // Buffer y → renderer y; the bar covers the line's text, not the space a review
            // hunk reserves above it.
            let (cell_y, _) =
                lines.line_yrange(line, sourceview5::GutterRendererAlignmentMode::Cell);
            let (line_y, _) = view.line_yrange(&iter);
            let shift = cell_y - line_y;
            let top = (view.iter_location(&iter).y() + shift) as f32;
            let end_rect = view.iter_location(&end);
            let bottom = (end_rect.y() + end_rect.height() + shift) as f32;
            if let Some(kind) = kind {
                let color = match kind {
                    MarkKind::Added => colors.added,
                    MarkKind::Changed => colors.changed,
                };
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(BAR_X, top, BAR_WIDTH, (bottom - top).max(1.0)),
                );
            }
            for (wanted, y) in [(deleted_above, top), (deleted_below, bottom)] {
                if wanted {
                    let builder = gsk::PathBuilder::new();
                    builder.move_to(0.0, y - WEDGE_HEIGHT / 2.0);
                    builder.line_to(WEDGE_WIDTH, y);
                    builder.line_to(0.0, y + WEDGE_HEIGHT / 2.0);
                    builder.close();
                    snapshot.append_fill(
                        &builder.to_path(),
                        gsk::FillRule::Winding,
                        &colors.deleted,
                    );
                }
            }
        }
    }
}

glib::wrapper! {
    /// The git gutter's renderer: bars and wedges from [`Marks`].
    pub struct GitGutterRenderer(ObjectSubclass<imp::GitGutterRenderer>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl GitGutterRenderer {
    fn new(doc: &Rc<Document>) -> Self {
        let renderer: Self = glib::Object::new();
        renderer.imp().doc.replace(Rc::downgrade(doc));
        renderer
    }

    fn set_marks(&self, marks: Marks) {
        if *self.imp().marks.borrow() != marks {
            self.imp().marks.replace(marks);
            self.queue_draw();
        }
    }
}

// ----- git -----

/// Where a repository lives.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RepoInfo {
    top: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

/// Runs `git` with `args` (no prompt, no optional locks, no fsmonitor); `None` when it could not
/// start, else whether it succeeded and its standard output.
fn git(dir: &Path, args: &[&str]) -> Option<(bool, Vec<u8>)> {
    GIT_RUNS.fetch_add(1, Ordering::SeqCst);
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["--literal-pathspecs", "-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) => Some((out.status.success(), out.stdout)),
        Err(e) => {
            tracing::info!("git gutter: cannot run git: {e}");
            None
        }
    }
}

/// The repository containing `dir` (jobs thread). `Err` when git cannot run.
fn discover(dir: &Path) -> Result<Option<RepoInfo>, ()> {
    let (ok, out) = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .ok_or(())?;
    if !ok {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines().map(|l| PathBuf::from(l.trim_end()));
    let (Some(top), Some(git_dir), Some(common_dir)) = (lines.next(), lines.next(), lines.next())
    else {
        return Ok(None);
    };
    if top.as_os_str().is_empty() {
        // A bare repository or inside the git directory.
        return Ok(None);
    }
    let top = top.canonicalize().unwrap_or(top);
    Ok(Some(RepoInfo {
        top,
        git_dir,
        common_dir,
    }))
}

/// The commit `HEAD` names (`None` before the first commit); `Err` when git cannot run.
fn resolve_head(top: &Path) -> Result<Option<String>, ()> {
    let (ok, out) = git(top, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).ok_or(())?;
    let oid = String::from_utf8_lossy(&out).trim().to_owned();
    Ok((ok && !oid.is_empty()).then_some(oid))
}

/// The base text of a file (see [`Base`]), on the jobs thread.
fn read_base(top: &Path, rel: &str, head: Option<&str>, charset: Option<&str>) -> Base {
    if let Some(head) = head
        && let Some((true, bytes)) = git(top, &["cat-file", "blob", &format!("{head}:./{rel}")])
    {
        return match crate::saver::decode(&bytes, charset) {
            Ok(decoded) => Base::Text(Arc::from(line_hashes(&decoded.text))),
            // A binary file: nothing to compare.
            Err(_) => Base::Untracked,
        };
    }
    match git(top, &["ls-files", "--error-unmatch", "--", rel]) {
        Some((true, _)) => Base::Added,
        _ => Base::Untracked,
    }
}

/// What a tab's file is compared with.
#[derive(Clone, Debug)]
enum Base {
    /// Not tracked by git (or a binary file): no marks.
    Untracked,
    /// Tracked, not in `HEAD`: every line is added.
    Added,
    /// The file's text in `HEAD`, as [`line_hashes`].
    Text(Arc<[u64]>),
}

// ----- the per-window state -----

struct Gutter {
    state: Weak<WindowState>,
    files: RefCell<Vec<Rc<Tracked>>>,
    repos: RefCell<HashMap<PathBuf, Rc<Repo>>>,
    /// Repository discovery, per directory: `None` = not in a work tree.
    dirs: RefCell<HashMap<PathBuf, Option<RepoInfo>>>,
    /// git could not be started: the gutter is off.
    git_missing: Cell<bool>,
    /// Discovery runs waiting for an answer.
    discovering: Cell<u32>,
    saved_hook: Cell<Option<HookId>>,
}

struct Repo {
    info: RepoInfo,
    /// `None` until resolved; then the commit `HEAD` names (`None` before the first commit).
    head: RefCell<Option<Option<String>>>,
    monitors: RefCell<Vec<gio::FileMonitor>>,
    timer: RefCell<Option<glib::SourceId>>,
    refreshing: Cell<bool>,
    /// Another refresh was asked for while one ran.
    again: Cell<bool>,
}

/// One tab's file.
struct Tracked {
    editor: Weak<Editor>,
    doc: Weak<Document>,
    path: PathBuf,
    renderer: GitGutterRenderer,
    view: glib::WeakRef<TexView>,
    inserted: Cell<bool>,
    repo: RefCell<Option<Rc<Repo>>>,
    /// The path relative to the repository's top.
    rel: RefCell<Option<String>>,
    /// The base and the `HEAD` commit it was read from.
    base: RefCell<Option<(Option<String>, Base)>>,
    /// Looked up but not in any repository.
    outside: Cell<bool>,
    loading: Cell<bool>,
    /// Another read of the base was asked for while one ran.
    load_again: Cell<bool>,
    observers: RefCell<Vec<ObserverId>>,
    diff_timer: RefCell<Option<glib::SourceId>>,
    diff_running: Cell<bool>,
    diff_again: Cell<bool>,
    /// Bumped when the base changes: an older diff's result is dropped.
    generation: Cell<u64>,
    /// What the last diff cost on the main thread (the copy of the line hashes) and on the jobs
    /// thread.
    cost: Cell<Option<(Duration, Duration)>>,
    /// The hashes of the text's lines while the file is tracked, kept exact through every change
    /// ([`Tracked::lines_changed`]).
    hashes: RefCell<Option<Vec<u64>>>,
}

thread_local! {
    static GUTTERS: RefCell<Vec<Rc<Gutter>>> = const { RefCell::new(Vec::new()) };
}

fn gutter_of(state: &WindowState) -> Option<Rc<Gutter>> {
    GUTTERS.with(|list| {
        list.borrow()
            .iter()
            .find(|g| std::ptr::eq(g.state.as_ptr(), state))
            .cloned()
    })
}

fn tracked_of(editor: &Editor) -> Option<(Rc<Gutter>, Rc<Tracked>)> {
    GUTTERS.with(|list| {
        list.borrow().iter().find_map(|g| {
            let tracked = g
                .files
                .borrow()
                .iter()
                .find(|t| std::ptr::eq(t.editor.as_ptr(), editor))
                .cloned()?;
            Some((g.clone(), tracked))
        })
    })
}

/// Called once per window after the M2 install hooks.
pub fn install(state: &Rc<WindowState>) {
    let gutter = Rc::new(Gutter {
        state: Rc::downgrade(state),
        files: RefCell::new(Vec::new()),
        repos: RefCell::new(HashMap::new()),
        dirs: RefCell::new(HashMap::new()),
        git_missing: Cell::new(false),
        discovering: Cell::new(0),
        saved_hook: Cell::new(None),
    });
    GUTTERS.with(|list| list.borrow_mut().push(gutter.clone()));
    let weak = Rc::downgrade(&gutter);
    gutter
        .saved_hook
        .set(Some(crate::saver::connect_saved(move |path, _| {
            if let Some(g) = weak.upgrade() {
                g.saved(path);
            }
        })));
    let weak = Rc::downgrade(&gutter);
    state.connect_config_changed(move |_, file| {
        if file == ConfigFile::Config
            && let Some(g) = weak.upgrade()
        {
            g.config_changed();
        }
    });
    let weak = Rc::downgrade(&gutter);
    state.window.connect_is_active_notify(move |window| {
        if window.is_active()
            && let Some(g) = weak.upgrade()
        {
            g.refresh_all();
        }
    });
    let weak = Rc::downgrade(&gutter);
    state.window.connect_destroy(move |_| {
        if let Some(g) = weak.upgrade() {
            g.close();
        }
    });
}

/// Called for every new editor tab.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(gutter) = gutter_of(state) else {
        return;
    };
    let doc = editor.document();
    let tracked = Rc::new(Tracked {
        editor: Rc::downgrade(editor),
        doc: Rc::downgrade(doc),
        path: doc.path().to_path_buf(),
        renderer: GitGutterRenderer::new(doc),
        view: editor.view().downgrade(),
        inserted: Cell::new(false),
        repo: RefCell::new(None),
        rel: RefCell::new(None),
        base: RefCell::new(None),
        outside: Cell::new(false),
        loading: Cell::new(false),
        load_again: Cell::new(false),
        observers: RefCell::new(Vec::new()),
        diff_timer: RefCell::new(None),
        diff_running: Cell::new(false),
        diff_again: Cell::new(false),
        generation: Cell::new(0),
        cost: Cell::new(None),
        hashes: RefCell::new(None),
    });
    let weak = Rc::downgrade(&tracked);
    let id = doc.connect_lines_changed(move |first, removed, added| {
        if let Some(t) = weak.upgrade() {
            t.lines_changed(first, removed, added);
            if removed != added {
                t.renderer
                    .imp()
                    .marks
                    .borrow_mut()
                    .shift(first, removed, added);
            }
            t.schedule_diff();
        }
    });
    tracked.observers.borrow_mut().push(id);
    gutter.files.borrow_mut().push(tracked.clone());
    if gutter.enabled() {
        // A file in a directory known to be in a work tree is most likely tracked: its strip
        // is there from the first frame (the text does not move when git answers).
        let in_repo = tracked
            .path
            .parent()
            .and_then(|dir| gutter.dirs.borrow().get(dir).cloned())
            .is_some_and(|info| info.is_some());
        if in_repo {
            tracked.insert_renderer();
        }
        gutter.locate(&tracked);
    }
}

/// Called when an editor tab closes.
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(gutter) = gutter_of(state) else {
        return;
    };
    let tracked = {
        let mut files = gutter.files.borrow_mut();
        let i = files
            .iter()
            .position(|t| std::ptr::eq(t.editor.as_ptr(), Rc::as_ptr(editor)));
        i.map(|i| files.remove(i))
    };
    if let Some(tracked) = tracked {
        tracked.release(editor.document());
        gutter.prune_repos();
    }
}

/// The marks shown for `editor` (`None` when the git gutter does not follow it).
pub fn marks(editor: &Editor) -> Option<Marks> {
    let (_, tracked) = tracked_of(editor)?;
    Some(tracked.renderer.imp().marks.borrow().clone())
}

/// What the git gutter knows about `editor`'s file.
pub fn status(editor: &Editor) -> Option<Status> {
    let (gutter, tracked) = tracked_of(editor)?;
    if !gutter.enabled() {
        return Some(Status::Off);
    }
    if tracked.outside.get() {
        return Some(Status::OutsideRepository);
    }
    Some(match tracked.base.borrow().as_ref() {
        None => Status::Pending,
        Some((_, Base::Untracked)) => Status::Untracked,
        Some((_, Base::Added)) => Status::Tracked { in_head: false },
        Some((_, Base::Text(_))) => Status::Tracked { in_head: true },
    })
}

/// What the last diff of `editor`'s text cost: on the main thread (a copy of its line hashes,
/// or hashing the whole text the first time) and on the jobs thread (the diff).
pub fn last_diff_cost(editor: &Editor) -> Option<(Duration, Duration)> {
    tracked_of(editor).and_then(|(_, t)| t.cost.get())
}

/// True when the renderer is in `editor`'s gutter.
pub fn has_renderer(editor: &Editor) -> bool {
    tracked_of(editor).is_some_and(|(_, t)| t.inserted.get())
}

/// True when no git query, diff or refresh of `state`'s window is running or waiting.
pub fn is_idle(state: &WindowState) -> bool {
    let Some(gutter) = gutter_of(state) else {
        return true;
    };
    let files_idle = gutter.files.borrow().iter().all(|t| {
        !t.loading.get()
            && !t.diff_running.get()
            && t.diff_timer.borrow().is_none()
            && (t.outside.get() || t.base.borrow().is_some() || !gutter.enabled())
    });
    let repos_idle = gutter
        .repos
        .borrow()
        .values()
        .all(|r| !r.refreshing.get() && r.timer.borrow().is_none());
    gutter.discovering.get() == 0 && files_idle && repos_idle
}

/// Resolves `HEAD` again in every repository of `state`'s window (what a monitor event, a save
/// or the window becoming active does), without the debounce.
pub fn refresh(state: &WindowState) {
    if let Some(gutter) = gutter_of(state) {
        let repos: Vec<Rc<Repo>> = gutter.repos.borrow().values().cloned().collect();
        for repo in repos {
            if let Some(timer) = repo.timer.take() {
                timer.remove();
            }
            gutter.refresh_repo(&repo);
        }
    }
}

impl Gutter {
    fn enabled(&self) -> bool {
        !self.git_missing.get() && self.state.upgrade().is_some_and(|s| s.config.git.gutter)
    }

    /// git cannot run: everything stops.
    fn no_git(&self) {
        if !self.git_missing.replace(true) {
            tracing::info!("git gutter: git is not available; the gutter is off");
        }
        for tracked in self.files.borrow().iter() {
            tracked.remove_renderer();
        }
    }

    /// Finds `tracked`'s repository (cached per directory), then its base.
    fn locate(self: &Rc<Self>, tracked: &Rc<Tracked>) {
        let Some(dir) = tracked.path.parent().map(Path::to_path_buf) else {
            return;
        };
        let known = self.dirs.borrow().get(&dir).cloned();
        if let Some(info) = known {
            self.located(tracked, info);
            return;
        }
        self.discovering.set(self.discovering.get() + 1);
        let weak = Rc::downgrade(self);
        let tracked = Rc::downgrade(tracked);
        glib::spawn_future_local(async move {
            let probe = dir.clone();
            let found = gio::spawn_blocking(move || discover(&probe))
                .await
                .unwrap_or(Ok(None));
            let Some(g) = weak.upgrade() else {
                return;
            };
            g.discovering.set(g.discovering.get().saturating_sub(1));
            let info = match found {
                Ok(info) => info,
                Err(()) => {
                    g.no_git();
                    return;
                }
            };
            g.dirs.borrow_mut().insert(dir, info.clone());
            if let Some(tracked) = tracked.upgrade()
                && g.enabled()
                && !tracked.is_detached()
            {
                g.located(&tracked, info);
            }
        });
    }

    fn located(self: &Rc<Self>, tracked: &Rc<Tracked>, info: Option<RepoInfo>) {
        let Some(info) = info else {
            tracked.outside.set(true);
            tracked.remove_renderer();
            tracked.renderer.set_marks(Marks::default());
            return;
        };
        let Some(rel) = tracked
            .path
            .strip_prefix(&info.top)
            .ok()
            .and_then(Path::to_str)
            .map(str::to_owned)
        else {
            // Not below the work tree's top (a path git cannot name): nothing to compare.
            tracked.outside.set(true);
            tracked.remove_renderer();
            return;
        };
        let repo = self.repo(info);
        tracked.rel.replace(Some(rel));
        tracked.repo.replace(Some(repo.clone()));
        if repo.head.borrow().is_some() {
            self.load_base(tracked, &repo);
        } else {
            self.refresh_repo(&repo);
        }
    }

    /// The repository at `info.top`, watched from its first use.
    fn repo(self: &Rc<Self>, info: RepoInfo) -> Rc<Repo> {
        if let Some(repo) = self.repos.borrow().get(&info.top) {
            return repo.clone();
        }
        let repo = Rc::new(Repo {
            info: info.clone(),
            head: RefCell::new(None),
            monitors: RefCell::new(Vec::new()),
            timer: RefCell::new(None),
            refreshing: Cell::new(false),
            again: Cell::new(false),
        });
        let mut watched = vec![info.git_dir.clone(), info.common_dir.join("refs/heads")];
        if info.common_dir != info.git_dir {
            watched.push(info.common_dir.clone());
        }
        for dir in watched {
            let monitor = match gio::File::for_path(&dir)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            {
                Ok(monitor) => monitor,
                Err(e) => {
                    tracing::info!("git gutter: cannot watch {}: {e}", dir.display());
                    continue;
                }
            };
            let in_refs = dir.ends_with("refs/heads");
            let (weak, repo_weak) = (Rc::downgrade(self), Rc::downgrade(&repo));
            monitor.connect_changed(move |_, file, other, _event| {
                let relevant = |f: &gio::File| {
                    f.basename().is_some_and(|name| {
                        let name = name.to_string_lossy();
                        !name.ends_with(".lock")
                            && (in_refs || matches!(&*name, "HEAD" | "index" | "packed-refs"))
                    })
                };
                if (relevant(file) || other.is_some_and(relevant))
                    && let (Some(g), Some(repo)) = (weak.upgrade(), repo_weak.upgrade())
                {
                    g.schedule_refresh(&repo);
                }
            });
            repo.monitors.borrow_mut().push(monitor);
        }
        self.repos.borrow_mut().insert(info.top, repo.clone());
        repo
    }

    /// Drops the repositories no tab uses any more (their monitors go with them).
    fn prune_repos(&self) {
        let used: Vec<PathBuf> = self
            .files
            .borrow()
            .iter()
            .filter_map(|t| t.repo.borrow().as_ref().map(|r| r.info.top.clone()))
            .collect();
        let gone: Vec<Rc<Repo>> = {
            let mut repos = self.repos.borrow_mut();
            let keys: Vec<PathBuf> = repos
                .keys()
                .filter(|k| !used.contains(k))
                .cloned()
                .collect();
            keys.iter().filter_map(|k| repos.remove(k)).collect()
        };
        for repo in gone {
            repo.stop();
        }
    }

    fn schedule_refresh(self: &Rc<Self>, repo: &Rc<Repo>) {
        if !self.enabled() {
            return;
        }
        if let Some(timer) = repo.timer.take() {
            timer.remove();
        }
        let (weak, repo_weak) = (Rc::downgrade(self), Rc::downgrade(repo));
        let timer = glib::timeout_add_local_once(REFRESH_DELAY, move || {
            if let (Some(g), Some(repo)) = (weak.upgrade(), repo_weak.upgrade()) {
                repo.timer.take();
                g.refresh_repo(&repo);
            }
        });
        repo.timer.replace(Some(timer));
    }

    fn refresh_all(self: &Rc<Self>) {
        let repos: Vec<Rc<Repo>> = self.repos.borrow().values().cloned().collect();
        for repo in repos {
            self.schedule_refresh(&repo);
        }
    }

    /// Resolves `HEAD` (jobs thread); a new commit re-reads every base of the repository, and
    /// files without a `HEAD` text are checked against the index again.
    fn refresh_repo(self: &Rc<Self>, repo: &Rc<Repo>) {
        if !self.enabled() {
            return;
        }
        if repo.refreshing.replace(true) {
            repo.again.set(true);
            return;
        }
        let (weak, repo_weak) = (Rc::downgrade(self), Rc::downgrade(repo));
        let top = repo.info.top.clone();
        glib::spawn_future_local(async move {
            let head = gio::spawn_blocking(move || resolve_head(&top))
                .await
                .unwrap_or(Err(()));
            let (Some(g), Some(repo)) = (weak.upgrade(), repo_weak.upgrade()) else {
                return;
            };
            repo.refreshing.set(false);
            match head {
                Err(()) => g.no_git(),
                Ok(head) => {
                    let moved = repo.head.borrow().as_ref() != Some(&head);
                    repo.head.replace(Some(head));
                    let files: Vec<Rc<Tracked>> = g
                        .files
                        .borrow()
                        .iter()
                        .filter(|t| {
                            t.repo
                                .borrow()
                                .as_ref()
                                .is_some_and(|r| Rc::ptr_eq(r, &repo))
                        })
                        .cloned()
                        .collect();
                    for tracked in files {
                        let stale = moved
                            || !matches!(tracked.base.borrow().as_ref(), Some((_, Base::Text(_))));
                        if stale {
                            g.load_base(&tracked, &repo);
                        }
                    }
                }
            }
            if repo.again.replace(false) {
                g.refresh_repo(&repo);
            }
        });
    }

    /// Reads `tracked`'s base for the repository's `HEAD` (jobs thread), then diffs.
    fn load_base(self: &Rc<Self>, tracked: &Rc<Tracked>, repo: &Rc<Repo>) {
        let Some(head) = repo.head.borrow().clone() else {
            return;
        };
        let Some(rel) = tracked.rel.borrow().clone() else {
            return;
        };
        if tracked.loading.replace(true) {
            // The running read finishes first; this one follows.
            tracked.load_again.set(true);
            return;
        }
        let charset = tracked.doc.upgrade().and_then(|d| d.charset());
        let top = repo.info.top.clone();
        let (weak, tracked_weak) = (Rc::downgrade(self), Rc::downgrade(tracked));
        let wanted = head.clone();
        glib::spawn_future_local(async move {
            let base = gio::spawn_blocking(move || {
                read_base(&top, &rel, head.as_deref(), charset.as_deref())
            })
            .await
            .unwrap_or(Base::Untracked);
            let (Some(g), Some(tracked)) = (weak.upgrade(), tracked_weak.upgrade()) else {
                return;
            };
            tracked.loading.set(false);
            if tracked.is_detached() {
                return;
            }
            let current = tracked
                .repo
                .borrow()
                .as_ref()
                .and_then(|r| r.head.borrow().clone());
            if tracked.load_again.replace(false) || current.as_ref() != Some(&wanted) {
                // HEAD moved while reading, or another read was asked for: read again.
                let repo = tracked.repo.borrow().clone();
                if let Some(repo) = repo {
                    g.load_base(&tracked, &repo);
                }
                return;
            }
            tracked.set_base(wanted, base, g.enabled());
        });
    }

    /// A document was saved: its repository is checked again.
    fn saved(self: &Rc<Self>, path: &Path) {
        let repo = self
            .files
            .borrow()
            .iter()
            .find(|t| t.path == path)
            .and_then(|t| t.repo.borrow().clone());
        if let Some(repo) = repo {
            self.schedule_refresh(&repo);
        }
    }

    fn config_changed(self: &Rc<Self>) {
        let files: Vec<Rc<Tracked>> = self.files.borrow().clone();
        if self.enabled() {
            for tracked in files {
                if tracked.inserted.get() {
                    continue;
                }
                if tracked.is_tracked() {
                    tracked.insert_renderer();
                }
                let repo = tracked.repo.borrow().clone();
                match repo {
                    Some(repo) => self.schedule_refresh(&repo),
                    None if !tracked.outside.get() => self.locate(&tracked),
                    None => {}
                }
                tracked.diff_now();
            }
        } else {
            for tracked in files {
                tracked.remove_renderer();
            }
        }
    }

    fn close(&self) {
        if let Some(hook) = self.saved_hook.take() {
            crate::saver::disconnect_saved(hook);
        }
        let files: Vec<Rc<Tracked>> = self.files.borrow_mut().drain(..).collect();
        for tracked in files {
            if let Some(doc) = tracked.doc.upgrade() {
                tracked.release(&doc);
            }
        }
        for (_, repo) in self.repos.borrow_mut().drain() {
            repo.stop();
        }
        GUTTERS.with(|list| {
            list.borrow_mut()
                .retain(|g| !std::ptr::eq(Rc::as_ptr(g), self))
        });
    }
}

impl Repo {
    fn stop(&self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        for monitor in self.monitors.borrow_mut().drain(..) {
            monitor.cancel();
        }
    }
}

impl Tracked {
    fn is_detached(&self) -> bool {
        self.view.upgrade().is_none() || self.editor.upgrade().is_none()
    }

    fn gutter(&self) -> Option<sourceview5::Gutter> {
        let view = self.view.upgrade()?;
        Some(sourceview5::prelude::ViewExt::gutter(
            view.upcast_ref::<sourceview5::View>(),
            gtk::TextWindowType::Left,
        ))
    }

    /// True when git tracks the file (its strip is shown).
    fn is_tracked(&self) -> bool {
        matches!(
            self.base.borrow().as_ref(),
            Some((_, Base::Added | Base::Text(_)))
        )
    }

    /// Takes the base read for `head`: a tracked file gets its strip and is diffed; any other
    /// file has no strip (the gutter keeps its width).
    fn set_base(self: &Rc<Self>, head: Option<String>, base: Base, enabled: bool) {
        self.base.replace(Some((head, base)));
        self.generation.set(self.generation.get() + 1);
        if !enabled {
            return;
        }
        if self.is_tracked() {
            self.insert_renderer();
            self.diff_now();
        } else {
            self.remove_renderer();
            self.hashes.replace(None);
            self.renderer.set_marks(Marks::default());
        }
    }

    /// Keeps the line hashes of the text exact after a change (the changed lines are hashed
    /// again; the others move): a diff then copies 8 bytes a line instead of the whole text.
    fn lines_changed(&self, first: u32, removed: u32, added: u32) {
        let mut hashes = self.hashes.borrow_mut();
        let Some(lines) = hashes.as_mut() else {
            return;
        };
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        let (first, removed) = (first as usize, removed as usize);
        if first + removed > lines.len() {
            // Out of step (cannot happen): read them all again at the next diff.
            *hashes = None;
            return;
        }
        let fresh: Vec<u64> = (first..first + added as usize)
            .map(|line| {
                u32::try_from(line)
                    .ok()
                    .and_then(|l| doc.line_text(l))
                    .map_or(0, |text| line_hash(&text))
            })
            .collect();
        lines.splice(first..first + removed, fresh);
    }

    /// The line hashes of the text, computed from the whole text the first time.
    fn current_hashes(&self, doc: &Document) -> Vec<u64> {
        let line_count = doc.line_index().line_count() as usize;
        let mut hashes = self.hashes.borrow_mut();
        if hashes.as_ref().is_none_or(|h| h.len() != line_count) {
            *hashes = Some(doc.with_text(line_hashes));
        }
        hashes.clone().unwrap_or_default()
    }

    fn insert_renderer(&self) {
        if self.inserted.get() {
            return;
        }
        if let Some(gutter) = self.gutter() {
            gutter.insert(&self.renderer, POSITION);
            self.inserted.set(true);
        }
    }

    fn remove_renderer(&self) {
        if !self.inserted.replace(false) {
            return;
        }
        if let Some(gutter) = self.gutter() {
            gutter.remove(&self.renderer);
        }
    }

    /// The tab closed: the renderer and every handler go.
    fn release(&self, doc: &Document) {
        self.remove_renderer();
        for id in self.observers.borrow_mut().drain(..) {
            doc.disconnect(id);
        }
        if let Some(timer) = self.diff_timer.take() {
            timer.remove();
        }
        self.base.replace(None);
        self.repo.replace(None);
        self.hashes.replace(None);
        self.renderer.imp().marks.replace(Marks::default());
    }

    fn schedule_diff(self: &Rc<Self>) {
        if self.base.borrow().is_none() || !self.inserted.get() {
            return;
        }
        if let Some(timer) = self.diff_timer.take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(DIFF_DELAY, move || {
            if let Some(t) = weak.upgrade() {
                t.diff_timer.take();
                t.diff_now();
            }
        });
        self.diff_timer.replace(Some(timer));
    }

    /// Diffs the text against the base on the jobs thread (one at a time; the newest text wins).
    fn diff_now(self: &Rc<Self>) {
        if let Some(timer) = self.diff_timer.take() {
            timer.remove();
        }
        if !self.inserted.get() {
            return;
        }
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        let base = match self.base.borrow().as_ref() {
            None => return,
            Some((_, Base::Untracked)) => None,
            Some((_, Base::Added)) => Some(Arc::<[u64]>::from(line_hashes(""))),
            Some((_, Base::Text(lines))) => Some(lines.clone()),
        };
        let Some(base) = base else {
            self.renderer.set_marks(Marks::default());
            return;
        };
        if self.diff_running.replace(true) {
            self.diff_again.set(true);
            return;
        }
        let started = Instant::now();
        let lines = self.current_hashes(&doc);
        let (version, generation) = (doc.version(), self.generation.get());
        let main_thread = started.elapsed();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let marks = gio::spawn_blocking(move || {
                let started = Instant::now();
                let marks = diff_hashes(&base, &lines);
                (marks, started.elapsed())
            })
            .await;
            let Some(t) = weak.upgrade() else {
                return;
            };
            t.diff_running.set(false);
            let marks = marks.map(|(marks, jobs_thread)| {
                t.cost.set(Some((main_thread, jobs_thread)));
                marks
            });
            let current = t.doc.upgrade().is_some_and(|d| d.version() == version);
            if let Ok(marks) = marks
                && current
                && t.generation.get() == generation
                && t.inserted.get()
            {
                t.renderer.set_marks(marks);
            } else if !current {
                t.diff_again.set(true);
            }
            if t.diff_again.replace(false) && t.diff_timer.borrow().is_none() {
                t.diff_now();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(marks: &Marks, lines: u32) -> String {
        (0..=lines)
            .map(|l| {
                let mut s = String::new();
                if marks.deleted_before(l) {
                    s.push('^');
                }
                s.push(match marks.kind_of(l) {
                    Some(MarkKind::Added) => 'A',
                    Some(MarkKind::Changed) => 'C',
                    None => '.',
                });
                s
            })
            .collect()
    }

    #[test]
    fn marks_added_changed_and_deleted_lines() {
        let old = "a\nb\nc\nd\ne\n";
        // Unchanged.
        assert!(compute_marks(old, old).is_empty());
        // A line changed, one added after it, one deleted.
        let marks = compute_marks(old, "a\nB\nnew\nc\ne\n");
        assert_eq!(kinds(&marks, 5), ".CA.^..", "{marks:?}");
        // Two lines became one: changed, and a wedge for the line that went.
        let marks = compute_marks(old, "a\nBC\nd\ne\n");
        assert_eq!(kinds(&marks, 4), ".C^...", "{marks:?}");
        // Pure insertion and pure deletion.
        let marks = compute_marks(old, "a\nb\nx\ny\nc\nd\ne\n");
        assert_eq!(kinds(&marks, 7), "..AA....");
        let marks = compute_marks(old, "a\nb\ne\n");
        assert_eq!(kinds(&marks, 3), "..^..");
        // Deleted at the start and at the end (no final line break any more).
        let marks = compute_marks(old, "b\nc\nd\ne\n");
        assert!(marks.deleted_before(0));
        let marks = compute_marks("a\nb", "a");
        assert!(marks.deleted_before(1), "after the last line: {marks:?}");
        // An empty base: every line is added.
        let marks = compute_marks("", "x\ny\n");
        assert_eq!(kinds(&marks, 2), "AA.");
    }

    #[test]
    fn line_ends_do_not_count() {
        assert!(compute_marks("a\nb\n", "a\r\nb\r\n").is_empty());
        let marks = compute_marks("a\r\nb\r\n", "a\r\nB\r\n");
        assert_eq!(marks.lines, vec![(1..2, MarkKind::Changed)]);
    }

    #[test]
    fn marks_follow_inserted_and_deleted_lines() {
        let mut marks = Marks {
            lines: vec![(2..4, MarkKind::Changed), (8..9, MarkKind::Added)],
            deleted: vec![6, 10],
        };
        // Two lines inserted at line 0 (line 0 became lines 0..3).
        marks.shift(0, 1, 3);
        assert_eq!(
            marks.lines,
            vec![(4..6, MarkKind::Changed), (10..11, MarkKind::Added)]
        );
        assert_eq!(marks.deleted, vec![8, 12]);
        // Lines 9..11 joined into one: the added run inside it stays one line.
        marks.shift(9, 3, 1);
        assert_eq!(
            marks.lines,
            vec![(4..6, MarkKind::Changed), (9..10, MarkKind::Added)]
        );
        assert_eq!(marks.deleted, vec![8, 10]);
        // Nothing moves before the change.
        marks.shift(20, 1, 2);
        assert_eq!(marks.lines[0], (4..6, MarkKind::Changed));
    }

    #[test]
    fn large_rewrites_end_within_the_deadline() {
        let old: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..20_000).map(|i| format!("other {}\n", i * 7)).collect();
        let started = Instant::now();
        let marks = compute_marks(&old, &new);
        assert!(!marks.is_empty());
        assert!(
            started.elapsed() < DIFF_DEADLINE * 4,
            "took {:?}",
            started.elapsed()
        );
    }
}
