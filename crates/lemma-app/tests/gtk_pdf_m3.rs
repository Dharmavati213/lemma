//! WP3.4: PDF links (geometry, hover preview, jumps and Back, the cursor, web links behind a
//! confirmation), search (counts against `pdftotext`, navigation, case and whole words,
//! cancellation and latest wins, highlights, a rebuilt PDF that keeps the reading place), the
//! strip (lazy thumbnails within their budget, bound rows that need more than it, the current
//! page, the bookmark tree), detaching the PDF into its own window
//! and back (anchor, search, SyncTeX, dark pages, reloads), and `--demo pdf-search`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_pdf_m3`. The pdf50
//! tests need `target/fixtures/pdf50.pdf` (`scripts/build-fixtures.sh`) and print `SKIP`
//! without it. `LEMMA_TEST_SNAPSHOTS=dir` saves review snapshots there.

mod broadway_input;
mod common;

use adw::prelude::*;
use gtk::{gdk, graphene};
use lemma_app::editor::goto;
use lemma_app::pdf::pdf_view::{self, PdfView};
use lemma_app::pdf::{detach, links, search, sidebar};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use lemma_pdf::{LinkDest, SearchOptions};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[("pdf_m3_real_keys_child", real_keys::child)]);
    }
    common::run(&[
        ("links_geometry_hover_preview_and_jump_back", links_and_back),
        ("web_links_ask_before_opening", web_links),
        ("search_counts_match_pdftotext_and_navigate", search_counts),
        (
            "search_options_cancel_latest_wins_and_rebuild",
            search_behaviour,
        ),
        ("search_highlights_are_drawn", search_highlights),
        ("thumbnails_are_lazy_within_their_budget", thumbnails),
        (
            "thumbnails_settle_when_the_rows_need_more_than_the_budget",
            thumbnails_beyond_the_budget,
        ),
        ("bookmark_tree_matches_the_golden_tree", bookmarks),
        ("detach_and_reattach_keep_the_anchor", detach_reattach),
        ("demo_pdf_search_snapshot", demo_pdf_search),
        ("pdf_keys_on_real_key_events", real_keys::parent),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);
const MIB: usize = 1 << 20;

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

/// A copy of `tests/fixtures/proj` with its built PDF and SyncTeX file.
fn copy_proj(name: &str) -> PathBuf {
    let dir = common::temp_dir(name);
    let src = common::fixtures().join("proj");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    for file in [
        "main.tex",
        "refs.bib",
        "chapters/yoneda.tex",
        "main.pdf",
        "main.synctex.gz",
    ] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    dir.canonicalize().unwrap()
}

/// A window (not scripted: dialogs are shown) on `dir`.
fn window_with(dir: &Path, scripted: bool, configure: impl FnOnce(&mut Config)) -> Rc<WindowState> {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_enable_animations(false);
        settings.set_gtk_cursor_blink(false);
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.spell.enabled = false;
    config.preview.delay_ms = 60;
    configure(&mut config);
    let workspace = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        None,
        workspace,
        scripted,
    );
    state.window.set_default_size(1000, 760);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    state
}

fn window(dir: &Path) -> Rc<WindowState> {
    window_with(dir, false, |_| {})
}

fn view(state: &WindowState) -> PdfView {
    state.pdf.get().expect("a PDF controller").view().clone()
}

/// Opens `pdf` and waits until every visible page is drawn from a current bitmap.
fn show_pdf(state: &WindowState, pdf: &Path) -> PdfView {
    state.pdf.get().unwrap().open(pdf);
    wait_for_pages(state);
    view(state)
}

fn wait_for_pages(state: &WindowState) {
    let view = view(state);
    common::pump_until(TIMEOUT, || {
        let visible = view.visible_pages();
        view.page_count() > 0
            && view.is_mapped()
            && view.render_scale_q() > 0
            && !visible.is_empty()
            && visible.clone().all(|p| view.is_page_rendered(p))
    });
}

fn activate(state: &WindowState, action: &str) {
    WidgetExt::activate_action(&state.window, action, None).expect("the action exists");
}

/// Draws a real frame and returns `widget`'s render node from it.
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

/// The BGRA pixel at `(x, y)`.
fn pixel(pixels: &[u8], width: i32, x: i32, y: i32) -> [u8; 4] {
    let i = (y as usize * width as usize + x as usize) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// With `LEMMA_TEST_SNAPSHOTS=dir`, saves `widget`'s next frame as `dir/name.png`.
fn review_snapshot(widget: &impl IsA<gtk::Widget>, name: &str) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let path = PathBuf::from(dir).join(format!("{name}.png"));
    let saved = glib::MainContext::default().block_on(async {
        lemma_app::testmode::wait_for_frames(widget, 2).await;
        lemma_app::testmode::snapshot_next_frame(widget, &path).await
    });
    match saved {
        Ok(_) => println!("  snapshot {}", path.display()),
        Err(e) => println!("  snapshot {name} failed: {e}"),
    }
}

/// True when the keyboard focus of `window` is `widget` or inside it.
fn focus_within(window: &impl IsA<gtk::Window>, widget: &impl IsA<gtk::Widget>) -> bool {
    gtk::prelude::GtkWindowExt::focus(window.as_ref())
        .is_some_and(|f| &f == widget.as_ref() || f.is_ancestor(widget))
}

/// The centre of `rect` (bp) of `page`, in view coordinates.
fn centre(view: &PdfView, page: u32, rect: (f64, f64, f64, f64)) -> (f64, f64) {
    pdf_view::view_point_of(view, page, rect.0 + rect.2 / 2.0, rect.1 + rect.3 / 2.0)
        .expect("a point of the document")
}

/// Waits for the links of `page` of the shown PDF.
fn page_links(state: &WindowState, page: u32) -> Vec<lemma_pdf::Link> {
    common::pump_until(TIMEOUT, || links::page_links(state, page).is_some());
    links::page_links(state, page).unwrap()
}

/// Searches for `query` and waits until every page has been searched.
fn search_all(state: &Rc<WindowState>, query: &str) -> search::Status {
    search::search_now(state, query);
    common::pump_until(TIMEOUT, || {
        search::status(state).is_some_and(|s| s.done && s.query.as_deref() == Some(query))
    });
    search::status(state).unwrap()
}

/// Acceptance: the hyperref links of `proj` (geometry and destinations); hovering one shows the
/// destination in a popover and a hand pointer; clicking jumps there; Alt+Left (with the PDF
/// focused) comes back; the editor's jump list is not touched.
fn links_and_back() {
    let dir = copy_proj("links");
    let state = window(&dir);
    let view = show_pdf(&state, &dir.join("main.pdf"));
    view.goto_page(0);
    wait_for_pages(&state);
    let links = page_links(&state, 0);
    let internal: Vec<_> = links
        .iter()
        .filter(|l| matches!(l.dest, LinkDest::Page { .. }))
        .collect();
    println!(
        "  page 1: {} links, {} internal",
        links.len(),
        internal.len()
    );
    assert!(internal.len() >= 4, "{links:#?}");

    // Geometry: the link of the citation [The26] covers its text.
    let status = search_all(&state, "The26");
    assert!(status.total >= 1);
    let hit = view.search_matches(0)[0][0];
    search::close(&state);
    let (cx, cy) = (hit.0 + hit.2 / 2.0, hit.1 + hit.3 / 2.0);
    let cite = links
        .iter()
        .find(|l| {
            let (x, y, w, h) = l.area;
            cx >= x && cx <= x + w && cy >= y && cy <= y + h
        })
        .expect("a link over [The26]");
    println!("  [The26] at {hit:?}; link {cite:?}");
    let LinkDest::Page {
        page: bib_page,
        y_bp,
    } = cite.dest.clone()
    else {
        panic!("the citation links into the document: {cite:?}");
    };
    assert_eq!(bib_page, 4, "the bibliography is on page 5");
    let bib_y = y_bp.expect("a destination with a position");
    assert!(
        cite.area.2 > hit.2 - 1.0 && cite.area.2 < hit.2 + 30.0,
        "{cite:?}"
    );

    // The pointer: a hand over the link, the preview after the delay.
    let (x, y) = centre(&view, 0, cite.area);
    links::pointer_moved(&state, x, y);
    assert_eq!(
        view.cursor().and_then(|c| c.name()).as_deref(),
        Some("pointer")
    );
    common::pump_until(TIMEOUT, || links::preview(&state).is_some());
    let (popover, picture, page, _) = links::preview(&state).unwrap();
    assert_eq!(page, 0, "the preview belongs to the link on page 1");
    assert!(popover.is_visible());
    assert_eq!(
        picture.page_size(),
        view.layout().unwrap().page_size(4).unwrap()
    );
    // The destination page is rendered for it (or borrowed from the view).
    common::pump_until(TIMEOUT, || picture.texture().is_some());
    assert!(!picture.can_focus() || !popover.can_focus());
    review_snapshot(&state.window, "m3-pdf-link-preview");
    // Off the link: the cursor is back and the preview goes after the grace period.
    let (x0, y0) = pdf_view::view_point_of(&view, 0, 300.0, 40.0).unwrap();
    links::pointer_moved(&state, x0, y0);
    assert_eq!(view.cursor(), None);
    common::pump_until(TIMEOUT, || links::preview(&state).is_none());
    assert!(!popover.is_visible());

    // A click jumps there, and Back (Alt+Left with the PDF focused) returns.
    let before = view.anchor().unwrap();
    assert!(view.grab_focus());
    common::pump_until(TIMEOUT, || links::pdf_has_focus(&state));
    assert!(links::click(&state, x, y));
    let after = view.anchor().unwrap();
    let (_, dest_y) = pdf_view::view_point_of(&view, 4, 0.0, bib_y).unwrap();
    println!("  jumped from {before:?} to {after:?}; destination at y {dest_y:.1}");
    // The destination is near the top of the view, unless the view is scrolled to the end.
    let height = f64::from(view.height());
    let content = view.layout().unwrap().content_size(view.zoom()).1;
    let at_end = view.scroll_position().1 >= content - height - 1.0;
    assert!(dest_y >= 0.0 && dest_y < height, "{dest_y}");
    assert!(
        (dest_y - links::JUMP_MARGIN).abs() <= 1.0 || at_end,
        "{dest_y} (at the end: {at_end})"
    );
    assert_eq!(links::history_len(&state), 1);
    assert!(goto::back(&state), "Alt+Left goes back");
    let back = view.anchor().unwrap();
    assert_eq!(back.page, before.page);
    assert!(
        (back.frac - before.frac).abs() < 1e-6,
        "{back:?} vs {before:?}"
    );
    assert_eq!(links::history_len(&state), 0);
    // With nothing left, Back is the editor's (here: nowhere to go).
    assert!(!links::back(&state));

    // The cursor is plain over a page without links under the pointer, and leaving resets it.
    links::pointer_moved(&state, x, y);
    links::pointer_left(&state);
    assert_eq!(view.cursor(), None);
    state.window.destroy();
}

/// Acceptance: a web link asks before it opens, and opens only on "Open"; runs that quit by
/// themselves never open one.
fn web_links() {
    let dir = copy_proj("uri");
    let state = window(&dir);
    let view = show_pdf(&state, &dir.join("main.pdf"));
    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = opened.clone();
    links::set_uri_hook(
        &state,
        Some(Box::new(move |uri| {
            seen.borrow_mut().push(uri.to_string());
            true
        })),
    );
    // The bibliography is on the last page.
    let page = view.page_count() - 1;
    view.goto_page(page);
    wait_for_pages(&state);
    let link = page_links(&state, page)
        .into_iter()
        .find(|l| matches!(l.dest, LinkDest::Uri(_)))
        .expect("the Stacks project's URL");
    let LinkDest::Uri(uri) = link.dest.clone() else {
        unreachable!()
    };
    assert_eq!(uri, "https://stacks.math.columbia.edu");
    view.goto_page(page);
    wait_for_pages(&state);
    let (x, y) = centre(&view, page, link.area);
    links::pointer_moved(&state, x, y);
    assert_eq!(
        view.cursor().and_then(|c| c.name()).as_deref(),
        Some("pointer")
    );

    assert!(links::click(&state, x, y));
    let dialog = links::uri_dialog(&state).expect("the confirmation");
    assert_eq!(dialog.body(), uri);
    assert!(
        opened.borrow().is_empty(),
        "nothing opens before the answer"
    );
    review_snapshot(&state.window, "m3-pdf-open-link");
    assert!(links::answer_uri_dialog(&state, "cancel"));
    common::pump_for(Duration::from_millis(50));
    assert!(opened.borrow().is_empty(), "Cancel opens nothing");
    assert!(links::uri_dialog(&state).is_none());

    assert!(links::click(&state, x, y));
    assert!(links::answer_uri_dialog(&state, "open"));
    common::pump_for(Duration::from_millis(50));
    assert_eq!(*opened.borrow(), std::slice::from_ref(&uri));
    state.window.destroy();

    // A scripted run refuses without asking.
    let dir = copy_proj("uri-scripted");
    let state = window_with(&dir, true, |_| {});
    let view = show_pdf(&state, &dir.join("main.pdf"));
    view.goto_page(page);
    wait_for_pages(&state);
    let _ = page_links(&state, page);
    let (x, y) = centre(&view, page, link.area);
    assert!(links::click(&state, x, y));
    assert!(links::uri_dialog(&state).is_none());
    state.window.destroy();
}

/// The text of every page of `pdf` as `pdftotext` extracts it, line breaks read as a search
/// across lines reads them (see `lemma-pdf/tests/m3.rs`).
fn pdftotext_pages(pdf: &Path) -> Option<Vec<String>> {
    let output = Command::new("pdftotext").arg(pdf).arg("-").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut pages: Vec<String> = text
        .split('\u{c}')
        .map(|page| page.replace("-\n", "").replace('\n', " "))
        .collect();
    if pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    Some(pages)
}

fn count_ci(hay: &str, needle: &str) -> usize {
    hay.to_lowercase().matches(&needle.to_lowercase()).count()
}

/// Acceptance: the match count equals `pdftotext`'s on pdf50; Enter and Shift+Enter walk the
/// matches in order and wrap around; the current match is scrolled into view.
fn search_counts() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("search");
    let state = window(&dir);
    let view = show_pdf(&state, &pdf);
    // Ctrl+F (win.pdf-search) with the PDF focused opens the bar with the focus in the entry.
    assert!(view.grab_focus());
    common::pump_for(Duration::from_millis(30));
    activate(&state, "win.pdf-search");
    let entry = search::entry(&state).unwrap();
    common::pump_until(TIMEOUT, || focus_within(&state.window, &entry));
    assert!(search::status(&state).unwrap().open);
    assert!(state.pdf.get().unwrap().search_button().is_active());

    let expected = pdftotext_pages(&pdf)
        .map(|pages| pages.iter().map(|p| count_ci(p, "nulla")).sum::<usize>());
    let status = search_all(&state, "nulla");
    println!("  \"nulla\": {status:?}, pdftotext {expected:?}");
    if let Some(expected) = expected {
        assert_eq!(status.total, expected);
    }
    assert_eq!(status.current, Some(1), "the first match on the first page");
    assert_eq!(status.label, format!("1 of {}", status.total));
    let first_page = view.current_match().unwrap().0;
    assert_eq!(first_page, 0);

    // Next and previous, in document order, wrapping.
    let pages = view.pages_with_matches();
    let per_page: Vec<usize> = pages
        .iter()
        .map(|p| view.search_matches(*p).len())
        .collect();
    assert_eq!(per_page.iter().sum::<usize>(), status.total);
    for i in 2..=per_page[0] + 1 {
        assert!(search::step(&state, true));
        assert_eq!(search::status(&state).unwrap().current, Some(i));
    }
    // The first match of the second page with matches, which is scrolled into view.
    assert_eq!(view.current_match(), Some((pages[1], 0)));
    let visible = view.visible_pages();
    assert!(
        visible.contains(&pages[1]),
        "{visible:?} shows page {}",
        pages[1]
    );
    // Back to the first, then before it: the last match.
    for _ in 0..per_page[0] {
        assert!(search::step(&state, false));
    }
    assert_eq!(search::status(&state).unwrap().current, Some(1));
    assert!(search::step(&state, false));
    let last = *pages.last().unwrap();
    assert_eq!(
        view.current_match(),
        Some((last, view.search_matches(last).len() - 1))
    );
    assert_eq!(search::status(&state).unwrap().current, Some(status.total));
    assert!(view.visible_pages().contains(&last));
    assert!(search::step(&state, true));
    assert_eq!(search::status(&state).unwrap().current, Some(1));

    // A search started further down begins there.
    search::close(&state);
    view.goto_page(20);
    wait_for_pages(&state);
    let status = search_all(&state, "nulla");
    let (page, _) = view.current_match().unwrap();
    assert!(
        page >= 20,
        "the first match at or after page 21, got {}",
        page + 1
    );
    assert!(status.current.unwrap() > 1);
    state.window.destroy();
}

/// Match Case and Whole Words; a new query replaces the search in progress; closing cancels it
/// and removes the highlights; a rebuilt PDF is searched again, keeping the current match and
/// the scroll position.
fn search_behaviour() {
    let Some(pdf50) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("search-options");
    // One path rebuilt in place.
    let pdf = dir.join("preview.pdf");
    std::fs::copy(&pdf50, &pdf).unwrap();
    let state = window(&dir);
    let view = show_pdf(&state, &pdf);

    let any = search_all(&state, "NULLA").total;
    assert!(any > 0);
    search::set_options(
        &state,
        SearchOptions {
            case_sensitive: true,
            whole_words: false,
        },
    );
    common::pump_until(TIMEOUT, || search::status(&state).is_some_and(|s| s.done));
    let exact = search::status(&state).unwrap();
    assert_eq!(exact.total, 0);
    assert_eq!(exact.label, "No matches");
    assert!(search::entry(&state).unwrap().has_css_class("error"));
    let capital = search_all(&state, "Nulla").total;
    assert!(capital > 0 && capital < any, "{capital} of {any}");
    search::set_options(&state, SearchOptions::default());

    // Whole words: "ut" alone, not inside "vulputate".
    let inside = search_all(&state, "ut").total;
    search::set_options(
        &state,
        SearchOptions {
            case_sensitive: false,
            whole_words: true,
        },
    );
    common::pump_until(TIMEOUT, || search::status(&state).is_some_and(|s| s.done));
    let whole = search::status(&state).unwrap().total;
    println!("  \"ut\": {inside} anywhere, {whole} as a word");
    assert!(whole > 0 && whole < inside);
    search::set_options(&state, SearchOptions::default());

    // Latest wins: only the last query's matches are shown.
    search::search_now(&state, "lorem");
    search::search_now(&state, "dolor");
    common::pump_until(TIMEOUT, || search::status(&state).is_some_and(|s| s.done));
    let dolor = search::status(&state).unwrap();
    assert_eq!(dolor.query.as_deref(), Some("dolor"));
    let shown: usize = view
        .pages_with_matches()
        .iter()
        .map(|p| view.search_matches(*p).len())
        .sum();
    assert_eq!(shown, dolor.total);
    let again = search_all(&state, "lorem").total;
    let dolor_alone = search_all(&state, "dolor").total;
    assert_eq!(dolor.total, dolor_alone);
    assert!(again > 0);

    // Closing cancels the search and removes the highlights.
    search::search_now(&state, "e");
    search::close(&state);
    common::pump_for(Duration::from_millis(300));
    let closed = search::status(&state).unwrap();
    assert!(!closed.open && closed.query.is_none() && closed.total == 0);
    assert!(view.pages_with_matches().is_empty());
    assert_eq!(view.current_match(), None);

    // A rebuilt PDF is searched again.
    let before = search_all(&state, "dolor").total;
    let controller = state.pdf.get().unwrap();
    let generation = controller.generation();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    common::pump_until(TIMEOUT, || {
        search::status(&state).is_some_and(|s| s.done && s.total == before)
    });
    assert!(view.current_match().is_some());

    // The search a rebuild runs again keeps the reading place. A word on a few pages only: the
    // user goes to the match on the second of them, then reads on a page without one.
    search_all(&state, "potenti");
    let pages = view.pages_with_matches();
    assert!(pages.len() >= 3, "{pages:?}");
    while view.current_match().map(|(p, _)| p) != Some(pages[1]) {
        assert!(search::step(&state, true));
    }
    let kept = view.current_match();
    let reading = (pages[1] + 1..pages[2])
        .next()
        .expect("a page without a match between");
    view.goto_page(reading);
    wait_for_pages(&state);
    let place = view.scroll_position();
    let rebuild = || {
        let generation = controller.generation();
        controller.open(&pdf);
        common::pump_until(TIMEOUT, || controller.generation() > generation);
        common::pump_until(TIMEOUT, || search::status(&state).is_some_and(|s| s.done));
        common::pump_for(Duration::from_millis(100));
        let moved = (view.scroll_position().1 - place.1).abs();
        assert!(moved <= 1.0, "{place:?} -> {:?}", view.scroll_position());
    };
    rebuild();
    assert_eq!(
        view.current_match(),
        kept,
        "the current match stays current"
    );
    assert_eq!(view.pages_with_matches(), pages);
    // Without a current match, the first one after the view becomes current, in place.
    view.set_current_match(None);
    rebuild();
    assert_eq!(view.current_match(), Some((pages[2], 0)));
    let before_it: usize = pages[..2]
        .iter()
        .map(|p| view.search_matches(*p).len())
        .sum();
    assert_eq!(search::status(&state).unwrap().current, Some(before_it + 1));
    // A search the user starts still goes to its match.
    search::close(&state);
    search_all(&state, "potenti");
    assert_eq!(view.current_match(), Some((pages[2], 0)));
    assert!(view.visible_pages().contains(&pages[2]));
    // So does one the PDF is rebuilt under before it found its match.
    search::close(&state);
    view.goto_page(reading);
    wait_for_pages(&state);
    search::search_now(&state, "potenti");
    let generation = controller.generation();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    common::pump_until(TIMEOUT, || search::status(&state).is_some_and(|s| s.done));
    assert_eq!(view.current_match(), Some((pages[2], 0)));
    assert!(view.visible_pages().contains(&pages[2]));
    state.window.destroy();
}

/// The matches on screen are drawn over the page, the current one in another colour; dark
/// pages keep the highlight colours.
fn search_highlights() {
    let dir = copy_proj("highlights");
    let state = window(&dir);
    let view = show_pdf(&state, &dir.join("main.pdf"));
    view.goto_page(0);
    wait_for_pages(&state);
    // Light pages first, whatever the style.
    if view.is_dark() {
        activate(&state, "win.pdf-invert");
    }
    let status = search_all(&state, "Yoneda");
    assert!(status.total >= 2, "{status:?}");
    wait_for_pages(&state);
    let (page, index) = view.current_match().unwrap();
    let matches = view.search_matches(page);
    let current = matches[index][0];
    // Another match, not on a link (hyperref draws link borders, which tint differently).
    let links = page_links(&state, page);
    let on_link = |r: (f64, f64, f64, f64)| {
        links.iter().any(|l| {
            let (x, y, w, h) = l.area;
            r.0 < x + w && x < r.0 + r.2 && r.1 < y + h && y < r.1 + r.3
        })
    };
    let other = matches
        .iter()
        .enumerate()
        .find(|(i, m)| *i != index && !on_link(m[0]))
        .map(|(_, m)| m[0]);
    // The mean colour change over each match's box: with the highlights minus without.
    let with = render(&view);
    view.clear_search_matches();
    let without = render(&view);
    let status = search_all(&state, "Yoneda");
    assert!(status.total >= 2);
    view.set_current_match(Some((page, index)));
    // Blending a colour c with alpha a over grey paper or ink changes red minus green by
    // a × (c.red − c.green) and green minus blue by a × (c.green − c.blue), whatever the grey:
    // orange (255, 120, 0) at 0.42 gives about +57 and +50, yellow (246, 211, 45) at 0.40 about
    // +14 and +66. Averaged over each match's box: (red − green, green − blue).
    let delta = |r: (f64, f64, f64, f64)| {
        let (x0, y0) = pdf_view::view_point_of(&view, page, r.0, r.1).unwrap();
        let (x1, y1) = pdf_view::view_point_of(&view, page, r.0 + r.2, r.1 + r.3).unwrap();
        let mut sum = (0.0f64, 0.0f64);
        let mut n = 0.0;
        for y in y0.ceil() as i32..y1.floor() as i32 {
            for x in x0.ceil() as i32..x1.floor() as i32 {
                let (a, b) = (
                    pixel(&without.2, without.0, x, y),
                    pixel(&with.2, with.0, x, y),
                );
                // BGRA.
                let rg = |p: [u8; 4]| f64::from(p[2]) - f64::from(p[1]);
                let gb = |p: [u8; 4]| f64::from(p[1]) - f64::from(p[0]);
                sum.0 += rg(b) - rg(a);
                sum.1 += gb(b) - gb(a);
                n += 1.0;
            }
        }
        assert!(n > 0.0);
        (sum.0 / n, sum.1 / n)
    };
    let d = delta(current);
    println!(
        "  current match: red−green +{:.1}, green−blue +{:.1}",
        d.0, d.1
    );
    assert!(
        d.0 > 45.0 && d.0 < 70.0 && d.1 > 38.0 && d.1 < 62.0,
        "orange: {d:?}"
    );
    if let Some(other) = other
        && view.visible_pages().contains(&page)
    {
        let d = delta(other);
        println!(
            "  other match: red−green +{:.1}, green−blue +{:.1}",
            d.0, d.1
        );
        assert!(
            d.0 > 4.0 && d.0 < 25.0 && d.1 > 54.0 && d.1 < 80.0,
            "yellow: {d:?}"
        );
    }
    review_snapshot(&state.window, "m3-pdf-search-light");
    activate(&state, "win.pdf-invert");
    assert!(view.is_dark());
    let (width, _, pixels) = render(&view);
    let (x, y) =
        pdf_view::view_point_of(&view, page, current.0, current.1 + current.3 / 2.0).unwrap();
    let px = pixel(&pixels, width, (x - 0.5).floor() as i32, y as i32);
    println!("  current match margin on a dark page {px:?}");
    // Orange over the dark paper: red well above blue.
    assert!(
        f64::from(px[2]) > f64::from(px[0]) + 40.0,
        "still orange: {px:?}"
    );
    review_snapshot(&state.window, "m3-pdf-search-dark");
    state.window.destroy();
}

/// Acceptance: thumbnails are rendered only for the rows on screen, at their width, within
/// their budget inside the shared cache; the current page is selected; a click goes to its
/// page; hiding the strip gives everything back.
fn thumbnails() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("thumbs");
    let state = window(&dir);
    let view = show_pdf(&state, &pdf);
    let total = view.cache_cap();
    assert_eq!(total, 48 * MIB);
    assert_eq!(sidebar::thumbnail_bytes(&state), Some((0, 6 * MIB)));
    // Hidden strip: nothing rendered.
    assert!(
        sidebar::thumbnail_rows(&state)
            .iter()
            .all(|(_, ready)| !ready)
    );

    activate(&state, "win.toggle-pdf-sidebar");
    assert!(state.pdf_sidebar.split.shows_sidebar());
    assert_eq!(
        sidebar::visible_page(&state),
        Some(sidebar::Page::Thumbnails)
    );
    common::pump_until(TIMEOUT, || sidebar::is_ready(&state));
    assert_eq!(view.cache_cap(), total - sidebar::thumbnail_budget(total));
    let rows = sidebar::thumbnail_rows(&state);
    println!("  {} rows bound: {rows:?}", rows.len());
    assert!(rows.len() < 20, "lazy: only the rows on screen");
    assert_eq!(rows.first().map(|r| r.0), Some(0));
    let (used, budget) = sidebar::thumbnail_bytes(&state).unwrap();
    assert!(used > 0 && used <= budget);
    assert_eq!(sidebar::thumbnail_labels(&state).len(), 50);
    assert_eq!(sidebar::thumbnail_labels(&state)[9], "10");
    review_snapshot(&state.window, "m3-pdf-thumbnails");

    // The current page is selected and kept in view.
    let (list, selection) = sidebar::thumbnail_list(&state).unwrap();
    view.goto_page(30);
    wait_for_pages(&state);
    common::pump_until(TIMEOUT, || selection.selected() == view.current_page());
    common::pump_until(TIMEOUT, || {
        let rows = sidebar::thumbnail_rows(&state);
        rows.iter().any(|(p, _)| *p == view.current_page()) && sidebar::is_ready(&state)
    });
    // Scrolling the whole list stays within the budget.
    let mut peak = 0;
    for page in (0..50).step_by(4).chain(std::iter::once(49)) {
        list.scroll_to(page, gtk::ListScrollFlags::NONE, None);
        common::pump_until(TIMEOUT, || {
            sidebar::thumbnail_rows(&state)
                .iter()
                .any(|(p, _)| *p == page)
                && sidebar::is_ready(&state)
        });
        let (used, budget) = sidebar::thumbnail_bytes(&state).unwrap();
        assert!(used <= budget, "{used} > {budget}");
        assert!(view.cache_used() <= view.cache_cap() || view.cache_used() < 12 * MIB);
        peak = peak.max(used);
    }
    println!(
        "  thumbnail cache peak {:.2} MiB of {} MiB",
        peak as f64 / MIB as f64,
        budget / MIB
    );

    // Selecting a thumbnail goes to its page.
    selection.select_item(42, true);
    common::pump_until(TIMEOUT, || view.anchor().is_some_and(|a| a.page == 42));
    // It stays selected (the page the toolbar reports near the end may be the next one).
    common::pump_for(Duration::from_millis(100));
    assert_eq!(selection.selected(), 42);
    // A rebuild keeps the thumbnails until the new ones come.
    let controller = state.pdf.get().unwrap();
    let generation = controller.generation();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    assert!(
        sidebar::thumbnail_rows(&state).iter().all(|(p, _)| *p < 50),
        "rows stay"
    );
    common::pump_until(TIMEOUT, || sidebar::is_ready(&state));

    // Hiding the strip drops the thumbnails and gives the view its whole cache back.
    activate(&state, "win.toggle-pdf-sidebar");
    assert_eq!(view.cache_cap(), total);
    assert_eq!(sidebar::thumbnail_bytes(&state).unwrap().0, 0);
    common::pump_for(Duration::from_millis(100));
    assert!(
        sidebar::thumbnail_rows(&state)
            .iter()
            .all(|(_, ready)| !ready)
    );
    state.window.destroy();
}

/// The bound thumbnail rows need more than the thumbnails' budget (`pdf.cache_mib = 1`: 128
/// KiB, two thumbnails at this width; a HiDPI screen with the default budget is the same): the
/// rows keep their thumbnails, the strip settles and nothing more is rendered, and scrolling it
/// gives back the thumbnails of the rows that left.
fn thumbnails_beyond_the_budget() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("thumbs-budget");
    let state = window_with(&dir, false, |config| config.pdf.cache_mib = 1);
    show_pdf(&state, &pdf);
    activate(&state, "win.toggle-pdf-sidebar");
    common::pump_until(TIMEOUT, || sidebar::is_ready(&state));
    let rows = sidebar::thumbnail_rows(&state);
    let (used, budget) = sidebar::thumbnail_bytes(&state).unwrap();
    println!(
        "  {} rows bound: {used} bytes of thumbnails, budget {budget}",
        rows.len()
    );
    assert_eq!(budget, 128 << 10);
    assert!(used > budget, "the bound rows need more than the budget");
    let arrived = sidebar::thumbnails_arrived(&state);
    common::pump_for(Duration::from_secs(1));
    assert_eq!(
        sidebar::thumbnails_arrived(&state),
        arrived,
        "a settled strip renders nothing"
    );
    assert!(sidebar::is_ready(&state));

    // The rows bound at the end settle too; the thumbnails of the rows that left are evicted.
    let (list, _) = sidebar::thumbnail_list(&state).unwrap();
    list.scroll_to(49, gtk::ListScrollFlags::NONE, None);
    common::pump_until(TIMEOUT, || {
        sidebar::thumbnail_rows(&state)
            .iter()
            .any(|(p, _)| *p == 49)
            && sidebar::is_ready(&state)
    });
    let rows_end = sidebar::thumbnail_rows(&state);
    let (used_end, _) = sidebar::thumbnail_bytes(&state).unwrap();
    println!(
        "  {} rows bound at the end: {used_end} bytes",
        rows_end.len()
    );
    let each = used / rows.len();
    assert!(
        used_end <= (rows_end.len() + 2) * each,
        "{used_end} bytes for {} rows",
        rows_end.len()
    );
    let arrived = sidebar::thumbnails_arrived(&state);
    common::pump_for(Duration::from_millis(500));
    assert_eq!(sidebar::thumbnails_arrived(&state), arrived);
    state.window.destroy();
}

/// The bookmark tree of `proj` as `depth title page` lines (1-based pages).
fn bookmark_text(rows: &[(u32, String, Option<u32>)]) -> String {
    rows.iter()
        .map(|(depth, title, page)| {
            format!(
                "{}{title} → {}\n",
                "  ".repeat(*depth as usize),
                page.map_or("-".into(), |p| (p + 1).to_string())
            )
        })
        .collect()
}

/// Acceptance: the bookmark tree of `proj` matches the golden tree, expands, goes to its
/// entries (recorded for Back), follows the view, and keeps its expansion across a rebuild.
fn bookmarks() {
    let dir = copy_proj("bookmarks");
    let pdf = dir.join("main.pdf");
    let state = window(&dir);
    let view = show_pdf(&state, &pdf);
    state.pdf_sidebar.split.set_show_sidebar(true);
    sidebar::show_page(&state, sidebar::Page::Bookmarks);
    common::pump_until(TIMEOUT, || sidebar::bookmarks_loaded(&state));
    let (rows, _) = sidebar::bookmark_rows(&state);
    let collapsed = bookmark_text(&rows);
    print!("{collapsed}");
    assert_eq!(
        collapsed,
        "1. Introduction → 1\n2. The Yoneda lemma → 1\n3. Descent → 2\n\
         4. Sheaves on sites → 2\nReferences → 5\n"
    );
    let sites = rows
        .iter()
        .position(|r| r.1 == "4. Sheaves on sites")
        .unwrap() as u32;
    sidebar::set_bookmark_expanded(&state, sites, true);
    let (rows, _) = sidebar::bookmark_rows(&state);
    let expanded = bookmark_text(&rows);
    let golden =
        std::fs::read_to_string(repo().join("crates/lemma-pdf/tests/golden/proj-outline.txt"))
            .expect("the outline golden of lemma-pdf");
    // The same tree as lemma-pdf's golden file, as the strip shows it.
    let from_golden: String = golden
        .lines()
        .map(|line| {
            let depth = (line.len() - line.trim_start().len()) / 2;
            let (title, dest) = line.trim_start().split_once(" -> ").unwrap();
            let page = dest.split_whitespace().nth(1).unwrap();
            format!("{}{title} → {page}\n", "  ".repeat(depth))
        })
        .collect();
    print!("{expanded}");
    assert_eq!(expanded, from_golden);
    review_snapshot(&state.window, "m3-pdf-bookmarks");

    // Selecting "4.3. Filler subsection 2" goes there, and Back returns.
    let row = rows
        .iter()
        .position(|r| r.1 == "4.3. Filler subsection 2")
        .unwrap() as u32;
    view.goto_page(0);
    wait_for_pages(&state);
    let before = view.anchor().unwrap();
    sidebar::select_bookmark(&state, row);
    common::pump_until(TIMEOUT, || view.anchor().is_some_and(|a| a.page == 2));
    assert_eq!(links::history_len(&state), 1);
    // The view follows: the selected row is that entry's.
    common::pump_until(TIMEOUT, || sidebar::bookmark_rows(&state).1 == Some(row));
    assert!(links::back(&state));
    assert_eq!(view.anchor().unwrap().page, before.page);
    common::pump_until(TIMEOUT, || {
        sidebar::bookmark_rows(&state).1.is_none_or(|r| r < sites)
    });

    // A rebuild keeps the expansion.
    let controller = state.pdf.get().unwrap();
    let generation = controller.generation();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    common::pump_until(TIMEOUT, || sidebar::bookmarks_loaded(&state));
    assert_eq!(bookmark_text(&sidebar::bookmark_rows(&state).0), expanded);
    // A PDF without bookmarks says so.
    controller.open(
        &common::fixtures()
            .join("pdf/stress.pdf")
            .canonicalize()
            .unwrap(),
    );
    common::pump_until(TIMEOUT, || {
        controller.pdf().is_some_and(|p| p.ends_with("stress.pdf"))
    });
    common::pump_until(TIMEOUT, || sidebar::bookmarks_loaded(&state));
    assert!(sidebar::bookmark_rows(&state).0.is_empty());
    state.window.destroy();
}

/// Acceptance: detaching moves the view into its own window and back, keeping the anchor; in
/// the PDF window search, SyncTeX, dark pages and reloads (with their anchor) keep working;
/// closing it re-attaches; showing the pane meanwhile brings it forward.
fn detach_reattach() {
    let Some(pdf50) = pdf50() else {
        return;
    };
    let dir = common::temp_dir("detach");
    let pdf = dir.join("pdf50.pdf");
    std::fs::copy(&pdf50, &pdf).unwrap();
    let synctex = pdf50.with_extension("synctex.gz");
    let tex = pdf50.with_extension("tex");
    std::fs::copy(&synctex, dir.join("pdf50.synctex.gz")).unwrap();
    let state = window(&dir);
    let view = show_pdf(&state, &pdf);
    let controller = state.pdf.get().unwrap();
    let main_window: gtk::Window = state.window.clone().upcast();
    view.set_scroll_position(0.0, 0.0);
    view.goto_page(30);
    let layout = view.layout().unwrap();
    let y = view.scroll_position().1 + 0.4 * layout.page_size(30).unwrap().1 * view.zoom();
    view.set_scroll_position(0.0, y);
    wait_for_pages(&state);
    let anchor = view.anchor().unwrap();
    println!("  anchor before: {anchor:?}");

    let close = |a: lemma_pdf::Anchor, b: lemma_pdf::Anchor| {
        a.page == b.page && (a.frac - b.frac).abs() < 0.01
    };
    // Detaching while the focus is in the PDF pane (the page entry) moves it out first.
    let page_entry = controller
        .toolbar()
        .observe_children()
        .into_iter()
        .filter_map(|w| w.ok()?.downcast::<gtk::Entry>().ok())
        .next()
        .expect("the page entry");
    page_entry.grab_focus();
    common::pump_until(TIMEOUT, || focus_within(&state.window, &page_entry));
    activate(&state, "win.pdf-detach");
    assert!(detach::is_detached(&state));
    let action_state = |state: &WindowState| {
        state
            .window
            .lookup_action("pdf-detach")
            .unwrap()
            .state()
            .and_then(|v| v.get::<bool>())
    };
    assert_eq!(action_state(&state), Some(true));
    let window = detach::window(&state).expect("the PDF window");
    common::pump_until(TIMEOUT, || window.is_mapped());
    assert!(!focus_within(&state.window, &page_entry));
    assert!(
        focus_within(&window, &view),
        "the PDF window's view has the focus"
    );
    assert_eq!(
        view.root().map(|r| r.upcast::<gtk::Widget>()),
        Some(window.clone().upcast())
    );
    assert!(!state.pdf_pane.is_visible());
    wait_for_pages(&state);
    common::pump_for(Duration::from_millis(200));
    let detached = view.anchor().unwrap();
    println!(
        "  anchor detached: {detached:?} (view {}x{})",
        view.width(),
        view.height()
    );
    assert!(close(anchor, detached), "{anchor:?} vs {detached:?}");
    review_snapshot(&window, "m3-pdf-detached");

    // The toolbar's actions work in the PDF window.
    let dark = view.is_dark();
    WidgetExt::activate_action(&view, "win.pdf-invert", None).expect("shared");
    assert_eq!(view.is_dark(), !dark);
    WidgetExt::activate_action(&view, "win.pdf-invert", None).unwrap();
    assert_eq!(view.is_dark(), dark);
    assert!(
        WidgetExt::activate_action(&view, "win.close-tab", None).is_err(),
        "editing actions stay in the main window"
    );
    // Search.
    search::show(&state);
    let entry = search::entry(&state).unwrap();
    assert_eq!(
        entry.root().map(|r| r.upcast::<gtk::Widget>()),
        Some(window.clone().upcast())
    );
    let status = search_all(&state, "Compare");
    assert_eq!(status.total, 50);
    search::close(&state);
    // SyncTeX forward search highlights in the detached view.
    let line = std::fs::read_to_string(&tex)
        .unwrap()
        .lines()
        .position(|l| l.contains("\\section{Section 12}"))
        .unwrap() as u32
        + 2;
    // The SyncTeX file records the fixture's own source path.
    controller.forward_search(&tex.canonicalize().unwrap(), line);
    common::pump_until(TIMEOUT, || view.highlight().is_some());
    assert_eq!(view.highlight().unwrap().0, 11);
    wait_for_pages(&state);
    // A reload keeps the anchor.
    view.goto_page(20);
    wait_for_pages(&state);
    let before_reload = view.anchor().unwrap();
    let generation = controller.generation();
    controller.open(&pdf);
    common::pump_until(TIMEOUT, || controller.generation() > generation);
    common::pump_for(Duration::from_millis(50));
    assert!(close(before_reload, view.anchor().unwrap()));
    wait_for_pages(&state);

    // Showing the pane brings the PDF window forward and keeps the pane hidden.
    state.pdf_pane.set_visible(true);
    common::pump_until(TIMEOUT, || !state.pdf_pane.is_visible());
    assert!(detach::is_detached(&state));

    // Re-attaching keeps the anchor.
    view.goto_page(7);
    wait_for_pages(&state);
    let before_attach = view.anchor().unwrap();
    activate(&state, "win.pdf-detach");
    assert!(!detach::is_detached(&state));
    assert_eq!(action_state(&state), Some(false));
    assert!(state.pdf_pane.is_visible());
    assert_eq!(
        view.root().map(|r| r.upcast::<gtk::Widget>()),
        Some(main_window.clone().upcast())
    );
    common::pump_until(TIMEOUT, || !window.is_visible());
    wait_for_pages(&state);
    common::pump_for(Duration::from_millis(200));
    let attached = view.anchor().unwrap();
    println!("  anchor attached: {attached:?}");
    assert!(
        close(before_attach, attached),
        "{before_attach:?} vs {attached:?}"
    );

    // Closing the PDF window re-attaches the view.
    println!("  closing the PDF window");
    activate(&state, "win.pdf-detach");
    let window = detach::window(&state).unwrap();
    common::pump_until(TIMEOUT, || window.is_mapped());
    window.close();
    common::pump_until(TIMEOUT, || !detach::is_detached(&state));
    assert_eq!(action_state(&state), Some(false));
    assert!(state.pdf_pane.is_visible());
    assert!(view.is_ancestor(&state.pdf_pane));
    wait_for_pages(&state);

    // Closing the main window closes the PDF window too.
    println!("  closing the main window");
    activate(&state, "win.pdf-detach");
    let window = detach::window(&state).unwrap();
    common::pump_until(TIMEOUT, || window.is_mapped());
    state.window.destroy();
    common::pump_until(TIMEOUT, || !window.is_visible());
}

/// `--demo pdf-search --snapshot` searches pdf50 and shows the matches (`l-pdf-search.png`);
/// with `LEMMA_DEMO_PDF_STRIP=thumbnails` and no query it shows the strip
/// (`l-pdf-thumbs.png`).
fn demo_pdf_search() {
    if pdf50().is_none() {
        return;
    }
    let dir = common::temp_dir("demo");
    for (name, env) in [
        ("search", vec![]),
        (
            "thumbs",
            vec![(search::QUERY_ENV, ""), (search::STRIP_ENV, "thumbnails")],
        ),
    ] {
        let png = dir.join(format!("{name}.png"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_lemma"));
        command
            .arg(repo().join("target/fixtures/pdf50.tex"))
            .args([
                "--demo",
                "pdf-search",
                "--quit-after-ms",
                "3000",
                "--snapshot",
            ])
            .arg(&png)
            .env("LEMMA_CONFIG", "/dev/null")
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_DATA_HOME", dir.join("data"));
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().expect("run lemma");
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "lemma failed: {:?}", output.status);
        assert!(stdout.contains("demo pdf-search:"), "{stdout}");
        if name == "search" {
            assert!(
                stdout.contains("\"nulla\":") && stdout.contains("1 of"),
                "{stdout}"
            );
        } else {
            assert!(stdout.contains("strip: thumbnails of pages 1–"), "{stdout}");
        }
        let texture = gdk::Texture::from_filename(&png).expect("the PNG loads");
        assert!(texture.width() > 0);
    }
}

// ----- real key events -----

mod real_keys {
    //! The PDF's keys on real key events (a private Broadway daemon, see `tests/broadway_input`):
    //! `/`, Enter, `n`, `N` and Escape (Vim style), Ctrl+F, Enter and Shift+Enter (GNOME
    //! style), Alt+Left after a link jump, and the detached window's Ctrl+F and Alt+Left (and
    //! no Ctrl+W there).

    use super::*;
    use broadway_input::{ALT, Browser, CONTROL, SHIFT};

    pub fn parent() {
        broadway_input::run_child("pdf_m3_real_keys_child", 34);
    }

    fn current(state: &WindowState) -> Option<usize> {
        search::status(state).and_then(|s| s.current)
    }

    pub fn child() {
        let dir = copy_proj("keys");
        let state = window(&dir);
        // The accelerators the application installs at start-up.
        lemma_app::app::set_accels(&state.app, &lemma_config::KeysCfg::default());
        let mut browser = Browser::connect_drawing();
        browser.settle = Duration::from_millis(20);
        let editor = glib::MainContext::default()
            .block_on(state.open_file(&dir.join("main.tex")))
            .expect("open main.tex");
        let view = show_pdf(&state, &dir.join("main.pdf"));
        view.goto_page(0);
        wait_for_pages(&state);
        assert!(view.grab_focus());
        common::pump_until(TIMEOUT, || view.has_focus());

        println!("child: / and n, N, Escape in the view");
        browser.key(gdk::Key::slash, 0);
        let entry = search::entry(&state).unwrap();
        common::pump_until(TIMEOUT, || focus_within(&state.window, &entry));
        browser.text("yoneda");
        browser.key(gdk::Key::Return, 0);
        common::pump_until(TIMEOUT, || {
            search::status(&state).is_some_and(|s| s.done && s.query.as_deref() == Some("yoneda"))
        });
        let total = search::status(&state).unwrap().total;
        assert!(total >= 3, "{total}");
        common::pump_until(TIMEOUT, || view.has_focus());
        assert_eq!(current(&state), Some(1));
        browser.key(gdk::Key::n, 0);
        common::pump_until(TIMEOUT, || current(&state) == Some(2));
        browser.key(gdk::Key::n, 0);
        common::pump_until(TIMEOUT, || current(&state) == Some(3));
        browser.key(gdk::Key::N, SHIFT);
        common::pump_until(TIMEOUT, || current(&state) == Some(2));
        browser.key(gdk::Key::Escape, 0);
        common::pump_until(TIMEOUT, || !search::status(&state).unwrap().open);
        assert!(view.pages_with_matches().is_empty());
        assert!(view.has_focus());
        // Without a search, n does nothing in the view.
        let anchor = view.anchor();
        browser.key(gdk::Key::n, 0);
        common::pump_for(Duration::from_millis(50));
        assert_eq!(view.anchor(), anchor);

        println!("child: Ctrl+F, Enter and Shift+Enter in the entry");
        browser.key(gdk::Key::f, CONTROL);
        common::pump_until(TIMEOUT, || focus_within(&state.window, &entry));
        browser.text("lemma");
        browser.key(gdk::Key::Return, 0);
        common::pump_until(TIMEOUT, || {
            search::status(&state).is_some_and(|s| s.done && s.query.as_deref() == Some("lemma"))
        });
        assert_eq!(current(&state), Some(1));
        browser.key(gdk::Key::Return, 0);
        common::pump_until(TIMEOUT, || current(&state) == Some(2));
        assert!(
            focus_within(&state.window, &entry),
            "the focus stays in the entry"
        );
        browser.key(gdk::Key::Return, SHIFT);
        common::pump_until(TIMEOUT, || current(&state) == Some(1));
        browser.key(gdk::Key::Escape, 0);
        common::pump_until(TIMEOUT, || !search::status(&state).unwrap().open);
        common::pump_until(TIMEOUT, || view.has_focus());

        println!("child: Alt+Left after a link jump");
        let links = page_links(&state, 0);
        let link = links
            .iter()
            .find(|l| matches!(l.dest, LinkDest::Page { page, .. } if page > 0))
            .expect("a link to another page");
        let before = view.anchor().unwrap();
        let (x, y) = centre(&view, 0, link.area);
        assert!(links::click(&state, x, y));
        assert_ne!(view.anchor().unwrap().page, before.page);
        browser.key(gdk::Key::Left, ALT);
        common::pump_until(TIMEOUT, || links::history_len(&state) == 0);
        assert_eq!(view.anchor().unwrap().page, before.page);

        println!("child: the detached window");
        WidgetExt::activate_action(&state.window, "win.pdf-detach", None).unwrap();
        let window = detach::window(&state).unwrap();
        common::pump_until(TIMEOUT, || window.is_mapped());
        window.present();
        common::pump_until(TIMEOUT, || window.is_active());
        assert!(view.grab_focus());
        common::pump_until(TIMEOUT, || view.has_focus());
        browser.key(gdk::Key::f, CONTROL);
        common::pump_until(TIMEOUT, || focus_within(&window, &entry));
        assert!(search::status(&state).unwrap().open);
        browser.key(gdk::Key::Escape, 0);
        common::pump_until(TIMEOUT, || !search::status(&state).unwrap().open);
        common::pump_until(TIMEOUT, || view.has_focus());
        view.goto_page(0);
        wait_for_pages(&state);
        let before = view.anchor().unwrap();
        let (x, y) = centre(&view, 0, link.area);
        assert!(links::click(&state, x, y));
        assert_eq!(links::history_len(&state), 1);
        assert_ne!(view.anchor().unwrap().page, before.page);
        browser.key(gdk::Key::Left, ALT);
        common::pump_until(TIMEOUT, || links::history_len(&state) == 0);
        assert_eq!(view.anchor().unwrap().page, before.page);
        // The editing keys stay in the main window: Ctrl+W closes no tab.
        browser.key(gdk::Key::w, CONTROL);
        common::pump_for(Duration::from_millis(100));
        assert_eq!(state.editors().len(), 1);
        assert!(state.editors()[0].document().path() == editor.document().path());
        state.window.destroy();
        common::pump_until(TIMEOUT, || !window.is_visible());
    }
}
