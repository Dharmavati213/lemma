//! Local history (WP2.4; QoL P1): content-addressed snapshots of project files.
//!
//! - **Store:** `paths.history_dir()/<fnv(root) as 16 hex>/`, with `objects/<hash>.zst` (ruzstd;
//!   or `objects/<hash>` when compressing does not make it smaller) and `index.jsonl`, one line
//!   per entry: `{"path": "/abs/file.tex", "hash": "<16 hex>", "unix": 1760000000, "reason":
//!   "save" | "turn:<n>" | "reject"}`. The hash is `fnv1a64` of the text's UTF-8 bytes. Every
//!   process that writes the index (appends, and the collection's rewrite) holds an exclusive
//!   lock on `index.lock` meanwhile ([`lock_index`]): several Lemma windows (one process each)
//!   on one project lose no entries (on a file system without locks it writes without one).
//! - **Triggers:** every save of a project document (through [`crate::saver::connect_saved`]),
//!   at most one entry per file per `history.min_interval_s`: a save within that time is written
//!   when the interval is over (with the newest text), so the last version of a burst of saves
//!   is kept; and every turn start ([`record_turn`], called by the bridge), for each project
//!   file (the workspace's documents and the turn snapshot's files). An entry is skipped when
//!   the file's newest entry has the same content.
//! - **Caps:** entries older than `history.max_days` go, then the oldest entries until the
//!   objects take at most `history.max_mib`; collected when the window opens (and when the
//!   root moves), on the jobs thread. The first window of a process also collects the other
//!   projects' directories (a project that is never opened again is trimmed too), and removes
//!   those left empty. Saves still waiting for their interval when the root moves are written
//!   to the old root's store. Nothing is written while `history.enabled` is off, which
//!   the app sets for runs that quit by themselves (`--quit-after-ms`: tests, snapshots,
//!   measurements), so they leave nothing in the user's state directory.
//! - **UI:** [`install`] adds `win.history-restore` ("Restore from History…" in the main menu's
//!   file section): a list of the active document's snapshots, newest first; choosing one
//!   restores it as reviewable hunks ([`restore`]).
//! - **API for M3's timeline:** [`entries`] and [`load`].
//!
//! All file work runs on the jobs thread, one job at a time and in order (so a query sees every
//! entry recorded before it).

use crate::document::DocRef;
use crate::hooks::HookId;
use crate::review_ui;
use crate::window::WindowState;
use crate::workspace::{TurnSnapshot, normalize};
use adw::prelude::*;
use lemma_core::{Origin, fnv1a64};
use lemma_review::minimal_edits;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Why a snapshot was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryReason {
    Save,
    /// Before agent turn `n`.
    Turn(u32),
    /// Before a Reject wrote the file.
    Reject,
}

impl HistoryReason {
    /// The `reason` of an `index.jsonl` line.
    pub fn as_index_str(self) -> String {
        match self {
            Self::Save => "save".to_owned(),
            Self::Turn(n) => format!("turn:{n}"),
            Self::Reject => "reject".to_owned(),
        }
    }

    /// Parses an `index.jsonl` reason.
    pub fn from_index_str(s: &str) -> Option<Self> {
        match s {
            "save" => Some(Self::Save),
            "reject" => Some(Self::Reject),
            _ => s.strip_prefix("turn:")?.parse().ok().map(Self::Turn),
        }
    }

    /// How the restore dialog names it.
    pub fn label(self) -> String {
        match self {
            Self::Save => "Saved".to_owned(),
            Self::Turn(n) => format!("Before agent turn {n}"),
            Self::Reject => "Before a reject".to_owned(),
        }
    }
}

/// One snapshot of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// `fnv1a64` of the snapshot's bytes (its object name).
    pub hash: u64,
    /// When it was taken (seconds since the Unix epoch).
    pub unix: u64,
    pub reason: HistoryReason,
}

/// One line of `index.jsonl`.
#[derive(Serialize, Deserialize)]
struct IndexLine {
    path: PathBuf,
    hash: String,
    unix: u64,
    reason: String,
}

impl IndexLine {
    fn entry(&self) -> Option<HistoryEntry> {
        Some(HistoryEntry {
            path: self.path.clone(),
            hash: u64::from_str_radix(&self.hash, 16).ok()?,
            unix: self.unix,
            reason: HistoryReason::from_index_str(&self.reason)?,
        })
    }
}

/// What the review calls a restore's changes (the banner, the Changes panel).
pub const RESTORE_NAME: &str = "Restore from History";

const INDEX: &str = "index.jsonl";
const OBJECTS: &str = "objects";
/// The file every writer of a project's index locks (see [`lock_index`]).
const LOCK: &str = "index.lock";
/// How long a writer waits for another process's hold on a project's index before it gives up
/// (a collection holds it for a moment; a process that hangs must not stall this one's history
/// for good).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// How long a window's close waits for the local history in all: for the job the jobs thread
/// runs (a write, or a collection, which holds the index's lock), then for the lock if another
/// process holds it. The main thread is blocked meanwhile, and the snapshots not written by then
/// are given up (mostly of a version just saved, which the file itself holds): a few seconds
/// are plenty for writes and collections, which take milliseconds, and more would freeze the
/// window for a copy in the history.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Objects nothing refers to are deleted at start-up once they are this old (younger ones may
/// belong to an entry another window is writing).
const ORPHAN_AGE: Duration = Duration::from_secs(3600);

/// A text to record.
struct Pending {
    path: PathBuf,
    text: String,
    reason: HistoryReason,
    unix: u64,
}

/// Work for the jobs thread, done in order.
enum Job {
    /// Collect garbage and read the newest hash of every file.
    Open {
        dir: PathBuf,
        max_bytes: u64,
        max_age: u64,
    },
    /// Collect garbage in the other projects' directories of `history_dir` (see [`sweep`]).
    Sweep {
        history_dir: PathBuf,
        current: PathBuf,
        max_bytes: u64,
        max_age: u64,
    },
    Write {
        dir: PathBuf,
        entries: Vec<Pending>,
    },
    Entries {
        dir: PathBuf,
        path: PathBuf,
        reply: async_channel::Sender<Vec<HistoryEntry>>,
    },
    /// Resolves after every job before it.
    Barrier(async_channel::Sender<()>),
}

/// The local history of one window's project.
struct Store {
    state: Weak<WindowState>,
    /// The project's directory in the history store; `None` without a home directory.
    dir: RefCell<Option<PathBuf>>,
    /// The newest written entry's hash per file (kept by the job runner).
    newest: RefCell<HashMap<PathBuf, u64>>,
    /// When each file last got (or was queued) a save entry.
    last_save: RefCell<HashMap<PathBuf, u64>>,
    /// Saves within the interval: the newest text, written when the interval is over.
    waiting: RefCell<HashMap<PathBuf, (String, glib::SourceId)>>,
    jobs: async_channel::Sender<Job>,
    /// The other end of `jobs`, for [`Store::close`]: it writes what is still queued itself.
    queue: async_channel::Receiver<Job>,
    /// How many jobs that take the index's lock (writes and collections) the jobs thread is
    /// running now.
    busy: Arc<AtomicUsize>,
    saved_hook: Cell<Option<HookId>>,
}

/// Counts a job of the jobs thread in [`Store::busy`] until it ends.
struct Busy(Arc<AtomicUsize>);

impl Busy {
    /// Counted from here, on the main thread, so that a close never misses it.
    fn start(store: &Weak<Store>) -> Option<Self> {
        let busy = store.upgrade()?.busy.clone();
        busy.fetch_add(1, Ordering::SeqCst);
        Some(Self(busy))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

thread_local! {
    static STORES: RefCell<Vec<Weak<Store>>> = const { RefCell::new(Vec::new()) };
}

fn store_of(state: &WindowState) -> Option<Rc<Store>> {
    STORES.with(|s| {
        s.borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|s| std::ptr::eq(s.state.as_ptr(), state))
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The history directory of the project at `root`.
pub fn project_dir(history_dir: &Path, root: &Path) -> PathBuf {
    history_dir.join(format!(
        "{:016x}",
        fnv1a64(&[root.as_os_str().as_encoded_bytes()])
    ))
}

/// Called once per window after the agent controller exists (see the module docs).
pub fn install(state: &Rc<WindowState>) {
    let (jobs, queue) = async_channel::unbounded();
    let store = Rc::new(Store {
        state: Rc::downgrade(state),
        dir: RefCell::new(None),
        newest: RefCell::new(HashMap::new()),
        last_save: RefCell::new(HashMap::new()),
        waiting: RefCell::new(HashMap::new()),
        jobs,
        queue: queue.clone(),
        busy: Arc::default(),
        saved_hook: Cell::new(None),
    });
    STORES.with(|s| {
        let mut stores = s.borrow_mut();
        stores.retain(|s| s.strong_count() > 0);
        stores.push(Rc::downgrade(&store));
    });
    glib::spawn_future_local(run_jobs(Rc::downgrade(&store), queue));
    let weak = Rc::downgrade(&store);
    store
        .saved_hook
        .set(Some(crate::saver::connect_saved(move |path, text| {
            if let Some(store) = weak.upgrade() {
                store.saved(path, text);
            }
        })));
    let weak = Rc::downgrade(&store);
    state.window.connect_destroy(move |_| {
        if let Some(store) = weak.upgrade() {
            store.close();
        }
    });
    install_action(state);
    store.open();
    store.sweep_once();
    // The window owns the store through this hook.
    let owner = store;
    state.connect_root_changed(move |_| owner.open());
}

fn install_action(state: &Rc<WindowState>) {
    let action = gio::SimpleAction::new("history-restore", None);
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, _| {
        if let Some(state) = weak.upgrade() {
            glib::spawn_future_local(show_restore_dialog(state));
        }
    });
    state.window.add_action(&action);
    state
        .header
        .menu_files
        .append(Some("Restore from _History…"), Some("win.history-restore"));
}

/// The snapshots of `path` in `state`'s project, newest first.
pub fn entries(
    state: &Rc<WindowState>,
    path: &Path,
) -> impl Future<Output = Vec<HistoryEntry>> + use<> {
    let store = store_of(state);
    let path = normalize(path);
    async move {
        let Some(store) = store else {
            return Vec::new();
        };
        let Some(dir) = store.dir.borrow().clone() else {
            return Vec::new();
        };
        let (reply, answer) = async_channel::bounded(1);
        if store
            .jobs
            .try_send(Job::Entries { dir, path, reply })
            .is_err()
        {
            return Vec::new();
        }
        answer.recv().await.unwrap_or_default()
    }
}

/// The text of a snapshot.
pub fn load(
    state: &Rc<WindowState>,
    entry: &HistoryEntry,
) -> impl Future<Output = Result<String, String>> + use<> {
    let dir = store_of(state).and_then(|s| s.dir.borrow().clone());
    let hash = entry.hash;
    async move {
        let dir = dir.ok_or("local history is not available (no home directory)")?;
        gio::spawn_blocking(move || read_object(&dir, hash))
            .await
            .map_err(|_| "reading the snapshot failed".to_owned())?
    }
}

/// The snapshot in `state`'s project history whose text hashes to `hash` (`fnv1a64` of its
/// UTF-8 bytes, as review states and documents hash their texts), of whatever file: `None` when
/// the history does not have it.
pub fn text_with_hash(
    state: &WindowState,
    hash: u64,
) -> impl Future<Output = Option<String>> + use<> {
    let dir = store_of(state).and_then(|s| s.dir.borrow().clone());
    async move {
        let dir = dir?;
        gio::spawn_blocking(move || read_object(&dir, hash).ok())
            .await
            .ok()
            .flatten()
    }
}

/// Records `text` as the content of `path` now, unless the file's newest entry has the same
/// content (for other modules: e.g. `reason = Reject` before a reject writes the file).
pub fn record(state: &Rc<WindowState>, path: &Path, text: &str, reason: HistoryReason) {
    if let Some(store) = store_of(state) {
        store.enqueue(vec![Pending {
            path: normalize(path),
            text: text.to_owned(),
            reason,
            unix: unix_now(),
        }]);
    }
}

/// The turn-start trigger: records every project file (the workspace's documents and the
/// snapshot's files) whose content differs from its newest entry, with `reason = Turn(turn)`.
pub fn record_turn(state: &Rc<WindowState>, turn: u32, snapshot: &TurnSnapshot) {
    let Some(store) = store_of(state) else {
        return;
    };
    let root = state.workspace.root().path().to_path_buf();
    let unix = unix_now();
    let mut entries: Vec<Pending> = state
        .workspace
        .documents()
        .into_iter()
        .filter(|d| d.path().starts_with(&root))
        .map(|d| Pending {
            path: d.path().to_path_buf(),
            text: d.text(),
            reason: HistoryReason::Turn(turn),
            unix,
        })
        .collect();
    entries.extend(snapshot.files.iter().map(|(path, text)| Pending {
        path: path.clone(),
        text: text.clone(),
        reason: HistoryReason::Turn(turn),
        unix,
    }));
    store.enqueue(entries);
}

/// Resolves once every recording asked for so far is on disk (tests).
pub fn flush(state: &Rc<WindowState>) -> impl Future<Output = ()> + use<> {
    let store = store_of(state);
    async move {
        let Some(store) = store else {
            return;
        };
        let (done, wait) = async_channel::bounded(1);
        if store.jobs.try_send(Job::Barrier(done)).is_ok() {
            let _ = wait.recv().await;
        }
    }
}

/// Writes every save still waiting for its interval now (tests).
pub fn write_waiting(state: &Rc<WindowState>) {
    if let Some(store) = store_of(state) {
        store.write_waiting();
    }
}

/// What the window's `destroy` runs (tests): see [`Store::close`].
pub fn close(state: &Rc<WindowState>) {
    if let Some(store) = store_of(state) {
        store.close();
    }
}

/// Restores `entry` into `doc` as reviewable hunks: the review's baseline is the current text
/// (unless a review already runs), the old text is applied with `Origin::Agent { call: 0 }` as
/// a named change apart from the agent's turns ([`review_ui::begin_named_change`], named
/// [`RESTORE_NAME`]) and saved. Resolves to the number of hunks of the document afterwards.
pub fn restore(
    state: &Rc<WindowState>,
    doc: &DocRef,
    entry: &HistoryEntry,
) -> impl Future<Output = Result<usize, String>> + use<> {
    let text = load(state, entry);
    let (state, doc) = (state.clone(), doc.clone());
    async move {
        let old = text.await?;
        // Computed and applied without yielding, so the text cannot change in between.
        let edits = doc.with_text(|current| minimal_edits(current, &old));
        if edits.is_empty() {
            return Ok(review_ui::hunk_count(&doc));
        }
        review_ui::start_review(&doc, false);
        // The user's restore, not the agent's change: the banner and the Changes panel name it,
        // and Undo Last Turn leaves it alone.
        let turn = review_ui::begin_named_change(&state, RESTORE_NAME);
        review_ui::in_turn(&doc, turn, || {
            doc.apply_edits(&edits, Origin::Agent { call: 0 })
        })
        .map_err(|e| format!("cannot restore {}: {e}", doc.display_name()))?;
        review_ui::recompute(&doc);
        doc.save()
            .await
            .map_err(|e| format!("restored, but saving {} failed: {e}", doc.display_name()))?;
        Ok(review_ui::hunk_count(&doc))
    }
}

impl Store {
    /// (Re)opens the store of the current root: garbage collection and the newest hashes.
    fn open(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        // Saves waiting for their interval belong to the old root's store.
        self.write_waiting();
        let dir = state
            .paths
            .as_ref()
            .map(|p| project_dir(&p.history_dir(), state.workspace.root().path()));
        self.dir.replace(dir.clone());
        self.newest.borrow_mut().clear();
        self.last_save.borrow_mut().clear();
        let cfg = &state.config.history;
        if let Some(dir) = dir
            && cfg.enabled
        {
            let _ = self.jobs.try_send(Job::Open {
                dir,
                max_bytes: u64::from(cfg.max_mib) << 20,
                max_age: u64::from(cfg.max_days) * 86_400,
            });
        }
    }

    /// Once per process (and history directory): collects the other projects' directories too
    /// (see the module docs).
    fn sweep_once(&self) {
        thread_local! {
            /// The history directories collected so far.
            static SWEPT: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let cfg = &state.config.history;
        let (Some(paths), Some(current)) = (state.paths.as_ref(), self.dir.borrow().clone()) else {
            return;
        };
        let history_dir = paths.history_dir();
        if !cfg.enabled || !SWEPT.with(|s| s.borrow_mut().insert(history_dir.clone())) {
            return;
        }
        let _ = self.jobs.try_send(Job::Sweep {
            history_dir,
            current,
            max_bytes: u64::from(cfg.max_mib) << 20,
            max_age: u64::from(cfg.max_days) * 86_400,
        });
    }

    fn enabled(&self) -> bool {
        self.dir.borrow().is_some()
            && self
                .state
                .upgrade()
                .is_some_and(|s| s.config.history.enabled)
    }

    fn enqueue(&self, entries: Vec<Pending>) {
        if entries.is_empty() || !self.enabled() {
            return;
        }
        let Some(dir) = self.dir.borrow().clone() else {
            return;
        };
        let _ = self.jobs.try_send(Job::Write { dir, entries });
    }

    /// A document was saved (the `connect_saved` hook).
    fn saved(self: &Rc<Self>, path: &Path, text: &str) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if !self.enabled()
            || !path.starts_with(state.workspace.root().path())
            || state.workspace.get(path).is_none()
        {
            return;
        }
        let now = unix_now();
        let interval = u64::from(state.config.history.min_interval_s);
        let last = self.last_save.borrow().get(path).copied();
        match last {
            Some(last) if now < last.saturating_add(interval) => {
                let mut waiting = self.waiting.borrow_mut();
                if let Some((waiting_text, _)) = waiting.get_mut(path) {
                    // The timer is running: it writes the newest text.
                    *waiting_text = text.to_owned();
                    return;
                }
                let delay = Duration::from_secs(last.saturating_add(interval) - now);
                let weak = Rc::downgrade(self);
                let key = path.to_path_buf();
                let id = glib::timeout_add_local_once(delay, move || {
                    if let Some(store) = weak.upgrade() {
                        let waiting = store.waiting.borrow_mut().remove(&key);
                        if let Some((text, _id)) = waiting {
                            store.write_save(&key, text);
                        }
                    }
                });
                waiting.insert(path.to_path_buf(), (text.to_owned(), id));
            }
            _ => {
                if let Some((_, id)) = self.waiting.borrow_mut().remove(path) {
                    id.remove();
                }
                self.write_save(path, text.to_owned());
            }
        }
    }

    fn write_save(&self, path: &Path, text: String) {
        let unix = unix_now();
        self.last_save.borrow_mut().insert(path.to_path_buf(), unix);
        self.enqueue(vec![Pending {
            path: path.to_path_buf(),
            text,
            reason: HistoryReason::Save,
            unix,
        }]);
    }

    fn write_waiting(&self) {
        let waiting: Vec<(PathBuf, (String, glib::SourceId))> =
            self.waiting.borrow_mut().drain().collect();
        for (path, (text, id)) in waiting {
            id.remove();
            self.write_save(&path, text);
        }
    }

    /// The window closes: the snapshots still queued for the jobs thread, and the saves still
    /// waiting for their interval, are written now (the main loop may not run again: the save
    /// right before the window closed would otherwise leave no snapshot), within [`CLOSE_WAIT`].
    fn close(&self) {
        if let Some(id) = self.saved_hook.take() {
            crate::saver::disconnect_saved(id);
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        // A job the jobs thread runs now ends first (the process may end right after this): a
        // write, or a collection, which holds the index's lock.
        while self.busy.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut newest = self.newest.borrow().clone();
        while let Ok(job) = self.queue.try_recv() {
            // The other jobs only read, or collect garbage, which the next window does again.
            if let Job::Write { dir, entries } = job {
                match write_entries_until(&dir, entries, &newest, Some(deadline)) {
                    Ok(written) => {
                        for entry in written {
                            newest.insert(entry.path, entry.hash);
                        }
                    }
                    Err(e) => tracing::warn!("local history: {e}"),
                }
            }
        }
        let waiting: Vec<(PathBuf, (String, glib::SourceId))> =
            self.waiting.borrow_mut().drain().collect();
        let Some(dir) = self.dir.borrow().clone() else {
            return;
        };
        let entries: Vec<Pending> = waiting
            .into_iter()
            .map(|(path, (text, id))| {
                id.remove();
                Pending {
                    path,
                    text,
                    reason: HistoryReason::Save,
                    unix: unix_now(),
                }
            })
            .collect();
        if !entries.is_empty()
            && self.enabled()
            && let Err(e) = write_entries_until(&dir, entries, &newest, Some(deadline))
        {
            tracing::warn!("local history: {e}");
        }
    }
}

/// Runs the jobs in order (a main-thread task; the work itself runs on the jobs thread).
async fn run_jobs(store: Weak<Store>, queue: async_channel::Receiver<Job>) {
    while let Ok(job) = queue.recv().await {
        match job {
            Job::Open {
                dir,
                max_bytes,
                max_age,
            } => {
                let busy = Busy::start(&store);
                let opened = gio::spawn_blocking(move || {
                    let _busy = busy;
                    open_dir(&dir, max_bytes, max_age, unix_now())
                })
                .await;
                match (store.upgrade(), opened) {
                    (Some(store), Ok(Ok(newest))) => {
                        let mut known = store.newest.borrow_mut();
                        for (path, hash) in newest {
                            known.entry(path).or_insert(hash);
                        }
                    }
                    (_, Ok(Err(e))) => tracing::warn!("local history: {e}"),
                    _ => {}
                }
            }
            Job::Sweep {
                history_dir,
                current,
                max_bytes,
                max_age,
            } => {
                let busy = Busy::start(&store);
                let _ = gio::spawn_blocking(move || {
                    let _busy = busy;
                    sweep(&history_dir, &current, max_bytes, max_age, unix_now());
                })
                .await;
            }
            Job::Write { dir, entries } => {
                let Some(known) = store.upgrade().map(|s| s.newest.borrow().clone()) else {
                    return;
                };
                let busy = Busy::start(&store);
                let written = gio::spawn_blocking(move || {
                    let _busy = busy;
                    write_entries(&dir, entries, &known)
                })
                .await;
                match (store.upgrade(), written) {
                    (Some(store), Ok(Ok(written))) => {
                        let mut newest = store.newest.borrow_mut();
                        for entry in written {
                            newest.insert(entry.path, entry.hash);
                        }
                    }
                    (_, Ok(Err(e))) => tracing::warn!("local history: {e}"),
                    _ => {}
                }
            }
            Job::Entries { dir, path, reply } => {
                let entries = gio::spawn_blocking(move || read_entries(&dir, &path))
                    .await
                    .unwrap_or_default();
                let _ = reply.try_send(entries);
            }
            Job::Barrier(done) => {
                let _ = done.try_send(());
            }
        }
    }
}

// ----- the store on disk (blocking; jobs thread) -----

fn object_path(dir: &Path, hash: u64, compressed: bool) -> PathBuf {
    let name = if compressed {
        format!("{hash:016x}.zst")
    } else {
        format!("{hash:016x}")
    };
    dir.join(OBJECTS).join(name)
}

/// The lines of `dir`'s index (none when it has none). A line that does not parse is left out,
/// and so is a byte that is not UTF-8 (an append cut by a full disk): one damaged line must not
/// hide the others, whose objects the collection would then take for garbage.
fn read_index(dir: &Path) -> std::io::Result<Vec<IndexLine>> {
    let bytes = match std::fs::read(dir.join(INDEX)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<IndexLine>(line).ok())
        .filter(|line| line.entry().is_some())
        .collect())
}

/// Writes `bytes` to `path` through a private temporary file, flushed to the disk before it is
/// renamed: after a crash of the machine an object is whole or missing (one left empty would
/// never be written again, its name being its hash) and the index is the old one or the new.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    crate::fsutil::private_file_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_data()
        })
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// The lock on a project's index that [`lock_index`] took (none on a file system without locks);
/// released when dropped.
struct IndexLock {
    _file: Option<std::fs::File>,
}

/// Takes the lock on `dir`'s index (an exclusive `File::try_lock` of `index.lock`: `flock`,
/// `LockFileEx`), creating the directory if needed; waits up to [`LOCK_WAIT`] while another
/// process (another Lemma on the same project) holds it. Every writer of the index takes it:
/// appends ([`write_entries`]) and the collection's rewrite ([`open_dir`], [`sweep`]) never
/// overlap. Without it, a line appended while the collection rewrote the index went to the
/// index the rename replaced (the entry was lost), and an object an append found and referred
/// to could be deleted as garbage meanwhile. Blocking.
fn lock_index(dir: &Path) -> Result<IndexLock, String> {
    lock_index_until(dir, Instant::now() + LOCK_WAIT)
}

/// [`lock_index`], waiting for another process's hold until `deadline` (once it has passed, the
/// lock is tried once). On a file system that has no locks (an NFS home without its lock daemon
/// answers `ENOLCK`; some network and FUSE file systems have none) the index is written without
/// the lock, as it was before there was one, rather than not at all: the history is kept, and
/// only two Lemmas on one project there can lose an entry, as they could then.
fn lock_index_until(dir: &Path, deadline: Instant) -> Result<IndexLock, String> {
    let path = dir.join(LOCK);
    loop {
        // The texts of the user's documents: private, as the review state and the recovery
        // copies are (a project directory made before it was private is made so).
        super::tools::create_private_dir(dir)?;
        let file = loop {
            let opened = crate::fsutil::private_file_options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path);
            match opened {
                Ok(file) => break file,
                Err(e) if open_may_pass(&e) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
            }
        };
        loop {
            match try_lock(&file) {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "{} is held by another Lemma; nothing was written",
                        path.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    static TOLD: AtomicBool = AtomicBool::new(false);
                    if !TOLD.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "local history: cannot lock {} ({e}); writing without the lock",
                            path.display()
                        );
                    }
                    return Ok(IndexLock { _file: None });
                }
            }
        }
        // The sweep of another process removes the lock file of a project left empty before it
        // lets go: a lock on that file guards nothing, so the one there now is taken.
        if is_same_file(&file, &path) {
            return Ok(IndexLock { _file: Some(file) });
        }
    }
}

/// Whether the lock file could not be opened for a reason that passes in a moment: on Windows a
/// lock file that another process removed stays, and cannot be opened, until its last handle is
/// closed.
fn open_may_pass(e: &std::io::Error) -> bool {
    cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied
}

/// `file.try_lock()`; tests make it fail as a file system without locks does.
fn try_lock(file: &std::fs::File) -> Result<(), std::fs::TryLockError> {
    #[cfg(test)]
    if tests::UNSUPPORTED_LOCKS.get() {
        return Err(std::fs::TryLockError::Error(tests::no_locks_error()));
    }
    file.try_lock()
}

/// Whether `path` still names the file `file` was opened from.
fn is_same_file(file: &std::fs::File, path: &Path) -> bool {
    let Ok(now) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        file.metadata()
            .is_ok_and(|m| (m.dev(), m.ino()) == (now.dev(), now.ino()))
    }
    #[cfg(not(unix))]
    {
        // No stable file identity in std here: a lock file removed and made again by a third
        // process in the moment this one waited is not told apart.
        let _ = (file, now);
        true
    }
}

/// Appends the entries whose text differs from the file's newest entry (`known`, then the
/// entries before it in this batch); returns those written.
fn write_entries(
    dir: &Path,
    entries: Vec<Pending>,
    known: &HashMap<PathBuf, u64>,
) -> Result<Vec<HistoryEntry>, String> {
    write_entries_until(dir, entries, known, None)
}

/// [`write_entries`], given up at `deadline` (the window closes, see [`Store::close`]): the lock
/// is waited for until then, and the entries not written by then are left out.
fn write_entries_until(
    dir: &Path,
    entries: Vec<Pending>,
    known: &HashMap<PathBuf, u64>,
    deadline: Option<Instant>,
) -> Result<Vec<HistoryEntry>, String> {
    let _lock = lock_index_until(dir, deadline.unwrap_or_else(|| Instant::now() + LOCK_WAIT))?;
    let objects = dir.join(OBJECTS);
    crate::fsutil::private_dir_builder()
        .create(&objects)
        .map_err(|e| format!("cannot create {}: {e}", objects.display()))?;
    let mut newest = known.clone();
    let mut lines = String::new();
    let mut written = Vec::new();
    let mut left_out = 0;
    for pending in entries {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            left_out += 1;
            continue;
        }
        let hash = fnv1a64(&[pending.text.as_bytes()]);
        if newest.get(&pending.path) == Some(&hash) {
            continue;
        }
        if !object_path(dir, hash, true).exists() && !object_path(dir, hash, false).exists() {
            let compressed = ruzstd::encoding::compress_to_vec(
                pending.text.as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            let (path, bytes) = if compressed.len() < pending.text.len() {
                (object_path(dir, hash, true), compressed.as_slice())
            } else {
                (object_path(dir, hash, false), pending.text.as_bytes())
            };
            write_atomic(&path, bytes)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        let line = IndexLine {
            path: pending.path.clone(),
            hash: format!("{hash:016x}"),
            unix: pending.unix,
            reason: pending.reason.as_index_str(),
        };
        let Ok(json) = serde_json::to_string(&line) else {
            // A path that is not UTF-8 cannot be recorded.
            continue;
        };
        lines.push_str(&json);
        lines.push('\n');
        newest.insert(pending.path.clone(), hash);
        written.push(HistoryEntry {
            path: pending.path,
            hash,
            unix: pending.unix,
            reason: pending.reason,
        });
    }
    if left_out > 0 {
        tracing::warn!("local history: {left_out} snapshots not written before the window closed");
    }
    if !lines.is_empty() {
        // One write of whole lines with O_APPEND: other windows' appends do not interleave.
        crate::fsutil::private_file_options()
            .create(true)
            .append(true)
            .read(true)
            .open(dir.join(INDEX))
            .and_then(|mut f| {
                // An append cut short (a full disk) left no line break: the first new line would
                // continue the damaged one, and be lost with it.
                if !ends_with_line_break(&mut f)? {
                    lines.insert(0, '\n');
                }
                f.write_all(lines.as_bytes())
            })
            .map_err(|e| format!("cannot write the history index: {e}"))?;
    }
    Ok(written)
}

/// Whether `file` is empty or ends with `\n`.
fn ends_with_line_break(file: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::{Seek, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Garbage collection (age, then size, then unreferenced objects) and the newest hash of every
/// file, holding the index's lock ([`lock_index`]).
fn open_dir(
    dir: &Path,
    max_bytes: u64,
    max_age: u64,
    now: u64,
) -> Result<HashMap<PathBuf, u64>, String> {
    if !dir.exists() {
        return Ok(HashMap::new());
    }
    let _lock = lock_index(dir)?;
    collect(dir, max_bytes, max_age, now)
}

/// [`open_dir`] while its caller holds the index's lock.
fn collect(
    dir: &Path,
    max_bytes: u64,
    max_age: u64,
    now: u64,
) -> Result<HashMap<PathBuf, u64>, String> {
    // An index that cannot be read leaves everything as it is (its objects are not garbage).
    let lines =
        read_index(dir).map_err(|e| format!("cannot read the index of {}: {e}", dir.display()))?;
    let total_lines = lines.len();
    let indexed: HashSet<String> = lines.iter().map(|l| l.hash.clone()).collect();
    let mut keep: Vec<IndexLine> = lines
        .into_iter()
        .filter(|l| l.unix.saturating_add(max_age) >= now)
        .collect();
    keep.sort_by_key(|l| l.unix);
    let size_of = |hash: &str| -> u64 {
        let Ok(hash) = u64::from_str_radix(hash, 16) else {
            return 0;
        };
        [true, false]
            .iter()
            .find_map(|&c| std::fs::metadata(object_path(dir, hash, c)).ok())
            .map_or(0, |m| m.len())
    };
    let mut refs: HashMap<String, usize> = HashMap::new();
    for line in &keep {
        *refs.entry(line.hash.clone()).or_default() += 1;
    }
    let sizes: HashMap<String, u64> = refs.keys().map(|h| (h.clone(), size_of(h))).collect();
    let mut total: u64 = sizes.values().sum();
    let mut drop_count = 0;
    while total > max_bytes && drop_count < keep.len() {
        let line = &keep[drop_count];
        let count = refs.get_mut(&line.hash).expect("counted above");
        *count -= 1;
        if *count == 0 {
            total = total.saturating_sub(sizes.get(&line.hash).copied().unwrap_or(0));
        }
        drop_count += 1;
    }
    keep.drain(..drop_count);
    if keep.len() != total_lines {
        let mut text = String::new();
        for line in &keep {
            if let Ok(json) = serde_json::to_string(line) {
                text.push_str(&json);
                text.push('\n');
            }
        }
        write_atomic(&dir.join(INDEX), text.as_bytes())
            .map_err(|e| format!("cannot rewrite the history index: {e}"))?;
    }
    // Objects of dropped entries go now; other unreferenced ones (a crash between writing an
    // object and its index line, temporary files) once they are old, since a young one may
    // belong to an entry another window is writing.
    let referenced: HashSet<&str> = keep.iter().map(|l| l.hash.as_str()).collect();
    if let Ok(objects) = std::fs::read_dir(dir.join(OBJECTS)) {
        for object in objects.flatten() {
            let name = object.file_name().to_string_lossy().into_owned();
            let hash = name.strip_suffix(".zst").unwrap_or(&name);
            if referenced.contains(hash) {
                continue;
            }
            let old = object
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > ORPHAN_AGE));
            if old || indexed.contains(hash) {
                let _ = std::fs::remove_file(object.path());
            }
        }
    }
    let mut newest = HashMap::new();
    for line in keep {
        if let Some(entry) = line.entry() {
            newest.insert(entry.path, entry.hash);
        }
    }
    Ok(newest)
}

/// Collects garbage ([`open_dir`]) in every project directory of `history_dir` but `current`,
/// and removes those left without entries and objects. Only directories named as
/// [`project_dir`] names them are touched.
fn sweep(history_dir: &Path, current: &Path, max_bytes: u64, max_age: u64, now: u64) {
    let Ok(dirs) = std::fs::read_dir(history_dir) else {
        return;
    };
    for dir in dirs.flatten() {
        let path = dir.path();
        let name = dir.file_name();
        let ours = name
            .to_str()
            .is_some_and(|n| n.len() == 16 && n.bytes().all(|b| b.is_ascii_hexdigit()));
        if !ours || path == current || !dir.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        // Another Lemma may have that project open and write to it meanwhile.
        let lock = match lock_index(&path) {
            Ok(lock) => lock,
            Err(e) => {
                tracing::warn!("local history: {e}");
                continue;
            }
        };
        if let Err(e) = collect(&path, max_bytes, max_age, now) {
            tracing::warn!("local history: {e}");
            continue;
        }
        let empty_index = std::fs::metadata(path.join(INDEX)).map_or(true, |m| m.len() == 0);
        let no_objects =
            std::fs::read_dir(path.join(OBJECTS)).map_or(true, |mut o| o.next().is_none());
        if empty_index && no_objects {
            let _ = std::fs::remove_file(path.join(INDEX));
            let _ = std::fs::remove_dir(path.join(OBJECTS));
            // The lock file goes while it is held (a writer that waited for it then takes a new
            // one, see `lock_index`), and the directory once it is let go (a file open there
            // keeps it from going on Windows).
            let _ = std::fs::remove_file(path.join(LOCK));
            drop(lock);
            if std::fs::remove_dir(&path).is_ok() {
                tracing::info!("local history: removed the empty {}", path.display());
            }
        }
    }
}

fn read_entries(dir: &Path, path: &Path) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = read_index(dir)
        .unwrap_or_default()
        .iter()
        .filter_map(IndexLine::entry)
        .filter(|e| e.path == path)
        .collect();
    // Newest first; entries of the same second keep their order reversed.
    entries.reverse();
    entries.sort_by_key(|e| std::cmp::Reverse(e.unix));
    entries
}

fn read_object(dir: &Path, hash: u64) -> Result<String, String> {
    let bytes = match std::fs::read(object_path(dir, hash, true)) {
        Ok(compressed) => {
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice())
                .map_err(|e| format!("the snapshot is damaged: {e}"))?;
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|e| format!("the snapshot is damaged: {e}"))?;
            out
        }
        Err(_) => std::fs::read(object_path(dir, hash, false))
            .map_err(|e| format!("the snapshot is missing: {e}"))?,
    };
    if fnv1a64(&[&bytes]) != hash {
        return Err("the snapshot is damaged (its hash does not match)".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "the snapshot is damaged (not UTF-8)".to_owned())
}

// ----- the restore dialog -----

fn format_time(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|u| glib::DateTime::from_unix_local(u).ok())
        .and_then(|t| t.format("%e %b %Y, %H:%M:%S").ok())
        .map_or_else(|| unix.to_string(), |s| s.trim().to_owned())
}

async fn show_restore_dialog(state: Rc<WindowState>) {
    let Some(editor) = state.active_editor() else {
        state.toast("Open a document to restore it from its history");
        return;
    };
    let doc = editor.document().clone();
    if store_of(&state).is_none_or(|s| s.dir.borrow().is_none()) {
        state.toast("Local history is not available (no home directory)");
        return;
    }
    let list = entries(&state, doc.path()).await;
    let name = doc.display_name();
    let dialog = adw::Dialog::builder()
        .title(format!("Restore {name}"))
        .content_width(460)
        .content_height(520)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    if list.is_empty() {
        // The description is Pango markup; a file name may hold `&` or `<`.
        let empty = adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No History Yet")
            .description(format!(
                "Lemma keeps a copy of {} when it is saved and before every agent turn.",
                glib::markup_escape_text(&name)
            ))
            .build();
        toolbar.set_content(Some(&empty));
    } else {
        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for entry in &list {
            let row = adw::ActionRow::builder()
                .title(format_time(entry.unix))
                .subtitle(entry.reason.label())
                .use_markup(false)
                .activatable(true)
                .build();
            let (state, doc, entry, dialog) = (
                Rc::downgrade(&state),
                Rc::downgrade(&doc),
                entry.clone(),
                dialog.downgrade(),
            );
            row.connect_activated(move |_| {
                let (Some(state), Some(doc)) = (state.upgrade(), doc.upgrade()) else {
                    return;
                };
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                let entry = entry.clone();
                glib::spawn_future_local(async move {
                    match restore(&state, &doc, &entry).await {
                        Ok(0) => state.toast(&format!(
                            "{} already has the text of {}",
                            doc.display_name(),
                            format_time(entry.unix)
                        )),
                        Ok(_) => state.toast(&format!(
                            "Restored {} from {}: review the changes",
                            doc.display_name(),
                            format_time(entry.unix)
                        )),
                        Err(e) => state.toast(&e),
                    }
                });
            });
            rows.append(&row);
        }
        let clamp = adw::Clamp::builder()
            .child(&rows)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        toolbar.set_content(Some(&scroller));
    }
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(&state.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Locks fail on this thread as on a file system without them (see [`try_lock`]).
        pub(super) static UNSUPPORTED_LOCKS: Cell<bool> = const { Cell::new(false) };
    }

    /// What `flock` says on an NFS mount whose lock daemon does not run.
    pub(super) fn no_locks_error() -> std::io::Error {
        #[cfg(unix)]
        {
            std::io::Error::from_raw_os_error(libc::ENOLCK)
        }
        #[cfg(not(unix))]
        {
            std::io::ErrorKind::Unsupported.into()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = crate::fsutil::private_temp_dir(&format!("lemma-history-{name}-")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pending(path: &str, text: &str, unix: u64) -> Pending {
        Pending {
            path: PathBuf::from(path),
            text: text.to_owned(),
            reason: HistoryReason::Save,
            unix,
        }
    }

    #[test]
    fn reasons_round_trip() {
        for reason in [
            HistoryReason::Save,
            HistoryReason::Reject,
            HistoryReason::Turn(17),
        ] {
            assert_eq!(
                HistoryReason::from_index_str(&reason.as_index_str()),
                Some(reason)
            );
        }
        assert_eq!(HistoryReason::from_index_str("turn:x"), None);
        assert_eq!(HistoryReason::from_index_str("other"), None);
    }

    #[test]
    fn entries_are_content_addressed_and_deduplicated() {
        let dir = temp("write");
        let big = "\\section{A}\n".repeat(200);
        let written = write_entries(
            &dir,
            vec![
                pending("/p/a.tex", &big, 10),
                pending("/p/a.tex", &big, 11),
                pending("/p/b.tex", "x", 12),
                pending("/p/a.tex", "short", 13),
            ],
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(written.len(), 3, "the repeated text is skipped");
        let hash = fnv1a64(&[big.as_bytes()]);
        assert!(
            object_path(&dir, hash, true).exists(),
            "repetitive text is compressed"
        );
        assert!(
            object_path(&dir, fnv1a64(&[b"x"]), false).exists(),
            "a tiny text is stored plain"
        );
        assert_eq!(read_object(&dir, hash).unwrap(), big);
        assert_eq!(read_object(&dir, fnv1a64(&[b"x"])).unwrap(), "x");
        let entries = read_entries(&dir, Path::new("/p/a.tex"));
        assert_eq!(
            entries.iter().map(|e| e.unix).collect::<Vec<_>>(),
            [13, 10],
            "newest first"
        );
        // Known newest hashes skip unchanged files.
        let known = HashMap::from([(PathBuf::from("/p/b.tex"), fnv1a64(&[b"x"]))]);
        assert!(
            write_entries(&dir, vec![pending("/p/b.tex", "x", 20)], &known)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_collection_by_age_and_size() {
        let dir = temp("gc");
        let day = 86_400;
        let texts: Vec<String> = (0..4).map(|i| format!("{i}").repeat(4000)).collect();
        let entries: Vec<Pending> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| pending(&format!("/p/{i}.tex"), t, 100 * day + i as u64 * day))
            .collect();
        write_entries(&dir, entries, &HashMap::new()).unwrap();
        // 30 days from the last entry: all are young enough.
        let newest = open_dir(&dir, u64::MAX, 30 * day, 104 * day).unwrap();
        assert_eq!(newest.len(), 4);
        // Only the last two days count.
        let newest = open_dir(&dir, u64::MAX, 2 * day, 104 * day).unwrap();
        let mut kept: Vec<_> = newest.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, [PathBuf::from("/p/2.tex"), PathBuf::from("/p/3.tex")]);
        assert!(read_object(&dir, fnv1a64(&[texts[0].as_bytes()])).is_err());
        // A size cap smaller than both objects keeps the newest that fits.
        let one = std::fs::metadata(object_path(&dir, fnv1a64(&[texts[3].as_bytes()]), true))
            .unwrap()
            .len();
        let newest = open_dir(&dir, one, 30 * day, 104 * day).unwrap();
        assert_eq!(
            newest.keys().collect::<Vec<_>>(),
            [&PathBuf::from("/p/3.tex")]
        );
        assert_eq!(
            read_object(&dir, fnv1a64(&[texts[3].as_bytes()])).unwrap(),
            texts[3]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweeping_trims_the_other_projects_and_removes_empty_ones() {
        let base = temp("sweep");
        let day = 86_400;
        let (old, young, current) = (
            base.join("00000000000000aa"),
            base.join("00000000000000bb"),
            base.join("00000000000000cc"),
        );
        write_entries(&old, vec![pending("/a/x.tex", "old", day)], &HashMap::new()).unwrap();
        write_entries(
            &young,
            vec![
                pending("/b/x.tex", "gone", day),
                pending("/b/y.tex", "kept", 40 * day),
            ],
            &HashMap::new(),
        )
        .unwrap();
        write_entries(
            &current,
            vec![pending("/c/x.tex", "mine", day)],
            &HashMap::new(),
        )
        .unwrap();
        let unrelated = base.join("not-a-project");
        std::fs::create_dir_all(&unrelated).unwrap();
        sweep(&base, &current, u64::MAX, 30 * day, 50 * day);
        assert!(!old.exists(), "a project with nothing left goes");
        let kept = read_entries(&young, Path::new("/b/y.tex"));
        assert_eq!(kept.len(), 1);
        assert!(read_entries(&young, Path::new("/b/x.tex")).is_empty());
        assert_eq!(
            read_entries(&current, Path::new("/c/x.tex")).len(),
            1,
            "the current project is collected when it is opened, not here"
        );
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The store holds the text of every save: like the review state and the copies of unsaved
    /// changes, only the user can read it, also in a directory made before it was private.
    #[cfg(unix)]
    #[test]
    fn the_store_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = temp("private");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let texts: Vec<String> = (0..2).map(|i| format!("{i}").repeat(4000)).collect();
        let entries = texts
            .iter()
            .enumerate()
            .map(|(i, t)| pending(&format!("/p/{i}.tex"), t, 10 + i as u64))
            .collect();
        write_entries(&dir, entries, &HashMap::new()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(OBJECTS)), 0o700);
        assert_eq!(mode(&dir.join(INDEX)), 0o600);
        for text in &texts {
            assert_eq!(
                mode(&object_path(&dir, fnv1a64(&[text.as_bytes()]), true)),
                0o600
            );
        }
        // The index written again by the collection.
        open_dir(&dir, u64::MAX, 0, 11).unwrap();
        assert_eq!(read_entries(&dir, Path::new("/p/0.tex")).len(), 0);
        assert_eq!(mode(&dir.join(INDEX)), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged line in the index (an append cut inside a multi-byte character) costs that
    /// line, not the whole history: the index was read as empty then, and the collection deleted
    /// every object older than an hour.
    #[test]
    fn a_damaged_index_line_keeps_the_others() {
        let dir = temp("damaged-index");
        write_entries(
            &dir,
            vec![pending("/p/a.tex", "kept", 100)],
            &HashMap::new(),
        )
        .unwrap();
        let mut index = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(INDEX))
            .unwrap();
        index.write_all(b"{\"path\":\"/p/caf\xc3").unwrap();
        drop(index);
        let object = object_path(&dir, fnv1a64(&[b"kept"]), false);
        std::fs::File::options()
            .write(true)
            .open(&object)
            .unwrap()
            .set_modified(SystemTime::now() - ORPHAN_AGE * 2)
            .unwrap();
        let newest = open_dir(&dir, u64::MAX, u64::MAX, 200).unwrap();
        assert_eq!(newest.len(), 1);
        assert!(object.exists(), "the object of an intact line was deleted");
        assert_eq!(read_entries(&dir, Path::new("/p/a.tex")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The append after a damaged line (one a full disk cut, without its line break) starts on a
    /// line of its own: it went on the damaged line, which then still did not parse, and the
    /// new entry was lost (its object deleted as unreferenced later).
    #[test]
    fn an_append_after_a_damaged_line_is_kept() {
        let dir = temp("damaged-append");
        write_entries(
            &dir,
            vec![pending("/p/a.tex", "first", 100)],
            &HashMap::new(),
        )
        .unwrap();
        let mut index = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(INDEX))
            .unwrap();
        index.write_all(b"{\"path\":\"/p/caf\xc3").unwrap();
        drop(index);
        write_entries(
            &dir,
            vec![pending("/p/a.tex", "second", 200)],
            &HashMap::new(),
        )
        .unwrap();
        let entries = read_entries(&dir, Path::new("/p/a.tex"));
        assert_eq!(
            entries.iter().map(|e| e.unix).collect::<Vec<_>>(),
            [200, 100]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On a file system without locks (an NFS home without its lock daemon: `ENOLCK`), the
    /// history is written without the lock, as it was before there was one: every write and
    /// every collection failed, and nothing was recorded.
    #[test]
    fn a_file_system_without_locks_still_gets_the_history() {
        let dir = temp("no-locks");
        UNSUPPORTED_LOCKS.set(true);
        let written = write_entries(&dir, vec![pending("/p/a.tex", "a", 1)], &HashMap::new());
        let opened = open_dir(&dir, u64::MAX, u64::MAX, 2);
        UNSUPPORTED_LOCKS.set(false);
        assert_eq!(written.map(|w| w.len()), Ok(1));
        assert_eq!(opened.map(|newest| newest.len()), Ok(1));
        assert_eq!(read_entries(&dir, Path::new("/p/a.tex")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past its deadline (the window closes), a writer does not wait for the lock another
    /// process holds, and writes no more entries.
    #[test]
    fn writes_stop_at_their_deadline() {
        let dir = temp("deadline");
        let held = lock_index(&dir).unwrap();
        let started = Instant::now();
        let refused = write_entries_until(
            &dir,
            vec![pending("/p/a.tex", "a", 1)],
            &HashMap::new(),
            Some(started),
        );
        assert!(refused.is_err());
        assert!(started.elapsed() < LOCK_WAIT / 2, "{:?}", started.elapsed());
        drop(held);
        // The lock is free now (tried once: refused if anything holds it this very moment), and
        // nothing is written either way.
        let written = write_entries_until(
            &dir,
            vec![pending("/p/a.tex", "a", 1), pending("/p/b.tex", "b", 2)],
            &HashMap::new(),
            Some(Instant::now()),
        );
        assert!(written.as_ref().map_or(true, Vec::is_empty), "{written:?}");
        assert!(read_entries(&dir, Path::new("/p/a.tex")).is_empty());
        assert!(read_entries(&dir, Path::new("/p/b.tex")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two Lemma processes on one project (here two threads, each with its own handles): one
    /// collects the history (and rewrites the index, an entry having expired) while the other
    /// appends entries. The rewrite read the index, and an append that came before its rename
    /// went to the old index: the entry was lost, and its object deleted as garbage later.
    #[test]
    fn appends_survive_a_concurrent_collection() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = temp("concurrent");
        let day = 86_400;
        let now = 1000 * day;
        write_entries(
            &dir,
            vec![pending("/p/a.tex", "seed", now)],
            &HashMap::new(),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let collector = {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut rounds = 0;
                while !stop.load(Ordering::Relaxed) {
                    open_dir(&dir, u64::MAX, 30 * day, now).unwrap();
                    rounds += 1;
                }
                rounds
            })
        };
        const APPENDS: usize = 150;
        for i in 0..APPENDS {
            // An expired entry makes the next collection rewrite the index.
            let entries = vec![
                pending("/p/old.tex", &format!("old {i}"), day),
                pending("/p/a.tex", &format!("young {i}"), now),
            ];
            write_entries(&dir, entries, &HashMap::new()).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let rounds = collector.join().unwrap();
        open_dir(&dir, u64::MAX, 30 * day, now).unwrap();
        let kept = read_entries(&dir, Path::new("/p/a.tex"));
        assert_eq!(
            kept.len(),
            APPENDS + 1,
            "{} entries lost to {rounds} collections",
            APPENDS + 1 - kept.len()
        );
        for i in 0..APPENDS {
            let text = format!("young {i}");
            assert_eq!(
                read_object(&dir, fnv1a64(&[text.as_bytes()])).unwrap(),
                text
            );
        }
        assert!(read_entries(&dir, Path::new("/p/old.tex")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While another process holds the index (its collection runs), a writer waits for it, and
    /// takes a lock file that the other's sweep removed in the meantime anew.
    #[test]
    fn writers_wait_for_the_lock() {
        let dir = temp("lock-wait");
        let held = lock_index(&dir).unwrap();
        let writer = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                write_entries(&dir, vec![pending("/p/a.tex", "a", 1)], &HashMap::new())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !writer.is_finished(),
            "the writer did not wait for the lock"
        );
        // The sweep of an emptied project removes the lock file before it lets go.
        std::fs::remove_file(dir.join(LOCK)).unwrap();
        drop(held);
        assert_eq!(writer.join().unwrap().unwrap().len(), 1);
        // On Windows the removed file stays until the writer, which had it open, closes it, and
        // nothing tells the two apart there (`is_same_file`): the writer wrote under that lock.
        #[cfg(unix)]
        assert!(dir.join(LOCK).exists(), "the writer locked a removed file");
        assert_eq!(read_entries(&dir, Path::new("/p/a.tex")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_objects_are_reported() {
        let dir = temp("damaged");
        write_entries(&dir, vec![pending("/p/a.tex", "abc", 1)], &HashMap::new()).unwrap();
        let hash = fnv1a64(&[b"abc"]);
        std::fs::write(object_path(&dir, hash, false), "abd").unwrap();
        assert!(read_object(&dir, hash).unwrap_err().contains("damaged"));
        assert!(read_object(&dir, 42).unwrap_err().contains("missing"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
