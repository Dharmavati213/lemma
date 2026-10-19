//! Changes that bypass our tools (WP2.4; ARCHITECTURE §4.2 item 4): a `gio::FileMonitor` on
//! every project directory.
//!
//! [`install`] runs once per window (after the agent controller exists) and watches the project
//! root and the directories below it (breadth first, at most [`MAX_WATCHED_DIRS`]; `.git`,
//! `.build`, other hidden directories and the build output directory are left out), and
//! directories created later. When the root moves (`WindowState::connect_root_changed`) it
//! watches the new root instead; it stops when the window is destroyed. Events are collected
//! for [`DEBOUNCE`] (a burst of writes is handled once), then each changed file is read.
//!
//! - A file whose content hash is our own latest (or running) write (`Document::is_own_write`)
//!   is ignored, and so is a file that already holds the document's text (only the record of
//!   what is on disk is updated). Bytes that are not an own write are read again until two
//!   reads agree (a save of the document may have ended while the file was read), so an earlier
//!   own write counts as another program's change only when the file really holds it (a `git
//!   checkout` of a version Lemma saved before).
//! - **During an agent turn, or within `bridge::AFTER_TURN_GRACE` after it**
//!   (`bridge::in_agent_window`):
//!   - a saved document in the workspace (open, or closed with a review) gets
//!     `minimal_edits(text, disk)` applied with `Origin::Agent { call: 0 }`: reviewable hunks,
//!     baseline = its text before the change; a deleted file becomes a deletion of everything
//!     (rejecting it writes the file again);
//!   - a document with **unsaved changes** gets a three-way merge: from the text its file held
//!     when it was last read or written (known from the saves seen during the turn, or the turn's
//!     start, and checked against the recorded hash), only the other program's changes are
//!     applied as agent edits, and the user's unsaved changes stay unsaved. When they overlap,
//!     or that text is unknown, the document goes on the conflict bar instead;
//!   - a project source file (`workspace::is_source_file`) that is not loaded becomes a closed
//!     document reviewed against its text when the turn started (`bridge::turn_start_text`: the
//!     snapshot, or the document the workspace had then); a file the snapshot knows did not
//!     exist is reviewed as a new file (empty baseline; rejecting it moves it to the trash); a
//!     file that existed but whose text is not known uses `git show HEAD:<path>` when it is
//!     tracked, else a toast says it "changed outside review". A file larger than
//!     `agent.snapshot_cap_mib` (at least 4 MiB) is not read or reviewed (a toast says so).
//! - **Otherwise**, for documents in the workspace: a clean one reloads (one undoable change
//!   with `Origin::Reload`; a review with nothing left to decide is dropped first, so the other
//!   program's change never shows as agent hunks, and a closed document without pending hunks
//!   simply leaves the workspace, to be read again when needed), a dirty one is put on the
//!   window's conflict bar ("Reload" / "Keep mine"; Keep mine makes the next save overwrite the
//!   other program's version without asking), and a deleted one is marked modified with a toast
//!   (saving writes it again). Files that are not loaded need nothing (they are not even read).
//!   A document on the bar that loses its unsaved changes (undone, or saved over the file) leaves
//!   it, and its file is looked at again: a clean document then reloads.

use super::bridge;
use crate::document::{DocRef, Document};
use crate::review_ui;
use crate::saver::{self, DiskState};
use crate::window::WindowState;
use crate::workspace::{self, normalize};
use adw::prelude::*;
use lemma_core::TextEdit;
use lemma_core::{Origin, fnv1a64};
use lemma_review::minimal_edits;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Quiet time after the last event before the changed files are read.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// Events keep coming (a long write): handle what came after this long anyway.
const MAX_DELAY: Duration = Duration::from_secs(1);
/// Most directories watched per window (the root may be a large directory).
pub const MAX_WATCHED_DIRS: usize = 512;
/// GIO's rate limit of `CHANGED` events, in ms (its default, 800 ms, would delay reloads).
const RATE_LIMIT_MS: i32 = 100;

const CSS: &str = "
.lemma-disk-conflict { padding: 6px 12px; background-color: @warning_bg_color; color: @warning_fg_color; }
.lemma-disk-conflict label { font-weight: bold; }
";

/// What the user chose for a file another program changed while its buffer had unsaved changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Replace the buffer with the file (one undoable change).
    Reload,
    /// Keep the buffer; the next save overwrites the file without asking.
    KeepMine,
}

/// What a changed file holds now.
enum Disk {
    Present {
        bytes: glib::Slice<u8>,
        etag: Option<glib::GString>,
    },
    Absent,
}

impl Disk {
    /// `fnv1a64` of the bytes; `None` for a file that does not exist.
    fn hash(&self) -> Option<u64> {
        match self {
            Disk::Present { bytes, .. } => Some(fnv1a64(&[bytes])),
            Disk::Absent => None,
        }
    }

    fn state(&self) -> DiskState {
        match self {
            Disk::Present { bytes, etag } => DiskState::of(bytes, etag.clone()),
            Disk::Absent => DiskState::Absent,
        }
    }
}

/// The file monitor of one window.
pub struct Watcher {
    state: Weak<WindowState>,
    monitors: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
    /// Changed paths waiting for the debounce.
    pending: RefCell<BTreeSet<PathBuf>>,
    first_pending: Cell<Option<Instant>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// A batch is being processed.
    busy: Cell<bool>,
    /// Incremented when the root moves: a walk of the old root is dropped.
    generation: Cell<u64>,
    /// Documents with unsaved changes whose file another program changed, oldest first.
    conflicts: RefCell<Vec<Conflict>>,
    /// Deleted files already reported (until they come back).
    deleted: RefCell<HashSet<PathBuf>>,
    /// During an agent turn and its grace period: the text each document's file held when a
    /// save or this monitor last saw it (merge bases for documents with unsaved changes).
    bases: RefCell<HashMap<PathBuf, String>>,
    /// Files too large to review that were already reported in this turn.
    too_large: RefCell<HashSet<PathBuf>>,
    bar: ConflictBar,
    saved_hook: Cell<Option<crate::hooks::HookId>>,
}

/// A document on the conflict bar.
struct Conflict {
    doc: Weak<Document>,
    /// Its buffer's `modified-changed` handler: the conflict is over when the buffer loses its
    /// unsaved changes.
    handler: Option<(glib::WeakRef<sourceview5::Buffer>, glib::SignalHandlerId)>,
}

impl Drop for Conflict {
    fn drop(&mut self) {
        if let Some((buffer, id)) = self.handler.take()
            && let Some(buffer) = buffer.upgrade()
        {
            buffer.disconnect(id);
        }
    }
}

/// The bar under the header that names a file in conflict.
struct ConflictBar {
    revealer: gtk::Revealer,
    label: gtk::Label,
    reload: gtk::Button,
    keep: gtk::Button,
}

thread_local! {
    static WATCHERS: RefCell<Vec<Weak<Watcher>>> = const { RefCell::new(Vec::new()) };
}

/// Starts watching `state`'s project (see the module docs).
pub fn install(state: &Rc<WindowState>) {
    crate::build_ui::install_css_once("lemma-watcher", CSS);
    let watcher = Rc::new(Watcher {
        state: Rc::downgrade(state),
        monitors: RefCell::new(HashMap::new()),
        pending: RefCell::new(BTreeSet::new()),
        first_pending: Cell::new(None),
        timer: RefCell::new(None),
        busy: Cell::new(false),
        generation: Cell::new(0),
        conflicts: RefCell::new(Vec::new()),
        deleted: RefCell::new(HashSet::new()),
        bases: RefCell::new(HashMap::new()),
        too_large: RefCell::new(HashSet::new()),
        bar: ConflictBar::new(state),
        saved_hook: Cell::new(None),
    });
    watcher.connect_bar(state);
    WATCHERS.with(|w| {
        let mut watchers = w.borrow_mut();
        watchers.retain(|w| w.strong_count() > 0);
        watchers.push(Rc::downgrade(&watcher));
    });
    // A save may resolve a conflict (the user overwrote the file), and during a turn it is the
    // merge base of the document's next unsaved changes.
    let weak = Rc::downgrade(&watcher);
    watcher
        .saved_hook
        .set(Some(crate::saver::connect_saved(move |path, text| {
            if let Some(watcher) = weak.upgrade() {
                watcher.saved(path, text);
            }
        })));
    let weak = Rc::downgrade(&watcher);
    state.window.connect_destroy(move |_| {
        if let Some(watcher) = weak.upgrade() {
            watcher.shut_down();
        }
    });
    watcher.start();
    // The window owns the watcher through this hook.
    let owner = watcher;
    state.connect_root_changed(move |_| owner.start());
}

fn watcher_of(state: &WindowState) -> Option<Rc<Watcher>> {
    WATCHERS.with(|w| {
        w.borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|w| std::ptr::eq(w.state.as_ptr(), state))
    })
}

/// Watches the directories of the workspace's documents too (the walk may not have reached
/// them in a large root). The bridge calls it when a turn starts.
pub fn watch_documents(state: &Rc<WindowState>) {
    let Some(watcher) = watcher_of(state) else {
        return;
    };
    // A new turn: files too large to review are reported again.
    watcher.too_large.borrow_mut().clear();
    let root = state.workspace.root().path().to_path_buf();
    for doc in state.workspace.documents() {
        if let Some(dir) = doc.path().parent()
            && dir.starts_with(&root)
            && !is_hidden(&root, dir)
        {
            watcher.watch_dir(dir);
        }
    }
}

/// The agent turn and its grace period are over: what the monitor kept for it goes (the bridge
/// calls it when it drops the turn's snapshot).
pub fn turn_over(state: &WindowState) {
    if let Some(watcher) = watcher_of(state) {
        watcher.bases.borrow_mut().clear();
        watcher.too_large.borrow_mut().clear();
    }
}

/// The directories watched, sorted (tests).
pub fn watched_dirs(state: &WindowState) -> Vec<PathBuf> {
    let Some(watcher) = watcher_of(state) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = watcher.monitors.borrow().keys().cloned().collect();
    dirs.sort();
    dirs
}

/// True when no change is waiting or being handled (tests wait for it).
pub fn is_idle(state: &WindowState) -> bool {
    watcher_of(state).is_none_or(|w| {
        !w.busy.get() && w.pending.borrow().is_empty() && w.timer.borrow().is_none()
    })
}

/// The documents on the conflict bar (changed on disk while they had unsaved changes), oldest
/// first.
pub fn conflicts(state: &WindowState) -> Vec<PathBuf> {
    watcher_of(state).map_or_else(Vec::new, |w| {
        w.live_conflicts()
            .iter()
            .map(|d| d.path().to_path_buf())
            .collect()
    })
}

/// True while the conflict bar is shown.
pub fn conflict_bar_shown(state: &WindowState) -> bool {
    watcher_of(state).is_some_and(|w| w.bar.revealer.reveals_child())
}

/// Resolves the conflict of the document at `path` as the bar's buttons do.
pub fn resolve_conflict(
    state: &Rc<WindowState>,
    path: &Path,
    resolution: Resolution,
) -> impl Future<Output = Result<(), String>> + use<> {
    let watcher = watcher_of(state);
    let path = normalize(path);
    async move {
        let watcher = watcher.ok_or("the window has no file monitor")?;
        watcher.resolve(&path, resolution).await
    }
}

impl ConflictBar {
    fn new(state: &WindowState) -> Self {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        let reload = gtk::Button::builder()
            .label("_Reload")
            .use_underline(true)
            .build();
        let keep = gtk::Button::builder()
            .label("_Keep Mine")
            .use_underline(true)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["lemma-disk-conflict"])
            .build();
        row.append(&label);
        row.append(&reload);
        row.append(&keep);
        let revealer = gtk::Revealer::builder()
            .child(&row)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        // Under the header bar and the review banner.
        match state
            .toasts
            .child()
            .and_then(|c| c.downcast::<adw::ToolbarView>().ok())
        {
            Some(toolbar) => toolbar.add_top_bar(&revealer),
            None => tracing::warn!("no toolbar view for the file conflict bar"),
        }
        Self {
            revealer,
            label,
            reload,
            keep,
        }
    }
}

impl Watcher {
    /// A document was saved (any window's: the hook is global).
    fn saved(self: &Rc<Self>, path: &Path, text: &str) {
        if let Some(state) = self.state.upgrade()
            && state.workspace.get(path).is_some()
        {
            if in_agent_window(&state) {
                self.bases
                    .borrow_mut()
                    .insert(path.to_path_buf(), text.to_owned());
            } else {
                self.bases.borrow_mut().clear();
            }
        }
        self.update_bar();
    }

    fn connect_bar(self: &Rc<Self>, _state: &Rc<WindowState>) {
        let buttons = [
            (self.bar.reload.clone(), Resolution::Reload),
            (self.bar.keep.clone(), Resolution::KeepMine),
        ];
        for (button, resolution) in buttons {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                let Some(watcher) = weak.upgrade() else {
                    return;
                };
                let Some(doc) = watcher.live_conflicts().into_iter().next() else {
                    return;
                };
                let path = doc.path().to_path_buf();
                glib::spawn_future_local(async move {
                    if let Err(e) = watcher.resolve(&path, resolution).await
                        && let Some(state) = watcher.state.upgrade()
                    {
                        state.toast(&e);
                    }
                });
            });
        }
    }

    /// (Re)starts watching the current root.
    fn start(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.cancel_monitors();
        let root = state.workspace.root().path().to_path_buf();
        let skip = super::tools::output_dirs(&state);
        // The root right away (changes made while the walk runs are seen), the rest after it.
        self.watch_dir(&root);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let walk = gio::spawn_blocking(move || {
                workspace::walk_project_dirs(&root, &skip, MAX_WATCHED_DIRS)
            })
            .await;
            let (Some(watcher), Ok(walk)) = (weak.upgrade(), walk) else {
                return;
            };
            if watcher.generation.get() != generation {
                return;
            }
            for dir in &walk.dirs {
                watcher.watch_dir(dir);
            }
            if walk.truncated {
                tracing::info!(
                    "watching the first {MAX_WATCHED_DIRS} directories of the project only"
                );
            }
        });
    }

    fn watch_dir(self: &Rc<Self>, dir: &Path) {
        {
            let monitors = self.monitors.borrow();
            if monitors.contains_key(dir) || monitors.len() >= MAX_WATCHED_DIRS {
                return;
            }
        }
        let monitor = match gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(monitor) => monitor,
            Err(e) => {
                tracing::warn!("cannot watch {}: {e}", dir.display());
                return;
            }
        };
        monitor.set_rate_limit(RATE_LIMIT_MS);
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, event| {
            if let Some(watcher) = weak.upgrade() {
                watcher.on_event(file, other, event);
            }
        });
        self.monitors
            .borrow_mut()
            .insert(dir.to_path_buf(), monitor);
    }

    fn cancel_monitors(&self) {
        let monitors: Vec<gio::FileMonitor> =
            self.monitors.borrow_mut().drain().map(|(_, m)| m).collect();
        for monitor in monitors {
            monitor.cancel();
        }
    }

    fn shut_down(&self) {
        self.generation.set(self.generation.get() + 1);
        self.cancel_monitors();
        self.pending.borrow_mut().clear();
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        if let Some(id) = self.saved_hook.take() {
            crate::saver::disconnect_saved(id);
        }
    }

    fn on_event(
        self: &Rc<Self>,
        file: &gio::File,
        other: Option<&gio::File>,
        event: gio::FileMonitorEvent,
    ) {
        use gio::FileMonitorEvent as E;
        let paths: Vec<PathBuf> = match event {
            E::Changed
            | E::ChangesDoneHint
            | E::Created
            | E::Deleted
            | E::MovedIn
            | E::MovedOut => file.path().into_iter().collect(),
            E::Renamed | E::Moved => file
                .path()
                .into_iter()
                .chain(other.and_then(|o| o.path()))
                .collect(),
            _ => return,
        };
        let Some(state) = self.state.upgrade() else {
            return;
        };
        // A directory that goes is no longer watched (one created again at its path gets a
        // monitor of its own).
        if matches!(event, E::Deleted | E::MovedOut | E::Renamed | E::Moved) {
            let removed = file
                .path()
                .and_then(|old| self.monitors.borrow_mut().remove(&old));
            if let Some(monitor) = removed {
                monitor.cancel();
            }
        }
        let mut added = false;
        for path in paths {
            if self.interesting(&state, &path, event) {
                added |= self.pending.borrow_mut().insert(path);
            }
        }
        if added {
            self.schedule();
        }
    }

    /// Whether a change of `path` may matter: a document of the workspace, a project source
    /// file, or a new directory to watch.
    fn interesting(&self, state: &WindowState, path: &Path, event: gio::FileMonitorEvent) -> bool {
        if state.workspace.get(path).is_some() {
            return true;
        }
        let root = state.workspace.root().path().to_path_buf();
        if !path.starts_with(&root) || is_hidden(&root, path) {
            return false;
        }
        if super::tools::output_dirs(state)
            .iter()
            .any(|d| path.starts_with(d))
        {
            return false;
        }
        workspace::is_source_file(path)
            || (matches!(
                event,
                gio::FileMonitorEvent::Created
                    | gio::FileMonitorEvent::MovedIn
                    | gio::FileMonitorEvent::Renamed
            ) && path.extension().is_none())
    }

    fn schedule(self: &Rc<Self>) {
        let first = *self.first_pending.get().get_or_insert_with(Instant::now);
        self.first_pending.set(Some(first));
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let delay = if first.elapsed() >= MAX_DELAY {
            Duration::ZERO
        } else {
            DEBOUNCE
        };
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(delay, move || {
            if let Some(watcher) = weak.upgrade() {
                watcher.timer.take();
                watcher.flush();
            }
        });
        self.timer.replace(Some(id));
    }

    fn flush(self: &Rc<Self>) {
        if self.busy.get() {
            // The running batch schedules the next one when it is done.
            return;
        }
        let paths: Vec<PathBuf> = std::mem::take(&mut *self.pending.borrow_mut())
            .into_iter()
            .collect();
        self.first_pending.set(None);
        if paths.is_empty() {
            return;
        }
        self.busy.set(true);
        let watcher = self.clone();
        glib::spawn_future_local(async move {
            for path in paths {
                watcher.process(&path).await;
            }
            watcher.busy.set(false);
            if !watcher.pending.borrow().is_empty() && watcher.timer.borrow().is_none() {
                watcher.schedule();
            }
        });
    }

    async fn process(self: &Rc<Self>, path: &Path) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let path = normalize(path);
        let root = state.workspace.root().path().to_path_buf();
        if !path.starts_with(&root) {
            return;
        }
        if !in_agent_window(&state) {
            self.bases.borrow_mut().clear();
            self.too_large.borrow_mut().clear();
        }
        let file = gio::File::for_path(&path);
        if state.workspace.get(&path).is_none() {
            let info = file
                .query_info_future(
                    "standard::type,standard::size",
                    gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    glib::Priority::DEFAULT,
                )
                .await;
            let kind = info.as_ref().map(gio::FileInfo::file_type);
            if kind == Ok(gio::FileType::Directory) {
                if !is_hidden(&root, &path) {
                    self.watch_tree(&state, path);
                }
                return;
            }
            if !workspace::is_source_file(&path) || is_hidden(&root, &path) {
                return;
            }
            // A file that is not loaded matters only during a turn.
            if !in_agent_window(&state) && state.workspace.get(&path).is_none() {
                return;
            }
            // Too large to review: reading, diffing and keeping it would stall the window.
            let cap = review_cap(&state);
            if let Ok(info) = &info
                && u64::try_from(info.size()).unwrap_or(0) > cap
                && state.workspace.get(&path).is_none()
            {
                if self.too_large.borrow_mut().insert(path.clone()) {
                    state.toast(&format!(
                        "{} changed outside review: it is too large to review (over {} MiB)",
                        review_ui::relative_name(&state, &path),
                        cap >> 20
                    ));
                }
                return;
            }
        }
        let Some(mut disk) = read_disk(&file, &path).await else {
            return;
        };
        // Bytes that are not the document's own write may still be: a save that was running
        // when the file was read can have ended since, and the version it replaced no longer
        // counts as ours. Read again until two reads agree (a few times at most): a save that
        // ended before the check is on disk for the next read.
        for _ in 0..3 {
            let Some(doc) = state.workspace.get(&path) else {
                break;
            };
            let hash = disk.hash();
            if hash.is_some_and(|h| doc.is_own_write(h)) {
                break;
            }
            let Some(again) = read_disk(&file, &path).await else {
                return;
            };
            let stable = again.hash() == hash;
            disk = again;
            if stable {
                break;
            }
        }
        if matches!(disk, Disk::Present { .. }) {
            self.deleted.borrow_mut().remove(&path);
        }
        let in_turn = in_agent_window(&state);
        match state.workspace.get(&path) {
            Some(doc) => self.document_changed(&state, &doc, disk, in_turn),
            None if in_turn => self.unloaded_file_changed(&state, &path, disk).await,
            None => {}
        }
    }

    /// Watches a new directory and the directories below it. Files written into them before
    /// their monitors existed (`mkdir -p a && echo > a/b.tex`) are looked at too.
    fn watch_tree(self: &Rc<Self>, state: &WindowState, dir: PathBuf) {
        let skip = super::tools::output_dirs(state);
        if skip.iter().any(|s| dir.starts_with(s)) {
            return;
        }
        self.watch_dir(&dir);
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        glib::spawn_future_local(async move {
            let walk =
                gio::spawn_blocking(move || workspace::walk_project(&dir, &skip, 256, 64)).await;
            let (Some(watcher), Ok(walk)) = (weak.upgrade(), walk) else {
                return;
            };
            if watcher.generation.get() != generation {
                return;
            }
            for dir in &walk.dirs {
                watcher.watch_dir(dir);
            }
            let mut added = false;
            for file in walk.files {
                if workspace::is_source_file(&file) {
                    added |= watcher.pending.borrow_mut().insert(file);
                }
            }
            if added {
                watcher.schedule();
            }
        });
    }

    /// A document of the workspace changed on disk (see the module docs).
    fn document_changed(
        self: &Rc<Self>,
        state: &WindowState,
        doc: &DocRef,
        disk: Disk,
        in_turn: bool,
    ) {
        let decoded = match &disk {
            Disk::Present { bytes, .. } => {
                if doc.is_own_write(fnv1a64(&[bytes])) {
                    return;
                }
                match saver::decode(bytes, doc.charset().as_deref()) {
                    Ok(decoded) => Some(decoded),
                    Err(e) => {
                        tracing::info!("{} changed on disk: {e}", doc.path().display());
                        return;
                    }
                }
            }
            Disk::Absent => None,
        };
        let charset = decoded
            .as_ref()
            .map_or_else(|| doc.charset(), |d| d.charset.clone());
        let new_text = decoded.as_ref().map_or("", |d| d.text.as_str());
        if decoded.is_some() && doc.with_text(|text| text == new_text) {
            // A touch, or a write of what the editor holds: only the file's record changes.
            doc.saver.set_loaded(charset, disk.state());
            doc.mark_saved(doc.version());
            if in_turn {
                self.note_base(doc, new_text);
            }
            self.update_bar();
            return;
        }
        let rel = review_ui::relative_name(state, doc.path());
        if in_turn && !doc.is_dirty() {
            tracing::info!("{rel} was changed outside the agent's tools; reviewing the change");
            if apply_as_agent(doc, new_text, charset, disk.state())
                && let Some(controller) = state.agent_controller()
            {
                bridge::note_file_changed(controller);
            }
            self.note_base(doc, new_text);
            self.update_bar();
            return;
        }
        if in_turn && decoded.is_some() && self.merge_into_unsaved(doc, new_text, &charset, &disk) {
            tracing::info!(
                "{rel} was changed outside the agent's tools while it has unsaved changes; \
                 merged the change for review"
            );
            if let Some(controller) = state.agent_controller() {
                bridge::note_file_changed(controller);
            }
            self.update_bar();
            return;
        }
        if decoded.is_none() {
            doc.saver.set_loaded(doc.charset(), DiskState::Absent);
            if let Some(buffer) = doc.buffer() {
                buffer.set_modified(true);
            }
            if self.deleted.borrow_mut().insert(doc.path().to_path_buf()) {
                state.toast(&format!(
                    "{rel} was deleted by another program; saving it writes it again"
                ));
            }
            return;
        }
        if !doc.is_dirty() {
            tracing::info!("{rel} changed on disk; reloading it");
            // Not the agent's change: a review with nothing left to decide (kept only for
            // undo) must not turn it into hunks. A closed document is then read again when
            // it is needed.
            let decided = doc
                .review
                .borrow()
                .as_ref()
                .is_some_and(|r| r.hunks().is_empty());
            if decided {
                review_ui::drop_review(doc);
                if !doc.is_open() {
                    state.workspace.release(doc);
                    return;
                }
            } else if !doc.is_open() && doc.review.borrow().is_none() {
                state.workspace.release(doc);
                return;
            }
            if let Err(e) = replace_text(doc, new_text, Origin::Reload) {
                state.toast(&format!("Cannot reload {rel}: {e}"));
                return;
            }
            doc.saver.set_loaded(charset, disk.state());
            doc.mark_saved(doc.version());
            return;
        }
        self.add_conflict(doc);
        self.update_bar();
    }

    /// Puts `doc` (with unsaved changes) on the conflict bar, unless it is there.
    fn add_conflict(self: &Rc<Self>, doc: &DocRef) {
        let mut conflicts = self.conflicts.borrow_mut();
        if conflicts
            .iter()
            .any(|c| std::ptr::eq(c.doc.as_ptr(), Rc::as_ptr(doc)))
        {
            return;
        }
        // When the buffer loses its unsaved changes (undone, or saved over the file), the bar
        // is looked at again (outside the signal's emission).
        let handler = doc.buffer().map(|buffer| {
            let weak = Rc::downgrade(self);
            let id = buffer.connect_modified_changed(move |buffer| {
                if buffer.is_modified() {
                    return;
                }
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(watcher) = weak.upgrade() {
                        watcher.update_bar();
                    }
                });
            });
            (buffer.downgrade(), id)
        });
        conflicts.push(Conflict {
            doc: Rc::downgrade(doc),
            handler,
        });
    }

    /// Remembers that `doc`'s file holds `text` (a merge base; only during a turn).
    fn note_base(&self, doc: &DocRef, text: &str) {
        self.bases
            .borrow_mut()
            .insert(doc.path().to_path_buf(), text.to_owned());
    }

    /// Applies another program's change of `doc`'s file (`theirs`) to the document, which has
    /// unsaved changes, by a three-way merge from the text the file held before (see the module
    /// docs). The merged-in changes are agent edits; the user's changes stay unsaved. False
    /// (nothing changed) when that text is unknown or the changes overlap.
    fn merge_into_unsaved(
        &self,
        doc: &DocRef,
        theirs: &str,
        charset: &Option<String>,
        disk: &Disk,
    ) -> bool {
        let Some(base) = self.merge_base(doc) else {
            return false;
        };
        let Some(edits) = doc.with_text(|ours| merge_edits(&base, ours, theirs)) else {
            return false;
        };
        review_ui::start_review(doc, false);
        if let Err(e) = doc.apply_edits(&edits, Origin::Agent { call: 0 }) {
            tracing::warn!("cannot merge the change of {}: {e}", doc.path().display());
            return false;
        }
        review_ui::recompute(doc);
        // The file holds `theirs`; the buffer holds it plus the unsaved changes, so saving it
        // loses nothing.
        doc.saver.set_loaded(charset.clone(), disk.state());
        self.note_base(doc, theirs);
        true
    }

    /// The text `doc`'s file held when the document last read or wrote it, when it is known:
    /// saved or seen during the turn, or the document's text at the turn's start, and its hash
    /// is the one recorded.
    fn merge_base(&self, doc: &DocRef) -> Option<String> {
        let recorded = doc.saver.disk_hash()?;
        let charset = doc.charset();
        let matches = |text: &str| encoded_hash(text, charset.as_deref()) == Some(recorded);
        if let Some(base) = self.bases.borrow().get(doc.path())
            && matches(base)
        {
            return Some(base.clone());
        }
        let state = self.state.upgrade()?;
        let controller = state.agent_controller()?;
        bridge::turn_start_text(controller, doc.path()).filter(|text| matches(text))
    }

    /// A project source file that is not loaded changed during an agent turn (see the module
    /// docs).
    async fn unloaded_file_changed(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        path: &Path,
        disk: Disk,
    ) {
        let Some(controller) = state.agent_controller().cloned() else {
            return;
        };
        let rel = review_ui::relative_name(state, path);
        let decoded = match &disk {
            Disk::Present { bytes, .. } => match saver::decode(bytes, None) {
                Ok(decoded) => Some(decoded),
                Err(e) => {
                    tracing::info!("{rel} changed during the turn: {e}");
                    return;
                }
            },
            Disk::Absent => None,
        };
        let snapshot = bridge::turn_snapshot(&controller);
        let (baseline, new_file) = match bridge::turn_start_text(&controller, path) {
            Some(text) => (Some(text), false),
            None => {
                let existed = snapshot.as_ref().is_some_and(|s| s.existed.contains(path));
                let known_new = snapshot.as_ref().is_some_and(|s| s.complete) && !existed;
                if known_new {
                    (Some(String::new()), true)
                } else {
                    let root = state.workspace.root().path().to_path_buf();
                    (git_head_text(root, rel.clone()).await, false)
                }
            }
        };
        let Some(baseline) = baseline else {
            if decoded.is_some() {
                state.toast(&format!("{rel} changed outside review"));
            }
            return;
        };
        let new_text = decoded.as_ref().map_or("", |d| d.text.as_str());
        if (decoded.is_none() && new_file) || baseline == new_text {
            return;
        }
        // Loaded meanwhile (an agent tool, the user): the document decides.
        if let Some(doc) = state.workspace.get(path) {
            self.document_changed(state, &doc, disk, true);
            return;
        }
        tracing::info!("{rel} was changed outside the agent's tools; reviewing the change");
        let charset = decoded.as_ref().and_then(|d| d.charset.clone());
        let doc = state.workspace.insert_closed(path, baseline);
        doc.saver.set_loaded(charset.clone(), DiskState::Unknown);
        review_ui::start_review(&doc, new_file);
        if apply_as_agent(&doc, new_text, charset, disk.state()) {
            bridge::note_file_changed(&controller);
        }
        self.note_base(&doc, new_text);
    }

    /// The documents on the conflict bar. One that lost its unsaved changes since (the user
    /// undid them, or saved over the file) leaves it, and its file is looked at again: a clean
    /// document reloads what the other program wrote.
    fn live_conflicts(self: &Rc<Self>) -> Vec<DocRef> {
        let mut settled = Vec::new();
        let live: Vec<DocRef> = {
            let mut conflicts = self.conflicts.borrow_mut();
            conflicts.retain(|c| match c.doc.upgrade() {
                Some(doc) if doc.is_dirty() => true,
                Some(doc) => {
                    settled.push(doc.path().to_path_buf());
                    false
                }
                None => false,
            });
            conflicts.iter().filter_map(|c| c.doc.upgrade()).collect()
        };
        if !settled.is_empty() {
            self.recheck(settled);
        }
        live
    }

    /// Looks at `paths` again soon (they go through the usual handling).
    fn recheck(self: &Rc<Self>, paths: Vec<PathBuf>) {
        self.pending.borrow_mut().extend(paths);
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_once(move || {
            if let Some(watcher) = weak.upgrade() {
                watcher.timer.take();
                watcher.flush();
            }
        });
        self.timer.replace(Some(id));
    }

    fn update_bar(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let docs = self.live_conflicts();
        let Some(first) = docs.first() else {
            self.bar.revealer.set_reveal_child(false);
            return;
        };
        let name = review_ui::relative_name(&state, first.path());
        let more = match docs.len() - 1 {
            0 => String::new(),
            1 => " (and 1 more file)".to_owned(),
            n => format!(" (and {n} more files)"),
        };
        self.bar.label.set_text(&format!(
            "“{name}” was changed by another program while you have unsaved changes{more}."
        ));
        self.bar.revealer.set_reveal_child(true);
    }

    async fn resolve(self: &Rc<Self>, path: &Path, resolution: Resolution) -> Result<(), String> {
        let doc = self
            .live_conflicts()
            .into_iter()
            .find(|d| d.path() == path)
            .ok_or_else(|| format!("{} has no conflict", path.display()))?;
        self.conflicts
            .borrow_mut()
            .retain(|c| !std::ptr::eq(c.doc.as_ptr(), Rc::as_ptr(&doc)));
        let result = self.resolve_document(&doc, resolution).await;
        self.update_bar();
        result
    }

    async fn resolve_document(&self, doc: &DocRef, resolution: Resolution) -> Result<(), String> {
        let name = doc.display_name();
        let disk = match gio::File::for_path(doc.path()).load_contents_future().await {
            Ok((bytes, etag)) => Disk::Present { bytes, etag },
            Err(e) if e.matches(gio::IOErrorEnum::NotFound) => Disk::Absent,
            Err(e) => return Err(format!("cannot read {name}: {}", e.message())),
        };
        match (resolution, &disk) {
            (Resolution::KeepMine, _) => {
                // The next save overwrites what the other program wrote, without asking again.
                doc.saver.set_loaded(doc.charset(), disk.state());
                Ok(())
            }
            (Resolution::Reload, Disk::Absent) => Err(format!("{name} no longer exists")),
            (Resolution::Reload, Disk::Present { bytes, .. }) => {
                let decoded = saver::decode(bytes, doc.charset().as_deref())
                    .map_err(|e| format!("cannot read {name}: {e}"))?;
                replace_text(doc, &decoded.text, Origin::Reload)
                    .map_err(|e| format!("cannot reload {name}: {e}"))?;
                doc.saver.set_loaded(decoded.charset, disk.state());
                doc.mark_saved(doc.version());
                Ok(())
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// True during an agent turn and its grace period (`bridge::in_agent_window`).
fn in_agent_window(state: &WindowState) -> bool {
    state
        .agent_controller()
        .is_some_and(|c| bridge::in_agent_window(c))
}

/// The largest file that is not loaded the monitor reviews: `agent.snapshot_cap_mib`, but at
/// least its default (4 MiB; a cap of 0 only means that the snapshot keeps no texts).
fn review_cap(state: &WindowState) -> u64 {
    u64::from(state.config.agent.snapshot_cap_mib)
        .saturating_mul(1 << 20)
        .max(workspace::DEFAULT_SNAPSHOT_CAP as u64)
}

/// What `file` (at `path`) holds now; `None` when it cannot be read.
async fn read_disk(file: &gio::File, path: &Path) -> Option<Disk> {
    match file.load_contents_future().await {
        Ok((bytes, etag)) => Some(Disk::Present { bytes, etag }),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => Some(Disk::Absent),
        Err(e) => {
            tracing::debug!("cannot read {}: {e}", path.display());
            None
        }
    }
}

/// `fnv1a64` of `text` encoded in `charset` (`None`: UTF-8), as the saver would write it.
fn encoded_hash(text: &str, charset: Option<&str>) -> Option<u64> {
    match charset {
        None => Some(fnv1a64(&[text.as_bytes()])),
        Some(charset) => glib::convert(text.as_bytes(), charset, "UTF-8")
            .ok()
            .map(|(bytes, _)| fnv1a64(&[&bytes[..]])),
    }
}

/// The edits (descending, offsets of `ours`) that bring the changes from `base` to `theirs`
/// into `ours` (a three-way merge whose two sides both started from `base`); `None` when a
/// change of `theirs` overlaps or touches one of `ours`.
fn merge_edits(base: &str, ours: &str, theirs: &str) -> Option<Vec<TextEdit>> {
    let mine = minimal_edits(base, ours);
    let other = minimal_edits(base, theirs);
    let mut out = Vec::with_capacity(other.len());
    for edit in &other {
        let (start, end) = (edit.range.start, edit.range.end);
        if mine
            .iter()
            .any(|m| start <= m.range.end && m.range.start <= end)
        {
            return None;
        }
        let shift: isize = mine
            .iter()
            .filter(|m| m.range.end < start)
            .map(TextEdit::delta)
            .sum();
        out.push(TextEdit {
            range: start.checked_add_signed(shift)?..end.checked_add_signed(shift)?,
            text: edit.text.clone(),
        });
    }
    Some(out)
}

/// True when a component of `path` below `root` is hidden (`.git`, `.build`, `.goutputstream-…`).
fn is_hidden(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    })
}

/// Replaces `doc`'s text by `text` with the smallest edits, as one undoable change.
fn replace_text(doc: &DocRef, text: &str, origin: Origin) -> Result<(), String> {
    let edits = doc.with_text(|current| minimal_edits(current, text));
    doc.apply_edits(&edits, origin).map_err(|e| e.to_string())
}

/// Makes `doc`'s text `text` as an agent edit (`Origin::Agent { call: 0 }`): the review starts
/// (baseline = the text before) and recomputes; the document then matches the file (`disk`).
fn apply_as_agent(doc: &DocRef, text: &str, charset: Option<String>, disk: DiskState) -> bool {
    review_ui::start_review(doc, false);
    if let Err(e) = replace_text(doc, text, Origin::Agent { call: 0 }) {
        tracing::warn!("cannot apply the change of {}: {e}", doc.path().display());
        return false;
    }
    review_ui::recompute(doc);
    doc.saver.set_loaded(charset, disk);
    doc.mark_saved(doc.version());
    true
}

/// The text of `rel` in git's `HEAD` (when the project is in a repository and the file is
/// tracked), read on the jobs thread.
async fn git_head_text(root: PathBuf, rel: String) -> Option<String> {
    gio::spawn_blocking(move || {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("show")
            .arg(format!("HEAD:./{rel}"))
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        saver::decode(&out.stdout, None).ok().map(|d| d.text)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &str, ours: &str, theirs: &str) -> Option<String> {
        let edits = merge_edits(base, ours, theirs)?;
        let mut text = ours.to_owned();
        lemma_core::apply_all(&mut text, &edits);
        Some(text)
    }

    #[test]
    fn three_way_merges_keep_both_sides_or_refuse() {
        let base = "alpha\nbeta\ngamma\n";
        // The user typed in line 1, the other program changed line 3 and added a line.
        assert_eq!(
            merged(
                base,
                "USER alpha\nbeta\ngamma\n",
                "alpha\nbeta\nGAMMA\ndelta\n"
            )
            .as_deref(),
            Some("USER alpha\nbeta\nGAMMA\ndelta\n")
        );
        // The other program changed line 1, the user line 3.
        assert_eq!(
            merged(base, "alpha\nbeta\ngamma!\n", "ALPHA\nbeta\ngamma\n").as_deref(),
            Some("ALPHA\nbeta\ngamma!\n")
        );
        // Nothing changed on the other side.
        assert_eq!(merged(base, "x\n", base).as_deref(), Some("x\n"));
        // The same line on both sides, or a deletion of what the user edited: a conflict.
        assert_eq!(
            merged(base, "alpha\nBETA\ngamma\n", "alpha\nbeta!\ngamma\n"),
            None
        );
        assert_eq!(merged(base, "alpha\nbeta typed\ngamma\n", ""), None);
    }

    #[test]
    fn merge_bases_are_hashed_as_written() {
        assert_eq!(
            encoded_hash("café", None),
            Some(fnv1a64(&["café".as_bytes()]))
        );
        assert_eq!(
            encoded_hash("café", Some("ISO-8859-15")),
            Some(fnv1a64(&[b"caf\xe9".as_slice()]))
        );
    }
}
