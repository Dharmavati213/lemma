//! The window shell: tabs, status bar, toggles, unsaved-changes dialogs, style schemes and
//! accelerators.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_window`.

mod common;

use adw::prelude::*;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{Config, KeysCfg};
use lemma_core::ProjectRoot;
use sourceview5::prelude::*;
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
