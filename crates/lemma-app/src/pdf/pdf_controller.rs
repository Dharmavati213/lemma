//! The PDF controller (ARCHITECTURE §6): owns the PDF thread handle and the document
//! generations, reloads after builds, and runs forward/inverse SyncTeX.
//!
//! The PDF pane shows a placeholder until the first PDF opens, then a toolbar (page n/N, zoom
//! out/in, fit width, dark pages, and a menu with Fit Page, 100% and the PDF's file) above the
//! [`PdfView`]. Over the left edge of the view, next to the editor, two labelled buttons jump
//! between the source and the PDF ([`PdfController::sync_buttons`]): "Source" is
//! `win.sync-inverse` (the source line of the middle of what the view shows of the current page)
//! and "PDF" is `win.sync-forward` (Ctrl+Alt+J: the cursor's line in the PDF), which the editor's
//! context menu offers too as Show in PDF; Ctrl+click in the PDF is inverse search at the point
//! clicked. The PDF thread starts with the first [`PdfController::open`]. Every open gets a
//! new generation; the view keeps showing the previous generation until the new one has opened,
//! and a PDF that fails to open leaves it in place. A new generation of the PDF shown keeps the
//! scroll position; another PDF starts at the top. A rebuild of the file asked for last is a
//! `PdfCmd::Reload`: with as many pages it shows with the old page sizes at once, and the view
//! and the strip follow the sizes the PDF thread reads afterwards (`PdfEvent::PageSizes`). A
//! build whose PDF has not changed since the last open (the same size and modification time)
//! opens nothing ([`PdfController::open_if_changed`]), so the textures, the thumbnails, the
//! links, a search and a SyncTeX highlight stay as they are.
//!
//! M3 (WP3.4): the controller also dispatches the PDF thread's answers to the [`links`],
//! [`search`] and [`sidebar`] modules (each registers itself here when it installs), sends their
//! commands, and merges the thumbnail requests of the strip and of the link preview into one
//! latest-wins list. Its toolbar gains a search button; the search bar sits below the toolbar
//! ([`PdfController::content`]).
//!
//! The PDF shown can be handed to another application: `win.pdf-open-external` opens it in the
//! system's PDF viewer (to print it, present slides full screen, …) and `win.pdf-show-in-folder`
//! shows it in the file manager (the build puts it in a hidden `.build` directory), both through
//! `gtk::FileLauncher`. They are disabled until a PDF is shown. Scripted runs start no other
//! application; tests route both with [`PdfController::set_launch_hook`].
//!
//! [`links`]: super::links
//! [`search`]: super::search
//! [`sidebar`]: super::sidebar

use super::pdf_view::{PdfView, Reveal};
use super::{detach, links, search, sidebar};
use crate::build_ui::{install_css_once, set_default_accels};
use crate::diagnostics_panel;
use crate::window::WindowState;
use adw::prelude::*;
use gtk::gdk;
use lemma_build::PdfStamp;
use lemma_pdf::{PdfCmd, PdfEvent, PdfHandle, SyncBox, ThumbReq, Thumbnail};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Follow-cursor mode searches this long after the cursor stops moving.
const FOLLOW_DELAY: Duration = Duration::from_millis(200);
/// How long closing the window waits for the PDF thread to let go of its document.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// Default accelerators of the actions this module adds (`[keys]` in config.toml wins).
const ACCELS: &[(&str, &[&str])] = &[
    ("win.sync-forward", &["<Control><Alt>j"]),
    ("win.pdf-invert", &["<Control><Shift>i"]),
];

/// The pane's backdrop is a shade darker than the window, so the pages stand out.
const CSS: &str = "
pdfview.lemma-pdf-view { background-color: color-mix(in srgb, var(--window-bg-color) 86%, black); }
@media (prefers-color-scheme: dark) {
  pdfview.lemma-pdf-view { background-color: color-mix(in srgb, var(--window-bg-color) 62%, black); }
}
.lemma-pdf-toolbar { padding: 3px 6px; border-bottom: 1px solid alpha(currentColor, 0.1);
                     background-color: var(--window-bg-color); }
.lemma-pdf-toolbar entry { min-height: 26px; padding: 0 6px; }
.lemma-pdf-toolbar button { min-height: 26px; min-width: 26px; padding: 2px; }
.lemma-pdf-toolbar .zoom-label { min-width: 3.4em; }
.lemma-pdf-sync { margin-left: 8px; opacity: 0.88; }
.lemma-pdf-sync:hover { opacity: 1; }
.lemma-pdf-sync button { min-height: 26px; padding: 2px 10px; font-size: 0.9em; }
.lemma-pdf-content searchbar > revealer > box { padding: 4px 6px; }
.lemma-pdf-content searchbar entry { min-height: 26px; }
.lemma-pdf-content searchbar button { min-height: 26px; min-width: 26px; padding: 2px; }
.lemma-pdf-search-count { min-width: 4.5em; }
";

/// The actions that hand the PDF shown to another application (disabled while there is none).
const HANDOFF_ACTIONS: [&str; 2] = ["pdf-open-external", "pdf-show-in-folder"];

/// What another application does with the PDF shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// Opens it (the system's default application for PDFs): `win.pdf-open-external`.
    Open,
    /// Shows it selected in the file manager: `win.pdf-show-in-folder`.
    ShowInFolder,
}

impl Handoff {
    /// The toast when it fails.
    fn failure(self) -> &'static str {
        match self {
            Handoff::Open => "Could not open the PDF",
            Handoff::ShowInFolder => "Could not show the PDF in Files",
        }
    }
}

/// Hands the PDF to another application instead of `gtk::FileLauncher` (see
/// [`PdfController::set_launch_hook`]); returns whether it did.
pub type LaunchHook = dyn Fn(Handoff, &Path) -> bool;

/// Who asked for thumbnails (see [`PdfController::set_thumbnail_wants`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbClient {
    /// The PDF strip's thumbnails.
    Strip,
    /// The page shown by a link's preview popover (served first).
    Preview,
}

/// The PDF state of one window (stored in [`WindowState::pdf`]).
pub struct PdfController {
    inner: Rc<Inner>,
}

struct Inner {
    state: Weak<WindowState>,
    view: PdfView,
    /// "empty" (the placeholder) or "pdf".
    stack: gtk::Stack,
    placeholder: adw::StatusPage,
    toolbar: Toolbar,
    /// [`build_sync_buttons`], over the view.
    sync_buttons: gtk::Box,
    /// Given to the PDF thread when it starts; `None` once the controller shuts down.
    events: RefCell<Option<async_channel::Sender<PdfEvent>>>,
    worker: RefCell<Option<PdfHandle>>,
    last_generation: Cell<u64>,
    /// `Open`s sent and not answered yet, oldest first.
    pending_opens: RefCell<Vec<(u64, PathBuf)>>,
    /// The latest `Open` sent.
    requested: RefCell<Option<(u64, PathBuf)>>,
    /// The stamp of the file the latest `Open` asked for, when it is known.
    requested_stamp: Cell<Option<PdfStamp>>,
    /// The PDF the view shows.
    shown: RefCell<Option<(u64, PathBuf)>>,
    /// Per-document dark-mode choices (Ctrl+Shift+I), overriding the system style.
    dark_overrides: RefCell<HashMap<PathBuf, bool>>,
    /// How the answer to the pending forward search scrolls; `None` when it was not asked by
    /// the user (no message when nothing is found).
    forward_reveal: Cell<Option<Reveal>>,
    follow_timer: RefCell<Option<glib::SourceId>>,
    /// The "pdf" page of `stack`: toolbar, search bar, strip and view.
    content: gtk::Box,
    // M3 modules (WP3.4), registered by their `install`.
    search: OnceCell<Rc<search::Search>>,
    links: OnceCell<Rc<links::Links>>,
    strip: OnceCell<Rc<sidebar::Strip>>,
    /// The thumbnail requests of the preview and of the strip, sent as one list.
    thumb_wants: RefCell<(Vec<ThumbReq>, Vec<ThumbReq>)>,
    launch_hook: RefCell<Option<Box<LaunchHook>>>,
}

struct Toolbar {
    bar: gtk::Box,
    page: gtk::Entry,
    pages: gtk::Label,
    zoom: gtk::Label,
    search: gtk::ToggleButton,
}

impl PdfController {
    /// Opens (or reloads) `pdf` as a new generation. The view keeps its scroll anchor when it
    /// already shows `pdf`.
    pub fn open(&self, pdf: &Path) {
        self.inner.open(pdf, None);
    }

    /// Opens `pdf` like [`PdfController::open`] unless the latest open asked for the same file
    /// with the same `stamp`, which a build that wrote no new PDF reports. The stamp is taken off
    /// the main thread (the build thread's [`lemma_build::BuildOutcome::pdf_stamp`]); `None`
    /// always opens. Returns whether it opened.
    pub fn open_if_changed(&self, pdf: &Path, stamp: Option<PdfStamp>) -> bool {
        let unchanged = stamp.is_some()
            && self.inner.requested_stamp.get() == stamp
            && self.requested_pdf().as_deref() == Some(pdf);
        if !unchanged {
            self.inner.open(pdf, stamp);
        }
        !unchanged
    }

    /// True when the view shows the generation the latest open asked for (false while it loads,
    /// and after it failed to open).
    pub fn shows_requested(&self) -> bool {
        let requested = self.inner.requested.borrow().as_ref().map(|(g, _)| *g);
        requested.is_some() && requested == self.generation()
    }

    /// The PDF the latest [`PdfController::open`] asked for.
    pub fn requested_pdf(&self) -> Option<PathBuf> {
        self.inner
            .requested
            .borrow()
            .as_ref()
            .map(|(_, p)| p.clone())
    }

    /// The PDF the view shows.
    pub fn pdf(&self) -> Option<PathBuf> {
        self.inner.shown.borrow().as_ref().map(|(_, p)| p.clone())
    }

    /// The generation the view shows.
    pub fn generation(&self) -> Option<u64> {
        self.inner.shown.borrow().as_ref().map(|(g, _)| *g)
    }

    /// The view.
    pub fn view(&self) -> &PdfView {
        &self.inner.view
    }

    /// Forward SyncTeX search: highlights the 1-based `line` of `file` in the PDF and scrolls
    /// to it.
    pub fn forward_search(&self, file: &Path, line: u32) {
        self.inner.forward(file, line, Some(Reveal::Always));
    }

    /// True while the pages are drawn in dark mode.
    pub fn is_dark(&self) -> bool {
        self.inner.view.is_dark()
    }

    /// Lets the PDF thread free what it keeps beside the document (`PdfCmd::Trim`): the SyncTeX
    /// scanner (about 85 KiB per page; the next sync parses the file again) and the heap's free
    /// pages; and forget that SyncTeX was used, so rebuilds parse nothing ahead. Nothing before
    /// the thread has started. ARCHITECTURE §7: a window in the background, a hidden pane.
    pub fn trim(&self) {
        self.inner.trim_worker();
    }

    // ----- M3 (WP3.4) -----

    /// Sends `cmd` to the PDF thread (starting it if needed).
    pub fn send(&self, cmd: PdfCmd) {
        self.inner.send(cmd);
    }

    /// The pane's content below its placeholder: the toolbar, the search bar, and the strip
    /// with the view.
    pub fn content(&self) -> &gtk::Box {
        &self.inner.content
    }

    /// The toolbar above the view.
    pub fn toolbar(&self) -> &gtk::Box {
        &self.inner.toolbar.bar
    }

    /// The SyncTeX buttons over the left edge of the view: Source (`win.sync-inverse`), then
    /// PDF (`win.sync-forward`).
    pub fn sync_buttons(&self) -> &gtk::Box {
        &self.inner.sync_buttons
    }

    /// The toolbar's search button (the search bar binds it).
    pub fn search_button(&self) -> &gtk::ToggleButton {
        &self.inner.toolbar.search
    }

    /// Replaces the thumbnails `client` wants; the PDF thread gets the preview's requests and
    /// the strip's in one latest-wins list.
    pub fn set_thumbnail_wants(&self, client: ThumbClient, reqs: Vec<ThumbReq>) {
        let merged = {
            let mut wants = self.inner.thumb_wants.borrow_mut();
            let slot = match client {
                ThumbClient::Preview => &mut wants.0,
                ThumbClient::Strip => &mut wants.1,
            };
            if *slot == reqs {
                return;
            }
            *slot = reqs;
            wants.0.iter().chain(&wants.1).copied().collect::<Vec<_>>()
        };
        // Nothing to withdraw before the PDF thread has started.
        if merged.is_empty() && self.inner.worker.borrow().is_none() {
            return;
        }
        self.inner.send(PdfCmd::WantThumbnails(merged));
    }

    /// Routes `win.pdf-open-external` and `win.pdf-show-in-folder` to `hook` (with the PDF; it
    /// returns whether it "opened" it) instead of `gtk::FileLauncher` (tests). `None` restores
    /// the launcher.
    pub fn set_launch_hook(&self, hook: Option<Box<LaunchHook>>) {
        self.inner.launch_hook.replace(hook);
    }

    pub(super) fn set_search(&self, search: Rc<search::Search>) {
        let _ = self.inner.search.set(search);
    }

    pub(super) fn search(&self) -> Option<&Rc<search::Search>> {
        self.inner.search.get()
    }

    pub(super) fn set_links(&self, links: Rc<links::Links>) {
        let _ = self.inner.links.set(links);
    }

    pub(super) fn links(&self) -> Option<&Rc<links::Links>> {
        self.inner.links.get()
    }

    pub(super) fn set_strip(&self, strip: Rc<sidebar::Strip>) {
        let _ = self.inner.strip.set(strip);
    }

    pub(super) fn strip(&self) -> Option<&Rc<sidebar::Strip>> {
        self.inner.strip.get()
    }
}

/// The texture of a thumbnail from the PDF thread (no copy) and its size in bytes; `None` for a
/// malformed bitmap.
fn thumbnail_texture(thumbnail: Thumbnail) -> Option<(ThumbReq, gdk::Texture, usize)> {
    let Thumbnail {
        req,
        width,
        height,
        stride,
        data,
    } = thumbnail;
    let expected = usize::try_from(stride)
        .ok()
        .zip(usize::try_from(height).ok())
        .map(|(s, h)| s * h);
    if width <= 0 || height <= 0 || expected.is_none_or(|n| data.len() < n) {
        tracing::warn!("dropping a malformed thumbnail for {req:?}");
        return None;
    }
    let bytes = glib::Bytes::from_owned(data);
    let size = bytes.len();
    let texture = gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride as usize,
    );
    Some((req, texture.upcast(), size))
}

impl Inner {
    /// The PDF thread, started on first use (`None` only while shutting down).
    fn worker(&self) -> Option<PdfHandle> {
        let mut worker = self.worker.borrow_mut();
        if worker.is_none() {
            let events = self.events.borrow().clone()?;
            *worker = Some(lemma_pdf::spawn(events));
        }
        worker.clone()
    }

    fn send(&self, cmd: PdfCmd) {
        if let Some(worker) = self.worker() {
            worker.send(cmd);
        }
    }

    /// Sends `PdfCmd::Trim` to the PDF thread if it runs (see [`PdfController::trim`]).
    fn trim_worker(&self) {
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.send(PdfCmd::Trim);
        }
    }

    fn open(&self, pdf: &Path, stamp: Option<PdfStamp>) {
        let generation = self.last_generation.get() + 1;
        self.last_generation.set(generation);
        // A rebuild of the file asked for last shows with its page sizes until the PDF thread has
        // read the new ones (`PdfEvent::PageSizes`), instead of after reading every page.
        let rebuild = self
            .requested
            .borrow()
            .as_ref()
            .is_some_and(|(_, requested)| requested == pdf);
        self.requested
            .replace(Some((generation, pdf.to_path_buf())));
        self.requested_stamp.set(stamp);
        self.pending_opens
            .borrow_mut()
            .push((generation, pdf.to_path_buf()));
        if self.shown.borrow().is_none() {
            self.placeholder.set_title("Loading PDF…");
            self.placeholder.set_description(None);
        }
        let pdf = pdf.to_path_buf();
        self.send(if rebuild {
            PdfCmd::Reload { generation, pdf }
        } else {
            PdfCmd::Open { generation, pdf }
        });
    }

    /// Takes the pending `Open` of `generation` (and drops older ones, which the PDF thread has
    /// answered already); returns its path.
    fn take_pending(&self, generation: u64) -> Option<PathBuf> {
        let mut pending = self.pending_opens.borrow_mut();
        let path = pending
            .iter()
            .find(|(g, _)| *g == generation)
            .map(|(_, p)| p.clone());
        pending.retain(|(g, _)| *g > generation);
        path
    }

    fn forward(&self, file: &Path, line: u32, reveal: Option<Reveal>) {
        if self.shown.borrow().is_none() {
            if reveal.is_some()
                && let Some(state) = self.state.upgrade()
            {
                state.toast("There is no PDF to search yet; build with F5");
            }
            return;
        }
        self.forward_reveal.set(reveal);
        self.send(PdfCmd::Forward {
            file: file.to_path_buf(),
            line,
        });
    }

    /// Inverse search from the page the view shows (`win.sync-inverse`).
    fn inverse_from_view(&self) {
        if self.shown.borrow().is_none() {
            if let Some(state) = self.state.upgrade() {
                state.toast("There is no PDF to search yet; build with F5");
            }
            return;
        }
        self.view.inverse_search_current();
    }

    /// Forward search from the active editor's cursor line.
    fn forward_from_cursor(&self, reveal: Option<Reveal>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(editor) = state.active_editor() else {
            return;
        };
        let (line, _) = editor.cursor_position();
        self.forward(editor.document().path(), line, reveal);
    }

    fn handle(self: &Rc<Self>, event: PdfEvent) {
        match event {
            PdfEvent::Opened {
                generation,
                page_sizes,
            } => self.opened(generation, page_sizes),
            PdfEvent::PageSizes {
                generation,
                page_sizes,
            } => self.page_sizes(generation, page_sizes),
            PdfEvent::OpenFailed { generation, error } => self.open_failed(generation, &error),
            PdfEvent::Rendered(rendered) => self.view.add_rendered(rendered),
            PdfEvent::Forward { boxes, .. } => self.forward_answer(&boxes),
            PdfEvent::Inverse { target } => {
                let Some(state) = self.state.upgrade() else {
                    return;
                };
                match target {
                    Some(target) => {
                        // From a PDF in its own window, the editor's window comes forward.
                        if detach::is_detached(&state) {
                            state.window.present();
                        }
                        glib::spawn_future_local(async move {
                            let opened = diagnostics_panel::open_at_line(
                                &state,
                                &target.file,
                                Some(target.line),
                            )
                            .await;
                            if let Err(e) = opened {
                                state.toast(&e);
                            }
                        });
                    }
                    None => state.toast("No source location found at this point"),
                }
            }
            PdfEvent::Links {
                generation,
                page,
                links,
            } => {
                if let Some(l) = self.links.get() {
                    l.links_arrived(generation, page, links);
                }
            }
            PdfEvent::SearchPage {
                id,
                generation,
                page,
                matches,
            } => {
                if let Some(s) = self.search.get() {
                    s.page_results(id, generation, page, matches);
                }
            }
            PdfEvent::SearchDone { id, generation } => {
                if let Some(s) = self.search.get() {
                    s.search_done(id, generation);
                }
            }
            PdfEvent::Thumbnail(thumbnail) => {
                // The PDF thread took it off its queue: so do the lists here, so that the next
                // change of either list does not ask for it again.
                {
                    let mut wants = self.thumb_wants.borrow_mut();
                    let req = thumbnail.req;
                    wants.0.retain(|r| *r != req);
                    wants.1.retain(|r| *r != req);
                }
                let Some((req, texture, bytes)) = thumbnail_texture(thumbnail) else {
                    return;
                };
                if let Some(l) = self.links.get() {
                    l.thumbnail(req, &texture);
                }
                if let Some(s) = self.strip.get() {
                    s.thumbnail(req, &texture, bytes);
                }
            }
            PdfEvent::Outline { generation, items } => {
                if let Some(s) = self.strip.get() {
                    s.outline(generation, items);
                }
            }
            PdfEvent::PageLabels { generation, labels } => {
                if let Some(s) = self.strip.get() {
                    s.page_labels(generation, labels);
                }
            }
        }
    }

    fn opened(&self, generation: u64, page_sizes: Vec<(f64, f64)>) {
        let Some(path) = self.take_pending(generation) else {
            return;
        };
        // A newer generation may already be on screen when answers cross; never go back.
        if self
            .shown
            .borrow()
            .as_ref()
            .is_some_and(|(g, _)| *g > generation)
        {
            return;
        }
        tracing::info!(
            "showing {} ({} pages, generation {generation})",
            path.display(),
            page_sizes.len()
        );
        // A rebuild of the PDF shown keeps the place; another document starts afresh.
        let rebuilt = self
            .shown
            .borrow()
            .as_ref()
            .is_some_and(|(_, shown)| *shown == path);
        self.shown.replace(Some((generation, path)));
        if rebuilt {
            self.view.set_document(generation, page_sizes);
        } else {
            self.view.open_document(generation, page_sizes);
        }
        self.stack.set_visible_child_name("pdf");
        self.refresh_dark();
        self.enable_handoff();
        // M3: the modules follow the new document (after the view, which they read).
        if let Some(l) = self.links.get() {
            l.document_changed(generation);
        }
        if let Some(s) = self.strip.get() {
            s.document_changed(generation, rebuilt);
        }
        if let Some(s) = self.search.get() {
            s.document_changed(generation, rebuilt);
        }
    }

    /// The page sizes of `generation` as the PDF thread read them, after a reload that showed with
    /// those of the generation before: the view lays the pages out again (keeping its place) and
    /// the strip sizes its thumbnails.
    fn page_sizes(&self, generation: u64, page_sizes: Vec<(f64, f64)>) {
        if self.shown.borrow().as_ref().map(|(g, _)| *g) != Some(generation) {
            return;
        }
        self.view.set_page_sizes(generation, page_sizes);
        if let Some(s) = self.strip.get() {
            s.page_sizes_changed();
        }
    }

    fn open_failed(&self, generation: u64, error: &str) {
        tracing::warn!("cannot open the PDF of generation {generation}: {error}");
        self.take_pending(generation);
        // The next build opens the file again, even unchanged.
        if self
            .requested
            .borrow()
            .as_ref()
            .is_some_and(|(g, _)| *g == generation)
        {
            self.requested_stamp.set(None);
        }
        if self.shown.borrow().is_none() {
            self.placeholder.set_title("Cannot Show the PDF");
            self.placeholder.set_description(Some(error));
        } else if let Some(state) = self.state.upgrade() {
            state.toast(&format!("Could not load the new PDF: {error}"));
        }
    }

    fn forward_answer(&self, boxes: &[SyncBox]) {
        let reveal = self.forward_reveal.take();
        let Some(first) = boxes.first() else {
            if reveal.is_some()
                && let Some(state) = self.state.upgrade()
            {
                state.toast("This line has no position in the PDF");
            }
            return;
        };
        // SyncBox pages are 1-based; highlight the boxes on the first page found.
        let page = first.page;
        let rects: Vec<_> = boxes
            .iter()
            .filter(|b| b.page == page)
            .map(|b| {
                (
                    f64::from(b.h),
                    f64::from(b.v_top),
                    f64::from(b.width),
                    f64::from(b.height),
                )
            })
            .collect();
        self.view.flash_boxes(
            page.saturating_sub(1),
            &rects,
            reveal.unwrap_or(Reveal::IfHidden),
        );
    }

    /// The dark mode of the shown document: its Ctrl+Shift+I choice, else the system style
    /// (when `pdf.dark_invert` is on).
    fn effective_dark(&self) -> bool {
        let shown = self.shown.borrow();
        if let Some((_, path)) = shown.as_ref()
            && let Some(dark) = self.dark_overrides.borrow().get(path)
        {
            return *dark;
        }
        self.state
            .upgrade()
            .is_some_and(|s| s.config.pdf.dark_invert)
            && adw::StyleManager::default().is_dark()
    }

    fn refresh_dark(&self) {
        let dark = self.effective_dark();
        self.view.set_dark(dark);
        if let Some(action) = self.state.upgrade().and_then(|s| {
            s.window
                .lookup_action("pdf-invert")
                .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        }) {
            action.set_state(&dark.into());
        }
    }

    fn toggle_dark(&self) {
        let dark = !self.effective_dark();
        match self.shown.borrow().as_ref() {
            Some((_, path)) => {
                self.dark_overrides.borrow_mut().insert(path.clone(), dark);
            }
            None => {
                self.view.set_dark(dark);
                return;
            }
        }
        self.refresh_dark();
    }

    /// Enables the actions that hand the PDF to another application once one is shown.
    fn enable_handoff(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let shown = self.shown.borrow().is_some();
        for name in HANDOFF_ACTIONS {
            if let Some(action) = state
                .window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(shown);
            }
        }
    }

    /// Hands the PDF shown to another application (or to the test hook), with a toast when it
    /// cannot. Scripted runs start none.
    fn hand_off(&self, handoff: Handoff) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some((_, pdf)) = self.shown.borrow().clone() else {
            return;
        };
        if let Some(hook) = self.launch_hook.borrow().as_ref() {
            if !hook(handoff, &pdf) {
                state.toast(handoff.failure());
            }
            return;
        }
        if state.scripted {
            state.toast("Other applications are not started from scripted runs");
            return;
        }
        // The window the view is in (the PDF may have a window of its own).
        let parent = self.view.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(&state);
        glib::spawn_future_local(async move {
            let result = launch(handoff, &pdf, parent.as_ref()).await;
            if let (Err(e), Some(state)) = (result, weak.upgrade())
                && !e.matches(gtk::DialogError::Dismissed)
            {
                state.toast(&format!("{}: {e}", handoff.failure()));
            }
        });
    }

    fn refresh_toolbar(&self) {
        let view = &self.view;
        let count = view.page_count();
        let page = if count == 0 {
            0
        } else {
            view.current_page() + 1
        };
        if !self.toolbar.page.has_focus() {
            self.toolbar.page.set_text(&page.to_string());
        }
        self.toolbar.pages.set_text(&format!("/ {count}"));
        self.toolbar
            .zoom
            .set_text(&format!("{:.0}%", view.zoom() * 100.0));
    }

    /// Follow-cursor mode: forward search after the cursor rests in `editor`'s buffer, while
    /// `pdf.follow_cursor` is on (read at that moment, so a reload of config.toml applies).
    fn follow_cursor(self: &Rc<Self>, buffer: &sourceview5::Buffer) {
        let weak = Rc::downgrade(self);
        buffer.connect_cursor_position_notify(move |buffer| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let active = inner
                .state
                .upgrade()
                .filter(|s| s.config.pdf.follow_cursor)
                .and_then(|s| s.active_editor())
                .is_some_and(|e| e.buffer() == buffer);
            if !active {
                return;
            }
            if let Some(timer) = inner.follow_timer.take() {
                timer.remove();
            }
            let weak = Rc::downgrade(&inner);
            let timer = glib::timeout_add_local_once(FOLLOW_DELAY, move || {
                if let Some(inner) = weak.upgrade() {
                    inner.follow_timer.take();
                    inner.forward_from_cursor(None);
                }
            });
            inner.follow_timer.replace(Some(timer));
        });
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(timer) = self.follow_timer.get_mut().take() {
            timer.remove();
        }
        self.events.get_mut().take();
        let Some(worker) = self.worker.get_mut().take() else {
            return;
        };
        // Wait for the PDF thread to free its poppler document: when the last window closes the
        // process exits next, and poppler tearing down its global state on that thread while
        // exit() runs crashes (GlobalParams::~GlobalParams). The event channel is unbounded, so
        // the thread never waits on a send meanwhile.
        if !worker.shutdown_and_join(SHUTDOWN_WAIT) {
            tracing::warn!("the PDF thread did not stop within {SHUTDOWN_WAIT:?}");
        }
    }
}

/// Hands `pdf` to the system: its default application for PDFs, or the file manager with the
/// file selected.
async fn launch(
    handoff: Handoff,
    pdf: &Path,
    parent: Option<&gtk::Window>,
) -> Result<(), glib::Error> {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(pdf)));
    match handoff {
        Handoff::Open => launcher.launch_future(parent).await,
        // GTK selects the file through the file manager portal or the freedesktop file
        // manager's D-Bus interface (on Windows, through Explorer); macOS has neither, and
        // Finder's `open -R` selects it there.
        Handoff::ShowInFolder if cfg!(target_os = "macos") => {
            let argv = [
                OsStr::new("/usr/bin/open"),
                OsStr::new("-R"),
                pdf.as_os_str(),
            ];
            gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE)?
                .wait_check_future()
                .await
        }
        Handoff::ShowInFolder => launcher.open_containing_folder_future(parent).await,
    }
}

/// The toolbar's menu: the zoom modes, and the PDF's file. The PDF view's own keys are shown
/// beside their items (they are no accelerators, so the menu cannot find them).
fn toolbar_menu() -> gio::Menu {
    let item = |label: &str, action: &str, key: Option<&str>| {
        let item = gio::MenuItem::new(Some(label), Some(action));
        if let Some(key) = key {
            item.set_attribute_value("accel", Some(&key.to_variant()));
        }
        item
    };
    let zoom = gio::Menu::new();
    zoom.append_item(&item("Fit _Width", "win.pdf-fit-width", Some("<Control>0")));
    zoom.append_item(&item("Fit _Page", "win.pdf-fit-page", None));
    zoom.append_item(&item(
        "_Zoom to 100%",
        "win.pdf-zoom-reset",
        Some("<Control>1"),
    ));
    let file = gio::Menu::new();
    file.append_item(&item("_Open in Another App", "win.pdf-open-external", None));
    file.append_item(&item("_Show in Files", "win.pdf-show-in-folder", None));
    let menu = gio::Menu::new();
    menu.append_section(None, &zoom);
    menu.append_section(None, &file);
    menu
}

fn build_toolbar() -> Toolbar {
    let page = gtk::Entry::builder()
        .width_chars(3)
        .max_width_chars(4)
        .xalign(1.0)
        .input_purpose(gtk::InputPurpose::Digits)
        .tooltip_text("Go to Page")
        .valign(gtk::Align::Center)
        .build();
    let pages = gtk::Label::builder()
        .css_classes(["dim-label", "numeric"])
        .build();
    let button = |icon: &str, tooltip: &str, action: &str| {
        gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .action_name(action)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build()
    };
    let zoom = gtk::Label::builder()
        .css_classes(["numeric", "zoom-label"])
        .build();
    let search = gtk::ToggleButton::builder()
        .icon_name("edit-find-symbolic")
        .tooltip_text("Search (Ctrl+F)")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let dark = gtk::ToggleButton::builder()
        .icon_name("weather-clear-night-symbolic")
        .tooltip_text("Dark Pages (Ctrl+Shift+I)")
        .action_name("win.pdf-invert")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("PDF Options")
        .menu_model(&toolbar_menu())
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let spacer = gtk::Box::builder().hexpand(true).build();
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    bar.add_css_class("lemma-pdf-toolbar");
    bar.append(&page);
    bar.append(&pages);
    bar.append(&spacer);
    bar.append(&search);
    bar.append(&button(
        "zoom-out-symbolic",
        "Zoom Out (Ctrl+−)",
        "win.pdf-zoom-out",
    ));
    bar.append(&zoom);
    bar.append(&button(
        "zoom-in-symbolic",
        "Zoom In (Ctrl++)",
        "win.pdf-zoom-in",
    ));
    bar.append(&button(
        "zoom-fit-best-symbolic",
        "Fit Width (Ctrl+0)",
        "win.pdf-fit-width",
    ));
    bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    bar.append(&dark);
    bar.append(&more);
    Toolbar {
        bar,
        page,
        pages,
        zoom,
        search,
    }
}

/// The SyncTeX buttons over the left edge of the view, next to the editor, so that they are found
/// without searching the toolbar (which has no room for their labels in a narrow pane). Their
/// arrows point at the side they jump to: the source on the left, and into the PDF.
fn build_sync_buttons() -> gtk::Box {
    let button = |icon: &str, label: &str, tooltip: &str, action: &str| {
        gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name(icon)
                    .label(label)
                    .build(),
            )
            .tooltip_text(tooltip)
            .action_name(action)
            .css_classes(["osd"])
            .build()
    };
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .css_classes(["linked", "lemma-pdf-sync"])
        .build();
    buttons.append(&button(
        "go-previous-symbolic",
        "Source",
        "Show This Page in the Source",
        "win.sync-inverse",
    ));
    buttons.append(&button(
        "go-next-symbolic",
        "PDF",
        "Show the Cursor in the PDF (Ctrl+Alt+J)",
        "win.sync-forward",
    ));
    buttons
}

/// Called once per window after the layout is built; puts the PDF view into `state.pdf_pane`.
pub fn install(state: &Rc<WindowState>) -> PdfController {
    install_css_once("pdf", CSS);
    let cache_bytes = (state.config.pdf.cache_mib as usize).max(1) << 20;
    let view = PdfView::new(cache_bytes);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .hexpand(true)
        .vexpand(true)
        .build();
    let toolbar = build_toolbar();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("lemma-pdf-content");
    content.append(&toolbar.bar);
    // M3 (WP3.0): the strip of thumbnails and bookmarks at the left of the view, and its
    // toggle at the start of the toolbar (see `WindowState::pdf_sidebar`).
    toolbar.bar.prepend(&state.pdf_sidebar.toggle);
    let sync_buttons = build_sync_buttons();
    let view_overlay = gtk::Overlay::builder().child(&scroller).build();
    view_overlay.add_overlay(&sync_buttons);
    state.pdf_sidebar.split.set_content(Some(&view_overlay));
    content.append(&state.pdf_sidebar.split);
    let placeholder = adw::StatusPage::builder()
        .icon_name("view-paged-symbolic")
        .title("No PDF Yet")
        .description("Build with F5 to see the preview here.")
        .css_classes(["compact", "dim-label"])
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&content, Some("pdf"));
    stack.add_css_class("lemma-pdf-stack");
    stack.set_visible_child_name("empty");
    state.pdf_pane.set_child(Some(&stack));

    let (events, receiver) = async_channel::unbounded();
    let inner = Rc::new(Inner {
        state: Rc::downgrade(state),
        view,
        stack,
        placeholder,
        toolbar,
        sync_buttons,
        events: RefCell::new(Some(events)),
        worker: RefCell::new(None),
        last_generation: Cell::new(0),
        pending_opens: RefCell::new(Vec::new()),
        requested: RefCell::new(None),
        requested_stamp: Cell::new(None),
        shown: RefCell::new(None),
        dark_overrides: RefCell::new(HashMap::new()),
        forward_reveal: Cell::new(None),
        follow_timer: RefCell::new(None),
        content,
        search: OnceCell::new(),
        links: OnceCell::new(),
        strip: OnceCell::new(),
        thumb_wants: RefCell::new((Vec::new(), Vec::new())),
        launch_hook: RefCell::new(None),
    });

    let weak = Rc::downgrade(&inner);
    glib::spawn_future_local(async move {
        while let Ok(event) = receiver.recv().await {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.handle(event);
        }
    });

    let weak = Rc::downgrade(&inner);
    inner.view.set_request_handler(move |requests| {
        if let Some(worker) = weak.upgrade().and_then(|i| i.worker.borrow().clone()) {
            worker.send(PdfCmd::Want(requests));
        }
    });
    let weak = Rc::downgrade(&inner);
    inner.view.set_inverse_handler(move |page, x_bp, y_bp| {
        if let Some(inner) = weak.upgrade()
            && inner.shown.borrow().is_some()
        {
            inner.send(PdfCmd::Inverse { page, x_bp, y_bp });
        }
    });
    let weak = Rc::downgrade(&inner);
    inner.view.connect_changed(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.refresh_toolbar();
        }
    });
    let weak = Rc::downgrade(&inner);
    inner.toolbar.page.connect_activate(move |entry| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let count = inner.view.page_count();
        match entry.text().trim().parse::<u32>() {
            Ok(page) if (1..=count).contains(&page) => inner.view.goto_page(page - 1),
            _ => inner.refresh_toolbar(),
        }
        inner.view.grab_focus();
    });

    install_actions(state, &inner);
    connect_style(state, &inner);
    // A reload of config.toml: `pdf.dark_invert` applies at once, and `pdf.follow_cursor` is
    // read whenever the cursor moves. `pdf.cache_mib` sizes the cache of windows opened later.
    let weak = Rc::downgrade(&inner);
    state.connect_config_changed(move |_, file| {
        if file == crate::settings::ConfigFile::Config
            && let Some(inner) = weak.upgrade()
        {
            inner.refresh_dark();
        }
    });
    let weak = Rc::downgrade(&inner);
    state.tab_view.connect_page_attached(move |_, page, _| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let child = page.child();
        let editor = inner.state.upgrade().and_then(|s| {
            s.editors()
                .into_iter()
                .find(|e| e.widget().upcast_ref::<gtk::Widget>() == &child)
        });
        if let Some(editor) = editor {
            inner.follow_cursor(editor.buffer());
        }
    });
    // A hidden pane gives its textures back, and the PDF thread its SyncTeX scanner (ARCHITECTURE
    // §7).
    let weak = Rc::downgrade(&inner);
    state.pdf_pane.connect_visible_notify(move |pane| {
        if !pane.is_visible()
            && let Some(inner) = weak.upgrade()
        {
            inner.view.trim_cache();
            crate::heap::trim();
            // Unless the PDF went to a window of its own, which hides the pane too (the window
            // is recorded after that).
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(inner) = weak.upgrade()
                    && let Some(state) = inner.state.upgrade()
                    && !state.pdf_pane.is_visible()
                    && !detach::is_detached(&state)
                {
                    inner.trim_worker();
                }
            });
        }
    });
    inner.refresh_toolbar();
    PdfController { inner }
}

fn install_actions(state: &Rc<WindowState>, inner: &Rc<Inner>) {
    let window = &state.window;
    let simple = |name: &str, run: fn(&Rc<Inner>)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(inner);
        action.connect_activate(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                run(&inner);
            }
        });
        window.add_action(&action);
        action
    };
    simple("pdf-zoom-in", |i| i.view.zoom_in());
    simple("pdf-zoom-out", |i| i.view.zoom_out());
    simple("pdf-fit-width", |i| i.view.set_fit_width());
    simple("pdf-fit-page", |i| i.view.set_fit_page());
    simple("pdf-zoom-reset", |i| i.view.reset_zoom());
    simple("sync-forward", |i| {
        i.forward_from_cursor(Some(Reveal::Always))
    });
    simple("sync-inverse", |i| i.inverse_from_view());
    // Until a PDF is shown (`Inner::enable_handoff`).
    simple("pdf-open-external", |i| i.hand_off(Handoff::Open)).set_enabled(false);
    simple("pdf-show-in-folder", |i| i.hand_off(Handoff::ShowInFolder)).set_enabled(false);

    let invert = gio::SimpleAction::new_stateful("pdf-invert", None, &false.into());
    let weak = Rc::downgrade(inner);
    invert.connect_activate(move |_, _| {
        if let Some(inner) = weak.upgrade() {
            inner.toggle_dark();
        }
    });
    window.add_action(&invert);
    set_default_accels(state, ACCELS);
}

/// Dark mode follows the system style unless the document has its own choice.
fn connect_style(state: &Rc<WindowState>, inner: &Rc<Inner>) {
    let style = adw::StyleManager::default();
    let weak = Rc::downgrade(inner);
    let handler = style.connect_dark_notify(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.refresh_dark();
        }
    });
    let handler = RefCell::new(Some(handler));
    state.window.connect_destroy(move |_| {
        if let Some(handler) = handler.take() {
            adw::StyleManager::default().disconnect(handler);
        }
    });
    inner.refresh_dark();
}
