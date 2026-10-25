//! The PDF crop of a reference hover (QoL): the region of the labelled environment in the PDF
//! preview, rendered at 2× and shown instead of its source lines, when the PDF has a SyncTeX
//! box for it.
//!
//! **How.** A PDF thread of its own — the preview's worker (`lemma_pdf::spawn`, poppler and
//! SyncTeX on one thread), a second instance, so the preview's queue and its forward-search
//! highlight are left alone — is started by the first crop and stopped [`IDLE_STOP`] after the
//! last one, so nothing stays in memory while hovers are not used. It opens the PDF the
//! preview shows (again after a rebuild), answers a forward search for each source line of the
//! environment, and renders the tiles of the boxes' region (padded) at twice
//! [`DISPLAY_ZOOM`]; the tiles are put together on the main thread into one texture, dark-mode
//! coloured like the preview when it is dark, and shown at [`DISPLAY_ZOOM`] logical pixels per
//! point. One crop is prepared at a time (the newest hover wins). No box (no SyncTeX data, a
//! label written after the last build, a mismatched file), an error or a timeout: the source
//! lines stay.

use crate::window::WindowState;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use lemma_pdf::{PageKey, PdfCmd, PdfEvent, PdfHandle, RenderReq, SyncBox};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Logical pixels per PDF point of the crop.
pub const DISPLAY_ZOOM: f64 = 1.4;
/// The crop's resolution relative to [`DISPLAY_ZOOM`].
const OVERSAMPLE: f64 = 2.0;
/// Padding around the boxes, in points (little above: the descenders of the line before a
/// display reach into its top skip).
const PAD_X: f64 = 10.0;
const PAD_TOP: f64 = 2.5;
const PAD_BOTTOM: f64 = 4.0;
/// The tallest region shown, in points (a long environment shows its beginning).
const MAX_HEIGHT: f64 = 240.0;
/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(3);
/// A source saved at most this long after its PDF counts as the one it was built from.
const SAME_TIME: Duration = Duration::from_secs(1);
/// The PDF thread stops this long after the last crop.
pub const IDLE_STOP: Duration = Duration::from_secs(30);

/// The crop worker of one window.
struct Worker {
    window: glib::WeakRef<adw::ApplicationWindow>,
    handle: PdfHandle,
    events: async_channel::Receiver<PdfEvent>,
    /// The PDF opened (path, modification stamp) and its generation and page sizes.
    opened: RefCell<Option<Opened>>,
    generation: Cell<u64>,
    /// A crop is being prepared.
    busy: Cell<bool>,
    idle: RefCell<Option<glib::SourceId>>,
}

#[derive(Clone)]
struct Opened {
    pdf: PathBuf,
    stamp: Option<(std::time::SystemTime, u64)>,
    generation: u64,
    page_sizes: Vec<(f64, f64)>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(idle) = self.idle.get_mut().take() {
            idle.remove();
        }
        // The thread lets go of its poppler document before the process may exit.
        self.handle.shutdown_and_join(Duration::from_secs(2));
    }
}

/// The content of a hover waiting for its crop, and whether the crop settled.
type PendingCrop = (glib::WeakRef<gtk::Box>, Rc<Cell<bool>>);

thread_local! {
    static WORKERS: RefCell<Vec<Rc<Worker>>> = const { RefCell::new(Vec::new()) };
    /// Bumped by every crop request: an older one gives up.
    static LATEST: Cell<u64> = const { Cell::new(0) };
    /// The crops being prepared or shown, by the view of their hover (for the demo).
    static PENDING: RefCell<Vec<PendingCrop>> = const { RefCell::new(Vec::new()) };
}

fn worker_of(state: &WindowState) -> Option<Rc<Worker>> {
    WORKERS.with_borrow_mut(|workers| {
        workers.retain(|w| w.window.upgrade().is_some());
        workers
            .iter()
            .find(|w| w.window.upgrade().as_ref() == Some(&state.window))
            .cloned()
    })
}

fn start_worker(state: &WindowState) -> Rc<Worker> {
    if let Some(worker) = worker_of(state) {
        return worker;
    }
    let (sender, events) = async_channel::unbounded();
    let worker = Rc::new(Worker {
        window: state.window.downgrade(),
        handle: lemma_pdf::spawn(sender),
        events,
        opened: RefCell::new(None),
        generation: Cell::new(0),
        busy: Cell::new(false),
        idle: RefCell::new(None),
    });
    WORKERS.with_borrow_mut(|workers| workers.push(worker.clone()));
    // Closing the window stops it.
    let window = state.window.downgrade();
    state.window.connect_destroy(move |_| {
        if let Some(window) = window.upgrade() {
            stop_worker_of(&window);
        }
    });
    worker
}

fn stop_worker_of(window: &adw::ApplicationWindow) {
    let gone: Vec<Rc<Worker>> = WORKERS.with_borrow_mut(|workers| {
        let (gone, kept) = std::mem::take(workers)
            .into_iter()
            .partition(|w| w.window.upgrade().as_ref() == Some(window));
        *workers = kept;
        gone
    });
    drop(gone);
}

/// Whether the crop worker of `state`'s window runs (tests).
pub fn worker_running(state: &WindowState) -> bool {
    worker_of(state).is_some()
}

/// Stops the crop worker of `state`'s window now (tests; normally [`IDLE_STOP`] after the last
/// crop).
pub fn stop_worker(state: &WindowState) {
    stop_worker_of(&state.window);
}

/// A crop being prepared for one hover.
pub(super) struct CropRequest {
    state: Weak<WindowState>,
    pdf: PathBuf,
    dark: bool,
    source: PathBuf,
    lines: Range<u32>,
    /// The 1-based page the `.aux` file gives, when it is a number: boxes on another page are
    /// not the label's.
    page: Option<u32>,
    id: u64,
}

/// Starts preparing the crop of the source `lines` (0-based) of `path` (the environment of a
/// label the last build numbered, on the page `aux_page` it gave): `None` when the preview
/// shows no PDF, or when the source changed since the PDF was made (its lines would not be the
/// PDF's: unsaved changes, or a save after the build).
pub(super) fn request(
    state: &Rc<WindowState>,
    path: &Path,
    lines: Range<u32>,
    aux_page: Option<&str>,
) -> Option<CropRequest> {
    let pdf = state.pdf.get()?;
    let shown = pdf.pdf()?;
    if state.workspace.get(path).is_some_and(|d| d.is_dirty()) {
        return None;
    }
    // (Within a second: copies of a project do not keep the order of the times.)
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (modified(path), modified(&shown)) {
        (Some(source), Some(made)) if source <= made + SAME_TIME => {}
        _ => return None,
    }
    let id = LATEST.with(|l| {
        l.set(l.get() + 1);
        l.get()
    });
    Some(CropRequest {
        state: Rc::downgrade(state),
        pdf: shown,
        dark: pdf.is_dark(),
        source: path.to_path_buf(),
        lines,
        page: aux_page.and_then(|p| p.trim().parse::<u32>().ok()),
        id,
    })
}

/// Shows the crop in `content` instead of `code` once it is rendered (the code stays when
/// there is none).
pub(super) fn show_when_ready(request: CropRequest, content: &gtk::Box, code: &gtk::Label) {
    let settled = Rc::new(Cell::new(false));
    PENDING.with_borrow_mut(|pending| {
        pending.retain(|(b, _)| b.upgrade().is_some());
        pending.push((content.downgrade(), settled.clone()));
    });
    let (content, code) = (content.downgrade(), code.downgrade());
    glib::spawn_future_local(async move {
        let crop = render(&request).await;
        settled.set(true);
        let (Some(crop), Some(content), Some(code)) = (crop, content.upgrade(), code.upgrade())
        else {
            return;
        };
        let view = CropView::new(crop.texture, crop.width, crop.height);
        view.set_tooltip_text(Some(&format!("Page {} of the PDF", crop.page + 1)));
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("lemma-hover-crop-frame");
        if request.dark {
            frame.add_css_class("dark");
        }
        frame.set_halign(gtk::Align::Start);
        frame.append(&view);
        content.insert_child_after(&frame, code.prev_sibling().as_ref());
        code.set_visible(false);
        // A popover is sized when it is presented: again, for its new content.
        if let Some(popover) = content
            .ancestor(gtk::Popover::static_type())
            .and_then(|p| p.downcast::<gtk::Popover>().ok())
            && popover.is_visible()
        {
            popover.present();
        }
    });
}

/// Whether the crop of the hover whose content holds `view`'s … has settled: shown, or known
/// not to come (the demo waits for it).
pub(super) fn settled(_view: &crate::editor::TexView) -> bool {
    PENDING.with_borrow_mut(|pending| {
        pending.retain(|(b, _)| b.upgrade().is_some());
        pending.iter().all(|(_, settled)| settled.get())
    })
}

/// A rendered crop.
struct Crop {
    texture: gtk::gdk::Texture,
    /// Logical size.
    width: f64,
    height: f64,
    page: u32,
}

/// Waits for the worker of `state` to be free; false when a newer request came meanwhile.
async fn take_turn(worker: &Worker, id: u64) -> bool {
    while worker.busy.get() {
        glib::timeout_future(Duration::from_millis(15)).await;
    }
    LATEST.with(Cell::get) == id
}

async fn render(request: &CropRequest) -> Option<Crop> {
    let state = request.state.upgrade()?;
    let worker = start_worker(&state);
    if !take_turn(&worker, request.id).await {
        return None;
    }
    worker.busy.set(true);
    let crop = render_with(&worker, request).await;
    worker.busy.set(false);
    // Stop the thread after a while without crops.
    if let Some(idle) = worker.idle.take() {
        idle.remove();
    }
    let window = worker.window.clone();
    let id = glib::timeout_add_local_once(IDLE_STOP, move || {
        if let Some(window) = window.upgrade() {
            if let Some(worker) = WORKERS.with_borrow(|w| {
                w.iter()
                    .find(|w| w.window.upgrade().as_ref() == Some(&window))
                    .cloned()
            }) {
                worker.idle.take();
            }
            stop_worker_of(&window);
        }
    });
    worker.idle.replace(Some(id));
    crop
}

/// The next event, within [`STEP_TIMEOUT`].
async fn next_event(worker: &Worker) -> Option<PdfEvent> {
    glib::future_with_timeout(STEP_TIMEOUT, worker.events.recv())
        .await
        .ok()?
        .ok()
}

fn stamp(pdf: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(pdf).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

async fn render_with(worker: &Worker, request: &CropRequest) -> Option<Crop> {
    // Answers of an earlier request that gave up.
    while worker.events.try_recv().is_ok() {}
    // The PDF the preview shows, opened again when it changed on disk (a rebuild).
    let stamp = stamp(&request.pdf);
    let current = worker.opened.borrow().clone();
    let opened = match current {
        Some(o) if o.pdf == request.pdf && o.stamp == stamp => o,
        _ => {
            let generation = worker.generation.get() + 1;
            worker.generation.set(generation);
            worker.handle.send(PdfCmd::Open {
                generation,
                pdf: request.pdf.clone(),
            });
            let opened = loop {
                match next_event(worker).await? {
                    PdfEvent::Opened {
                        generation: g,
                        page_sizes,
                    } if g == generation => {
                        break Opened {
                            pdf: request.pdf.clone(),
                            stamp,
                            generation,
                            page_sizes,
                        };
                    }
                    PdfEvent::OpenFailed { generation: g, .. } if g == generation => {
                        worker.opened.replace(None);
                        return None;
                    }
                    _ => {}
                }
            };
            worker.opened.replace(Some(opened.clone()));
            opened
        }
    };
    // The boxes of the environment's lines, on the first page that has any.
    let mut all: Vec<SyncBox> = Vec::new();
    for line in request.lines.clone() {
        worker.handle.send(PdfCmd::Forward {
            file: request.source.clone(),
            line: line + 1,
        });
        loop {
            if let PdfEvent::Forward { boxes, .. } = next_event(worker).await? {
                all.extend(boxes);
                break;
            }
        }
        if LATEST.with(Cell::get) != request.id {
            return None;
        }
    }
    let first = all.first()?.page;
    if request.page.is_some_and(|page| page != first) {
        return None;
    }
    let boxes: Vec<SyncBox> = all.into_iter().filter(|b| b.page == first).collect();
    let page = boxes.first()?.page.checked_sub(1)?;
    let size = *opened.page_sizes.get(page as usize)?;
    let region = region_of(&boxes, size)?;
    let texture = render_region(worker, &opened, page, size, region, request.dark).await?;
    Some(Crop {
        texture,
        width: region.2 * DISPLAY_ZOOM,
        height: region.3 * DISPLAY_ZOOM,
        page,
    })
}

/// The region `(x, y, width, height)` (points, top-left origin) around `boxes` on a page of
/// `size`: padded, clamped to the page, at most [`MAX_HEIGHT`] tall.
pub fn region_of(boxes: &[SyncBox], size: (f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let page = boxes.first()?.page;
    let on_page = boxes.iter().filter(|b| b.page == page);
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for b in on_page {
        x0 = x0.min(f64::from(b.h));
        y0 = y0.min(f64::from(b.v_top));
        x1 = x1.max(f64::from(b.h + b.width));
        y1 = y1.max(f64::from(b.v_top + b.height));
    }
    if !(x1 > x0 && y1 > y0) {
        return None;
    }
    let x0 = (x0 - PAD_X).max(0.0);
    let y0 = (y0 - PAD_TOP).max(0.0);
    let x1 = (x1 + PAD_X).min(size.0);
    let y1 = (y1 + PAD_BOTTOM).min(size.1).min(y0 + MAX_HEIGHT);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// Renders `region` of `page` at twice [`DISPLAY_ZOOM`]: the tiles that cover it (or the
/// whole page when it is not tiled at that scale), put together into one texture.
async fn render_region(
    worker: &Worker,
    opened: &Opened,
    page: u32,
    size: (f64, f64),
    region: (f64, f64, f64, f64),
    dark: bool,
) -> Option<gtk::gdk::Texture> {
    let scale_q = lemma_pdf::scale_q(DISPLAY_ZOOM, OVERSAMPLE);
    let scale = lemma_pdf::scale_of(scale_q);
    let (page_w, page_h) = lemma_pdf::page_pixels(size, scale_q);
    let px = |v: f64| (v * scale).round() as i32;
    let (rx, ry) = (px(region.0).clamp(0, page_w), px(region.1).clamp(0, page_h));
    let (rw, rh) = (
        (px(region.0 + region.2) - rx).clamp(1, page_w - rx),
        (px(region.1 + region.3) - ry).clamp(1, page_h - ry),
    );
    let tiles: Vec<Option<(u16, u16)>> =
        match lemma_pdf::tiles_covering(size, scale_q, (rx, ry, rw, rh)) {
            None => vec![None],
            Some(tiles) => tiles.into_iter().map(Some).collect(),
        };
    let wants: Vec<RenderReq> = tiles
        .iter()
        .map(|tile| RenderReq {
            key: PageKey {
                generation: opened.generation,
                page,
                scale_q,
                tile: *tile,
                dark: false,
            },
        })
        .collect();
    worker.handle.send(PdfCmd::Want(wants.clone()));
    // The tiles copied into one bitmap of the region (both BGRA premultiplied, opaque).
    let row_bytes = rw as usize * 4;
    let mut data = vec![255u8; row_bytes * rh as usize];
    let mut missing = wants.len();
    while missing > 0 {
        let PdfEvent::Rendered(rendered) = next_event(worker).await? else {
            continue;
        };
        if !wants.iter().any(|w| w.key == rendered.key) {
            continue;
        }
        missing -= 1;
        let (tx, ty) = match rendered.key.tile {
            Some(tile) => {
                let (x, y, _, _) = lemma_pdf::tile_rect(size, scale_q, tile);
                (x, y)
            }
            None => (0, 0),
        };
        let (x0, x1) = (tx.max(rx), (tx + rendered.width).min(rx + rw));
        let (y0, y1) = (ty.max(ry), (ty + rendered.height).min(ry + rh));
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let stride = rendered.stride as usize;
        let src: &[u8] = &rendered.data;
        let bytes = (x1 - x0) as usize * 4;
        for y in y0..y1 {
            let from = (y - ty) as usize * stride + (x0 - tx) as usize * 4;
            let to = (y - ry) as usize * row_bytes + (x0 - rx) as usize * 4;
            if let (Some(src), Some(dst)) =
                (src.get(from..from + bytes), data.get_mut(to..to + bytes))
            {
                dst.copy_from_slice(src);
            }
        }
    }
    if dark {
        // As the PDF view's dark pages are coloured.
        lemma_pdf::dark::darken_bgra(&mut data);
    }
    let bytes = glib::Bytes::from_owned(data);
    Some(
        gtk::gdk::MemoryTexture::new(
            rw,
            rh,
            gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            row_bytes,
        )
        .upcast(),
    )
}

// ----- the widget -----

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CropView {
        pub(super) texture: RefCell<Option<gtk::gdk::Texture>>,
        pub(super) size: Cell<(f64, f64)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CropView {
        const NAME: &'static str = "LemmaCropView";
        type Type = super::CropView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("cropview");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for CropView {}

    impl WidgetImpl for CropView {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self.size.get();
            let v = match orientation {
                gtk::Orientation::Horizontal => w,
                _ => h,
            }
            .ceil() as i32;
            (v, v, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(texture) = self.texture.borrow().as_ref() {
                let (w, h) = self.size.get();
                snapshot.append_texture(
                    texture,
                    &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
                );
            }
        }
    }
}

glib::wrapper! {
    /// A crop of the PDF at a fixed logical size (the texture has twice its pixels).
    pub struct CropView(ObjectSubclass<imp::CropView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CropView {
    fn new(texture: gtk::gdk::Texture, width: f64, height: f64) -> Self {
        let view: Self = glib::Object::new();
        view.imp().texture.replace(Some(texture));
        view.imp().size.set((width, height));
        view.set_can_focus(false);
        view.add_css_class("lemma-hover-crop");
        view.update_property(&[gtk::accessible::Property::Label(
            "The labelled place in the PDF",
        )]);
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_box(page: u32, h: f32, v_top: f32, width: f32, height: f32) -> SyncBox {
        SyncBox {
            page,
            h,
            v_top,
            width,
            height,
        }
    }

    #[test]
    fn regions_pad_and_clamp() {
        let a4 = (595.0, 842.0);
        let region = region_of(
            &[
                sync_box(2, 100.0, 200.0, 300.0, 12.0),
                sync_box(2, 150.0, 214.0, 100.0, 12.0),
                sync_box(3, 0.0, 0.0, 10.0, 10.0),
            ],
            a4,
        )
        .unwrap();
        assert_eq!(region, (90.0, 197.5, 320.0, 32.5));
        // Near the page edge: clamped.
        let region = region_of(&[sync_box(1, 2.0, 1.0, 590.0, 10.0)], a4).unwrap();
        assert_eq!(region, (0.0, 0.0, 595.0, 15.0));
        // Tall environments show their beginning.
        let region = region_of(&[sync_box(1, 100.0, 100.0, 300.0, 600.0)], a4).unwrap();
        assert_eq!(region.3, MAX_HEIGHT);
        assert_eq!(region_of(&[], a4), None);
        assert_eq!(region_of(&[sync_box(1, 1.0, 1.0, 0.0, 0.0)], a4), None);
    }
}
