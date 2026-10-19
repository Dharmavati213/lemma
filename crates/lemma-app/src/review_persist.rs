//! Persistence of agent-edit reviews across sessions (WP2.6; ARCHITECTURE §4.5).
//!
//! Each project's pending reviews are kept in `paths.review_dir()/<fnv(root) as 16 hex>.json`:
//! one [`PersistedReview`] per file with hunks (baseline, a hash of the text on disk, the turn
//! attribution, hunk ids and `user_touched` flags), plus the app's `new_file` flag and the labels
//! and start times of the turns they mention. The file is private to the user (0600 in a 0700
//! directory) since it holds document text.
//!
//! - **Saving:** 500 ms after the last change of the review state (every refresh of the review
//!   controller, and every save of a reviewed file), serialized and written atomically on gio's
//!   I/O thread pool; synchronously when the window closes (also when a write is still queued).
//!   A document with unsaved changes keeps the entry it had when its text was last written to
//!   the file (captured when that save took its snapshot), since only that one matches the file
//!   on disk; no hunks left means no file. When nothing changed since the last snapshot (no
//!   review, document state or turn changed: typing in a document with unsaved changes), nothing
//!   is done, so a big reviewed file costs nothing while the user types. A state that cannot be
//!   written (or serialized) leaves the file as it was and is reported once.
//! - **Restoring:** the file is read when the window opens (and when the project root moves); its
//!   turns are taken in at once (they keep their numbers, and this session's turns are numbered
//!   after them, see [`review_ui::restore_turns`]). Its reviews are restored when the first
//!   document is shown or the first review appears (a window that never shows a document of the
//!   project does not fill its workspace with them). A review comes back through
//!   [`review_ui::adopt_review`] only when the file's text hashes to the saved `current_hash`
//!   and the document has no unsaved changes; the banner then reads "Restored N pending changes
//!   from your last session". An agent edit to a file whose saved review is still waiting
//!   starts from that review ([`take_saved`], called by [`review_ui::start_review`]), so its
//!   hunks are never lost. Until the restore has finished nothing is written, so a window
//!   closed early never loses the saved state.
//!
//! Persistence is off for windows without [`Paths`](lemma_config::Paths) and for runs that quit
//! by themselves (`state.scripted`: tests, snapshots, measurements), so they neither read nor
//! write the user's state.

use crate::document::{DocRef, Document};
use crate::hooks::HookId;
use crate::review_ui::{self, TurnInfo};
use crate::saver;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::fnv1a64;
use lemma_review::{FileReview, PersistedReview};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Quiet time after the last change before the state is written.
pub const DEBOUNCE: Duration = Duration::from_millis(500);
/// The format of [`StateFile`].
const VERSION: u32 = 1;

/// The contents of a project's review file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    /// The project root, for people reading the file (the file name is its hash); a root that is
    /// not valid UTF-8 is written with replacement characters.
    #[serde(serialize_with = "lossy_path")]
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<SavedTurn>,
    #[serde(default)]
    pub reviews: Vec<SavedReview>,
}

/// One file's review: `review.path` is relative to the project root.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedReview {
    #[serde(flatten)]
    pub review: PersistedReview,
    /// The agent created the file (rejecting all of it moves it to the trash).
    #[serde(default, skip_serializing_if = "is_false")]
    pub new_file: bool,
}

/// What is known about a review turn (see [`review_ui::TurnInfo`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTurn {
    pub turn: u32,
    pub label: u32,
    pub started_unix: u64,
    /// A named turn's name (`review_ui::begin_named_turn`; M3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn lossy_path<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy())
}

/// Either form of the file: the current object, or a bare list of reviews (ARCHITECTURE §4.5).
#[derive(Deserialize)]
#[serde(untagged)]
enum AnyFile {
    Current(StateFile),
    Bare(Vec<SavedReview>),
}

/// Parses a review file.
pub fn parse(bytes: &[u8]) -> Result<StateFile, String> {
    match serde_json::from_slice::<AnyFile>(bytes).map_err(|e| e.to_string())? {
        AnyFile::Current(file) => Ok(file),
        AnyFile::Bare(reviews) => Ok(StateFile {
            version: VERSION,
            reviews,
            ..StateFile::default()
        }),
    }
}

/// The review file of the project at `root`: `review_dir/<fnv1a64(root) as 16 hex>.json`.
pub fn file_for_root(review_dir: &Path, root: &Path) -> PathBuf {
    let hash = fnv1a64(&[root.as_os_str().as_encoded_bytes()]);
    review_dir.join(format!("{hash:016x}.json"))
}

/// Where a window stands with its project's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The file is being read.
    Reading,
    /// Read: its reviews wait to be restored (or taken by [`take_saved`]).
    Loaded,
    /// They are being restored.
    Applying,
    /// Restored: changes are written.
    Ready,
    /// The file could not be read: it is neither restored nor overwritten.
    Unreadable,
    /// The window closed.
    Closed,
}

/// The persistence of one window.
struct Persister {
    state: Weak<WindowState>,
    review_dir: PathBuf,
    phase: Cell<Phase>,
    /// Bumped when the root moves: work for the previous root stops.
    generation: Cell<u64>,
    /// The saved reviews not restored yet.
    saved: RefCell<Vec<SavedReview>>,
    /// The one being restored now (while its document loads).
    current: RefCell<Option<SavedReview>>,
    /// A document was shown or a review appeared: restore once the file is read.
    wanted: Cell<bool>,
    /// Hunks restored so far (for the banner).
    restored: Cell<usize>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Per document (absolute path), its entry as of the text last written to its file: what is
    /// saved while it has unsaved changes.
    last: RefCell<HashMap<PathBuf, SavedReview>>,
    /// Bumped whenever `last` changes other than by a snapshot (a save, a restore).
    last_changes: Cell<u64>,
    /// Entries captured when a save took the snapshot of its text, until the save succeeds.
    saving: RefCell<HashMap<PathBuf, SavedReview>>,
    /// What the last snapshot was taken from: when it is unchanged, so is the snapshot.
    scheduled: RefCell<Option<Fingerprint>>,
    /// The last snapshot handed to a writer, with its sequence number.
    pending: RefCell<Option<(u64, Arc<Option<StateFile>>)>>,
    /// Sequence numbers of writes; the writer commits only newer ones (async writes may finish
    /// after a later synchronous one).
    seq: Cell<u64>,
    committed: Arc<Mutex<Committed>>,
    saved_hook: Cell<Option<HookId>>,
    saving_hook: Cell<Option<HookId>>,
    /// A write failure was reported (once per window).
    reported: Cell<bool>,
}

/// What the writers last committed.
#[derive(Debug, Default)]
struct Committed {
    seq: u64,
    /// The file written and the hash of its contents (`0` for no file), to skip identical writes.
    last: Option<(PathBuf, u64)>,
}

/// What a snapshot is made of (see [`Persister::fingerprint`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    /// Every reviewed document.
    docs: Vec<DocPrint>,
    last_changes: u64,
    /// The known turns: number, label, start time.
    turns: Vec<(u32, u32, u64)>,
}

/// What a reviewed document contributes to a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DocPrint {
    path: PathBuf,
    /// Its text version and review generation when it has no unsaved changes; else its `last`
    /// entry is written.
    clean: Option<(u64, u64)>,
    new_file: bool,
}

thread_local! {
    static PERSISTERS: RefCell<Vec<Rc<Persister>>> = const { RefCell::new(Vec::new()) };
}

fn persister(state: &WindowState) -> Option<Rc<Persister>> {
    PERSISTERS.with(|list| {
        list.borrow()
            .iter()
            .find(|p| std::ptr::eq(p.state.as_ptr(), state))
            .cloned()
    })
}

/// Starts persisting `state`'s reviews (see the module docs). Runs once per window, after the
/// review controller and the Changes panel exist, before the agent controller.
pub fn install(state: &Rc<WindowState>) {
    let Some(paths) = &state.paths else {
        return;
    };
    if state.scripted {
        return;
    }
    let p = Rc::new(Persister {
        state: Rc::downgrade(state),
        review_dir: paths.review_dir(),
        phase: Cell::new(Phase::Reading),
        generation: Cell::new(0),
        saved: RefCell::new(Vec::new()),
        current: RefCell::new(None),
        wanted: Cell::new(false),
        restored: Cell::new(0),
        timer: RefCell::new(None),
        last: RefCell::new(HashMap::new()),
        last_changes: Cell::new(0),
        saving: RefCell::new(HashMap::new()),
        scheduled: RefCell::new(None),
        pending: RefCell::new(None),
        seq: Cell::new(0),
        committed: Arc::new(Mutex::new(Committed::default())),
        saved_hook: Cell::new(None),
        saving_hook: Cell::new(None),
        reported: Cell::new(false),
    });
    PERSISTERS.with(|list| list.borrow_mut().push(p.clone()));
    glib::spawn_future_local(read(p.clone(), 0));

    let weak = Rc::downgrade(&p);
    state.connect_root_changed(move |_| {
        if let Some(p) = weak.upgrade() {
            p.root_changed();
        }
    });
    let weak = Rc::downgrade(&p);
    let hook = saver::connect_saving(move |doc| {
        if let Some(p) = weak.upgrade() {
            p.saving(doc);
        }
    });
    p.saving_hook.set(Some(hook));
    let weak = Rc::downgrade(&p);
    let hook = saver::connect_saved(move |path, text| {
        if let Some(p) = weak.upgrade() {
            p.saved(path, text);
        }
    });
    p.saved_hook.set(Some(hook));
    let weak = Rc::downgrade(&p);
    state.window.connect_close_request(move |_| {
        if let Some(p) = weak.upgrade() {
            p.flush();
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&p);
    state.window.connect_destroy(move |_| {
        if let Some(p) = weak.upgrade() {
            p.close();
        }
    });
}

/// The review state of `state`'s window changed (the review controller calls this after every
/// refresh): it is written [`DEBOUNCE`] later. The first change also starts the restore.
pub fn changed(state: &WindowState) {
    if let Some(p) = persister(state) {
        p.changed();
    }
}

/// A document was shown in `state`'s window: the project's saved reviews are restored, if that
/// has not happened yet.
pub fn editor_attached(state: &WindowState) {
    if let Some(p) = persister(state) {
        p.want_restore();
    }
}

/// The saved review of `doc`, if one waits to be restored and still applies (the document has no
/// unsaved changes and its text hashes to the saved hash), with its `new_file` flag. A review
/// about to be created for `doc` (an agent edit that arrives before the restore reached `doc`)
/// starts from it, so the saved hunks are not lost (see `review_ui::start_review`).
pub fn take_saved(doc: &DocRef) -> Option<(FileReview, bool)> {
    let p = PERSISTERS.with(|list| {
        list.borrow()
            .iter()
            .find(|p| {
                p.state.upgrade().is_some_and(|s| {
                    s.workspace
                        .get(doc.path())
                        .is_some_and(|d| Rc::ptr_eq(&d, doc))
                })
            })
            .cloned()
    })?;
    if !matches!(p.phase.get(), Phase::Loaded | Phase::Applying) {
        return None;
    }
    let state = p.state.upgrade()?;
    let rel = doc
        .path()
        .strip_prefix(state.workspace.root().path())
        .ok()?
        .to_path_buf();
    let entry = {
        let mut current = p.current.borrow_mut();
        if current.as_ref().is_some_and(|e| e.review.path == rel) {
            current.take()
        } else {
            let mut saved = p.saved.borrow_mut();
            let i = saved.iter().position(|e| e.review.path == rel)?;
            Some(saved.remove(i))
        }
    }?;
    let review = restorable(doc, &entry)?;
    p.restored.set(p.restored.get() + review.hunks().len());
    p.remember(doc.path(), entry.clone());
    Some((review, entry.new_file))
}

/// Writes `state`'s review state now (as on window close). Nothing happens before the restore has
/// finished or when persistence is off.
pub fn flush(state: &WindowState) {
    if let Some(p) = persister(state) {
        p.flush();
    }
}

/// The review file of `state`'s project, while persistence is on.
pub fn state_file(state: &WindowState) -> Option<PathBuf> {
    let p = persister(state)?;
    let state = p.state.upgrade()?;
    Some(file_for_root(&p.review_dir, state.workspace.root().path()))
}

/// True once the saved reviews of the current root were restored (always false while
/// persistence is off).
pub fn is_restored(state: &WindowState) -> bool {
    persister(state).is_some_and(|p| p.phase.get() == Phase::Ready)
}

impl Persister {
    fn file(&self, state: &WindowState) -> PathBuf {
        file_for_root(&self.review_dir, state.workspace.root().path())
    }

    fn changed(self: &Rc<Self>) {
        match self.phase.get() {
            Phase::Ready => self.schedule(),
            // The first review restores the saved ones (which then write the state).
            Phase::Reading | Phase::Loaded if self.has_reviews() => self.want_restore(),
            _ => {}
        }
    }

    /// Whether some document of the window is under review.
    fn has_reviews(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            state
                .workspace
                .documents()
                .iter()
                .any(|d| d.review.borrow().is_some())
        })
    }

    fn want_restore(self: &Rc<Self>) {
        self.wanted.set(true);
        if self.phase.get() == Phase::Loaded {
            self.phase.set(Phase::Applying);
            glib::spawn_future_local(apply(self.clone(), self.generation.get()));
        }
    }

    fn root_changed(self: &Rc<Self>) {
        if self.phase.get() == Phase::Closed {
            return;
        }
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        self.generation.set(self.generation.get() + 1);
        self.saved.borrow_mut().clear();
        self.current.replace(None);
        self.last.borrow_mut().clear();
        self.last_changes.set(self.last_changes.get() + 1);
        self.saving.borrow_mut().clear();
        self.scheduled.replace(None);
        self.pending.replace(None);
        self.restored.set(0);
        // The first document of the new root is on its way.
        self.wanted.set(true);
        self.phase.set(Phase::Reading);
        glib::spawn_future_local(read(self.clone(), self.generation.get()));
    }

    fn schedule(self: &Rc<Self>) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(DEBOUNCE, move || {
            if let Some(p) = weak.upgrade() {
                p.timer.replace(None);
                p.save(false);
            }
        });
        self.timer.replace(Some(timer));
    }

    /// Keeps `entry` as the one of the document at `path` (its text was written to the file).
    fn remember(&self, path: &Path, entry: SavedReview) {
        self.last.borrow_mut().insert(path.to_path_buf(), entry);
        self.last_changes.set(self.last_changes.get() + 1);
    }

    /// A save of `doc` is taking the snapshot of its text: its entry for exactly that text is
    /// captured, and kept once the save succeeds ([`saved`](Self::saved)).
    fn saving(&self, doc: &Document) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let ours = state
            .workspace
            .get(doc.path())
            .is_some_and(|d| std::ptr::eq(Rc::as_ptr(&d), doc));
        let Ok(rel) = doc.path().strip_prefix(state.workspace.root().path()) else {
            return;
        };
        let review = doc.review.borrow();
        let Some(review) = review.as_ref().filter(|r| ours && !r.hunks().is_empty()) else {
            return;
        };
        let entry = SavedReview {
            review: doc.with_text(|text| review.to_persisted(rel.to_path_buf(), text)),
            new_file: review_ui::is_new_file(doc),
        };
        self.saving
            .borrow_mut()
            .insert(doc.path().to_path_buf(), entry);
    }

    /// A save of the document at `path` wrote `text`: the entry captured for it is the one to
    /// keep while the document has unsaved changes; the state is written.
    fn saved(self: &Rc<Self>, path: &Path, text: &str) {
        let entry = self.saving.borrow_mut().remove(path);
        if let Some(entry) = entry.filter(|e| e.review.current_hash == fnv1a64(&[text.as_bytes()]))
        {
            self.remember(path, entry);
        }
        let reviewed = self.state.upgrade().is_some_and(|state| {
            state
                .workspace
                .get(path)
                .is_some_and(|d| d.review.borrow().is_some())
        });
        if reviewed {
            self.changed();
        }
    }

    /// What a snapshot of `state` would be made of: cheap to compute and compare, so that an
    /// unchanged state is not snapshot, serialized and hashed again.
    fn fingerprint(&self, state: &WindowState) -> Fingerprint {
        let docs = review_ui::reviewed_documents(&state.workspace)
            .iter()
            .map(|doc| {
                let clean = (!doc.is_dirty()).then(|| {
                    let generation = doc.review.borrow().as_ref().map_or(0, |r| r.generation());
                    (doc.version(), generation)
                });
                DocPrint {
                    path: doc.path().to_path_buf(),
                    clean,
                    new_file: review_ui::is_new_file(doc),
                }
            })
            .collect();
        let turns = review_ui::turn_infos(state)
            .into_iter()
            .map(|(turn, info)| (turn, info.label, info.started_unix))
            .collect();
        Fingerprint {
            docs,
            last_changes: self.last_changes.get(),
            turns,
        }
    }

    /// Writes the current state (synchronously with `now`, else on the I/O thread pool), unless
    /// nothing changed since the last snapshot.
    fn save(self: &Rc<Self>, now: bool) {
        if self.phase.get() != Phase::Ready {
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let file = self.file(&state);
        let fingerprint = self.fingerprint(&state);
        if self.scheduled.borrow().as_ref() == Some(&fingerprint) {
            if now {
                // The window closes: the last snapshot must be on disk, not queued.
                self.write_pending_now(&state, &file);
            }
            return;
        }
        let snapshot = Arc::new(self.snapshot(&state));
        self.scheduled.replace(Some(fingerprint));
        self.seq.set(self.seq.get() + 1);
        let seq = self.seq.get();
        self.pending.replace(Some((seq, snapshot.clone())));
        if now {
            if let Err(e) = write_snapshot(&file, &snapshot, seq, &self.committed) {
                self.failed(&state, &e);
            }
            return;
        }
        let committed = self.committed.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result =
                gio::spawn_blocking(move || write_snapshot(&file, &snapshot, seq, &committed))
                    .await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => "the writer panicked".to_owned(),
            };
            if let Some(p) = weak.upgrade()
                && let Some(state) = p.state.upgrade()
            {
                p.failed(&state, &error);
            }
        });
    }

    /// Writes the last snapshot synchronously if no write of it (or of a later one) has
    /// committed yet: its queued write, when it runs, then changes nothing.
    fn write_pending_now(&self, state: &WindowState, file: &Path) {
        let Some((seq, snapshot)) = self.pending.borrow().clone() else {
            return;
        };
        let done = self.committed.lock().map_or(true, |c| c.seq >= seq);
        if done {
            return;
        }
        self.seq.set(self.seq.get() + 1);
        let seq = self.seq.get();
        self.pending.replace(Some((seq, snapshot.clone())));
        if let Err(e) = write_snapshot(file, &snapshot, seq, &self.committed) {
            self.failed(state, &e);
        }
    }

    /// A write failed: the file stays as it was, the next change tries again, and the user is
    /// told (once).
    fn failed(&self, state: &WindowState, error: &str) {
        self.scheduled.replace(None);
        self.report(state, error);
    }

    fn report(&self, state: &WindowState, error: &str) {
        tracing::warn!("cannot save the review state: {error}");
        if !self.reported.replace(true) {
            state.toast(&format!(
                "Could not save the pending changes for later: {error}"
            ));
        }
    }

    /// Writes now, if the restore has finished (window close).
    fn flush(self: &Rc<Self>) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        self.save(true);
    }

    fn close(self: &Rc<Self>) {
        if self.phase.get() == Phase::Closed {
            return;
        }
        self.flush();
        self.phase.set(Phase::Closed);
        if let Some(hook) = self.saved_hook.take() {
            saver::disconnect_saved(hook);
        }
        if let Some(hook) = self.saving_hook.take() {
            saver::disconnect_saving(hook);
        }
        PERSISTERS.with(|list| list.borrow_mut().retain(|p| !Rc::ptr_eq(p, self)));
    }

    /// What to write: every reviewed document of the project (a document with unsaved changes
    /// keeps the entry of the text last written to its file). `None` when nothing is left to
    /// review.
    fn snapshot(&self, state: &WindowState) -> Option<StateFile> {
        let root = state.workspace.root().path().to_path_buf();
        let mut last = self.last.borrow_mut();
        let mut reviews = Vec::new();
        let mut kept = HashMap::new();
        for doc in review_ui::reviewed_documents(&state.workspace) {
            let Ok(rel) = doc.path().strip_prefix(&root) else {
                continue;
            };
            let entry = if doc.is_dirty() {
                last.get(doc.path()).cloned()
            } else {
                let review = doc.review.borrow();
                review.as_ref().map(|review| SavedReview {
                    review: doc.with_text(|text| review.to_persisted(rel.to_path_buf(), text)),
                    new_file: review_ui::is_new_file(&doc),
                })
            };
            if let Some(entry) = entry {
                kept.insert(doc.path().to_path_buf(), entry.clone());
                reviews.push(entry);
            }
        }
        *last = kept;
        if reviews.is_empty() {
            return None;
        }
        let used: BTreeSet<u32> = reviews
            .iter()
            .flat_map(|r| r.review.attribution.iter().map(|a| a.turn))
            .collect();
        let turns = review_ui::turn_infos(state)
            .into_iter()
            .filter(|(turn, _)| used.contains(turn))
            .map(|(turn, info)| SavedTurn {
                turn,
                label: info.label,
                started_unix: info.started_unix,
                name: info.name,
            })
            .collect();
        Some(StateFile {
            version: VERSION,
            root,
            turns,
            reviews,
        })
    }
}

/// Serializes `snapshot` and writes it with [`write_state`] (on the I/O thread pool, except when
/// the window closes). A state that cannot be serialized is an error: the file stays as it was.
fn write_snapshot(
    file: &Path,
    snapshot: &Option<StateFile>,
    seq: u64,
    committed: &Mutex<Committed>,
) -> Result<(), String> {
    let contents = match snapshot {
        Some(state) => Some(
            serde_json::to_vec(state)
                .map_err(|e| format!("cannot serialize the review state: {e}"))?,
        ),
        None => None,
    };
    write_state(file, contents.as_deref(), seq, committed)
}

/// Writes `contents` to `file` atomically (a private temporary file renamed over it), or removes
/// the file for `None`, unless a write with a later `seq` already happened; contents equal to the
/// last committed ones are not written again.
fn write_state(
    file: &Path,
    contents: Option<&[u8]>,
    seq: u64,
    committed: &Mutex<Committed>,
) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let mut last = committed
        .lock()
        .map_err(|_| "a writer panicked".to_owned())?;
    if seq <= last.seq {
        return Ok(());
    }
    let hash = contents.map_or(0, |c| fnv1a64(&[c]));
    if last
        .last
        .as_ref()
        .is_some_and(|(f, h)| f == file && *h == hash)
    {
        last.seq = seq;
        return Ok(());
    }
    let shown = |e: std::io::Error| format!("{}: {e}", file.display());
    match contents {
        None => match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(shown(e)),
        },
        Some(contents) => {
            if let Some(dir) = file.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(shown)?;
            }
            let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| f.write_all(contents))
                .and_then(|()| std::fs::rename(&tmp, file));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(shown(e));
            }
        }
    }
    last.seq = seq;
    last.last = Some((file.to_path_buf(), hash));
    Ok(())
}

/// Reads the project's file; its turns are taken in at once (this session's turns are numbered
/// after them), its reviews wait for [`apply`] (or [`take_saved`]).
async fn read(p: Rc<Persister>, generation: u64) {
    let Some(state) = p.state.upgrade() else {
        return;
    };
    let file = p.file(&state);
    drop(state);
    let saved = match gio::File::for_path(&file).load_contents_future().await {
        // An empty file holds nothing (an earlier version could write one).
        Ok((bytes, _)) if bytes.trim_ascii().is_empty() => StateFile::default(),
        Ok((bytes, _)) => match parse(&bytes) {
            Ok(saved) => saved,
            Err(e) => {
                // Possibly a newer format: leave it alone, and do not overwrite it either.
                tracing::warn!(
                    "cannot parse the review state {}, leaving it alone: {e}",
                    file.display()
                );
                if p.generation.get() == generation && p.phase.get() == Phase::Reading {
                    p.phase.set(Phase::Unreadable);
                }
                return;
            }
        },
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => StateFile::default(),
        Err(e) => {
            // Unreadable (permissions): leave it alone and do not overwrite it either.
            tracing::warn!("cannot read the review state {}: {e}", file.display());
            if p.generation.get() == generation && p.phase.get() == Phase::Reading {
                p.phase.set(Phase::Unreadable);
            }
            return;
        }
    };
    if p.generation.get() != generation || p.phase.get() != Phase::Reading {
        return;
    }
    let Some(state) = p.state.upgrade() else {
        return;
    };
    let infos: BTreeMap<u32, TurnInfo> = saved
        .turns
        .iter()
        .map(|t| {
            let info = TurnInfo {
                label: t.label,
                started_unix: t.started_unix,
                this_session: false,
                name: t.name.clone(),
            };
            (t.turn, info)
        })
        .collect();
    let max_turn = saved
        .reviews
        .iter()
        .flat_map(|r| r.review.attribution.iter().map(|a| a.turn))
        .chain(infos.keys().copied())
        .max()
        .unwrap_or(0);
    review_ui::restore_turns(&state, &infos, max_turn);
    p.saved.replace(saved.reviews);
    p.phase.set(Phase::Loaded);
    if p.wanted.get() {
        p.want_restore();
    }
}

/// Restores the saved reviews (see the module docs), then writes the state back (entries that no
/// longer apply go).
async fn apply(p: Rc<Persister>, generation: u64) {
    let current =
        |p: &Persister| p.generation.get() == generation && p.phase.get() == Phase::Applying;
    loop {
        let Some(entry) = p.saved.borrow_mut().pop() else {
            break;
        };
        let Some(state) = p.state.upgrade() else {
            return;
        };
        let Ok(path) = state.workspace.root().resolve(&entry.review.path) else {
            continue;
        };
        p.current.replace(Some(entry));
        let doc = match state.workspace.get(&path) {
            Some(doc) => Some(doc),
            // Gone or unreadable: nothing to restore.
            None => state.workspace.get_or_load(&path).await.ok(),
        };
        if !current(&p) {
            return;
        }
        // `take_saved` may have restored it meanwhile.
        let (Some(doc), Some(entry)) = (doc, p.current.take()) else {
            continue;
        };
        match restorable(&doc, &entry) {
            Some(review) => {
                p.restored.set(p.restored.get() + review.hunks().len());
                review_ui::adopt_review(&doc, review);
                if entry.new_file {
                    review_ui::start_review(&doc, true);
                }
                p.remember(doc.path(), entry);
            }
            None => {
                if !doc.is_open() && doc.review.borrow().is_none() {
                    state.workspace.release(&doc);
                }
            }
        }
    }
    if !current(&p) {
        return;
    }
    p.phase.set(Phase::Ready);
    if let Some(state) = p.state.upgrade()
        && p.restored.get() > 0
    {
        review_ui::announce_restored(&state, p.restored.get());
    }
    p.schedule();
}

/// The review `entry` restores for `doc`: only when the document has no review and no unsaved
/// changes, its text is the one the review was saved with, and hunks remain.
fn restorable(doc: &DocRef, entry: &SavedReview) -> Option<FileReview> {
    if doc.review.borrow().is_some() || doc.is_dirty() {
        return None;
    }
    doc.with_text(|text| FileReview::from_persisted(&entry.review, text))
        .filter(|r| !r.hunks().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_hash_the_root() {
        let dir = Path::new("/state/review");
        let a = file_for_root(dir, Path::new("/home/u/thesis"));
        assert_eq!(a.parent(), Some(dir));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("json"));
        assert_eq!(a.file_stem().map(|s| s.len()), Some(16));
        assert_ne!(a, file_for_root(dir, Path::new("/home/u/paper")));
        assert_eq!(a, file_for_root(dir, Path::new("/home/u/thesis")));
    }

    #[test]
    fn both_file_forms_parse() {
        let review = r#"{"path":"main.tex","baseline":"a\n","current_hash":1,"created_unix":2}"#;
        let bare = parse(format!("[{review}]").as_bytes()).unwrap();
        assert_eq!(bare.reviews.len(), 1);
        assert!(!bare.reviews[0].new_file);
        let full = r#"{"version":1,"root":"/p","turns":[{"turn":3,"label":1,"started_unix":5}],
            "reviews":[{"path":"x.tex","baseline":"","current_hash":0,"created_unix":0,"new_file":true}]}"#;
        let full = parse(full.as_bytes()).unwrap();
        assert_eq!(full.turns[0].turn, 3);
        assert!(full.reviews[0].new_file);
        assert!(parse(b"{oops").is_err());
    }

    #[test]
    fn writes_are_atomic_private_and_ordered() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("lemma-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("review").join("x.json");
        let committed = Mutex::new(Committed::default());
        write_state(&file, Some(b"two"), 2, &committed).unwrap();
        // An older write that finishes later changes nothing.
        write_state(&file, Some(b"one"), 1, &committed).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        write_state(&file, None, 3, &committed).unwrap();
        assert!(!file.exists());
        write_state(&file, None, 4, &committed).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
