//! WP3.5: the command palette, quick open and the keyboard shortcuts window.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_palette`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `palette-light.png`, `palette-dark.png`,
//! `quick-open.png` and `shortcuts.png` there.

mod broadway_input;
mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::app::{self, ShortcutGroup};
use lemma_app::editor::Editor;
use lemma_app::palette::{self, Command};
use lemma_app::quick_open::{self, FileCandidate, Source};
use lemma_app::shortcuts;
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::{Config, Paths};
use lemma_core::ProjectRoot;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[("palette_real_keys_child", real_keys::child)]);
    }
    common::run(&[
        (
            "palette_lists_every_action_with_its_accelerator",
            every_action,
        ),
        ("palette_follows_keys_overrides", keys_overrides),
        (
            "palette_quick_actions_and_unavailable_reasons",
            quick_actions,
        ),
        ("palette_ranking_table", ranking_table),
        ("palette_runs_the_chosen_command", runs_command),
        ("palette_gives_the_focus_back_before_running", focus_back),
        ("palette_recent_commands_persist", recent_persist),
        ("palette_rows_show_keycaps_and_matches", rows_widgets),
        ("quick_open_orders_tabs_index_then_walk", quick_open_order),
        ("quick_open_ranks_and_opens", quick_open_ranks),
        ("pickers_replace_each_other", pickers_replace),
        ("shortcuts_window_lists_every_group", shortcuts_groups),
        (
            "shortcuts_window_follows_overrides_and_vim",
            shortcuts_overrides,
        ),
        ("shortcut_labels_from_key_names", shortcut_labels),
        ("palette_demo_resolves", demo),
        ("palette_keys_on_real_key_events", real_keys::parent),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

const MAIN: &str = "\\documentclass{article}\n\\begin{document}\n\\input{chapters/yoneda}\n\
                    Hello.\n\\bibliography{refs}\n\\end{document}\n";

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, files, None, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
    paths: Option<Paths>,
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.history.enabled = false;
    configure(&mut config);
    let scripted = paths.is_none();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), paths, ws, scripted);
    state.set_session_autosave(Some(false));
    app::set_accels(&state.app, &state.config.keys);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        normalize(&self.dir.join(file))
    }

    fn active_path(&self) -> Option<PathBuf> {
        self.state
            .active_editor()
            .map(|e| e.document().path().to_path_buf())
    }

    fn command(&self, name: &str) -> Command {
        palette::commands(&self.state)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is in the palette"))
    }

    /// Closes whatever picker or dialog is open and waits until it is gone.
    fn close_dialogs(&self) {
        palette::close(&self.state);
        if let Some(d) = self.state.window.visible_dialog() {
            d.force_close();
        }
        common::pump_until(TIMEOUT, || self.state.window.visible_dialog().is_none());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(d) = self.state.window.visible_dialog() {
            d.force_close();
        }
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// Every action registered on the window and the application that takes no parameter.
fn registered_actions(state: &WindowState) -> Vec<String> {
    let mut out = Vec::new();
    let groups: [(&str, gio::ActionGroup); 2] = [
        ("win", state.window.clone().upcast()),
        ("app", state.app.clone().upcast()),
    ];
    for (prefix, group) in groups {
        for name in group.list_actions() {
            if group.action_parameter_type(&name).is_none() {
                out.push(format!("{prefix}.{name}"));
            }
        }
    }
    out.sort();
    out
}

fn every_action() {
    let f = fixture("every", &[("main.tex", MAIN)]);
    let _editor = f.open("main.tex");
    let commands = palette::commands(&f.state);
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "no command is listed twice");
    // Every registered action appears, with its accelerators from the registry.
    for action in registered_actions(&f.state) {
        let c = commands
            .iter()
            .find(|c| c.name == action)
            .unwrap_or_else(|| panic!("{action} is missing from the palette"));
        let accels: Vec<String> = f
            .state
            .app
            .accels_for_action(&action)
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(c.accels, accels, "{action}");
        assert!(!c.label.is_empty(), "{action} has a label");
    }
    // Actions nobody described get a label from their name (listed, so they can be described).
    let undescribed: Vec<String> = commands
        .iter()
        .filter(|c| !c.quick && app::action_info(&c.name).is_none())
        .map(|c| format!("{} ({})", c.name, c.label))
        .collect();
    println!("    actions without a description: {undescribed:?}");
    // Described actions keep their label and group.
    for info in app::ACTIONS {
        if let Some(c) = commands.iter().find(|c| c.name == info.name) {
            assert_eq!(c.label, info.label, "{}", info.name);
            assert_eq!(c.group, info.group, "{}", info.name);
        }
    }
    // The M3 actions are all there.
    for name in [
        "win.command-palette",
        "win.quick-open",
        "win.project-search",
        "win.shortcuts",
        "win.preferences",
        "win.pdf-search",
        "win.pdf-detach",
        "win.toggle-pdf-sidebar",
        "win.goto-definition",
        "win.goto-back",
        "win.goto-forward",
        "win.show-hover",
        "win.find-references",
        "win.rename-label",
        "win.jump-pair",
        "win.history",
        "win.word-count",
    ] {
        assert!(names.contains(&name), "{name} is in the palette");
    }
    // Default accelerators.
    let accels = |name: &str| canon_all(&f.command(name).accels);
    assert_eq!(accels("win.command-palette"), canon("<Control><Shift>p"));
    assert_eq!(accels("win.quick-open"), canon("<Control>p"));
    assert_eq!(accels("win.shortcuts"), canon("<Control>question"));
    assert_eq!(accels("win.build"), canon("F5"));
    assert_eq!(accels("win.review-accept"), canon("<Alt>y"));
    assert!(f.command("win.history").accels.is_empty());
    // Switches show their state.
    assert_eq!(f.command("win.toggle-vim").state, Some(false));
    assert_eq!(f.command("win.build").state, None);
    // In registry order (the test application has no `app.*` actions).
    let registered = registered_actions(&f.state);
    let first = app::ACTIONS
        .iter()
        .find(|a| registered.iter().any(|r| r == a.name))
        .unwrap();
    assert_eq!(names[0], first.name);
}

/// `accel` (alternatives separated by spaces) in GTK's canonical spelling
/// (`<Control><Shift>p` → `<Shift><Control>p`).
fn canon(accel: &str) -> String {
    accel
        .split(' ')
        .filter(|a| !a.is_empty())
        .map(|a| {
            let (key, mods) = gtk::accelerator_parse(a).unwrap_or_else(|| panic!("{a}"));
            gtk::accelerator_name(key, mods).to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn canon_all(accels: &[String]) -> String {
    canon(&accels.join(" "))
}

fn keys_overrides() {
    let f = fixture("overrides", &[("main.tex", MAIN)]);
    let _editor = f.open("main.tex");
    let mut config = (*f.state.config.get()).clone();
    for (action, accel) in [
        ("win.build", "<Control>b"),
        ("win.save", ""),
        ("win.preferences", "<Control><Alt>comma"),
        ("win.history", "<Control><Shift>h"),
    ] {
        config.keys.bindings.insert(action.into(), accel.into());
    }
    f.state.set_config(Rc::new(config));
    let accels = |name: &str| canon_all(&f.command(name).accels);
    assert_eq!(accels("win.build"), canon("<Control>b"));
    assert!(f.command("win.save").accels.is_empty(), "removed binding");
    assert_eq!(accels("win.preferences"), canon("<Control><Alt>comma"));
    assert_eq!(accels("win.history"), canon("<Control><Shift>h"));
    // Every action still shows exactly what the registry has.
    for c in palette::commands(&f.state) {
        let accels: Vec<String> = f
            .state
            .app
            .accels_for_action(&c.name)
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(c.accels, accels, "{}", c.name);
    }
    // And the rows the palette shows.
    palette::show(&f.state);
    palette::set_query(&f.state, "build");
    let rows = palette::visible_rows(&f.state);
    let build = rows.iter().find(|r| r.key == "win.build").unwrap();
    assert_eq!(canon(&build.accel), canon("<Control>b"));
    palette::set_query(&f.state, "save");
    let rows = palette::visible_rows(&f.state);
    assert_eq!(rows[0].key, "win.save");
    assert_eq!(rows[0].accel, "");
    // Back to the defaults.
    let mut config = (*f.state.config.get()).clone();
    config.keys.bindings.clear();
    f.state.set_config(Rc::new(config));
    assert_eq!(accels("win.build"), canon("F5"));
    assert_eq!(accels("win.save"), canon("<Control>s"));
    f.close_dialogs();
}

fn quick_actions() {
    let f = fixture("quick", &[("main.tex", MAIN)]);
    let editor = f.open("main.tex");
    let quick: Vec<Command> = palette::commands(&f.state)
        .into_iter()
        .filter(|c| c.quick)
        .collect();
    let labels: Vec<&str> = quick.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Explain",
            "Proofread",
            "Make this proof more detailed",
            "Convert to align",
            "Translate to English"
        ],
        "the quick actions of prompts.toml, not the Problems-only one"
    );
    for c in &quick {
        assert_eq!(c.group, ShortcutGroup::Agent);
        assert!(c.name.starts_with("win.quick-"), "{}", c.name);
        assert!(!c.enabled, "no selection");
        assert_eq!(c.reason.as_deref(), Some("Select some text first"));
    }
    // With a selection they are available.
    let buffer = editor.buffer();
    let (start, end) = (buffer.iter_at_offset(0), buffer.iter_at_offset(9));
    buffer.select_range(&start, &end);
    common::pump_for(Duration::from_millis(20));
    let c = f.command("win.quick-proofread");
    assert!(c.enabled && c.reason.is_none());
    // A disabled action shows its reason, and choosing it rings the bell and keeps the
    // palette open.
    let pdf = f.command("win.pdf-search");
    assert!(!pdf.enabled);
    assert_eq!(
        pdf.reason.as_deref(),
        Some("Click into the PDF preview first")
    );
    palette::show(&f.state);
    palette::set_query(&f.state, "search in pdf");
    let rows = palette::visible_rows(&f.state);
    assert_eq!(rows[0].key, "win.pdf-search");
    assert!(!rows[0].enabled);
    assert!(
        rows[0]
            .subtitle
            .contains("Click into the PDF preview first")
    );
    assert!(!palette::activate_selected(&f.state));
    common::pump_for(Duration::from_millis(30));
    assert!(palette::is_open(&f.state));
    f.close_dialogs();
}

/// The names of the best matches of `query` (with `recent`), from the real commands.
fn ranked(commands: &[Command], query: &str, recent: &[&str]) -> Vec<String> {
    let recent: Vec<String> = recent.iter().map(|s| (*s).to_owned()).collect();
    palette::rank(commands, query, &recent)
        .iter()
        .map(|r| commands[r.index].name.clone())
        .collect()
}

fn ranking_table() {
    let f = fixture("ranking", &[("main.tex", MAIN)]);
    let _editor = f.open("main.tex");
    let commands = palette::commands(&f.state);
    // (query, recent, expected best match)
    let table: &[(&str, &[&str], &str)] = &[
        ("save", &[], "win.save"),
        ("save all", &[], "win.save-all"),
        ("palette", &[], "win.command-palette"),
        ("prefs", &[], "win.preferences"),
        ("prefernces", &[], "win.preferences"),
        ("keyboard", &[], "win.shortcuts"),
        ("build", &[], "win.build"),
        ("stop build", &[], "win.stop-build"),
        ("clean", &[], "win.clean-build"),
        ("next problem", &[], "win.problem-next"),
        ("prev problem", &[], "win.problem-prev"),
        ("go def", &[], "win.goto-definition"),
        ("references", &[], "win.find-references"),
        ("rename", &[], "win.rename-label"),
        ("accept", &[], "win.review-accept"),
        ("accept all", &[], "win.review-accept-all"),
        ("all accept", &[], "win.review-accept-all"),
        ("accept file", &[], "win.review-accept-file"),
        ("zoom in", &[], "win.pdf-zoom-in"),
        ("dark", &[], "win.pdf-invert"),
        ("thumb", &[], "win.toggle-pdf-sidebar"),
        ("vim", &[], "win.toggle-vim"),
        ("toggle pdf", &[], "win.toggle-pdf"),
        ("sync", &[], "win.sync-forward"),
        ("hist", &[], "win.history"),
        ("proofread", &[], "win.quick-proofread"),
        ("go to file", &[], "win.quick-open"),
        // Recent commands win ties, never a clearly better match.
        (
            "accept",
            &["win.review-accept-all"],
            "win.review-accept-all",
        ),
        ("build", &["win.stop-build"], "win.build"),
        ("", &["win.build", "win.save"], "win.build"),
    ];
    let mut failures = Vec::new();
    for (query, recent, expected) in table {
        let got = ranked(&commands, query, recent);
        if got.first().map(String::as_str) != Some(*expected) {
            failures.push(format!(
                "{query:?} (recent {recent:?}): expected {expected}, got {:?}",
                &got[..got.len().min(4)]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Without a query: the recent ones in order, then the registry order.
    let all = ranked(&commands, "", &["win.save", "win.build"]);
    let rest: Vec<&str> = commands
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| !["win.save", "win.build"].contains(n))
        .collect();
    assert_eq!(all[..2], ["win.save", "win.build"]);
    assert_eq!(all[2..], rest);
    assert_eq!(all.len(), commands.len());
    // Nothing matches nonsense; every word must match.
    assert!(ranked(&commands, "qxzw", &[]).is_empty());
    assert!(ranked(&commands, "zoom qxzw", &[]).is_empty());
    // Matched characters are reported for bold rendering.
    let r = &palette::rank(&commands, "zoom", &[])[0];
    assert_eq!(&commands[r.index].label[..4], "Zoom");
    assert_eq!(r.positions, [0, 1, 2, 3]);
}

fn runs_command() {
    let f = fixture("runs", &[("main.tex", MAIN)]);
    let editor = f.open("main.tex");
    palette::set_recent(Vec::new());
    editor.view().grab_focus();
    let shown = f.state.sidebar.split.shows_sidebar();
    palette::show(&f.state);
    assert!(palette::is_open(&f.state));
    assert_eq!(
        f.state.window.visible_dialog().as_ref(),
        palette::dialog(&f.state).as_ref()
    );
    // The full list first, then filtered as the query is typed.
    assert_eq!(
        palette::visible(&f.state).len(),
        palette::commands(&f.state).len()
    );
    palette::set_query(&f.state, "sidebar");
    assert_eq!(
        palette::selected(&f.state).as_deref(),
        Some("win.toggle-sidebar")
    );
    // Down and Up move the selection and stop at the ends.
    let visible = palette::visible(&f.state);
    palette::move_selection(&f.state, 1);
    assert_eq!(palette::selected(&f.state).as_ref(), visible.get(1));
    palette::move_selection(&f.state, -5);
    assert_eq!(
        palette::selected(&f.state).as_deref(),
        Some("win.toggle-sidebar")
    );
    palette::move_selection(&f.state, 100);
    assert_eq!(palette::selected(&f.state).as_ref(), visible.last());
    palette::move_selection(&f.state, -100);
    // Enter: the dialog closes, then the command runs.
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || {
        f.state.sidebar.split.shows_sidebar() != shown && !palette::is_open(&f.state)
    });
    assert!(f.state.window.visible_dialog().is_none());
    assert_eq!(palette::recent(), ["win.toggle-sidebar"]);
    // Scripted windows never write the recent commands.
    assert!(f.state.paths.is_none());
    // The list is the same again; the recent command is first without a query.
    palette::show(&f.state);
    assert_eq!(
        palette::visible(&f.state).first().map(String::as_str),
        Some("win.toggle-sidebar")
    );
    let first = &palette::visible_rows(&f.state)[0];
    assert_eq!(first.badge.as_deref(), Some("Recent"));
    // Escape: nothing runs.
    palette::close(&f.state);
    common::pump_until(TIMEOUT, || !palette::is_open(&f.state));
    common::pump_for(Duration::from_millis(30));
    assert_ne!(f.state.sidebar.split.shows_sidebar(), shown);
    assert_eq!(palette::recent(), ["win.toggle-sidebar"]);
    assert_eq!(editor.document().text(), MAIN);
}

fn focus_back() {
    let f = fixture("focus-back", &[("main.tex", MAIN)]);
    let pdf_file = f.dir.join("main.pdf");
    std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf_file).unwrap();
    let _editor = f.open("main.tex");
    let pdf = f.state.pdf.get().unwrap();
    f.state.pdf_pane.set_visible(true);
    pdf.open(&pdf_file);
    common::pump_until(TIMEOUT, || {
        pdf.pdf().is_some() && pdf.view().is_mapped() && pdf.view().height() > 0
    });
    assert!(pdf.view().grab_focus());
    let search = f.state.window.lookup_action("pdf-search").unwrap();
    common::pump_until(TIMEOUT, || search.is_enabled());
    // A command added at run time that records where the focus is when it runs.
    let focus_at_run: Rc<std::cell::RefCell<Option<bool>>> = Rc::default();
    let log = focus_at_run.clone();
    let (view, window) = (pdf.view().clone(), f.state.window.clone());
    let probe = gio::SimpleAction::new("test-focus-probe", None);
    probe.connect_activate(move |_, _| {
        let in_pdf = gtk::prelude::GtkWindowExt::focus(&window)
            .is_some_and(|w| w == *view.upcast_ref::<gtk::Widget>() || w.is_ancestor(&view));
        log.replace(Some(in_pdf));
    });
    f.state.window.add_action(&probe);
    app::describe_action(
        "win.test-focus-probe",
        "Focus Probe",
        ShortcutGroup::General,
        None,
    );
    // The palette sees the window behind it: the PDF has the focus, so its search is there.
    palette::show(&f.state);
    let c = f.command("win.pdf-search");
    assert!(c.enabled && c.reason.is_none(), "{c:?}");
    palette::set_query(&f.state, "focus probe");
    assert_eq!(
        palette::selected(&f.state).as_deref(),
        Some("win.test-focus-probe")
    );
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || focus_at_run.borrow().is_some());
    assert_eq!(
        *focus_at_run.borrow(),
        Some(true),
        "the PDF had the focus back when the command ran"
    );
    assert!(search.is_enabled());
    f.state.window.remove_action("test-focus-probe");
}

fn recent_persist() {
    let f = fixture("recent", &[("main.tex", MAIN)]);
    let xdg = f.dir.join("xdg");
    let paths = Paths::from_vars(|name| match name {
        "HOME" => Some(xdg.join("home").into()),
        "XDG_CONFIG_HOME" => Some(xdg.join("config").into()),
        "XDG_STATE_HOME" => Some(xdg.join("state").into()),
        "XDG_DATA_HOME" => Some(xdg.join("data").into()),
        "XDG_CACHE_HOME" => Some(xdg.join("cache").into()),
        "XDG_RUNTIME_DIR" => Some(xdg.join("run").into()),
        _ => None,
    })
    .unwrap();
    assert!(paths.state_dir.starts_with(&f.dir));
    let file = paths.state_dir.join("palette.json");
    drop(f);
    let g = fixture_with(
        "recent-2",
        &[("main.tex", MAIN)],
        Some(paths.clone()),
        |_| {},
    );
    let _editor = g.open("main.tex");
    // A file from an earlier run is read the first time (invalid names are dropped).
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(&file, r#"["win.build", "not an action", "win.save"]"#).unwrap();
    palette::forget_recent();
    assert_eq!(palette::recent_for(&g.state), ["win.build", "win.save"]);
    palette::show(&g.state);
    palette::set_query(&g.state, "vim mode");
    assert_eq!(
        palette::selected(&g.state).as_deref(),
        Some("win.toggle-vim")
    );
    assert!(palette::activate_selected(&g.state));
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&file).is_ok_and(|t| t.starts_with("[\"win.toggle-vim\""))
    });
    let saved: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved, ["win.toggle-vim", "win.build", "win.save"]);
    assert!(g.state.vim_enabled(), "the command ran");
    g.state.set_vim_enabled(false);
    palette::set_recent(Vec::new());
}

/// Every widget inside `widget`, depth first.
fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.last_child();
        while let Some(c) = child {
            child = c.prev_sibling();
            stack.push(c);
        }
        out.push(w);
    }
    out
}

fn rows_widgets() {
    let f = fixture("rows", &[("main.tex", MAIN)]);
    let _editor = f.open("main.tex");
    palette::set_recent(vec!["win.toggle-pdf".into()]);
    palette::show(&f.state);
    palette::set_query(&f.state, "pdf");
    let dialog = palette::dialog(&f.state).unwrap();
    common::pump_until(TIMEOUT, || dialog.is_mapped() && dialog.height() > 0);
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
    let widgets = descendants(dialog.upcast_ref());
    // Keycaps of the accelerators, the bold match, the dim reason of the greyed-out row.
    let shortcuts: Vec<String> = widgets
        .iter()
        .filter_map(|w| w.downcast_ref::<adw::ShortcutLabel>())
        .filter(|s| s.is_visible())
        .map(|s| s.accelerator().to_string())
        .collect();
    assert!(shortcuts.contains(&"F8".to_owned()), "{shortcuts:?}");
    assert!(
        shortcuts.contains(&"<Control>f".to_owned()),
        "{shortcuts:?}"
    );
    let markups: Vec<String> = widgets
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>())
        .filter(|l| l.is_visible())
        .map(|l| l.label().to_string())
        .collect();
    assert!(
        markups.iter().any(|m| m == "Show <b>PDF</b> Preview"),
        "{markups:?}"
    );
    assert!(
        markups
            .iter()
            .any(|m| m.contains("Click into the PDF preview first")),
        "{markups:?}"
    );
    assert!(markups.iter().any(|m| m == "Recent"), "{markups:?}");
    let insensitive = widgets.iter().any(|w| {
        w.downcast_ref::<gtk::Label>()
            .is_some_and(|l| l.label().contains("Search in"))
            && !w.is_sensitive()
    });
    assert!(insensitive, "the unavailable command is greyed out");
    // The focus is in the search entry.
    let entry = widgets
        .iter()
        .find_map(|w| w.downcast_ref::<gtk::SearchEntry>().cloned())
        .unwrap();
    assert_eq!(entry.text(), "pdf");
    snapshot(&f, "palette-light.png");
    let style = adw::StyleManager::default();
    style.set_color_scheme(adw::ColorScheme::ForceDark);
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
    snapshot(&f, "palette-dark.png");
    style.set_color_scheme(adw::ColorScheme::Default);
    palette::set_recent(Vec::new());
    f.close_dialogs();
}

/// Saves a snapshot of the window when `LEMMA_TEST_SNAPSHOTS` names a directory.
fn snapshot(f: &Fixture, name: &str) {
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") {
        let path = PathBuf::from(dir).join(name);
        block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
    }
}

const PROJECT: &[(&str, &str)] = &[
    ("main.tex", MAIN),
    ("chapters/yoneda.tex", "\\section{Yoneda}\n"),
    ("refs.bib", "@book{a, title={A}}\n"),
    ("notes.md", "# Notes\n"),
    ("appendix.tex", "\\section{Appendix}\n"),
    ("deep/er/z.sty", "% z\n"),
    ("figures/plot.png", "not text"),
    ("latexmkrc", "$pdf_mode = 1;\n"),
    (".build/main.aux", "\\relax\n"),
    (".git/HEAD", "ref: refs/heads/main\n"),
    ("build/out.tex", "% build output\n"),
];

fn quick_open_order() {
    let f = fixture_with("quick-order", PROJECT, None, |c| {
        c.build.outdir = "build".into();
    });
    let _appendix = f.open("appendix.tex");
    let _main = f.open("main.tex");
    // The project index: main.tex includes the chapter and the bibliography.
    {
        let mut index = f.state.workspace.index.borrow_mut();
        for file in ["main.tex", "chapters/yoneda.tex"] {
            let text = std::fs::read_to_string(f.dir.join(file)).unwrap();
            let idx = lemma_syntax::index_file(&text);
            index.update_file(f.path(file), std::sync::Arc::new(idx));
        }
        index
            .bibs
            .insert(f.path("refs.bib"), std::sync::Arc::new(Vec::new()));
    }
    let candidates = quick_open::candidates(&f.state);
    let shown: Vec<(&str, Source)> = candidates
        .iter()
        .map(|c| (c.display.as_str(), c.source))
        .collect();
    assert_eq!(
        shown,
        [
            ("appendix.tex", Source::Tab),
            ("main.tex", Source::Tab),
            ("chapters/yoneda.tex", Source::Index),
            ("refs.bib", Source::Index),
        ],
        "tabs first (the active one last), then the index"
    );
    // The walk: sources only; hidden directories and the build directory are skipped.
    let skip = vec![f.dir.join("build")];
    let walked: Vec<String> = quick_open::walk_files(&f.dir, &skip)
        .iter()
        .map(|p| p.strip_prefix(&f.dir).unwrap().display().to_string())
        .collect();
    assert_eq!(
        walked,
        [
            "appendix.tex",
            "latexmkrc",
            "main.tex",
            "notes.md",
            "refs.bib",
            "chapters/yoneda.tex",
            "deep/er/z.sty"
        ]
    );
    // The picker: tabs and index at once, the rest when the walk is done.
    quick_open::show(&f.state);
    assert!(quick_open::is_open(&f.state));
    common::pump_until(TIMEOUT, || palette::visible(&f.state).len() == 7);
    let rel = |key: &String| {
        Path::new(key)
            .strip_prefix(&f.dir)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| key.clone())
    };
    let visible: Vec<String> = palette::visible(&f.state).iter().map(rel).collect();
    assert_eq!(
        visible,
        [
            "appendix.tex",
            "main.tex",
            "chapters/yoneda.tex",
            "refs.bib",
            "latexmkrc",
            "notes.md",
            "deep/er/z.sty"
        ]
    );
    let rows = palette::visible_rows(&f.state);
    assert_eq!(rows[0].badge.as_deref(), Some("Open"));
    assert_eq!(rows[2].title, "yoneda.tex");
    if std::env::var_os("LEMMA_TEST_SNAPSHOTS").is_some() {
        palette::set_query(&f.state, "tex");
        let dialog = palette::dialog(&f.state).unwrap();
        common::pump_until(TIMEOUT, || dialog.is_mapped());
        assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
        snapshot(&f, "quick-open.png");
        palette::set_query(&f.state, "");
    }
    assert_eq!(rows[2].subtitle, "chapters");
    assert_eq!(rows[2].badge, None);
    // An unsaved tab says so.
    f.state
        .active_editor()
        .unwrap()
        .buffer()
        .insert_at_cursor("x");
    f.close_dialogs();
    let candidates = quick_open::candidates(&f.state);
    assert!(
        candidates
            .iter()
            .any(|c| c.display == "main.tex" && c.dirty)
    );
    quick_open::show(&f.state);
    let rows = palette::visible_rows(&f.state);
    assert_eq!(rows[1].badge.as_deref(), Some("Unsaved"));
    f.close_dialogs();
}

fn quick_open_ranks() {
    let f = fixture("quick-rank", PROJECT);
    let _main = f.open("main.tex");
    let file = |display: &str, source| FileCandidate {
        path: f.dir.join(display),
        display: display.into(),
        source,
        dirty: false,
    };
    let files = [
        file("yoneda/notes.md", Source::Walk),
        file("chapters/yoneda.tex", Source::Walk),
        file("main.tex", Source::Tab),
        file("appendix.tex", Source::Walk),
        file("chapters/appendix-b.tex", Source::Index),
    ];
    let best = |q: &str| -> Vec<&str> {
        quick_open::rank_files(&files, q)
            .iter()
            .map(|r| files[r.index].display.as_str())
            .collect()
    };
    assert_eq!(best("yon")[0], "chapters/yoneda.tex", "file names first");
    assert_eq!(best("yoneda notes")[0], "yoneda/notes.md");
    assert_eq!(best("chap yon")[0], "chapters/yoneda.tex");
    assert_eq!(best("main")[0], "main.tex");
    assert_eq!(best("tex")[0], "main.tex", "an open tab wins a tie");
    assert_eq!(
        best("appendix")[..2],
        ["chapters/appendix-b.tex", "appendix.tex"],
        "the index bonus"
    );
    assert!(best("qqq").is_empty());

    // Choosing a file opens it.
    quick_open::show(&f.state);
    common::pump_until(TIMEOUT, || palette::visible(&f.state).len() >= 6);
    palette::set_query(&f.state, "yoneda");
    let selected = palette::selected(&f.state).unwrap();
    assert_eq!(PathBuf::from(&selected), f.path("chapters/yoneda.tex"));
    let row = &palette::visible_rows(&f.state)[0];
    assert_eq!(row.title_positions, [0, 1, 2, 3, 4, 5]);
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || {
        f.active_path() == Some(f.path("chapters/yoneda.tex"))
    });
    assert!(!quick_open::is_open(&f.state));
    // An open file's tab is selected again.
    quick_open::show(&f.state);
    palette::set_query(&f.state, "main");
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || f.active_path() == Some(f.path("main.tex")));
    assert_eq!(f.state.editors().len(), 2);
}

fn pickers_replace() {
    let f = fixture("replace", PROJECT);
    let _main = f.open("main.tex");
    palette::show(&f.state);
    palette::set_query(&f.state, "build");
    assert!(palette::is_open(&f.state));
    // The same picker again keeps its query (selected, to type over).
    palette::show(&f.state);
    assert!(palette::is_open(&f.state));
    assert_eq!(palette::selected(&f.state).as_deref(), Some("win.build"));
    // Quick open replaces it.
    let first = palette::dialog(&f.state).unwrap();
    quick_open::show(&f.state);
    assert!(quick_open::is_open(&f.state) && !palette::is_open(&f.state));
    common::pump_until(TIMEOUT, || !first.is_mapped());
    assert_ne!(palette::dialog(&f.state).as_ref(), Some(&first));
    common::pump_for(Duration::from_millis(50));
    assert!(f.state.active_editor().is_some());
    // The command palette runs other dialogs: the shortcuts window opens over nothing else.
    palette::show(&f.state);
    palette::set_query(&f.state, "keyboard shortcuts");
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || shortcuts::dialog(&f.state).is_some());
    common::pump_until(TIMEOUT, || !palette::is_open(&f.state));
    f.close_dialogs();
    // Preferences from the palette.
    palette::show(&f.state);
    palette::set_query(&f.state, "preferences");
    assert!(palette::activate_selected(&f.state));
    common::pump_until(TIMEOUT, || lemma_app::prefs::dialog(&f.state).is_some());
    f.close_dialogs();
}

fn shortcuts_groups() {
    let f = fixture("shortcuts", &[("main.tex", MAIN)]);
    let _main = f.open("main.tex");
    let sections = shortcuts::sections(&f.state);
    let groups: Vec<ShortcutGroup> = sections.iter().map(|(g, _)| *g).collect();
    assert_eq!(groups, ShortcutGroup::ALL);
    for (group, entries) in &sections {
        assert!(!entries.is_empty(), "{} has shortcuts", group.title());
        for e in entries {
            assert!(!e.title.is_empty() && !e.accel.is_empty(), "{e:?}");
        }
    }
    // Accelerators in GTK's spelling; sequences (`g+d`) and keys (`Control_L`) as written.
    let find = |group: ShortcutGroup, title: &str| {
        sections
            .iter()
            .find(|(g, _)| *g == group)
            .and_then(|(_, list)| list.iter().find(|e| e.title == title))
            .map(|e| {
                if e.accel.contains('+') {
                    e.accel.clone()
                } else {
                    canon(&e.accel)
                }
            })
    };
    use ShortcutGroup::*;
    let table = [
        (General, "Save", "<Control>s"),
        (General, "Command Palette", "<Control><Shift>p"),
        (General, "Keyboard Shortcuts", "<Control>question"),
        (Navigation, "Go to File…", "<Control>p"),
        (Navigation, "Go to Definition", "F12"),
        (
            Navigation,
            "Go to the definition under the pointer",
            "Control_L",
        ),
        (Editing, "Rename Label", "F2"),
        (Editing, "Show completions", "<Control>space"),
        (Review, "Accept Change", "<Alt>y"),
        (Agent, "Edit with Agent…", "<Control>k"),
        (
            Agent,
            "Answer the agent's permission card: once / always / reject",
            "y <Shift>y n",
        ),
        (BuildPdf, "Build", "F5"),
        (BuildPdf, "Next Problem", "F4"),
        (BuildPdf, "Previous Problem", "<Shift>F4"),
        (Vim, "Go to Definition", "g+d"),
        (Vim, "Next Problem", "bracketright+e"),
        (Vim, "Previous Problem", "bracketleft+e"),
        (Vim, "Show Hover", "<Shift>k"),
        (Vim, "Next Change", "bracketright+h"),
        (Vim, "Focus Agent Prompt", "space+a"),
        (
            Vim,
            "Go back after a jump (else Vim's jump list)",
            "<Control>o",
        ),
        (Vim, "Jump to the matching delimiter", "percent"),
    ];
    for (group, title, accel) in table {
        let wanted = if accel.contains('+') {
            accel.to_owned()
        } else {
            canon(accel)
        };
        assert_eq!(find(group, title), Some(wanted), "{title}");
    }
    // Actions without a key are not listed.
    assert_eq!(find(Editing, "Local History"), None);

    // The dialog has one section per group, titled, with the items.
    shortcuts::show(&f.state);
    let dialog = shortcuts::dialog(&f.state).expect("the shortcuts dialog");
    common::pump_until(TIMEOUT, || dialog.is_mapped());
    assert_eq!(
        f.state.window.visible_dialog().as_ref(),
        Some(dialog.upcast_ref())
    );
    let widgets = descendants(dialog.upcast_ref());
    let labels: Vec<String> = widgets
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>())
        .map(|l| l.label().to_string())
        .collect();
    for group in ShortcutGroup::ALL {
        assert!(
            labels.iter().any(|l| l == group.title()),
            "the section {} is shown: {labels:?}",
            group.title()
        );
    }
    assert!(labels.iter().any(|l| l == "Go to Definition"));
    // Shown again: the same dialog.
    shortcuts::show(&f.state);
    assert_eq!(shortcuts::dialog(&f.state).as_ref(), Some(&dialog));
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
    snapshot(&f, "shortcuts.png");
    f.close_dialogs();
    common::pump_until(TIMEOUT, || shortcuts::dialog(&f.state).is_none());
}

fn shortcuts_overrides() {
    let f = fixture("shortcuts-keys", &[("main.tex", MAIN)]);
    let _main = f.open("main.tex");
    let mut config = (*f.state.config.get()).clone();
    config
        .keys
        .bindings
        .insert("win.build".into(), "<Control>b".into());
    config
        .keys
        .bindings
        .insert("win.save".into(), String::new());
    config.vim.normal.insert("gd".into(), String::new());
    config
        .vim
        .normal
        .insert("<Space>p".into(), "win.command-palette".into());
    f.state.set_config(Rc::new(config));
    let sections: BTreeMap<ShortcutGroup, Vec<shortcuts::ShortcutEntry>> =
        shortcuts::sections(&f.state).into_iter().collect();
    let accel = |group: ShortcutGroup, title: &str| {
        sections[&group]
            .iter()
            .find(|e| e.title == title)
            .map(|e| e.accel.clone())
    };
    assert_eq!(
        accel(ShortcutGroup::BuildPdf, "Build").map(|a| canon(&a)),
        Some(canon("<Control>b"))
    );
    assert_eq!(accel(ShortcutGroup::General, "Save"), None, "removed");
    assert_eq!(
        accel(ShortcutGroup::Vim, "Go to Definition"),
        None,
        "unmapped"
    );
    assert_eq!(
        accel(ShortcutGroup::Vim, "Command Palette").as_deref(),
        Some("space+p")
    );
}

fn shortcut_labels() {
    let cases = [
        ("Tab / Shift+Tab", "Tab <Shift>Tab", false),
        ("Ctrl+Space", "<Control>space", false),
        ("Ctrl+Click", "Control_L", true),
        ("Ctrl+Click in the PDF", "Control_L", true),
        ("Ctrl+O", "<Control>o", false),
        ("%", "percent", false),
        ("y / Shift+Y / n", "y <Shift>y n", false),
        ("F12", "F12", false),
        ("Alt+]", "<Alt>bracketright", false),
    ];
    for (label, accel, click) in cases {
        assert_eq!(
            shortcuts::key_accelerator(label),
            Some((accel.to_owned(), click)),
            "{label}"
        );
    }
    assert_eq!(shortcuts::key_accelerator("Hyper+x"), None);
    // Every key of the registry converts, and adw parses it without complaint.
    for (keys, _, _) in app::KEY_LAYER_SHORTCUTS {
        let (accel, _) = shortcuts::key_accelerator(keys).unwrap_or_else(|| panic!("{keys}"));
        for alternative in accel.split(' ') {
            for key in alternative.split('+') {
                assert!(gtk::accelerator_parse(key).is_some(), "{keys}: {key}");
            }
        }
    }
    // Vim sequences.
    for (keys, accel) in [
        ("gd", Some("g+d")),
        ("]h", Some("bracketright+h")),
        ("<Space>a", Some("space+a")),
        ("K", Some("<Shift>k")),
        ("<C-k>", Some("<Control>k")),
        ("<lt>", Some("less")),
        ("%", Some("percent")),
        ("<S-Tab>", Some("<Shift>Tab")),
        ("<M-x>", Some("<Alt>x")),
        ("<Leader>x", None),
    ] {
        assert_eq!(palette::vim_accelerator(keys).as_deref(), accel, "{keys}");
    }
    // What the label shows.
    let label = adw::ShortcutLabel::new("g+d");
    let texts: Vec<String> = descendants(label.upcast_ref())
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>())
        .map(|l| l.label().to_string())
        .collect();
    assert!(
        texts.contains(&"G".to_owned()) && texts.contains(&"D".to_owned()),
        "{texts:?}"
    );
}

fn demo() {
    let f = fixture("demo", &[("main.tex", MAIN)]);
    let _main = f.open("main.tex");
    palette::set_recent(Vec::new());
    block_on(palette::run_demo(f.state.clone())).unwrap();
    assert!(palette::is_open(&f.state));
    let visible = palette::visible(&f.state);
    assert_eq!(visible[0], "win.toggle-pdf", "the recent command first");
    assert!(visible.contains(&"win.pdf-search".to_owned()));
    palette::set_recent(Vec::new());
    f.close_dialogs();
}

// ----- real key events -----

mod real_keys {
    //! The palette, quick open, the shortcuts window and Preferences on real key events (a
    //! private Broadway daemon, see `tests/broadway_input`): the search entry has the keyboard,
    //! Enter runs, Escape closes, and the focus goes back to the editor.
    //!
    //! GTK drops key events whose target is not mapped yet: a dialog's entry is mapped in the
    //! dialog's first frame, which Broadway draws about a second later (on Wayland, within a
    //! frame). So the keys are typed once the picker has been drawn.

    use super::*;
    use broadway_input::{Browser, CONTROL, SHIFT};

    macro_rules! step {
        ($($arg:tt)*) => { println!("child: {}", format!($($arg)*)) };
    }

    pub fn parent() {
        broadway_input::run_child("palette_real_keys_child", 35);
    }

    /// Runs the main loop until `done`; on a timeout, fails with what the picker shows.
    fn wait(state: &WindowState, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while !done() {
            if std::time::Instant::now() > deadline {
                let focus = gtk::prelude::GtkWindowExt::focus(&state.window)
                    .map(|w| format!("{} has_focus={}", w.type_().name(), w.has_focus()));
                let visible = palette::visible(state);
                panic!(
                    "timed out waiting for {what}: focus {focus:?}, query {:?}, selected {:?}, \
                     visible {:?}",
                    entry(state).map(|e| e.text().to_string()),
                    palette::selected(state),
                    &visible[..visible.len().min(4)]
                );
            }
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The search entry of the open picker.
    fn entry(state: &WindowState) -> Option<gtk::SearchEntry> {
        palette::dialog(state).and_then(|d| {
            descendants(d.upcast_ref())
                .into_iter()
                .find_map(|w| w.downcast::<gtk::SearchEntry>().ok())
        })
    }

    /// Waits until the open picker's entry has the keyboard focus and has been drawn.
    fn wait_ready(state: &WindowState) {
        wait(state, "the picker's entry", || {
            entry(state).is_some_and(|e| e.is_mapped())
                && gtk::prelude::GtkWindowExt::focus(&state.window).is_some_and(|w| {
                    w.has_focus() && w.ancestor(gtk::SearchEntry::static_type()).is_some()
                })
        });
        assert!(block_on(testmode::wait_for_frames(&state.window, 1)));
    }

    /// Waits until the dialog `present` returns has the focus and has been drawn (Escape goes
    /// to it).
    fn wait_drawn(state: &WindowState, present: impl Fn() -> bool) {
        wait(state, "the dialog", &present);
        let dialog = state.window.visible_dialog().unwrap();
        wait(state, "the dialog's focus", || {
            dialog.is_mapped()
                && gtk::prelude::GtkWindowExt::focus(&state.window)
                    .is_some_and(|w| w.has_focus() && w.is_mapped() && w.is_ancestor(&dialog))
        });
        assert!(block_on(testmode::wait_for_frames(&state.window, 1)));
    }

    pub fn child() {
        let f = fixture_with("real-keys", PROJECT, None, |c| c.editor.vim = true);
        let editor = f.open("main.tex");
        palette::set_recent(Vec::new());
        let view = editor.view().clone();
        wait(&f.state, "the window", || f.state.window.is_active());
        view.grab_focus();
        wait(&f.state, "the view's focus", || view.has_focus());
        let mut browser = Browser::connect_drawing();
        browser.settle = Duration::from_millis(20);
        let mode = || editor.vim_status().unwrap().mode;
        let normal = mode();

        step!("Ctrl+Shift+P, type, Down, Up, Enter");
        let shown = f.state.sidebar.split.shows_sidebar();
        browser.key(gdk::Key::P, CONTROL | SHIFT);
        wait(&f.state, "the palette", || palette::is_open(&f.state));
        wait_ready(&f.state);
        browser.text("sidebar");
        wait(&f.state, "the query", || {
            palette::selected(&f.state).as_deref() == Some("win.toggle-sidebar")
        });
        let second = palette::visible(&f.state).get(1).cloned();
        browser.key(gdk::Key::Down, 0);
        wait(&f.state, "Down", || palette::selected(&f.state) == second);
        browser.key(gdk::Key::Up, 0);
        wait(&f.state, "Up", || {
            palette::selected(&f.state).as_deref() == Some("win.toggle-sidebar")
        });
        browser.key(gdk::Key::Return, 0);
        wait(&f.state, "the command", || {
            !palette::is_open(&f.state) && f.state.sidebar.split.shows_sidebar() != shown
        });
        // The focus is back in the editor, nothing was typed, Vim is still in normal mode.
        f.state.sidebar.split.set_show_sidebar(shown);
        wait(&f.state, "the editor's focus", || view.has_focus());
        assert_eq!(editor.document().text(), MAIN);
        assert_eq!(mode(), normal);

        step!("Escape closes without running");
        browser.key(gdk::Key::P, CONTROL | SHIFT);
        wait(&f.state, "the palette", || palette::is_open(&f.state));
        wait_ready(&f.state);
        browser.text("build");
        wait(&f.state, "the query", || {
            palette::selected(&f.state).as_deref() == Some("win.build")
        });
        browser.key(gdk::Key::Escape, 0);
        wait(&f.state, "the palette to close", || {
            !palette::is_open(&f.state)
        });
        wait(&f.state, "the editor's focus", || view.has_focus());
        assert_eq!(palette::recent(), ["win.toggle-sidebar"]);
        assert_eq!(mode(), normal);

        step!("Ctrl+P opens a file");
        browser.key(gdk::Key::p, CONTROL);
        wait(&f.state, "quick open", || quick_open::is_open(&f.state));
        wait_ready(&f.state);
        browser.text("yoneda");
        wait(&f.state, "the file", || {
            palette::selected(&f.state).is_some_and(|s| s.ends_with("yoneda.tex"))
        });
        browser.key(gdk::Key::Return, 0);
        wait(&f.state, "the chapter", || {
            f.active_path() == Some(f.path("chapters/yoneda.tex"))
        });
        let chapter = f.state.active_editor().unwrap();
        wait(&f.state, "the chapter's focus", || {
            chapter.view().has_focus()
        });

        step!("Ctrl+Shift+P then Ctrl+P switches pickers");
        browser.key(gdk::Key::P, CONTROL | SHIFT);
        wait(&f.state, "the palette", || palette::is_open(&f.state));
        wait_ready(&f.state);
        browser.key(gdk::Key::p, CONTROL);
        wait(&f.state, "quick open", || quick_open::is_open(&f.state));
        wait_ready(&f.state);
        browser.key(gdk::Key::Escape, 0);
        wait(&f.state, "no dialog", || {
            f.state.window.visible_dialog().is_none()
        });
        wait(&f.state, "the chapter's focus", || {
            chapter.view().has_focus()
        });

        step!("Ctrl+? and Escape");
        browser.key(gdk::Key::question, CONTROL);
        wait_drawn(&f.state, || shortcuts::dialog(&f.state).is_some());
        browser.key(gdk::Key::Escape, 0);
        wait(&f.state, "no dialog", || {
            f.state.window.visible_dialog().is_none()
        });
        wait(&f.state, "the chapter's focus", || {
            chapter.view().has_focus()
        });

        step!("Ctrl+, and Escape");
        browser.key(gdk::Key::comma, CONTROL);
        wait_drawn(&f.state, || lemma_app::prefs::dialog(&f.state).is_some());
        browser.key(gdk::Key::Escape, 0);
        wait(&f.state, "no dialog", || {
            f.state.window.visible_dialog().is_none()
        });
        wait(&f.state, "the chapter's focus", || {
            chapter.view().has_focus()
        });
        assert_eq!(
            chapter.document().text(),
            "\\section{Yoneda}\n",
            "no key reached the editor"
        );
        assert_eq!(mode(), normal);
    }
}
