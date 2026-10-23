//! Session restore.
//!
//! Per project root, `Paths::sessions_dir()/<fnv(root) as 16 hex>.json`
//! (`$XDG_STATE_HOME/lemma/sessions/…`, [`file_for_root`]) holds a [`SessionFile`]: the open
//! tabs in order, the active tab, the cursor and the first line shown per tab, the PDF's place
//! (page and fraction at the top of the view), zoom (or fit width) and dark mode (only when the
//! user chose one with Ctrl+Shift+I), the visibility and sizes of the sidebar, PDF and agent
//! panes, the PDF strip and the sidebar page. The file is private to the user (0600 in a 0700
//! directory): it names their files.
//!
//! **Saving.** The shell calls [`save`] when the window is about to close, and 2 s
//! (`window::SESSION_DEBOUNCE`) after the last change it sees (tabs, cursor moves, scrolling,
//! panes, the split, the sidebar page, the PDF view); only while
//! `state.session_saves_enabled()` (windows with `paths` that do not quit by themselves, while
//! `[session] restore` is on). The snapshot is taken on the main thread (a few hundred bytes of
//! JSON) and written atomically on the jobs thread; an unchanged snapshot is not written again.
//! When the window closes, the write queued by the close-time save is done synchronously (in a
//! close-request handler after the shell's, and again when the window is destroyed), so closing
//! never loses the last state. Nothing is written before the window's session was restored
//! ([`restore`] runs right after the FILE argument opened), so a save can never overwrite the
//! session that is about to be restored. What the window cannot see at the moment of a save is carried over
//! from the session it restored (or saved last): the place of a PDF that is not shown (no build
//! yet, or a pane never laid out), the visibility of a pane while it is an overlay (a narrow
//! window), the first line of a restored tab that has not been drawn yet. The restored state
//! is saved once the restore is done.
//!
//! **The last session.** `Paths::last_session_file()` (`sessions/last.json`, a [`LastSession`])
//! names the project root and the selected file of the window that saved last: a save that
//! changes them writes it (with the session), and closing a window writes it again, so the window
//! closed last wins. A window without tabs leaves it alone. `lemma` without a FILE reopens it
//! ([`reopen`]): the app opens that root (pinned, as `lemma DIR` does) and the file, then
//! restores the root's session as usual.
//!
//! **Restoring.** The app calls [`restore`] once, after the FILE argument was handled and
//! before the scripted steps (not in scripted runs unless `--restore-session`, and not while
//! `[session] restore` is false). Tabs whose files are gone are skipped; the others open in
//! their saved order (a tab the window already has, such as the FILE argument, moves to its
//! saved place; one the session does not know stays first); cursors are clamped to the text
//! and each view scrolls to its first line once it has drawn. The FILE given on the command line
//! stays the active tab; otherwise the saved active tab is selected. Panes: the sidebar (and its
//! page and width), the PDF pane (and the editor/PDF split, as a share of the width), the PDF
//! strip and the agent pane are restored; a pane that is an overlay in a narrow window is left
//! closed. Showing the agent pane starts the agent (as F10 does), so a run that quits by itself
//! never restores it unless it plays a `--agent-scenario`. The PDF's place, zoom and dark mode
//! are applied the first time the same PDF is shown and laid out in the window (also later,
//! after the first build, or when a hidden PDF pane is shown); another PDF drops them.
//!
//! Tests: [`file_for_root`], [`parse`], [`state_file`], [`is_restoring`],
//! [`pdf_place_pending`], [`flush`], [`reopen`]; with temporary XDG directories only.

use crate::editor::Editor;
use crate::pdf::pdf_view;
use crate::window::WindowState;
use crate::workspace::normalize;
use adw::prelude::*;
use lemma_config::Paths;
use lemma_core::fnv1a64;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

/// The format of [`SessionFile`].
pub const VERSION: u32 = 1;

/// A project's session (see the module docs). Every field is optional when read, so files of
/// other versions restore what they can.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionFile {
    pub version: u32,
    /// The project root, for people reading the file (the file name is its hash).
    pub root: String,
    /// The open tabs, in order.
    pub tabs: Vec<TabState>,
    /// The selected tab, an index into `tabs`.
    pub active: Option<usize>,
    pub sidebar: SidebarState,
    pub agent: PaneState,
    pub pdf: PdfState,
}

/// One tab.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    /// The file: relative to the project root when it lies inside it, else absolute.
    pub path: String,
    /// The cursor: 0-based line and byte column (clamped to the text when restored).
    pub line: u32,
    pub column: u32,
    /// The first line shown at the top of the view (0-based).
    pub top: u32,
}

/// The left sidebar.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarState {
    pub visible: Option<bool>,
    /// The page shown (`outline`, `changes`, `problems`, `search`).
    pub page: Option<String>,
    /// Its share of the window width (`sidebar-width-fraction`).
    pub width: Option<f64>,
}

/// A side pane (the agent pane).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneState {
    pub visible: Option<bool>,
    /// Its share of the window width (`sidebar-width-fraction`).
    pub width: Option<f64>,
}

/// The PDF pane and the place in the PDF.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfState {
    pub visible: Option<bool>,
    /// The editor's share of the editor | PDF split.
    pub split: Option<f64>,
    /// The strip of thumbnails and bookmarks.
    pub strip: Option<bool>,
    /// The PDF shown (relative to the root when inside it), with its place.
    pub file: Option<String>,
    pub place: Option<PdfPlace>,
}

/// Where a PDF was shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfPlace {
    /// The page (0-based) and the fraction of it at the top of the view; the horizontal scroll
    /// offset (pixels).
    pub page: u32,
    pub frac: f64,
    pub x: f64,
    /// `None`: fit width.
    pub zoom: Option<f64>,
    /// The user's dark-mode choice for this PDF (Ctrl+Shift+I); `None`: it follows the style.
    pub dark: Option<bool>,
}

/// What `lemma` without a FILE reopens (see the module docs): the project and file of the
/// window that saved or closed last.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastSession {
    pub version: u32,
    /// The project root (absolute).
    pub root: String,
    /// The selected tab's file (absolute).
    pub file: Option<String>,
}

/// Where `lemma` without a FILE starts: [`LastSession`] with what is still there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reopen {
    /// The project root: a directory.
    pub root: PathBuf,
    /// The file to open: a file, if it still exists.
    pub file: Option<PathBuf>,
}

/// Parses `last.json`.
pub fn parse_last(bytes: &[u8]) -> Result<LastSession, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// The project and file that `lemma` without a FILE reopens; `None` when nothing was saved, the
/// file does not parse, or its root is no longer a directory. It reads the disk: the app calls it
/// once, before the window is built.
pub fn reopen(paths: &Paths) -> Option<Reopen> {
    let file = paths.last_session_file();
    let bytes = std::fs::read(&file).ok()?;
    let last = match parse_last(&bytes) {
        Ok(last) => last,
        Err(e) => {
            tracing::warn!("ignoring {} that does not parse: {e}", file.display());
            return None;
        }
    };
    let root = PathBuf::from(&last.root);
    if !root.is_absolute() || root.parent().is_none() || !root.is_dir() {
        tracing::info!("session: the last project {} is gone", last.root);
        return None;
    }
    let file = last
        .file
        .map(PathBuf::from)
        .filter(|f| f.is_absolute() && f.is_file());
    tracing::info!("session: reopening the last project {}", root.display());
    Some(Reopen {
        root: normalize(&root),
        file: file.map(|f| normalize(&f)),
    })
}

/// The [`LastSession`] of a snapshot of the project at `root`: `None` without tabs (or for a
/// root that is not UTF-8).
fn last_session_of(snapshot: &SessionFile, root: &Path) -> Option<LastSession> {
    let tab = snapshot
        .active
        .and_then(|i| snapshot.tabs.get(i))
        .or_else(|| snapshot.tabs.first())?;
    let file = absolute(root, &tab.path);
    Some(LastSession {
        version: VERSION,
        root: root.to_str()?.to_owned(),
        file: file.to_str().map(str::to_owned),
    })
}

/// The session file of the project at `root`: `sessions_dir/<fnv1a64(root) as 16 hex>.json`.
pub fn file_for_root(sessions_dir: &Path, root: &Path) -> PathBuf {
    let hash = fnv1a64(&[root.as_os_str().as_encoded_bytes()]);
    sessions_dir.join(format!("{hash:016x}.json"))
}

/// Parses a session file.
pub fn parse(bytes: &[u8]) -> Result<SessionFile, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// The session file of `state`'s project (`None` without `paths`).
pub fn state_file(state: &WindowState) -> Option<PathBuf> {
    let paths = state.paths.as_ref()?;
    Some(file_for_root(
        &paths.sessions_dir(),
        state.workspace.root().path(),
    ))
}

/// True while [`restore`] runs for `state`'s window.
pub fn is_restoring(state: &WindowState) -> bool {
    session_of(state).is_some_and(|s| s.phase.get() == Phase::Restoring)
}

/// True while a restored PDF place waits for its PDF to be shown.
pub fn pdf_place_pending(state: &WindowState) -> bool {
    session_of(state).is_some_and(|s| s.pending_pdf.borrow().is_some())
}

/// Writes a save still queued for `state`'s window now (what closing the window does).
pub fn flush(state: &WindowState) {
    if let Some(s) = session_of(state) {
        s.flush();
    }
}

/// Saves `state`'s session (see the module docs for when the shell calls it).
pub fn save(state: &Rc<WindowState>) {
    let Some(paths) = state.paths.as_ref() else {
        return;
    };
    let s = session_for(state);
    match s.phase.get() {
        // Nothing is written before the session was restored (the app restores it right after
        // opening the FILE argument), so a save never overwrites the session about to be
        // restored; the restored state is saved when the restore is done.
        Phase::Fresh | Phase::Restoring | Phase::Closed => return,
        Phase::Ready => {}
    }
    let root = state.workspace.root().path().to_path_buf();
    let file = file_for_root(&paths.sessions_dir(), &root);
    let snapshot = s.capture(state, &root);
    if let Some(last) = last_session_of(&snapshot, &root) {
        s.remember_last(paths.last_session_file(), &last);
    }
    let unchanged = s
        .last
        .borrow()
        .as_ref()
        .is_some_and(|(r, last)| *r == root && *last == snapshot);
    if unchanged {
        return;
    }
    let bytes = match serde_json::to_vec_pretty(&snapshot) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("cannot serialize the session: {e}");
            return;
        }
    };
    s.last.replace(Some((root, snapshot)));
    s.write(file, bytes);
}

/// Restores the session of `state`'s project (see the module docs). Runs once per window; a
/// session file that cannot be read is an error, one that does not parse is ignored.
pub async fn restore(state: &Rc<WindowState>) -> Result<(), String> {
    let Some(paths) = state.paths.clone() else {
        return Ok(());
    };
    let s = session_for(state);
    if s.phase.get() != Phase::Fresh {
        return Ok(());
    }
    s.phase.set(Phase::Restoring);
    let root = state.workspace.root().path().to_path_buf();
    let file = file_for_root(&paths.sessions_dir(), &root);
    let result = s.restore_from(state, &root, &file).await;
    if s.phase.get() == Phase::Restoring {
        s.phase.set(Phase::Ready);
    }
    // What was restored (and whatever changed meanwhile) is saved as usual.
    state.session_changed();
    result
}

// ----- the per-window state -----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No restore has run.
    Fresh,
    Restoring,
    Ready,
    /// The window is gone.
    Closed,
}

/// The session of one window.
struct Session {
    state: Weak<WindowState>,
    phase: Cell<Phase>,
    /// The session last restored or saved, with its root: what cannot be seen now is carried
    /// over from it.
    last: RefCell<Option<(PathBuf, SessionFile)>>,
    /// Restored tabs whose view has not drawn yet: their first line (by path).
    pending_scroll: RefCell<HashMap<PathBuf, u32>>,
    /// A restored PDF place waiting for its PDF, and the PDF (absolute).
    pending_pdf: RefCell<Option<(Option<PathBuf>, PdfPlace)>>,
    pdf_attempt_queued: Cell<bool>,
    pdf_map_handler: RefCell<Option<glib::SignalHandlerId>>,
    writer: Arc<Writer>,
    seq: Cell<u64>,
    /// The last snapshot handed to a writer.
    queued: RefCell<Option<Queued>>,
    /// A write failure was reported (once per window).
    reported: Cell<bool>,
    /// Writes `last.json` (see [`LastSession`]).
    last_writer: Arc<Writer>,
    /// This window's [`LastSession`] as last handed to `last_writer`: closing writes it again.
    last_queued: RefCell<Option<Queued>>,
}

/// A snapshot handed to a writer.
#[derive(Clone)]
struct Queued {
    seq: u64,
    file: PathBuf,
    bytes: Arc<Vec<u8>>,
}

thread_local! {
    static SESSIONS: RefCell<Vec<Rc<Session>>> = const { RefCell::new(Vec::new()) };
}

fn session_of(state: &WindowState) -> Option<Rc<Session>> {
    SESSIONS.with(|list| {
        list.borrow()
            .iter()
            .find(|s| std::ptr::eq(s.state.as_ptr(), state))
            .cloned()
    })
}

fn session_for(state: &Rc<WindowState>) -> Rc<Session> {
    if let Some(s) = session_of(state) {
        return s;
    }
    let s = Rc::new(Session {
        state: Rc::downgrade(state),
        phase: Cell::new(Phase::Fresh),
        last: RefCell::new(None),
        pending_scroll: RefCell::new(HashMap::new()),
        pending_pdf: RefCell::new(None),
        pdf_attempt_queued: Cell::new(false),
        pdf_map_handler: RefCell::new(None),
        writer: Arc::new(Writer::default()),
        seq: Cell::new(0),
        queued: RefCell::new(None),
        reported: Cell::new(false),
        last_writer: Arc::new(Writer::default()),
        last_queued: RefCell::new(None),
    });
    SESSIONS.with(|list| list.borrow_mut().push(s.clone()));
    // The shell saves in its close-request handler (connected before this one), so the write
    // queued there is done here, before the window goes; the destroy handler is the fallback
    // when this one does not run (the shell kept the window open to ask about unsaved changes).
    let weak = Rc::downgrade(&s);
    state.window.connect_close_request(move |_| {
        if let Some(s) = weak.upgrade() {
            s.flush();
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&s);
    state.window.connect_destroy(move |_| {
        if let Some(s) = weak.upgrade() {
            s.close();
        }
    });
    s
}

impl Session {
    // ----- saving -----

    /// What the window shows now, as a session (values it cannot see come from `last`).
    fn capture(&self, state: &WindowState, root: &Path) -> SessionFile {
        let last = self
            .last
            .borrow()
            .as_ref()
            .filter(|(r, _)| r == root)
            .map(|(_, f)| f.clone());
        let mut tabs = Vec::new();
        let mut active = None;
        let selected = state.tab_view.selected_page();
        let editors = state.editors();
        for i in 0..state.tab_view.n_pages() {
            let page = state.tab_view.nth_page(i);
            let child = page.child();
            let Some(editor) = editors
                .iter()
                .find(|e| e.widget().upcast_ref::<gtk::Widget>() == &child)
            else {
                continue;
            };
            let Some(tab) = self.tab_state(editor, root, last.as_ref()) else {
                continue;
            };
            if selected.as_ref() == Some(&page) {
                active = Some(tabs.len());
            }
            tabs.push(tab);
        }
        let last_sidebar = last.as_ref().map(|l| l.sidebar.clone()).unwrap_or_default();
        let split = &state.sidebar.split;
        let sidebar = SidebarState {
            visible: if split.is_collapsed() {
                last_sidebar.visible
            } else {
                Some(split.shows_sidebar())
            },
            page: state
                .sidebar
                .stack
                .visible_child_name()
                .map(|n| n.to_string()),
            width: Some(split.sidebar_width_fraction()),
        };
        let last_agent = last.as_ref().map(|l| l.agent.clone()).unwrap_or_default();
        let agent_split = &state.agent.split;
        let agent = PaneState {
            visible: if agent_split.is_collapsed() {
                last_agent.visible
            } else {
                Some(agent_split.shows_sidebar())
            },
            width: Some(agent_split.sidebar_width_fraction()),
        };
        let last_pdf = last.as_ref().map(|l| l.pdf.clone()).unwrap_or_default();
        // A PDF in a window of its own (pdf::detach) hides the pane but is shown: it comes back
        // attached and visible.
        let detached = crate::pdf::detach::is_detached(state);
        let pdf_visible = detached || state.pdf_pane.is_visible();
        let paned_width = state.paned.width();
        let split = if !detached && pdf_visible && paned_width > 0 {
            Some(f64::from(state.paned.position()) / f64::from(paned_width))
        } else {
            last_pdf.split
        };
        let (file, place) = self.pdf_place(state, root, &last_pdf);
        SessionFile {
            version: VERSION,
            root: root.to_string_lossy().into_owned(),
            tabs,
            active,
            sidebar,
            agent,
            pdf: PdfState {
                visible: Some(pdf_visible),
                split,
                strip: Some(state.pdf_sidebar.split.shows_sidebar()),
                file,
                place,
            },
        }
    }

    /// The tab of `editor`; `None` for a file whose path is not UTF-8 (it could not be read
    /// back).
    fn tab_state(
        &self,
        editor: &Editor,
        root: &Path,
        last: Option<&SessionFile>,
    ) -> Option<TabState> {
        let doc = editor.document();
        let path = stored_path(doc.path(), root)?;
        let buffer = editor.buffer();
        let (line, column) = {
            let iter = buffer.iter_at_mark(&buffer.get_insert());
            doc.line_index().position(doc.byte_offset(&iter))
        };
        let top = match self.pending_scroll.borrow().get(doc.path()) {
            Some(&top) => top,
            None => top_line(editor).unwrap_or_else(|| {
                last.and_then(|l| l.tabs.iter().find(|t| t.path == path))
                    .map_or(line, |t| t.top)
            }),
        };
        Some(TabState {
            path,
            line,
            column,
            top,
        })
    }

    /// The PDF shown and its place; while none is shown or laid out (or a restored place still
    /// waits), the saved ones.
    fn pdf_place(
        &self,
        state: &WindowState,
        root: &Path,
        last: &PdfState,
    ) -> (Option<String>, Option<PdfPlace>) {
        if self.pending_pdf.borrow().is_some() {
            return (last.file.clone(), last.place);
        }
        let Some(pdf) = state.pdf.get() else {
            return (last.file.clone(), last.place);
        };
        let view = pdf.view();
        let (Some(shown), Some(anchor)) = (pdf.pdf(), pdf_view::anchor_of(view)) else {
            return (last.file.clone(), last.place);
        };
        if view.width() <= 0 || view.height() <= 0 {
            return (last.file.clone(), last.place);
        }
        let Some(file) = stored_path(&shown, root) else {
            return (last.file.clone(), last.place);
        };
        let follows = state.config.pdf.dark_invert && adw::StyleManager::default().is_dark();
        let dark = view.is_dark();
        let place = PdfPlace {
            page: anchor.page,
            frac: anchor.frac,
            x: anchor.x,
            zoom: (!view.is_fit_width()).then(|| view.zoom()),
            dark: (dark != follows).then_some(dark),
        };
        (Some(file), Some(place))
    }

    /// The next sequence number of a write (for either writer).
    fn next_seq(&self) -> u64 {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        seq
    }

    /// Hands `last` to the jobs thread for `file` unless it is what this window wrote last.
    fn remember_last(&self, file: PathBuf, last: &LastSession) {
        let bytes = match serde_json::to_vec_pretty(last) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("cannot serialize the last session: {e}");
                return;
            }
        };
        let same = self
            .last_queued
            .borrow()
            .as_ref()
            .is_some_and(|q| q.file == file && *q.bytes == bytes);
        if same {
            return;
        }
        let seq = self.next_seq();
        let bytes = Arc::new(bytes);
        self.last_queued.replace(Some(Queued {
            seq,
            file: file.clone(),
            bytes: bytes.clone(),
        }));
        let writer = self.last_writer.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || writer.write(&file, &bytes, seq)).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("cannot save the last session: {e}"),
                Err(_) => tracing::warn!("cannot save the last session: the writer panicked"),
            }
        });
    }

    /// Writes this window's [`LastSession`] again, now: the window closes, so it is the one
    /// `lemma` reopens (even when another window wrote the file since).
    fn write_last_now(&self) {
        let Some(Queued { file, bytes, .. }) = self.last_queued.borrow().clone() else {
            return;
        };
        // Not when sessions were switched off meanwhile.
        if self
            .state
            .upgrade()
            .is_some_and(|state| !state.session_saves_enabled())
        {
            return;
        }
        let seq = self.next_seq();
        if let Err(e) = self.last_writer.rewrite(&file, &bytes, seq) {
            tracing::warn!("cannot save the last session: {e}");
        }
    }

    /// Hands `bytes` to the jobs thread for `file`.
    fn write(self: &Rc<Self>, file: PathBuf, bytes: Vec<u8>) {
        let seq = self.next_seq();
        let bytes = Arc::new(bytes);
        self.queued.replace(Some(Queued {
            seq,
            file: file.clone(),
            bytes: bytes.clone(),
        }));
        let writer = self.writer.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || writer.write(&file, &bytes, seq)).await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => "the writer panicked".to_owned(),
            };
            if let Some(s) = weak.upgrade() {
                s.failed(&error);
            }
        });
    }

    /// A write failed: the next save tries again; the user is told once.
    fn failed(&self, error: &str) {
        tracing::warn!("cannot save the session: {error}");
        self.last.replace(None);
        if !self.reported.replace(true)
            && let Some(state) = self.state.upgrade()
            && self.phase.get() != Phase::Closed
        {
            state.toast(&format!("Could not save the session: {error}"));
        }
    }

    /// Writes the queued snapshot now unless a writer has done so already.
    fn flush(&self) {
        let Some(Queued { seq, file, bytes }) = self.queued.borrow().clone() else {
            return;
        };
        if let Err(e) = self.writer.write(&file, &bytes, seq) {
            tracing::warn!("cannot save the session: {e}");
        }
    }

    fn close(&self) {
        if self.phase.get() == Phase::Closed {
            return;
        }
        self.flush();
        self.write_last_now();
        self.phase.set(Phase::Closed);
        self.pending_pdf.replace(None);
        if let (Some(handler), Some(state)) = (self.pdf_map_handler.take(), self.state.upgrade())
            && let Some(pdf) = state.pdf.get()
        {
            pdf.view().disconnect(handler);
        }
        SESSIONS.with(|list| {
            list.borrow_mut()
                .retain(|s| !std::ptr::eq(Rc::as_ptr(s), self))
        });
    }

    // ----- restoring -----

    async fn restore_from(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        root: &Path,
        file: &Path,
    ) -> Result<(), String> {
        let saved = match gio::File::for_path(file).load_contents_future().await {
            Ok((bytes, _)) if bytes.trim_ascii().is_empty() => return Ok(()),
            Ok((bytes, _)) => match parse(&bytes) {
                Ok(saved) => saved,
                Err(e) => {
                    tracing::warn!(
                        "ignoring the session {} that does not parse: {e}",
                        file.display()
                    );
                    return Ok(());
                }
            },
            Err(e) if e.matches(gio::IOErrorEnum::NotFound) => return Ok(()),
            Err(e) => {
                return Err(format!(
                    "Could not read the saved session {}: {}",
                    file.display(),
                    e.message()
                ));
            }
        };
        if !saved.root.is_empty() && Path::new(&saved.root) != root {
            tracing::warn!(
                "the session {} belongs to {}, not {}",
                file.display(),
                saved.root,
                root.display()
            );
            return Ok(());
        }
        self.last.replace(Some((root.to_path_buf(), saved.clone())));
        // The panes first: the tabs then open into the final layout.
        self.restore_panes(state, &saved);
        let opened = self.restore_tabs(state, root, &saved).await;
        if let Some(place) = saved.pdf.place {
            let pdf = saved.pdf.file.as_deref().map(|f| absolute(root, f));
            self.want_pdf_place(state, pdf, place);
        }
        tracing::info!(
            "session: restored {opened} of {} tabs from {}",
            saved.tabs.len(),
            file.display()
        );
        Ok(())
    }

    /// Opens the saved tabs (see the module docs); returns how many are open.
    async fn restore_tabs(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        root: &Path,
        saved: &SessionFile,
    ) -> usize {
        let wanted: Vec<(PathBuf, &TabState)> = saved
            .tabs
            .iter()
            .map(|t| (absolute(root, &t.path), t))
            .collect();
        let paths: Vec<PathBuf> = wanted.iter().map(|(p, _)| p.clone()).collect();
        let present = gio::spawn_blocking(move || {
            paths
                .iter()
                .map(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
                .collect::<Vec<bool>>()
        })
        .await
        .unwrap_or_default();
        let first_active = state.active_editor();
        let mut restored: Vec<(Rc<Editor>, &TabState)> = Vec::new();
        let mut saved_active = None;
        for (i, (path, tab)) in wanted.iter().enumerate() {
            let open = state
                .workspace
                .get(path)
                .and_then(|doc| state.editor_for(&doc));
            let editor = match open {
                Some(editor) => editor,
                None if present.get(i).copied().unwrap_or(false) => {
                    if state.workspace.is_empty() {
                        // The session belongs to this root: opening its first tab (maybe in a
                        // subdirectory) must not move it.
                        state.pin_root();
                    }
                    match state.open_file(path).await {
                        Ok(editor) => editor,
                        Err(e) => {
                            tracing::info!("session: not restoring {}: {e}", path.display());
                            continue;
                        }
                    }
                }
                None => {
                    tracing::info!("session: {} is gone", path.display());
                    continue;
                }
            };
            if restored.iter().any(|(e, _)| Rc::ptr_eq(e, &editor)) {
                continue;
            }
            if saved.active == Some(i) {
                saved_active = Some(editor.clone());
            }
            restored.push((editor, tab));
        }
        // Tabs the session does not know (a FILE argument it does not list) stay first; the
        // others follow in their saved order.
        let editors = state.editors();
        let mut order: Vec<Rc<Editor>> = Vec::new();
        for i in 0..state.tab_view.n_pages() {
            let child = state.tab_view.nth_page(i).child();
            if let Some(e) = editors
                .iter()
                .find(|e| e.widget().upcast_ref::<gtk::Widget>() == &child)
                && !restored.iter().any(|(r, _)| Rc::ptr_eq(r, e))
            {
                order.push(e.clone());
            }
        }
        order.extend(restored.iter().map(|(e, _)| e.clone()));
        for (position, editor) in order.iter().enumerate() {
            if let Some(page) = page_of(state, editor)
                && let Ok(position) = i32::try_from(position)
                && state.tab_view.page_position(&page) != position
            {
                state.tab_view.reorder_page(&page, position);
            }
        }
        for (editor, tab) in &restored {
            self.restore_position(editor, tab);
        }
        let select = first_active
            .filter(|e| page_of(state, e).is_some())
            .or(saved_active)
            .or_else(|| restored.first().map(|(e, _)| e.clone()));
        if let Some(editor) = select {
            state.select_editor(&editor);
        }
        restored.len()
    }

    /// Puts `editor`'s cursor where `tab` says (clamped) and scrolls its view to the saved first
    /// line once it has drawn.
    fn restore_position(self: &Rc<Self>, editor: &Rc<Editor>, tab: &TabState) {
        let doc = editor.document();
        let (cursor, top) = {
            let index = doc.line_index();
            let last = index.line_count().saturating_sub(1);
            let line = tab.line.min(last);
            let range = index.line_range(line);
            let line_len = doc.line_text(line).map_or(0, |t| t.len());
            let column = (tab.column as usize).min(line_len);
            (range.start + column, tab.top.min(last))
        };
        editor.place_cursor_at(cursor);
        let path = doc.path().to_path_buf();
        self.pending_scroll.borrow_mut().insert(path.clone(), top);
        let weak = Rc::downgrade(self);
        editor.after_first_frame(move |editor| {
            let Some(s) = weak.upgrade() else {
                return;
            };
            let wanted = s.pending_scroll.borrow().get(&path).copied();
            if let Some(top) = wanted {
                scroll_to_line(editor, top);
                // Kept until the scroll is done (it waits for the lines to be measured).
                let weak = Rc::downgrade(&s);
                let path = path.clone();
                editor.after_first_frame(move |editor| {
                    if let Some(s) = weak.upgrade() {
                        scroll_to_line(editor, top);
                        s.pending_scroll.borrow_mut().remove(&path);
                    }
                });
            }
        });
    }

    fn restore_panes(&self, state: &Rc<WindowState>, saved: &SessionFile) {
        let fraction = |w: Option<f64>| w.filter(|w| (0.05..=0.95).contains(w));
        let sidebar = &state.sidebar;
        if let Some(width) = fraction(saved.sidebar.width) {
            sidebar.split.set_sidebar_width_fraction(width);
        }
        if let Some(page) = saved.sidebar.page.as_deref()
            && sidebar.stack.child_by_name(page).is_some()
        {
            sidebar.stack.set_visible_child_name(page);
        }
        if let Some(visible) = saved.sidebar.visible
            && !sidebar.split.is_collapsed()
        {
            sidebar.split.set_show_sidebar(visible);
        }
        let agent = &state.agent.split;
        if let Some(width) = fraction(saved.agent.width) {
            agent.set_sidebar_width_fraction(width);
        }
        if let Some(visible) = saved.agent.visible
            && !agent.is_collapsed()
        {
            // Showing the pane starts the agent: a run that quits by itself never starts the
            // real one implicitly.
            let real_agent = state
                .agent_controller()
                .is_none_or(|c| c.scenario().is_none());
            if !visible || !(state.scripted && real_agent) {
                agent.set_show_sidebar(visible);
            }
        }
        if let Some(visible) = saved.pdf.visible {
            state.pdf_pane.set_visible(visible);
        }
        if let Some(strip) = saved.pdf.strip {
            state.pdf_sidebar.split.set_show_sidebar(strip);
        }
        if let Some(split) = fraction(saved.pdf.split) {
            set_split(state, split);
        }
    }

    // ----- the PDF's place -----

    /// Applies `place` once `pdf` (or, for `None`, any PDF) is shown and laid out.
    fn want_pdf_place(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        pdf: Option<PathBuf>,
        place: PdfPlace,
    ) {
        let Some(controller) = state.pdf.get() else {
            return;
        };
        self.pending_pdf.replace(Some((pdf, place)));
        let view = controller.view();
        // The view has no way to disconnect this: it does nothing once the place is applied.
        let weak = Rc::downgrade(self);
        view.connect_changed(move |_| {
            if let Some(s) = weak.upgrade() {
                s.try_pdf_place_soon();
            }
        });
        let weak = Rc::downgrade(self);
        let handler = view.connect_map(move |_| {
            if let Some(s) = weak.upgrade() {
                s.try_pdf_place_soon();
            }
        });
        if let Some(old) = self.pdf_map_handler.replace(Some(handler)) {
            view.disconnect(old);
        }
        self.try_pdf_place_soon();
    }

    /// Tries to apply the pending PDF place from an idle (never inside the view's own
    /// notifications).
    fn try_pdf_place_soon(self: &Rc<Self>) {
        if self.pending_pdf.borrow().is_none() || self.pdf_attempt_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(s) = weak.upgrade() {
                s.pdf_attempt_queued.set(false);
                s.try_pdf_place();
            }
        });
    }

    fn try_pdf_place(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(controller) = state.pdf.get() else {
            return;
        };
        let Some((wanted, place)) = self.pending_pdf.borrow().clone() else {
            return;
        };
        let view = controller.view();
        let Some(shown) = controller.pdf() else {
            return;
        };
        if wanted
            .as_ref()
            .is_some_and(|w| normalize(w) != normalize(&shown))
        {
            // Another PDF: its place is not this one.
            self.pending_pdf.replace(None);
            self.pdf_place_done();
            return;
        }
        if view.layout().is_none() || !view.is_mapped() {
            return;
        }
        if view.width() <= 0 || view.height() <= 0 {
            // Mapped but not laid out yet: the next frame is.
            let weak = Rc::downgrade(self);
            view.add_tick_callback(move |_, _| {
                if let Some(s) = weak.upgrade() {
                    s.try_pdf_place_soon();
                }
                glib::ControlFlow::Break
            });
            return;
        }
        self.pending_pdf.replace(None);
        self.pdf_place_done();
        match place.zoom {
            None if !view.is_fit_width() => view.set_fit_width(),
            None => {}
            Some(zoom) => view.set_zoom(zoom, None),
        }
        if let Some(layout) = view.layout() {
            let anchor = lemma_pdf::Anchor {
                page: place.page,
                frac: place.frac.clamp(0.0, 1.0),
                x: place.x.max(0.0),
            };
            let y = layout.y_for_anchor(anchor, view.zoom());
            view.set_scroll_position(anchor.x, y);
        }
        if let Some(dark) = place.dark
            && view.is_dark() != dark
        {
            let _ = WidgetExt::activate_action(&state.window, "win.pdf-invert", None);
        }
        tracing::info!(
            "session: the PDF is back at page {} ({:.0}%)",
            place.page + 1,
            view.zoom() * 100.0
        );
    }

    fn pdf_place_done(&self) {
        if let (Some(handler), Some(state)) = (self.pdf_map_handler.take(), self.state.upgrade())
            && let Some(pdf) = state.pdf.get()
        {
            pdf.view().disconnect(handler);
        }
    }
}

// ----- helpers -----

/// How a session stores `path`: relative to `root` when inside it, else absolute; `None` when
/// it is not UTF-8.
fn stored_path(path: &Path, root: &Path) -> Option<String> {
    let shown = path.strip_prefix(root).unwrap_or(path);
    shown.to_str().map(str::to_owned)
}

/// The absolute path of a stored path.
fn absolute(root: &Path, stored: &str) -> PathBuf {
    normalize(&root.join(stored))
}

fn page_of(state: &WindowState, editor: &Editor) -> Option<adw::TabPage> {
    let widget = editor.widget().upcast_ref::<gtk::Widget>();
    (0..state.tab_view.n_pages())
        .map(|i| state.tab_view.nth_page(i))
        .find(|page| &page.child() == widget)
}

/// The document line at the top of `editor`'s view; `None` while the view has never been laid
/// out.
fn top_line(editor: &Editor) -> Option<u32> {
    let view = editor.view();
    let adjustment = view.vadjustment()?;
    if adjustment.upper() <= 0.0 || adjustment.page_size() <= 0.0 {
        return None;
    }
    // The visible rectangle starts in the top margin: the first line shown is below it.
    let (iter, _) = view.line_at_y(view.visible_rect().y() + view.top_margin());
    let doc = editor.document();
    Some(doc.line_index().line_of(doc.byte_offset(&iter)))
}

/// Scrolls `editor` so that document line `line` is at the top (GtkTextView waits until the
/// lines above it are measured).
fn scroll_to_line(editor: &Editor, line: u32) {
    let doc = editor.document();
    let offset = {
        let index = doc.line_index();
        index.line_start(line.min(index.line_count().saturating_sub(1)))
    };
    let Some(iter) = editor.iter_near(offset) else {
        return;
    };
    let buffer = editor.buffer();
    let mark = buffer.create_mark(None, &iter, true);
    editor.view().scroll_to_mark(&mark, 0.0, true, 0.0, 0.0);
    // GtkTextView keeps a mark of its own for a scroll it has to wait for.
    buffer.delete_mark(&mark);
}

/// Gives the editor `share` of the editor | PDF split, now or once the split is laid out.
fn set_split(state: &Rc<WindowState>, share: f64) {
    let paned = &state.paned;
    let apply = move |paned: &gtk::Paned| {
        let width = paned.width();
        if width <= 0 {
            return false;
        }
        paned.set_position((f64::from(width) * share).round() as i32);
        true
    };
    if apply(paned) {
        return;
    }
    // The window's first allocation sets the default split (`layout::set_initial_split`) in
    // the first handler of this signal; this one runs after it.
    let slot: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::default();
    let handler = slot.clone();
    let id = paned.connect_max_position_notify(move |paned| {
        if apply(paned)
            && let Some(id) = handler.borrow_mut().take()
        {
            paned.disconnect(id);
        }
    });
    slot.replace(Some(id));
}

/// Writes session files in order: only a newer snapshot replaces an older one.
#[derive(Debug, Default)]
struct Writer {
    /// The newest sequence number written, and the file and hash of what it wrote.
    committed: Mutex<(u64, Option<(PathBuf, u64)>)>,
}

impl Writer {
    /// Writes `bytes` to `file` atomically (a private temporary file renamed over it) unless a
    /// newer snapshot was written already; identical contents are not written again.
    fn write(&self, file: &Path, bytes: &[u8], seq: u64) -> Result<(), String> {
        self.write_with(file, bytes, seq, true)
    }

    /// [`Writer::write`], also when it wrote the same contents before (another process may have
    /// replaced them since).
    fn rewrite(&self, file: &Path, bytes: &[u8], seq: u64) -> Result<(), String> {
        self.write_with(file, bytes, seq, false)
    }

    fn write_with(
        &self,
        file: &Path,
        bytes: &[u8],
        seq: u64,
        skip_identical: bool,
    ) -> Result<(), String> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let mut committed = self
            .committed
            .lock()
            .map_err(|_| "a writer panicked".to_owned())?;
        if seq <= committed.0 {
            return Ok(());
        }
        let hash = fnv1a64(&[bytes]);
        if skip_identical
            && committed
                .1
                .as_ref()
                .is_some_and(|(f, h)| f == file && *h == hash)
        {
            committed.0 = seq;
            return Ok(());
        }
        let shown = |e: std::io::Error| format!("{}: {e}", file.display());
        if let Some(dir) = file.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(shown)?;
        }
        // Unique in the process too: the windows of one process all write `last.json`.
        static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = file.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(bytes))
            .and_then(|()| std::fs::rename(&tmp, file));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(shown(e));
        }
        *committed = (seq, Some((file.to_path_buf(), hash)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_hash_the_root() {
        let dir = Path::new("/state/sessions");
        let a = file_for_root(dir, Path::new("/home/u/thesis"));
        assert_eq!(a.parent(), Some(dir));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("json"));
        assert_eq!(a.file_stem().map(|s| s.len()), Some(16));
        assert_ne!(a, file_for_root(dir, Path::new("/home/u/paper")));
    }

    #[test]
    fn sessions_round_trip_and_tolerate_missing_fields() {
        let session = SessionFile {
            version: VERSION,
            root: "/p".into(),
            tabs: vec![TabState {
                path: "chapters/a.tex".into(),
                line: 3,
                column: 7,
                top: 1,
            }],
            active: Some(0),
            sidebar: SidebarState {
                visible: Some(true),
                page: Some("search".into()),
                width: Some(0.2),
            },
            agent: PaneState {
                visible: Some(false),
                width: None,
            },
            pdf: PdfState {
                visible: Some(true),
                split: Some(0.55),
                strip: Some(false),
                file: Some(".build/main.pdf".into()),
                place: Some(PdfPlace {
                    page: 4,
                    frac: 0.25,
                    x: 0.0,
                    zoom: None,
                    dark: Some(true),
                }),
            },
        };
        let json = serde_json::to_vec(&session).unwrap();
        assert_eq!(parse(&json).unwrap(), session);
        // An older or newer file restores what it has.
        let partial = parse(br#"{"tabs":[{"path":"main.tex","line":2}],"extra":1}"#).unwrap();
        assert_eq!(partial.tabs[0].path, "main.tex");
        assert_eq!(partial.tabs[0].line, 2);
        assert_eq!(partial.tabs[0].top, 0);
        assert_eq!(partial.sidebar.visible, None);
        assert!(parse(b"{oops").is_err());
    }

    #[test]
    fn stored_paths_are_relative_inside_the_root() {
        let root = Path::new("/home/u/thesis");
        assert_eq!(
            stored_path(Path::new("/home/u/thesis/ch/a.tex"), root).as_deref(),
            Some("ch/a.tex")
        );
        assert_eq!(
            stored_path(Path::new("/etc/macros.tex"), root).as_deref(),
            Some("/etc/macros.tex")
        );
        assert_eq!(
            absolute(Path::new("/nonexistent-root"), "/abs/x.tex"),
            PathBuf::from("/abs/x.tex")
        );
    }

    #[test]
    fn writes_are_atomic_private_and_ordered() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("lemma-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("sessions").join("x.json");
        let writer = Writer::default();
        writer.write(&file, b"two", 2).unwrap();
        writer.write(&file, b"one", 1).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        writer.write(&file, b"three", 3).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"three");
        // Another process replaces the file: the same contents are not written again, unless
        // asked to (closing a window rewrites `last.json`).
        std::fs::write(&file, b"other").unwrap();
        writer.write(&file, b"three", 4).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"other");
        writer.rewrite(&file, b"three", 5).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"three");
        writer.rewrite(&file, b"stale", 5).unwrap();
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"three",
            "older than the last write"
        );
        let leftovers = std::fs::read_dir(file.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary files are left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_session_is_the_selected_tab_of_a_window_with_tabs() {
        let root = Path::new("/home/u/thesis");
        let tab = |path: &str| TabState {
            path: path.into(),
            ..TabState::default()
        };
        let mut snapshot = SessionFile {
            tabs: vec![tab("main.tex"), tab("ch/a.tex"), tab("/etc/macros.tex")],
            active: Some(1),
            ..SessionFile::default()
        };
        let last = last_session_of(&snapshot, root).unwrap();
        assert_eq!(last.root, "/home/u/thesis");
        assert_eq!(last.file.as_deref(), Some("/home/u/thesis/ch/a.tex"));
        assert_eq!(last.version, VERSION);
        snapshot.active = Some(2);
        let last = last_session_of(&snapshot, root).unwrap();
        assert_eq!(last.file.as_deref(), Some("/etc/macros.tex"));
        // No selection (or a stale one): the first tab.
        snapshot.active = Some(7);
        let last = last_session_of(&snapshot, root).unwrap();
        assert_eq!(last.file.as_deref(), Some("/home/u/thesis/main.tex"));
        // A window without tabs leaves the last session alone.
        snapshot.tabs.clear();
        assert_eq!(last_session_of(&snapshot, root), None);
        let json = serde_json::to_vec(&last).unwrap();
        assert_eq!(parse_last(&json).unwrap(), last);
        assert_eq!(parse_last(b"{}").unwrap(), LastSession::default());
        assert!(parse_last(b"[").is_err());
    }

    #[test]
    fn reopen_reads_what_is_still_there() {
        let base = std::env::temp_dir().join(format!("lemma-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths::from_vars(|name| match name {
            "HOME" => Some(base.join("home").into()),
            "XDG_STATE_HOME" => Some(base.join("state").into()),
            _ => None,
        })
        .unwrap();
        let project = base.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("main.tex"), "x\n").unwrap();
        let file = paths.last_session_file();
        assert_eq!(reopen(&paths), None, "nothing saved yet");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let write = |root: &Path, main: &Path| {
            let last = LastSession {
                version: VERSION,
                root: root.to_str().unwrap().into(),
                file: Some(main.to_str().unwrap().into()),
            };
            std::fs::write(&file, serde_json::to_vec(&last).unwrap()).unwrap();
        };
        write(&project, &project.join("main.tex"));
        assert_eq!(
            reopen(&paths),
            Some(Reopen {
                root: project.clone(),
                file: Some(project.join("main.tex")),
            })
        );
        // A file that is gone: the project still opens.
        write(&project, &project.join("gone.tex"));
        assert_eq!(
            reopen(&paths),
            Some(Reopen {
                root: project.clone(),
                file: None,
            })
        );
        // A project that is gone, a relative one, `/`, or a file that does not parse: nothing.
        write(&base.join("gone"), &base.join("gone/main.tex"));
        assert_eq!(reopen(&paths), None);
        write(Path::new("proj"), Path::new("proj/main.tex"));
        assert_eq!(reopen(&paths), None);
        write(Path::new("/"), &project.join("main.tex"));
        assert_eq!(reopen(&paths), None);
        std::fs::write(&file, "{oops").unwrap();
        assert_eq!(reopen(&paths), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
