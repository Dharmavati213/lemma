//! Recovery of unsaved changes after a crash, a kill or a logout.
//!
//! Auto-save is off by default, and a crash, `SIGTERM` (a shutdown), `SIGHUP` (the terminal
//! closed) or a logout ends the process without the "Save changes?" dialog. So while
//! `[editor] recovery` is on (the default), each edited document keeps a copy of its unsaved
//! text in `Paths::recovery_dir()/<fnv(path) as 16 hex>.json`
//! (`$XDG_STATE_HOME/lemma/recovery/…`, [`file_for`]): a [`RecoveryFile`] with the document's
//! path, the hash of the file on disk the text is based on (the saver's `disk_hash`), the time of
//! the copy and the text, byte for byte (its line breaks are in it; the charset is the file's,
//! which the document reads again when it opens). The file is private to the user (0600 in a 0700
//! directory): it holds their text.
//!
//! **Copying.** [`DELAY`] (5 s) after the last change to a document of the window, at the latest
//! [`MAX_DELAY`] after the first change not copied yet (typing without a pause), and at once when
//! the window loses the focus, the text of every edited document that changed since its last copy
//! is taken on the main thread (one copy of the buffer, GTK's `get_slice`: under 1 ms for 10k
//! lines, about 4.5 ms for the 60k lines of `large.tex`; `gtk_session`'s
//! `perf_recovery_copy_large_tex`), then serialized and written atomically on the jobs thread,
//! the newest copy of each file winning (a copy still on its way never replaces a newer one, nor
//! brings back a file that was removed meanwhile). A keystroke only notes the time. The file is
//! removed when the document has no unsaved changes any more (saved, reloaded from disk, its
//! changes undone, or discarded with `:q!`) and when the user discards them in the "Save
//! changes?" dialog or with `:qa!`. Closing the window, and the end of the process, do the queued
//! work synchronously. Nothing is copied for windows without `Paths`, in runs that quit by
//! themselves (tests, snapshots, measurements) or while `[editor] recovery` is off; turning it off
//! removes the window's files.
//!
//! **Offering.** When a document opens in a tab without unsaved changes (from the command line,
//! the session restore, Ctrl+O) and has a recovery file, the file is read on the jobs thread and
//! [`decide`]d: a bar under the header names it, "Unsaved changes to “main.tex” from 5 minutes ago
//! were recovered" (with "; the file changed on disk since" when the file on disk is not the one
//! the text was based on), with Review and Discard. **Review** ([`review`]) applies the recovered
//! texts as a named review turn, "Recovered changes" (`review_ui::begin_named_turn`, as Replace in
//! Project does), so each change is a hunk to keep or reject against the text the document has
//! now (the file on disk), and saves the documents that had no other unsaved changes: as after an
//! agent's edit, the disk holds the proposal, and a reject puts the old text back.
//! **Discard** ([`discard_offered`]) removes the files. While a document's file is read or
//! offered, no copy of the document is written, so the recovered text is never overwritten before
//! the user answers; a tab closed meanwhile leaves the file for the next time it opens. A
//! recovery file that cannot be read or parsed is logged and left alone, one that holds the
//! document's text is removed, and one of another file (whose path hashes the same) is ignored.
//! So is the copy of a document that another Lemma still has open with unsaved changes (Lemma
//! runs one process per window, and two can show the same file): every process that writes
//! recovery files holds a lock on `<pid>.lock` in the directory while it runs ([`lock_file`]),
//! and a file whose writer still holds its lock is not left by a crash ([`is_running`]).
//!
//! **Signals** (Unix): the app ([`connect_termination`], for runs that do not quit by themselves)
//! writes every edited document's file on `SIGTERM`, `SIGHUP` and `SIGINT` (Ctrl+C in the
//! terminal, which also killed the process and its unsaved changes), synchronously
//! ([`write_now`]), then closes the windows as Ctrl+Q does, but without asking about the
//! changes, which stay in their files. Windows has no such signals: the timer and the copy on
//! focus loss cover it.
//!
//! **Pruning.** Once per process, 10 s after an interactive start ([`prune_once`]), the jobs
//! thread removes the temporary files of writes a crash interrupted, the lock files of processes
//! that ended, and recovery files not written for 180 days.
//!
//! Hooks: [`install`] once per window; [`attach`] / [`detach`] per editor tab; [`discard`] where
//! the user throws changes away; [`directory`] and [`is_left`] for the session, which restores
//! the tab of a file that is gone (a New File never saved) when a crash left its text. Tests:
//! [`set_enabled`], [`set_delays`], [`is_scheduled`], [`copy_now`], [`offered`], [`shown`],
//! [`review`], [`discard_offered`], [`flush`], [`file_for`], [`lock_file`], [`encode`],
//! [`parse`], [`decide`], [`offer_text`].

use crate::document::{self, DocRef, Document, ObserverId};
use crate::editor::Editor;
use crate::review_ui;
use crate::saver::SaveError;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_config::Paths;
use lemma_core::{Origin, fnv1a64};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A document's text is copied this long after its last change.
pub const DELAY: Duration = Duration::from_secs(5);

/// … and at the latest this long after the first change not copied yet.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// The format of [`RecoveryFile`].
pub const VERSION: u32 = 1;

/// The name of the review turn that [`review`] makes.
pub const TURN_NAME: &str = "Recovered changes";

/// A temporary file of a write this old was left by a process that died while writing.
const STALE_TMP_AGE: Duration = Duration::from_secs(10 * 60);

/// A recovery file not written for this long is removed by [`prune_once`].
const PRUNE_AGE: Duration = Duration::from_secs(180 * 24 * 60 * 60);

const CSS: &str = "
.lemma-recovery { padding: 6px 12px;
                  background-color: color-mix(in srgb, var(--accent-bg-color) 30%, transparent); }
";

// ----- the file -----

/// A document's recovery file (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryFile {
    #[serde(default)]
    pub version: u32,
    /// The document's path (absolute; with replacement characters when it is not UTF-8). The file
    /// name is its hash.
    pub path: String,
    /// `fnv1a64` of the bytes the document's file held when the document last read or wrote it;
    /// `None` when the file did not exist (a new file not saved yet).
    #[serde(default)]
    pub disk_hash: Option<u64>,
    /// When the text was copied (Unix time, seconds).
    #[serde(default)]
    pub saved_at: u64,
    /// The process that wrote it (see [`is_running`]).
    #[serde(default)]
    pub pid: Option<u32>,
    /// The document's text.
    pub text: String,
}

/// The recovery file of the document at `path` (normalized): `recovery_dir/<fnv1a64(path) as 16
/// hex>.json`.
pub fn file_for(recovery_dir: &Path, path: &Path) -> PathBuf {
    let hash = fnv1a64(&[path.as_os_str().as_encoded_bytes()]);
    recovery_dir.join(format!("{hash:016x}.json"))
}

/// The lock file that the process `pid` holds in `recovery_dir` while it runs:
/// `recovery_dir/<pid>.lock`.
pub fn lock_file(recovery_dir: &Path, pid: u32) -> PathBuf {
    recovery_dir.join(format!("{pid}.lock"))
}

/// The bytes of the recovery file of the document at `path` (a [`RecoveryFile`] written by this
/// process).
pub fn encode(
    path: &Path,
    disk_hash: Option<u64>,
    saved_at: u64,
    text: &str,
) -> Result<Vec<u8>, String> {
    /// [`RecoveryFile`] without copying the text.
    #[derive(Serialize)]
    struct Borrowed<'a> {
        version: u32,
        path: &'a str,
        disk_hash: Option<u64>,
        saved_at: u64,
        pid: u32,
        text: &'a str,
    }
    serde_json::to_vec(&Borrowed {
        version: VERSION,
        path: &path.to_string_lossy(),
        disk_hash,
        saved_at,
        pid: std::process::id(),
        text,
    })
    .map_err(|e| e.to_string())
}

/// Parses a recovery file.
pub fn parse(bytes: &[u8]) -> Result<RecoveryFile, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// What a document that opens does with the recovery file it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// Offer the text; `changed`: the file on disk is no longer the one the text was based on.
    Offer { changed: bool },
    /// The document holds that text already: there is nothing to recover, and the file goes.
    Remove,
    /// The file belongs to another document (whose path hashes the same), holds a text no
    /// document can hold (with U+0000), or is the copy of a document another Lemma that still
    /// runs has open: leave it alone.
    Ignore,
}

/// Decides what the document at `path` (whose file on disk hashes to `disk_hash`, `None` when it
/// does not exist, and whose text is `text`) does with the recovery file `file`; `writer_runs`:
/// the process that wrote it, another Lemma, still runs ([`is_running`]).
pub fn decide(
    file: &RecoveryFile,
    path: &Path,
    disk_hash: Option<u64>,
    text: &str,
    writer_runs: bool,
) -> Found {
    if writer_runs || file.path != path.to_string_lossy() || file.text.contains('\0') {
        Found::Ignore
    } else if file.text == text {
        Found::Remove
    } else {
        Found::Offer {
            changed: file.disk_hash != disk_hash,
        }
    }
}

/// How long ago something happened `seconds` ago, for the bar: "less than a minute ago",
/// "5 minutes ago", "2 hours ago", "yesterday", "3 days ago".
pub fn relative_time(seconds: u64) -> String {
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match seconds {
        0..60 => "less than a minute ago".to_owned(),
        60..3_600 => plural(seconds / 60, "minute"),
        3_600..86_400 => plural(seconds / 3_600, "hour"),
        86_400..172_800 => "yesterday".to_owned(),
        _ => plural(seconds / 86_400, "day"),
    }
}

/// One offered document, as the bar names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferLabel {
    /// The document's path relative to the project root.
    pub name: String,
    /// How many seconds ago its text was copied.
    pub age: u64,
    /// The file on disk changed since.
    pub changed: bool,
}

/// The bar's text for `offers` (empty for none): the document and how long ago its text was
/// copied (for several, the newest copy's age), and which files changed on disk since.
pub fn offer_text(offers: &[OfferLabel]) -> String {
    let changed: Vec<&OfferLabel> = offers.iter().filter(|o| o.changed).collect();
    match offers {
        [] => String::new(),
        [one] => {
            let since = if one.changed {
                "; the file changed on disk since"
            } else {
                ""
            };
            format!(
                "Unsaved changes to “{}” from {} were recovered{since}",
                one.name,
                relative_time(one.age)
            )
        }
        several => {
            let newest = several.iter().map(|o| o.age).min().unwrap_or(0);
            let names: Vec<&str> = several.iter().map(|o| o.name.as_str()).collect();
            let since = match changed.as_slice() {
                [] => String::new(),
                [one] => format!("; “{}” changed on disk since", one.name),
                all if all.len() == several.len() => "; the files changed on disk since".to_owned(),
                some => format!("; {} of the files changed on disk since", some.len()),
            };
            format!(
                "Unsaved changes to {} documents from {} were recovered: {}{since}",
                several.len(),
                relative_time(newest),
                names.join(", ")
            )
        }
    }
}

/// How long to wait before the next copy: `None` when it is due, that is when the last change
/// (at `last`) is `delay` old, or the first change not copied yet (at `first`) is `max` old.
pub fn wait_left(
    first: Instant,
    last: Instant,
    now: Instant,
    delay: Duration,
    max: Duration,
) -> Option<Duration> {
    let quiet = now.saturating_duration_since(last);
    let waited = now.saturating_duration_since(first);
    if quiet >= delay || waited >= max {
        None
    } else {
        Some((delay - quiet).min(max - waited))
    }
}

/// Reads the recovery file `file`: `None` when there is none.
fn read_file(file: &Path) -> Result<Option<RecoveryFile>, String> {
    match std::fs::read(file) {
        Ok(bytes) => parse(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// True when the process `pid`, another than this one, holds its lock in `recovery_dir`
/// ([`lock_file`]): a Lemma that still runs, whose recovery files are copies of documents it has
/// open. The lock goes with the process however it ends, so a crash, a kill or a reboot leaves
/// none, and a new process that got the same `pid` does not hold it. Blocking.
pub fn is_running(recovery_dir: &Path, pid: u32) -> bool {
    pid != std::process::id()
        && std::fs::File::open(lock_file(recovery_dir, pid))
            .is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

/// True when `recovery_dir` holds a copy of the unsaved text of the document at `path`
/// (normalized) that no running Lemma keeps: one a crash, a kill or a logout left, which the
/// document offers when it opens. Blocking.
pub fn is_left(recovery_dir: &Path, path: &Path) -> bool {
    read_file(&file_for(recovery_dir, path))
        .ok()
        .flatten()
        .is_some_and(|file| {
            file.path == path.to_string_lossy()
                && !file.pid.is_some_and(|pid| is_running(recovery_dir, pid))
        })
}

/// This process's lock in each recovery directory it writes to (`None` when it could not be
/// taken), held until the process ends.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Option<std::fs::File>>>> =
    LazyLock::new(Mutex::default);

/// Takes this process's lock in the recovery directory `dir` ([`lock_file`]) the first time.
/// Blocking.
fn hold_lock(dir: &Path) {
    let mut locks = lock(&LOCKS);
    if locks.contains_key(dir) {
        return;
    }
    let file = lock_file(dir, std::process::id());
    let held = crate::fsutil::private_file_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&file)
        .and_then(|f| f.try_lock().map(|()| f).map_err(std::io::Error::from));
    if let Err(e) = &held {
        tracing::warn!("recovery: cannot lock {}: {e}", file.display());
    }
    locks.insert(dir.to_path_buf(), held.ok());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ----- writing -----

/// A copy of a document's text, with what its recovery file records about it.
struct Snapshot {
    path: PathBuf,
    disk_hash: Option<u64>,
    saved_at: u64,
    /// One copy of the buffer, taken on the main thread (a `GString` can go to the jobs thread).
    text: glib::GString,
}

/// What the jobs thread does to a recovery file.
enum Op {
    Write(Snapshot),
    Remove,
}

/// Writes and removes the recovery files of the process, one operation at a time, the newest
/// operation on each file winning: an operation that a newer one replaced before it ran never
/// runs, so a copy on its way never replaces a newer copy or brings back a removed file.
#[derive(Default)]
struct Writer {
    /// Per recovery file, the newest operation not done yet.
    queued: Mutex<HashMap<PathBuf, Op>>,
    /// Held while operations run.
    running: Mutex<()>,
}

static WRITER: LazyLock<Writer> = LazyLock::new(Writer::default);

/// A lock, also when a thread panicked while holding it (the data stays consistent: each
/// operation is taken out of the map before it runs).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Writer {
    /// Queues `op` on `file` (replacing an operation queued before it) and runs it on the jobs
    /// thread; `failed` hears about an error.
    fn queue(&'static self, file: PathBuf, op: Op, failed: impl FnOnce(String) + 'static) {
        lock(&self.queued).insert(file.clone(), op);
        glib::spawn_future_local(async move {
            let error = match gio::spawn_blocking(move || self.run(&file)).await {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => "the writer panicked".to_owned(),
            };
            failed(error);
        });
    }

    /// Runs the operation queued on `file`, if a flush has not run it already.
    fn run(&self, file: &Path) -> Result<(), String> {
        let _running = lock(&self.running);
        let op = lock(&self.queued).remove(file);
        op.map_or(Ok(()), |op| perform(file, op))
    }

    /// Runs every queued operation now, on this thread.
    fn flush(&self) {
        let _running = lock(&self.running);
        let ops: Vec<(PathBuf, Op)> = lock(&self.queued).drain().collect();
        for (file, op) in ops {
            if let Err(e) = perform(&file, op) {
                tracing::warn!("recovery: {e}");
            }
        }
    }
}

fn perform(file: &Path, op: Op) -> Result<(), String> {
    let shown = |e: std::io::Error| format!("{}: {e}", file.display());
    match op {
        Op::Write(s) => {
            let bytes = encode(&s.path, s.disk_hash, s.saved_at, &s.text)?;
            write_atomically(file, &bytes).map_err(shown)
        }
        Op::Remove => match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(shown(e)),
            _ => Ok(()),
        },
    }
}

/// Writes `bytes` to `file` through a private temporary file, flushed to the disk before it is
/// renamed over `file`: the copy is there to survive the process.
fn write_atomically(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = file.parent() {
        crate::fsutil::private_dir_builder().create(dir)?;
        // Before the copy can be found: it is this process's while the process runs.
        hold_lock(dir);
    }
    static TMP: AtomicU64 = AtomicU64::new(0);
    let n = TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = file.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
    let written = crate::fsutil::private_file_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_data()
        })
        .and_then(|()| std::fs::rename(&tmp, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Does every queued write and removal now, on the main thread: the window closes, or the
/// process ends.
pub fn flush() {
    WRITER.flush();
}

// ----- one window -----

/// What a window knows about a document's recovery file.
enum Entry {
    /// Being read (the document just opened): nothing is copied meanwhile.
    Checking,
    /// Offered in the bar: nothing is copied until the user answers.
    Offered(Offer),
    /// This window's: the file holds the text of the document's `version` (`None` after a
    /// write failed: the next copy writes it again).
    Written { version: Option<u64> },
    /// The user threw the text of `version` away: it is not copied again (the window losing the
    /// focus as it closes would), only a later change is.
    Discarded { version: u64 },
}

/// A recovered text waiting in the bar.
struct Offer {
    doc: Weak<Document>,
    file: RecoveryFile,
    changed: bool,
}

/// An attached editor, its document's edit observer and its buffer's `modified-changed` handler.
struct Watched {
    editor: Weak<Editor>,
    doc: DocRef,
    observer: ObserverId,
    modified: glib::SignalHandlerId,
}

/// The bar that offers recovered changes.
struct Bar {
    revealer: gtk::Revealer,
    label: gtk::Label,
}

/// One window's recovery.
struct Recovery {
    state: Weak<WindowState>,
    /// `None`: the default (see [`Recovery::enabled`]).
    enabled: Cell<Option<bool>>,
    delay: Cell<Duration>,
    max_delay: Cell<Duration>,
    timer: RefCell<Option<glib::SourceId>>,
    /// The first change not copied yet, and the last change.
    first_change: Cell<Option<Instant>>,
    last_change: Cell<Option<Instant>>,
    /// By document path.
    entries: RefCell<HashMap<PathBuf, Entry>>,
    watched: RefCell<Vec<Watched>>,
    bar: Bar,
    /// A write failure was reported (once per window).
    reported: Cell<bool>,
}

thread_local! {
    static RECOVERIES: RefCell<Vec<Rc<Recovery>>> = const { RefCell::new(Vec::new()) };
}

fn recovery(state: &WindowState) -> Option<Rc<Recovery>> {
    RECOVERIES.with_borrow_mut(|list| {
        list.retain(|r| r.state.strong_count() > 0);
        list.iter()
            .find(|r| std::ptr::eq(r.state.as_ptr(), state))
            .cloned()
    })
}

/// Sets up the window's recovery: the bar, the copy on focus loss, the flush when it closes.
pub fn install(state: &Rc<WindowState>) {
    crate::build_ui::install_css_once("lemma-recovery", CSS);
    let rec = Rc::new(Recovery {
        state: Rc::downgrade(state),
        enabled: Cell::new(None),
        delay: Cell::new(DELAY),
        max_delay: Cell::new(MAX_DELAY),
        timer: RefCell::new(None),
        first_change: Cell::new(None),
        last_change: Cell::new(None),
        entries: RefCell::new(HashMap::new()),
        watched: RefCell::new(Vec::new()),
        bar: Bar::new(state),
        reported: Cell::new(false),
    });
    RECOVERIES.with_borrow_mut(|list| list.push(rec.clone()));
    rec.connect_bar();
    let weak = Rc::downgrade(&rec);
    state.window.connect_is_active_notify(move |window| {
        if !window.is_active()
            && let Some(rec) = weak.upgrade()
        {
            rec.copy_now();
        }
    });
    let weak = Rc::downgrade(&rec);
    state.connect_config_changed(move |state, file| {
        if file == ConfigFile::Config
            && let Some(rec) = weak.upgrade()
            && !rec.enabled(state)
        {
            rec.switched_off();
        }
    });
    // Runs only when the window really closes (after the shell's handler, which keeps it open
    // to ask about unsaved changes); the destroy handler is the fallback.
    state.window.connect_close_request(|_| {
        flush();
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&rec);
    state.window.connect_destroy(move |_| {
        if let Some(rec) = weak.upgrade() {
            rec.cancel_timer();
        }
        flush();
    });
}

/// Watches `editor`'s document: its changes are copied, a copy goes when it has no unsaved
/// changes any more, and a recovery file it has is offered.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(rec) = recovery(state) else {
        return;
    };
    let doc = editor.document().clone();
    let weak = Rc::downgrade(&rec);
    let id = doc.connect_edit(move |_, _, _| {
        if let Some(rec) = weak.upgrade() {
            rec.changed();
        }
    });
    let (weak, weak_doc) = (Rc::downgrade(&rec), Rc::downgrade(&doc));
    let handler = editor.buffer().connect_modified_changed(move |_| {
        if let (Some(rec), Some(doc)) = (weak.upgrade(), weak_doc.upgrade())
            && !doc.is_dirty()
        {
            rec.cleaned(&doc);
        }
    });
    rec.watched.borrow_mut().push(Watched {
        editor: Rc::downgrade(editor),
        doc: doc.clone(),
        observer: id,
        modified: handler,
    });
    if !rec.enabled(state) {
        return;
    }
    if doc.is_dirty() {
        rec.changed();
    } else {
        rec.check(state, &doc);
    }
}

/// Undoes [`attach`]: the tab closes. An offer leaves the bar (its file stays, for the next time
/// the document opens); the copy of a document with unsaved changes stays too (the window
/// closes, or the system ends the process).
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(rec) = recovery(state) else {
        return;
    };
    let removed: Vec<Watched> = rec
        .watched
        .borrow_mut()
        .extract_if(.., |w| w.editor.as_ptr() == Rc::as_ptr(editor))
        .collect();
    for Watched {
        doc,
        observer,
        modified,
        ..
    } in removed
    {
        doc.disconnect(observer);
        editor.buffer().disconnect(modified);
        let entry = rec.entries.borrow_mut().remove(doc.path());
        match entry {
            Some(Entry::Offered(_)) => rec.refresh_bar(),
            Some(Entry::Written { .. }) if !doc.is_dirty() => rec.queue(doc.path(), Op::Remove),
            _ => {}
        }
    }
}

/// The user threw `doc`'s unsaved changes away (Discard in the "Save changes?" dialog, `:qa!`):
/// its recovery file goes too, and that text is not copied again. A file offered in the bar
/// stays: it holds older changes, which the user has not answered about.
pub fn discard(state: &WindowState, doc: &Document) {
    let Some(rec) = recovery(state) else {
        return;
    };
    let written = match rec.entries.borrow().get(doc.path()) {
        Some(Entry::Checking | Entry::Offered(_)) => return,
        Some(Entry::Written { .. }) => true,
        Some(Entry::Discarded { .. }) | None => false,
    };
    rec.entries.borrow_mut().insert(
        doc.path().to_path_buf(),
        Entry::Discarded {
            version: doc.version(),
        },
    );
    if written {
        rec.queue(doc.path(), Op::Remove);
    }
}

/// Copies the text of every document of the window with unsaved changes now and writes the
/// files synchronously, with everything else still queued: the process is about to end
/// (`SIGTERM`, `SIGHUP`, `SIGINT`).
pub fn write_now(state: &Rc<WindowState>) {
    if let Some(rec) = recovery(state)
        && rec.enabled(state)
    {
        rec.cancel_timer();
        for doc in state.workspace.dirty_documents() {
            rec.copy(&doc);
        }
    }
    flush();
}

/// The window's recovery directory while it copies and offers unsaved text (see
/// [`set_enabled`]); `None` otherwise.
pub fn directory(state: &WindowState) -> Option<PathBuf> {
    let rec = recovery(state)?;
    if rec.enabled(state) {
        state.paths.as_ref().map(Paths::recovery_dir)
    } else {
        None
    }
}

/// Copies the text of every edited document of the window now (what the window losing the focus
/// does).
pub fn copy_now(state: &WindowState) {
    if let Some(rec) = recovery(state) {
        rec.copy_now();
    }
}

/// Switches the window's recovery on or off regardless of the window (`None`: the default, on
/// for windows with `Paths` that do not quit by themselves). For tests, with temporary XDG
/// directories.
pub fn set_enabled(state: &WindowState, on: Option<bool>) {
    if let Some(rec) = recovery(state) {
        rec.enabled.set(on);
    }
}

/// Changes [`DELAY`] and [`MAX_DELAY`] for the window (tests).
pub fn set_delays(state: &WindowState, delay: Duration, max_delay: Duration) {
    if let Some(rec) = recovery(state) {
        rec.delay.set(delay);
        rec.max_delay.set(max_delay);
    }
}

/// True while a copy is scheduled.
pub fn is_scheduled(state: &WindowState) -> bool {
    recovery(state).is_some_and(|r| r.timer.borrow().is_some())
}

/// The documents whose recovered changes the bar offers, sorted.
pub fn offered(state: &WindowState) -> Vec<PathBuf> {
    recovery(state).map_or_else(Vec::new, |rec| {
        rec.offers().into_iter().map(|(path, _)| path).collect()
    })
}

/// The bar's text while it is shown.
pub fn shown(state: &WindowState) -> Option<String> {
    let rec = recovery(state)?;
    rec.bar
        .revealer
        .reveals_child()
        .then(|| rec.bar.label.text().to_string())
}

/// The bar's Review: applies every offered text as hunks of one "Recovered changes" review turn,
/// shows the first hunk and saves the documents that had no other unsaved changes. Resolves to
/// the number of hunks of the documents it changed.
pub fn review(state: &Rc<WindowState>) -> impl Future<Output = usize> + use<> {
    let (rec, state) = (recovery(state), state.clone());
    async move {
        let Some(rec) = rec else {
            return 0;
        };
        let mut turn = None;
        let mut changed: Vec<DocRef> = Vec::new();
        let mut to_save: Vec<DocRef> = Vec::new();
        for (path, offer) in rec.take_offers() {
            let doc = offer.doc.upgrade().filter(|doc| {
                state
                    .workspace
                    .get(doc.path())
                    .is_some_and(|d| Rc::ptr_eq(&d, doc))
            });
            let Some(doc) = doc else {
                continue;
            };
            let clean = !doc.is_dirty();
            let edits = doc.with_text(|text| lemma_review::minimal_edits(text, &offer.file.text));
            if edits.is_empty() {
                rec.queue(&path, Op::Remove);
                continue;
            }
            let turn = *turn.get_or_insert_with(|| review_ui::begin_named_turn(&state, TURN_NAME));
            review_ui::start_review(&doc, false);
            let applied = review_ui::in_turn(&doc, turn, || {
                doc.apply_edits(&edits, Origin::Agent { call: 0 })
            });
            if let Err(e) = applied {
                // The file stays for the next time the document opens.
                state.toast(&format!(
                    "Could not recover the changes to “{}”: {e}",
                    doc.display_name()
                ));
                continue;
            }
            review_ui::recompute(&doc);
            // The file holds the document's text now: this window takes it over.
            rec.entries.borrow_mut().insert(
                path,
                Entry::Written {
                    version: Some(doc.version()),
                },
            );
            if clean {
                to_save.push(doc.clone());
            }
            changed.push(doc);
        }
        let first = changed.iter().find_map(|doc| {
            let line = doc
                .review
                .borrow()
                .as_ref()
                .and_then(|r| r.hunks().first().map(|h| h.cur_lines.start))?;
            Some((doc.clone(), line))
        });
        if let Some((doc, line)) = first {
            review_ui::show_line(&state, &doc, line);
        }
        // Disk holds the proposed text, as after an agent's edit (ARCHITECTURE §4.5).
        for doc in to_save {
            match doc.save().await {
                Ok(()) => {}
                Err(SaveError::ExternallyModified) => state.offer_overwrite(&doc),
                Err(e) => state.toast(&format!("Could not save “{}”: {e}", doc.display_name())),
            }
        }
        rec.changed_if_dirty();
        changed.iter().map(|doc| review_ui::hunk_count(doc)).sum()
    }
}

/// The bar's Discard: removes every offered file.
pub fn discard_offered(state: &WindowState) {
    let Some(rec) = recovery(state) else {
        return;
    };
    for (path, _) in rec.take_offers() {
        rec.queue(&path, Op::Remove);
    }
    rec.changed_if_dirty();
}

impl Bar {
    fn new(state: &WindowState) -> Self {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        // No mnemonics: Alt+D is the review's reading mode, and Alt+R the review banner's.
        let review = gtk::Button::builder()
            .label("Review")
            .css_classes(["suggested-action"])
            .build();
        let discard = gtk::Button::builder().label("Discard").build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["lemma-recovery"])
            .build();
        row.append(&label);
        row.append(&review);
        row.append(&discard);
        let revealer = gtk::Revealer::builder()
            .child(&row)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();
        // Under the header bar, the review banner and the file conflict bar.
        match state
            .toasts
            .child()
            .and_then(|c| c.downcast::<adw::ToolbarView>().ok())
        {
            Some(toolbar) => toolbar.add_top_bar(&revealer),
            None => tracing::warn!("no toolbar view for the recovery bar"),
        }
        Self { revealer, label }
    }

    /// The bar's buttons, in order.
    fn buttons(&self) -> Vec<gtk::Button> {
        let Some(row) = self.revealer.child() else {
            return Vec::new();
        };
        std::iter::successors(row.first_child(), |w| w.next_sibling())
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .collect()
    }
}

impl Recovery {
    /// True when copies are written and recovery files read: windows with `Paths` that do not
    /// quit by themselves ([`set_enabled`] overrides that, for tests), while `[editor] recovery`
    /// is on.
    fn enabled(&self, state: &WindowState) -> bool {
        let wanted = self
            .enabled
            .get()
            .unwrap_or(!state.scripted && state.paths.is_some());
        wanted && state.paths.is_some() && state.config.editor.recovery
    }

    fn connect_bar(self: &Rc<Self>) {
        let buttons = self.bar.buttons();
        let [review_button, discard_button] = buttons.as_slice() else {
            return;
        };
        let weak = Rc::downgrade(self);
        review_button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade().and_then(|rec| rec.state.upgrade()) {
                glib::spawn_future_local(async move {
                    review(&state).await;
                });
            }
        });
        let weak = Rc::downgrade(self);
        discard_button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade().and_then(|rec| rec.state.upgrade()) {
                discard_offered(&state);
            }
        });
    }

    /// A document of the window changed (each keystroke: only the time is noted).
    fn changed(self: &Rc<Self>) {
        let now = Instant::now();
        self.last_change.set(Some(now));
        if self.first_change.get().is_none() {
            self.first_change.set(Some(now));
        }
        if self.timer.borrow().is_none() && self.state.upgrade().is_some_and(|s| self.enabled(&s)) {
            self.arm(self.delay.get());
        }
    }

    /// [`Recovery::changed`] when a document of the window has unsaved changes (an offer was
    /// answered: copies of its document were held back meanwhile).
    fn changed_if_dirty(self: &Rc<Self>) {
        if self.documents().iter().any(|doc| doc.is_dirty()) {
            self.changed();
        }
    }

    fn arm(self: &Rc<Self>, after: Duration) {
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(after, move || {
            if let Some(rec) = weak.upgrade() {
                rec.timer.take();
                rec.due();
            }
        });
        self.timer.replace(Some(id));
    }

    fn cancel_timer(&self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
    }

    /// The timer ran out: copy now, or wait for the rest of the delay.
    fn due(self: &Rc<Self>) {
        let (Some(first), Some(last)) = (self.first_change.get(), self.last_change.get()) else {
            return;
        };
        let (delay, max) = (self.delay.get(), self.max_delay.get());
        match wait_left(first, last, Instant::now(), delay, max) {
            None => self.copy_now(),
            Some(left) => self.arm(left),
        }
    }

    /// Copies every edited document of the window that changed since its last copy.
    fn copy_now(&self) {
        self.cancel_timer();
        self.first_change.set(None);
        self.last_change.set(None);
        if !self.state.upgrade().is_some_and(|s| self.enabled(&s)) {
            return;
        }
        for doc in self.documents() {
            self.copy(&doc);
        }
    }

    /// The documents of the attached editors.
    fn documents(&self) -> Vec<DocRef> {
        let mut docs: Vec<DocRef> = Vec::new();
        for w in self.watched.borrow().iter() {
            if !docs.iter().any(|d| Rc::ptr_eq(d, &w.doc)) {
                docs.push(w.doc.clone());
            }
        }
        docs
    }

    /// Queues a copy of `doc`'s text if it has unsaved changes that its file does not hold yet
    /// (nothing while its file is read or offered), or the removal of its file if it has none.
    fn copy(&self, doc: &DocRef) {
        let written = match self.entries.borrow().get(doc.path()) {
            Some(Entry::Checking | Entry::Offered(_)) => return,
            Some(Entry::Discarded { version }) if *version == doc.version() => return,
            Some(Entry::Written { version }) => Some(*version),
            Some(Entry::Discarded { .. }) | None => None,
        };
        if !doc.is_dirty() {
            self.entries.borrow_mut().remove(doc.path());
            if written.is_some() {
                self.queue(doc.path(), Op::Remove);
            }
            return;
        }
        if written == Some(Some(doc.version())) {
            return;
        }
        let text = match doc.buffer() {
            Some(buffer) => document::buffer_text(&buffer),
            None => doc.text().into(),
        };
        let snapshot = Snapshot {
            path: doc.path().to_path_buf(),
            disk_hash: doc.saver.disk_hash(),
            saved_at: unix_now(),
            text,
        };
        self.entries.borrow_mut().insert(
            doc.path().to_path_buf(),
            Entry::Written {
                version: Some(doc.version()),
            },
        );
        self.queue(doc.path(), Op::Write(snapshot));
    }

    /// `doc` has no unsaved changes any more: its copy goes.
    fn cleaned(&self, doc: &Document) {
        let entry = self.entries.borrow().get(doc.path()).map(|e| match e {
            Entry::Written { .. } => Some(true),
            Entry::Discarded { .. } => Some(false),
            Entry::Checking | Entry::Offered(_) => None,
        });
        if let Some(Some(written)) = entry {
            self.entries.borrow_mut().remove(doc.path());
            if written {
                self.queue(doc.path(), Op::Remove);
            }
        }
    }

    /// `[editor] recovery` was turned off: the window's copies go (an offer stays in the bar).
    fn switched_off(&self) {
        self.cancel_timer();
        let written: Vec<PathBuf> = self
            .entries
            .borrow()
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Written { .. }))
            .map(|(path, _)| path.clone())
            .collect();
        for path in written {
            self.entries.borrow_mut().remove(&path);
            self.queue(&path, Op::Remove);
        }
    }

    /// Queues `op` on the recovery file of the document at `path`.
    fn queue(&self, path: &Path, op: Op) {
        let Some(dir) = self
            .state
            .upgrade()
            .and_then(|s| s.paths.as_ref().map(Paths::recovery_dir))
        else {
            return;
        };
        let (weak, path) = (self.state.clone(), path.to_path_buf());
        WRITER.queue(file_for(&dir, &path), op, move |error| {
            if let Some(state) = weak.upgrade()
                && let Some(rec) = recovery(&state)
            {
                rec.failed(&state, &path, &error);
            }
        });
    }

    /// Writing or removing the file of the document at `path` failed: the next copy writes it
    /// again; the user is told once.
    fn failed(&self, state: &WindowState, path: &Path, error: &str) {
        tracing::warn!("recovery: {error}");
        if let Some(Entry::Written { version }) = self.entries.borrow_mut().get_mut(path) {
            *version = None;
        }
        if !self.reported.replace(true) {
            state.toast(&format!(
                "Could not keep a copy of the unsaved changes: {error}"
            ));
        }
    }

    /// Reads `doc`'s recovery file on the jobs thread, and whether its writer still runs (see
    /// [`Recovery::checked`]).
    fn check(self: &Rc<Self>, state: &WindowState, doc: &DocRef) {
        let Some(dir) = state.paths.as_ref().map(Paths::recovery_dir) else {
            return;
        };
        let file = file_for(&dir, doc.path());
        self.entries
            .borrow_mut()
            .insert(doc.path().to_path_buf(), Entry::Checking);
        let (weak, weak_doc) = (Rc::downgrade(self), Rc::downgrade(doc));
        glib::spawn_future_local(async move {
            let shown = file.display().to_string();
            let read = gio::spawn_blocking(move || {
                let found = read_file(&file)?;
                Ok(found.map(|found| {
                    let runs = found.pid.is_some_and(|pid| is_running(&dir, pid));
                    (found, runs)
                }))
            })
            .await
            .unwrap_or_else(|_| Err("the reader panicked".to_owned()));
            if let (Some(rec), Some(doc)) = (weak.upgrade(), weak_doc.upgrade()) {
                rec.checked(&doc, &shown, read);
            }
        });
    }

    /// `doc`'s recovery file was read: offers it, removes it, or leaves it alone.
    fn checked(
        self: &Rc<Self>,
        doc: &DocRef,
        shown: &str,
        read: Result<Option<(RecoveryFile, bool)>, String>,
    ) {
        // Closed (or read again) meanwhile.
        if !matches!(self.entries.borrow().get(doc.path()), Some(Entry::Checking)) {
            return;
        }
        self.entries.borrow_mut().remove(doc.path());
        let found = match read {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("recovery: ignoring {shown}: {e}");
                None
            }
        };
        if let Some((file, writer_runs)) = found {
            let disk_hash = doc.saver.disk_hash();
            let found =
                doc.with_text(|text| decide(&file, doc.path(), disk_hash, text, writer_runs));
            match found {
                Found::Offer { changed } => {
                    tracing::info!("recovery: offering {shown} for {}", doc.path().display());
                    self.entries.borrow_mut().insert(
                        doc.path().to_path_buf(),
                        Entry::Offered(Offer {
                            doc: Rc::downgrade(doc),
                            file,
                            changed,
                        }),
                    );
                    self.refresh_bar();
                }
                Found::Remove => self.queue(doc.path(), Op::Remove),
                Found::Ignore if writer_runs => {
                    tracing::info!("recovery: {shown} is kept by process {:?}", file.pid);
                }
                Found::Ignore => tracing::info!("recovery: {shown} belongs to {}", file.path),
            }
        }
        // Changes made while the file was read were not copied.
        if doc.is_dirty() {
            self.changed();
        }
    }

    /// The offered documents, sorted by path, with their offers' labels.
    fn offers(&self) -> Vec<(PathBuf, OfferLabel)> {
        let Some(state) = self.state.upgrade() else {
            return Vec::new();
        };
        let now = unix_now();
        let mut offers: Vec<(PathBuf, OfferLabel)> = self
            .entries
            .borrow()
            .iter()
            .filter_map(|(path, entry)| match entry {
                Entry::Offered(offer) => Some((
                    path.clone(),
                    OfferLabel {
                        name: review_ui::relative_name(&state, path),
                        age: now.saturating_sub(offer.file.saved_at),
                        changed: offer.changed,
                    },
                )),
                _ => None,
            })
            .collect();
        offers.sort_by(|a, b| a.0.cmp(&b.0));
        offers
    }

    /// Takes every offer out (sorted by path) and hides the bar.
    fn take_offers(&self) -> Vec<(PathBuf, Offer)> {
        let mut taken: Vec<(PathBuf, Offer)> = self
            .entries
            .borrow_mut()
            .extract_if(|_, e| matches!(e, Entry::Offered(_)))
            .filter_map(|(path, e)| match e {
                Entry::Offered(offer) => Some((path, offer)),
                _ => None,
            })
            .collect();
        taken.sort_by(|a, b| a.0.cmp(&b.0));
        self.refresh_bar();
        taken
    }

    /// Shows the bar with the offers, or hides it without any.
    fn refresh_bar(&self) {
        let offers: Vec<OfferLabel> = self.offers().into_iter().map(|(_, o)| o).collect();
        if !offers.is_empty() {
            self.bar.label.set_text(&offer_text(&offers));
        }
        self.bar.revealer.set_reveal_child(!offers.is_empty());
    }
}

// ----- pruning -----

/// Sweeps the recovery directory once per process, on the jobs thread (see [`prune_dir`]). The
/// app calls it once start-up has settled, in interactive runs only.
pub fn prune_once(paths: &Paths) {
    static PRUNED: AtomicBool = AtomicBool::new(false);
    if PRUNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let dir = paths.recovery_dir();
    glib::spawn_future_local(async move {
        let now = SystemTime::now();
        match gio::spawn_blocking(move || prune_dir(&dir, now)).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("recovery: removed {n} old files"),
            Err(_) => tracing::warn!("recovery: removing old files panicked"),
        }
    });
}

/// Removes from the recovery directory `dir`, as of `now`: temporary files older than
/// [`STALE_TMP_AGE`], lock files as old that no process holds, and recovery files not written
/// for [`PRUNE_AGE`]. Returns how many files were removed. Blocking: for the jobs thread.
pub fn prune_dir(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default();
        let stale = if name.ends_with(".tmp") {
            age > STALE_TMP_AGE
        } else if name.ends_with(".lock") {
            // Of a process that ended: a running one holds its lock (where the file system has
            // locks at all).
            age > STALE_TMP_AGE
                && std::fs::File::open(&path).is_ok_and(|file| {
                    !matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
                })
        } else {
            name.ends_with(".json") && age > PRUNE_AGE
        };
        if stale {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) => tracing::info!("recovery: cannot remove {}: {e}", path.display()),
            }
        }
    }
    removed
}

// ----- signals -----

/// Calls `f(signal)` on the main thread when the process gets `SIGTERM`, `SIGHUP` or `SIGINT`,
/// instead of letting the signal end it (GLib's Unix signal source: the handler itself only
/// wakes the main loop). Once per process; the app connects it for runs that do not quit by
/// themselves.
#[cfg(unix)]
pub fn connect_termination(f: impl Fn(i32) + 'static) {
    signals::connect(f);
}

#[cfg(unix)]
mod signals {
    use std::cell::RefCell;
    use std::rc::Rc;

    unsafe extern "C" {
        /// GLib's `g_unix_signal_add_full` (`glib-unix.h`, in libglib-2.0, which the GTK stack
        /// links), which the Rust bindings no longer wrap.
        fn g_unix_signal_add_full(
            priority: libc::c_int,
            signum: libc::c_int,
            handler: glib::ffi::GSourceFunc,
            user_data: glib::ffi::gpointer,
            notify: glib::ffi::GDestroyNotify,
        ) -> libc::c_uint;
    }

    type Handler = dyn Fn(i32);

    thread_local! {
        static HANDLER: RefCell<Option<Rc<Handler>>> = const { RefCell::new(None) };
    }

    /// The source's callback; `data` is the signal number.
    unsafe extern "C" fn dispatch(data: glib::ffi::gpointer) -> glib::ffi::gboolean {
        let signal = i32::try_from(data.addr()).unwrap_or_default();
        if let Some(f) = HANDLER.with_borrow(Clone::clone) {
            f(signal);
        }
        // Keep the source: a second signal is handled too.
        glib::ffi::GTRUE
    }

    pub(super) fn connect(f: impl Fn(i32) + 'static) {
        let first = HANDLER.with_borrow_mut(|h| h.replace(Rc::new(f)).is_none());
        if !first {
            return;
        }
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            // SAFETY: `dispatch` matches `GSourceFunc`, its data is a plain integer (no
            // destroy notify needed), and the source is attached to the default main context,
            // which the GTK main thread (this one) runs, so `HANDLER` is read on this thread.
            unsafe {
                g_unix_signal_add_full(
                    glib::ffi::G_PRIORITY_HIGH,
                    signal,
                    Some(dispatch),
                    std::ptr::without_provenance_mut(signal.unsigned_abs() as usize),
                    None,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_round_trip_and_are_named_by_the_path() {
        let path = Path::new("/home/u/thesis/main.tex");
        let text = "\\section{Intro}\r\n“quoted” \\\\ \"x\"\n";
        let bytes = encode(path, Some(u64::MAX - 1), 1_700_000_000, text).unwrap();
        let file = parse(&bytes).unwrap();
        assert_eq!(
            file,
            RecoveryFile {
                version: VERSION,
                path: "/home/u/thesis/main.tex".into(),
                disk_hash: Some(u64::MAX - 1),
                saved_at: 1_700_000_000,
                pid: Some(std::process::id()),
                text: text.into(),
            }
        );
        // A new file that was never saved has no hash.
        let new = parse(&encode(path, None, 1, "x").unwrap()).unwrap();
        assert_eq!(new.disk_hash, None);
        // Only the path and the text are required.
        let bare = parse(br#"{"path": "/a.tex", "text": "t"}"#).unwrap();
        assert_eq!(
            (bare.version, bare.saved_at, bare.disk_hash, bare.pid),
            (0, 0, None, None)
        );
        assert!(parse(b"{\"path\": \"/a.tex\"}").is_err());
        assert!(parse(b"not json").is_err());
        assert!(parse(b"").is_err());

        let dir = Path::new("/state/recovery");
        let a = file_for(dir, path);
        assert_eq!(a.parent(), Some(dir));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("json"));
        assert_eq!(a.file_stem().map(|s| s.len()), Some(16));
        assert_ne!(a, file_for(dir, Path::new("/home/u/thesis/other.tex")));
    }

    #[test]
    fn found_files_are_offered_removed_or_ignored() {
        let path = Path::new("/p/main.tex");
        let file = RecoveryFile {
            version: VERSION,
            path: "/p/main.tex".into(),
            disk_hash: Some(7),
            saved_at: 0,
            pid: Some(1),
            text: "unsaved".into(),
        };
        assert_eq!(
            decide(&file, path, Some(7), "saved", false),
            Found::Offer { changed: false }
        );
        // The file changed on disk since, or is gone.
        assert_eq!(
            decide(&file, path, Some(8), "other", false),
            Found::Offer { changed: true }
        );
        assert_eq!(
            decide(&file, path, None, "", false),
            Found::Offer { changed: true }
        );
        // Nothing the document lacks.
        assert_eq!(
            decide(&file, path, Some(8), "unsaved", false),
            Found::Remove
        );
        // Another document whose path hashes the same; a text no document can hold; the copy of
        // a document that another Lemma, still running, has open.
        assert_eq!(
            decide(&file, Path::new("/p/other.tex"), Some(7), "saved", false),
            Found::Ignore
        );
        let nul = RecoveryFile {
            text: "a\0b".into(),
            ..file.clone()
        };
        assert_eq!(decide(&nul, path, Some(7), "saved", false), Found::Ignore);
        assert_eq!(decide(&file, path, Some(7), "saved", true), Found::Ignore);
        // A new file that was never saved, opened again while it still does not exist.
        let new = RecoveryFile {
            disk_hash: None,
            ..file
        };
        assert_eq!(
            decide(&new, path, None, "", false),
            Found::Offer { changed: false }
        );
    }

    #[test]
    fn bar_texts_read_well() {
        assert_eq!(relative_time(0), "less than a minute ago");
        assert_eq!(relative_time(59), "less than a minute ago");
        assert_eq!(relative_time(60), "1 minute ago");
        assert_eq!(relative_time(5 * 60 + 30), "5 minutes ago");
        assert_eq!(relative_time(3_600), "1 hour ago");
        assert_eq!(relative_time(23 * 3_600), "23 hours ago");
        assert_eq!(relative_time(30 * 3_600), "yesterday");
        assert_eq!(relative_time(3 * 86_400), "3 days ago");
        let label = |name: &str, age: u64, changed: bool| OfferLabel {
            name: name.into(),
            age,
            changed,
        };
        assert_eq!(offer_text(&[]), "");
        assert_eq!(
            offer_text(&[label("main.tex", 300, false)]),
            "Unsaved changes to “main.tex” from 5 minutes ago were recovered"
        );
        assert_eq!(
            offer_text(&[label("ch/a.tex", 10, true)]),
            "Unsaved changes to “ch/a.tex” from less than a minute ago were recovered; \
             the file changed on disk since"
        );
        let two = [label("a.tex", 7_200, false), label("b.tex", 4_000, true)];
        assert_eq!(
            offer_text(&two),
            "Unsaved changes to 2 documents from 1 hour ago were recovered: a.tex, b.tex; \
             “b.tex” changed on disk since"
        );
        let three = [
            label("a.tex", 60, true),
            label("b.tex", 60, true),
            label("c.tex", 60, false),
        ];
        assert_eq!(
            offer_text(&three),
            "Unsaved changes to 3 documents from 1 minute ago were recovered: a.tex, b.tex, \
             c.tex; 2 of the files changed on disk since"
        );
        assert!(offer_text(&three[..2]).ends_with("; the files changed on disk since"));
    }

    #[test]
    fn copies_wait_for_a_pause_but_not_forever() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let (delay, max) = (s(5), s(30));
        // One change: 5 s after it.
        assert_eq!(wait_left(t0, t0, t0, delay, max), Some(s(5)));
        assert_eq!(wait_left(t0, t0, t0 + s(2), delay, max), Some(s(3)));
        assert_eq!(wait_left(t0, t0, t0 + s(5), delay, max), None);
        // Typing on: the wait starts again from the last change …
        assert_eq!(wait_left(t0, t0 + s(4), t0 + s(5), delay, max), Some(s(4)));
        // … but never past `max` after the first change.
        assert_eq!(
            wait_left(t0, t0 + s(28), t0 + s(29), delay, max),
            Some(s(1))
        );
        assert_eq!(wait_left(t0, t0 + s(29), t0 + s(30), delay, max), None);
        // A clock read before the change (cannot happen) counts as no time at all.
        assert_eq!(wait_left(t0 + s(1), t0 + s(1), t0, delay, max), Some(s(5)));
    }

    #[test]
    fn the_writer_lets_the_newest_operation_win() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recovery").join("0123456789abcdef.json");
        let snapshot = |text: &str| {
            Op::Write(Snapshot {
                path: PathBuf::from("/p/main.tex"),
                disk_hash: Some(1),
                saved_at: 2,
                text: text.into(),
            })
        };
        let writer = Writer::default();
        lock(&writer.queued).insert(file.clone(), snapshot("old"));
        lock(&writer.queued).insert(file.clone(), snapshot("new"));
        writer.run(&file).unwrap();
        assert_eq!(parse(&std::fs::read(&file).unwrap()).unwrap().text, "new");
        // Run again (the job of the replaced operation): nothing is queued, nothing happens.
        writer.run(&file).unwrap();
        assert!(file.exists());
        // A removal queued after a copy wins over it, also when the copy's job runs first.
        lock(&writer.queued).insert(file.clone(), snapshot("newer"));
        lock(&writer.queued).insert(file.clone(), Op::Remove);
        writer.run(&file).unwrap();
        assert!(!file.exists());
        writer.run(&file).unwrap();
        assert!(!file.exists());
        // Removing a file that is not there is fine; a flush runs what is queued.
        lock(&writer.queued).insert(file.clone(), Op::Remove);
        writer.flush();
        lock(&writer.queued).insert(file.clone(), snapshot("flushed"));
        writer.flush();
        assert_eq!(
            parse(&std::fs::read(&file).unwrap()).unwrap().text,
            "flushed"
        );
        // Private to the user, with no temporary file left behind.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        // The process's lock is the only other file, and it holds it.
        let dir = file.parent().unwrap();
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        let own = lock_file(dir, std::process::id());
        assert_eq!(names, [file.clone(), own.clone()]);
        let probe = std::fs::File::open(&own).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
    }

    #[test]
    fn running_writers_are_known_by_their_lock() {
        let dir = tempfile::tempdir().unwrap();
        let pid = std::process::id().wrapping_add(1);
        assert!(!is_running(dir.path(), pid), "no lock file");
        let held = std::fs::File::create(lock_file(dir.path(), pid)).unwrap();
        assert!(!is_running(dir.path(), pid), "a lock file nobody holds");
        held.lock().unwrap();
        assert!(is_running(dir.path(), pid));
        drop(held);
        assert!(!is_running(dir.path(), pid), "the process ended");
        // This process's own copies are never another's.
        let own = std::fs::File::create(lock_file(dir.path(), std::process::id())).unwrap();
        own.lock().unwrap();
        assert!(!is_running(dir.path(), std::process::id()));
    }

    #[test]
    fn pruning_removes_stale_temporaries_and_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("0000000000000001.json");
        let tmp = dir.path().join("0000000000000001.json.123.0.tmp");
        let other = dir.path().join("notes.txt");
        let (ended, running) = (lock_file(dir.path(), 123), lock_file(dir.path(), 124));
        for file in [&fresh, &tmp, &other, &ended, &running] {
            std::fs::write(file, "x").unwrap();
        }
        let held = std::fs::File::open(&running).unwrap();
        held.lock().unwrap();
        let now = SystemTime::now();
        assert_eq!(prune_dir(dir.path(), now), 0);
        // Eleven minutes on: the temporary file of an interrupted write goes, and the lock of a
        // process that ended; a running process keeps its lock.
        assert_eq!(prune_dir(dir.path(), now + Duration::from_secs(11 * 60)), 2);
        assert!(!tmp.exists() && !ended.exists());
        assert!(fresh.exists() && running.exists());
        // Half a year on: the recovery file goes; other files stay.
        assert_eq!(
            prune_dir(dir.path(), now + PRUNE_AGE + Duration::from_secs(60)),
            1
        );
        assert!(!fresh.exists() && other.exists());
        assert_eq!(prune_dir(&dir.path().join("missing"), now), 0);
    }
}
