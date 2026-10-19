//! WP5.2: the agent backends. Edits Pi makes through ACP `fs/*` become the same hunks as MCP
//! edits (end to end with `lemma-fake-agent`), `fs/read_text_file` reads the live buffer, the
//! backend picker switches (asking while a turn runs) and is written to `config.toml`, the Pi
//! install branches run with a fake `node`/`npm` on `PATH`, the login hint, the API key row with
//! a fake `secret-tool`, and the cost label.
//!
//! The process environment is isolated before GTK starts (as in `gtk_agent_panel`): `HOME`, the
//! `XDG_*` directories and `LEMMA_CONFIG` point into a temporary directory, `PATH` is a temporary
//! `bin` (which always holds a fake `secret-tool`, so the real keyring is never asked) plus
//! `/usr/bin` and `/bin`, and `ANTHROPIC_API_KEY` is unset. Nothing here runs the real node, npm,
//! pi or claude, or calls any API.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_agent_backends`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `r-api-key-row.png`, `r-backend-picker.png` (its list open),
//! `r-api-key-dialog.png` and `r-api-key-stored.png` there.

mod common;

use adw::prelude::*;
use lemma_agent::{AgentEvent, PromptBlock, StopReason, Usd};
use lemma_app::agent::panel::{self, Entry, PanelCommand};
use lemma_app::agent::{AgentController, AgentState, PromptOrigin, SentPrompt, install};
use lemma_app::review_ui;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{AgentBackend, Config, Paths};
use lemma_core::ProjectRoot;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);
const AGENT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    // The end-to-end tests need the fake agent and the MCP bridge; cargo needs the real PATH.
    common::require_agent_binaries();
    isolate_environment();
    common::run(&[
        ("e2e_fs_edits_make_the_same_hunks_as_mcp", fs_hunks_like_mcp),
        ("e2e_fs_read_returns_the_live_buffer", fs_read_live_buffer),
        ("picker_switches_and_saves_the_backend", picker_switches),
        ("picker_asks_while_a_turn_runs", picker_asks_while_busy),
        ("pi_install_branches", pi_install_branches),
        ("pi_login_hint", pi_login_hint),
        ("api_key_row_and_dialog", api_key_row),
        ("api_cost_label", api_cost_label),
        // Final review (M5)
        (
            "regress5_config_reload_keeps_the_running_session",
            regress5_config_reload,
        ),
        (
            "regress5_no_keyring_lookup_before_the_pane_opens",
            regress5_lazy_key_lookup,
        ),
        ("regress5_pi_is_not_called_claude", regress5_pi_names),
    ])
}

// ----- the isolated environment -----

struct TestEnv {
    home: PathBuf,
    bin: PathBuf,
    config: PathBuf,
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

fn env() -> &'static TestEnv {
    ENV.get().expect("isolate_environment ran")
}

/// A `secret-tool` that keeps the key in `$HOME/stored-key` and logs its arguments.
const FAKE_SECRET_TOOL: &str = r#"echo "$@" >> "$HOME/secret-tool-args.txt"
case "$1" in
  lookup) if [ -f "$HOME/stored-key" ]; then cat "$HOME/stored-key"; exit 0; fi; exit 1;;
  store) cat > "$HOME/stored-key"; exit 0;;
  clear) rm -f "$HOME/stored-key"; exit 0;;
esac
exit 2"#;

fn isolate_environment() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-backends-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let bin = root.join("bin");
    for dir in [&home, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let config = home.join(".config/lemma/config.toml");
    // SAFETY: single-threaded: GTK is not initialised yet and no thread was spawned.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("LEMMA_CONFIG", &config);
        std::env::remove_var("MISE_DATA_DIR");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    let _ = ENV.set(TestEnv { home, bin, config });
    fake_program("secret-tool", FAKE_SECRET_TOOL);
}

/// Writes an executable shell script into the temporary `bin`.
fn fake_program(name: &str, body: &str) -> PathBuf {
    let path = env().bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn remove_program(name: &str) {
    let _ = std::fs::remove_file(env().bin.join(name));
}

/// `LEMMA_TEST_SNAPSHOTS=dir`: saves the window as `dir/<name>.png` (`scripts/shots.sh`
/// `t-agent-backends`).
fn snapshot(f: &Fixture, name: &str) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(block_on(lemma_app::testmode::wait_for_frames(
        &f.state.window,
        2
    )));
    block_on(lemma_app::testmode::snapshot_next_frame(
        &f.state.window,
        &dir.join(format!("{name}.png")),
    ))
    .unwrap();
}

// ----- fixtures -----

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
    commands: Rc<RefCell<Vec<PanelCommand>>>,
}

const MAIN: &str = "\\documentclass{article}\n\\begin{document}\nFirst line.\n\nThe OLD \
                    claim.\nLast line.\n\\end{document}\n";

/// A window on a new project with `files`; `swallow` makes the command hook handle every panel
/// command (no agent runs), else commands are only recorded.
fn fixture(
    name: &str,
    files: &[(&str, &str)],
    swallow: bool,
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
    config.build.on_agent_turn_end = false;
    config.editor.vim = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let paths = Paths::from_env();
    assert!(
        paths.as_ref().is_some_and(|p| p.home == env().home),
        "the test home is used"
    );
    let state = WindowState::new(&common::application(), Rc::new(config), paths, ws, true);
    let commands: Rc<RefCell<Vec<PanelCommand>>> = Rc::default();
    let sink = commands.clone();
    panel::set_command_hook(
        &state,
        Some(Box::new(move |c| {
            sink.borrow_mut().push(c.clone());
            swallow
        })),
    );
    state.window.set_default_size(1000, 740);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.window.is_active() && Instant::now() < deadline {
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    common::pump_for(Duration::from_millis(20));
    Fixture {
        state,
        dir: dir.canonicalize().unwrap(),
        commands,
    }
}

impl Fixture {
    fn panel(&self) -> panel::Panel {
        panel::get(&self.state).expect("the panel is installed")
    }

    fn agent(&self) -> Rc<AgentController> {
        self.state.agent_controller().expect("controller").clone()
    }

    fn show_panel(&self) {
        self.state.agent.split.set_show_sidebar(true);
        let widget = self.panel().widget();
        common::pump_until(TIMEOUT, || widget.is_mapped());
    }

    fn take_commands(&self) -> Vec<PanelCommand> {
        self.commands.take()
    }

    fn event(&self, event: AgentEvent) {
        self.agent().emit_event(&event);
    }

    /// Starts an emulated turn (no agent process).
    fn begin(&self, text: &str) -> u32 {
        let agent = self.agent();
        let turn = agent.begin_turn();
        agent.set_state(AgentState::Running { turn });
        agent.emit_prompt(&SentPrompt {
            turn,
            origin: PromptOrigin::Panel,
            blocks: vec![PromptBlock::Text(text.into())],
        });
        turn
    }

    fn scenario(&self, json: &serde_json::Value) -> PathBuf {
        let path = self.dir.with_extension("scenario.json");
        std::fs::write(&path, serde_json::to_vec_pretty(json).unwrap()).unwrap();
        path
    }

    /// Runs one prompt through the real bridge with the fake agent and `scenario`; returns the
    /// events.
    fn run_turn(&self, scenario: &serde_json::Value, origin: PromptOrigin) -> Vec<AgentEvent> {
        let agent = self.agent();
        agent.set_scenario(Some(self.scenario(scenario)));
        let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
        let sink = events.clone();
        agent.connect_event(move |e| sink.borrow_mut().push(e.clone()));
        assert!(panel::send_prompt(
            &self.state,
            vec![PromptBlock::Text("please edit".into())],
            origin
        ));
        common::pump_until(AGENT_TIMEOUT, || {
            events
                .borrow()
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnEnded(_)))
        });
        events.take()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(agent) = self.state.agent_controller() {
            agent.stop();
        }
        panel::set_command_hook(&self.state, None);
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// The hunks of `file` with their places and texts, and the proposal's text.
fn review_of(f: &Fixture, file: &str) -> (Vec<String>, String) {
    let doc = f
        .state
        .workspace
        .get(&f.dir.join(file))
        .unwrap_or_else(|| panic!("{file} is in the workspace"));
    let review = doc.review.borrow();
    let hunks = review
        .as_ref()
        .map(|r| {
            r.hunks()
                .iter()
                .map(|h| {
                    format!(
                        "{:?} {:?} {:?} {:?} {:?} {:?}",
                        h.base_lines, h.cur_lines, h.base_bytes, h.cur_bytes, h.old_text, h.turns
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (hunks, doc.text())
}

fn errors(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
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

/// Answers an `adw::AlertDialog` as its button would, then closes it.
fn respond(dialog: &adw::AlertDialog, response: &str) {
    dialog.emit_by_name::<()>("response", &[&response]);
    dialog.force_close();
    common::pump_for(Duration::from_millis(20));
}

// ----- ACP fs/* end to end -----

fn fs_hunks_like_mcp() {
    // The same change once through Lemma's MCP tools (Claude) and once through ACP fs/* (Pi).
    let mcp = fixture("hunks-mcp", &[("main.tex", MAIN)], false, |_| {});
    let events = mcp.run_turn(
        &serde_json::json!({"default": [
            {"tool": {"name": "edit", "args": {
                "file_path": "main.tex", "old_string": "OLD", "new_string": "NEW"}}},
            {"tool": {"name": "write", "args": {"file_path": "notes.tex", "content": "fresh\n"}}}
        ]}),
        PromptOrigin::Panel,
    );
    assert!(errors(&events).is_empty(), "{events:?}");
    let fs = fixture("hunks-fs", &[("main.tex", MAIN)], false, |c| {
        c.agent.backend = AgentBackend::Pi;
    });
    // Quick actions (and every other prompt origin) go to whichever backend is chosen.
    let events = fs.run_turn(
        &serde_json::json!({"_options": {"fs": "required"}, "default": [
            {"fs_edit": {"path": "main.tex", "old_string": "OLD", "new_string": "NEW"}},
            {"fs_write": {"path": "notes.tex", "content": "fresh\n"}}
        ]}),
        PromptOrigin::QuickAction("fix".into()),
    );
    assert!(errors(&events).is_empty(), "{events:?}");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    for file in ["main.tex", "notes.tex"] {
        let (mcp_hunks, mcp_text) = review_of(&mcp, file);
        let (fs_hunks, fs_text) = review_of(&fs, file);
        assert!(!fs_hunks.is_empty(), "{file}: a hunk to review");
        assert_eq!(fs_hunks, mcp_hunks, "{file}: the same hunks");
        assert_eq!(fs_text, mcp_text, "{file}: the same proposal");
        assert_eq!(
            std::fs::read_to_string(fs.dir.join(file)).unwrap(),
            fs_text,
            "{file}: on disk"
        );
    }
    assert!(fs_text_of(&fs, "main.tex").contains("The NEW claim."));
    // The tool records name the ACP methods; the review counts the same.
    let tools: Vec<&str> = fs
        .agent()
        .bridge()
        .tool_records()
        .iter()
        .map(|r| r.tool)
        .collect();
    assert_eq!(
        tools,
        [
            "fs/read_text_file",
            "fs/write_text_file",
            "fs/write_text_file"
        ]
    );
    assert_eq!(
        review_ui::summary(&fs.state.workspace),
        review_ui::summary(&mcp.state.workspace)
    );
    // Rejecting everything restores the files like for MCP edits.
    review_ui::decide_everything(&fs.state, review_ui::Decision::Reject);
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(fs.dir.join("main.tex")).unwrap() == MAIN
    });
}

fn fs_text_of(f: &Fixture, file: &str) -> String {
    review_of(f, file).1
}

fn fs_read_live_buffer() {
    let f = fixture("fs-live", &[("main.tex", MAIN)], false, |c| {
        c.agent.backend = AgentBackend::Pi;
        c.agent.save_before_prompt = false;
    });
    let editor = block_on(f.state.open_file(&f.dir.join("main.tex"))).expect("open");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% unsaved\n");
    let outside = f.dir.parent().unwrap().join("outside.tex");
    std::fs::write(&outside, "secret\n").unwrap();
    let events = f.run_turn(
        &serde_json::json!({"_options": {"fs": "required"}, "default": [
            {"fs_read": {"path": "main.tex", "expect_text": "% unsaved\n\\documentclass"}},
            {"fs_read": {"path": "main.tex", "line": 1, "limit": 2,
                         "expect_content": "% unsaved\n\\documentclass{article}\n"}},
            {"fs_read": {"path": outside, "expect_error": "path outside project root"}},
            {"fs_write": {"path": ".latexmkrc", "content": "system('x');\n",
                          "expect_error": "any"}}
        ]}),
        PromptOrigin::Panel,
    );
    assert!(errors(&events).is_empty(), "{events:?}");
    // Nothing was saved or written.
    assert_eq!(
        std::fs::read_to_string(f.dir.join("main.tex")).unwrap(),
        MAIN
    );
    assert!(!f.dir.join(".latexmkrc").exists());
}

// ----- the backend picker -----

fn picker_switches() {
    let _ = std::fs::remove_file(&env().config);
    let f = fixture("picker", &[("main.tex", MAIN)], true, |_| {});
    f.state
        .set_settings_files(Some(lemma_app::settings::SettingsFiles::next_to(
            env().config.clone(),
        )));
    f.show_panel();
    let panel = f.panel();
    f.take_commands();
    let picker = panel.backend_picker();
    assert!(picker.is_visible());
    assert_eq!(picker.selected(), 0, "Claude Code by default");
    assert!(panel.status_text().starts_with("Claude"));
    assert_eq!(panel.key_status(), None, "no key row for Claude");
    // Pi: the choice is saved, the old agent stops, the new one starts with the next prompt.
    picker.set_selected(2);
    assert_eq!(
        f.take_commands(),
        [PanelCommand::SwitchBackend(AgentBackend::Pi)]
    );
    assert_eq!(f.state.config.agent.backend, AgentBackend::Pi);
    let saved = std::fs::read_to_string(&env().config).unwrap();
    assert!(saved.contains("backend = \"pi\""), "{saved}");
    assert!(
        panel.status_text().starts_with("Pi"),
        "{}",
        panel.status_text()
    );
    assert!(
        panel.entries().contains(&Entry::Notice(
            "Switched to Pi. It starts with your next prompt.".into()
        )),
        "{:?}",
        panel.entries()
    );
    // The API: the key row appears.
    picker.set_selected(1);
    assert_eq!(
        f.take_commands(),
        [PanelCommand::SwitchBackend(AgentBackend::Api)]
    );
    assert!(panel.key_status().is_some());
    // An edit of config.toml elsewhere switches too, and the picker follows.
    let text = std::fs::read_to_string(&env().config)
        .unwrap()
        .replace("backend = \"api\"", "backend = \"claude\"");
    f.state
        .edit_config_file(|_| Ok(text.clone()))
        .expect("write the config");
    assert_eq!(picker.selected(), 0);
    assert_eq!(
        f.take_commands(),
        [PanelCommand::SwitchBackend(AgentBackend::Claude)]
    );
    assert_eq!(panel.key_status(), None);
}

fn picker_asks_while_busy() {
    let _ = std::fs::remove_file(&env().config);
    // No configuration file (as in a window without one): the switch holds for this window.
    let f = fixture("picker-busy", &[("main.tex", MAIN)], true, |_| {});
    f.show_panel();
    let panel = f.panel();
    f.agent().set_state(AgentState::Idle);
    f.begin("long job");
    f.take_commands();
    let picker = panel.backend_picker();
    picker.set_selected(2);
    let dialog = panel.switch_dialog().expect("asks first");
    assert!(f.take_commands().is_empty(), "nothing stops yet");
    assert_eq!(f.state.config.agent.backend, AgentBackend::Claude);
    respond(&dialog, "cancel");
    assert_eq!(picker.selected(), 0, "the picker goes back");
    assert_eq!(f.state.config.agent.backend, AgentBackend::Claude);
    assert!(panel.switch_dialog().is_none());
    picker.set_selected(2);
    let dialog = panel.switch_dialog().expect("asks again");
    respond(&dialog, "switch");
    assert_eq!(
        f.take_commands(),
        [PanelCommand::SwitchBackend(AgentBackend::Pi)]
    );
    assert_eq!(f.state.config.agent.backend, AgentBackend::Pi);
    assert!(!env().config.exists());
}

// ----- Pi: install and login -----

const FAKE_NPM_PI: &str = r#"echo "$@" > "$HOME/npm-args.txt"
prefix=""; pkg=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2;;
    @openma/*) pkg="$1"; shift;;
    *) shift;;
  esac
done
ver="${pkg##*@}"
echo "npm notice fake install of $pkg"
dir="$prefix/node_modules/@openma/pi-acp"
mkdir -p "$dir/dist"
printf '{"name":"@openma/pi-acp","version":"%s"}' "$ver" > "$dir/package.json"
echo "// fake" > "$dir/dist/bin.js"
echo "added 120 packages in 1s""#;

fn pi_dir() -> PathBuf {
    env().home.join(".local/share/lemma/acp-pi")
}

fn pi_install_branches() {
    let _ = std::fs::remove_dir_all(pi_dir());
    let f = fixture("pi-install", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Pi;
    });
    let panel = f.panel();
    let paths = f.state.paths.clone().unwrap();
    // No node: nothing else matters.
    remove_program("node");
    assert_eq!(
        install::detect_pi(&f.state.config.agent, &paths).unwrap_err(),
        install::ToolchainError::NodeNotFound
    );
    let node = fake_program("node", "exit 0");
    fake_program("npm", FAKE_NPM_PI);
    remove_program("pi");
    let error = install::detect_pi(&f.state.config.agent, &paths).unwrap_err();
    assert_eq!(
        error,
        install::ToolchainError::PiAdapter {
            found: None,
            wanted: "0.1.7".into()
        }
    );
    f.agent().set_state(AgentState::Failed(error.to_string()));
    let (title, message) = panel.setup().expect("the set-up card");
    assert_eq!(title, "Pi could not start");
    assert!(
        message.contains("@openma/pi-acp 0.1.7, which is not installed"),
        "{message}"
    );
    assert!(panel.install_button().is_visible());
    // The install runs only on the click.
    assert!(!env().home.join("npm-args.txt").exists());
    panel.install_button().emit_clicked();
    assert!(panel.installing());
    assert_eq!(
        panel.setup().map(|(t, _)| t).as_deref(),
        Some("Installing the Pi ACP adapter…")
    );
    common::pump_until(TIMEOUT, || !panel.installing());
    let log = panel.install_log();
    assert!(
        log.contains("npm notice fake install of @openma/pi-acp@0.1.7")
            && log.ends_with("Installed."),
        "{log}"
    );
    let args = std::fs::read_to_string(env().home.join("npm-args.txt")).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "i --prefix {} @openma/pi-acp@0.1.7 --omit=optional --no-audit --no-fund",
            pi_dir().display()
        )
    );
    assert_eq!(f.take_commands().last(), Some(&PanelCommand::Start));
    let toolchain = install::detect_pi(&f.state.config.agent, &paths).unwrap();
    assert_eq!(toolchain.node, node);
    assert_eq!(
        toolchain.adapter_js,
        pi_dir().join("node_modules/@openma/pi-acp/dist/bin.js")
    );
    assert_eq!(toolchain.pi, None, "pi is only looked for");
    let pi = fake_program("pi", "exit 0");
    let toolchain = install::detect_pi(&f.state.config.agent, &paths).unwrap();
    assert_eq!(toolchain.pi, Some(pi));
    // Another version is offered for install again.
    let mut cfg = f.state.config.agent.clone();
    cfg.pi_adapter_version = "0.2.0".into();
    assert_eq!(
        install::detect_pi(&cfg, &paths).unwrap_err(),
        install::ToolchainError::PiAdapter {
            found: Some("0.1.7".into()),
            wanted: "0.2.0".into()
        }
    );
}

fn pi_login_hint() {
    let f = fixture("pi-login", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Pi;
    });
    f.show_panel();
    let panel = f.panel();
    let login = format!(
        "The agent could not start a session: Configure an API key or log in. {}",
        lemma_agent::LOGIN_REQUIRED
    );
    fake_program("pi", "exit 0");
    f.event(AgentEvent::Error(login.clone()));
    let hint = panel.update_hint().expect("the login hint");
    assert!(hint.contains("run pi once in a terminal"), "{hint}");
    // Without pi, the hint says how to install it.
    remove_program("pi");
    f.event(AgentEvent::Error(login));
    let hint = panel.update_hint().expect("the login hint");
    assert!(
        hint.contains("npm i -g @earendil-works/pi-coding-agent"),
        "{hint}"
    );
    // Other errors, and Claude's, give no such hint.
    let g = fixture("claude-login", &[("main.tex", MAIN)], true, |_| {});
    g.show_panel();
    g.event(AgentEvent::Error(format!(
        "nope {}",
        lemma_agent::LOGIN_REQUIRED
    )));
    assert_eq!(g.panel().update_hint(), None);
}

// ----- the API backend -----

fn api_key_row() {
    let _ = std::fs::remove_file(env().home.join("stored-key"));
    let _ = std::fs::remove_file(env().home.join("secret-tool-args.txt"));
    let f = fixture("api-key", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Api;
    });
    f.show_panel();
    let panel = f.panel();
    common::pump_until(TIMEOUT, || {
        panel.key_status().as_deref() == Some("No API key")
    });
    assert!(panel.status_text().starts_with("Claude (API)"));
    if std::env::var_os("LEMMA_TEST_SNAPSHOTS").is_some() {
        snapshot(&f, "r-api-key-row");
        // The backend picker's list, open (after the frames `snapshot` waits for: a popup
        // placed before Broadway has given the window a monitor logs a Gdk critical).
        let picker = panel.backend_picker();
        picker.emit_by_name::<()>("activate", &[]);
        let popover = descendants(&picker)
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Popover>().ok())
            .expect("the picker's popover");
        common::pump_until(TIMEOUT, || popover.is_mapped());
        snapshot(&f, "r-backend-picker");
        popover.popdown();
        common::pump_until(TIMEOUT, || !popover.is_mapped());
    }
    panel.key_button().emit_clicked();
    let dialog = panel.key_dialog().expect("the key dialog");
    let entry = dialog
        .extra_child()
        .and_downcast::<gtk::PasswordEntry>()
        .expect("a password entry");
    entry.set_text("not a key");
    assert!(!dialog.is_response_enabled("save"), "spaces are refused");
    entry.set_text("sk-ant-test-0123456789");
    assert!(dialog.is_response_enabled("save"));
    if std::env::var_os("LEMMA_TEST_SNAPSHOTS").is_some() {
        common::pump_until(TIMEOUT, || dialog.is_mapped());
        snapshot(&f, "r-api-key-dialog");
    }
    respond(&dialog, "save");
    assert!(panel.key_dialog().is_none());
    common::pump_until(TIMEOUT, || {
        panel.key_status().as_deref() == Some("Key: from GNOME keyring")
    });
    snapshot(&f, "r-api-key-stored");
    assert_eq!(
        std::fs::read_to_string(env().home.join("stored-key"))
            .unwrap()
            .trim(),
        "sk-ant-test-0123456789"
    );
    // The key went on stdin, never in the arguments.
    let args = std::fs::read_to_string(env().home.join("secret-tool-args.txt")).unwrap();
    assert!(
        args.contains("store --label=Lemma Anthropic API key service lemma kind anthropic-api-key"),
        "{args}"
    );
    assert!(!args.contains("sk-ant-test"), "{args}");
    // Cancel stores nothing.
    panel.key_button().emit_clicked();
    let dialog = panel.key_dialog().expect("the key dialog");
    let entry = dialog
        .extra_child()
        .and_downcast::<gtk::PasswordEntry>()
        .unwrap();
    entry.set_text("sk-ant-other");
    respond(&dialog, "cancel");
    common::pump_for(Duration::from_millis(100));
    assert_eq!(
        std::fs::read_to_string(env().home.join("stored-key"))
            .unwrap()
            .trim(),
        "sk-ant-test-0123456789"
    );
    assert!(
        descendants(&panel.widget())
            .iter()
            .all(|w| w.downcast_ref::<gtk::PasswordEntry>().is_none()),
        "the key is not kept in the pane"
    );
}

fn api_cost_label() {
    let f = fixture("api-cost", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Api;
    });
    f.show_panel();
    let panel = f.panel();
    assert_eq!(panel.cost(), None);
    f.agent().set_state(AgentState::Idle);
    let cost = |usd: u64| AgentEvent::Cost {
        input: 1000,
        output: 200,
        cache_read: 0,
        cache_write: 0,
        usd_estimate: Usd(usd),
    };
    f.begin("one");
    f.event(cost(12_300));
    f.event(cost(1_000));
    let (text, tooltip) = panel.cost().expect("the cost label");
    assert_eq!(text, "$0.0133");
    assert!(tooltip.contains("this session: $0.0133"), "{tooltip}");
    f.event(AgentEvent::TurnEnded(StopReason::EndTurn));
    f.agent().set_state(AgentState::Idle);
    f.begin("two");
    f.event(cost(2_000_000));
    let (text, tooltip) = panel.cost().unwrap();
    assert_eq!(text, "$2.00", "the turn's cost");
    assert!(
        tooltip.contains("This turn: $2.00 · this session: $2.01"),
        "{tooltip}"
    );
}

// ----- final review (M5) -----

/// Another Lemma process picks Pi: `config.toml` changes under this window, which reloads it.
/// Its running turn (and idle session) must not be stopped without asking; the new backend
/// applies once the session is gone.
fn regress5_config_reload() {
    let _ = std::fs::remove_file(&env().config);
    let f = fixture("r5-reload", &[("main.tex", MAIN)], true, |_| {});
    f.state
        .set_settings_files(Some(lemma_app::settings::SettingsFiles::next_to(
            env().config.clone(),
        )));
    f.show_panel();
    let panel = f.panel();
    f.agent().set_state(AgentState::Idle);
    f.begin("long job");
    f.take_commands();
    f.state
        .edit_config_file(|_| Ok("[agent]\nbackend = \"pi\"\n".to_owned()))
        .expect("write the config");
    assert!(f.take_commands().is_empty(), "the running turn goes on");
    assert!(panel.switch_dialog().is_none());
    assert!(
        panel.status_text().starts_with("Claude"),
        "{}",
        panel.status_text()
    );
    assert_eq!(panel.backend_picker().selected(), 0, "the running agent");
    assert!(
        panel
            .entries()
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("Pi"))),
        "{:?}",
        panel.entries()
    );
    // The turn ends: the idle session stays as well.
    f.event(AgentEvent::TurnEnded(StopReason::EndTurn));
    f.agent().set_state(AgentState::Idle);
    common::pump_for(Duration::from_millis(20));
    assert!(f.take_commands().is_empty());
    assert!(panel.status_text().starts_with("Claude"));
    // Once it is gone, the configured backend applies.
    f.agent().stop();
    common::pump_for(Duration::from_millis(20));
    assert!(
        panel.status_text().starts_with("Pi"),
        "{}",
        panel.status_text()
    );
    assert_eq!(panel.backend_picker().selected(), 2);
    let _ = std::fs::remove_file(&env().config);
}

/// With `backend = "api"`, building the window looks nothing up in the keyring (it may ask to
/// be unlocked); the key row does when the pane is first shown.
fn regress5_lazy_key_lookup() {
    let args = env().home.join("secret-tool-args.txt");
    let _ = std::fs::remove_file(&args);
    let f = fixture("r5-lazy-key", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Api;
    });
    assert!(!f.state.agent.split.shows_sidebar(), "the pane is closed");
    common::pump_for(Duration::from_millis(300));
    let looked = std::fs::read_to_string(&args).unwrap_or_default();
    assert!(!looked.contains("lookup"), "{looked}");
    f.show_panel();
    let panel = f.panel();
    common::pump_until(TIMEOUT, || {
        panel
            .key_status()
            .is_some_and(|s| s == "No API key" || s.starts_with("Key: from"))
    });
    let looked = std::fs::read_to_string(&args).unwrap_or_default();
    assert!(looked.contains("lookup"), "{looked}");
}

/// With the Pi backend the pane does not call the agent "Claude".
fn regress5_pi_names() {
    let f = fixture("r5-pi-names", &[("main.tex", MAIN)], true, |c| {
        c.agent.backend = AgentBackend::Pi;
    });
    f.show_panel();
    let panel = f.panel();
    assert!(panel.status_text().starts_with("Pi"));
    let texts: Vec<String> = descendants(&panel.widget())
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>().map(|l| l.text().to_string()))
        .collect();
    assert!(texts.iter().any(|t| t == "Ask Pi…"), "{texts:?}");
    // (The backend picker offers "Claude Code".)
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("Claude") && t != "Claude Code"),
        "{texts:?}"
    );
    panel.actions().activate_action("shutdown", None);
    common::pump_for(Duration::from_millis(20));
    let notices: Vec<String> = panel
        .entries()
        .into_iter()
        .filter_map(|e| match e {
            Entry::Notice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|n| n.starts_with("Pi was stopped")),
        "{notices:?}"
    );
    assert!(!notices.iter().any(|n| n.contains("Claude")), "{notices:?}");
}
