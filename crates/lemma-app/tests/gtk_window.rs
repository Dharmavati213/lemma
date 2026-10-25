//! The window shell: tabs, status bar, toggles, unsaved-changes dialogs, style schemes,
//! accelerators, and New File and Save As (`file_commands`, their file dialog answered by a test
//! hook).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_window`.

mod common;

use adw::prelude::*;
use lemma_agent::EditorRequest;
use lemma_app::agent::AgentState;
use lemma_app::agent_edits;
use lemma_app::file_commands::{self, SaveDialog};
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::{Config, KeysCfg};
use lemma_core::{Origin, ProjectRoot, TextEdit};
use sourceview5::prelude::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        ("opening_files_creates_and_selects_tabs", opening_files),
        ("status_bar_and_titles_follow_the_editor", status_and_titles),
        ("pane_toggles", pane_toggles),
        (
            "sidebar_overlay_is_dismissed_after_navigating",
            sidebar_overlay_is_dismissed,
        ),
        ("closing_a_dirty_tab_asks_first", closing_a_dirty_tab),
        ("closing_the_window_saves_on_request", closing_the_window),
        ("new_file_opens_a_tab_its_save_creates", new_file),
        ("save_as_writes_the_file_and_takes_the_tab", save_as),
        (
            "save_as_keeps_the_encoding_and_line_breaks",
            save_as_encoding,
        ),
        (
            "save_as_refuses_reviews_turns_and_open_files",
            save_as_refusals,
        ),
        (
            "style_scheme_follows_the_system_style",
            style_scheme_follows_style,
        ),
        ("accelerators_and_overrides", accelerators),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    let dir = common::temp_dir(name);
    for (file, text) in files {
        std::fs::write(dir.join(file), text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<lemma_app::editor::Editor> {
        glib::MainContext::default()
            .block_on(self.state.open_file(&self.dir.join(file)))
            .expect("open")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn selected_title(&self) -> String {
        self.state
            .tab_view
            .selected_page()
            .map(|p| p.title().to_string())
            .unwrap_or_default()
    }

    /// Answers the alert dialog that is open on the window.
    fn answer(&self, response: &str) {
        common::pump_until(TIMEOUT, || self.state.window.visible_dialog().is_some());
        let dialog = self.state.window.visible_dialog().unwrap();
        let alert = dialog
            .downcast::<adw::AlertDialog>()
            .expect("an alert dialog");
        alert.emit_by_name::<()>("response", &[&response]);
        alert.force_close();
        common::pump_for(Duration::from_millis(50));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn opening_files() {
    let f = fixture("tabs", &[("a.tex", "A\n"), ("b.tex", "B\n")]);
    assert_eq!(f.state.tab_view.n_pages(), 0);
    let a = f.open("a.tex");
    let b = f.open("b.tex");
    assert_eq!(f.state.tab_view.n_pages(), 2);
    assert_eq!(f.selected_title(), "b.tex");
    assert!(Rc::ptr_eq(&f.state.active_editor().unwrap(), &b));
    assert!(
        Rc::ptr_eq(&f.open("a.tex"), &a),
        "reopening selects the existing tab"
    );
    assert_eq!(f.state.tab_view.n_pages(), 2);
    assert_eq!(f.selected_title(), "a.tex");
    assert_eq!(f.state.editors().len(), 2);
    assert_eq!(f.state.workspace.documents().len(), 2);

    // Closing every tab empties the workspace.
    for _ in 0..2 {
        let page = f.state.tab_view.selected_page().unwrap();
        f.state.tab_view.close_page(&page);
    }
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert!(f.state.editors().is_empty());
    assert!(f.state.workspace.is_empty());
    assert!(f.state.active_editor().is_none());
}

fn status_and_titles() {
    let f = fixture(
        "status",
        &[("a.tex", "first line\nsecond line\n"), ("b.tex", "B\n")],
    );
    let a = f.open("a.tex");
    assert_eq!(f.state.status.file.text(), "a.tex");
    assert_eq!(f.state.status.position.text(), "Ln 1, Col 1");
    assert_eq!(f.state.header.title.title(), "a.tex");
    assert_eq!(f.state.window.title().unwrap(), "a.tex – Lemma");

    let buffer = a.buffer();
    buffer.place_cursor(&buffer.iter_at_line_offset(1, 3).unwrap());
    assert_eq!(f.state.status.position.text(), "Ln 2, Col 4");

    buffer.insert_at_cursor("X");
    assert_eq!(f.state.status.file.text(), "a.tex •");
    assert_eq!(f.state.header.title.title(), "a.tex •");
    assert_eq!(f.selected_title(), "• a.tex");

    f.open("b.tex");
    assert_eq!(f.state.status.file.text(), "b.tex");
    f.state.select_editor(&a);
    assert_eq!(f.state.status.file.text(), "a.tex •");

    WidgetExt::activate_action(&f.state.window, "win.save", None).unwrap();
    common::pump_until(TIMEOUT, || !a.document().is_dirty());
    assert_eq!(read(&f.path("a.tex")), "first line\nsecXond line\n");
    assert_eq!(f.state.status.file.text(), "a.tex");
    assert_eq!(f.selected_title(), "a.tex");
}

fn sidebar_overlay_is_dismissed() {
    use lemma_app::diagnostics_panel::Problem;
    use lemma_app::window::PAGE_PROBLEMS;
    use lemma_core::{DiagSource, Severity};

    let f = fixture("overlay", &[("a.tex", "one\ntwo\nthree\n")]);
    let editor = f.open("a.tex");
    let sidebar = &f.state.sidebar;

    // Docked (wide window): navigating leaves the sidebar alone.
    sidebar.split.set_collapsed(false);
    sidebar.show_page(PAGE_PROBLEMS);
    assert_eq!(
        sidebar.stack.visible_child_name().as_deref(),
        Some(PAGE_PROBLEMS)
    );
    assert!(sidebar.split.shows_sidebar());
    sidebar.dismiss_overlay();
    assert!(sidebar.split.shows_sidebar());

    // An overlay (narrow window) hides when a problem is opened from it, uncovering the editor.
    sidebar.split.set_collapsed(true);
    sidebar.show_page(PAGE_PROBLEMS);
    assert!(sidebar.split.shows_sidebar());
    let path = lemma_app::workspace::normalize(&f.path("a.tex"));
    let diagnostics = f
        .state
        .build
        .get()
        .expect("a build controller")
        .diagnostics();
    diagnostics.set(
        vec![Problem {
            path: path.clone(),
            line: Some(3),
            severity: Severity::Error,
            message: "Undefined control sequence.".into(),
            context: None,
            source: DiagSource::Tex,
        }],
        &path,
    );
    diagnostics.activate(0);
    assert!(!sidebar.split.shows_sidebar());
    common::pump_until(TIMEOUT, || editor.cursor_position().0 == 3);
}

fn pane_toggles() {
    let f = fixture("toggles", &[]);
    let toggle = |name: &str| {
        WidgetExt::activate_action(&f.state.window, name, None).unwrap();
    };
    assert!(f.state.pdf_pane.is_visible());
    toggle("win.toggle-pdf");
    assert!(!f.state.pdf_pane.is_visible());
    toggle("win.toggle-pdf");
    assert!(f.state.pdf_pane.is_visible());

    assert!(
        !f.state.agent.split.shows_sidebar(),
        "the agent pane starts hidden"
    );
    toggle("win.toggle-agent");
    assert!(f.state.agent.split.shows_sidebar());

    let shown = f.state.sidebar.split.shows_sidebar();
    toggle("win.toggle-sidebar");
    assert_eq!(f.state.sidebar.split.shows_sidebar(), !shown);

    // The sidebar pages exist under their names.
    for page in [
        lemma_app::window::PAGE_OUTLINE,
        lemma_app::window::PAGE_CHANGES,
        lemma_app::window::PAGE_PROBLEMS,
    ] {
        assert!(
            f.state.sidebar.stack.child_by_name(page).is_some(),
            "{page}"
        );
    }
}

fn closing_a_dirty_tab() {
    let f = fixture("close-tab", &[("a.tex", "A\n")]);
    let a = f.open("a.tex");
    a.buffer().insert_at_cursor("1");

    let page = f.state.tab_view.page(a.widget());
    f.state.tab_view.close_page(&page);
    f.answer("cancel");
    assert_eq!(f.state.tab_view.n_pages(), 1, "cancel keeps the tab");
    assert!(a.document().is_dirty());

    f.state.tab_view.close_page(&page);
    f.answer("save");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert_eq!(read(&f.path("a.tex")), "1A\n", "save writes before closing");

    let a = f.open("a.tex");
    a.buffer().insert_at_cursor("2");
    f.state
        .tab_view
        .close_page(&f.state.tab_view.page(a.widget()));
    f.answer("discard");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert_eq!(
        read(&f.path("a.tex")),
        "1A\n",
        "discard leaves the file alone"
    );

    // A clean tab closes without asking.
    let a = f.open("a.tex");
    f.state
        .tab_view
        .close_page(&f.state.tab_view.page(a.widget()));
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert!(f.state.window.visible_dialog().is_none());
}

fn closing_the_window() {
    let f = fixture("close-window", &[("a.tex", "A\n"), ("b.tex", "B\n")]);
    let a = f.open("a.tex");
    let b = f.open("b.tex");
    a.buffer().insert_at_cursor("1");
    b.buffer().insert_at_cursor("2");
    f.state.window.close();
    f.answer("cancel");
    assert!(f.state.window.is_visible(), "cancel keeps the window open");

    f.state.window.close();
    f.answer("save");
    common::pump_until(TIMEOUT, || !f.state.window.is_visible());
    assert_eq!(read(&f.path("a.tex")), "1A\n");
    assert_eq!(read(&f.path("b.tex")), "2B\n");
}

fn style_scheme_follows_style() {
    let f = fixture("scheme", &[("a.tex", "A\n")]);
    let a = f.open("a.tex");
    let style = adw::StyleManager::default();
    let scheme = || a.buffer().style_scheme().map(|s| s.id().to_string());
    style.set_color_scheme(adw::ColorScheme::ForceDark);
    assert_eq!(scheme().as_deref(), Some("Adwaita-dark"));
    style.set_color_scheme(adw::ColorScheme::ForceLight);
    assert_eq!(scheme().as_deref(), Some("Adwaita"));
    style.set_color_scheme(adw::ColorScheme::Default);
}

fn accelerators() {
    let app = common::application();
    let accels = |action: &str| -> Vec<String> {
        app.accels_for_action(action)
            .iter()
            .map(|a| a.to_string())
            .collect()
    };
    lemma_app::app::set_accels(&app, &KeysCfg::default());
    assert_eq!(accels("win.save"), ["<Control>s"]);
    assert_eq!(accels("win.build"), ["F5"]);
    assert_eq!(accels("win.toggle-pdf"), ["F8"]);
    assert_eq!(accels("win.toggle-sidebar"), ["F9"]);
    assert_eq!(accels("win.toggle-agent"), ["F10"]);
    assert_eq!(accels("win.close-tab"), ["<Control>w"]);
    assert_eq!(accels("app.open"), ["<Control>o"]);
    assert_eq!(accels("app.quit"), ["<Control>q"]);
    assert_eq!(accels("win.new-file"), ["<Control>n"]);
    assert_eq!(
        accels("win.save-as")
            .iter()
            .map(gtk::accelerator_parse)
            .collect::<Vec<_>>(),
        [gtk::accelerator_parse("<Control><Shift>s")]
    );
    assert_eq!(accels("win.goto-line"), ["<Control>g"]);

    let mut keys = KeysCfg::default();
    keys.bindings
        .insert("win.build".into(), "<Control>b".into());
    keys.bindings.insert("win.toggle-pdf".into(), String::new());
    keys.bindings
        .insert("win.save".into(), "not an accelerator".into());
    lemma_app::app::set_accels(&app, &keys);
    assert_eq!(accels("win.build"), ["<Control>b"]);
    assert!(accels("win.toggle-pdf").is_empty());
    assert_eq!(
        accels("win.save"),
        ["<Control>s"],
        "invalid overrides are ignored"
    );
    lemma_app::app::set_accels(&app, &KeysCfg::default());
}

// ----- New File and Save As -----

/// Answers New File's and Save As's file dialog with `answers` in turn (`None`: Cancel) and
/// records what it asked; dropping it shows the real dialog again.
struct Dialogs {
    asked: Rc<RefCell<Vec<SaveDialog>>>,
}

impl Dialogs {
    fn answer(answers: Vec<Option<PathBuf>>) -> Self {
        let asked: Rc<RefCell<Vec<SaveDialog>>> = Rc::default();
        let log = asked.clone();
        let answers = RefCell::new(answers.into_iter());
        file_commands::set_dialog_hook(Some(Box::new(move |question| {
            log.borrow_mut().push(question.clone());
            answers.borrow_mut().next().flatten()
        })));
        Self { asked }
    }

    fn asked(&self) -> Vec<SaveDialog> {
        self.asked.borrow().clone()
    }
}

impl Drop for Dialogs {
    fn drop(&mut self) {
        file_commands::set_dialog_hook(None);
    }
}

fn activate(f: &Fixture, action: &str) {
    WidgetExt::activate_action(&f.state.window, action, None).unwrap();
}

fn active_path(f: &Fixture) -> Option<PathBuf> {
    f.state
        .active_editor()
        .map(|e| e.document().path().to_path_buf())
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn new_file() {
    let f = fixture("new-file", &[("a.tex", "A\n"), ("old.tex", "old\n")]);
    let dir = normalize(&f.dir);
    std::fs::create_dir_all(dir.join("ch")).unwrap();
    std::fs::write(dir.join("ch/c.tex"), "C\n").unwrap();
    // The first document decides the project root: `dir`.
    f.open("a.tex");
    f.open("ch/c.tex");
    let dialogs = Dialogs::answer(vec![
        Some(dir.join("ch/new.tex")),
        None,
        Some(dir.join("old.tex")),
    ]);
    // In the active document's folder, named `untitled.tex`; the file chosen opens in a tab,
    // empty and saved, and is not on disk yet.
    activate(&f, "win.new-file");
    common::pump_until(TIMEOUT, || active_path(&f) == Some(dir.join("ch/new.tex")));
    assert_eq!(
        dialogs.asked(),
        [SaveDialog {
            title: "New File",
            folder: dir.join("ch"),
            name: "untitled.tex".into(),
        }]
    );
    let new = f.state.active_editor().unwrap();
    assert_eq!(new.document().text(), "");
    assert!(!new.document().is_dirty());
    assert!(!dir.join("ch/new.tex").exists());
    assert_eq!(f.state.tab_view.n_pages(), 3);
    // Its first save creates it.
    new.buffer().insert_at_cursor("\\section{New}\n");
    activate(&f, "win.save");
    common::pump_until(TIMEOUT, || !new.document().is_dirty());
    assert_eq!(read(&dir.join("ch/new.tex")), "\\section{New}\n");

    // Cancelled: nothing happens.
    activate(&f, "win.new-file");
    common::pump_until(TIMEOUT, || dialogs.asked().len() == 2);
    common::pump_for(Duration::from_millis(50));
    assert_eq!(f.state.tab_view.n_pages(), 3);

    // Without a document, in the project root. A file that exists opens as it is.
    for editor in f.state.editors() {
        f.state.close_editor(&editor, true);
    }
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    activate(&f, "win.new-file");
    common::pump_until(TIMEOUT, || active_path(&f) == Some(dir.join("old.tex")));
    assert_eq!(dialogs.asked()[2].folder, dir);
    assert_eq!(f.state.active_editor().unwrap().document().text(), "old\n");
    assert_eq!(read(&dir.join("old.tex")), "old\n");
}

fn save_as() {
    let f = fixture(
        "save-as",
        &[("a.tex", "one\ntwo\nthree\n"), ("b.tex", "B\n")],
    );
    let dir = normalize(&f.dir);
    let a = f.open("a.tex");
    f.open("b.tex");
    f.state.select_editor(&a);
    let buffer = a.buffer();
    buffer.place_cursor(&buffer.iter_at_line_offset(1, 2).unwrap());
    buffer.insert_at_cursor("X");
    let cursor = a.cursor_offset();
    let dialogs = Dialogs::answer(vec![Some(dir.join("c.tex"))]);
    activate(&f, "win.save-as");
    common::pump_until(TIMEOUT, || active_path(&f) == Some(dir.join("c.tex")));
    assert_eq!(
        dialogs.asked(),
        [SaveDialog {
            title: "Save As",
            folder: dir.clone(),
            name: "a.tex".into(),
        }]
    );
    // The new file has the text with its unsaved change; the old one is as it was on disk, and
    // its document is gone.
    assert_eq!(read(&dir.join("c.tex")), "one\ntwXo\nthree\n");
    assert_eq!(read(&dir.join("a.tex")), "one\ntwo\nthree\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 2);
    assert!(f.state.workspace.get(&dir.join("a.tex")).is_none());
    // In the old tab's place, selected, the cursor where it was, saved, with an undo history of
    // its own.
    let c = f.state.active_editor().unwrap();
    assert_eq!(
        f.state.tab_view.nth_page(0).child(),
        *c.widget().upcast_ref::<gtk::Widget>()
    );
    assert_eq!(f.selected_title(), "c.tex");
    assert_eq!(c.cursor_offset(), cursor);
    assert!(!c.document().is_dirty());
    assert!(!c.buffer().can_undo());
    // It joins the project index as any file opened does.
    common::pump_until(TIMEOUT, || {
        f.state
            .workspace
            .index
            .borrow()
            .files
            .contains_key(&dir.join("c.tex"))
    });
    // Its own name is a plain save.
    c.buffer().insert_at_cursor("Y");
    block_on(file_commands::save_as(
        &f.state,
        &c,
        &dir.join("c.tex"),
        false,
    ))
    .unwrap();
    common::pump_until(TIMEOUT, || !c.document().is_dirty());
    assert_eq!(read(&dir.join("c.tex")), "one\ntwXYo\nthree\n");
    assert_eq!(f.state.tab_view.n_pages(), 2);
}

fn save_as_encoding() {
    let f = fixture("save-as-encoding", &[]);
    let dir = normalize(&f.dir);
    std::fs::write(dir.join("crlf.tex"), "one\r\ntwo\r\n").unwrap();
    std::fs::write(dir.join("latin.tex"), b"caf\xe9\n").unwrap();
    // Line breaks as they were.
    let crlf = f.open("crlf.tex");
    crlf.buffer().insert_at_cursor("X");
    block_on(file_commands::save_as(
        &f.state,
        &crlf,
        &dir.join("crlf2.tex"),
        false,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(dir.join("crlf2.tex")).unwrap(),
        b"Xone\r\ntwo\r\n"
    );
    // The encoding the file was read in, also for the new document's next saves.
    let latin = f.open("latin.tex");
    let charset = latin.document().charset();
    assert!(charset.is_some(), "not UTF-8");
    latin.buffer().insert_at_cursor("é");
    block_on(file_commands::save_as(
        &f.state,
        &latin,
        &dir.join("latin2.tex"),
        false,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(dir.join("latin2.tex")).unwrap(),
        b"\xe9caf\xe9\n"
    );
    assert_eq!(std::fs::read(dir.join("latin.tex")).unwrap(), b"caf\xe9\n");
    let latin2 = f.state.active_editor().unwrap();
    assert_eq!(latin2.document().path(), dir.join("latin2.tex"));
    assert_eq!(latin2.document().charset(), charset);
    // Without overwriting (`:sav`), a file that exists is refused; with it (`:sav!`, or the
    // dialog, which asked) it is replaced.
    let e = block_on(file_commands::save_as(
        &f.state,
        &latin2,
        &dir.join("crlf.tex"),
        false,
    ))
    .unwrap_err();
    assert!(e.contains("exists"), "{e}");
    assert_eq!(
        std::fs::read(dir.join("crlf.tex")).unwrap(),
        b"one\r\ntwo\r\n"
    );
    assert!(f.state.workspace.get(&dir.join("crlf.tex")).is_none());
    assert_eq!(active_path(&f), Some(dir.join("latin2.tex")));
    block_on(file_commands::save_as(
        &f.state,
        &latin2,
        &dir.join("crlf.tex"),
        true,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(dir.join("crlf.tex")).unwrap(),
        b"\xe9caf\xe9\n"
    );
}

fn save_as_refusals() {
    let f = fixture(
        "save-as-refusals",
        &[("a.tex", "one\ntwo\n"), ("b.tex", "B\n")],
    );
    let dir = normalize(&f.dir);
    let b = f.open("b.tex");
    let a = f.open("a.tex");
    // Another tab's file, for Save As and for a copy.
    let e = block_on(file_commands::save_as(
        &f.state,
        &a,
        &dir.join("b.tex"),
        true,
    ))
    .unwrap_err();
    assert!(e.contains("open in another tab"), "{e}");
    let e = block_on(file_commands::write_copy(
        &f.state,
        a.document(),
        &dir.join("b.tex"),
        true,
    ))
    .unwrap_err();
    assert!(e.contains("open in another tab"), "{e}");
    assert_eq!(read(&dir.join("b.tex")), "B\n");
    // A closed document that holds only its file (a project-wide replace leaves such ones) is
    // no reason to refuse: it leaves the workspace. One with unsaved changes is.
    std::fs::write(dir.join("e.tex"), "E\n").unwrap();
    let closed = block_on(f.state.workspace.get_or_load(&dir.join("e.tex"))).unwrap();
    closed
        .apply_edits(&[TextEdit::insert(0, "%")], Origin::User)
        .unwrap();
    let e = block_on(file_commands::write_copy(
        &f.state,
        a.document(),
        &dir.join("e.tex"),
        true,
    ))
    .unwrap_err();
    assert!(e.contains("not saved"), "{e}");
    block_on(closed.save()).unwrap();
    block_on(file_commands::write_copy(
        &f.state,
        a.document(),
        &dir.join("e.tex"),
        true,
    ))
    .unwrap();
    assert_eq!(read(&dir.join("e.tex")), "one\ntwo\n");
    assert!(f.state.workspace.get(&dir.join("e.tex")).is_none());

    // Agent changes that are not decided: no Save As (not even its dialog). A copy holds them.
    let reply = block_on(agent_edits::apply(
        &f.state.workspace,
        1,
        EditorRequest::Edit {
            path: "a.tex".into(),
            old_string: "two".into(),
            new_string: "TWO".into(),
            replace_all: false,
        },
    ));
    assert!(!reply.is_error);
    let e = block_on(file_commands::save_as(
        &f.state,
        &a,
        &dir.join("c.tex"),
        false,
    ))
    .unwrap_err();
    assert!(e.contains("not decided"), "{e}");
    let dialogs = Dialogs::answer(vec![Some(dir.join("c.tex"))]);
    activate(&f, "win.save-as");
    common::pump_for(Duration::from_millis(100));
    assert!(dialogs.asked().is_empty());
    assert!(!dir.join("c.tex").exists());
    assert!(Rc::ptr_eq(&f.state.active_editor().unwrap(), &a));
    block_on(file_commands::write_copy(
        &f.state,
        a.document(),
        &dir.join("copy.tex"),
        false,
    ))
    .unwrap();
    assert_eq!(read(&dir.join("copy.tex")), "one\nTWO\n");
    assert!(
        f.state.workspace.get(&dir.join("copy.tex")).is_none(),
        "no document stays for the copy"
    );
    // A copy replaces a file that exists only when asked to (`:w!`).
    let e = block_on(file_commands::write_copy(
        &f.state,
        b.document(),
        &dir.join("copy.tex"),
        false,
    ))
    .unwrap_err();
    assert!(e.contains("exists"), "{e}");
    assert_eq!(read(&dir.join("copy.tex")), "one\nTWO\n");

    // While a turn runs, neither.
    let agent = f.state.agent_controller().unwrap().clone();
    agent.set_state(AgentState::Running { turn: 1 });
    let e = block_on(file_commands::save_as(
        &f.state,
        &b,
        &dir.join("d.tex"),
        false,
    ))
    .unwrap_err();
    assert!(e.contains("still working"), "{e}");
    let e = block_on(file_commands::write_copy(
        &f.state,
        b.document(),
        &dir.join("d.tex"),
        false,
    ))
    .unwrap_err();
    assert!(e.contains("still working"), "{e}");
    assert!(!dir.join("d.tex").exists());
    agent.set_state(AgentState::Idle);
    block_on(file_commands::save_as(
        &f.state,
        &b,
        &dir.join("d.tex"),
        false,
    ))
    .unwrap();
    assert_eq!(read(&dir.join("d.tex")), "B\n");
    assert_eq!(active_path(&f), Some(dir.join("d.tex")));
}
