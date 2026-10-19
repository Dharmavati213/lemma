//! One file's text, wherever it lives: a GtkSourceView buffer while an editor shows it, a plain
//! `String` (the "shadow") otherwise.
//!
//! Every change, whatever its source, passes through one pipeline:
//!
//! 1. **before** the change (buffer `insert-text`/`delete-range` before-handlers, or
//!    [`Document::apply_edits`] for a closed document) a [`TextEdit`] with pre-edit byte offsets is
//!    built; the review hook ([`crate::review_ui::before_edit`], only while a review exists) and
//!    the [`Document::connect_edit`] observers see it; then the [`LineIndex`] is updated;
//! 2. **after** the change the version counter increments, the review hook
//!    [`crate::review_ui::after_edit`] runs, and the [`Document::connect_lines_changed`]
//!    observers get the changed line range.
//!
//! While the document is open, the scanner's [`LineStates`] are kept exact for the text
//! (`line_states`): built when the buffer becomes the text (on the jobs thread from
//! [`ASYNC_SCAN_LINES`] lines on, synchronously below), then updated after every change with
//! `LineStates::apply_edit`; [`Document::context_at`] answers from them, and
//! [`Document::connect_retag`] reports the lines whose tokens may have changed.
//!
//! Each change carries the [`Origin`] in the document's origin cell: `User` by default,
//! `History` while a buffer `undo`/`redo` runs (set by the signal's before-handler, restored by its
//! after-handler), and whatever [`Document::with_origin`]/[`Document::apply_edits`] set around
//! our own changes.
//!
//! No text can hold U+0000 (GTK cannot): loading refuses such files and [`Document::apply_edits`]
//! refuses such edits.

use crate::saver::{self, DiskState, SaveError, Saver};
use gtk::prelude::*;
use lemma_core::{LineIndex, Origin, TextEdit};
use lemma_review::FileReview;
use lemma_syntax::{Context, LineStates, Lines, ScanConfig, StrLines};
use sourceview5::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, OnceCell, Ref, RefCell};
use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// Documents with at least this many lines build their scanner states on the jobs thread.
pub const ASYNC_SCAN_LINES: u32 = 10_000;

/// Shared handle to a [`Document`].
pub type DocRef = Rc<Document>;

/// Where a document's text lives.
enum DocKind {
    /// Shown in an editor; the buffer is the text.
    Open { buffer: sourceview5::Buffer },
    /// Not shown anywhere (e.g. a file an agent edited); the string is the text.
    Closed { shadow: String },
}

/// Handle returned by the `connect_*` methods, for [`Document::disconnect`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObserverId(u64);

/// An edit that does not fit the document's current text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The range is inverted, past the end, or not on `char` boundaries.
    InvalidRange(TextEdit),
    /// The edits are not in descending, non-overlapping order.
    NotDescending,
    /// The inserted text contains U+0000, which a document cannot hold.
    NulCharacter,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange(e) => write!(f, "edit range {:?} does not fit the text", e.range),
            Self::NotDescending => f.write_str("edits must be descending and non-overlapping"),
            Self::NulCharacter => f.write_str("the text contains a NUL character (U+0000)"),
        }
    }
}

impl std::error::Error for EditError {}

type EditObserver = dyn Fn(&Document, &TextEdit, Origin);
type LinesObserver = dyn Fn(u32, u32, u32);
type HunksObserver = dyn Fn();
type RetagObserver = dyn Fn(Range<u32>);
type ApplyObserver = dyn Fn(&Document, &[TextEdit], Origin);

/// A change between its before- and after-handler.
struct PendingChange {
    edit: TextEdit,
    /// `LineIndex::line_delta` of the edit: (first, removed, added).
    delta: (u32, u32, u32),
}

/// A rebuild of the scanner states running on the jobs thread.
struct PendingScan {
    generation: u64,
    /// The line changes made since its text was copied, composed into one (see
    /// [`compose_line_deltas`]).
    delta: Option<(u32, u32, u32)>,
}

/// A list of callbacks. Callbacks run on a snapshot of the list, so they may connect or disconnect.
struct Observers<F: ?Sized>(RefCell<Vec<(ObserverId, Rc<F>)>>);

impl<F: ?Sized> Default for Observers<F> {
    fn default() -> Self {
        Self(RefCell::new(Vec::new()))
    }
}

impl<F: ?Sized> Observers<F> {
    fn add(&self, id: ObserverId, f: Rc<F>) -> ObserverId {
        self.0.borrow_mut().push((id, f));
        id
    }

    fn remove(&self, id: ObserverId) {
        self.0.borrow_mut().retain(|(i, _)| *i != id);
    }

    fn snapshot(&self) -> Vec<Rc<F>> {
        self.0.borrow().iter().map(|(_, f)| f.clone()).collect()
    }
}

/// A file's text plus everything kept in sync with it (ARCHITECTURE §4.1, §10.5).
pub struct Document {
    path: PathBuf,
    kind: RefCell<DocKind>,
    /// Location, and for a document read by `Workspace::open` the encoding and newline type
    /// GtkSourceView's loader detected. (The saver keeps the charset it writes in itself.)
    file: sourceview5::File,
    line_index: RefCell<LineIndex>,
    /// Scanner states per line, exact for the text while the document is open (see the module
    /// docs); `None` while it is closed and while a rebuild runs on the jobs thread.
    pub line_states: RefCell<Option<LineStates>>,
    /// The scanner rules (VimTeX's plus `editor.math_envs`); see [`Document::set_scan_config`].
    scan_config: RefCell<Arc<ScanConfig>>,
    pending_scan: RefCell<Option<PendingScan>>,
    scan_generation: Cell<u64>,
    origin: Cell<Origin>,
    /// The origin to restore when the running undo/redo finishes.
    origin_before_history: Cell<Option<Origin>>,
    /// The agent-edit review of this file, if any (WP1.2 creates and drops it).
    pub review: RefCell<Option<FileReview>>,
    pub(crate) saver: Saver,
    /// Incremented after every change.
    version: Cell<u64>,
    /// For a closed document: the version that matches the file on disk; `None` when the shadow
    /// holds changes that were never saved.
    saved_version: Cell<Option<u64>>,
    /// The change between its before- and after-handler.
    pending: RefCell<Option<PendingChange>>,
    /// True while the buffer has a user action open (between the outermost `begin-user-action`
    /// and `end-user-action`); Vim's insert mode keeps one open for the whole insert session.
    user_action_open: Cell<bool>,
    /// A save of this version ended while a user action was open (see [`Document::mark_saved`]).
    saved_in_action: Cell<Option<u64>>,
    next_observer: Cell<u64>,
    edit_observers: Observers<EditObserver>,
    lines_observers: Observers<LinesObserver>,
    hunks_observers: Observers<HunksObserver>,
    retag_observers: Observers<RetagObserver>,
    changed_observers: Observers<EditObserver>,
    apply_observers: Observers<ApplyObserver>,
    buffer_handlers: RefCell<Vec<glib::SignalHandlerId>>,
}

impl Document {
    /// A document whose text is `buffer`'s current content, saved as UTF-8 without checking the
    /// file for external changes (`Workspace::open` records what it read). `file` is the one the
    /// buffer was loaded with, if any.
    pub fn new_open(path: PathBuf, buffer: sourceview5::Buffer, file: sourceview5::File) -> DocRef {
        Self::new_open_with_scan(path, buffer, file, Arc::new(ScanConfig::default()))
    }

    /// [`Document::new_open`] with the scanner rules `scan` (the workspace passes the window's).
    pub fn new_open_with_scan(
        path: PathBuf,
        buffer: sourceview5::Buffer,
        file: sourceview5::File,
        scan: Arc<ScanConfig>,
    ) -> DocRef {
        let index = LineIndex::new(&buffer_text(&buffer));
        let doc = Rc::new(Self::new(
            path,
            DocKind::Open {
                buffer: buffer.clone(),
            },
            file,
            index,
        ));
        doc.scan_config.replace(scan);
        doc.connect_buffer(&buffer);
        doc.start_line_states();
        doc
    }

    /// A document that is not shown in any editor.
    pub fn new_closed(path: PathBuf, text: String) -> DocRef {
        let file = sourceview5::File::new();
        file.set_location(Some(&gio::File::for_path(&path)));
        let index = LineIndex::new(&text);
        Rc::new(Self::new(
            path,
            DocKind::Closed { shadow: text },
            file,
            index,
        ))
    }

    fn new(path: PathBuf, kind: DocKind, file: sourceview5::File, index: LineIndex) -> Self {
        Self {
            path,
            kind: RefCell::new(kind),
            file,
            line_index: RefCell::new(index),
            line_states: RefCell::new(None),
            scan_config: RefCell::new(Arc::new(ScanConfig::default())),
            pending_scan: RefCell::new(None),
            scan_generation: Cell::new(0),
            origin: Cell::new(Origin::User),
            origin_before_history: Cell::new(None),
            review: RefCell::new(None),
            saver: Saver::new(),
            version: Cell::new(0),
            saved_version: Cell::new(Some(0)),
            pending: RefCell::new(None),
            user_action_open: Cell::new(false),
            saved_in_action: Cell::new(None),
            next_observer: Cell::new(0),
            edit_observers: Observers::default(),
            lines_observers: Observers::default(),
            hunks_observers: Observers::default(),
            retag_observers: Observers::default(),
            changed_observers: Observers::default(),
            apply_observers: Observers::default(),
            buffer_handlers: RefCell::new(Vec::new()),
        }
    }

    /// The absolute path (the workspace's key for this document).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file name, for tab titles and messages.
    pub fn display_name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// GtkSourceView's file object: location, encoding, newline type.
    pub fn file(&self) -> &sourceview5::File {
        &self.file
    }

    /// The buffer, while the document is open in an editor.
    pub fn buffer(&self) -> Option<sourceview5::Buffer> {
        match &*self.kind.borrow() {
            DocKind::Open { buffer } => Some(buffer.clone()),
            DocKind::Closed { .. } => None,
        }
    }

    /// True while the document is shown in an editor.
    pub fn is_open(&self) -> bool {
        matches!(&*self.kind.borrow(), DocKind::Open { .. })
    }

    /// True when the text differs from what was last loaded or saved.
    pub fn is_dirty(&self) -> bool {
        match &*self.kind.borrow() {
            DocKind::Open { buffer } => buffer.is_modified(),
            DocKind::Closed { .. } => self.saved_version.get() != Some(self.version.get()),
        }
    }

    /// Counts changes; it increases after every edit.
    pub fn version(&self) -> u64 {
        self.version.get()
    }

    /// The whole text.
    pub fn text(&self) -> String {
        self.with_text(str::to_owned)
    }

    /// Runs `f` on the whole text without copying a closed document's shadow.
    pub fn with_text<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        let buffer = match &*self.kind.borrow() {
            DocKind::Closed { shadow } => return f(shadow),
            DocKind::Open { buffer } => buffer.clone(),
        };
        f(&buffer_text(&buffer))
    }

    /// The line index, exact for the current text (between a change's before- and after-handler
    /// it already describes the text after the change).
    pub fn line_index(&self) -> Ref<'_, LineIndex> {
        self.line_index.borrow()
    }

    /// The origin that the next change will carry.
    pub fn origin(&self) -> Origin {
        self.origin.get()
    }

    /// Runs `f` with the origin set to `origin`, then restores the previous origin (also when `f`
    /// panics).
    pub fn with_origin<R>(&self, origin: Origin, f: impl FnOnce() -> R) -> R {
        let _restore = RestoreOrigin {
            doc: self,
            previous: self.origin.replace(origin),
        };
        f()
    }

    /// Byte offset of `iter` (an iter of this document's buffer).
    ///
    /// O(1) normally: `LineIndex::line_start(iter.line()) + iter.line_index()`. GTK also breaks
    /// lines at a lone `\r` and at U+2029, which `LineIndex` does not; while the text contains
    /// such a break the GTK line numbers do not match, and the offset is measured instead (O(n)).
    pub fn byte_offset(&self, iter: &gtk::TextIter) -> usize {
        let buffer = iter.buffer();
        let index = self.line_index.borrow();
        if gtk_lines_match(&buffer, &index) {
            let line = u32::try_from(iter.line()).unwrap_or(0);
            let col = u32::try_from(iter.line_index()).unwrap_or(0);
            index.offset(line, col)
        } else {
            buffer.slice(&buffer.start_iter(), iter, true).len()
        }
    }

    /// The buffer iter at byte offset `byte`; `None` for a closed document or an offset that is
    /// past the end or inside a character.
    pub fn iter_at_byte(&self, byte: usize) -> Option<gtk::TextIter> {
        let buffer = self.buffer()?;
        let index = self.line_index.borrow();
        if byte > index.text_len() {
            return None;
        }
        if !gtk_lines_match(&buffer, &index) {
            drop(index);
            let text = buffer_text(&buffer);
            let chars = text.get(..byte)?.chars().count();
            return Some(buffer.iter_at_offset(i32::try_from(chars).ok()?));
        }
        let (line, col) = index.position(byte);
        drop(index);
        let line = i32::try_from(line).ok()?;
        // GTK warns about an index inside a character, so check against the line's text
        // (with its line break, so a position between `\r` and `\n` is valid).
        let start = buffer.iter_at_line(line)?;
        let mut next = start;
        next.forward_line();
        let text = buffer.slice(&start, &next, true);
        let col = col as usize;
        if !text.is_char_boundary(col) {
            return None;
        }
        buffer
            .iter_at_line_index(line, i32::try_from(col).ok()?)
            .or_else(|| {
                // Between the `\r` and `\n` of a line break, which the line-index lookup refuses.
                let mut iter = start;
                iter.forward_chars(i32::try_from(text[..col].chars().count()).ok()?);
                Some(iter)
            })
    }

    /// Applies `edits` (pre-edit offsets, descending `range.start`, non-overlapping) with
    /// `origin`. On a buffer they form one user action, so one undo step reverts all of them.
    /// Marks (the cursor among them) outside the edited ranges keep their place.
    ///
    /// Edits that are not the user's (`origin` other than `User`) are always an undo step of
    /// their own: a user action left open across main-loop iterations (Vim's insert mode keeps
    /// one for the whole insert session) is closed before them and opened again after them.
    ///
    /// Nothing is applied when any edit does not fit the text or inserts U+0000.
    pub fn apply_edits(&self, edits: &[TextEdit], origin: Origin) -> Result<(), EditError> {
        if !edits.windows(2).all(|w| w[1].range.end <= w[0].range.start) {
            return Err(EditError::NotDescending);
        }
        self.check_edits(edits)?;
        if edits.is_empty() {
            return Ok(());
        }
        for f in self.apply_observers.snapshot() {
            f(self, edits, origin);
        }
        self.with_origin(origin, || match self.buffer() {
            Some(buffer) => {
                let split = origin != Origin::User && self.user_action_open.get();
                if split {
                    buffer.end_user_action();
                }
                let _action = UserAction::begin(&buffer, split);
                for edit in edits {
                    // Every change invalidates iters, so each one is looked up just before use.
                    if !edit.range.is_empty()
                        && let (Some(mut start), Some(mut end)) = (
                            self.iter_at_byte(edit.range.start),
                            self.iter_at_byte(edit.range.end),
                        )
                    {
                        buffer.delete(&mut start, &mut end);
                    }
                    if !edit.text.is_empty()
                        && let Some(mut at) = self.iter_at_byte(edit.range.start)
                    {
                        buffer.insert(&mut at, &edit.text);
                    }
                }
            }
            None => {
                for edit in edits {
                    self.before_change(edit.clone());
                    if let DocKind::Closed { shadow } = &mut *self.kind.borrow_mut() {
                        edit.apply(shadow);
                    }
                    self.after_change();
                }
            }
        });
        Ok(())
    }

    fn check_edits(&self, edits: &[TextEdit]) -> Result<(), EditError> {
        if edits.iter().any(|e| e.text.contains('\0')) {
            return Err(EditError::NulCharacter);
        }
        self.with_text(|text| {
            for edit in edits {
                let r = &edit.range;
                if r.start > r.end
                    || r.end > text.len()
                    || !text.is_char_boundary(r.start)
                    || !text.is_char_boundary(r.end)
                {
                    return Err(EditError::InvalidRange(edit.clone()));
                }
            }
            Ok(())
        })
    }

    /// Saves to disk (coalesced with saves already running, see [`crate::saver`]). Refuses with
    /// [`SaveError::ExternallyModified`] when the file changed on disk since it was loaded.
    pub fn save(self: &Rc<Self>) -> impl Future<Output = Result<(), SaveError>> + use<> {
        self.saver.request(self, false)
    }

    /// Like [`Document::save`], but overwrites a file that changed on disk.
    pub fn save_overwriting(
        self: &Rc<Self>,
    ) -> impl Future<Output = Result<(), SaveError>> + use<> {
        self.saver.request(self, true)
    }

    /// True when `hash` (`fnv1a64` of a file's bytes) is the latest text this document wrote (or
    /// is writing now); the file monitor uses it to ignore its own writes. An earlier write does
    /// not count (see [`crate::saver`]).
    pub fn is_own_write(&self, hash: u64) -> bool {
        self.saver.is_own_write(hash)
    }

    /// The charset the file is read and written in; `None` for UTF-8.
    pub fn charset(&self) -> Option<String> {
        self.saver.charset()
    }

    /// Replaces the text with the file on disk, as one undoable change with `Origin::User` (the
    /// user taking back their unsaved changes: a review folds it into its baseline outside the
    /// hunks), and marks the document clean. The file is decoded with the document's charset.
    pub fn reload(self: &Rc<Self>) -> impl Future<Output = Result<(), String>> + use<> {
        let doc = self.clone();
        async move {
            let name = doc.display_name();
            let (bytes, etag) = gio::File::for_path(doc.path())
                .load_contents_future()
                .await
                .map_err(|e| format!("cannot read {name}: {}", e.message()))?;
            let charset = doc.charset();
            let decoded = saver::decode(&bytes, charset.as_deref())
                .map_err(|e| format!("cannot read {name}: {e}"))?;
            // No await from here on: the text cannot change in between.
            let edits = doc.with_text(|text| lemma_review::minimal_edits(text, &decoded.text));
            doc.apply_edits(&edits, Origin::User)
                .map_err(|e| format!("cannot reload {name}: {e}"))?;
            doc.saver
                .set_loaded(decoded.charset, DiskState::of(&bytes, etag));
            doc.mark_saved(doc.version());
            Ok(())
        }
    }

    /// Calls `f(doc, edit, origin)` before every change, with pre-edit byte offsets.
    pub fn connect_edit(&self, f: impl Fn(&Document, &TextEdit, Origin) + 'static) -> ObserverId {
        self.edit_observers.add(self.next_id(), Rc::new(f))
    }

    /// Calls `f(first, removed, added)` after every change: lines `[first, first + removed)` of
    /// the old text became `[first, first + added)` (see `LineIndex::line_delta`).
    pub fn connect_lines_changed(&self, f: impl Fn(u32, u32, u32) + 'static) -> ObserverId {
        self.lines_observers.add(self.next_id(), Rc::new(f))
    }

    /// Calls `f` whenever [`Document::notify_hunks_changed`] runs (after a review recompute).
    pub fn connect_hunks_changed(&self, f: impl Fn() + 'static) -> ObserverId {
        self.hunks_observers.add(self.next_id(), Rc::new(f))
    }

    /// Tells the [`Document::connect_hunks_changed`] observers that the review's hunks changed.
    pub fn notify_hunks_changed(&self) {
        for f in self.hunks_observers.snapshot() {
            f();
        }
    }

    /// Calls `f(doc, edit, origin)` after every change, once the text, the line index and the
    /// scanner states describe the text after it; `edit` has pre-edit offsets (the inserted text
    /// now covers `edit.range.start..edit.range.start + edit.text.len()`). Like every change
    /// observer it runs inside the buffer's signal emission: it must not change the buffer
    /// (queue such work, e.g. in an idle callback).
    pub fn connect_changed(
        &self,
        f: impl Fn(&Document, &TextEdit, Origin) + 'static,
    ) -> ObserverId {
        self.changed_observers.add(self.next_id(), Rc::new(f))
    }

    /// Calls `f(lines)` whenever the scanner states change for `lines` (post-change line
    /// numbers, `\n` lines as in [`LineIndex`]): after each change with the range
    /// `LineStates::apply_edit` returned, and with every line after a (re)build. Runs inside the
    /// buffer's signal emission (see [`Document::connect_changed`]).
    pub fn connect_retag(&self, f: impl Fn(Range<u32>) + 'static) -> ObserverId {
        self.retag_observers.add(self.next_id(), Rc::new(f))
    }

    /// Calls `f(doc, edits, origin)` at the start of every [`Document::apply_edits`] that will
    /// change the text (agent, review and reload edits all come this way), before anything is
    /// applied: the snippet driver ends its sessions there.
    pub fn connect_before_apply(
        &self,
        f: impl Fn(&Document, &[TextEdit], Origin) + 'static,
    ) -> ObserverId {
        self.apply_observers.add(self.next_id(), Rc::new(f))
    }

    /// Removes an observer added by any `connect_*` method.
    pub fn disconnect(&self, id: ObserverId) {
        self.edit_observers.remove(id);
        self.lines_observers.remove(id);
        self.hunks_observers.remove(id);
        self.retag_observers.remove(id);
        self.changed_observers.remove(id);
        self.apply_observers.remove(id);
    }

    // ----- scanner states -----

    /// The scanner rules the line states are built with.
    pub fn scan_config(&self) -> Arc<ScanConfig> {
        self.scan_config.borrow().clone()
    }

    /// Uses `config` for the scanner states from now on; an open document rebuilds them (and
    /// reports every line to [`Document::connect_retag`]) when the rules changed.
    pub fn set_scan_config(self: &Rc<Self>, config: Arc<ScanConfig>) {
        if **self.scan_config.borrow() == *config {
            return;
        }
        self.scan_config.replace(config);
        if self.is_open() {
            self.start_line_states();
        }
    }

    /// True when the scanner states describe the text (the document is open and no rebuild is
    /// running).
    pub fn has_line_states(&self) -> bool {
        self.line_states.borrow().is_some()
    }

    /// The scanner context at `iter` (an iter of this document's buffer); see
    /// [`Document::context_at_byte`].
    pub fn context_at(&self, iter: &gtk::TextIter) -> Context {
        self.context_at_byte(self.byte_offset(iter))
    }

    /// The scanner context at byte `offset`, decided by the text before it on its line and the
    /// state at the line's start (`LineStates::context_at`, < 50 µs). `Context::Text` while there
    /// are no states (a closed document, or the first moments of a large one).
    pub fn context_at_byte(&self, offset: usize) -> Context {
        let states = self.line_states.borrow();
        let Some(states) = states.as_ref() else {
            return Context::Text;
        };
        let (line, col) = self.line_index.borrow().position(offset);
        match self.line_text(line) {
            Some(text) => states.context_at(line, &text, col),
            None => Context::Text,
        }
    }

    /// Line `line` (0-based, `\n` lines as in [`LineIndex`]) without its `\n`; a `\r` before it
    /// stays. `None` past the last line.
    pub fn line_text(&self, line: u32) -> Option<String> {
        let index = self.line_index.borrow();
        if line >= index.line_count() {
            return None;
        }
        match &*self.kind.borrow() {
            DocKind::Closed { shadow } => Some(shadow[index.line_range(line)].to_owned()),
            DocKind::Open { buffer } => {
                Some(BufferLines::new(buffer, &index).line(line).into_owned())
            }
        }
    }

    /// Builds the scanner states for the buffer's text: synchronously below
    /// [`ASYNC_SCAN_LINES`] lines, else on the jobs thread (changes made meanwhile are applied
    /// to the result before it is used).
    fn start_line_states(self: &Rc<Self>) {
        let generation = self.scan_generation.get() + 1;
        self.scan_generation.set(generation);
        self.pending_scan.replace(None);
        let Some(buffer) = self.buffer() else {
            self.line_states.replace(None);
            return;
        };
        let config = self.scan_config();
        let lines = self.line_index.borrow().line_count();
        if lines < ASYNC_SCAN_LINES {
            self.line_states
                .replace(Some(self.scan_now(config, &buffer)));
            self.notify_retag(0..lines);
            return;
        }
        self.line_states.replace(None);
        self.pending_scan.replace(Some(PendingScan {
            generation,
            delta: None,
        }));
        let text = self.text();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let built = gio::spawn_blocking(move || {
                let index = LineIndex::new(&text);
                let mut states = LineStates::new(config);
                states.rebuild(&StrLines {
                    text: &text,
                    index: &index,
                });
                states
            })
            .await;
            if let Some(doc) = weak.upgrade() {
                doc.finish_line_states(generation, built.ok());
            }
        });
    }

    /// Takes the states a jobs-thread rebuild produced, unless a newer rebuild or a close
    /// superseded it, and brings them up to date with the changes made meanwhile.
    fn finish_line_states(self: &Rc<Self>, generation: u64, built: Option<LineStates>) {
        let delta = match &*self.pending_scan.borrow() {
            Some(pending) if pending.generation == generation => pending.delta,
            _ => return,
        };
        self.pending_scan.replace(None);
        let Some(buffer) = self.buffer() else {
            return;
        };
        let Some(mut states) = built else {
            // The rebuild panicked: scan here instead.
            tracing::warn!("scanning {} on the jobs thread failed", self.path.display());
            self.line_states
                .replace(Some(self.scan_now(self.scan_config(), &buffer)));
            self.notify_retag(0..self.line_index.borrow().line_count());
            return;
        };
        if let Some((first, removed, added)) = delta {
            states.apply_edit(
                &BufferLines::new(&buffer, &self.line_index.borrow()),
                first,
                removed,
                added,
            );
        }
        self.line_states.replace(Some(states));
        self.notify_retag(0..self.line_index.borrow().line_count());
    }

    /// Scans the whole buffer here (one copy of the text, cut by the exact line index).
    fn scan_now(&self, config: Arc<ScanConfig>, buffer: &sourceview5::Buffer) -> LineStates {
        let text = buffer_text(buffer);
        let mut states = LineStates::new(config);
        states.rebuild(&StrLines {
            text: &text,
            index: &self.line_index.borrow(),
        });
        states
    }

    /// Brings the scanner states up to date after a change of `delta` lines; returns the lines to
    /// re-tag (`None` when there are no states to update).
    fn update_line_states(&self, (first, removed, added): (u32, u32, u32)) -> Option<Range<u32>> {
        if let Some(pending) = self.pending_scan.borrow_mut().as_mut() {
            pending.delta = Some(compose_line_deltas(pending.delta, (first, removed, added)));
            return None;
        }
        let buffer = self.buffer()?;
        let mut states = self.line_states.borrow_mut();
        let states = states.as_mut()?;
        let index = self.line_index.borrow();
        Some(states.apply_edit(&BufferLines::new(&buffer, &index), first, removed, added))
    }

    fn notify_retag(&self, lines: Range<u32>) {
        for f in self.retag_observers.snapshot() {
            f(lines.clone());
        }
    }

    fn next_id(&self) -> ObserverId {
        let id = self.next_observer.get();
        self.next_observer.set(id + 1);
        ObserverId(id)
    }

    /// Shows a closed document in `buffer`: the buffer gets the shadow text (not undoable, not
    /// reported as a change) and becomes the document's text. Does nothing if already open, or
    /// if the text contains U+0000, which a buffer cannot hold (the document stays closed).
    pub fn open_in_buffer(self: &Rc<Self>, buffer: &sourceview5::Buffer) {
        let dirty = self.is_dirty();
        {
            let kind = self.kind.borrow();
            let DocKind::Closed { shadow } = &*kind else {
                return;
            };
            if shadow.contains('\0') {
                return;
            }
            // The buffer is not connected to this document yet: nothing observes this change.
            buffer.begin_irreversible_action();
            buffer.set_text(shadow);
            buffer.end_irreversible_action();
        }
        buffer.set_modified(dirty);
        buffer.place_cursor(&buffer.start_iter());
        self.kind.replace(DocKind::Open {
            buffer: buffer.clone(),
        });
        self.connect_buffer(buffer);
        self.start_line_states();
    }

    /// Stops using the buffer (its editor is closing) and keeps the current text as the shadow.
    /// Unsaved buffer changes stay in the shadow; save or reload first if that is not wanted.
    pub fn close_buffer(&self) {
        let Some(buffer) = self.buffer() else {
            return;
        };
        for id in self.buffer_handlers.borrow_mut().drain(..) {
            buffer.disconnect(id);
        }
        self.user_action_open.set(false);
        let saved_in_action = self.saved_in_action.take() == Some(self.version.get());
        // A closed document has no scanner states (and a running rebuild is dropped).
        self.scan_generation.set(self.scan_generation.get() + 1);
        self.pending_scan.replace(None);
        self.line_states.replace(None);
        let shadow = buffer_text(&buffer).into();
        self.saved_version
            .set((!buffer.is_modified() || saved_in_action).then_some(self.version.get()));
        self.kind.replace(DocKind::Closed { shadow });
    }

    /// Records that the file holds the text of `version`: a closed document remembers it, an open
    /// one is marked unmodified unless it changed since.
    ///
    /// While a user action is open (Vim's insert mode keeps one), the buffer is marked
    /// unmodified only when the action ends, and only if the text is still that version: GTK's
    /// undo history would otherwise put the "unmodified" point inside the open action, and
    /// undoing it would show a text that differs from the file as unmodified.
    pub(crate) fn mark_saved(&self, version: u64) {
        let buffer = match &*self.kind.borrow() {
            DocKind::Open { buffer } => buffer.clone(),
            DocKind::Closed { .. } => {
                self.saved_version.set(Some(version));
                return;
            }
        };
        if self.user_action_open.get() {
            self.saved_in_action.set(Some(version));
            return;
        }
        // Outside the borrow: the modified-changed handlers read the document.
        if self.version.get() == version {
            buffer.set_modified(false);
        }
    }

    fn connect_buffer(self: &Rc<Self>, buffer: &sourceview5::Buffer) {
        let mut ids = Vec::with_capacity(10);
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_insert_text(move |_, iter, text| {
            if let Some(doc) = weak.upgrade() {
                doc.before_change(TextEdit::insert(doc.byte_offset(iter), text));
            }
        }));
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_delete_range(move |_, start, end| {
            if let Some(doc) = weak.upgrade() {
                let (a, b) = (doc.byte_offset(start), doc.byte_offset(end));
                doc.before_change(TextEdit::delete(a.min(b)..a.max(b)));
            }
        }));
        // Paintables and child anchors occupy U+FFFC in the text.
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_insert_paintable(move |_, iter, _| {
            if let Some(doc) = weak.upgrade() {
                doc.before_change(TextEdit::insert(doc.byte_offset(iter), "\u{FFFC}"));
            }
        }));
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_insert_child_anchor(move |_, iter, _| {
            if let Some(doc) = weak.upgrade() {
                doc.before_change(TextEdit::insert(doc.byte_offset(iter), "\u{FFFC}"));
            }
        }));
        for signal in [
            "insert-text",
            "delete-range",
            "insert-paintable",
            "insert-child-anchor",
        ] {
            let weak = Rc::downgrade(self);
            ids.push(buffer.connect_local(signal, true, move |_| {
                if let Some(doc) = weak.upgrade() {
                    doc.after_change();
                }
                None
            }));
        }
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_begin_user_action(move |_| {
            if let Some(doc) = weak.upgrade() {
                doc.user_action_open.set(true);
            }
        }));
        let weak = Rc::downgrade(self);
        ids.push(buffer.connect_end_user_action(move |_| {
            if let Some(doc) = weak.upgrade() {
                doc.user_action_open.set(false);
                // A save that ended during the action (see `mark_saved`): marked once GTK's
                // history has closed the action too.
                if let Some(version) = doc.saved_in_action.take() {
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(doc) = weak.upgrade() {
                            doc.mark_saved(version);
                        }
                    });
                }
            }
        }));
        for signal in ["undo", "redo"] {
            let weak = Rc::downgrade(self);
            ids.push(buffer.connect_local(signal, false, move |_| {
                if let Some(doc) = weak.upgrade() {
                    doc.origin_before_history
                        .set(Some(doc.origin.replace(Origin::History)));
                }
                None
            }));
            let weak = Rc::downgrade(self);
            ids.push(buffer.connect_local(signal, true, move |_| {
                if let Some(doc) = weak.upgrade()
                    && let Some(previous) = doc.origin_before_history.take()
                {
                    doc.origin.set(previous);
                }
                None
            }));
        }
        self.buffer_handlers.borrow_mut().extend(ids);
    }

    fn before_change(&self, edit: TextEdit) {
        if edit.range.is_empty() && edit.text.is_empty() {
            return;
        }
        let origin = self.origin.get();
        if self.review.borrow().is_some() {
            crate::review_ui::before_edit(self, &edit, origin);
        }
        for f in self.edit_observers.snapshot() {
            f(self, &edit, origin);
        }
        let mut index = self.line_index.borrow_mut();
        let delta = index.line_delta(&edit);
        index.apply(&edit);
        drop(index);
        debug_assert!(self.pending.borrow().is_none(), "nested buffer change");
        self.pending.replace(Some(PendingChange { edit, delta }));
    }

    fn after_change(&self) {
        let Some(PendingChange { edit, delta }) = self.pending.take() else {
            return;
        };
        let (first, removed, added) = delta;
        self.version.set(self.version.get() + 1);
        let origin = self.origin.get();
        if self.review.borrow().is_some() {
            crate::review_ui::after_edit(self, origin);
        }
        if let Some(lines) = self.update_line_states(delta) {
            self.notify_retag(lines);
        }
        for f in self.lines_observers.snapshot() {
            f(first, removed, added);
        }
        for f in self.changed_observers.snapshot() {
            f(self, &edit, origin);
        }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("path", &self.path)
            .field("open", &self.is_open())
            .field("version", &self.version.get())
            .finish_non_exhaustive()
    }
}

/// Restores the document's origin when dropped (see [`Document::with_origin`]).
struct RestoreOrigin<'a> {
    doc: &'a Document,
    previous: Origin,
}

impl Drop for RestoreOrigin<'_> {
    fn drop(&mut self) {
        self.doc.origin.set(self.previous);
    }
}

/// One user action on a buffer, ended when dropped (also on a panic). `reopen` begins another one
/// afterwards, for a user action that was closed before this one began.
struct UserAction<'a> {
    buffer: &'a sourceview5::Buffer,
    reopen: bool,
}

impl<'a> UserAction<'a> {
    fn begin(buffer: &'a sourceview5::Buffer, reopen: bool) -> Self {
        buffer.begin_user_action();
        Self { buffer, reopen }
    }
}

impl Drop for UserAction<'_> {
    fn drop(&mut self) {
        self.buffer.end_user_action();
        if self.reopen {
            self.buffer.begin_user_action();
        }
    }
}

/// [`Lines`] over a buffer and its exact line index: line `i` is the text between GTK's line
/// starts `i` and `i + 1` without the final `\n` (O(log n) per line). While GTK numbers lines
/// differently (a lone `\r` or U+2029 in the text), the whole text is read once and cut by the
/// index instead.
struct BufferLines<'a> {
    buffer: &'a sourceview5::Buffer,
    index: &'a LineIndex,
    matches: bool,
    text: OnceCell<glib::GString>,
}

impl<'a> BufferLines<'a> {
    fn new(buffer: &'a sourceview5::Buffer, index: &'a LineIndex) -> Self {
        Self {
            buffer,
            index,
            matches: gtk_lines_match(buffer, index),
            text: OnceCell::new(),
        }
    }
}

impl Lines for BufferLines<'_> {
    fn line_count(&self) -> u32 {
        self.index.line_count()
    }

    fn line(&self, i: u32) -> Cow<'_, str> {
        if !self.matches {
            let text = self.text.get_or_init(|| buffer_text(self.buffer));
            return Cow::Borrowed(&text[self.index.line_range(i)]);
        }
        let Some(start) = i32::try_from(i)
            .ok()
            .and_then(|line| self.buffer.iter_at_line(line))
        else {
            return Cow::Borrowed("");
        };
        let mut end = start;
        if !end.forward_line() {
            end = self.buffer.end_iter();
        }
        let mut line = self.buffer.slice(&start, &end, true).to_string();
        if line.ends_with('\n') {
            line.pop();
        }
        Cow::Owned(line)
    }
}

/// One line change equivalent to `earlier` followed by `later`, both `(first, removed, added)`
/// as `LineIndex::line_delta` gives them (`later` in the line numbers after `earlier`). The
/// result covers both: lines before its `first` and after its end are unchanged by either.
pub fn compose_line_deltas(
    earlier: Option<(u32, u32, u32)>,
    later: (u32, u32, u32),
) -> (u32, u32, u32) {
    let Some((s, r, a)) = earlier else {
        return later;
    };
    let (f2, r2, a2) = later;
    let (old_end, mid_end) = (s + r, s + a);
    let first = s.min(f2);
    // The end of the covered lines in the text between the two changes …
    let end = mid_end.max(f2 + r2);
    // … in the text before the first change, and after the second.
    let end_before = end - mid_end + old_end;
    let end_after = end - r2 + a2;
    (first, end_before - first, end_after - first)
}

/// True when GTK's line numbering equals the `\n` numbering of `index` (see
/// [`Document::byte_offset`]). GTK never has fewer lines, so equal counts mean equal lines.
fn gtk_lines_match(buffer: &impl IsA<gtk::TextBuffer>, index: &LineIndex) -> bool {
    u32::try_from(buffer.line_count()).is_ok_and(|n| n == index.line_count())
}

/// The buffer's whole text, byte for byte as the iters see it (U+FFFC for embedded objects).
pub fn buffer_text(buffer: &impl IsA<gtk::TextBuffer>) -> glib::GString {
    let (start, end) = buffer.bounds();
    buffer.slice(&start, &end, true)
}

/// A buffer configured for `path`: the built-in `latex` language for `.tex`/`.sty`/`.cls`/…,
/// `bibtex` for `.bib`, a guess from the file name otherwise; bracket matching; and no implicit
/// trailing newline, so the buffer holds exactly the bytes of the file.
pub fn new_buffer(path: &Path) -> sourceview5::Buffer {
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_implicit_trailing_newline(false);
    buffer.set_highlight_matching_brackets(true);
    buffer.set_highlight_syntax(true);
    buffer.set_enable_undo(true);
    buffer.set_language(language_for(path).as_ref());
    buffer
}

/// The GtkSourceView language for a file name.
pub fn language_for(path: &Path) -> Option<sourceview5::Language> {
    let manager = sourceview5::LanguageManager::default();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("tex" | "sty" | "cls" | "ltx" | "dtx" | "ins" | "bbl") => manager.language("latex"),
        Some("bib") => manager.language("bibtex"),
        _ => {
            let name = path.file_name().and_then(|n| n.to_str());
            manager.guess_language(name, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compose_line_deltas;

    /// Composing the line changes of random splices gives one change that covers them all:
    /// the lines before its start and after its end are the same in the first and last text.
    #[test]
    fn composed_line_deltas_cover_every_change() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut fresh_id = 1000;
        for _ in 0..2000 {
            let original: Vec<u32> = (0..1 + next(20) as u32).collect();
            let mut lines = original.clone();
            let mut composed = None;
            for _ in 0..1 + next(6) {
                // Like `LineIndex::line_delta`: `first` is a line of the text, at least one
                // line remains.
                let first = next(lines.len() as u64) as u32;
                let removed = next((lines.len() as u32 - first + 1) as u64) as u32;
                let mut added = next(4) as u32;
                if lines.len() as u32 - removed + added == 0 {
                    added = 1;
                }
                let new: Vec<u32> = (0..added)
                    .map(|_| {
                        fresh_id += 1;
                        fresh_id
                    })
                    .collect();
                lines.splice(first as usize..(first + removed) as usize, new);
                composed = Some(compose_line_deltas(composed, (first, removed, added)));
            }
            let (first, removed, added) = composed.unwrap();
            let (first, removed, added) = (first as usize, removed as usize, added as usize);
            assert_eq!(original.len() - removed + added, lines.len());
            assert_eq!(original[..first], lines[..first]);
            assert_eq!(original[first + removed..], lines[first + added..]);
        }
    }
}
