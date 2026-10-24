//! The PDF preview, builds and diagnostics (WP1.6 acceptance, ARCHITECTURE §6 and §10.7):
//! `--demo pdf`, scrolling cost and the cache cap, reload anchors, an unchanged PDF that is not
//! reloaded, anchored zoom, dark mode, save → build → diagnostics (and a build with nothing to
//! do), stopping a build, and SyncTeX in both directions.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_pdf`. The scrolling and
//! demo tests need `target/fixtures/pdf50.pdf` (`scripts/build-fixtures.sh`); the build tests
//! need latexmk. Tests whose prerequisites are missing print `SKIP` and pass.

mod common;

use adw::prelude::*;
use gtk::{gdk, graphene};
use lemma_app::build_ui::Reason;
use lemma_app::diagnostics_panel::{ERROR_CATEGORY, ERROR_TAG};
use lemma_app::pdf::pdf_view::{self, PdfView};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_build::{BuildStatus, PdfStamp};
use lemma_config::Config;
use lemma_core::{ProjectRoot, Severity};
use sourceview5::prelude::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

fn main() -> ExitCode {
    common::run(&[
        ("demo_snapshot_shows_three_pages", demo_snapshot),
        ("scrolling_pdf50_is_fast_and_bounded", scrolling_pdf50),
        ("reload_keeps_the_scroll_anchor", reload_keeps_anchor),
        ("an_unchanged_pdf_is_not_reloaded", unchanged_pdf),
        ("ctrl_wheel_zoom_is_anchored_at_the_pointer", anchored_zoom),
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
