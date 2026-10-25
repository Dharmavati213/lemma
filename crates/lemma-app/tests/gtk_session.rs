//! WP3.8: session restore (`session.rs`) and the local-history timeline (`history_ui.rs`),
//! with the start-up around them: the command line's FILEs (`app::open_documents`), the blank
//! editor area while they open, the window size `last.json` remembers.
//!
//! Every window gets `Paths` in a temporary directory, and `XDG_DATA_HOME` (the trash) points
//! into this test's temporary directory too: nothing reads or writes the user's state. The
//! binary runs (`scripted_runs_restore_only_when_asked`) get temporary `HOME` and `XDG_*`
//! directories and `LEMMA_CONFIG=/dev/null`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_session`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `history-timeline-light.png` and
//! `history-timeline-dark.png` there.

mod common;

use adw::prelude::*;
use lemma_app::agent::history::{self, HistoryReason};
use lemma_app::agent::panel::{self, PanelCommand};
use lemma_app::app;
use lemma_app::cli::FileArg;
use lemma_app::editor::{Editor, spell};
use lemma_app::history_ui;
use lemma_app::pdf::pdf_view;
use lemma_app::review_ui::{self, Decision};
use lemma_app::session::{
    self, LastSession, PaneState, PdfState, Reopen, SessionFile, SidebarState, TabState,
    WindowGeometry,
};
use lemma_app::testmode;
use lemma_app::window::{PAGE_PROBLEMS, PAGE_SEARCH, WindowState};
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::{Config, Paths};
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    // A restored file that is rejected goes to the trash: keep that out of the user's.
    let data = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-session-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    common::run(&[
        ("session_round_trip_tabs_positions_and_panes", round_trip),
        ("session_pdf_place_zoom_and_dark_mode", pdf_round_trip),
        (
            "session_missing_files_and_clamped_positions",
            missing_and_clamped,
        ),
        ("session_restores_tabs_in_the_background", background_tabs),
        ("startup_opens_several_files_and_a_line", several_files),
        ("startup_shows_a_blank_page_while_opening", opening_page),
        ("session_panes_before_the_first_frame", early_panes),
        ("session_remembers_the_window_size", window_size),
        ("session_saves_debounced_and_on_close", saves),
        ("session_agent_pane_only_when_safe", agent_pane),
        ("session_bad_files_are_ignored_or_reported", bad_files),
        ("scripted_runs_restore_only_when_asked", scripted_runs),
        ("session_remembers_the_last_project", last_project),
        ("lemma_without_a_file_reopens_the_last_session", reopen_runs),
        ("timeline_lists_previews_and_restores", timeline_restore),
        ("timeline_reaches_deleted_files", timeline_deleted),
        ("timeline_empty_state_and_menu", timeline_empty),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// `Paths` with every directory under `dir`.
fn paths_in(dir: &Path) -> Paths {
    Paths::from_vars(|name| {
        let sub = match name {
            "HOME" => "home",
            "XDG_CONFIG_HOME" => "config",
            "XDG_STATE_HOME" => "state",
            "XDG_DATA_HOME" => "data",
            "XDG_CACHE_HOME" => "cache",
            "XDG_RUNTIME_DIR" => "run",
            _ => return None,
        };
        Some(dir.join(sub).into())
    })
    .unwrap()
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A copy of the `proj` fixture (with its PDF) and temporary `Paths`.
fn project(name: &str) -> (PathBuf, Paths) {
    let base = common::temp_dir(name);
    let dir = base.join("proj");
    copy_dir(&common::fixtures().join("proj"), &dir);
    (dir.canonicalize().unwrap(), paths_in(&base.join("xdg")))
}

/// A window on `root` with session saves on (debounced 60 ms), as wide as a desktop window.
fn window(root: &Path, paths: &Paths, configure: impl FnOnce(&mut Config)) -> Rc<WindowState> {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.history.enabled = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(root).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        Some(paths.clone()),
        ws,
        true,
    );
    state.window.set_default_size(1024, 740);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    assert!(block_on(testmode::wait_for_frames(&state.window, 1)));
    // Broadway's screen is 1024 px wide, below the breakpoint: take the sidebar out of overlay
    // mode as a wide window does (the breakpoint applies again only when the width changes).
    state.sidebar.split.set_collapsed(false);
    state.set_session_autosave(Some(true));
    state.set_session_debounce(Duration::from_millis(60));
    common::pump_for(Duration::from_millis(30));
    state
}

fn open(state: &Rc<WindowState>, root: &Path, file: &str) -> Rc<Editor> {
    let editor = block_on(state.open_file(&root.join(file))).expect("open");
    let view = editor.view().clone();
    common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
    editor
}

fn close(state: Rc<WindowState>) {
    state.window.destroy();
    common::pump_for(Duration::from_millis(30));
}

fn session_file(state: &WindowState) -> SessionFile {
    let file = session::state_file(state).expect("a session file");
    session::parse(&std::fs::read(&file).expect("the session was written")).unwrap()
}

fn write_session(paths: &Paths, root: &Path, session: &SessionFile) -> PathBuf {
    let file = session::file_for_root(&paths.sessions_dir(), root);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, serde_json::to_vec(session).unwrap()).unwrap();
    file
}

/// The editors in tab order, by path relative to `root`.
fn tab_paths(state: &WindowState, root: &Path) -> Vec<String> {
    let editors = state.editors();
    (0..state.tab_view.n_pages())
        .filter_map(|i| {
            let child = state.tab_view.nth_page(i).child();
            editors
                .iter()
                .find(|e| e.widget().upcast_ref::<gtk::Widget>() == &child)
                .map(|e| rel(root, e.document().path()))
        })
        .collect()
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn editor_of(state: &WindowState, root: &Path, file: &str) -> Rc<Editor> {
    let path = normalize(&root.join(file));
    state
        .editors()
        .into_iter()
        .find(|e| e.document().path() == path)
        .unwrap_or_else(|| panic!("{file} is open"))
}

/// (line, byte column) of the cursor.
fn cursor(editor: &Editor) -> (u32, u32) {
    let doc = editor.document();
    doc.line_index().position(editor.cursor_offset())
}

fn place(editor: &Editor, line: u32, column: u32) {
    let offset = editor.document().line_index().offset(line, column);
    assert!(editor.place_cursor_at(offset));
}

/// The document line at the top of the view.
fn top_line(editor: &Editor) -> u32 {
    let view = editor.view();
    let (iter, _) = view.line_at_y(view.visible_rect().y() + view.top_margin());
    let doc = editor.document();
    doc.line_index().line_of(doc.byte_offset(&iter))
}

/// Scrolls `editor` so that `line` is at the top, and waits for it.
fn scroll_to(state: &WindowState, editor: &Editor, line: u32) {
    let view = editor.view();
    let iter = editor.buffer().iter_at_line(line as i32).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while top_line(editor) != line {
        let (y, _) = view.line_yrange(&iter);
        view.vadjustment().unwrap().set_value(f64::from(y));
        assert!(block_on(testmode::wait_for_frames(&state.window, 1)));
        assert!(
            Instant::now() < deadline,
            "scrolled to {}",
            top_line(editor)
        );
    }
}

fn wait_pdf(state: &WindowState) {
    common::pump_until(TIMEOUT, || {
        state.pdf.get().is_some_and(|pdf| {
            let view = pdf.view();
            pdf.pdf().is_some() && view.page_count() > 0 && view.width() > 0 && view.is_mapped()
        })
    });
}

// ----- session -----

fn round_trip() {
    let (dir, paths) = project("round-trip");
    let a = window(&dir, &paths, |_| {});
    // Nothing to restore yet; from now on the window's session is saved.
    block_on(session::restore(&a)).unwrap();
    let main = open(&a, &dir, "main.tex");
    let yoneda = open(&a, &dir, "chapters/yoneda.tex");
    let _bib = open(&a, &dir, "refs.bib");
    a.select_editor(&main);
    assert!(block_on(testmode::wait_for_frames(&a.window, 2)));
    place(&main, 20, 4);
    scroll_to(&a, &main, 12);
    a.select_editor(&yoneda);
    assert!(block_on(testmode::wait_for_frames(&a.window, 2)));
    place(&yoneda, 5, 2);
    a.sidebar.show_page(PAGE_SEARCH);
    a.pdf_sidebar.split.set_show_sidebar(true);
    let width = a.paned.width();
    assert!(width > 0);
    a.paned.set_position((f64::from(width) * 0.62) as i32);
    common::pump_for(Duration::from_millis(30));
    a.save_session_now();
    session::flush(&a);
    let saved = session_file(&a);
    let tabs: Vec<&str> = saved.tabs.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(tabs, ["main.tex", "chapters/yoneda.tex", "refs.bib"]);
    assert_eq!(saved.active, Some(1));
    assert_eq!((saved.tabs[0].line, saved.tabs[0].column), (20, 4));
    assert_eq!(saved.tabs[0].top, 12);
    assert_eq!((saved.tabs[1].line, saved.tabs[1].column), (5, 2));
    assert_eq!(saved.sidebar.page.as_deref(), Some(PAGE_SEARCH));
    assert_eq!(saved.sidebar.visible, Some(true));
    assert_eq!(saved.pdf.strip, Some(true));
    assert_eq!(saved.pdf.visible, Some(true));
    let split = saved.pdf.split.unwrap();
    assert!((split - 0.62).abs() < 0.01, "split {split}");
    assert_eq!(saved.root, dir.to_string_lossy());
    // Private: the file names the user's documents.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(session::state_file(&a).unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    close(a);

    // The next window opens refs.bib (the FILE argument), then restores.
    let b = window(&dir, &paths, |_| {});
    let bib = open(&b, &dir, "refs.bib");
    b.sidebar.show_page(PAGE_PROBLEMS);
    b.sidebar.split.set_show_sidebar(false);
    block_on(session::restore(&b)).expect("restore");
    assert!(!session::is_restoring(&b));
    assert_eq!(
        tab_paths(&b, &dir),
        ["main.tex", "chapters/yoneda.tex", "refs.bib"],
        "the saved order, the FILE at its saved place"
    );
    let active = b.active_editor().unwrap();
    assert!(Rc::ptr_eq(&active, &bib), "the FILE stays the active tab");
    let main = editor_of(&b, &dir, "main.tex");
    let yoneda = editor_of(&b, &dir, "chapters/yoneda.tex");
    assert_eq!(cursor(&main), (20, 4));
    assert_eq!(cursor(&yoneda), (5, 2));
    assert_eq!(
        b.sidebar.stack.visible_child_name().as_deref(),
        Some(PAGE_SEARCH)
    );
    assert!(b.sidebar.split.shows_sidebar());
    assert!(b.pdf_sidebar.split.shows_sidebar());
    common::pump_for(Duration::from_millis(30));
    let share = f64::from(b.paned.position()) / f64::from(b.paned.width());
    assert!((share - 0.62).abs() < 0.01, "split {share}");
    // A background tab scrolls once it is shown.
    b.select_editor(&main);
    let deadline = Instant::now() + TIMEOUT;
    while top_line(&main) != 12 {
        assert!(block_on(testmode::wait_for_frames(&b.window, 1)));
        assert!(Instant::now() < deadline, "top line {}", top_line(&main));
    }
    // The restored session is what the next save writes (nothing lost, nothing changed).
    b.select_editor(&bib);
    b.save_session_now();
    session::flush(&b);
    let again = session_file(&b);
    assert_eq!(again.tabs.len(), 3);
    assert_eq!(again.tabs[0], saved.tabs[0]);
    assert_eq!(again.active, Some(2));
    // A second restore does nothing.
    block_on(session::restore(&b)).unwrap();
    assert_eq!(b.tab_view.n_pages(), 3);
    close(b);
}

fn pdf_round_trip() {
    let (dir, paths) = project("pdf");
    let a = window(&dir, &paths, |_| {});
    block_on(session::restore(&a)).unwrap();
    let _main = open(&a, &dir, "main.tex");
    wait_pdf(&a);
    let pdf = a.pdf.get().unwrap();
    let view = pdf.view().clone();
    let dark_before = view.is_dark();
    view.set_zoom(1.3, None);
    let layout = view.layout().unwrap();
    let anchor = lemma_pdf::Anchor {
        page: 1,
        frac: 0.4,
        x: 0.0,
    };
    view.set_scroll_position(0.0, layout.y_for_anchor(anchor, view.zoom()));
    WidgetExt::activate_action(&a.window, "win.pdf-invert", None).unwrap();
    assert_ne!(view.is_dark(), dark_before);
    common::pump_for(Duration::from_millis(30));
    a.save_session_now();
    session::flush(&a);
    let saved = session_file(&a);
    let place = saved.pdf.place.expect("the PDF's place");
    assert_eq!(saved.pdf.file.as_deref(), Some("main.pdf"));
    assert_eq!(place.page, 1);
    assert!((place.frac - 0.4).abs() < 0.01, "{place:?}");
    assert_eq!(place.zoom, Some(1.3));
    assert_eq!(place.dark, Some(!dark_before));
    close(a);

    let b = window(&dir, &paths, |_| {});
    let _main = open(&b, &dir, "main.tex");
    block_on(session::restore(&b)).unwrap();
    wait_pdf(&b);
    common::pump_until(TIMEOUT, || !session::pdf_place_pending(&b));
    let view = b.pdf.get().unwrap().view().clone();
    assert!((view.zoom() - 1.3).abs() < 1e-6, "zoom {}", view.zoom());
    let anchor = pdf_view::anchor_of(&view).unwrap();
    assert_eq!(anchor.page, 1);
    assert!((anchor.frac - 0.4).abs() < 0.01, "{anchor:?}");
    assert_eq!(view.is_dark(), !dark_before);
    // The place is saved as it is now.
    b.save_session_now();
    session::flush(&b);
    assert_eq!(session_file(&b).pdf.place.unwrap().page, 1);
    // WP3.10: a PDF in a window of its own (its pane hidden meanwhile) is saved as shown, so it
    // comes back.
    WidgetExt::activate_action(&b.window, "win.pdf-detach", None).unwrap();
    common::pump_for(Duration::from_millis(30));
    assert!(lemma_app::pdf::detach::is_detached(&b));
    assert!(!b.pdf_pane.is_visible());
    b.save_session_now();
    session::flush(&b);
    assert_eq!(session_file(&b).pdf.visible, Some(true));
    WidgetExt::activate_action(&b.window, "win.pdf-detach", None).unwrap();
    common::pump_for(Duration::from_millis(30));
    assert!(!lemma_app::pdf::detach::is_detached(&b));
    // Fit Page is saved as a mode, without a zoom.
    WidgetExt::activate_action(&b.window, "win.pdf-fit-page", None).unwrap();
    common::pump_for(Duration::from_millis(30));
    b.save_session_now();
    session::flush(&b);
    let fitted = session_file(&b);
    let place = fitted.pdf.place.unwrap();
    assert_eq!((place.fit, place.zoom), (pdf_view::Fit::Page, None));
    close(b);

    // A session whose place belongs to another PDF leaves the view at the top.
    let mut other = saved.clone();
    other.pdf.file = Some("other.pdf".into());
    write_session(&paths, &dir, &other);
    let c = window(&dir, &paths, |_| {});
    let _main = open(&c, &dir, "main.tex");
    block_on(session::restore(&c)).unwrap();
    wait_pdf(&c);
    common::pump_until(TIMEOUT, || !session::pdf_place_pending(&c));
    let anchor = pdf_view::anchor_of(c.pdf.get().unwrap().view()).unwrap();
    assert_eq!(anchor.page, 0, "{anchor:?}");
    close(c);

    // And comes back as Fit Page.
    write_session(&paths, &dir, &fitted);
    let d = window(&dir, &paths, |_| {});
    let _main = open(&d, &dir, "main.tex");
    block_on(session::restore(&d)).unwrap();
    wait_pdf(&d);
    common::pump_until(TIMEOUT, || !session::pdf_place_pending(&d));
    let view = d.pdf.get().unwrap().view().clone();
    assert_eq!(view.fit(), pdf_view::Fit::Page);
    let (_, height) = view.page_size(1).unwrap();
    assert!(
        view.zoom() <= (f64::from(view.height()) - 2.0 * pdf_view::PAGE_GAP) / height + 1e-9,
        "zoom {} in a view {} high",
        view.zoom(),
        view.height()
    );
    assert_eq!(view.current_page(), place.page);
    close(d);
}

fn missing_and_clamped() {
    let base = common::temp_dir("missing");
    let dir = base.join("proj");
    write(
        &dir,
        &[
            ("main.tex", "zero\none\ntwo two\nthree\n"),
            ("ch/b.tex", "x\ny\nz"),
        ],
    );
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let tab = |path: &str, line, column, top| TabState {
        path: path.into(),
        line,
        column,
        top,
    };
    write_session(
        &paths,
        &dir,
        &SessionFile {
            version: session::VERSION,
            root: dir.to_string_lossy().into_owned(),
            tabs: vec![
                tab("gone.tex", 3, 0, 0),
                tab("ch/b.tex", 99, 99, 50),
                tab("main.tex", 2, 999, 0),
                tab("/nonexistent/abs.tex", 0, 0, 0),
            ],
            active: Some(0),
            sidebar: SidebarState::default(),
            agent: PaneState::default(),
            pdf: PdfState::default(),
        },
    );
    // No FILE: the window starts empty.
    let w = window(&dir, &paths, |_| {});
    block_on(session::restore(&w)).unwrap();
    assert_eq!(tab_paths(&w, &dir), ["ch/b.tex", "main.tex"]);
    assert_eq!(
        w.workspace.root().path(),
        dir,
        "a tab in a subdirectory does not move the root"
    );
    let b = editor_of(&w, &dir, "ch/b.tex");
    let main = editor_of(&w, &dir, "main.tex");
    assert_eq!(cursor(&b), (2, 1), "clamped to the last line's end");
    assert_eq!(cursor(&main), (2, 7), "clamped to the line's end");
    // The saved active tab is gone: the first restored one is selected.
    assert!(Rc::ptr_eq(&w.active_editor().unwrap(), &b));
    assert!(block_on(testmode::wait_for_frames(&w.window, 3)));
    assert_eq!(top_line(&b), 0, "a top line past the end is clamped");
    close(w);
}

/// The restored tabs open in the background: the FILE stays selected and focused throughout,
/// and a tab that has not been shown loads nothing that waits for its first frame (spell
/// checking, completion's popup) until it is selected.
fn background_tabs() {
    let base = common::temp_dir("background");
    let dir = base.join("proj");
    let long: String = (0..300).map(|i| format!("Line {i} of b.\n")).collect();
    write(
        &dir,
        &[
            ("main.tex", "Main text.\n"),
            ("a.tex", "First.\n"),
            ("b.tex", &long),
            ("c.tex", "Third.\n"),
        ],
    );
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let tab = |path: &str, line, top| TabState {
        path: path.into(),
        line,
        column: 0,
        top,
    };
    let session = SessionFile {
        version: session::VERSION,
        root: dir.to_string_lossy().into_owned(),
        tabs: vec![
            tab("main.tex", 0, 0),
            tab("a.tex", 0, 0),
            tab("b.tex", 210, 200),
            tab("c.tex", 0, 0),
        ],
        active: Some(3),
        ..SessionFile::default()
    };
    write_session(&paths, &dir, &session);
    let w = window(&dir, &paths, |c| {
        c.spell.enabled = true;
        c.spell.language = "en_US".into();
    });
    let main = open(&w, &dir, "main.tex");
    common::pump_until(TIMEOUT, || spell::is_checking(&main));
    let selections = Rc::new(Cell::new(0));
    let counter = selections.clone();
    w.tab_view
        .connect_selected_page_notify(move |_| counter.set(counter.get() + 1));
    block_on(session::restore(&w)).unwrap();
    assert_eq!(tab_paths(&w, &dir), ["main.tex", "a.tex", "b.tex", "c.tex"]);
    assert_eq!(selections.get(), 0, "no tab but the FILE was ever selected");
    assert!(Rc::ptr_eq(&w.active_editor().unwrap(), &main));
    for _ in 0..3 {
        assert!(block_on(testmode::wait_for_frames(&w.window, 1)));
    }
    assert!(main.view().is_focus(), "the FILE keeps the focus");
    for file in ["a.tex", "b.tex", "c.tex"] {
        let editor = editor_of(&w, &dir, file);
        assert!(!editor.view().is_mapped(), "{file} was never shown");
        assert!(
            spell::adapter(&editor).is_none(),
            "{file} is not checked yet"
        );
    }
    // Shown, a tab loads them and scrolls to its saved place.
    let b = editor_of(&w, &dir, "b.tex");
    w.select_editor(&b);
    common::pump_until(TIMEOUT, || spell::is_checking(&b));
    let deadline = Instant::now() + TIMEOUT;
    while top_line(&b) != 200 {
        assert!(block_on(testmode::wait_for_frames(&w.window, 1)));
        assert!(Instant::now() < deadline, "top line {}", top_line(&b));
    }
    assert_eq!(cursor(&b), (210, 0));
    assert!(spell::adapter(&editor_of(&w, &dir, "a.tex")).is_none());
    drop((main, b));
    close(w);

    // Without a FILE (`lemma DIR`): the saved active tab is the only one ever shown.
    write_session(&paths, &dir, &session);
    let w = window(&dir, &paths, |c| {
        c.spell.enabled = true;
        c.spell.language = "en_US".into();
    });
    let selections = Rc::new(Cell::new(0));
    let counter = selections.clone();
    w.tab_view
        .connect_selected_page_notify(move |_| counter.set(counter.get() + 1));
    block_on(session::restore(&w)).unwrap();
    assert_eq!(tab_paths(&w, &dir), ["main.tex", "a.tex", "b.tex", "c.tex"]);
    let c = editor_of(&w, &dir, "c.tex");
    assert!(Rc::ptr_eq(&w.active_editor().unwrap(), &c));
    assert_eq!(
        selections.get(),
        1,
        "only the saved active tab was selected"
    );
    common::pump_until(TIMEOUT, || spell::is_checking(&c));
    for file in ["main.tex", "a.tex", "b.tex"] {
        let editor = editor_of(&w, &dir, file);
        assert!(
            spell::adapter(&editor).is_none(),
            "{file} is not checked yet"
        );
    }
    drop(c);
    close(w);
}

/// True when document line `line` (0-based) of `editor` is in its view.
fn line_in_view(editor: &Editor, line: u32) -> bool {
    let view = editor.view();
    let Some(iter) = editor.buffer().iter_at_line(line as i32) else {
        return false;
    };
    let (y, height) = view.line_yrange(&iter);
    let rect = view.visible_rect();
    view.height() > 0 && y >= rect.y() && y + height <= rect.y() + rect.height()
}

/// The command line's documents (`app::open_documents`): the first is shown and focused at
/// once, the others open behind it in order; a line puts the cursor there, in view once the
/// tab is shown, and wins over the place the session saved.
fn several_files() {
    let base = common::temp_dir("several");
    let dir = base.join("proj");
    let long: String = (1..=300).map(|i| format!("    line {i}\n")).collect();
    write(
        &dir,
        &[("main.tex", &long), ("b.tex", &long), ("c.tex", "c\n")],
    );
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let tab = |path: &str, line, top| TabState {
        path: path.into(),
        line,
        column: 0,
        top,
    };
    write_session(
        &paths,
        &dir,
        &SessionFile {
            version: session::VERSION,
            root: dir.to_string_lossy().into_owned(),
            tabs: vec![tab("b.tex", 20, 10), tab("c.tex", 0, 0)],
            active: Some(1),
            ..SessionFile::default()
        },
    );
    let w = window(&dir, &paths, |_| {});
    let selections = Rc::new(Cell::new(0));
    let counter = selections.clone();
    w.tab_view
        .connect_selected_page_notify(move |_| counter.set(counter.get() + 1));
    let documents = [
        FileArg {
            line: Some(120),
            ..FileArg::new(dir.join("main.tex"))
        },
        FileArg {
            line: Some(250),
            column: Some(7),
            ..FileArg::new(dir.join("b.tex"))
        },
    ];
    let errors = block_on(app::open_documents(&w, &documents));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(tab_paths(&w, &dir), ["main.tex", "b.tex"]);
    let main = editor_of(&w, &dir, "main.tex");
    let b = editor_of(&w, &dir, "b.tex");
    assert!(Rc::ptr_eq(&w.active_editor().unwrap(), &main));
    assert_eq!(selections.get(), 1, "only the first document was selected");
    assert_eq!(cursor(&main), (119, 4), "line 120, on its text");
    assert_eq!(cursor(&b), (249, 6), "line 250, character 7");
    common::pump_until(TIMEOUT, || {
        main.view().is_focus() && line_in_view(&main, 119)
    });
    assert!(!b.view().is_mapped(), "b.tex opened behind");
    // The session's tabs follow; b.tex keeps the command line's place.
    block_on(session::restore(&w)).unwrap();
    assert_eq!(tab_paths(&w, &dir), ["main.tex", "b.tex", "c.tex"]);
    assert!(Rc::ptr_eq(&w.active_editor().unwrap(), &main));
    assert_eq!(cursor(&b), (249, 6), "not the session's place");
    w.select_editor(&b);
    common::pump_until(TIMEOUT, || b.view().is_mapped() && line_in_view(&b, 249));
    for _ in 0..2 {
        assert!(block_on(testmode::wait_for_frames(&w.window, 1)));
    }
    assert!(
        line_in_view(&b, 249),
        "the session's scroll does not follow"
    );
    // A document that does not open is reported; the others open all the same.
    write(&dir, &[("binary.tex", "a\0b\n")]);
    let errors = block_on(app::open_documents(
        &w,
        &[
            FileArg::new(dir.join("binary.tex")),
            FileArg::new(dir.join("c.tex")),
        ],
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("binary.tex"), "{errors:?}");
    drop((main, b));
    close(w);

    // The first document decides the project root even when it does not open: the next one,
    // elsewhere, opens without moving the root to its directory.
    write(&base, &[("elsewhere/x.tex", "x\n")]);
    let w = window(&dir, &paths, |_| {});
    let errors = block_on(app::open_documents(
        &w,
        &[
            FileArg::new(dir.join("binary.tex")),
            FileArg::new(base.join("elsewhere/x.tex")),
        ],
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(w.tab_view.n_pages(), 1);
    assert_eq!(w.workspace.root().path(), dir);
    close(w);
}

/// While the documents of the start open, the editor area is blank instead of "No Document
/// Open"; once they are open it shows them, and when none opened after all, the empty page.
fn opening_page() {
    let base = common::temp_dir("opening");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "x\n"), ("binary.tex", "a\0b\n")]);
    let dir = dir.canonicalize().unwrap();
    let mut config = Config::default();
    config.spell.enabled = false;
    let w = WindowState::new(
        &common::application(),
        Rc::new(config),
        None,
        Workspace::new(ProjectRoot::new(&dir).unwrap()),
        true,
    );
    assert_eq!(w.editor_page().as_deref(), Some("empty"));
    // As the app does before the window is shown.
    w.set_opening(true);
    w.window.set_default_size(1024, 740);
    w.window.present();
    common::pump_until(TIMEOUT, || w.window.is_mapped());
    assert!(block_on(testmode::wait_for_frames(&w.window, 1)));
    assert_eq!(w.editor_page().as_deref(), Some("opening"));
    let main = block_on(w.open_file(&dir.join("main.tex"))).unwrap();
    assert_eq!(w.editor_page().as_deref(), Some("tabs"));
    w.set_opening(false);
    assert_eq!(w.editor_page().as_deref(), Some("tabs"));
    w.close_editor(&main, true);
    common::pump_until(TIMEOUT, || w.tab_view.n_pages() == 0);
    assert_eq!(w.editor_page().as_deref(), Some("empty"));
    // Nothing opened: the empty page once the start is done.
    w.set_opening(true);
    assert_eq!(w.editor_page().as_deref(), Some("opening"));
    assert!(block_on(w.open_file(&dir.join("binary.tex"))).is_err());
    assert_eq!(w.editor_page().as_deref(), Some("opening"));
    w.set_opening(false);
    assert_eq!(w.editor_page().as_deref(), Some("empty"));
    drop(main);
    close(w);
}

/// `session::restore_panes_early`, before the window is shown: its first frame has the saved
/// panes (here the PDF pane hidden, the Search page and the PDF strip), and the restore then
/// opens the tabs from what it read.
fn early_panes() {
    let (dir, paths) = project("early-panes");
    let file = write_session(
        &paths,
        &dir,
        &SessionFile {
            version: session::VERSION,
            root: dir.to_string_lossy().into_owned(),
            tabs: vec![TabState {
                path: "main.tex".into(),
                ..TabState::default()
            }],
            active: Some(0),
            sidebar: SidebarState {
                visible: Some(true),
                page: Some(PAGE_SEARCH.into()),
                width: Some(0.25),
            },
            pdf: PdfState {
                visible: Some(false),
                strip: Some(true),
                ..PdfState::default()
            },
            ..SessionFile::default()
        },
    );
    let mut config = Config::default();
    config.spell.enabled = false;
    config.history.enabled = false;
    let w = WindowState::new(
        &common::application(),
        Rc::new(config),
        Some(paths.clone()),
        Workspace::new(ProjectRoot::new(&dir).unwrap()),
        true,
    );
    assert!(w.pdf_pane.get_visible(), "shown by default");
    assert!(session::restore_panes_early(&w), "the session has a tab");
    // What the first frame shows.
    let first: Rc<RefCell<Option<String>>> = Rc::default();
    let seen = first.clone();
    let (pdf_pane, stack, strip) = (
        w.pdf_pane.clone(),
        w.sidebar.stack.clone(),
        w.pdf_sidebar.split.clone(),
    );
    w.window.connect_realize(move |window| {
        let clock = window.frame_clock().unwrap();
        let (seen, pdf_pane, stack, strip) =
            (seen.clone(), pdf_pane.clone(), stack.clone(), strip.clone());
        clock.connect_after_paint(move |_| {
            if seen.borrow().is_none() {
                let page = stack.visible_child_name().unwrap_or_default();
                let (pdf, strip) = (pdf_pane.is_visible(), strip.shows_sidebar());
                seen.replace(Some(format!("pdf={pdf} page={page} strip={strip}")));
            }
        });
    });
    w.window.set_default_size(1024, 740);
    w.window.present();
    common::pump_until(TIMEOUT, || first.borrow().is_some());
    assert_eq!(
        first.borrow().as_deref(),
        Some("pdf=false page=search strip=true")
    );
    assert!((w.sidebar.split.sidebar_width_fraction() - 0.25).abs() < 1e-9);
    // The restore uses what was read: the file is not read again.
    std::fs::remove_file(&file).unwrap();
    block_on(session::restore(&w)).unwrap();
    assert_eq!(tab_paths(&w, &dir), ["main.tex"]);
    assert!(!w.pdf_pane.is_visible());
    close(w);
}

/// `last.json` records the size and maximized state of a window that remembers its size, and
/// the next window starts with them; a window that keeps the remembered size writes it unchanged.
fn window_size() {
    let base = common::temp_dir("window-size");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "x\n")]);
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let unpresented = || {
        let mut config = Config::default();
        config.build.on_save = false;
        config.spell.enabled = false;
        config.history.enabled = false;
        let w = WindowState::new(
            &common::application(),
            Rc::new(config),
            Some(paths.clone()),
            Workspace::new(ProjectRoot::new(&dir).unwrap()),
            true,
        );
        w.set_session_autosave(Some(true));
        w.set_session_debounce(Duration::from_millis(60));
        w
    };
    let show = |w: &Rc<WindowState>| {
        w.window.present();
        common::pump_until(TIMEOUT, || w.window.is_mapped());
        assert!(block_on(testmode::wait_for_frames(&w.window, 1)));
        block_on(session::restore(w)).unwrap();
        open(w, &dir, "main.tex")
    };
    let remembered = || session::read_last(&paths).and_then(|l| l.window);
    let a = unpresented();
    session::remember_window_size(&a, None);
    a.window.set_default_size(900, 650);
    let main = show(&a);
    let expected = WindowGeometry {
        width: 900,
        height: 650,
        maximized: false,
    };
    common::pump_until(TIMEOUT, || remembered() == Some(expected));
    a.window.maximize();
    common::pump_until(TIMEOUT, || a.window.is_maximized());
    let maximized = WindowGeometry {
        maximized: true,
        ..expected
    };
    common::pump_until(TIMEOUT, || remembered() == Some(maximized));
    drop(main);
    close(a);
    assert_eq!(remembered(), Some(maximized), "closing keeps it");

    // The next window starts with them.
    let b = unpresented();
    session::remember_window_size(&b, remembered());
    assert_eq!(b.window.default_size(), (900, 650));
    let main = show(&b);
    common::pump_until(TIMEOUT, || b.window.is_maximized());
    b.window.unmaximize();
    common::pump_until(TIMEOUT, || remembered() == Some(expected));
    drop(main);
    close(b);

    // A window whose size is not the user's writes the remembered one as it is.
    let c = unpresented();
    session::keep_window_size(&c, remembered());
    c.window.set_default_size(700, 500);
    let main = show(&c);
    place(&main, 0, 1);
    c.save_session_now();
    session::flush(&c);
    common::pump_for(Duration::from_millis(200));
    drop(main);
    close(c);
    assert_eq!(remembered(), Some(expected));
    let last = session::read_last(&paths).unwrap();
    assert_eq!(
        last.file,
        Some(dir.join("main.tex").to_string_lossy().into())
    );
}

fn saves() {
    let (dir, paths) = project("saves");
    let w = window(&dir, &paths, |_| {});
    let file = session::state_file(&w).unwrap();
    assert!(file.starts_with(paths.sessions_dir()));
    // Before the restore nothing is written (it would overwrite the session to restore).
    let main = open(&w, &dir, "main.tex");
    common::pump_for(Duration::from_millis(200));
    assert!(!w.session_save_pending());
    assert!(!file.exists());
    block_on(session::restore(&w)).unwrap();
    // Debounced: written once things are quiet.
    common::pump_until(TIMEOUT, || !w.session_save_pending() && file.exists());
    common::pump_until(TIMEOUT, || session_file(&w).tabs.len() == 1);
    place(&main, 7, 0);
    assert!(w.session_save_pending());
    common::pump_until(TIMEOUT, || session_file(&w).tabs[0].line == 7);
    // Closing writes at once, before the main loop runs again.
    place(&main, 9, 3);
    w.window.close();
    let saved = session::parse(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!((saved.tabs[0].line, saved.tabs[0].column), (9, 3));
    common::pump_for(Duration::from_millis(100));
    // A queued write that finishes later does not put an older state back.
    let saved = session::parse(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved.tabs[0].line, 9);
    drop(main);
    close(w);

    // `[session] restore = false`: nothing is saved, nothing restored.
    let w = window(&dir, &paths, |c| c.session.restore = false);
    std::fs::remove_file(&file).unwrap();
    block_on(session::restore(&w)).unwrap();
    let _main = open(&w, &dir, "main.tex");
    common::pump_for(Duration::from_millis(200));
    w.save_session_now();
    assert!(!file.exists());
    close(w);
}

fn agent_pane() {
    let base = common::temp_dir("agent-pane");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "x\n")]);
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let session = SessionFile {
        version: session::VERSION,
        root: dir.to_string_lossy().into_owned(),
        tabs: vec![TabState {
            path: "main.tex".into(),
            ..TabState::default()
        }],
        agent: PaneState {
            visible: Some(true),
            width: Some(0.35),
        },
        ..SessionFile::default()
    };
    write_session(&paths, &dir, &session);
    // Every agent command is swallowed (and recorded) before anything can show the pane: no
    // agent ever starts here. `scenario` makes the window play a fake-agent scenario.
    let setup = |scenario: bool| {
        let w = window(&dir, &paths, |_| {});
        let commands: Rc<RefCell<Vec<PanelCommand>>> = Rc::default();
        let seen = commands.clone();
        panel::set_command_hook(
            &w,
            Some(Box::new(move |cmd: &PanelCommand| {
                seen.borrow_mut().push(cmd.clone());
                true
            })),
        );
        if scenario {
            w.agent_controller()
                .unwrap()
                .set_scenario(Some(dir.join("scenario.json")));
        }
        // A wide window: the pane beside the editor (no room for the PDF pane on Broadway's
        // screen). Leaving overlay mode shows the pane (libadwaita); it starts hidden.
        w.pdf_pane.set_visible(false);
        w.agent.split.set_collapsed(false);
        w.agent.split.set_show_sidebar(false);
        commands.borrow_mut().clear();
        (w, commands)
    };
    // A run that quits by itself never starts the real agent by restoring its pane.
    let (w, commands) = setup(false);
    block_on(session::restore(&w)).unwrap();
    common::pump_for(Duration::from_millis(50));
    assert!(!w.agent.split.shows_sidebar());
    assert!(commands.borrow().is_empty());
    close(w);
    // (That window saved the session it showed, without the pane: the next one gets the file
    // again.)
    write_session(&paths, &dir, &session);
    // With a scenario (the fake agent), or interactively, the pane comes back (and starts it).
    let (w, commands) = setup(true);
    block_on(session::restore(&w)).unwrap();
    common::pump_for(Duration::from_millis(50));
    assert!(w.agent.split.shows_sidebar());
    assert!((w.agent.split.sidebar_width_fraction() - 0.35).abs() < 1e-9);
    assert!(
        commands.borrow().contains(&PanelCommand::Start),
        "{:?}",
        commands.borrow()
    );
    close(w);
    // In a narrow window the pane is an overlay: it stays closed.
    write_session(&paths, &dir, &session);
    let (w, commands) = setup(true);
    w.agent.split.set_collapsed(true);
    w.agent.split.set_show_sidebar(false);
    block_on(session::restore(&w)).unwrap();
    common::pump_for(Duration::from_millis(50));
    assert!(!w.agent.split.shows_sidebar());
    assert!(!commands.borrow().contains(&PanelCommand::Start));
    close(w);
}

fn bad_files() {
    let base = common::temp_dir("bad");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "x\n")]);
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let file = session::file_for_root(&paths.sessions_dir(), &dir);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    // Not JSON: ignored.
    std::fs::write(&file, "{oops").unwrap();
    let w = window(&dir, &paths, |_| {});
    block_on(session::restore(&w)).unwrap();
    assert_eq!(w.tab_view.n_pages(), 0);
    close(w);
    // Another project's (a hash collision, or a copied file): ignored.
    let foreign = SessionFile {
        root: "/somewhere/else".into(),
        tabs: vec![TabState {
            path: "main.tex".into(),
            ..TabState::default()
        }],
        ..SessionFile::default()
    };
    write_session(&paths, &dir, &foreign);
    let w = window(&dir, &paths, |_| {});
    block_on(session::restore(&w)).unwrap();
    assert_eq!(w.tab_view.n_pages(), 0);
    close(w);
    // Unreadable: an error the app shows.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir_all(&file).unwrap();
    let w = window(&dir, &paths, |_| {});
    let e = block_on(session::restore(&w)).unwrap_err();
    assert!(e.contains("Could not read the saved session"), "{e}");
    close(w);
}

/// The binary: a run that quits by itself restores only with `--restore-session`, and never
/// writes the session.
fn scripted_runs() {
    let base = common::temp_dir("scripted");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "main\n"), ("b.tex", "b\n")]);
    let dir = dir.canonicalize().unwrap();
    let xdg = base.join("xdg");
    let paths = paths_in(&xdg);
    let session = SessionFile {
        version: session::VERSION,
        root: dir.to_string_lossy().into_owned(),
        tabs: ["main.tex", "b.tex"]
            .iter()
            .map(|p| TabState {
                path: (*p).into(),
                ..TabState::default()
            })
            .collect(),
        active: Some(1),
        ..SessionFile::default()
    };
    let file = write_session(&paths, &dir, &session);
    let before = std::fs::read(&file).unwrap();
    let run = |extra: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
            .arg(dir.join("main.tex"))
            .args(["--quit-after-ms", "1500"])
            .args(extra)
            .env("LEMMA_CONFIG", "/dev/null")
            .env("LEMMA_LOG", "lemma_app::session=info")
            .env("HOME", xdg.join("home"))
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .output()
            .expect("run lemma");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "lemma failed: {stderr}");
        stderr
    };
    let plain = run(&[]);
    assert!(!plain.contains("session: restored"), "{plain}");
    let restored = run(&["--restore-session"]);
    assert!(
        restored.contains("session: restored 2 of 2 tabs"),
        "{restored}"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "scripted runs never write the session"
    );
}

/// `last.json`: the project and selected file of the window that saved or closed last; windows
/// without tabs, or with sessions off, leave it alone.
fn last_project() {
    let base = common::temp_dir("last");
    write(
        &base,
        &[
            ("a/main.tex", "a\n"),
            ("a/ch.tex", "c\n"),
            ("b/main.tex", "b\n"),
        ],
    );
    let a = base.join("a").canonicalize().unwrap();
    let b = base.join("b").canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let file = paths.last_session_file();
    let reopened = || session::reopen(&paths);
    let last = |root: &Path, f: &str| {
        Some(Reopen {
            root: root.to_path_buf(),
            file: Some(root.join(f)),
        })
    };
    // No tabs: nothing to reopen.
    let wa = window(&a, &paths, |_| {});
    block_on(session::restore(&wa)).unwrap();
    common::pump_for(Duration::from_millis(200));
    assert!(!file.exists());
    let main_a = open(&wa, &a, "main.tex");
    let _ch = open(&wa, &a, "ch.tex");
    common::pump_until(TIMEOUT, || reopened() == last(&a, "ch.tex"));
    let saved: LastSession = session::parse_last(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved.root, a.to_string_lossy());
    // Another window saves later: its project is the last one.
    let wb = window(&b, &paths, |_| {});
    block_on(session::restore(&wb)).unwrap();
    let main_b = open(&wb, &b, "main.tex");
    common::pump_until(TIMEOUT, || reopened() == last(&b, "main.tex"));
    // Selecting another tab of the first window changes it.
    wa.select_editor(&main_a);
    common::pump_until(TIMEOUT, || reopened() == last(&a, "main.tex"));
    // A cursor move does not (the project and file are the same): the second window's save
    // after a move leaves the first window's choice.
    place(&main_b, 0, 1);
    common::pump_for(Duration::from_millis(300));
    assert_eq!(reopened(), last(&a, "main.tex"));
    // The window closed last wins, even when it changed nothing since its last save.
    drop(main_b);
    close(wb);
    assert_eq!(reopened(), last(&b, "main.tex"));
    drop(main_a);
    close(wa);
    assert_eq!(reopened(), last(&a, "main.tex"));
    // With sessions off, a window does not touch it.
    let wb = window(&b, &paths, |c| c.session.restore = false);
    block_on(session::restore(&wb)).unwrap();
    let _main_b = open(&wb, &b, "main.tex");
    wb.save_session_now();
    common::pump_for(Duration::from_millis(200));
    close(wb);
    assert_eq!(reopened(), last(&a, "main.tex"));
}

/// The binary: without a FILE, an interactive run (here a scripted one with `--restore-session`)
/// opens the last session's project and file instead of the current directory.
fn reopen_runs() {
    let base = common::temp_dir("reopen");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "main\n"), ("b.tex", "b\n")]);
    let dir = dir.canonicalize().unwrap();
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let xdg = base.join("xdg");
    let paths = paths_in(&xdg);
    let session = SessionFile {
        version: session::VERSION,
        root: dir.to_string_lossy().into_owned(),
        tabs: ["main.tex", "b.tex"]
            .iter()
            .map(|p| TabState {
                path: (*p).into(),
                ..TabState::default()
            })
            .collect(),
        active: Some(1),
        ..SessionFile::default()
    };
    write_session(&paths, &dir, &session);
    let last = LastSession {
        version: session::VERSION,
        root: dir.to_string_lossy().into_owned(),
        file: Some(dir.join("b.tex").to_string_lossy().into_owned()),
        window: None,
    };
    std::fs::write(
        paths.last_session_file(),
        serde_json::to_vec(&last).unwrap(),
    )
    .unwrap();
    let run = |args: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
            .current_dir(&elsewhere)
            .args(["--quit-after-ms", "1500"])
            .args(args)
            .env("LEMMA_CONFIG", "/dev/null")
            .env("LEMMA_LOG", "lemma_app::session=info")
            .env("HOME", xdg.join("home"))
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .output()
            .expect("run lemma");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "lemma failed: {stderr}");
        stderr
    };
    let reopened = run(&["--restore-session"]);
    assert!(
        reopened.contains(&format!("reopening the last project {}", dir.display())),
        "{reopened}"
    );
    assert!(
        reopened.contains("session: restored 2 of 2 tabs"),
        "{reopened}"
    );
    // Scripted runs do not reopen it unless asked; a FILE (or `lemma .`) wins.
    let plain = run(&[]);
    assert!(!plain.contains("reopening"), "{plain}");
    let here = run(&[".", "--restore-session"]);
    assert!(!here.contains("reopening"), "{here}");
    assert!(!here.contains("session: restored"), "{here}");
    // A project that is gone: the current directory, as before.
    std::fs::remove_dir_all(&dir).unwrap();
    let gone = run(&["--restore-session"]);
    assert!(!gone.contains("reopening"), "{gone}");
    assert!(gone.contains("the last project"), "{gone}");
}

// ----- the history timeline -----

/// A window with local history on, every save recorded.
fn history_window(dir: &Path, paths: &Paths) -> Rc<WindowState> {
    window(dir, paths, |c| {
        c.history.enabled = true;
        c.history.min_interval_s = 0;
    })
}

fn save_as(state: &Rc<WindowState>, editor: &Editor, text: &str) {
    editor.buffer().set_text(text);
    assert_eq!(block_on(editor.document().save()), Ok(()));
    history::write_waiting(state);
    block_on(history::flush(state));
}

fn open_timeline(state: &Rc<WindowState>) -> Rc<history_ui::Timeline> {
    WidgetExt::activate_action(&state.window, "win.history", None).unwrap();
    common::pump_until(TIMEOUT, || {
        history_ui::timeline(state).is_some_and(|t| !t.is_busy() && t.page() != "loading")
    });
    history_ui::timeline(state).unwrap()
}

fn wait_idle(timeline: &history_ui::Timeline) {
    common::pump_until(TIMEOUT, || {
        !timeline.is_busy() && timeline.page() != "loading"
    });
}

fn timeline_restore() {
    let base = common::temp_dir("timeline");
    let dir = base.join("proj");
    write(&dir, &[("main.tex", "v1\nshared\n")]);
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let w = history_window(&dir, &paths);
    let editor = open(&w, &dir, "main.tex");
    let doc = editor.document().clone();
    save_as(&w, &editor, "v1\nshared\n");
    save_as(&w, &editor, "v2\nshared\nadded\n");
    save_as(&w, &editor, "v3\nshared\n");
    let t = open_timeline(&w);
    assert_eq!(
        t.files(),
        [history_ui::HistoryFile {
            path: doc.path().to_path_buf(),
            exists: true
        }]
    );
    let entries = t.entries();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|e| e.reason == HistoryReason::Save));
    assert!(!t.banner_shown(), "history is recorded");
    // The newest snapshot is the current text: the one before it is shown first.
    assert_eq!(t.selected_entry().as_ref(), Some(&entries[1]));
    assert_eq!(t.page(), "preview");
    assert!(
        t.preview_text().contains("+added\n"),
        "{}",
        t.preview_text()
    );
    assert!(t.select_entry(0));
    wait_idle(&t);
    assert_eq!(t.page(), "same");
    assert!(!t.can_restore());
    // The oldest: the diff from now to then.
    assert!(t.select_entry(2));
    wait_idle(&t);
    assert_eq!(t.page(), "preview");
    let preview = t.preview_text();
    assert!(
        preview.contains("-v3\n") && preview.contains("+v1\n"),
        "{preview}"
    );
    assert!(!preview.contains("+++"), "no file header: {preview}");
    let (title, subtitle) = t.heading();
    assert!(title.starts_with("Today, "), "{title}");
    assert_eq!(subtitle, "Saved · +1 −1");
    assert!(t.can_restore());
    snapshot_dialog(&w, "history-timeline");
    // Restore as hunks: reviewable, and Reject gives the current text back.
    let count = block_on(t.restore()).unwrap();
    assert!(count >= 1);
    common::pump_until(TIMEOUT, || history_ui::timeline(&w).is_none());
    assert_eq!(doc.text(), "v1\nshared\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("main.tex")).unwrap(),
        "v1\nshared\n"
    );
    assert!(review_ui::hunk_count(&doc) >= 1);
    review_ui::decide_document(&w, &doc, Decision::Reject);
    assert_eq!(doc.text(), "v3\nshared\n");
    // The restore and the reject added snapshots; v2 is still the second oldest.
    let t = open_timeline(&w);
    assert!(t.entries().len() > 3, "{:?}", t.entries());
    assert!(t.select_entry(t.entries().len() - 2));
    wait_idle(&t);
    assert!(t.preview_text().contains("+added\n"));
    t.dialog.close();
    common::pump_until(TIMEOUT, || history_ui::timeline(&w).is_none());
    close(w);
}

fn timeline_deleted() {
    let base = common::temp_dir("timeline-deleted");
    let dir = base.join("proj");
    write(
        &dir,
        &[("keep.tex", "keep\n"), ("sub/gone.tex", "gone 1\n")],
    );
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    let w = history_window(&dir, &paths);
    // `lemma DIR`: opening sub/gone.tex first does not make sub/ the project.
    w.pin_root();
    let gone = open(&w, &dir, "sub/gone.tex");
    let gone_path = gone.document().path().to_path_buf();
    save_as(&w, &gone, "gone 1\n");
    save_as(&w, &gone, "gone 2\n");
    w.close_editor(&gone, true);
    drop(gone);
    common::pump_for(Duration::from_millis(50));
    std::fs::remove_dir_all(dir.join("sub")).unwrap();
    let keep = open(&w, &dir, "keep.tex");
    let t = open_timeline(&w);
    let files = t.files();
    let index = history::project_dir(&paths.history_dir(), &dir).join("index.jsonl");
    assert_eq!(
        files.len(),
        2,
        "{files:?}\n{}",
        std::fs::read_to_string(&index).unwrap_or_default()
    );
    assert_eq!(
        files[0].path,
        keep.document().path(),
        "the active document first"
    );
    assert_eq!(
        files[1],
        history_ui::HistoryFile {
            path: gone_path.clone(),
            exists: false
        }
    );
    // keep.tex was never saved: nothing yet.
    assert_eq!(t.page(), "empty");
    assert!(t.select_file(&gone_path));
    common::pump_until(TIMEOUT, || {
        t.entries().len() == 2 && !t.is_busy() && t.page() == "preview"
    });
    assert!(
        t.preview_text().contains("+gone 2\n"),
        "{}",
        t.preview_text()
    );
    assert!(t.heading().1.ends_with("+1 −0"), "{:?}", t.heading());
    // Restored as a new file under review; rejecting it deletes it again (to the trash).
    let count = block_on(t.restore()).unwrap();
    assert_eq!(count, 1);
    assert_eq!(std::fs::read_to_string(&gone_path).unwrap(), "gone 2\n");
    let doc = w.workspace.get(&gone_path).unwrap();
    assert!(review_ui::is_new_file(&doc));
    review_ui::decide_document(&w, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !gone_path.exists());
    close(w);
}

fn timeline_empty() {
    let base = common::temp_dir("timeline-empty");
    let dir = base.join("proj");
    write(&dir, &[("a&b <x>.tex", "x\n")]);
    let dir = dir.canonicalize().unwrap();
    let paths = paths_in(&base.join("xdg"));
    // History off and nothing recorded: the empty state, and the banner says why.
    let w = window(&dir, &paths, |_| {});
    let menu = &w.header.menu_files;
    let found = (0..menu.n_items()).any(|i| {
        menu.item_attribute_value(i, "action", Some(glib::VariantTy::STRING))
            .and_then(|v| v.get::<String>())
            .as_deref()
            == Some("win.history")
    });
    assert!(found, "Local History… is in the file menu");
    let _editor = open(&w, &dir, "a&b <x>.tex");
    let before = common::logged_problems();
    let t = open_timeline(&w);
    assert_eq!(t.page(), "empty");
    assert!(t.banner_shown());
    assert!(!t.can_restore());
    assert_eq!(t.heading().0, "a&b <x>.tex");
    assert!(block_on(t.restore()).is_err(), "nothing selected");
    common::assert_no_new_problems(before);
    // Shown again while open: the same dialog.
    WidgetExt::activate_action(&w.window, "win.history", None).unwrap();
    assert!(Rc::ptr_eq(&history_ui::timeline(&w).unwrap(), &t));
    t.dialog.close();
    close(w);
}

/// Saves the window with the timeline over it, light and dark (`LEMMA_TEST_SNAPSHOTS`).
fn snapshot_dialog(state: &WindowState, name: &str) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let style = adw::StyleManager::default();
    for (scheme, suffix) in [
        (adw::ColorScheme::ForceLight, "light"),
        (adw::ColorScheme::ForceDark, "dark"),
    ] {
        style.set_color_scheme(scheme);
        assert!(block_on(testmode::wait_for_frames(&state.window, 2)));
        let png = PathBuf::from(&dir).join(format!("{name}-{suffix}.png"));
        block_on(testmode::snapshot_next_frame(&state.window, &png)).unwrap();
    }
    style.set_color_scheme(adw::ColorScheme::Default);
    assert!(block_on(testmode::wait_for_frames(&state.window, 1)));
}
