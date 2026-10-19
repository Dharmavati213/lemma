//! Per-document asynchronous saving (ARCHITECTURE §10.5), and the decoding of files when they are
//! read.
//!
//! Every save, of an open or a closed document, writes a **snapshot** of the text taken when the
//! write starts (typing during the write is not part of it; the document stays dirty), encoded
//! with the charset the file was read with, **byte for byte**: the text keeps the file's line
//! terminators (`\n`, `\r\n`, `\r`, mixed) and any byte-order mark, so nothing is rewritten that
//! the user did not change. The write is an atomic replace (`g_file_replace_contents`).
//!
//! **External changes.** The saver remembers what the file held when it was last read or written
//! (GIO's entity tag, the size, and the `fnv1a64` hash of the bytes). Before writing it compares
//! the file's current tag and size with that; when they differ it reads the file and compares
//! the hash, so a file another program changed is refused with
//! [`SaveError::ExternallyModified`] (whatever its timestamps say), while a file that was only
//! touched is written. [`Document::save_overwriting`] skips the check.
//!
//! Saves are **coalesced**: while one save runs, any number of new requests share a single
//! follow-up save, and every request resolves once a save that *started after it* has finished.
//! So after `doc.save().await` the disk holds at least the text the document had when `save` was
//! called.
//!
//! The saver also remembers the hash (`fnv1a64` of the bytes written) of its latest write, and of a
//! write still running, so the file monitor (WP2.4) can tell our own writes from external ones. The
//! hash is recorded before the write starts, because the monitor may report the change before the
//! write completes. Only those count as ours: once a later write completes, or a version another
//! program wrote is recorded ([`Saver::set_loaded`]), a file holding an **earlier** own write (a
//! `git checkout` of a version Lemma saved before) is a change like any other.
//!
//! [`connect_saved`] observes every successful save of every document (local history, WP2.4).

use crate::document::{DocRef, Document};
use crate::hooks::{HookId, Hooks};
use gio::prelude::*;
use lemma_core::fnv1a64;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::rc::{Rc, Weak};

type SavedFn = dyn Fn(&Path, &str);

thread_local! {
    static SAVED: Hooks<SavedFn> = Hooks::default();
}

/// Calls `f(path, text)` after every successful save of any document (open or closed, by the
/// user, a build, an agent edit or a reject), with the document's path and the text that was
/// written (decoded; the snapshot taken when the write started). Runs on the main thread, after
/// the document was marked saved. Returns an id for [`disconnect_saved`].
pub fn connect_saved(f: impl Fn(&Path, &str) + 'static) -> HookId {
    SAVED.with(|hooks| hooks.add(Rc::new(f)))
}

/// Removes a callback added by [`connect_saved`].
pub fn disconnect_saved(id: HookId) {
    SAVED.with(|hooks| hooks.remove(id));
}

fn saved_observed() -> bool {
    SAVED.with(|hooks| !hooks.is_empty())
}

fn notify_saved(path: &Path, text: &str) {
    for f in SAVED.with(Hooks::snapshot) {
        f(path, text);
    }
}

type SavingFn = dyn Fn(&Document);

thread_local! {
    static SAVING: Hooks<SavingFn> = Hooks::default();
}

/// Calls `f(doc)` when a save of `doc` takes the snapshot of the text it writes (before the
/// write), so that state belonging to exactly that text can be captured; [`connect_saved`] says
/// whether the write succeeded. Runs on the main thread. Returns an id for
/// [`disconnect_saving`].
pub fn connect_saving(f: impl Fn(&Document) + 'static) -> HookId {
    SAVING.with(|hooks| hooks.add(Rc::new(f)))
}

/// Removes a callback added by [`connect_saving`].
pub fn disconnect_saving(id: HookId) {
    SAVING.with(|hooks| hooks.remove(id));
}

fn notify_saving(doc: &Document) {
    for f in SAVING.with(Hooks::snapshot) {
        f(doc);
    }
}

/// The charset of a file that is not valid UTF-8 (GtkSourceView's fallback candidate, a superset
/// of Latin-1 for text).
pub const FALLBACK_CHARSET: &str = "ISO-8859-15";

/// The file attributes compared before a save.
const DISK_ATTRIBUTES: &str = "etag::value,standard::size";

/// Why a save failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The file changed on disk since it was loaded or last saved; nothing was written.
    /// [`Document::save_overwriting`] writes anyway.
    ExternallyModified,
    /// Any other failure, with GIO's message.
    Io(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternallyModified => f.write_str("the file was changed by another program"),
            Self::Io(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<glib::Error> for SaveError {
    fn from(e: glib::Error) -> Self {
        if e.matches(sourceview5::FileSaverError::ExternallyModified)
            || e.matches(gio::IOErrorEnum::WrongEtag)
        {
            Self::ExternallyModified
        } else {
            Self::Io(e.message().to_owned())
        }
    }
}

/// What the file held when the document last read or wrote it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiskState {
    /// Not known (a document that was not read from disk): saves do not check the file.
    #[default]
    Unknown,
    /// The file did not exist.
    Absent,
    /// The file held `size` bytes hashing to `hash`; `etag` is GIO's entity tag of that version.
    Present {
        etag: Option<String>,
        size: u64,
        hash: u64,
    },
}

impl DiskState {
    /// The state of a file that holds `bytes`, with entity tag `etag`.
    pub(crate) fn of(bytes: &[u8], etag: Option<glib::GString>) -> Self {
        Self::Present {
            etag: etag.map(String::from),
            size: bytes.len() as u64,
            hash: fnv1a64(&[bytes]),
        }
    }
}

/// A file's text and the charset it was decoded from (`None` for UTF-8).
pub(crate) struct Decoded {
    pub text: String,
    pub charset: Option<String>,
}

/// Decodes a file's bytes: with `charset` when given, else as UTF-8 when valid, else as
/// [`FALLBACK_CHARSET`]. The bytes are kept exactly (line terminators, a byte-order mark). Refuses
/// text containing U+0000, which no document can hold (a binary file).
pub(crate) fn decode(bytes: &[u8], charset: Option<&str>) -> Result<Decoded, String> {
    let decoded = match charset {
        Some(charset) => Decoded {
            text: convert(bytes, "UTF-8", charset)?,
            charset: Some(charset.to_owned()),
        },
        None => match std::str::from_utf8(bytes) {
            Ok(text) => Decoded {
                text: text.to_owned(),
                charset: None,
            },
            Err(_) => Decoded {
                text: convert(bytes, "UTF-8", FALLBACK_CHARSET)?,
                charset: Some(FALLBACK_CHARSET.to_owned()),
            },
        },
    };
    if decoded.text.contains('\0') {
        return Err("it contains NUL characters (is it a binary file?)".into());
    }
    Ok(decoded)
}

/// `bytes` (text in `from`) converted to `to`, which is UTF-8.
fn convert(bytes: &[u8], to: &str, from: &str) -> Result<String, String> {
    let (converted, _) =
        glib::convert(bytes, to, from).map_err(|e| format!("it cannot be read as {from}: {e}"))?;
    String::from_utf8(converted.to_vec()).map_err(|_| format!("it cannot be read as {from}"))
}

type Waiter = async_channel::Sender<Result<(), SaveError>>;

#[derive(Default)]
struct State {
    /// Number of save requests so far; request `n` is satisfied by a save that started after it.
    requested: u64,
    /// The highest request satisfied by a finished save.
    completed: u64,
    running: bool,
    overwrite: bool,
    waiters: Vec<(u64, Waiter)>,
}

/// The hashes of the bytes this document wrote that its file may hold now.
#[derive(Clone, Copy, Debug, Default)]
struct OwnWrites {
    /// The latest completed write, until a version another program wrote is recorded.
    latest: Option<u64>,
    /// The write running now (saves of one document never overlap).
    running: Option<u64>,
}

/// The save state of one document (owned by [`Document`]).
#[derive(Default)]
pub struct Saver {
    state: RefCell<State>,
    own_writes: Cell<OwnWrites>,
    /// The charset the file is written in; `None` for UTF-8.
    charset: RefCell<Option<String>>,
    disk: RefCell<DiskState>,
}

impl Saver {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Queues a save of `doc` and returns a future for its result.
    pub(crate) fn request(
        &self,
        doc: &DocRef,
        overwrite: bool,
    ) -> impl Future<Output = Result<(), SaveError>> + use<> {
        let (tx, rx) = async_channel::bounded(1);
        let start = {
            let mut st = self.state.borrow_mut();
            st.requested += 1;
            let ticket = st.requested;
            st.overwrite |= overwrite;
            st.waiters.push((ticket, tx));
            !std::mem::replace(&mut st.running, true)
        };
        if start {
            glib::spawn_future_local(run(Rc::downgrade(doc)));
        }
        async move {
            rx.recv().await.unwrap_or_else(|_| {
                Err(SaveError::Io(
                    "the document was closed before it was saved".into(),
                ))
            })
        }
    }

    /// True when `hash` is the hash of the latest text written, or of the text being written now.
    /// An earlier write does not count: the file holding it again means another program put it
    /// back (see the module docs).
    pub fn is_own_write(&self, hash: u64) -> bool {
        let writes = self.own_writes.get();
        writes.running == Some(hash) || writes.latest == Some(hash)
    }

    /// The hash of the most recent write (the running one first), if it still counts as ours.
    pub fn last_write(&self) -> Option<u64> {
        let writes = self.own_writes.get();
        writes.running.or(writes.latest)
    }

    /// The charset the file is written in (`None` for UTF-8).
    pub fn charset(&self) -> Option<String> {
        self.charset.borrow().clone()
    }

    /// The `fnv1a64` hash of the bytes the file held when the document last read or wrote it,
    /// if known (the file monitor checks a merge base against it).
    pub(crate) fn disk_hash(&self) -> Option<u64> {
        match &*self.disk.borrow() {
            DiskState::Present { hash, .. } => Some(*hash),
            DiskState::Unknown | DiskState::Absent => None,
        }
    }

    /// Records what the file held when it was read: its charset and state on disk. That version
    /// is not ours, so the latest own write no longer counts as ours (a write still running
    /// does, until it completes).
    pub(crate) fn set_loaded(&self, charset: Option<String>, disk: DiskState) {
        self.charset.replace(charset);
        self.disk.replace(disk);
        let mut writes = self.own_writes.get();
        writes.latest = None;
        self.own_writes.set(writes);
    }

    /// A write of bytes hashing to `hash` starts.
    fn write_started(&self, hash: u64) {
        let mut writes = self.own_writes.get();
        writes.running = Some(hash);
        self.own_writes.set(writes);
    }

    /// The running write ended; `completed` when the file now holds its bytes (which replace
    /// every earlier write as "ours").
    fn write_ended(&self, completed: bool) {
        let mut writes = self.own_writes.get();
        let hash = writes.running.take();
        if completed {
            writes.latest = hash;
        }
        self.own_writes.set(writes);
    }

    /// `text` in the file's charset.
    fn encode(&self, text: &str) -> Result<Vec<u8>, SaveError> {
        match self.charset.borrow().as_deref() {
            None => Ok(text.as_bytes().to_vec()),
            Some(charset) => glib::convert(text.as_bytes(), charset, "UTF-8")
                .map(|(bytes, _)| bytes.to_vec())
                .map_err(|e| {
                    SaveError::Io(format!(
                        "the text cannot be written in the file's encoding, {charset}: {e}"
                    ))
                }),
        }
    }

    /// Checks that the file still holds what this document last read or wrote. Returns the entity
    /// tag to pass to `replace_contents` (which checks it once more), `None` for no check.
    async fn check_disk(&self, file: &gio::File) -> Result<Option<String>, SaveError> {
        let recorded = self.disk.borrow().clone();
        let (etag, size, hash) = match recorded {
            DiskState::Unknown => return Ok(None),
            DiskState::Absent => (None, None, None),
            DiskState::Present { etag, size, hash } => (etag, Some(size), Some(hash)),
        };
        let info = file
            .query_info_future(
                DISK_ATTRIBUTES,
                gio::FileQueryInfoFlags::NONE,
                glib::Priority::DEFAULT,
            )
            .await;
        let info = match info {
            Ok(info) => info,
            // Deleted (or still absent): writing creates it again.
            Err(e) if e.matches(gio::IOErrorEnum::NotFound) => return Ok(None),
            Err(e) => return Err(SaveError::Io(e.message().to_owned())),
        };
        let current = info.etag().map(String::from);
        let current_size = u64::try_from(info.size()).ok();
        if current.is_some() && current == etag && current_size == size {
            return Ok(current);
        }
        // Touched or changed: only the contents decide.
        let Some(hash) = hash else {
            return Err(SaveError::ExternallyModified);
        };
        let (bytes, etag) = file
            .load_contents_future()
            .await
            .map_err(|e| SaveError::Io(e.message().to_owned()))?;
        if fnv1a64(&[&bytes]) == hash {
            Ok(etag.map(String::from))
        } else {
            Err(SaveError::ExternallyModified)
        }
    }
}

/// Saves until every request is satisfied, then stops.
async fn run(doc: Weak<Document>) {
    loop {
        // A dropped document drops its waiters too; their futures resolve with an error.
        let Some(doc) = doc.upgrade() else {
            return;
        };
        let (ticket, overwrite) = {
            let mut st = doc.saver.state.borrow_mut();
            if st.completed >= st.requested {
                st.running = false;
                return;
            }
            (st.requested, std::mem::take(&mut st.overwrite))
        };
        let result = write(&doc, overwrite).await;
        if let Err(e) = &result {
            tracing::warn!("saving {} failed: {e}", doc.path().display());
        }
        let ready: Vec<Waiter> = {
            let mut st = doc.saver.state.borrow_mut();
            st.completed = ticket;
            let (ready, waiting): (Vec<_>, Vec<_>) =
                st.waiters.drain(..).partition(|(t, _)| *t <= ticket);
            st.waiters = waiting;
            ready.into_iter().map(|(_, tx)| tx).collect()
        };
        for tx in ready {
            // A waiter that stopped listening is fine.
            let _ = tx.try_send(result.clone());
        }
    }
}

async fn write(doc: &Document, overwrite: bool) -> Result<(), SaveError> {
    let saver = &doc.saver;
    // The snapshot: changes made while the write runs keep the document dirty.
    let version = doc.version();
    notify_saving(doc);
    // The decoded text is kept only for `connect_saved` callbacks.
    let observed = saved_observed();
    let (bytes, text) = doc.with_text(|text| {
        saver
            .encode(text)
            .map(|bytes| (bytes, observed.then(|| text.to_owned())))
    })?;
    let file = gio::File::for_path(doc.path());
    let etag = if overwrite {
        None
    } else {
        saver.check_disk(&file).await?
    };
    let hash = fnv1a64(&[&bytes]);
    let size = bytes.len() as u64;
    saver.write_started(hash);
    let written = file
        .replace_contents_future(bytes, etag.as_deref(), false, gio::FileCreateFlags::NONE)
        .await;
    saver.write_ended(written.is_ok());
    let (_, etag) = written.map_err(|(_, e)| SaveError::from(e))?;
    saver.disk.replace(DiskState::Present {
        etag: etag.map(String::from),
        size,
        hash,
    });
    doc.mark_saved(version);
    if let Some(text) = text {
        notify_saved(doc.path(), &text);
    }
    Ok(())
}
