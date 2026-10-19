//! Local history (WP2.4; QoL P1): content-addressed snapshots of project files.
//!
//! - **Store:** `paths.history_dir()/<fnv(root) as 16 hex>/`, with `objects/<hash>.zst` (ruzstd;
//!   or `objects/<hash>` when compressing does not make it smaller) and `index.jsonl`, one line
//!   per entry: `{"path": "/abs/file.tex", "hash": "<16 hex>", "unix": 1760000000, "reason":
//!   "save" | "turn:<n>" | "reject"}`. The hash is `fnv1a64` of the text's UTF-8 bytes.
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

const INDEX: &str = "index.jsonl";
const OBJECTS: &str = "objects";
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
    saved_hook: Cell<Option<HookId>>,
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
    use std::os::unix::ffi::OsStrExt;
    history_dir.join(format!("{:016x}", fnv1a64(&[root.as_os_str().as_bytes()])))
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

/// Writes every save still waiting for its interval now (tests; window close).
pub fn write_waiting(state: &Rc<WindowState>) {
    if let Some(store) = store_of(state) {
        store.write_waiting();
    }
}

/// Restores `entry` into `doc` as reviewable hunks: the review's baseline is the current text
/// (unless a review already runs), the old text is applied with `Origin::Agent { call: 0 }`
/// outside any agent turn ([`review_ui::outside_turn`]) and saved. Resolves to the number of
/// hunks of the document afterwards.
pub fn restore(
    state: &Rc<WindowState>,
    doc: &DocRef,
    entry: &HistoryEntry,
) -> impl Future<Output = Result<usize, String>> + use<> {
    let text = load(state, entry);
    let doc = doc.clone();
    async move {
        let old = text.await?;
        // Computed and applied without yielding, so the text cannot change in between.
        let edits = doc.with_text(|current| minimal_edits(current, &old));
        if edits.is_empty() {
            return Ok(review_ui::hunk_count(&doc));
        }
        review_ui::start_review(&doc, false);
        // The user's restore, not the last agent turn's change (turn 0: "Other changes").
        review_ui::outside_turn(&doc, || doc.apply_edits(&edits, Origin::Agent { call: 0 }))
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

    /// The window closes: saves still waiting for their interval are written now (the main
    /// loop may not run again).
    fn close(&self) {
        if let Some(id) = self.saved_hook.take() {
            crate::saver::disconnect_saved(id);
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
        if !entries.is_empty() && self.enabled() {
            let newest = self.newest.borrow().clone();
            if let Err(e) = write_entries(&dir, entries, &newest) {
                tracing::warn!("local history: {e}");
            }
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
                let opened =
                    gio::spawn_blocking(move || open_dir(&dir, max_bytes, max_age, unix_now()))
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
                let _ = gio::spawn_blocking(move || {
                    sweep(&history_dir, &current, max_bytes, max_age, unix_now());
                })
                .await;
            }
            Job::Write { dir, entries } => {
                let Some(known) = store.upgrade().map(|s| s.newest.borrow().clone()) else {
                    return;
                };
                let written =
                    gio::spawn_blocking(move || write_entries(&dir, entries, &known)).await;
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

fn read_index(dir: &Path) -> Vec<IndexLine> {
    let Ok(text) = std::fs::read_to_string(dir.join(INDEX)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<IndexLine>(line).ok())
        .filter(|line| line.entry().is_some())
        .collect()
}

/// Writes `bytes` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Appends the entries whose text differs from the file's newest entry (`known`, then the
/// entries before it in this batch); returns those written.
fn write_entries(
    dir: &Path,
    entries: Vec<Pending>,
    known: &HashMap<PathBuf, u64>,
) -> Result<Vec<HistoryEntry>, String> {
    let objects = dir.join(OBJECTS);
    std::fs::create_dir_all(&objects)
        .map_err(|e| format!("cannot create {}: {e}", objects.display()))?;
    let mut newest = known.clone();
    let mut lines = String::new();
    let mut written = Vec::new();
    for pending in entries {
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
    if !lines.is_empty() {
        // One write of whole lines with O_APPEND: other windows' appends do not interleave.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(INDEX))
            .and_then(|mut f| f.write_all(lines.as_bytes()))
            .map_err(|e| format!("cannot write the history index: {e}"))?;
    }
    Ok(written)
}

/// Garbage collection (age, then size, then unreferenced objects) and the newest hash of every
/// file.
fn open_dir(
    dir: &Path,
    max_bytes: u64,
    max_age: u64,
    now: u64,
) -> Result<HashMap<PathBuf, u64>, String> {
    if !dir.exists() {
        return Ok(HashMap::new());
    }
    let lines = read_index(dir);
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
        if let Err(e) = open_dir(&path, max_bytes, max_age, now) {
            tracing::warn!("local history: {e}");
            continue;
        }
        let empty_index = std::fs::metadata(path.join(INDEX)).map_or(true, |m| m.len() == 0);
        let no_objects =
            std::fs::read_dir(path.join(OBJECTS)).map_or(true, |mut o| o.next().is_none());
        if empty_index && no_objects {
            let _ = std::fs::remove_file(path.join(INDEX));
            let _ = std::fs::remove_dir(path.join(OBJECTS));
            if std::fs::remove_dir(&path).is_ok() {
                tracing::info!("local history: removed the empty {}", path.display());
            }
        }
    }
}

fn read_entries(dir: &Path, path: &Path) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = read_index(dir)
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

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lemma-history-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
