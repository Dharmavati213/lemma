//! The PDF preview, builds and diagnostics (WP1.6 acceptance, ARCHITECTURE §6 and §10.7):
//! `--demo pdf`, scrolling cost and the cache cap, reload anchors (also when the page sizes of a
//! rebuild come after it showed), an unchanged PDF that is not reloaded, anchored zoom (zooming
//! out renders the new pages once, at the new scale; pages in bands or tiles are rendered whole
//! once they are one bitmap, and over several notches keep the scale of their first bands, which
//! are all drawn), large pages in full-width bands (the band below the view rendered ahead,
//! search highlights and dark mode over bands), Fit Page and 100%, handing the PDF to another
//! application, dark mode, save → build → diagnostics (and a build with nothing to do), stopping
//! a build, and SyncTeX in both directions (also after a memory trim); with a fake latexmk, the
//! build toasts, a latexmk that is not installed, and Clean Build Files and Rebuild.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_pdf`. The scrolling and
//! demo tests need `target/fixtures/pdf50.pdf` (`scripts/build-fixtures.sh`); the build tests
//! need latexmk, except those with the fake one. Tests whose prerequisites are missing print
//! `SKIP` and pass.

mod common;

use adw::prelude::*;
use gtk::{gdk, graphene};
use lemma_app::build_ui::{LATEXMK_MISSING, Reason};
use lemma_app::diagnostics_panel::{ERROR_CATEGORY, ERROR_TAG};
use lemma_app::pdf::pdf_view::{self, PdfView};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_build::{BuildStatus, PdfStamp};
use lemma_config::Config;
use lemma_core::{ProjectRoot, Severity};
use lemma_pdf::PageKey;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

fn main() -> ExitCode {
    common::run(&[
        ("demo_snapshot_shows_three_pages", demo_snapshot),
        ("scrolling_pdf50_is_fast_and_bounded", scrolling_pdf50),
        ("reload_keeps_the_scroll_anchor", reload_keeps_anchor),
        (
            "pages_resized_by_a_rebuild_keep_the_anchor",
            resized_pages_keep_the_anchor,
        ),
        ("an_unchanged_pdf_is_not_reloaded", unchanged_pdf),
        ("ctrl_wheel_zoom_is_anchored_at_the_pointer", anchored_zoom),
        (
            "zoom_out_renders_at_the_live_scale",
            zoom_out_renders_at_the_live_scale,
        ),
        (
            "zoom_out_of_tiles_renders_whole_pages",
            zoom_out_of_tiles_renders_whole_pages,
        ),
        (
            "zoom_out_in_notches_keeps_one_scale_of_bands",
            zoom_out_in_notches_keeps_one_scale_of_bands,
        ),
        (
            "large_pages_are_drawn_from_full_width_bands",
            large_pages_in_bands,
        ),
        ("fit_page_and_100_percent", fit_page_and_100_percent),
        ("the_pdf_is_handed_to_other_apps", pdf_handed_to_other_apps),
        ("dark_mode_toggle_inverts_the_pages", dark_toggle),
        (
            "forward_search_scrolls_to_and_flashes_the_box",
            forward_search,
        ),
        ("ctrl_click_opens_the_source_line", inverse_search),
        (
            "follow_cursor_searches_after_the_cursor_rests",
            follow_cursor,
        ),
        ("save_builds_and_shows_diagnostics", save_build_diagnostics),
        ("stop_cancels_a_running_build", stop_cancels),
        ("build_toasts_replace_each_other", build_toasts_replace),
        ("a_missing_latexmk_is_explained_once", missing_latexmk),
        ("clean_build_files_and_rebuild", clean_and_rebuild),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_TIMEOUT: Duration = Duration::from_secs(180);

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `target/fixtures/pdf50.pdf`, if `scripts/build-fixtures.sh` has run.
fn pdf50() -> Option<PathBuf> {
    let pdf = repo().join("target/fixtures/pdf50.pdf");
    if pdf.is_file() {
        Some(pdf.canonicalize().expect("canonical fixture path"))
    } else {
        println!(
            "  SKIP: {} is missing; run scripts/build-fixtures.sh",
            pdf.display()
        );
        None
    }
}

fn have_latexmk() -> bool {
    let found = Command::new("latexmk")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        println!("  SKIP: latexmk is not installed");
    }
    found
}

/// A copy of `tests/fixtures/proj` (sources, plus the built PDF and SyncTeX file if `with_pdf`).
fn copy_proj(name: &str, with_pdf: bool) -> PathBuf {
    let dir = common::temp_dir(name);
    let src = common::fixtures().join("proj");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    let mut files = vec!["main.tex", "refs.bib", "chapters/yoneda.tex"];
    if with_pdf {
        files.extend(["main.pdf", "main.synctex.gz"]);
    }
    for file in files {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    dir.canonicalize().unwrap()
}

fn config() -> Config {
    let mut config = Config::default();
    config.build.on_save = false;
    config
}

fn window(dir: &Path, config: Config) -> Rc<WindowState> {
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

fn open(state: &Rc<WindowState>, path: &Path) -> Rc<lemma_app::editor::Editor> {
    glib::MainContext::default()
        .block_on(state.open_file(path))
        .expect("open the document")
}

/// Activates a window action (`win.…`), as its accelerator or button does.
fn activate(state: &WindowState, action: &str) {
    WidgetExt::activate_action(&state.window, action, None).expect("the action exists");
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

/// Draws a real frame and returns `widget`'s render node from it. (A widget's cached node is
/// dropped whenever it queues a redraw, so it is taken right after the paint.)
fn next_frame_node(widget: &impl IsA<gtk::Widget>) -> gtk::gsk::RenderNode {
    let widget = widget.as_ref().clone();
    let clock = widget.frame_clock().expect("the widget is realized");
    let node = Rc::new(RefCell::new(None));
    let (target, slot) = (widget.clone(), node.clone());
    let handler = clock.connect_after_paint(move |_| {
        if slot.borrow().is_none() {
            let paintable = gtk::WidgetPaintable::new(Some(&target));
            let snapshot = gtk::Snapshot::new();
            let (w, h) = (f64::from(target.width()), f64::from(target.height()));
            paintable.snapshot(&snapshot, w, h);
            slot.replace(snapshot.to_node());
        }
    });
    widget.queue_draw();
    common::pump_until(TIMEOUT, || node.borrow().is_some());
    clock.disconnect(handler);
    node.take().expect("a render node")
}

/// Renders `widget`'s next frame with the cairo renderer; returns BGRA pixels.
fn render(widget: &impl IsA<gtk::Widget>) -> (i32, i32, Vec<u8>) {
    let (width, height) = (widget.as_ref().width(), widget.as_ref().height());
    let node = next_frame_node(widget);
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer.realize(None).expect("a cairo renderer");
    let viewport = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&viewport));
    renderer.unrealize();
    let stride = width as usize * 4;
    let mut pixels = vec![0; stride * height as usize];
    texture.download(&mut pixels, stride);
    (width, height, pixels)
}

/// With `LEMMA_TEST_SNAPSHOTS=dir`, saves the window's next frame as `dir/name.png` for review.
fn review_snapshot(state: &WindowState, name: &str) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let path = PathBuf::from(dir).join(format!("{name}.png"));
    // Two frames first, so transitions have finished (Broadway paints about once a second).
    let saved = glib::MainContext::default().block_on(async {
        lemma_app::testmode::wait_for_frames(&state.window, 2).await;
        lemma_app::testmode::snapshot_next_frame(&state.window, &path).await
    });
    match saved {
        Ok(_) => println!("  snapshot {}", path.display()),
        Err(e) => println!("  snapshot {name} failed: {e}"),
    }
}

/// Mean of the colour channels of a BGRA pixel.
fn lightness(pixels: &[u8], width: i32, x: i32, y: i32) -> f64 {
    let i = (y as usize * width as usize + x as usize) * 4;
    f64::from(u16::from(pixels[i]) + u16::from(pixels[i + 1]) + u16::from(pixels[i + 2])) / 3.0
}

/// Acceptance: `--demo pdf --snapshot` shows three pages.
fn demo_snapshot() {
    if pdf50().is_none() {
        return;
    }
    let dir = common::temp_dir("demo");
    let png = dir.join("demo.png");
    let output = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(repo().join("target/fixtures/pdf50.tex"))
        .args(["--demo", "pdf", "--quit-after-ms", "2500", "--snapshot"])
        .arg(&png)
        .env("LEMMA_CONFIG", "/dev/null")
        .output()
        .expect("run lemma");
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "lemma failed: {:?}", output.status);
    assert!(stdout.contains("showing pages 1–3"), "{stdout}");

    let texture = gdk::Texture::from_filename(&png).expect("the PNG loads");
    let (width, height) = (texture.width(), texture.height());
    let mut pixels = vec![0; width as usize * height as usize * 4];
    texture.download(&mut pixels, width as usize * 4);
    // Scan down the middle of the PDF pane (the right 45 % of the window) and count the runs
    // that differ from the backdrop beside the pages: each page is one long run.
    let x = (f64::from(width) * 0.775) as i32;
    let backdrop = lightness(&pixels, width, width - 40, height / 2);
    let mut pages = 0;
    let mut run = 0;
    for y in 100..height - 40 {
        if (lightness(&pixels, width, x, y) - backdrop).abs() > 8.0 {
            run += 1;
        } else {
            if run > 100 {
                pages += 1;
            }
            run = 0;
        }
    }
    if run > 100 {
        pages += 1;
    }
    println!("  {pages} pages in the snapshot (backdrop lightness {backdrop:.0})");
    assert_eq!(pages, 3);
}

/// Acceptance: scrolling pdf50 never takes 16 ms to draw a frame of the view, and the texture
/// cache stays within its cap (the cache also debug-asserts it on every insert).
fn scrolling_pdf50() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("scroll");
    let state = window(&dir, config());
    let controller = state.pdf.get().unwrap();
    controller.open(&pdf);
    wait_for_pages(&state);
    let view = view(&state);
    assert_eq!(view.page_count(), 50);
    let cap = view.cache_cap();
    assert_eq!(cap, 48 << 20);
    view.reset_snapshot_time();
    let content = view.layout().unwrap().content_size(view.zoom()).1;
    let step = f64::from(view.height()) * 0.8;
    let (mut y, mut steps, mut peak) = (0.0, 0, 0);
    while y < content {
        view.set_scroll_position(0.0, y);
        wait_for_pages(&state);
        // Real frames are slow offscreen (Broadway paints about once a second), so every third
        // position is drawn; the snapshot timing covers those frames.
        if steps % 3 == 0 {
            next_frame_node(&view);
        }
        peak = peak.max(view.cache_used());
        assert!(view.cache_used() <= cap, "the cache exceeds its cap");
        y += step;
        steps += 1;
    }
    let worst = view.max_snapshot_time();
    println!(
        "  {steps} scroll steps; slowest view snapshot {worst:?}; cache peak {:.1} MiB",
        peak as f64 / f64::from(1 << 20)
    );
    assert!(worst > Duration::ZERO, "the view was drawn");
    assert!(worst < Duration::from_millis(16), "a frame took {worst:?}");
    assert!(
        peak > 40 << 20,
        "scrolling 50 pages fills most of the cache"
    );

    // Hiding the pane gives back everything except what is on screen.
    state.pdf_pane.set_visible(false);
    let visible = view.visible_pages().len();
    assert!(
        view.cache_used() <= visible * (2 << 20),
        "{} bytes left",
        view.cache_used()
    );
    state.window.destroy();
}

/// Acceptance: after a rebuild (a new generation of the same PDF) the anchor is kept within
/// 1 px, also when zoomed; a shorter document clamps the anchor to its last page.
fn reload_keeps_anchor() {
    let Some(pdf50) = pdf50() else {
        return;
    };
    let dir = copy_proj("reload", true);
    // One PDF path throughout, rebuilt in place (another path is another document).
    let pdf = dir.join("preview.pdf");
    std::fs::copy(&pdf50, &pdf).unwrap();
    let state = window(&dir, config());
    let controller = state.pdf.get().unwrap();
    controller.open(&pdf);
    wait_for_pages(&state);
    let view = view(&state);

    let reload = |path: &Path| {
        let before = controller.generation().unwrap();
        controller.open(path);
        common::pump_until(TIMEOUT, || controller.generation() > Some(before));
        common::pump_for(Duration::from_millis(50));
    };
    for zoom in [None, Some(1.3)] {
        if let Some(zoom) = zoom {
            view.set_zoom(zoom, None);
        }
        view.set_scroll_position(17.0, 12_345.6);
        let before = view.scroll_position();
        let anchor = pdf_view::anchor_of(&view).unwrap();
        reload(&pdf);
        let after = view.scroll_position();
        println!(
            "  zoom {:.2}: {before:?} -> {after:?} ({anchor:?})",
            view.zoom()
        );
        assert!((after.1 - before.1).abs() <= 1.0, "{before:?} -> {after:?}");
        assert!((after.0 - before.0).abs() <= 1.0, "{before:?} -> {after:?}");
        assert_eq!(pdf_view::anchor_of(&view).unwrap().page, anchor.page);
        wait_for_pages(&state);
    }

    // Page 30 of 50 becomes the last page of a 5-page document.
    view.goto_page(30);
    std::fs::copy(dir.join("main.pdf"), &pdf).unwrap();
    reload(&pdf);
    assert_eq!(view.page_count(), 5);
    let anchor = pdf_view::anchor_of(&view).unwrap();
    assert_eq!(anchor.page, 4);
    wait_for_pages(&state);
    state.window.destroy();
}

/// A rebuild with as many pages shows at once with the page sizes of the generation before;
/// once the PDF thread has read the new ones, the view lays them out keeping its anchor (a page
/// above it that grew moves nothing in view), and the strip's thumbnail rows take them.
fn resized_pages_keep_the_anchor() {
    let dir = common::temp_dir("resized");
    let pdf = dir.join("doc.pdf");
    let mut sizes = vec![(595.0, 842.0); 30];
    common::write_pdf(&pdf, &sizes, &[]);
    let state = window(&dir, config());
    state.pdf_sidebar.split.set_show_sidebar(true);
    let controller = state.pdf.get().unwrap();
    controller.open(&pdf);
    wait_for_pages(&state);
    let view = view(&state);
    view.goto_page(12);
    wait_for_pages(&state);
    let (x, y) = view.scroll_position();
    view.set_scroll_position(x, y + 123.0);
    common::pump_until(TIMEOUT, || {
        lemma_app::pdf::sidebar::thumbnail_page_sizes(&state)
            .iter()
            .any(|(page, _)| *page == 13)
    });
    let anchor = pdf_view::anchor_of(&view).unwrap();
    // The page above the anchor's grows, the one below it shrinks.
    sizes[11] = (595.0, 1400.0);
    sizes[13] = (420.0, 600.0);
    common::write_pdf(&pdf, &sizes, &[]);
    let generation = controller.generation();
    let updates = view.page_size_updates();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || {
        controller.generation() > generation
            && view.layout().is_some_and(|layout| layout.sizes() == sizes)
    });
    assert!(
        view.page_size_updates() > updates,
        "the rebuild showed with the old sizes, which the new ones replaced"
    );
    let after = pdf_view::anchor_of(&view).unwrap();
    println!("  {anchor:?} -> {after:?}");
    assert_eq!(after.page, anchor.page);
    assert!(
        (after.frac - anchor.frac).abs() < 1e-6,
        "{anchor:?} -> {after:?}"
    );
    common::pump_until(TIMEOUT, || {
        lemma_app::pdf::sidebar::thumbnail_page_sizes(&state)
            .iter()
            .all(|(page, size)| *size == sizes[*page as usize])
    });
    // The pages are drawn at their new sizes.
    view.goto_page(13);
    wait_for_pages(&state);
    let rect = pdf_view::view_point_of(&view, 13, 420.0, 600.0)
        .zip(pdf_view::view_point_of(&view, 13, 0.0, 0.0))
        .map(|((x1, y1), (x0, y0))| (x1 - x0, y1 - y0))
        .unwrap();
    assert!(
        (rect.0 / rect.1 - 420.0 / 600.0).abs() < 1e-3,
        "page 14 laid out at {rect:?}"
    );
    state.window.destroy();
}

/// A build that wrote no new PDF reports the stamp of the PDF shown (size and modification
/// time): `open_if_changed` opens nothing, so no page is rendered again. A PDF written since,
/// or one without a stamp, is opened as a new generation.
fn unchanged_pdf() {
    let dir = copy_proj("unchanged", true);
    let pdf = dir.join("main.pdf");
    let state = window(&dir, config());
    let controller = state.pdf.get().unwrap();
    let stamp = PdfStamp::of(&pdf);
    assert!(stamp.is_some(), "a stamp of {}", pdf.display());
    assert!(controller.open_if_changed(&pdf, stamp));
    wait_for_pages(&state);
    let view = view(&state);
    let shown = controller.generation();

    assert!(!controller.open_if_changed(&pdf, stamp), "the same stamp");
    common::pump_for(Duration::from_millis(300));
    assert_eq!(controller.generation(), shown);
    assert_eq!(view.generation(), shown);
    assert!(controller.shows_requested());

    std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf).unwrap();
    let rewritten = PdfStamp::of(&pdf);
    assert!(rewritten.is_some_and(|r| Some(r) != stamp), "{rewritten:?}");
    assert!(controller.open_if_changed(&pdf, rewritten));
    common::pump_until(TIMEOUT, || controller.generation() > shown);
    let shown = controller.generation();
    assert!(controller.open_if_changed(&pdf, None), "no stamp");
    common::pump_until(TIMEOUT, || controller.generation() > shown);
    state.window.destroy();
}

/// Acceptance: Ctrl+wheel zoom keeps the point under the pointer; textures are re-rendered at
/// the new scale after the delay; the log names the surface scale.
fn anchored_zoom() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("zoom");
    let state = window(&dir, config());
    state.pdf.get().unwrap().open(&pdf);
    wait_for_pages(&state);
    let view = view(&state);
    assert!(view.is_fit_width());
    assert!(view.device_scale() >= 1.0);
    view.goto_page(10);
    wait_for_pages(&state);
    let pointer = (90.0, 200.0);
    let before = pdf_view::page_point_at(&view, pointer.0, pointer.1).expect("on a page");
    let old_q = view.render_scale_q();
    // Three wheel notches with Ctrl held (what the scroll controller does).
    view.zoom_by(1.1f64.powi(3), Some(pointer));
    let after = pdf_view::page_point_at(&view, pointer.0, pointer.1).expect("on a page");
    println!("  {before:?} -> {after:?} at zoom {:.3}", view.zoom());
    assert_eq!(before.0, after.0);
    assert!((before.1 - after.1).abs() * view.zoom() <= 1.0);
    assert!((before.2 - after.2).abs() * view.zoom() <= 1.0);
    assert!(!view.is_fit_width());
    // The old textures are stretched first; the new scale is requested after the delay.
    assert_eq!(view.render_scale_q(), old_q);
    let wanted = lemma_pdf::scale_q(view.zoom(), view.device_scale());
    common::pump_until(TIMEOUT, || view.render_scale_q() == wanted);
    wait_for_pages(&state);

    // Fit width brings the zoom back.
    activate(&state, "win.pdf-fit-width");
    assert!(view.is_fit_width());
    state.window.destroy();
}

/// Fit Page (`win.pdf-fit-page`) shows the page shown whole at the top of the view and keeps it
/// whole when the view is resized; 100% (`win.pdf-zoom-reset`, Ctrl+1 in the view) is a zoom of
/// one bp per pixel that a resize leaves alone; Fit Width comes back.
fn fit_page_and_100_percent() {
    let dir = copy_proj("fit-page", true);
    let state = window(&dir, config());
    state.pdf.get().unwrap().open(&dir.join("main.pdf"));
    wait_for_pages(&state);
    let view = view(&state);
    // A wide pane, so that the page's height decides.
    state.paned.set_position(380);
    common::pump_until(TIMEOUT, || view.width() > 600);
    wait_for_pages(&state);
    let layout = view.layout().unwrap();
    let (_, page_height) = view.page_size(2).unwrap();
    view.set_scroll_position(0.0, layout.page_rect(2, view.zoom(), 0.0).1 + 200.0);
    assert_eq!(view.current_page(), 2);

    let whole = |view: &PdfView| {
        let (_, top) = view.widget_point(2, 0.0, 0.0).unwrap();
        let (_, bottom) = view.widget_point(2, 0.0, page_height).unwrap();
        let height = f64::from(view.height());
        println!(
            "  zoom {:.3} in {}x{}: page 3 from {top:.1} to {bottom:.1}",
            view.zoom(),
            view.width(),
            height
        );
        // On screen whole, and as tall as the view less a gap above and below it.
        top >= 0.0
            && bottom <= height
            && (bottom - top + 2.0 * pdf_view::PAGE_GAP - height).abs() <= 1.0
    };
    activate(&state, "win.pdf-fit-page");
    assert_eq!(view.fit(), pdf_view::Fit::Page);
    assert!(view.zoom() < layout.fit_width_zoom(f64::from(view.width())));
    assert!(whole(&view), "page 3 fills the view");
    let (_, top) = view.widget_point(2, 0.0, 0.0).unwrap();
    assert!(
        (top - pdf_view::PAGE_GAP).abs() <= 1.0,
        "page 3 at the top: {top}"
    );
    wait_for_pages(&state);

    // Shorter (as when the search bar opens above it): the page is fitted again, and stays in
    // place. (Not the search bar itself: after it, gtk4-broadwayd 4.24 aborted in about half
    // the runs of this binary, "decode_nodes: assertion failed: (node != NULL)".)
    let height = view.height();
    view.set_margin_bottom(60);
    common::pump_until(TIMEOUT, || view.height() < height - 20);
    common::pump_until(TIMEOUT, || whole(&view));
    assert_eq!(view.fit(), pdf_view::Fit::Page);
    assert_eq!(view.current_page(), 2);
    wait_for_pages(&state);

    activate(&state, "win.pdf-zoom-reset");
    assert_eq!(view.zoom(), 1.0);
    assert_eq!(view.fit(), pdf_view::Fit::None);
    let width = view.width();
    state.paned.set_position(300);
    common::pump_until(TIMEOUT, || view.width() > width + 50);
    assert_eq!(view.zoom(), 1.0, "a resize keeps 100%");
    let wanted = lemma_pdf::scale_q(1.0, view.device_scale());
    common::pump_until(TIMEOUT, || view.render_scale_q() == wanted);
    wait_for_pages(&state);

    activate(&state, "win.pdf-fit-width");
    assert!(view.is_fit_width());
    let width = f64::from(view.width());
    assert!((view.zoom() - layout.fit_width_zoom(width)).abs() < 1e-9);
    state.window.destroy();
}

/// The labels of the toasts `state` shows.
fn toast_titles(state: &WindowState) -> Vec<String> {
    fn labels(widget: &gtk::Widget, out: &mut Vec<String>) {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            out.push(label.label().to_string());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            labels(&c, out);
            child = c.next_sibling();
        }
    }
    let mut out = Vec::new();
    let mut child = state.toasts.first_child();
    while let Some(c) = child {
        if c.type_().name() == "AdwToastWidget" {
            labels(&c, &mut out);
        }
        child = c.next_sibling();
    }
    out
}

/// Open PDF in Another App and Show PDF in Files hand the PDF shown to the system (here to the
/// test's hook): unavailable, with the palette's reason, until a PDF is shown; a failure is a
/// toast; a scripted run starts nothing.
fn pdf_handed_to_other_apps() {
    use lemma_app::pdf::pdf_controller::Handoff;
    let dir = copy_proj("handoff", true);
    let pdf = dir.join("main.pdf");
    let state = window(&dir, config());
    let controller = state.pdf.get().unwrap();
    let handed = Rc::new(RefCell::new(Vec::new()));
    let works = Rc::new(Cell::new(true));
    let (log, ok) = (handed.clone(), works.clone());
    controller.set_launch_hook(Some(Box::new(move |handoff, pdf| {
        log.borrow_mut().push((handoff, pdf.to_path_buf()));
        ok.get()
    })));
    for name in ["pdf-open-external", "pdf-show-in-folder"] {
        assert!(
            !state.window.is_action_enabled(name),
            "{name} without a PDF"
        );
        let command = lemma_app::palette::commands(&state)
            .into_iter()
            .find(|c| c.name == format!("win.{name}"))
            .expect("in the palette");
        assert!(!command.enabled);
        assert_eq!(command.reason.as_deref(), Some("Build the document first"));
    }

    controller.open(&pdf);
    wait_for_pages(&state);
    assert!(state.window.is_action_enabled("pdf-open-external"));
    activate(&state, "win.pdf-open-external");
    activate(&state, "win.pdf-show-in-folder");
    assert_eq!(
        *handed.borrow(),
        [
            (Handoff::Open, pdf.clone()),
            (Handoff::ShowInFolder, pdf.clone())
        ]
    );
    works.set(false);
    activate(&state, "win.pdf-open-external");
    common::pump_until(TIMEOUT, || {
        toast_titles(&state).contains(&"Could not open the PDF".to_owned())
    });
    state.window.destroy();

    let scripted = WindowState::new(
        &common::application(),
        Rc::new(config()),
        None,
        Workspace::new(ProjectRoot::new(&dir).unwrap()),
        true,
    );
    scripted.window.present();
    common::pump_until(TIMEOUT, || scripted.window.is_mapped());
    scripted.pdf.get().unwrap().open(&pdf);
    wait_for_pages(&scripted);
    activate(&scripted, "win.pdf-show-in-folder");
    common::pump_until(TIMEOUT, || {
        toast_titles(&scripted)
            .contains(&"Other applications are not started from scripted runs".to_owned())
    });
    scripted.window.destroy();
}

/// Zooming out brings pages into view that have no texture to stretch: they are requested at
/// once at the new zoom's scale, not at the old one (whose bitmaps the re-render after
/// `RERENDER_DELAY` would throw away). Zooming out further before the re-render, such a page
/// waits for it with the bitmap it has. So each page is rendered once at the final scale (and
/// at most once before it), and the cache stays within its cap.
fn zoom_out_renders_at_the_live_scale() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("zoom-out");
    let state = window(&dir, config());
    state.pdf.get().unwrap().open(&pdf);
    wait_for_pages(&state);
    let view = view(&state);
    view.goto_page(20);
    wait_for_pages(&state);
    common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    let old_q = view.render_scale_q();
    let received = view.bitmaps_received();
    let centre = (
        f64::from(view.width()) / 2.0,
        f64::from(view.height()) / 2.0,
    );
    let fitted = view.zoom();
    // Out sixfold in two steps, the second once the first's bitmaps are there. The pages asked
    // for in the first are rendered again at the final scale.
    let mut early = 0;
    let mut live = old_q;
    for (step, factor) in [2.5, 6.0].into_iter().enumerate() {
        view.set_zoom(fitted / factor, Some(centre));
        live = lemma_pdf::scale_q(view.zoom(), view.device_scale());
        assert_ne!(live, old_q);
        // The requests of the update that follows, long before the re-render's delay is over.
        let started = Instant::now();
        common::pump_until(TIMEOUT, || view.outstanding_requests() > 0);
        let asked = started.elapsed();
        let keys = view.outstanding_keys();
        let scales: Vec<(u32, u16)> = keys.iter().map(|k| (k.page, k.scale_q)).collect();
        println!("  out {factor}x, scale {old_q} -> {live}: asked after {asked:?} for {scales:?}");
        assert!(asked < pdf_view::RERENDER_DELAY, "{asked:?}");
        assert!(keys.iter().all(|k| k.scale_q == live), "{scales:?}");
        if step == 0 {
            early = keys.len();
        }
        // Their bitmaps, while the zoom still moves (as in a pinch, which keeps the re-render
        // waiting).
        while view.outstanding_requests() > 0 {
            view.set_zoom(view.zoom() * (1.0 + 1e-6), Some(centre));
            common::pump_for(Duration::from_millis(20));
        }
        assert_eq!(view.render_scale_q(), old_q, "the re-render waits");
    }
    common::pump_until(TIMEOUT, || view.render_scale_q() == live);
    wait_for_pages(&state);
    common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    let renders = view.bitmaps_received() - received;
    let pages = view.visible_pages().len() + 2;
    println!(
        "  {renders} renders for {pages} pages ({early} also at the first step's scale); cache \
         {:.1} of {} MiB",
        view.cache_used() as f64 / f64::from(1 << 20),
        view.cache_cap() >> 20
    );
    assert!(
        renders <= pages + early,
        "{renders} renders for {pages} pages"
    );
    assert!(view.cache_used() <= view.cache_cap());
    state.window.destroy();
}

/// Shows `tests/fixtures/pdf/stress.pdf` (23 Letter pages, in bands from scale_q 24 on) in light
/// mode.
fn show_stress(state: &WindowState) -> PdfView {
    let stress = common::fixtures().join("pdf/stress.pdf");
    state
        .pdf
        .get()
        .unwrap()
        .open(&stress.canonicalize().unwrap());
    wait_for_pages(state);
    let view = view(state);
    if view.is_dark() {
        activate(state, "win.pdf-invert");
    }
    view
}

/// Runs the main loop until `done`, adding the render requests outstanding after each iteration
/// to `asked`. (A bitmap arrives an iteration after its request at the earliest, so every request
/// is seen, also those rendered at once.)
fn pump_asking(view: &PdfView, asked: &mut HashSet<PageKey>, mut done: impl FnMut() -> bool) {
    common::pump_until(TIMEOUT, || {
        let done = done();
        if !done {
            asked.extend(view.outstanding_keys());
        }
        done
    });
}

/// The fraction of the rows of the 0-based `page` in view that the textures `keys` of it cover
/// (whole pages, or full-width bands).
fn rows_covered(view: &PdfView, page: u32, keys: &[PageKey]) -> f64 {
    let size = view.page_size(page).unwrap();
    let layout = view.layout().unwrap();
    let (_, top, width, height) = layout.page_rect(page, view.zoom(), f64::from(view.width()));
    let (_, sy) = view.scroll_position();
    let shown = (
        top.max(sy),
        (top + height).min(sy + f64::from(view.height())),
    );
    if shown.1 <= shown.0 {
        return 1.0;
    }
    let mut spans: Vec<(f64, f64)> = keys
        .iter()
        .map(|key| match key.tile {
            None => (top, top + height),
            Some(tile) => {
                let (_, y, _, h) = lemma_pdf::tile_rect(size, key.scale_q, tile);
                // Logical pixels per pixel of the bitmap.
                let k = width / size.0 / lemma_pdf::scale_of(key.scale_q);
                (top + f64::from(y) * k, top + f64::from(y + h) * k)
            }
        })
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut covered, mut reached) = (0.0, shown.0);
    for (from, to) in spans {
        let (from, to) = (from.max(reached), to.min(shown.1));
        if to > from {
            covered += to - from;
            reached = to;
        }
    }
    covered / (shown.1 - shown.0)
}

/// Zooming out of pages in tiles (stress.pdf) to a zoom at which they are one bitmap, with the
/// bottom of the view in the gap above a page whose first band or row of tiles was rendered ahead
/// at the old scale: no bitmap is asked for at the old scale or in tiles before the re-render, and
/// no page twice. Ctrl+minus from zoom × scale 3.25 (bands, as in `large_pages_in_bands`) shows
/// no more of the page below than that band, so it is drawn from it until the re-render, and the
/// band after it is not rendered ahead at the old scale meanwhile. Ctrl+1 from zoom × scale 7
/// (tiles of about 1024²) shows more of it, and of the page above, than their tiles cover: both
/// are rendered whole at the new scale at once, which the re-render keeps (here they were asked
/// for in 18 tiles at the old scale, 65 MiB, then whole).
fn zoom_out_of_tiles_renders_whole_pages() {
    let dir = common::temp_dir("zoom-out-tiles");
    let state = window(&dir, config());
    let view = show_stress(&state);
    let ds = view.device_scale();
    let settle = || {
        wait_for_pages(&state);
        common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    };
    // At `zoom`, the bottom of the view in the gap under the 0-based `page`, so the first row of
    // the page after it is rendered ahead.
    let bottom_in_the_gap_under = |page: u32, zoom: f64| {
        view.set_zoom(zoom, None);
        let q = lemma_pdf::scale_q(view.zoom(), ds);
        common::pump_until(TIMEOUT, || view.render_scale_q() == q);
        let layout = view.layout().unwrap();
        let (_, top, _, page_height) = layout.page_rect(page, view.zoom(), f64::from(view.width()));
        let height = f64::from(view.height());
        view.set_scroll_position(0.0, top + page_height + pdf_view::PAGE_GAP / 2.0 - height);
        settle();
        let ahead: Vec<_> = view
            .cached_keys(page + 1)
            .iter()
            .map(|k| (k.scale_q, k.tile))
            .collect();
        println!("  at scale {q}, the page below the view has {ahead:?}");
        assert!(
            ahead
                .iter()
                .any(|&(scale, tile)| scale == q && tile.is_some_and(|(_, row)| row == 0)),
            "its first row ahead"
        );
    };
    let zoom_out = |what: &str, zoom: &dyn Fn(), below: u32| {
        let old = view.render_scale_q();
        let received = view.bitmaps_received();
        zoom();
        let live = lemma_pdf::scale_q(view.zoom(), ds);
        let visible = view.visible_pages();
        assert!(visible.contains(&below));
        assert!(visible.clone().all(|p| {
            let size = view.page_size(p).unwrap();
            lemma_pdf::tile_grid(size, old).is_some() && lemma_pdf::tile_grid(size, live).is_none()
        }));
        // Whether its tiles at the old scale cover what is in view of the page below.
        let covered = view.is_page_rendered(below);
        // Every request until the re-render, then those of the re-render.
        let mut early = HashSet::new();
        pump_asking(&view, &mut early, || view.render_scale_q() != old);
        assert_eq!(view.render_scale_q(), live);
        let mut late = HashSet::new();
        pump_asking(&view, &mut late, || {
            view.visible_pages().all(|p| view.is_page_rendered(p))
                && view.outstanding_requests() == 0
        });
        let sorted = |keys: &HashSet<PageKey>| {
            let mut keys: Vec<_> = keys.iter().map(|k| (k.page, k.scale_q, k.tile)).collect();
            keys.sort_unstable();
            keys
        };
        let renders = view.bitmaps_received() - received;
        println!(
            "  {what}, scale {old} -> {live}, pages {visible:?} (the one below covered: \
             {covered}): asked for {:?}, then {:?}; {renders} renders, cache {:.1} of {} MiB",
            sorted(&early),
            sorted(&late),
            view.cache_used() as f64 / f64::from(1 << 20),
            view.cache_cap() >> 20
        );
        assert!(
            early.iter().all(|k| k.scale_q == live && k.tile.is_none()),
            "asked for at the old scale or in tiles: {:?}",
            sorted(&early)
        );
        // Each page once, whole.
        let asked: HashSet<PageKey> = early.union(&late).copied().collect();
        let pages: HashSet<u32> = asked.iter().map(|k| k.page).collect();
        assert_eq!(pages.len(), asked.len(), "a page asked for twice");
        assert!(renders <= asked.len(), "{renders} renders");
        assert!(view.cache_used() <= view.cache_cap());
        covered
    };
    bottom_in_the_gap_under(7, 3.25 / ds);
    assert!(
        zoom_out("Ctrl+minus", &|| view.zoom_out(), 8),
        "the page below shows only its first band"
    );
    bottom_in_the_gap_under(11, 7.0 / ds);
    let far = view.render_scale_q();
    assert_eq!(
        lemma_pdf::tile_size(view.page_size(12).unwrap(), far).map(|(w, _)| w < 1100),
        Some(true),
        "tiles of about 1024²"
    );
    assert!(
        !zoom_out("Ctrl+1", &|| view.reset_zoom(), 12),
        "the page below shows more than its first row"
    );
    state.window.destroy();
}

/// Zooming out in notches (Ctrl+wheel, a pinch) at zooms at which stress.pdf's pages are all in
/// bands, anchored at the bottom of the view: the page that comes into view from above, with
/// nothing to stand in for it, gets its bands at the first notch's scale and keeps that scale
/// until the re-render, so each band is rendered once before it, and they are all drawn. (Each
/// notch used to ask for the page again at its own scale, and only the bands of the scale nearest
/// the old one were drawn, so the rest of the page was blank while bands that covered it were
/// cached.) Notches 3 ms apart, faster than a band renders, can leave bands of several scales:
/// the page then grows the group that is drawn (it grew the one nearest the zoom's scale, which
/// was not), so once the bitmaps asked for are there, all it has cached is drawn. The re-render
/// then draws everything from bands at the final scale.
fn zoom_out_in_notches_keeps_one_scale_of_bands() {
    let dir = common::temp_dir("zoom-notches");
    let state = window(&dir, config());
    let view = show_stress(&state);
    let ds = view.device_scale();
    let settle = || {
        wait_for_pages(&state);
        common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    };
    let for_a_while = |duration: Duration| {
        let started = Instant::now();
        move || started.elapsed() >= duration
    };
    let scales = |keys: &[PageKey]| {
        let mut scales: Vec<u16> = keys.iter().map(|k| k.scale_q).collect();
        scales.sort_unstable();
        scales.dedup();
        scales
    };
    // Seven notches with the top of the view at the top of the page after the 0-based `page`,
    // which has no texture: `quick`, each 3 ms after the last, else once its bitmaps are there.
    let notches = |page: u32, quick: bool| {
        view.set_zoom(6.0 / ds, None);
        let q = lemma_pdf::scale_q(view.zoom(), ds);
        common::pump_until(TIMEOUT, || view.render_scale_q() == q);
        let layout = view.layout().unwrap();
        let (width, height) = (f64::from(view.width()), f64::from(view.height()));
        view.set_scroll_position(0.0, layout.page_rect(page + 1, view.zoom(), width).1);
        settle();
        assert!(view.cached_keys(page).is_empty());
        let size = view.page_size(page).unwrap();
        let received = view.bitmaps_received();
        let bottom = Some((width / 2.0, height));
        let mut requested = HashSet::new();
        // The requests until the bitmaps asked for are there, while the zoom still moves (as in
        // a pinch, which keeps the re-render waiting).
        let finish = |requested: &mut HashSet<PageKey>| {
            while view.outstanding_requests() > 0 {
                view.set_zoom(view.zoom() * (1.0 + 1e-6), bottom);
                pump_asking(&view, requested, for_a_while(Duration::from_millis(20)));
            }
            assert_eq!(view.render_scale_q(), q, "the re-render waits");
        };
        let check = |what: &str| {
            let (generation, dark) = (view.generation(), view.is_dark());
            let cached: Vec<_> = view
                .cached_keys(page)
                .into_iter()
                .filter(|k| Some(k.generation) == generation && k.dark == dark)
                .collect();
            let drawn = view.drawn_keys(page);
            let (covered, available) = (
                rows_covered(&view, page, &drawn),
                rows_covered(&view, page, &cached),
            );
            println!(
                "  {what}: page {} drawn from {:?} covering {:.0}% of it in view; cached {:?} \
                 covering {:.0}%",
                page + 1,
                scales(&drawn),
                covered * 100.0,
                scales(&cached),
                available * 100.0
            );
            assert!(covered >= available - 1e-9, "bands cached but not drawn");
        };
        let mut live = q;
        for notch in 1..=7 {
            view.zoom_by(1.0 / 1.1, bottom);
            live = lemma_pdf::scale_q(view.zoom(), ds);
            assert!(
                lemma_pdf::tile_grid(size, live).is_some(),
                "in bands at {live}"
            );
            if quick {
                pump_asking(&view, &mut requested, for_a_while(Duration::from_millis(3)));
            } else {
                pump_asking(
                    &view,
                    &mut requested,
                    for_a_while(Duration::from_millis(10)),
                );
                finish(&mut requested);
                check(&format!("notch {notch} at scale {live}"));
            }
        }
        if quick {
            finish(&mut requested);
            check(&format!("quick notches to scale {live}"));
        }
        let asked: Vec<_> = requested
            .iter()
            .filter(|k| k.page == page)
            .map(|k| (k.scale_q, k.tile))
            .collect();
        let renders = view.bitmaps_received() - received;
        println!(
            "  page {} asked for {asked:?}; {renders} renders of {} bitmaps asked for",
            page + 1,
            requested.len()
        );
        assert!(!asked.is_empty());
        assert!(
            quick || asked.iter().all(|&(scale, _)| scale == asked[0].0),
            "page {} asked for at several scales: {asked:?}",
            page + 1
        );
        assert!(renders <= requested.len(), "a bitmap rendered twice");
        // The re-render: everything at the final scale.
        common::pump_until(TIMEOUT, || view.render_scale_q() == live);
        settle();
        let drawn = view.drawn_keys(page);
        assert!(
            !drawn.is_empty() && drawn.iter().all(|k| k.scale_q == live),
            "{drawn:?}"
        );
        assert!(view.cache_used() <= view.cache_cap());
    };
    notches(6, false);
    notches(14, true);
    state.window.destroy();
}

/// Pages too large for one bitmap (stress.pdf at zoom × scale 3.25 is 1989 × 2574 px, as a
/// fit-width page on a HiDPI pane about 1000 px wide) are drawn from full-width bands: the ones
/// in view, then the one that scrolling down shows next, so that a scroll by a band finds it
/// rendered; once a page's last band is in view, that is the next page's first. Search
/// highlights are drawn across bands, and the bands meet without a seam, in light and dark mode.
fn large_pages_in_bands() {
    let dir = common::temp_dir("bands");
    let state = window(&dir, config());
    let view = show_stress(&state);
    let ds = view.device_scale();
    view.set_zoom(3.25 / ds, None);
    let q = lemma_pdf::scale_q(view.zoom(), ds);
    common::pump_until(TIMEOUT, || view.render_scale_q() == q);
    let size = view.page_size(5).unwrap();
    let (columns, rows) = lemma_pdf::tile_grid(size, q).expect("tiled at zoom 3.25");
    let (_, band_px) = lemma_pdf::tile_size(size, q).unwrap();
    assert_eq!(columns, 1, "full-width bands");
    // A band's height in logical pixels.
    let band = f64::from(band_px) / ds;
    let height = f64::from(view.height());
    assert!(
        height + band < band * f64::from(rows),
        "the view shows part of a page"
    );
    let layout = view.layout().unwrap();
    let rect = |page| layout.page_rect(page, view.zoom(), f64::from(view.width()));
    let settle = || {
        wait_for_pages(&state);
        common::pump_until(TIMEOUT, || view.outstanding_requests() == 0);
    };
    // Scrolled down by one band from `y`, the view is complete at once (before the main loop
    // runs again, so before any bitmap can arrive).
    let scroll_by_a_band_from = |y: f64| {
        view.set_scroll_position(0.0, y);
        settle();
        let received = view.bitmaps_received();
        view.set_scroll_position(0.0, y + band);
        let complete = view.visible_pages().all(|p| view.is_page_rendered(p));
        println!(
            "  from {y:.0} down by a band of {band:.0} px: pages {:?} complete {complete} \
             ({received} bitmaps so far, cache {:.1} MiB)",
            view.visible_pages(),
            view.cache_used() as f64 / f64::from(1 << 20)
        );
        assert!(complete, "the band below the view was not rendered ahead");
        assert!(view.cache_used() <= view.cache_cap());
    };
    // Within a page: the next band of the same page.
    let (_, top, _, page_height) = rect(5);
    scroll_by_a_band_from(top + band / 2.0);
    assert_eq!(view.visible_pages(), 5..6);
    // Its last band in view (the bottom of the view in the gap below it): the next page's first.
    scroll_by_a_band_from(top + page_height + pdf_view::PAGE_GAP / 2.0 - height);
    assert!(view.visible_pages().contains(&6));

    // A match across the boundary of the first two bands (in the middle of the view), in the
    // left margin.
    view.set_scroll_position(0.0, top + band - height / 2.0);
    settle();
    let boundary_bp = f64::from(band_px) / lemma_pdf::scale_of(q);
    let found = (20.0, boundary_bp - 10.0, 40.0, 20.0);
    view.set_search_matches(5, vec![vec![found]]);
    view.set_current_match(Some((5, 0)));
    let check = |dark: bool| {
        let (width, _, pixels) = render(&view);
        let at = |x_bp: f64, y_bp: f64| {
            let (x, y) = view.widget_point(5, x_bp, y_bp).unwrap();
            let i = (y.floor() as usize * width as usize + x.floor() as usize) * 4;
            // BGRA.
            let p = &pixels[i..i + 4];
            (f64::from(p[2]), f64::from(p[1]), f64::from(p[0]))
        };
        // Orange at 0.42 over grey changes red − green by +57 and green − blue by +50.
        for y_bp in [boundary_bp - 5.0, boundary_bp + 5.0] {
            let (r, g, b) = at(40.0, y_bp);
            println!("  dark {dark}: the match at {y_bp:.1} bp is {r:.0},{g:.0},{b:.0}");
            assert!((45.0..70.0).contains(&(r - g)) && (38.0..62.0).contains(&(g - b)));
        }
        // The paper on either side of the boundary, and on it.
        let (x, boundary) = view.widget_point(5, 10.0, boundary_bp).unwrap();
        let paper: Vec<f64> = [-1.0, 0.0, 1.0]
            .iter()
            .map(|dy| lightness(&pixels, width, x as i32, (boundary + dy).floor() as i32))
            .collect();
        println!("  dark {dark}: paper at the band boundary {paper:?}");
        let expected = if dark { 0.0..60.0 } else { 250.0..256.0 };
        assert!(paper.iter().all(|l| expected.contains(l)), "{paper:?}");
        assert!(!has_color_matrix(&next_frame_node(&view)));
    };
    check(false);
    activate(&state, "win.pdf-invert");
    assert!(view.is_dark());
    settle();
    check(true);
    activate(&state, "win.pdf-invert");
    state.window.destroy();
}

/// Acceptance: the dark-mode toggle (Ctrl+Shift+I) inverts the pages, at once and then from
/// pages rendered dark.
fn dark_toggle() {
    let dir = copy_proj("dark", true);
    let state = window(&dir, config());
    state.pdf.get().unwrap().open(&dir.join("main.pdf"));
    wait_for_pages(&state);
    let view = view(&state);
    // A point in the top margin of the first visible page.
    let page = view.visible_pages().start;
    let (x, y) = pdf_view::view_point_of(&view, page, 30.0, 20.0).unwrap();
    let paper = |view: &PdfView| {
        let (width, _, pixels) = render(view);
        lightness(&pixels, width, x as i32, y as i32)
    };
    let initial = view.is_dark();
    let before = paper(&view);
    activate(&state, "win.pdf-invert");
    assert_eq!(view.is_dark(), !initial);
    let after = paper(&view);
    println!(
        "  dark {initial} -> {}: paper {before:.0} -> {after:.0}",
        !initial
    );
    let (light, dark) = if initial {
        (after, before)
    } else {
        (before, after)
    };
    assert!(light > 240.0 && dark < 60.0, "light {light}, dark {dark}");
    // That was the other mode's pages, recoloured. Once the pages are rendered in the new mode
    // they look the same and are drawn as they are: a colour matrix over the pages costs GTK's
    // cairo renderer a per-pixel pass on every scrolled frame.
    wait_for_pages(&state);
    let settled = paper(&view);
    assert!((settled - after).abs() <= 2.0, "{after} -> {settled}");
    assert!(!has_color_matrix(&next_frame_node(&view)));
    let state_value =
        gio::prelude::ActionGroupExt::action_state(&state.window, "pdf-invert").unwrap();
    assert_eq!(state_value.get::<bool>(), Some(!initial));
    activate(&state, "win.pdf-invert");
    assert_eq!(view.is_dark(), initial);
    wait_for_pages(&state);
    let back = paper(&view);
    assert!((back - before).abs() <= 2.0, "{before} -> {back}");
    assert!(!has_color_matrix(&next_frame_node(&view)));
    state.window.destroy();
}

/// True when `node` or a node under it is a colour matrix.
fn has_color_matrix(node: &gtk::gsk::RenderNode) -> bool {
    use gtk::gsk::*;
    if node.downcast_ref::<ColorMatrixNode>().is_some() {
        return true;
    }
    let children: Vec<RenderNode> = if let Some(n) = node.downcast_ref::<ContainerNode>() {
        (0..n.n_children()).map(|i| n.child(i)).collect()
    } else if let Some(n) = node.downcast_ref::<TransformNode>() {
        vec![n.child()]
    } else if let Some(n) = node.downcast_ref::<ClipNode>() {
        vec![n.child()]
    } else if let Some(n) = node.downcast_ref::<RoundedClipNode>() {
        vec![n.child()]
    } else if let Some(n) = node.downcast_ref::<OpacityNode>() {
        vec![n.child()]
    } else if let Some(n) = node.downcast_ref::<DebugNode>() {
        vec![n.child()]
    } else {
        Vec::new()
    };
    children.iter().any(has_color_matrix)
}

/// Acceptance: Ctrl+Alt+J scrolls the line's box to a third of the view and flashes it.
fn forward_search() {
    let dir = copy_proj("forward", true);
    let state = window(&dir, config());
    let editor = open(&state, &dir.join("main.tex"));
    // The PDF of the earlier build is shown as soon as the document is.
    wait_for_pages(&state);
    let controller = state.pdf.get().unwrap();
    assert_eq!(controller.pdf(), Some(dir.join("main.pdf")));
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(65).unwrap());
    let view = view(&state);
    assert!(view.highlight().is_none());
    activate(&state, "win.sync-forward");
    common::pump_until(TIMEOUT, || view.highlight().is_some());
    let (page, boxes) = view.highlight().unwrap();
    assert_eq!(page, 1, "main.tex:66 is on page 2");
    let (h, v, w, hh) = boxes[0];
    println!(
        "  box on page {} at ({h:.2}, {v:.2}, {w:.2}, {hh:.2})",
        page + 1
    );
    for (got, want) in [(h, 126.67), (v, 415.05), (w, 358.66), (hh, 9.70)] {
        assert!((got - want).abs() < 0.1, "{got} vs {want}");
    }
    let (_, y) = pdf_view::view_point_of(&view, page, h, v).unwrap();
    let third = f64::from(view.height()) / 3.0;
    assert!((y - third).abs() <= 1.0, "the box is at {y}, not {third}");
    review_snapshot(&state, "forward-search");
    // After the window's memory trim, which frees the SyncTeX scanner, the next search parses
    // the file again.
    state.trim_memory();
    controller.forward_search(&dir.join("chapters/yoneda.tex"), 26);
    common::pump_until(TIMEOUT, || {
        view.highlight()
            .is_some_and(|(page, b)| page == 1 && b[0].1 != boxes[0].1)
    });
    state.window.destroy();
}

/// Acceptance: Ctrl+click in the PDF opens the source line (also in an `\input` file).
fn inverse_search() {
    let dir = copy_proj("inverse", true);
    let state = window(&dir, config());
    open(&state, &dir.join("main.tex"));
    wait_for_pages(&state);
    let view = view(&state);
    view.goto_page(1);
    wait_for_pages(&state);
    let cursor = |state: &WindowState| {
        state.active_editor().map(|e| {
            let buffer = e.buffer();
            let line = buffer.iter_at_mark(&buffer.get_insert()).line();
            (e.document().path().to_path_buf(), line)
        })
    };
    for (x_bp, y_bp, file, line) in [
        (200.0, 420.0, "main.tex", 65),
        (200.0, 240.0, "chapters/yoneda.tex", 24),
    ] {
        let (x, y) = pdf_view::view_point_of(&view, 1, x_bp, y_bp).unwrap();
        assert!(view.inverse_search_at(x, y), "({x}, {y}) is on a page");
        let want = (dir.join(file), line);
        common::pump_until(TIMEOUT, || cursor(&state).as_ref() == Some(&want));
        println!("  page 2 ({x_bp}, {y_bp}) -> {file}:{}", line + 1);
    }
    assert!(
        !view.inverse_search_at(1.0, 1.0),
        "the gap is not on a page"
    );
    state.window.destroy();
}

/// With `pdf.follow_cursor`, the PDF follows the cursor once it rests.
fn follow_cursor() {
    let dir = copy_proj("follow", true);
    let mut config = config();
    config.pdf.follow_cursor = true;
    let state = window(&dir, config);
    let editor = open(&state, &dir.join("main.tex"));
    wait_for_pages(&state);
    let view = view(&state);
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(65).unwrap());
    common::pump_until(TIMEOUT, || view.highlight().is_some_and(|(p, _)| p == 1));
    state.window.destroy();
}

/// Acceptance: save → build → diagnostics in the Problems panel, gutter marks and underlines;
/// the PDF reloads (TeX errors with output); a fixed document clears them. A build with nothing
/// to do (latexmk: "All targets are up-to-date") reloads nothing; one after the PDF changed
/// does.
fn save_build_diagnostics() {
    if !have_latexmk() {
        return;
    }
    let dir = copy_proj("build", false);
    let mut config = config();
    config.build.on_save = true;
    let state = window(&dir, config);
    let editor = open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let seen = outcomes.clone();
    build.connect_finished(move |o| seen.borrow_mut().push(o.clone()));

    let buffer = editor.buffer();
    let mut iter = buffer.iter_at_line(65).unwrap();
    buffer.insert(&mut iter, "\\undefinedcommand ");
    state.save_document(editor.document());
    common::pump_until(BUILD_TIMEOUT, || outcomes.borrow().len() == 1);
    let outcome = outcomes.borrow()[0].clone();
    println!(
        "  first build: {:?}, exit {:?}, {:.1?}",
        outcome.status, outcome.exit_code, outcome.elapsed
    );
    assert_eq!(outcome.status, BuildStatus::Errors);
    assert_eq!(outcome.exit_code, Some(12));
    assert!(
        std::fs::read_to_string(dir.join("main.tex"))
            .unwrap()
            .contains("\\undefinedcommand"),
        "the buffer was saved before building"
    );

    let main = dir.join("main.tex");
    let problems = build.diagnostics().problems();
    let index = problems
        .iter()
        .position(|p| {
            p.path == main
                && p.line == Some(66)
                && p.severity == Severity::Error
                && p.message.contains("Undefined control sequence")
        })
        .unwrap_or_else(|| panic!("no error at main.tex:66 in {problems:#?}"));
    assert!(
        !buffer
            .source_marks_at_line(65, Some(ERROR_CATEGORY))
            .is_empty()
    );
    let tag = buffer.tag_table().lookup(ERROR_TAG).unwrap();
    assert!(buffer.iter_at_line(65).unwrap().has_tag(&tag));
    assert!(!buffer.iter_at_line(64).unwrap().has_tag(&tag));
    assert!(editor.view().shows_line_marks(), "the marks column appears");
    assert!(state.status.build.text().starts_with("Build failed"));
    let problems_page = state.sidebar.stack.page(&state.sidebar.problems);
    assert!(problems_page.needs_attention());

    // The PDF with the errors' output replaces the (missing) earlier one.
    let controller = state.pdf.get().unwrap();
    common::pump_until(TIMEOUT, || {
        controller.pdf() == Some(dir.join(".build/main.pdf"))
    });
    // Open PDF in Another App hands over the PDF the build made, in the hidden `.build`.
    let handed = Rc::new(RefCell::new(Vec::new()));
    let log = handed.clone();
    controller.set_launch_hook(Some(Box::new(move |handoff, pdf| {
        log.borrow_mut().push((handoff, pdf.to_path_buf()));
        true
    })));
    activate(&state, "win.pdf-open-external");
    assert_eq!(
        *handed.borrow(),
        [(
            lemma_app::pdf::pdf_controller::Handoff::Open,
            dir.join(".build/main.pdf")
        )]
    );
    buffer.place_cursor(&buffer.iter_at_line(60).unwrap());
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.3);
    review_snapshot(&state, "build-errors-editor");
    lemma_app::diagnostics_panel::reveal(&state);
    review_snapshot(&state, "build-errors-problems");

    // Activating the problem jumps to its line.
    buffer.place_cursor(&buffer.start_iter());
    build.diagnostics().activate(index);
    common::pump_until(TIMEOUT, || {
        buffer.iter_at_mark(&buffer.get_insert()).line() == 65
    });

    // Fixing the error and saving again clears it.
    let generation = controller.generation();
    let mut start = buffer.iter_at_line(65).unwrap();
    let mut end = start;
    end.forward_chars("\\undefinedcommand ".len() as i32);
    buffer.delete(&mut start, &mut end);
    state.save_document(editor.document());
    common::pump_until(BUILD_TIMEOUT, || outcomes.borrow().len() == 2);
    let outcome = outcomes.borrow()[1].clone();
    assert_eq!(outcome.status, BuildStatus::Success, "{outcome:?}");
    let problems = build.diagnostics().problems();
    assert!(
        problems.iter().all(|p| p.severity != Severity::Error),
        "{problems:#?}"
    );
    assert!(
        buffer
            .source_marks_at_line(65, Some(ERROR_CATEGORY))
            .is_empty()
    );
    assert!(!buffer.iter_at_line(65).unwrap().has_tag(&tag));
    // The marks column stays only while warnings with lines are left in main.tex.
    let marks_left = problems
        .iter()
        .any(|p| p.path == main && p.line.is_some() && p.severity == Severity::Warning);
    assert_eq!(editor.view().shows_line_marks(), marks_left);
    assert!(state.status.build.text().starts_with("Built in"));
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    review_snapshot(&state, "build-clean");

    // F5 with nothing changed: latexmk runs no TeX, and its log still says "Output written".
    let shown = controller.generation();
    activate(&state, "win.build");
    common::pump_until(BUILD_TIMEOUT, || outcomes.borrow().len() == 3);
    let again = outcomes.borrow()[2].clone();
    assert_eq!(again.status, BuildStatus::Success, "{again:?}");
    assert_eq!(again.pdf, outcome.pdf);
    assert!(again.pdf_stamp.is_some());
    assert_eq!(again.pdf_stamp, outcome.pdf_stamp);
    common::pump_for(Duration::from_millis(300));
    assert_eq!(
        controller.generation(),
        shown,
        "the same PDF is not reloaded"
    );
    assert!(controller.shows_requested());
    // After the PDF changed on disk, the next build reloads it.
    let pdf = dir.join(".build/main.pdf");
    std::fs::File::options()
        .write(true)
        .open(&pdf)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .unwrap();
    activate(&state, "win.build");
    common::pump_until(BUILD_TIMEOUT, || outcomes.borrow().len() == 4);
    assert_ne!(outcomes.borrow()[3].pdf_stamp, again.pdf_stamp);
    common::pump_until(TIMEOUT, || controller.generation() > shown);
    state.window.destroy();
}

/// Acceptance: Stop cancels a running build (the whole process group).
fn stop_cancels() {
    if !have_latexmk() {
        return;
    }
    let dir = common::temp_dir("stop");
    let tex = dir.join("loop.tex");
    std::fs::write(
        &tex,
        "\\documentclass{article}\n\\begin{document}\nx\\loop\\iftrue\\repeat\n\\end{document}\n",
    )
    .unwrap();
    let state = window(&dir, config());
    open(&state, &tex);
    let build = state.build.get().unwrap();
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let seen = outcomes.clone();
    build.connect_finished(move |o| seen.borrow_mut().push(o.clone()));
    build.request(Reason::Manual);
    common::pump_until(TIMEOUT, || build.is_running());
    review_snapshot(&state, "building");
    assert_eq!(
        state.header.build_button.action_name().as_deref(),
        Some("win.stop-build"),
        "the build button stops while building"
    );
    // Let TeX get into its loop.
    common::pump_for(Duration::from_millis(1500));
    assert!(
        outcomes.borrow().is_empty(),
        "the loop does not end by itself"
    );
    activate(&state, "win.stop-build");
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 1);
    let outcome = outcomes.borrow()[0].clone();
    println!("  stopped after {:.1?}", outcome.elapsed);
    assert_eq!(outcome.status, BuildStatus::Cancelled);
    assert_eq!(state.status.build.text(), "Build stopped");
    assert_eq!(
        state.header.build_button.action_name().as_deref(),
        Some("win.build")
    );
    assert!(!build.is_running());
    state.window.destroy();
}

// ----- with a fake latexmk -----

/// A fake latexmk that appends its arguments to `calls.txt` next to it and "builds"
/// `main.tex` into `.build`: it fails with one TeX error while a file `fail` lies next to it or
/// `.build/main.aux` says `broken`, sleeps 30 s first while a file `slow` does, and with `-c`
/// removes the `.aux`, the log and `slow` (after 300 ms, so the clean-up can be watched, and
/// 30 s more while a file `slow-clean` lies next to it).
const FAKE_LATEXMK: &str = r#"#!/bin/sh
here="$(dirname "$0")"
echo "$*" >> "$here/calls.txt"
case " $* " in
  *" -c "*)
    sleep 0.3
    [ -f "$here/slow-clean" ] && sleep 30
    rm -f .build/main.aux .build/main.log "$here/slow"
    exit 0 ;;
esac
[ -f "$here/slow" ] && sleep 30
mkdir -p .build
if [ -f "$here/fail" ] || grep -q broken .build/main.aux 2>/dev/null; then
  printf '(./main.tex\n./main.tex:3: Undefined control sequence.\nl.3 \\foo\n\n)\n' > .build/main.log
  exit 12
fi
printf '\\relax\n' > .build/main.aux
printf '(./main.tex)\n' > .build/main.log
exit 0
"#;

const SIMPLE_DOC: &str = "\\documentclass{article}\n\\begin{document}\nx\n\\end{document}\n";

/// A project with `main.tex` and, in a directory of its own, the fake latexmk; returns the
/// project and the fake.
fn fake_latexmk_project(name: &str) -> (PathBuf, PathBuf) {
    let dir = common::temp_dir(name).canonicalize().unwrap();
    std::fs::write(dir.join("main.tex"), SIMPLE_DOC).unwrap();
    let bin = common::temp_dir(&format!("{name}-bin"));
    // Copied into place by `cp`: had this process written it, a child forked meanwhile by a
    // build thread could still hold it open for writing when it runs ("Text file busy").
    let source = bin.join("latexmk.src");
    std::fs::write(&source, FAKE_LATEXMK).unwrap();
    let latexmk = bin.join("latexmk");
    let copied = Command::new("cp").arg(&source).arg(&latexmk).status();
    assert!(copied.is_ok_and(|s| s.success()));
    let chmod = Command::new("chmod").arg("755").arg(&latexmk).status();
    assert!(chmod.is_ok_and(|s| s.success()));
    (dir, latexmk)
}

/// The fake latexmk's calls so far, one argument line each.
fn fake_calls(latexmk: &Path) -> Vec<String> {
    std::fs::read_to_string(latexmk.with_file_name("calls.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Collects every finished build's outcome.
fn record_outcomes(state: &WindowState) -> Rc<RefCell<Vec<lemma_build::BuildOutcome>>> {
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let seen = outcomes.clone();
    state
        .build
        .get()
        .unwrap()
        .connect_finished(move |o| seen.borrow_mut().push(o.clone()));
    outcomes
}

/// Counts how often `toast` is dismissed.
fn count_dismissals(toast: &adw::Toast) -> Rc<Cell<u32>> {
    let count = Rc::new(Cell::new(0));
    let seen = count.clone();
    toast.connect_dismissed(move |_| seen.set(seen.get() + 1));
    count
}

/// Each build's toast replaces the one before: failing builds leave one toast (with the latest
/// count), not a queue of them, and a build that succeeds withdraws it.
fn build_toasts_replace() {
    let (dir, latexmk) = fake_latexmk_project("toasts");
    let fail = latexmk.with_file_name("fail");
    std::fs::write(&fail, "").unwrap();
    let mut config = config();
    config.build.latexmk = latexmk.clone();
    let state = window(&dir, config);
    open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = record_outcomes(&state);

    build.request(Reason::Manual);
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 1);
    assert_eq!(outcomes.borrow()[0].status, BuildStatus::Errors);
    let first = build.toast().expect("a failed build is toasted");
    assert_eq!(first.title().as_deref(), Some("Build failed: 1 error"));
    let first_dismissed = count_dismissals(&first);

    // A save while the error remains: its toast takes the place of the first.
    build.request(Reason::Save);
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 2);
    assert_eq!(first_dismissed.get(), 1, "the first toast is withdrawn");
    let second = build.toast().expect("the second build's toast");
    assert_ne!(second, first);
    assert_eq!(second.title().as_deref(), Some("Build failed: 1 error"));
    let second_dismissed = count_dismissals(&second);

    // Fixed: the success withdraws the failure's toast and shows none of its own.
    std::fs::remove_file(&fail).unwrap();
    build.request(Reason::Save);
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 3);
    assert_eq!(outcomes.borrow()[2].status, BuildStatus::Success);
    assert_eq!(second_dismissed.get(), 1);
    assert!(build.toast().is_none());
    assert_eq!(fake_calls(&latexmk).len(), 3);
    state.window.destroy();
}

/// Without latexmk, a build says what to install. F5 says it every time; the automatic builds
/// (saves, the agent) once, then only in the status bar, until a latexmk starts again.
fn missing_latexmk() {
    let (dir, fake) = fake_latexmk_project("no-latexmk");
    let missing = dir.join("no-such-dir/latexmk");
    let mut config = config();
    config.build.latexmk = missing.clone();
    let state = window(&dir, config.clone());
    open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = record_outcomes(&state);
    let built = |n: usize| common::pump_until(TIMEOUT, || outcomes.borrow().len() == n);
    let missing_text = "Not built: latexmk not found";

    build.request(Reason::Save);
    built(1);
    assert_eq!(
        outcomes.borrow()[0].status,
        BuildStatus::SpawnFailed(LATEXMK_MISSING.to_owned())
    );
    assert_eq!(state.status.build.text(), missing_text);
    let toast = build.toast().expect("the first automatic build explains");
    assert_eq!(toast.title().as_deref(), Some(LATEXMK_MISSING));
    let dismissed = count_dismissals(&toast);
    review_snapshot(&state, "build-latexmk-missing");

    // The next save says nothing new, and leaves the explanation alone.
    build.request(Reason::Save);
    built(2);
    assert_eq!(build.toast().as_ref(), Some(&toast));
    assert_eq!(dismissed.get(), 0);
    assert_eq!(state.status.build.text(), missing_text);
    // Gone (its timeout): the agent's builds bring it back no more.
    toast.dismiss();
    assert!(build.toast().is_none());
    build.request(Reason::AgentTurnEnd);
    built(3);
    build.request(Reason::AgentCompile);
    built(4);
    assert!(build.toast().is_none());
    assert_eq!(state.status.build.text(), missing_text);

    // F5 always explains.
    build.request(Reason::Manual);
    built(5);
    let manual = build.toast().expect("F5 explains");
    assert_eq!(manual.title().as_deref(), Some(LATEXMK_MISSING));

    // A build that fails without starting a latexmk (this one cannot be executed) does not
    // bring the advice back: the save after it withdraws that build's toast and says nothing new.
    let unrunnable = fake.with_file_name("latexmk-not-executable");
    std::fs::write(&unrunnable, "#!/bin/sh\nexit 0\n").unwrap();
    let mut refused = config.clone();
    refused.build.latexmk = unrunnable;
    state.set_config(Rc::new(refused));
    build.request(Reason::Save);
    built(6);
    assert!(
        matches!(&outcomes.borrow()[5].status, BuildStatus::SpawnFailed(e) if e != LATEXMK_MISSING),
        "{:?}",
        outcomes.borrow()[5].status
    );
    let could_not = build.toast().expect("the failure is toasted");
    assert!(
        could_not
            .title()
            .is_some_and(|t| t.starts_with("Could not build: cannot run ")),
        "{:?}",
        could_not.title()
    );
    let withdrawn = count_dismissals(&could_not);
    state.set_config(Rc::new(config.clone()));
    build.request(Reason::Save);
    built(7);
    assert_eq!(state.status.build.text(), missing_text);
    assert!(build.toast().is_none(), "nothing new: {:?}", build.toast());
    assert_eq!(withdrawn.get(), 1, "the failure before is withdrawn");

    // Once a latexmk runs, a missing one is explained again.
    let mut working = config.clone();
    working.build.latexmk = fake;
    state.set_config(Rc::new(working));
    build.request(Reason::Save);
    built(8);
    assert_eq!(outcomes.borrow()[7].status, BuildStatus::Success);
    assert!(build.toast().is_none(), "the success withdraws the advice");
    state.set_config(Rc::new(config));
    build.request(Reason::Save);
    built(9);
    let again = build.toast().expect("explained again");
    assert_eq!(again.title().as_deref(), Some(LATEXMK_MISSING));
    assert!(!missing.exists());
    state.window.destroy();
}

/// Clean Build Files and Rebuild: stops the running build, runs `latexmk -c` with the build's
/// engine flag and output directory, then builds again, which a stale `.aux` no longer breaks.
/// Stop drops the clean-up before it starts, and stops it while it runs.
fn clean_and_rebuild() {
    let (dir, latexmk) = fake_latexmk_project("clean");
    let mut config = config();
    config.build.latexmk = latexmk.clone();
    let state = window(&dir, config);
    let build = state.build.get().unwrap();
    let outcomes = record_outcomes(&state);

    // Asked for with nothing to build: the clean-up goes with the build that cannot start, and
    // the next build does not clean.
    activate(&state, "win.clean-build");
    common::pump_until(TIMEOUT, || {
        build
            .toast()
            .is_some_and(|t| t.title().as_deref() == Some("Open a document to build"))
    });
    open(&state, &dir.join("main.tex"));

    // A broken `.aux` fails every build.
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    std::fs::write(dir.join(".build/main.aux"), "broken\n").unwrap();
    build.request(Reason::Manual);
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 1);
    assert_eq!(outcomes.borrow()[0].status, BuildStatus::Errors);
    assert_eq!(fake_calls(&latexmk).len(), 1, "no clean-up");

    // Another build runs (slowly, until the clean-up) when the clean-up is asked for: it is
    // stopped first.
    let slow = latexmk.with_file_name("slow");
    std::fs::write(&slow, "").unwrap();
    build.request(Reason::Save);
    common::pump_until(TIMEOUT, || fake_calls(&latexmk).len() == 2);
    activate(&state, "win.clean-build");
    common::pump_until(TIMEOUT, || {
        state.status.build.text() == "Cleaning and building main.tex…"
    });
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 3);
    let statuses: Vec<BuildStatus> = outcomes.borrow().iter().map(|o| o.status.clone()).collect();
    assert_eq!(
        statuses,
        [
            BuildStatus::Errors,
            BuildStatus::Cancelled,
            BuildStatus::Success
        ]
    );
    let calls = fake_calls(&latexmk);
    assert_eq!(calls.len(), 4, "{calls:#?}");
    assert_eq!(calls[2], "-pdf -bibtex-cond1 -c -outdir=.build ./main.tex");
    assert!(calls[3].starts_with("-pdf -synctex=1 "), "{calls:#?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("main.tex")).unwrap(),
        SIMPLE_DOC
    );
    assert!(state.status.build.text().starts_with("Built in"));
    assert!(build.diagnostics().problems().is_empty());

    // A Stop before the build starts drops the clean-up with it.
    std::fs::write(&slow, "").unwrap();
    build.request(Reason::Manual);
    common::pump_until(TIMEOUT, || fake_calls(&latexmk).len() == 5);
    activate(&state, "win.clean-build");
    activate(&state, "win.stop-build");
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 4);
    assert_eq!(outcomes.borrow()[3].status, BuildStatus::Cancelled);
    std::fs::remove_file(&slow).unwrap();
    common::pump_for(Duration::from_millis(500));
    assert_eq!(outcomes.borrow().len(), 4, "nothing builds after the Stop");
    build.request(Reason::Manual);
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 5);
    let calls = fake_calls(&latexmk);
    assert!(calls[5..].iter().all(|c| !c.contains(" -c ")), "{calls:#?}");

    // A Stop while the clean-up runs stops it, and nothing builds after it.
    let slow_clean = latexmk.with_file_name("slow-clean");
    std::fs::write(&slow_clean, "").unwrap();
    std::fs::write(dir.join(".build/main.aux"), "broken\n").unwrap();
    let before = fake_calls(&latexmk).len();
    activate(&state, "win.clean-build");
    common::pump_until(TIMEOUT, || fake_calls(&latexmk).len() == before + 1);
    activate(&state, "win.stop-build");
    common::pump_until(TIMEOUT, || outcomes.borrow().len() == 6);
    assert_eq!(outcomes.borrow()[5].status, BuildStatus::Cancelled);
    assert_eq!(state.status.build.text(), "Build stopped");
    common::pump_for(Duration::from_millis(300));
    let calls = fake_calls(&latexmk);
    assert_eq!(calls.len(), before + 1, "{calls:#?}");
    assert!(calls[before].contains(" -c "), "{calls:#?}");
    assert!(
        dir.join(".build/main.aux").exists(),
        "the clean-up was stopped"
    );
    std::fs::remove_file(&slow_clean).unwrap();
    state.window.destroy();
}
