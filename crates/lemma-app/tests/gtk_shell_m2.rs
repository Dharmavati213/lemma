//! The M2 shell hooks (WP2.0): the agent controller, the key layer, editor menus, live settings,
//! `saver::connect_saved`, "Ask agent to fix", notifications, root changes, and the two M1
//! review leftovers (a stale closed document read again on open; Overwrite offered by a build
//! whose save was refused).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_shell_m2`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_agent::{AgentEvent, EditorRequest, PromptBlock, StopReason, ToolReply};
use lemma_app::agent::{AgentState, PromptOrigin, SentPrompt};
use lemma_app::build_ui::Reason;
use lemma_app::diagnostics_panel::Problem;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, VimMode};
use lemma_app::review_ui::{self, Decision};
use lemma_app::saver::{self, SaveError};
use lemma_app::settings::{self, ConfigFile, SettingsFiles};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_app::{agent_edits, testmode};
use lemma_config::Config;
use lemma_core::{DiagSource, ProjectRoot, Severity};
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        (
            "agent_controller_state_turns_and_callbacks",
            agent_controller,
        ),
        ("key_layer_runs_handlers_by_priority", key_layer),
        ("editor_context_menu_sections", editor_menus),
        ("after_first_frame_runs_once", after_first_frame),
        ("config_reload_applies_and_notifies", config_reload),
        ("settings_files_are_watched_and_debounced", settings_watcher),
        ("saves_are_announced", saves_are_announced),
        ("problems_ask_agent_to_fix", ask_agent_to_fix),
        ("notifications_and_focus", notifications_and_focus),
        ("root_changes_are_announced", root_changes),
        ("tool_calls_for_file_tools_reply_once", tool_dispatch),
        ("helper_binaries_next_to_the_executable", sibling_binaries),
        ("agent_scenario_option", agent_scenario_option),
        (
            "regress_open_reads_a_stale_closed_document_again",
            stale_closed_document_reloads,
        ),
        (
            "regress_open_keeps_a_closed_document_with_pending_hunks",
            stale_closed_document_with_hunks,
        ),
        (
            "regress_build_offers_overwrite_when_its_save_is_refused",
            build_offers_overwrite,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, files, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
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
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.dir.join(file))).expect("open")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn disk(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    fn agent(&self, call: u64, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, call, req))
    }

    fn edit(&self, file: &str, old: &str, new: &str) {
        let reply = self.agent(
            1,
            EditorRequest::Edit {
                path: file.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        );
        assert!(!reply.is_error, "edit failed: {reply:?}");
    }

    /// Writes `text` to `file` as another program would, dated in the future (GtkSourceView
    /// compares modification times in whole seconds).
    fn external_write(&self, file: &str, text: &str) {
        std::fs::write(self.path(file), text).unwrap();
        std::fs::File::options()
            .write(true)
            .open(self.path(file))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut child = widget.first_child();
    while let Some(c) = child {
        out.extend(descendants(&c));
        child = c.next_sibling();
        out.push(c);
    }
    out
}

fn toast_buttons(state: &WindowState) -> Vec<gtk::Button> {
    descendants(&state.toasts)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter(|b| b.label().is_some_and(|l| l.contains("Overwrite")))
        .collect()
}

// ----- the agent controller -----

fn agent_controller() {
    let f = fixture("agent-controller", &[("main.tex", "x\n")]);
    let agent = f.state.agent_controller().expect("installed").clone();
    assert_eq!(agent.state(), AgentState::Stopped);
    assert_eq!(agent.turn_id(), 0);
    assert!(!agent.is_running());

    let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
    let states: Rc<RefCell<Vec<AgentState>>> = Rc::default();
    let prompts: Rc<RefCell<Vec<SentPrompt>>> = Rc::default();
    let sink = events.clone();
    let event_id = agent.connect_event(move |e| sink.borrow_mut().push(e.clone()));
    let sink = states.clone();
    agent.connect_state_changed(move |s| sink.borrow_mut().push(s.clone()));
    let sink = prompts.clone();
    agent.connect_prompt(move |p| sink.borrow_mut().push(p.clone()));

    agent.set_state(AgentState::Starting);
    agent.set_state(AgentState::Starting);
    agent.set_state(AgentState::Idle);
    let turn = agent.begin_turn();
    assert_eq!((turn, agent.turn_id()), (1, 1));
    agent.set_state(AgentState::Running { turn });
    assert!(agent.is_running());
    let sent = SentPrompt {
        turn,
        origin: PromptOrigin::Inline {
            path: f.path("main.tex"),
            range: 0..1,
        },
        blocks: vec![PromptBlock::Text("tighten this".into())],
    };
    agent.emit_prompt(&sent);
    for chunk in ["a", "b", "c"] {
        agent.emit_event(&AgentEvent::MessageChunk(chunk.into()));
    }
    agent.emit_event(&AgentEvent::Usage {
        used: 10,
        size: 200,
    });
    agent.emit_event(&AgentEvent::TurnEnded(StopReason::EndTurn));
    agent.set_state(AgentState::Idle);
    assert_eq!(agent.turn_id(), 1, "the last turn after it ended");

    assert_eq!(
        *states.borrow(),
        [
            AgentState::Starting,
            AgentState::Idle,
            AgentState::Running { turn: 1 },
            AgentState::Idle
        ],
        "only changes are announced"
    );
    assert_eq!(*prompts.borrow(), [sent]);
    let seen = events.borrow().clone();
    assert_eq!(seen.len(), 5);
    assert_eq!(
        seen[..3],
        [
            AgentEvent::MessageChunk("a".into()),
            AgentEvent::MessageChunk("b".into()),
            AgentEvent::MessageChunk("c".into())
        ]
    );
    agent.disconnect(event_id);
    agent.emit_event(&AgentEvent::Notice("after".into()));
    assert_eq!(events.borrow().len(), 5, "disconnected");
    assert_eq!(agent.begin_turn(), 2);

    agent.set_scenario(Some(f.path("basic.json")));
    assert_eq!(agent.scenario(), Some(f.path("basic.json")));

    // The window going stops the agent.
    agent.set_state(AgentState::Idle);
    f.state.window.destroy();
    common::pump_for(Duration::from_millis(20));
    assert_eq!(agent.state(), AgentState::Stopped);
}

// ----- the key layer -----

fn key_layer() {
    let f = fixture("key-layer", &[("main.tex", "x\n")]);
    let editor = f.open("main.tex");
    let view = editor.view();
    // The registry's own mechanics, without the handlers completion (WP2.9a), the typing
    // helpers (WP2.9b), the M3 hover and goto registries (WP3.0) and the `%` of delimiter
    // pairs (WP3.9) register.
    lemma_app::editor::completion::detach(&f.state, &editor);
    lemma_app::editor::typing::detach(&f.state, &editor);
    lemma_app::editor::hover::detach(&f.state, &editor);
    lemma_app::editor::goto::detach(&f.state, &editor);
    lemma_app::editor::pairs::detach(&f.state, &editor);
    let layer = view
        .key_layer()
        .expect("Editor::new installs the key layer");
    assert!(layer.is_empty());
    let host = editor.widget().clone().upcast::<gtk::Widget>();
    let controllers = host.observe_controllers();
    let installed = (0..controllers.n_items())
        .filter_map(|i| controllers.item(i))
        .filter_map(|o| o.downcast::<gtk::EventControllerKey>().ok())
        .any(|c| {
            c.name().as_deref() == Some("lemma-key-layer")
                && c.propagation_phase() == gtk::PropagationPhase::Capture
        });
    assert!(installed, "a capture-phase controller on the view's parent");

    type Log = Rc<RefCell<Vec<(&'static str, VimMode, bool)>>>;
    let log: Log = Rc::default();
    let handler = |name: &'static str, stop: bool| {
        let log = log.clone();
        move |event: &KeyEvent, mode: VimMode| {
            log.borrow_mut().push((name, mode, event.vim));
            if stop && event.keyval == gdk::Key::Tab {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    };
    keys::register(view, keys::PRIORITY_VIM_BINDINGS, handler("vim", false)).unwrap();
    let typing = keys::register(view, keys::PRIORITY_TYPING, handler("typing", false)).unwrap();
    keys::register(view, keys::PRIORITY_SNIPPETS, handler("snippets", true)).unwrap();
    keys::register(view, keys::PRIORITY_TYPING, handler("typing-2", false)).unwrap();
    assert_eq!(layer.len(), 4);

    let event = keys::event_for(&editor, gdk::Key::a, gdk::ModifierType::empty());
    assert!(!event.vim);
    assert_eq!(keys::dispatch(view, &event), glib::Propagation::Proceed);
    assert_eq!(
        *log.borrow(),
        [
            ("snippets", VimMode::Insert, false),
            ("typing", VimMode::Insert, false),
            ("typing-2", VimMode::Insert, false),
            ("vim", VimMode::Insert, false)
        ],
        "highest priority first, equal priorities in order; plain editing reports Insert"
    );
    log.borrow_mut().clear();
    let tab = keys::event_for(&editor, gdk::Key::Tab, gdk::ModifierType::empty());
    assert_eq!(keys::dispatch(view, &tab), glib::Propagation::Stop);
    assert_eq!(log.borrow().len(), 1, "Stop ends the dispatch");

    keys::unregister(view, typing);
    assert_eq!(layer.len(), 3);
    f.state.set_vim_enabled(true);
    log.borrow_mut().clear();
    let event = keys::event_for(&editor, gdk::Key::a, gdk::ModifierType::empty());
    assert!(event.vim);
    keys::dispatch(view, &event);
    assert_eq!(log.borrow()[0], ("snippets", VimMode::Normal, true));

    // Closing the tab takes the layer with it, without warnings.
    let before = common::logged_problems();
    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_for(Duration::from_millis(50));
    common::assert_no_new_problems(before);
}

fn editor_menus() {
    let f = fixture("editor-menus", &[("main.tex", "x\n")]);
    // WP2.5's quick actions already fill the section (a "Claude" submenu).
    let before = f.state.editor_menu.n_items();
    f.state
        .editor_menu
        .append(Some("Quick Thing"), Some("win.quick-thing"));
    let editor = f.open("main.tex");
    let model = editor.view().extra_menu().expect("an extra menu");
    assert_eq!(
        model.downcast_ref::<gio::Menu>(),
        Some(editor.extra_menu()),
        "the editor's model is the view's"
    );
    assert_eq!(editor.extra_menu().n_items(), 1, "the window's section");
    let section = editor
        .extra_menu()
        .item_link(0, gio::MENU_LINK_SECTION)
        .expect("a section");
    assert_eq!(section.n_items(), before + 1);
    // Spell checking inserts its section first.
    let spelling = gio::Menu::new();
    spelling.append(Some("Add to Dictionary"), Some("spelling.add"));
    editor.extra_menu().insert_section(0, None, &spelling);
    assert_eq!(editor.extra_menu().n_items(), 2);

    for (section, item) in [
        (&f.state.header.menu_files, "win.new-file"),
        (&f.state.header.menu_view, "win.toggle-vim"),
    ] {
        let action = section
            .item_attribute_value(0, gio::MENU_ATTRIBUTE_ACTION, None)
            .and_then(|v| v.get::<String>());
        assert_eq!(action.as_deref(), Some(item));
    }
    assert_eq!(f.state.header.menu.n_items(), 4, "files, go, view, app");
}

fn after_first_frame() {
    let f = fixture("first-frame", &[("main.tex", "x\n"), ("b.tex", "y\n")]);
    let editor = f.open("main.tex");
    let runs = Rc::new(std::cell::Cell::new(0));
    let count = runs.clone();
    editor.after_first_frame(move |_| count.set(count.get() + 1));
    common::pump_until(TIMEOUT, || runs.get() == 1);
    common::pump_for(Duration::from_millis(100));
    assert_eq!(runs.get(), 1, "once");

    // An editor that is not on screen yet runs it once it is.
    let b = f.open("b.tex");
    f.state.select_editor(&editor);
    common::pump_for(Duration::from_millis(50));
    let count = runs.clone();
    b.after_first_frame(move |_| count.set(count.get() + 10));
    if !b.view().is_mapped() {
        common::pump_for(Duration::from_millis(100));
        assert_eq!(runs.get(), 1, "not before it is shown");
        f.state.select_editor(&b);
    }
    common::pump_until(TIMEOUT, || runs.get() == 11);
}

// ----- live settings -----

fn config_reload() {
    let f = fixture(
        "config-reload",
        &[("main.tex", "\\begin{mathpar}\nx\n\\end{mathpar}\n")],
    );
    let editor = f.open("main.tex");
    let app = f.state.app.clone();
    lemma_app::app::set_accels(&app, &f.state.config.keys);
    let changes: Rc<RefCell<Vec<ConfigFile>>> = Rc::default();
    let sink = changes.clone();
    f.state.connect_config_changed(move |state, file| {
        assert!(Rc::strong_count(state) > 0);
        sink.borrow_mut().push(file);
    });

    let mut next = (*f.state.config.get()).clone();
    next.keys
        .bindings
        .insert("win.build".into(), "<Control>b".into());
    next.keys
        .bindings
        .insert("win.review-accept".into(), String::new());
    next.editor.wrap = false;
    next.editor.math_envs = vec!["mathpar".into()];
    next.editor.font = "Monospace 13".into();
    next.typing.autopair = false;
    f.state.set_config(Rc::new(next.clone()));
    assert_eq!(*changes.borrow(), [ConfigFile::Config]);
    assert!(
        !f.state.config.typing.autopair,
        "state.config reads the new values"
    );
    assert!(!f.state.config.editor.wrap);
    assert_eq!(app.accels_for_action("win.build"), ["<Control>b"]);
    assert!(app.accels_for_action("win.review-accept").is_empty());
    assert_eq!(editor.view().wrap_mode(), gtk::WrapMode::None);
    let doc = editor.document();
    assert!(doc.scan_config().math_envs.iter().any(|e| e == "mathpar"));
    let inside = doc.buffer().unwrap().iter_at_line(1).unwrap();
    assert!(
        doc.context_at(&inside).is_math(),
        "the new rules apply at once"
    );

    // The same configuration again changes nothing.
    f.state.set_config(Rc::new(next.clone()));
    assert_eq!(changes.borrow().len(), 1);

    // Removing the overrides brings back the defaults.
    let mut back = next;
    back.keys.bindings.clear();
    back.editor.wrap = true;
    f.state.set_config(Rc::new(back));
    assert_eq!(app.accels_for_action("win.build"), ["F5"]);
    assert_eq!(app.accels_for_action("win.review-accept"), ["<Alt>y"]);
    assert_eq!(editor.view().wrap_mode(), gtk::WrapMode::WordChar);

    // Snippets and prompts.
    assert_eq!(f.state.prompts().len(), 6, "the defaults");
    let defaults = f.state.snippets().len();
    let user = lemma_snippets::SnippetSet::from_toml(
        "[[snippet]]\ntrigger = \"zz\"\nbody = 'zeta'\nauto = true\n",
    )
    .unwrap();
    f.state
        .set_snippets(Rc::new(lemma_snippets::SnippetSet::merged(
            lemma_snippets::SnippetSet::defaults(),
            user,
        )));
    assert_eq!(f.state.snippets().len(), defaults + 1);
    let mut prompts = lemma_config::default_prompts();
    prompts.truncate(2);
    f.state.set_prompts(Rc::new(prompts.clone()));
    f.state.set_prompts(Rc::new(prompts));
    assert_eq!(f.state.prompts().len(), 2);
    assert_eq!(
        *changes.borrow(),
        [
            ConfigFile::Config,
            ConfigFile::Config,
            ConfigFile::Snippets,
            ConfigFile::Prompts
        ]
    );
    lemma_app::app::set_accels(&app, &Config::default().keys);
}

fn settings_watcher() {
    let dir = common::temp_dir("settings-watch");
    let files = SettingsFiles::next_to(dir.join("config.toml"));
    let seen: Rc<RefCell<Vec<ConfigFile>>> = Rc::default();
    let sink = seen.clone();
    let watcher = settings::Watcher::start(&files, move |file| sink.borrow_mut().push(file));
    assert_eq!(watcher.len(), 3);
    common::pump_for(Duration::from_millis(100));

    // Several writes in a row are one reload, 300 ms after the last.
    std::fs::write(&files.config, "[editor]\nvim = false\n").unwrap();
    common::pump_for(Duration::from_millis(100));
    std::fs::write(&files.config, "[editor]\nvim = false\nwrap = false\n").unwrap();
    common::pump_for(Duration::from_millis(100));
    assert!(seen.borrow().is_empty(), "debounced");
    common::pump_until(TIMEOUT, || !seen.borrow().is_empty());
    common::pump_for(Duration::from_millis(400));
    // One reload for the burst (GIO may still report a late change of the same burst).
    assert!(
        seen.borrow().len() <= 2 && seen.borrow().iter().all(|f| *f == ConfigFile::Config),
        "{:?}",
        seen.borrow()
    );
    match block_on(settings::reload(&files, ConfigFile::Config)) {
        Ok(settings::Reloaded::Config(c)) => assert!(!c.editor.vim && !c.editor.wrap),
        _ => panic!("the config reloads"),
    }

    // Replacing a file atomically (as editors do) is a change too.
    seen.borrow_mut().clear();
    let tmp = dir.join(".prompts.toml.tmp");
    std::fs::write(
        &tmp,
        "[[prompt]]\nname = \"Shorten\"\nprompt = \"p {selection}\"\n",
    )
    .unwrap();
    std::fs::rename(&tmp, &files.prompts).unwrap();
    common::pump_until(TIMEOUT, || seen.borrow().contains(&ConfigFile::Prompts));
    match block_on(settings::reload(&files, ConfigFile::Prompts)) {
        Ok(settings::Reloaded::Prompts(p)) => assert_eq!(p.len(), 7),
        _ => panic!("the prompts reload"),
    }

    // Errors keep the previous values (the caller reports them); a deleted file means defaults.
    std::fs::write(&files.snippets, "[[snippet]]\ntrigger = 3\n").unwrap();
    common::pump_until(TIMEOUT, || seen.borrow().contains(&ConfigFile::Snippets));
    let error = block_on(settings::reload(&files, ConfigFile::Snippets))
        .err()
        .expect("an error");
    assert!(error.contains("snippets.toml has errors"), "{error}");
    std::fs::remove_file(&files.config).unwrap();
    match block_on(settings::reload(&files, ConfigFile::Config)) {
        Ok(settings::Reloaded::Config(c)) => assert_eq!(*c, Config::default()),
        _ => panic!("a missing config means defaults"),
    }
    drop(watcher);
    seen.borrow_mut().clear();
    std::fs::write(&files.config, "").unwrap();
    common::pump_for(Duration::from_millis(500));
    assert!(seen.borrow().is_empty(), "a dropped watcher is silent");

    // `/dev/null` (LEMMA_CONFIG in golden tests) is not watched.
    let null = settings::Watcher::start(&SettingsFiles::next_to("/dev/null".into()), |_| {});
    assert_eq!(null.len(), 2, "only the two missing files next to it");
}

// ----- saves, problems, notifications, root -----

fn saves_are_announced() {
    let f = fixture("saved-hook", &[("main.tex", "x\n")]);
    let editor = f.open("main.tex");
    let saved: Rc<RefCell<Vec<(PathBuf, String)>>> = Rc::default();
    let sink = saved.clone();
    let id = saver::connect_saved(move |path, text| {
        sink.borrow_mut()
            .push((path.to_path_buf(), text.to_owned()));
    });
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "typed\n");
    block_on(editor.document().save()).unwrap();
    assert_eq!(
        *saved.borrow(),
        [(
            f.path("main.tex").canonicalize().unwrap(),
            "typed\nx\n".to_owned()
        )]
    );
    // Agent edits to closed documents are saves too.
    std::fs::write(f.path("ch.tex"), "a\n").unwrap();
    f.edit("ch.tex", "a\n", "b\n");
    assert_eq!(saved.borrow().len(), 2);
    assert_eq!(saved.borrow()[1].1, "b\n");
    saver::disconnect_saved(id);
    buffer.insert(&mut buffer.start_iter(), "more\n");
    block_on(editor.document().save()).unwrap();
    assert_eq!(saved.borrow().len(), 2, "disconnected");
}

fn ask_agent_to_fix() {
    let f = fixture("ask-agent", &[("main.tex", "a\nb\nc\nd\n")]);
    let build = f.state.build.get().unwrap();
    let path = f.path("main.tex").canonicalize().unwrap();
    let problem = Problem {
        path: path.clone(),
        line: Some(3),
        severity: Severity::Error,
        message: "Undefined control sequence".into(),
        context: Some("l.3 \\foo".into()),
        source: DiagSource::Tex,
    };
    build.diagnostics().set(vec![problem.clone()], &path);
    assert_eq!(
        build.diagnostics().problem_for_agent(0),
        Some(problem.clone())
    );
    assert_eq!(build.diagnostics().problem_for_agent(1), None);

    // The line follows edits made since the build.
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "new 1\nnew 2\n");
    let moved = build.diagnostics().problem_for_agent(0).unwrap();
    assert_eq!(moved.line, Some(5));
    assert_eq!(moved.message, problem.message);

    // The rows' context action is installed for the list and reaches the agent hook.
    let list = descendants(&build.diagnostics().widget())
        .into_iter()
        .find_map(|w| w.downcast::<gtk::ListBox>().ok())
        .expect("the Problems list");
    assert!(
        WidgetExt::activate_action(&list, "problems.ask-agent-fix", Some(&0u32.to_variant()))
            .is_ok(),
        "the action exists"
    );
    build.diagnostics().ask_agent(0);
    build.diagnostics().ask_agent(7);

    // The context menu of a row offers it. (Popping it up is not tested: Broadway has no
    // monitor to place popovers on.)
    let before = common::logged_problems();
    let model = build.diagnostics().context_menu(0).expect("a menu");
    let action = model
        .item_attribute_value(0, gio::MENU_ATTRIBUTE_ACTION, None)
        .and_then(|v| v.get::<String>());
    assert_eq!(action.as_deref(), Some("problems.ask-agent-fix"));
    let target = model
        .item_attribute_value(0, gio::MENU_ATTRIBUTE_TARGET, None)
        .and_then(|v| v.get::<u32>());
    assert_eq!(target, Some(0));
    assert!(build.diagnostics().context_menu(5).is_none());
    common::assert_no_new_problems(before);
}

fn notifications_and_focus() {
    let f = fixture("notify", &[("main.tex", "x\n")]);
    let sent: Rc<RefCell<Vec<String>>> = Rc::default();
    // Offscreen runs never notify the desktop.
    f.state
        .send_notification("turn-ended", &gio::Notification::new("Claude finished"));
    let sink = sent.clone();
    f.state.set_notification_hook(Some(Box::new(move |id, _| {
        sink.borrow_mut().push(id.into())
    })));
    f.state
        .send_notification("turn-ended", &gio::Notification::new("Claude finished"));
    assert_eq!(*sent.borrow(), ["turn-ended"]);
    f.state.set_focus_override(Some(false));
    assert!(!f.state.window_focused());
    f.state.set_focus_override(Some(true));
    assert!(f.state.window_focused());
    f.state.set_focus_override(None);
    assert_eq!(f.state.window_focused(), f.state.window.is_active());
    f.state.set_notification_hook(None);
}

fn root_changes() {
    let f = fixture("root-a", &[]);
    let other = common::temp_dir("root-b");
    std::fs::write(other.join("doc.tex"), "x\n").unwrap();
    let roots: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let sink = roots.clone();
    let id = f.state.connect_root_changed(move |state| {
        sink.borrow_mut()
            .push(state.workspace.root().path().to_path_buf());
    });
    block_on(f.state.open_file(&other.join("doc.tex"))).unwrap();
    assert_eq!(*roots.borrow(), [other.canonicalize().unwrap()]);
    // Not empty any more: the root stays.
    std::fs::write(f.dir.join("x.tex"), "y\n").unwrap();
    block_on(f.state.open_file(&f.dir.join("x.tex"))).unwrap();
    assert_eq!(roots.borrow().len(), 1);
    f.state.disconnect(id);
}

fn tool_dispatch() {
    let f = fixture("tools", &[("main.tex", "alpha\n")]);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let call = lemma_agent::ToolCall {
        call: 4,
        req: EditorRequest::ReadFile {
            path: "main.tex".into(),
            offset: None,
            limit: None,
        },
        reply: tx,
    };
    block_on(lemma_app::agent::tools::handle(f.state.clone(), call));
    let reply = rx
        .try_recv()
        .expect("exactly one reply, sent before the future resolved");
    assert!(!reply.is_error);
    assert!(rx.try_recv().is_err());
    // A reply nobody waits for any more is dropped quietly.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    drop(rx);
    let call = lemma_agent::ToolCall {
        call: 5,
        req: EditorRequest::ListFiles,
        reply: tx,
    };
    block_on(lemma_app::agent::tools::handle(f.state.clone(), call));
    assert_eq!(
        lemma_app::agent::tools::tool_name(&EditorRequest::RenderPage { page: 1 }),
        "render_page"
    );
}

fn sibling_binaries() {
    // This test runs from `target/<profile>/deps/`; the `lemma` binary is one level up.
    let lemma = testmode::sibling_binary("lemma").expect("lemma next to the test's directory");
    assert_eq!(lemma.file_name().unwrap(), "lemma");
    assert!(testmode::sibling_binary("no-such-helper").is_none());
}

/// `--agent-scenario` is accepted and is no scripted step by itself: with `--quit-after-ms` the
/// run ends normally.
fn agent_scenario_option() {
    let dir = common::temp_dir("agent-scenario");
    std::fs::write(dir.join("main.tex"), "x\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(dir.join("main.tex"))
        .args(["--agent-scenario", "basic.json", "--quit-after-ms", "300"])
        .env("LEMMA_CONFIG", "/dev/null")
        .output()
        .expect("run lemma");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{:?}", out.status);
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg("--help")
        .output()
        .expect("run lemma");
    assert!(String::from_utf8_lossy(&help.stdout).contains("--agent-scenario JSON"));
}

// ----- M1 review leftovers -----

/// A closed document whose review was decided keeps its text; when another program changes the
/// file meanwhile, opening it shows the file, not the stale text.
fn stale_closed_document_reloads() {
    let f = fixture(
        "stale-open",
        &[("main.tex", "x\n"), ("ch.tex", "c1\nc2\nc3\n")],
    );
    f.edit("ch.tex", "c1\n", "C1\n");
    let doc = f.state.workspace.get(&f.path("ch.tex")).unwrap();
    assert!(!doc.is_open());
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    assert_eq!(review_ui::hunk_count(&doc), 0);
    let external = "C1\nc2\nc3\nexternal line\n";
    f.external_write("ch.tex", external);

    let editor = f.open("ch.tex");
    let shown = editor.document().text();
    assert_eq!(
        shown, external,
        "the editor shows the file, not the stale text"
    );
    assert!(!editor.document().is_dirty());
    assert!(editor.document().has_line_states());
    // Typing and saving work without a conflict, and keep the other program's change.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "typed\n");
    block_on(editor.document().save()).expect("no conflict");
    assert_eq!(f.disk("ch.tex"), format!("typed\n{external}"));

    // A file that is unchanged on disk keeps its document (and its decided review).
    f.edit("main.tex", "x\n", "X\n");
    let main = f.state.workspace.get(&f.path("main.tex")).unwrap();
    review_ui::decide_document(&f.state, &main, Decision::Accept);
    let editor = f.open("main.tex");
    assert!(Rc::ptr_eq(editor.document(), &main), "the same document");
    assert_eq!(main.text(), "X\n");
}

/// A closed document with hunks awaiting review is never replaced by the file: its text is
/// shown, and saving it over the other program's change asks first.
fn stale_closed_document_with_hunks() {
    let f = fixture(
        "stale-pending",
        &[("main.tex", "x\n"), ("ch.tex", "c1\nc2\n")],
    );
    f.edit("ch.tex", "c1\n", "C1\n");
    let doc = f.state.workspace.get(&f.path("ch.tex")).unwrap();
    assert_eq!(review_ui::hunk_count(&doc), 1);
    f.external_write("ch.tex", "c1\nc2\nexternal\n");
    let editor = f.open("ch.tex");
    assert!(Rc::ptr_eq(editor.document(), &doc));
    assert_eq!(doc.text(), "C1\nc2\n", "the pending proposal stays");
    assert_eq!(review_ui::hunk_count(&doc), 1);
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "typed\n");
    assert_eq!(
        block_on(doc.save()),
        Err(SaveError::ExternallyModified),
        "the other program's change is not overwritten silently"
    );
    assert_eq!(f.disk("ch.tex"), "c1\nc2\nexternal\n");
}

/// F5 after the file changed on disk: the build cannot save it and offers Overwrite, as a user
/// save does; overwriting builds again.
fn build_offers_overwrite() {
    let f = fixture(
        "build-overwrite",
        &[("main.tex", "plain text, no document\n")],
    );
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "typed\n");
    f.external_write("main.tex", "EXTERNAL\n");
    assert!(toast_buttons(&f.state).is_empty());

    f.state.request_build(Reason::Manual);
    common::pump_until(TIMEOUT, || !toast_buttons(&f.state).is_empty());
    let build = f.state.build.get().unwrap();
    common::pump_until(TIMEOUT, || !build.is_busy());
    let status = f.state.status.build.text();
    assert!(
        status.starts_with("Not building: could not save") && status.contains("another program"),
        "{status}"
    );
    assert_eq!(f.disk("main.tex"), "EXTERNAL\n", "nothing was overwritten");
    assert_eq!(toast_buttons(&f.state).len(), 1);

    toast_buttons(&f.state)[0].emit_clicked();
    common::pump_until(TIMEOUT, || {
        f.disk("main.tex") == "typed\nplain text, no document\n"
    });
    // The build was requested again: this file is no main document, and a scripted window
    // does not ask for one.
    common::pump_until(TIMEOUT, || {
        f.state.status.build.text() == "Could not find the main document to build"
    });
    assert!(!editor.document().is_dirty());
}
