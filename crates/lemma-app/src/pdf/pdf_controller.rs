//! The PDF controller (ARCHITECTURE §6): owns the PDF thread handle and the document
//! generations, reloads after builds, and runs forward/inverse SyncTeX.
//!
//! The PDF pane shows a placeholder until the first PDF opens, then a toolbar (page n/N, zoom
//! out/in, fit width, dark pages) above the [`PdfView`]. The PDF thread starts with the first
//! [`PdfController::open`]. Every open gets a new generation; the view keeps showing the previous
//! generation until the new one has opened, and a PDF that fails to open leaves it in place. A
//! new generation of the PDF shown keeps the scroll position; another PDF starts at the top.
//!
//! M3 (WP3.4): the controller also dispatches the PDF thread's answers to the [`links`],
//! [`search`] and [`sidebar`] modules (each registers itself here when it installs), sends their
//! commands, and merges the thumbnail requests of the strip and of the link preview into one
//! latest-wins list. Its toolbar gains a search button; the search bar sits below the toolbar
//! ([`PdfController::content`]).
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
use lemma_pdf::{PdfCmd, PdfEvent, PdfHandle, SyncBox, ThumbReq, Thumbnail};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
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
.lemma-pdf-content searchbar > revealer > box { padding: 4px 6px; }
.lemma-pdf-content searchbar entry { min-height: 26px; }
.lemma-pdf-content searchbar button { min-height: 26px; min-width: 26px; padding: 2px; }
.lemma-pdf-search-count { min-width: 4.5em; }
";

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
    /// Given to the PDF thread when it starts; `None` once the controller shuts down.
    events: RefCell<Option<async_channel::Sender<PdfEvent>>>,
    worker: RefCell<Option<PdfHandle>>,
    last_generation: Cell<u64>,
    /// `Open`s sent and not answered yet, oldest first.
    pending_opens: RefCell<Vec<(u64, PathBuf)>>,
    /// The latest `Open` sent.
    requested: RefCell<Option<(u64, PathBuf)>>,
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
        self.inner.open(pdf);
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

    fn open(&self, pdf: &Path) {
        let generation = self.last_generation.get() + 1;
        self.last_generation.set(generation);
        self.requested
            .replace(Some((generation, pdf.to_path_buf())));
        self.pending_opens
            .borrow_mut()
            .push((generation, pdf.to_path_buf()));
        if self.shown.borrow().is_none() {
            self.placeholder.set_title("Loading PDF…");
            self.placeholder.set_description(None);
        }
        self.send(PdfCmd::Open {
            generation,
            pdf: pdf.to_path_buf(),
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
        // M3: the modules follow the new document (after the view, which they read).
        if let Some(l) = self.links.get() {
            l.document_changed(generation);
        }
        if let Some(s) = self.strip.get() {
            s.document_changed(generation, rebuilt);
        }
        if let Some(s) = self.search.get() {
            s.document_changed(generation);
        }
    }

    fn open_failed(&self, generation: u64, error: &str) {
        tracing::warn!("cannot open the PDF of generation {generation}: {error}");
        self.take_pending(generation);
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
    Toolbar {
        bar,
        page,
        pages,
        zoom,
        search,
    }
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
    state.pdf_sidebar.split.set_content(Some(&scroller));
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
        events: RefCell::new(Some(events)),
        worker: RefCell::new(None),
        last_generation: Cell::new(0),
        pending_opens: RefCell::new(Vec::new()),
        requested: RefCell::new(None),
        shown: RefCell::new(None),
        dark_overrides: RefCell::new(HashMap::new()),
        forward_reveal: Cell::new(None),
        follow_timer: RefCell::new(None),
        content,
        search: OnceCell::new(),
        links: OnceCell::new(),
        strip: OnceCell::new(),
        thumb_wants: RefCell::new((Vec::new(), Vec::new())),
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
    // A hidden pane gives its textures back (ARCHITECTURE §7).
    let weak = Rc::downgrade(&inner);
    state.pdf_pane.connect_visible_notify(move |pane| {
        if !pane.is_visible()
            && let Some(inner) = weak.upgrade()
        {
            inner.view.trim_cache();
            // SAFETY: malloc_trim only returns free heap pages to the OS.
            unsafe {
                libc::malloc_trim(0);
            }
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
    };
    simple("pdf-zoom-in", |i| i.view.zoom_in());
    simple("pdf-zoom-out", |i| i.view.zoom_out());
    simple("pdf-fit-width", |i| i.view.set_fit_width());
    simple("sync-forward", |i| {
        i.forward_from_cursor(Some(Reveal::Always))
    });

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
