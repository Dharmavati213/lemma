//! Regression tests for PDF view and controller defects found in the M1 review (area C).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress_c`.
//!
//! Most tests drive a bare [`PdfView`] with a fake PDF thread: the test captures the view's
//! render requests and answers them with plain grey bitmaps of the right size, so the order of
//! deliveries and frames is deterministic. The others use the real window, controller and PDF
//! thread on the committed fixtures.

mod common;

use adw::prelude::*;
use gtk::{graphene, gsk};
use lemma_app::pdf::pdf_view::{self, PdfView};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use lemma_pdf::{PageKey, RenderReq, Rendered, page_pixels, scale_q, tile_rect};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    common::run(&[
        (
            "regress_prefetch_overflow_evicts_visible_page",
            prefetch_overflow_evicts_visible_page,
        ),
        (
            "regress_prefetch_overflow_rerenders_on_every_scroll",
            prefetch_overflow_rerenders_on_every_scroll,
        ),
        (
            "regress_cache_smaller_than_a_page_never_shows_it",
            cache_smaller_than_a_page_never_shows_it,
        ),
        (
            "regress_rebuild_blanks_tiled_pages",
            rebuild_blanks_tiled_pages,
        ),
        (
            "zoom_in_the_tiled_range_keeps_the_old_tiles",
            zoom_in_the_tiled_range_keeps_the_old_tiles,
        ),
        (
            "regress_reload_with_wider_page_keeps_stale_render_scale",
            reload_with_wider_page_keeps_stale_render_scale,
        ),
        (
            "regress_other_pdf_keeps_the_previous_anchor",
            other_pdf_keeps_the_previous_anchor,
        ),
        (
            "regress_hidden_pane_renders_on_rebuild",
            hidden_pane_renders_on_rebuild,
        ),
        (
            "regress_hiding_does_not_cancel_inflight_renders",
            hiding_does_not_cancel_inflight_renders,
        ),
        // Last: a failing run leaves seconds of work queued on the main loop.
        (
            "regress_huge_mediabox_blocks_the_main_thread",
            huge_mediabox_blocks_the_main_thread,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);
const A4: (f64, f64) = (595.276, 841.89);
const LETTER: (f64, f64) = (612.0, 792.0);
const MIB: f64 = 1048576.0;

// ----- A bare PdfView with a fake PDF thread -----

/// A `PdfView` in its own window whose render requests are captured instead of rendered.
struct Bench {
    window: gtk::Window,
    view: PdfView,
    requests: Rc<RefCell<Vec<Vec<RenderReq>>>>,
    sizes: RefCell<Vec<(f64, f64)>>,
}

impl Bench {
    fn new(cache_bytes: usize) -> Self {
        let view = PdfView::new(cache_bytes);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        view.set_request_handler(move |reqs| sink.borrow_mut().push(reqs));
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hexpand(true)
            .vexpand(true)
            .build();
        let window = gtk::Window::builder()
            .default_width(800)
            .default_height(700)
            .child(&scroller)
            .build();
        window.present();
        common::pump_until(TIMEOUT, || {
            view.is_mapped() && view.width() > 0 && view.height() > 0
        });
        Self {
            window,
            view,
            requests,
            sizes: RefCell::new(Vec::new()),
        }
    }

    /// Shows a document of `generation` with these page sizes (what `PdfEvent::Opened` does).
    fn open(&self, generation: u64, sizes: Vec<(f64, f64)>) {
        self.sizes.replace(sizes.clone());
        self.view.set_document(generation, sizes);
    }

    /// Answers `reqs` in order, like the PDF thread. With `draw_between`, the view draws a frame
    /// after each bitmap (a live window on a fast display); otherwise the bitmaps all arrive
    /// before the next frame (Broadway, or a window that is not being drawn).
    fn deliver(&self, reqs: &[RenderReq], draw_between: bool, grey: f64) -> usize {
        for req in reqs {
            let size = self.sizes.borrow()[req.key.page as usize];
            self.view.add_rendered(bitmap(req.key, size, grey));
            if draw_between {
                draw(&self.view);
            }
        }
        reqs.len()
    }

    /// Acts as the PDF thread until the view stops asking (latest request wins); returns the
    /// number of bitmaps delivered.
    fn serve(&self, draw_between: bool, grey: f64) -> usize {
        let deadline = Instant::now() + TIMEOUT;
        let mut delivered = 0;
        let mut quiet_since = Instant::now();
        loop {
            common::pump_for(Duration::from_millis(20));
            let latest = std::mem::take(&mut *self.requests.borrow_mut()).pop();
            match latest {
                Some(reqs) if !reqs.is_empty() => {
                    delivered += self.deliver(&reqs, draw_between, grey);
                    quiet_since = Instant::now();
                }
                _ if quiet_since.elapsed() > Duration::from_millis(300) => return delivered,
                _ => {}
            }
            assert!(Instant::now() < deadline, "the view keeps requesting");
        }
    }

    /// Sets the zoom so that zoom × device scale is `scale` and waits for the re-render scale.
    fn zoom_to_scale(&self, scale: f64) -> u16 {
        let view = &self.view;
        view.set_zoom(scale / view.device_scale(), None);
        let q = scale_q(view.zoom(), view.device_scale());
        common::pump_until(TIMEOUT, || view.render_scale_q() == q);
        q
    }

    fn rendered(&self) -> Vec<(u32, bool)> {
        let view = &self.view;
        view.visible_pages()
            .map(|p| (p, view.is_page_rendered(p)))
            .collect()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        self.window.destroy();
    }
}

/// A uniform grey bitmap for `key` of a page of `size_bp`.
fn bitmap(key: PageKey, size_bp: (f64, f64), grey: f64) -> Rendered {
    let (width, height) = match key.tile {
        None => page_pixels(size_bp, key.scale_q),
        Some(tile) => {
            let (_, _, w, h) = tile_rect(size_bp, key.scale_q, tile);
            (w, h)
        }
    };
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).unwrap();
    {
        let cr = cairo::Context::new(&surface).unwrap();
        cr.set_source_rgb(grey, grey, grey);
        cr.paint().unwrap();
    }
    let stride = surface.stride();
    let data = surface.take_data().unwrap();
    Rendered {
        key,
        width,
        height,
        stride,
        data,
    }
}

/// Draws a real frame of the view (so it draws, and uses, its textures) and returns the view's
/// render node from it. (A widget's cached node is dropped whenever it queues a redraw, so it
/// is taken right after the paint.)
fn draw(view: &PdfView) -> gsk::RenderNode {
    let clock = view.frame_clock().expect("the view is realized");
    let node = Rc::new(RefCell::new(None));
    let (target, slot) = (view.clone(), node.clone());
    let handler = clock.connect_after_paint(move |_| {
        if slot.borrow().is_none() {
            let paintable = gtk::WidgetPaintable::new(Some(&target));
            let snapshot = gtk::Snapshot::new();
            let (w, h) = (f64::from(target.width()), f64::from(target.height()));
            paintable.snapshot(&snapshot, w, h);
            slot.replace(snapshot.to_node());
        }
    });
    view.queue_draw();
    common::pump_until(TIMEOUT, || node.borrow().is_some());
    clock.disconnect(handler);
    node.take().expect("a render node")
}

/// The view's pixels now (BGRA), rendered with the cairo renderer.
fn render(view: &PdfView) -> (i32, i32, Vec<u8>) {
    let (width, height) = (view.width(), view.height());
    let node = draw(view);
    let renderer = gsk::CairoRenderer::new();
    renderer.realize(None).expect("a cairo renderer");
    let viewport = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&viewport));
    renderer.unrealize();
    let stride = width as usize * 4;
    let mut pixels = vec![0; stride * height as usize];
    texture.download(&mut pixels, stride);
    (width, height, pixels)
}

fn lightness(pixels: &[u8], width: i32, x: i32, y: i32) -> f64 {
    let i = (y as usize * width as usize + x as usize) * 4;
    f64::from(u16::from(pixels[i]) + u16::from(pixels[i + 1]) + u16::from(pixels[i + 2])) / 3.0
}

/// Scrolls so that the boundary between `page - 1` and `page` is in the middle of the view.
fn straddle(view: &PdfView, page: u32) {
    let layout = view.layout().unwrap();
    let (_, y, _, _) = layout.page_rect(page, view.zoom(), f64::from(view.width()));
    view.set_scroll_position(0.0, y - f64::from(view.height()) / 2.0);
}

/// C1: at zoom × scale 2.75 (scale_q 22; HiDPI fit-width in an ~800 px pane is the same) an A4
/// page is 15.2 MB, so two visible pages plus the two prefetch pages exceed the 48 MiB cache.
/// When the four bitmaps arrive before the next frame, inserting the last one evicts the first,
/// a visible page, and nothing asks for it again: it stays blank until the next scroll.
fn prefetch_overflow_evicts_visible_page() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![A4; 50]);
    bench.serve(false, 0.5);
    let q = bench.zoom_to_scale(2.75);
    bench.serve(false, 0.5);
    straddle(view, 5);
    bench.serve(false, 0.5);
    let rendered = bench.rendered();
    println!(
        "  q {q}, visible {:?} rendered {rendered:?}, outstanding {}, cache {:.1}/{:.1} MiB",
        view.visible_pages(),
        view.outstanding_requests(),
        view.cache_used() as f64 / MIB,
        view.cache_cap() as f64 / MIB
    );
    assert_eq!(rendered.len(), 2, "two pages in view");
    assert!(
        rendered.iter().all(|&(_, done)| done),
        "a visible page has no bitmap and none is pending: {rendered:?}"
    );
}

/// C1: in the same situation on a live display (a frame after every bitmap keeps the visible
/// pages), the two prefetch pages keep evicting each other: every scroll by a few pixels, which
/// brings no new page into view, makes the PDF thread render a whole page again.
fn prefetch_overflow_rerenders_on_every_scroll() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![A4; 50]);
    bench.serve(true, 0.5);
    bench.zoom_to_scale(2.75);
    bench.serve(true, 0.5);
    straddle(view, 5);
    bench.serve(true, 0.5);
    view.reset_snapshot_time();
    draw(view);
    assert!(
        view.max_snapshot_time() > Duration::ZERO,
        "the forced frames reach the view"
    );
    let visible = view.visible_pages();
    let mut rerendered = 0;
    for step in 0..4 {
        let (x, y) = view.scroll_position();
        let dy = if step % 2 == 0 { 4.0 } else { -4.0 };
        view.set_scroll_position(x, y + dy);
        rerendered += bench.serve(true, 0.5);
        assert_eq!(view.visible_pages(), visible);
    }
    println!(
        "  4 scrolls of 4 px over pages {visible:?}: {rerendered} page renders, cache {:.1} MiB",
        view.cache_used() as f64 / MIB
    );
    assert_eq!(
        rerendered, 0,
        "scrolling within the same pages re-renders pages"
    );
}

/// C2: zoomed into the tiled range (scale 3.5: an A4 page is 24.6 MB, so it is rendered in
/// tiles), a rebuild must keep the old tiles on screen until the new ones arrive
/// (ARCHITECTURE §6 "Reload"). Only whole-page textures stand in, so the page turns blank.
fn rebuild_blanks_tiled_pages() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![A4; 20]);
    bench.serve(false, 0.5);
    let q = bench.zoom_to_scale(3.5);
    assert!(lemma_pdf::tile_grid(A4, q).is_some(), "tiled at q {q}");
    view.goto_page(12);
    // Black tiles, so the page's content is easy to tell from white paper.
    bench.serve(false, 0.0);
    assert!(view.is_page_rendered(12));
    let (w, h) = (view.width(), view.height());
    let (width, _, pixels) = render(view);
    let before = lightness(&pixels, width, w / 2, h / 2);

    // A rebuild: generation 2 of the same document; its tiles have not arrived yet.
    bench.open(2, vec![A4; 20]);
    common::pump_for(Duration::from_millis(100));
    let (width, _, pixels) = render(view);
    let after = lightness(&pixels, width, w / 2, h / 2);
    println!(
        "  q {q}: view centre lightness {before:.0} before the rebuild, {after:.0} right after; \
         cache {:.1} MiB",
        view.cache_used() as f64 / MIB
    );
    assert!(before < 64.0, "the tiles are drawn ({before})");
    assert!(
        after < 64.0,
        "the old tiles must stay on screen until the new ones arrive, but the page is blank \
         ({after})"
    );
}

/// C2 (zoom): zooming within the tiled range changes the render scale; until the tiles of the
/// new scale arrive, the tiles of the old one are stretched in their place rather than the page
/// turning blank (it has no whole-page texture to fall back on).
fn zoom_in_the_tiled_range_keeps_the_old_tiles() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![A4; 20]);
    bench.serve(false, 0.5);
    let q = bench.zoom_to_scale(3.5);
    view.goto_page(12);
    bench.serve(false, 0.0);
    assert!(view.is_page_rendered(12));
    let (w, h) = (view.width(), view.height());

    // Zoom in further; the new tiles are requested but not delivered.
    let new_q = bench.zoom_to_scale(4.0);
    assert!(lemma_pdf::tile_grid(A4, new_q).is_some() && new_q != q);
    common::pump_for(Duration::from_millis(100));
    assert!(!view.is_page_rendered(12), "no tiles of the new scale yet");
    let (width, _, pixels) = render(view);
    let centre = lightness(&pixels, width, w / 2, h / 2);
    println!("  q {q} -> {new_q}: view centre lightness {centre:.0} before the new tiles");
    assert!(
        centre < 64.0,
        "the old tiles must stand in until the new ones arrive ({centre})"
    );
}

/// C8: in fit-width mode, a rebuilt PDF whose widest page changed (here a landscape page was
/// added) gets a new zoom, but the render scale stays at the old value, so pages are rendered
/// too large or too small (blurry) for good; Ctrl+0 (fit width) does not fix it either.
fn reload_with_wider_page_keeps_stale_render_scale() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![LETTER; 3]);
    bench.serve(false, 0.5);
    let ds = view.device_scale();
    let first = (view.zoom(), view.render_scale_q());
    assert_eq!(first.1, scale_q(first.0, ds));

    bench.open(2, vec![LETTER, (792.0, 612.0), LETTER]);
    common::pump_for(Duration::from_millis(400));
    bench.serve(false, 0.5);
    let reloaded = (view.zoom(), view.render_scale_q());

    view.set_fit_width();
    common::pump_for(Duration::from_millis(400));
    bench.serve(false, 0.5);
    let fitted = (view.zoom(), view.render_scale_q());
    println!(
        "  zoom/scale_q: first {first:?}, after the rebuild {reloaded:?} (want q {}), after \
         Ctrl+0 {fitted:?} (want q {})",
        scale_q(reloaded.0, ds),
        scale_q(fitted.0, ds)
    );
    assert_ne!(first.0, reloaded.0, "the fit-width zoom changed");
    assert!(
        reloaded.1 == scale_q(reloaded.0, ds) && fitted.1 == scale_q(fitted.0, ds),
        "the render scale must follow the zoom after a rebuild (and after Ctrl+0)"
    );
}

// ----- The real window, controller and PDF thread -----

fn fixture(rel: &str) -> PathBuf {
    common::fixtures().join(rel).canonicalize().unwrap()
}

fn config() -> Config {
    let mut config = Config::default();
    config.build.on_save = false;
    config
}

fn window(dir: &Path) -> Rc<WindowState> {
    window_with(dir, config())
}

fn window_with(dir: &Path, config: Config) -> Rc<WindowState> {
    let workspace = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        None,
        workspace,
        false,
    );
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    state
}

fn view(state: &WindowState) -> PdfView {
    state.pdf.get().expect("a PDF controller").view().clone()
}

/// Waits until a PDF is shown and every visible page is drawn from a current bitmap.
fn wait_for_pages(state: &WindowState) {
    let view = view(state);
    common::pump_until(TIMEOUT, || {
        let visible = view.visible_pages();
        view.page_count() > 0
            && view.render_scale_q() > 0
            && !visible.is_empty()
            && visible.clone().all(|p| view.is_page_rendered(p))
    });
}

/// C6: building another root document in the same window (paper.tex, then response.tex) shows
/// its PDF at the previous document's page and manual zoom, as if it were a rebuild of the
/// same PDF. A different PDF must start at the top, fitted to the width (`set_document` docs).
fn other_pdf_keeps_the_previous_anchor() {
    let dir = common::temp_dir("regress-otherpdf");
    let state = window(&dir);
    let controller = state.pdf.get().unwrap();
    let view = view(&state);
    let first = fixture("pdf/stress.pdf");
    let second = fixture("proj/main.pdf");
    controller.open(&first);
    wait_for_pages(&state);
    view.set_zoom(1.3, None);
    view.goto_page(10);
    wait_for_pages(&state);

    controller.open(&second);
    common::pump_until(TIMEOUT, || {
        controller.pdf().as_deref() == Some(second.as_path())
    });
    common::pump_for(Duration::from_millis(100));
    let anchor = pdf_view::anchor_of(&view).unwrap();
    println!(
        "  {} after page 11 of {}: anchor {anchor:?}, scroll {:?}, zoom {:.2}, fit width {}",
        second.display(),
        first.display(),
        view.scroll_position(),
        view.zoom(),
        view.is_fit_width()
    );
    let at_top = anchor.page == 0 && view.scroll_position().1 <= 1.0;
    assert!(
        at_top && view.is_fit_width(),
        "another PDF opens at the top in fit-width mode"
    );
    state.window.destroy();
}

/// C7: a hidden PDF pane gives its textures back (ARCHITECTURE §7), but every rebuild while
/// it is hidden renders the visible and prefetch pages again for nobody, refilling the cache.
fn hidden_pane_renders_on_rebuild() {
    let dir = common::temp_dir("regress-hidden");
    let state = window(&dir);
    let controller = state.pdf.get().unwrap();
    let view = view(&state);
    let pdf = fixture("pdf/stress.pdf");
    controller.open(&pdf);
    wait_for_pages(&state);
    common::pump_for(Duration::from_millis(500));
    let shown = view.cache_used();

    state.pdf_pane.set_visible(false);
    let trimmed = view.cache_used();
    for _ in 0..2 {
        let before = controller.generation();
        controller.open(&pdf);
        common::pump_until(TIMEOUT, || controller.generation() > before);
        common::pump_for(Duration::from_millis(1500));
    }
    println!(
        "  cache {:.2} MiB shown, {:.2} MiB after hiding, {:.2} MiB after two rebuilds while \
         hidden (mapped {})",
        shown as f64 / MIB,
        trimmed as f64 / MIB,
        view.cache_used() as f64 / MIB,
        view.is_mapped()
    );
    assert!(
        view.cache_used() <= trimmed,
        "pages were rendered for the hidden pane"
    );
    state.window.destroy();
}

/// C12: hiding the pane (F8) while pages are being rendered trims the cache, but the PDF
/// thread keeps its queue and every bitmap that arrives afterwards is cached for the hidden
/// pane.
fn hiding_does_not_cancel_inflight_renders() {
    let dir = common::temp_dir("regress-inflight");
    let state = window(&dir);
    let controller = state.pdf.get().unwrap();
    let view = view(&state);
    controller.open(&fixture("pdf/stress.pdf"));
    wait_for_pages(&state);
    view.set_zoom(2.0 / view.device_scale(), None);
    let q = scale_q(view.zoom(), view.device_scale());
    common::pump_until(TIMEOUT, || view.render_scale_q() == q);
    wait_for_pages(&state);
    common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);

    // Scroll to pages that are not cached and hide the pane while they render.
    let layout = view.layout().unwrap();
    let (_, y, _, _) = layout.page_rect(15, view.zoom(), f64::from(view.width()));
    view.set_scroll_position(0.0, y - 10.0);
    common::pump_until(TIMEOUT, || view.outstanding_requests() > 0);
    state.pdf_pane.set_visible(false);
    let trimmed = view.cache_used();
    common::pump_for(Duration::from_secs(3));
    let (w, h) = page_pixels(LETTER, q);
    let page_bytes = w as usize * h as usize * 4;
    println!(
        "  q {q}: cache {:.1} MiB right after hiding, {:.1} MiB 3 s later (one page is {:.1} MiB)",
        trimmed as f64 / MIB,
        view.cache_used() as f64 / MIB,
        page_bytes as f64 / MIB
    );
    // The page being rendered when the pane was hidden may still arrive.
    assert!(
        view.cache_used() <= trimmed + page_bytes,
        "bitmaps rendered after hiding were cached"
    );
    state.window.destroy();
}

/// C1/C10: with `pdf.cache_mib` below the size of one page bitmap (here 4 MiB, and a 5.1 MB
/// page at scale 1.6), every bitmap is evicted as soon as it is inserted, so the page is never
/// shown.
fn cache_smaller_than_a_page_never_shows_it() {
    let dir = common::temp_dir("regress-smallcache");
    let mut config = config();
    config.pdf.cache_mib = 4;
    let state = window_with(&dir, config);
    let controller = state.pdf.get().unwrap();
    let view = view(&state);
    controller.open(&fixture("pdf/stress.pdf"));
    wait_for_pages(&state);
    view.set_zoom(1.6 / view.device_scale(), None);
    let q = scale_q(view.zoom(), view.device_scale());
    common::pump_until(TIMEOUT, || view.render_scale_q() == q);
    common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    common::pump_for(Duration::from_secs(1));
    let rendered: Vec<(u32, bool)> = view
        .visible_pages()
        .map(|p| (p, view.is_page_rendered(p)))
        .collect();
    println!(
        "  q {q}, visible {:?} rendered {rendered:?}, outstanding {}, cache {:.1}/{:.1} MiB",
        view.visible_pages(),
        view.outstanding_requests(),
        view.cache_used() as f64 / MIB,
        view.cache_cap() as f64 / MIB
    );
    assert!(
        !rendered.is_empty() && rendered.iter().all(|&(_, done)| done),
        "a visible page is never shown: {rendered:?}"
    );
    state.window.destroy();
}

/// C13: a page with an absurd MediaBox (2e7 bp; poppler accepts such a PDF) is tiled into
/// 4883 × 4883 tiles at the minimum zoom, and the view scans the whole grid on the main thread
/// for every request computation, arriving bitmap and `is_page_rendered`. (Driven through a
/// bare view: the scan is the view's, whatever page size the PDF thread reports.)
fn huge_mediabox_blocks_the_main_thread() {
    let bench = Bench::new(48 << 20);
    let view = &bench.view;
    bench.open(1, vec![(2e7, 2e7)]);
    let started = Instant::now();
    let _ = view.is_page_rendered(0);
    let elapsed = started.elapsed();
    println!(
        "  zoom {:.2}, q {}: one is_page_rendered took {elapsed:?}",
        view.zoom(),
        view.render_scale_q()
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "the main thread is blocked for {elapsed:?}"
    );
}
