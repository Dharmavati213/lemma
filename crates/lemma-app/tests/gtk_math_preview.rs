//! WP3.3: the math hover preview in the editor (`editor/math_preview.rs`), with real TeX: the
//! hover on inline, display, `align*` and `tikzcd` formulas of the fixture project, the root's
//! preamble and macros for a chapter, errors and timeouts, the format following the live
//! preamble, the cache bound, latest-wins, the off switch, light and dark, the live preview,
//! detaching, the keyboard, and that nothing happens before the first hover.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_math_preview`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `k-math-preview-light.png` and `k-math-preview-dark.png`
//! (the window with a hover) and `k-math-live.png` (the live preview) there. The latency test:
//! `scripts/broadway.sh cargo test --release -p lemma-app --test gtk_math_preview -- --ignored perf`.

mod common;

use adw::prelude::*;
use lemma_app::editor::hover;
use lemma_app::editor::math_preview::{self, FormulaPicture};
use lemma_app::editor::{Editor, Trigger};
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let mut tests: Vec<common::Test> = vec![
        ("nothing_happens_before_the_first_hover", unused),
        ("hover_renders_inline_display_align_and_tikzcd", kinds),
        ("chapters_use_the_root_preamble_and_their_macros", chapter),
        ("errors_are_shown_and_never_hang", errors),
        (
            "the_format_follows_the_live_preamble",
            format_follows_preamble,
        ),
        ("the_cache_is_bounded_and_answers_at_once", cache_bound),
        ("the_latest_request_wins", latest_wins),
        ("math_hover_off_and_other_documents", off_switch),
        ("pictures_follow_the_light_and_dark_style", light_and_dark),
        ("keyboard_hover_at_the_cursor", keyboard),
        ("live_preview_below_display_math", live),
        ("detach_removes_everything", detach),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_math_preview_hover_latency", perf_latency));
    }
    common::run(&tests)
}

/// Long enough for a format build on a loaded machine.
const TIMEOUT: Duration = Duration::from_secs(60);
const FIXTURE_FILES: [&str; 3] = ["main.tex", "refs.bib", "chapters/yoneda.tex"];
/// Added to the fixture's main.tex before `\printbibliography`.
const EXTRA: &str = "\\subsection{Previews}
\\begin{align*}
a &= b + c \\\\
  &= \\frac{d}{e}
\\end{align*}
Inline $u_1$, $u_2$, $u_3$, $u_4$, $u_5$, $u_6$ and $\\undefinedmacro x$, $\\frac{1}{$ end.
Loop $\\def\\lemmaloop{\\lemmaloop}\\lemmaloop$ here.
";

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn tex_available() -> bool {
    let ok = std::process::Command::new("kpsewhich")
        .arg("mylatexformat.ltx")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        println!("    SKIP: pdflatex or mylatexformat is not installed");
    }
    ok
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
    cache: PathBuf,
}

fn fixture(name: &str, configure: impl FnOnce(&mut Config)) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    let proj = dir.join("proj");
    for file in FIXTURE_FILES {
        let to = proj.join(file);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(common::fixtures().join("proj").join(file), &to).unwrap();
    }
    let main = proj.join("main.tex");
    let text = std::fs::read_to_string(&main).unwrap();
    std::fs::write(
        &main,
        text.replacen(
            "\\printbibliography",
            &format!("{EXTRA}\\printbibliography"),
            1,
        ),
    )
    .unwrap();
    let cache = dir.join("cache");
    math_preview::set_cache_dir(Some(cache.clone()));
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.preview.delay_ms = 60;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&proj).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 760);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir: proj,
        cache,
    }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = self.open_plain(file);
        let doc = editor.document().clone();
        common::pump_until(TIMEOUT, || doc.has_line_states());
        editor
    }

    /// Opens `file` without waiting for scanner states (documents that are not LaTeX).
    fn open_plain(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        assert!(block_on(testmode::wait_for_frames(&self.state.window, 1)));
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        normalize(&self.dir.join(file))
    }

    fn set_config(&self, change: impl FnOnce(&mut Config)) {
        let mut config = (*self.state.config.get()).clone();
        change(&mut config);
        self.state.set_config(Rc::new(config));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn offset_of(editor: &Editor, needle: &str, delta: usize) -> usize {
    editor
        .document()
        .text()
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
        + delta
}

/// Shows `offset` on screen (the popover points into the view).
fn reveal(f: &Fixture, editor: &Editor, offset: usize) {
    let mut iter = editor.iter_near(offset).unwrap();
    editor.view().scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.5);
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
}

fn is_error(w: &gtk::Widget) -> bool {
    w.has_css_class("lemma-math-error")
}

/// The texts of the labels in `widget` (an error box).
fn texts(widget: &gtk::Widget) -> String {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone()];
    while let Some(w) = stack.pop() {
        if let Some(label) = w.downcast_ref::<gtk::Label>() {
            out.push(label.text().to_string());
        }
        let mut child = w.last_child();
        while let Some(c) = child {
            stack.push(c.clone());
            child = c.prev_sibling();
        }
    }
    out.join(" | ")
}

/// Hovers `offset` (as the pointer does) and waits for the rendered formula or its error.
fn hover_at(f: &Fixture, editor: &Editor, offset: usize) -> gtk::Widget {
    reveal(f, editor, offset);
    assert!(
        hover::show_at(editor, offset, Trigger::Pointer),
        "a hover at {offset}"
    );
    wait_shown(editor)
}

fn wait_shown(editor: &Editor) -> gtk::Widget {
    let view = editor.view();
    common::pump_until(TIMEOUT, || {
        hover::shown_widget(view).is_some_and(|w| w.is::<FormulaPicture>() || is_error(&w))
    });
    hover::shown_widget(view).unwrap()
}

fn picture(w: &gtk::Widget) -> Rc<math_preview::Picture> {
    let p = w
        .downcast_ref::<FormulaPicture>()
        .unwrap_or_else(|| panic!("a formula, not {}", texts(w)));
    p.picture().unwrap()
}

/// The resident set size of this process in kB.
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

// ----- tests -----

fn unused() {
    let f = fixture("unused", |_| {});
    let editor = f.open("main.tex");
    common::pump_for(Duration::from_millis(300));
    let before = rss_kb();
    let view = editor.view();
    assert!(
        hover::provider_count(view) >= 1,
        "the provider is registered"
    );
    // Typing and hovering text: no formula, nothing loads.
    let at = offset_of(&editor, "Lorem ipsum", 0);
    editor.buffer().place_cursor(&editor.iter_near(at).unwrap());
    editor.buffer().insert_at_cursor("Some words ");
    reveal(&f, &editor, at + 3);
    assert!(!hover::show_at(&editor, at + 3, Trigger::Pointer));
    for word in ["Introduction", "scheme and"] {
        let at = offset_of(&editor, word, 1);
        reveal(&f, &editor, at);
        assert!(!hover::show_at(&editor, at, Trigger::Pointer), "{word}");
    }
    common::pump_for(Duration::from_millis(300));
    let after = rss_kb();
    println!("    RSS {before} kB → {after} kB: typing and hovering text, no formula hovered");
    assert!(after < before + 8 * 1024, "{before} → {after} kB");
    assert!(
        !math_preview::is_loaded(),
        "no pipeline before the first hover"
    );
    assert_eq!(math_preview::stats(), math_preview::Stats::default());
    assert!(!f.cache.exists(), "no cache directory either");
    // The first hover over a formula loads it.
    if !tex_available() {
        return;
    }
    let w = hover_at(&f, &editor, offset_of(&editor, "\\cO_X", 1));
    picture(&w);
    assert!(math_preview::is_loaded());
    assert!(f.cache.is_dir());
}

fn kinds() {
    if !tex_available() {
        return;
    }
    let f = fixture("kinds", |_| {});
    let editor = f.open("main.tex");
    let view = editor.view().clone();
    let text = editor.document().text();
    let cases = [
        ("inline", "\\cO_X", "$\\cO_X$"),
        ("display", "\\mathrm{QCoh}(S)\\longrightarrow", "\\["),
        ("align*", "&= \\frac{d}{e}", "\\begin{align*}"),
        ("align", "\\lim_{U\\in\\cC}", "\\begin{align}"),
        ("tikzcd", "shift left", "\\["),
    ];
    let mut sizes = Vec::new();
    for (name, needle, starts) in cases {
        let w = hover_at(&f, &editor, offset_of(&editor, needle, 2));
        let p = picture(&w);
        assert!(p.mask, "{name}: black ink is a mask");
        let (range, trigger) = hover::shown_range(&view).unwrap();
        assert_eq!(trigger, Trigger::Pointer);
        let source = &text[range];
        assert!(
            source.starts_with(starts),
            "{name}: the whole formula, {source:?}"
        );
        assert!(source.contains(needle));
        assert_eq!(
            w.accessible_role(),
            gtk::AccessibleRole::Img,
            "{name}: an image for screen readers"
        );
        sizes.push((name, p.logical_size()));
        hover::hide(&view);
    }
    println!("    logical sizes: {sizes:?}");
    let size = |n: &str| sizes.iter().find(|s| s.0 == n).unwrap().1;
    assert!(
        size("inline").1 < 40.0 && size("inline").0 < 80.0,
        "{sizes:?}"
    );
    assert!(size("align").1 > size("display").1 * 2.0, "{sizes:?}");
    assert!(size("tikzcd").0 > 150.0, "{sizes:?}");
    let stats = math_preview::stats();
    assert_eq!(stats.format_builds, 1, "one format for every formula");
    assert_eq!(stats.renders, 5);
    assert_eq!(
        math_preview::last_outcome().unwrap().route,
        Some(lemma_preview::Route::Format)
    );
    // The popover never took the focus.
    assert!(!hover::popover(&view).unwrap().can_focus());
}

fn chapter() {
    if !tex_available() {
        return;
    }
    let f = fixture("chapter", |_| {});
    let main = f.path("main.tex");
    let editor = f.open("chapters/yoneda.tex");
    // The project index finds the root that includes the chapter.
    let doc = editor.document().clone();
    common::pump_until(TIMEOUT, || {
        math_preview::root_for(&f.state, &doc).as_deref() == Some(main.as_path())
    });
    for needle in [
        "h_X(X) \\arrow",
        "\\Hom_{\\mathrm{PSh}(\\cC)}",
        "\\mathrm{id}_X$ gives",
    ] {
        let w = hover_at(&f, &editor, offset_of(&editor, needle, 1));
        picture(&w);
    }
    // A macro the chapter defines itself.
    let buffer = editor.buffer();
    buffer.insert(
        &mut buffer.start_iter(),
        "\\newcommand{\\yo}{\\mathbf{y}}\n",
    );
    let at = offset_of(&editor, "This is used", 0);
    buffer.insert(
        &mut editor.iter_near(at).unwrap(),
        "The embedding $\\yo\\colon\\cC\\to\\mathrm{PSh}(\\cC)$.\n",
    );
    common::pump_until(TIMEOUT, || doc.has_line_states());
    let w = hover_at(&f, &editor, offset_of(&editor, "\\yo\\colon", 1));
    assert!(picture(&w).logical_size().0 > 60.0);
}

fn errors() {
    if !tex_available() {
        return;
    }
    let f = fixture("errors", |_| {});
    let editor = f.open("main.tex");
    let w = hover_at(&f, &editor, offset_of(&editor, "\\undefinedmacro", 1));
    assert!(is_error(&w));
    let t = texts(&w);
    assert!(
        t.contains("TeX error") && t.contains("Undefined control sequence"),
        "{t}"
    );
    assert!(t.contains("\\undefinedmacro"), "the context names it: {t}");
    let w = hover_at(&f, &editor, offset_of(&editor, "\\frac{1}{$", 1));
    assert!(is_error(&w), "{}", texts(&w));
    hover::hide(editor.view());
    // An endless loop: stopped at the timeout, and the UI never waited for it.
    let started = Instant::now();
    let at = offset_of(&editor, "\\def\\lemmaloop", 1);
    reveal(&f, &editor, at);
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    let mut longest = Duration::ZERO;
    let context = glib::MainContext::default();
    while !hover::shown_widget(editor.view()).is_some_and(|w| is_error(&w)) {
        assert!(started.elapsed() < TIMEOUT);
        let t = Instant::now();
        context.iteration(false);
        longest = longest.max(t.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    let w = hover::shown_widget(editor.view()).unwrap();
    assert!(texts(&w).contains("Preview timed out"), "{}", texts(&w));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert!(
        longest < Duration::from_millis(200),
        "the main loop was blocked {longest:?}"
    );
    // Errors from TeX are cached (the same hover answers at once); the timeout is not.
    let before = math_preview::stats().renders;
    hover::hide(editor.view());
    let at = offset_of(&editor, "\\undefinedmacro", 1);
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    assert!(hover::shown_widget(editor.view()).is_some_and(|w| is_error(&w)));
    assert_eq!(math_preview::stats().renders, before);
}

fn format_follows_preamble() {
    if !tex_available() {
        return;
    }
    let f = fixture("format", |_| {});
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    picture(&hover_at(&f, &editor, offset_of(&editor, "\\cO_X", 1)));
    assert_eq!(math_preview::stats().format_builds, 1);
    // Editing the body: no new format.
    let buffer = editor.buffer();
    let at = offset_of(&editor, "Lorem ipsum", 0);
    buffer.insert(
        &mut editor.iter_near(at).unwrap(),
        "We note $\\alpha+\\beta$. ",
    );
    common::pump_until(TIMEOUT, || doc.has_line_states());
    picture(&hover_at(&f, &editor, offset_of(&editor, "\\alpha+", 1)));
    assert_eq!(math_preview::stats().format_builds, 1);
    // Editing the preamble (unsaved, in the buffer): the next render builds it again, once.
    let at = offset_of(&editor, "\\title{", 0);
    buffer.insert(
        &mut editor.iter_near(at).unwrap(),
        "\\newcommand{\\Zed}{\\mathbb{Z}}\n",
    );
    let at = offset_of(&editor, "We note", 0);
    buffer.insert(&mut editor.iter_near(at).unwrap(), "Also $\\Zed^2$. ");
    common::pump_until(TIMEOUT, || doc.has_line_states());
    let w = hover_at(&f, &editor, offset_of(&editor, "\\Zed^2", 1));
    picture(&w);
    assert_eq!(math_preview::stats().format_builds, 2);
    assert!(math_preview::last_outcome().unwrap().format_built);
    picture(&hover_at(
        &f,
        &editor,
        offset_of(&editor, "\\mathrm{QCoh}", 1),
    ));
    assert_eq!(math_preview::stats().format_builds, 2);
    assert!(f.cache.read_dir().unwrap().count() >= 1);
}

fn cache_bound() {
    if !tex_available() {
        return;
    }
    let f = fixture("cache", |c| c.preview.cache = 3);
    let editor = f.open("main.tex");
    let view = editor.view().clone();
    for i in 1..=5 {
        picture(&hover_at(
            &f,
            &editor,
            offset_of(&editor, &format!("u_{i}$"), 0),
        ));
        hover::hide(&view);
        let s = math_preview::stats();
        assert!(s.cache_entries <= 3, "{s:?}");
        assert!(s.cache_bytes <= math_preview::CACHE_BYTES);
    }
    let s = math_preview::stats();
    assert_eq!((s.renders, s.cache_entries), (5, 3));
    // A cached formula is shown at once, with no render.
    let at = offset_of(&editor, "u_5$", 0);
    reveal(&f, &editor, at);
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    assert!(
        hover::shown_widget(&view).is_some_and(|w| w.is::<FormulaPicture>()),
        "no pending state"
    );
    let s = math_preview::stats();
    assert_eq!(s.renders, 5);
    assert!(s.cache_hits >= 1);
    // The oldest was evicted: rendered again.
    hover::hide(&view);
    picture(&hover_at(&f, &editor, offset_of(&editor, "u_1$", 0)));
    assert_eq!(math_preview::stats().renders, 6);
    // The bound follows the configuration.
    f.set_config(|c| c.preview.cache = 1);
    hover::hide(&view);
    picture(&hover_at(&f, &editor, offset_of(&editor, "u_2$", 0)));
    assert_eq!(math_preview::stats().cache_entries, 1);
}

fn latest_wins() {
    if !tex_available() {
        return;
    }
    let f = fixture("latest", |_| {});
    let editor = f.open("main.tex");
    let view = editor.view().clone();
    // Hovers in a row while the first renders (with the format build): the middle ones are
    // dropped, the last is shown.
    let starts = math_preview::stats().renders;
    // One line: shown once, then hovered without waiting for frames.
    reveal(&f, &editor, offset_of(&editor, "u_1$", 0));
    for i in 1..=4 {
        let at = offset_of(&editor, &format!("u_{i}$"), 0);
        assert!(hover::show_at(&editor, at, Trigger::Pointer));
    }
    assert!(math_preview::stats().running);
    let w = wait_shown(&editor);
    picture(&w);
    let (range, _) = hover::shown_range(&view).unwrap();
    assert_eq!(&editor.document().text()[range], "$u_4$");
    common::pump_until(TIMEOUT, || !math_preview::stats().running);
    let s = math_preview::stats();
    assert_eq!(
        s.renders - starts,
        2,
        "u_1 (running) and u_4 (the latest): {s:?}"
    );
    assert!(!s.queued);
    // u_2 was never rendered: hovering it renders now.
    hover::hide(&view);
    picture(&hover_at(&f, &editor, offset_of(&editor, "u_2$", 0)));
    assert_eq!(math_preview::stats().renders - starts, 3);
}

fn off_switch() {
    let f = fixture("off", |c| c.preview.math_hover = false);
    std::fs::write(f.dir.join("notes.md"), "Some $x^2$ math in Markdown.\n").unwrap();
    let editor = f.open("main.tex");
    let view = editor.view().clone();
    let count_off = hover::provider_count(&view);
    // On the `X` (WP3.2 shows a macro's definition when the math preview is off).
    let at = offset_of(&editor, "\\cO_X", 4);
    reveal(&f, &editor, at);
    assert!(!hover::show_at(&editor, at, Trigger::Pointer), "off");
    assert!(!math_preview::is_loaded());
    // Turned on by a reload: the open tab gets the provider.
    f.set_config(|c| c.preview.math_hover = true);
    assert_eq!(hover::provider_count(&view), count_off + 1);
    if tex_available() {
        picture(&hover_at(&f, &editor, at));
    }
    f.set_config(|c| c.preview.math_hover = false);
    assert_eq!(hover::provider_count(&view), count_off);
    // Not LaTeX: no provider at all.
    f.set_config(|c| c.preview.math_hover = true);
    let md = f.open_plain("notes.md");
    let at = offset_of(&md, "x^2", 0);
    hover::show_at(&md, at, Trigger::Pointer);
    common::pump_for(Duration::from_millis(100));
    assert!(!math_preview::stats().running);
    assert!(
        hover::shown_widget(md.view()).is_none_or(|w| !w.is::<FormulaPicture>()),
        "no formula in Markdown"
    );
}

fn light_and_dark() {
    if !tex_available() {
        return;
    }
    let f = fixture("styles", |_| {});
    let editor = f.open("main.tex");
    let at = offset_of(&editor, "\\lim_{U\\in\\cC}", 2);
    let snapshots = std::env::var_os("LEMMA_TEST_SNAPSHOTS").map(PathBuf::from);
    let style = adw::StyleManager::default();
    let mut ink = Vec::new();
    for (scheme, name) in [
        (adw::ColorScheme::ForceLight, "light"),
        (adw::ColorScheme::ForceDark, "dark"),
    ] {
        style.set_color_scheme(scheme);
        hover::hide(editor.view());
        let w = hover_at(&f, &editor, at);
        assert!(picture(&w).mask);
        assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
        // The ink as drawn: the popover's text colour, through the mask.
        let shot = f.dir.join(format!("formula-{name}.png"));
        block_on(testmode::snapshot_next_frame(&w, &shot)).unwrap();
        let mut file = std::fs::File::open(&shot).unwrap();
        let surface = cairo::ImageSurface::create_from_png(&mut file).unwrap();
        ink.push((name, mean_ink_luminance(surface), luminance(&w.color())));
        if let Some(dir) = &snapshots {
            let path = dir.join(format!("k-math-preview-{name}.png"));
            block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
            println!("    saved {}", path.display());
        }
    }
    style.set_color_scheme(adw::ColorScheme::Default);
    println!("    ink luminance (drawn, text colour): {ink:?}");
    let (light, dark) = (ink[0], ink[1]);
    assert!(
        light.1 < 0.35 && light.2 < 0.35,
        "dark ink on light: {light:?}"
    );
    assert!(
        dark.1 > 0.65 && dark.2 > 0.65,
        "light ink on dark: {dark:?}"
    );
}

fn luminance(c: &gtk::gdk::RGBA) -> f32 {
    0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue()
}

/// The mean luminance of the mostly opaque pixels of a widget's snapshot.
fn mean_ink_luminance(mut surface: cairo::ImageSurface) -> f32 {
    let stride = surface.stride() as usize;
    let (w, h) = (surface.width() as usize, surface.height() as usize);
    let data = surface.data().unwrap();
    let (mut sum, mut n) = (0.0f32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let p = &data[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = f32::from(p[3]);
            if a > 128.0 {
                let (b, g, r) = (
                    f32::from(p[0]) / a,
                    f32::from(p[1]) / a,
                    f32::from(p[2]) / a,
                );
                sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
                n += 1;
            }
        }
    }
    assert!(n > 50, "the formula has ink ({n} pixels)");
    sum / n as f32
}

fn keyboard() {
    if !tex_available() {
        return;
    }
    let f = fixture("keyboard", |_| {});
    let editor = f.open("main.tex");
    // On the `X`: the keyboard on `\cO` shows its definition (WP3.2).
    let at = offset_of(&editor, "\\cO_X", 4);
    reveal(&f, &editor, at);
    editor.view().grab_focus();
    assert!(editor.place_cursor_at(at));
    assert!(hover::show_at_cursor(&f.state));
    let w = wait_shown(&editor);
    picture(&w);
    assert_eq!(
        hover::shown_range(editor.view()).unwrap().1,
        Trigger::Keyboard
    );
    assert!(editor.view().has_focus(), "the view kept the focus");
}

fn live() {
    if !tex_available() {
        return;
    }
    let f = fixture("live", |c| c.preview.live = true);
    let editor = f.open("main.tex");
    let buffer = editor.buffer().clone();
    let at = offset_of(&editor, "&= \\frac{d}{e}", 2);
    reveal(&f, &editor, at);
    assert!(editor.place_cursor_at(at));
    common::pump_until(TIMEOUT, || {
        math_preview::live_content(&editor).is_some_and(|(p, _)| p.is_some())
    });
    let (line, reserved) = math_preview::live_reserved(&editor).unwrap();
    let end_line = editor
        .iter_near(offset_of(&editor, "\\end{align*}", 0))
        .unwrap()
        .line();
    assert_eq!(line, end_line, "below the formula's last line");
    assert!(reserved > 30, "{reserved}");
    let card = math_preview::live_card(&editor).unwrap();
    assert!(card.has_css_class("card") && !card.can_target());
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") {
        let path = Path::new(&dir).join("k-math-live.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
        println!("    saved {}", path.display());
    }
    // The card sits below the text of that line, never on it.
    let view = editor.view();
    let iter = buffer.iter_at_line(line).unwrap();
    let (line_y, line_h) = view.line_yrange(&iter);
    let bounds = card.compute_bounds(view).unwrap();
    let (_, top) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, 0, bounds.y() as i32);
    assert!(
        top >= line_y + line_h - reserved,
        "{top} vs {line_y}+{line_h}-{reserved}"
    );
    // Typing: the picture follows after the pause.
    let before = math_preview::live_content(&editor)
        .unwrap()
        .0
        .unwrap()
        .picture()
        .unwrap();
    buffer.insert_at_cursor("+x");
    common::pump_until(TIMEOUT, || {
        math_preview::live_content(&editor)
            .and_then(|(p, _)| p?.picture())
            .is_some_and(|p| !Rc::ptr_eq(&p, &before))
    });
    // A formula that does not compile: the last picture stays, with the error.
    buffer.insert_at_cursor("\\undefinedlive");
    common::pump_until(TIMEOUT, || {
        math_preview::live_content(&editor).is_some_and(|(_, e)| !e.is_empty())
    });
    let (picture, error) = math_preview::live_content(&editor).unwrap();
    assert!(picture.is_some(), "the last good picture stays");
    assert!(error.contains("Undefined control sequence"), "{error}");
    // Leaving the formula hides it at once and frees the space.
    let out = offset_of(&editor, "Lorem ipsum", 0);
    assert!(editor.place_cursor_at(out));
    assert!(math_preview::live_card(&editor).is_none());
    assert!(math_preview::live_reserved(&editor).is_none());
    let tag = buffer.tag_table().lookup(math_preview::LIVE_TAG).unwrap();
    let mut start = buffer.start_iter();
    assert!(
        !start.forward_to_tag_toggle(Some(&tag)),
        "no reserved space left"
    );
    // Inline math gets a live preview too, below its line.
    let inline = offset_of(&editor, "u_3", 1);
    assert!(editor.place_cursor_at(inline));
    // Wait for the card itself: the content keeps the previous formula's picture.
    common::pump_until(TIMEOUT, || math_preview::live_reserved(&editor).is_some());
    let (inline_line, _) = math_preview::live_reserved(&editor).unwrap();
    assert_eq!(
        inline_line,
        editor.iter_near(inline).unwrap().line(),
        "below the inline formula's line"
    );
    assert!(editor.place_cursor_at(out));
    assert!(math_preview::live_card(&editor).is_none());
    // Vim's normal mode: none.
    f.state.set_vim_enabled(true);
    assert!(editor.place_cursor_at(at));
    common::pump_for(Duration::from_millis(900));
    assert!(math_preview::live_card(&editor).is_none(), "normal mode");
    f.state.set_vim_enabled(false);
    // Turned off: the card, its tag and its signals go.
    assert!(editor.place_cursor_at(at + 1));
    common::pump_until(TIMEOUT, || math_preview::live_card(&editor).is_some());
    f.set_config(|c| c.preview.live = false);
    assert!(math_preview::live_card(&editor).is_none());
    assert!(buffer.tag_table().lookup(math_preview::LIVE_TAG).is_none());
    assert!(
        !card.is_visible(),
        "the card is hidden (GTK cannot take an overlay back)"
    );
    // On again: the same card is used (never a second one).
    f.set_config(|c| c.preview.live = true);
    assert!(editor.place_cursor_at(at));
    common::pump_until(TIMEOUT, || math_preview::live_card(&editor).is_some());
    assert_eq!(math_preview::live_card(&editor).as_ref(), Some(&card));
}

fn detach() {
    if !tex_available() {
        return;
    }
    let f = fixture("detach", |c| c.preview.live = true);
    let editor = f.open("main.tex");
    let other = f.open("chapters/yoneda.tex");
    f.state
        .tab_view
        .set_selected_page(&f.state.tab_view.page(editor.widget()));
    let at = offset_of(&editor, "&= \\frac{d}{e}", 2);
    picture(&hover_at(&f, &editor, at));
    assert!(editor.place_cursor_at(at));
    common::pump_until(TIMEOUT, || math_preview::live_card(&editor).is_some());
    let card = math_preview::live_card(&editor).unwrap();
    let popover = hover::popover(editor.view()).unwrap();
    let weak_view = editor.view().downgrade();
    let page = f.state.tab_view.page(editor.widget());
    drop(editor);
    f.state.tab_view.close_page(&page);
    common::pump_for(Duration::from_millis(100));
    assert!(card.parent().is_none(), "the live card left the view");
    assert!(popover.parent().is_none());
    common::pump_until(TIMEOUT, || weak_view.upgrade().is_none());
    // The other tab still previews.
    picture(&hover_at(
        &f,
        &other,
        offset_of(&other, "h_X(X) \\arrow", 1),
    ));
}

/// p50 ≤ 250 ms from the hover to the shown picture (a built format, distinct formulas).
fn perf_latency() {
    if !tex_available() {
        return;
    }
    let f = fixture("perf", |_| {});
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer().clone();
    let at = offset_of(&editor, "Lorem ipsum", 0);
    let formulas: String = (0..25)
        .map(|i| format!("$x_{{{i}}}^2+\\cF_{{{i}}}$ "))
        .collect();
    buffer.insert(&mut editor.iter_near(at).unwrap(), &formulas);
    common::pump_until(TIMEOUT, || doc.has_line_states());
    picture(&hover_at(&f, &editor, offset_of(&editor, "\\cO_X", 1)));
    let mut times = Vec::new();
    for i in 0..25 {
        hover::hide(editor.view());
        let at = offset_of(&editor, &format!("x_{{{i}}}^2"), 1);
        reveal(&f, &editor, at);
        let started = Instant::now();
        assert!(hover::show_at(&editor, at, Trigger::Pointer));
        let w = wait_shown(&editor);
        times.push(started.elapsed());
        picture(&w);
    }
    times.sort();
    let p50 = times[times.len() / 2];
    println!(
        "    hover → picture: p50 {p50:?}, p90 {:?}, max {:?}",
        times[times.len() * 9 / 10],
        times[times.len() - 1]
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(p50 <= Duration::from_millis(250), "p50 {p50:?} over 250 ms");
    }
}
