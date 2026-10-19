//! Regression tests for verified M2 defects of the agent UI (review round 2, area C: the agent
//! panel, inline edit, quick actions). Each test fails until its defect is fixed; pure ones
//! (Markdown, toolchain detection) are in `regress2_c.rs`.
//!
//! The panel is driven as in `gtk_agent_panel.rs`: a command hook takes the panel's commands
//! and the test emulates the agent through `AgentController::{begin_turn, set_state,
//! emit_prompt, emit_event}`; tests of the start-up path attach a command channel to the bridge
//! (`bridge::attach_channel`), so no agent process runs. The environment is isolated as there
//! (temporary HOME, XDG dirs and PATH, `LEMMA_CONFIG`).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_c`.

mod common;

use adw::prelude::*;
use gtk::gdk::{Key, ModifierType};
use lemma_agent::StopReason;
use lemma_agent::{AgentCmd, AgentEvent, ConfigOpt, EditorRequest, PermOption, PromptBlock};
use lemma_app::agent::panel::{self, Entry, PanelCommand, PermissionAnswer};
use lemma_app::agent::{AgentController, AgentState, PromptOrigin, SentPrompt, bridge};
use lemma_app::agent::{inline, quick};
use lemma_app::diagnostics_panel::Problem;
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{Config, Paths, parse_prompts};
use lemma_core::{DiagSource, ProjectRoot, Severity};
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    isolate_environment();
    common::run(&[
        (
            "regress2_permission_request_keeps_the_editor_focus",
            permission_request_keeps_the_editor_focus,
        ),
        (
            "regress2_permission_autofocus_is_not_an_allow_button",
            permission_autofocus_is_not_an_allow_button,
        ),
        (
            "regress2_inline_esc_cancels_while_starting",
            inline_esc_cancels_while_starting,
        ),
        (
            "regress2_agent_stop_cancels_while_starting",
            agent_stop_cancels_while_starting,
        ),
        (
            "regress2_clear_answers_pending_permission",
            clear_answers_pending_permission,
        ),
        (
            "regress2_refused_prompt_keeps_the_sent_text_out_of_the_box",
            refused_prompt_keeps_the_sent_text_out_of_the_box,
        ),
        (
            "regress2_picker_resyncs_after_a_refused_change",
            picker_resyncs_after_a_refused_change,
        ),
        (
            "regress2_picker_choice_while_stopped_is_not_lost",
            picker_choice_while_stopped_is_not_lost,
        ),
        (
            "regress2_usage_ring_resets_on_new_session",
            usage_ring_resets_on_new_session,
        ),
        (
            "regress2_stop_agent_while_starting_shows_stopped",
            stop_agent_while_starting_shows_stopped,
        ),
        ("regress2_caps_lock_y_allows_once", caps_lock_y_allows_once),
        (
            "regress2_prompt_box_has_an_accessible_name",
            prompt_box_has_an_accessible_name,
        ),
        (
            "regress2_fix_diagnostic_context_is_bounded",
            fix_diagnostic_context_is_bounded,
        ),
        (
            "regress2_quick_key_override_survives_prompts_reload",
            quick_key_override_survives_prompts_reload,
        ),
        (
            "regress2_summary_row_follows_the_review",
            summary_row_follows_the_review,
        ),
        (
            "regress2_start_failure_reported_once",
            start_failure_reported_once,
        ),
        (
            "regress2_code_block_streaming_frame_cost",
            code_block_streaming_frame_cost,
        ),
        (
            "regress2_transcript_rows_are_recycled",
            transcript_rows_are_recycled,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

// ----- the isolated environment (as in gtk_agent_panel.rs) -----

struct TestEnv {
    home: PathBuf,
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

fn env() -> &'static TestEnv {
    ENV.get().expect("isolate_environment ran")
}

fn isolate_environment() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress2-c-env-{}", std::process::id()));
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
    }
    let _ = ENV.set(TestEnv { home });
}

// ----- fixtures -----

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hook {
    /// Records every command and handles it (nothing reaches the controller).
    Swallow,
    /// Records every command and lets it through.
    Forward,
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
    commands: Rc<RefCell<Vec<PanelCommand>>>,
}

fn fixture(name: &str, files: &[(&str, &str)], hook: Hook) -> Fixture {
    fixture_with(name, files, hook, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
    hook: Hook,
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
            hook == Hook::Swallow
        })),
    );
    // 1000 px: below the 1100sp breakpoint, so the agent pane is an overlay (a half-screen
    // tiled window).
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
        dir,
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

    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.dir.join(file))).expect("open")
    }

    fn show_panel(&self) {
        self.state.agent.split.set_show_sidebar(true);
        let widget = self.panel().widget();
        common::pump_until(TIMEOUT, || widget.is_mapped());
    }

    fn commands(&self) -> Vec<PanelCommand> {
        self.commands.borrow().clone()
    }

    fn take_commands(&self) -> Vec<PanelCommand> {
        self.commands.take()
    }

    fn begin(&self, text: &str, origin: PromptOrigin) -> u32 {
        let agent = self.agent();
        let turn = agent.begin_turn();
        agent.set_state(AgentState::Running { turn });
        agent.emit_prompt(&SentPrompt {
            turn,
            origin,
            blocks: vec![PromptBlock::Text(text.into())],
        });
        turn
    }

    fn event(&self, event: AgentEvent) {
        self.agent().emit_event(&event);
    }

    fn end(&self, reason: StopReason) {
        self.event(AgentEvent::TurnEnded(reason));
        self.agent().set_state(AgentState::Idle);
    }

    fn edit(&self, call: u64, file: &str, old: &str, new: &str) {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        block_on(lemma_app::agent::tools::handle(
            self.state.clone(),
            lemma_agent::ToolCall {
                call,
                req: EditorRequest::Edit {
                    path: file.into(),
                    old_string: old.into(),
                    new_string: new.into(),
                    replace_all: false,
                },
                reply,
            },
        ));
        let reply = answer.try_recv().expect("the tool answered");
        assert!(!reply.is_error, "edit failed: {reply:?}");
    }

    fn frames(&self, n: u32) {
        block_on(testmode::wait_for_frames(&self.state.window, n));
    }

    fn focus(&self) -> Option<gtk::Widget> {
        gtk::prelude::RootExt::focus(&self.state.window)
    }

    fn focus_in_panel(&self) -> bool {
        let root = self.panel().widget();
        self.focus().is_some_and(|w| w.is_ancestor(&root))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        panel::set_command_hook(&self.state, None);
        if let Some(agent) = self.state.agent_controller() {
            agent.stop();
        }
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn perm(id: &str, name: &str, kind: &str) -> PermOption {
    PermOption {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
    }
}

fn bash_permission(request: u64) -> AgentEvent {
    AgentEvent::Permission {
        request,
        title: "rm -rf .build".into(),
        options: vec![
            perm("allow-once", "Yes", "allow_once"),
            perm(
                "allow-with-updates",
                "Yes, and don't ask again for rm commands",
                "allow_always",
            ),
            perm("reject", "No", "reject_once"),
        ],
    }
}

fn answers(commands: &[PanelCommand]) -> Vec<(u64, Option<String>)> {
    commands
        .iter()
        .filter_map(|c| match c {
            PanelCommand::Permission { request, option_id } => Some((*request, option_id.clone())),
            _ => None,
        })
        .collect()
}

fn prompts(commands: &[PanelCommand]) -> Vec<(Vec<PromptBlock>, PromptOrigin)> {
    commands
        .iter()
        .filter_map(|c| match c {
            PanelCommand::Prompt { blocks, origin } => Some((blocks.clone(), origin.clone())),
            _ => None,
        })
        .collect()
}

fn text_of(blocks: &[PromptBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            PromptBlock::Text(t) => Some(t.as_str()),
            PromptBlock::Resource { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn select(editor: &Editor, from: (i32, i32), to: (i32, i32)) {
    let buffer = editor.buffer();
    let a = buffer.iter_at_line_offset(from.0, from.1).unwrap();
    let b = buffer.iter_at_line_offset(to.0, to.1).unwrap();
    buffer.select_range(&a, &b);
}

fn ready_event() -> AgentEvent {
    AgentEvent::Ready {
        session: "s1".into(),
        agent: "claude-agent-acp 0.89.1".into(),
        modes: vec![
            ("default".into(), "Default".into()),
            ("plan".into(), "Plan Mode".into()),
        ],
        mode: "default".into(),
        options: vec![ConfigOpt {
            id: "model".into(),
            name: "Model".into(),
            current: "default".into(),
            values: vec![
                ("default".into(), "Default (recommended)".into()),
                ("opus".into(), "Opus".into()),
                ("sonnet".into(), "Sonnet".into()),
            ],
        }],
    }
}

fn drain(rx: &async_channel::Receiver<AgentCmd>) -> Vec<AgentCmd> {
    common::pump_for(Duration::from_millis(30));
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn sent_prompts(cmds: &[AgentCmd]) -> usize {
    cmds.iter()
        .filter(|c| matches!(c, AgentCmd::Prompt(_)))
        .count()
}

fn describe(widget: Option<&gtk::Widget>) -> String {
    match widget {
        None => "none".into(),
        Some(w) => {
            let label = w
                .downcast_ref::<gtk::Button>()
                .and_then(|b| b.label().map(|l| l.to_string()))
                .or_else(|| w.tooltip_text().map(|t| t.to_string()))
                .unwrap_or_default();
            format!("{} {:?} {label:?}", w.type_().name(), w.widget_name())
        }
    }
}

fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone().upcast::<gtk::Widget>()];
    while let Some(w) = stack.pop() {
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c.clone());
            out.push(c);
        }
    }
    out
}

const MAIN: &str = "\\documentclass{article}\n\\begin{document}\nFirst line of text.\n\nA second paragraph\nthat spans two lines.\n\n\\end{document}\n";

// ----- C-1: a permission request takes the focus from the editor -----

/// Narrow window, the user writes in the editor while a turn runs; a permission request must
/// not move the keyboard focus into the pane, where the next `y` (or Space on a focused
/// button) answers it.
fn permission_request_keeps_the_editor_focus() {
    let f = fixture("perm-editor-focus", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    assert!(f.state.agent.split.is_collapsed(), "a narrow window");
    f.begin("compile and clean up", PromptOrigin::Panel);
    editor.view().grab_focus();
    common::pump_for(Duration::from_millis(50));
    let view: gtk::Widget = editor.view().clone().upcast();
    assert!(
        f.focus().is_some_and(|w| w == view),
        "the user types in the editor"
    );
    f.event(bash_permission(7));
    common::pump_for(Duration::from_millis(200));
    f.frames(2);
    let focus = f.focus();
    println!("    focus after the request: {}", describe(focus.as_ref()));
    assert!(
        !f.focus_in_panel() && focus.is_some_and(|w| w == view),
        "the request moved the focus out of the editor into the pane ({})",
        describe(f.focus().as_ref())
    );
}

// ----- C-36: the card's auto-focus lands on "Yes" -----

/// A card that takes the focus from the empty prompt box puts it on its first button,
/// "Yes": the Space or Enter of whatever the user types next allows the command.
fn permission_autofocus_is_not_an_allow_button() {
    let f = fixture("perm-autofocus", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("clean up", PromptOrigin::Panel);
    panel.prompt_view().grab_focus();
    f.frames(1);
    assert!(panel.prompt_view().has_focus());
    f.event(bash_permission(3));
    let prompt: gtk::Widget = panel.prompt_view().upcast();
    common::pump_until(TIMEOUT, || f.focus().is_some_and(|w| w != prompt));
    f.frames(2);
    let focus = f.focus().expect("a focus widget");
    println!("    focus after the card: {}", describe(Some(&focus)));
    // Space or Enter activates the focused widget.
    // (A button's `activate` clicks it after GTK's 250 ms press animation.)
    focus.activate();
    common::pump_for(Duration::from_millis(500));
    let commands = f.commands();
    assert!(
        !answers(&commands)
            .iter()
            .any(|(_, o)| o.as_deref().is_some_and(|o| o.starts_with("allow")))
            && !commands.contains(&PanelCommand::Cancel),
        "Space/Enter on the auto-focused widget ({}) answered the card: {:?}",
        describe(Some(&focus)),
        commands
    );
}

// ----- C-2: Esc / Stop while the agent starts -----

fn starting_fixture(name: &str) -> (Fixture, async_channel::Receiver<AgentCmd>) {
    let f = fixture(name, &[("main.tex", MAIN)], Hook::Forward);
    let (tx, rx) = async_channel::unbounded();
    bridge::attach_channel(&f.agent(), tx);
    assert_eq!(f.agent().state(), AgentState::Starting);
    (f, rx)
}

/// The first Ctrl+K of a session: Enter, then Esc during "Starting Claude…". The prompt must
/// not be sent once the agent is ready.
fn inline_esc_cancels_while_starting() {
    let (f, rx) = starting_fixture("inline-esc-start");
    let editor = f.open("main.tex");
    select(&editor, (2, 0), (2, 19));
    inline::open(&f.state, &editor);
    inline::submit(&f.state, "Make it bold");
    assert!(!prompts(&f.commands()).is_empty(), "submitted");
    common::pump_for(Duration::from_millis(50));
    inline::cancel(&f.state);
    assert!(inline::popover(&f.state).is_none());
    // The agent becomes ready.
    bridge::handle_event(&f.agent(), ready_event());
    common::pump_for(Duration::from_millis(500));
    let cmds = drain(&rx);
    assert_eq!(
        sent_prompts(&cmds),
        0,
        "the cancelled inline edit was sent after Esc (state {:?}): {cmds:?}",
        f.agent().state()
    );
}

/// A prompt queued while the agent starts: Ctrl+Shift+Backspace (win.agent-stop) must cancel
/// it.
fn agent_stop_cancels_while_starting() {
    let (f, rx) = starting_fixture("stop-start");
    f.show_panel();
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("Please tighten the abstract".into())],
        PromptOrigin::Panel
    ));
    common::pump_for(Duration::from_millis(50));
    WidgetExt::activate_action(&f.state.window, "win.agent-stop", None).unwrap();
    bridge::handle_event(&f.agent(), ready_event());
    common::pump_for(Duration::from_millis(500));
    let cmds = drain(&rx);
    assert_eq!(
        sent_prompts(&cmds),
        0,
        "Stop during the start did not cancel the queued prompt (state {:?}): {cmds:?}",
        f.agent().state()
    );
}

// ----- C-3: Clear Transcript with a pending permission -----

/// Clearing the transcript drops a pending card without answering it: the agent waits for
/// ever and y/n no longer answer.
fn clear_answers_pending_permission() {
    let f = fixture("clear-perm", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compile it", PromptOrigin::Panel);
    f.event(bash_permission(9));
    panel.flush();
    f.frames(1);
    panel.actions().activate_action("clear", None);
    common::pump_for(Duration::from_millis(50));
    let answered = answers(&f.commands()).iter().any(|(r, _)| *r == 9);
    // Still answerable with a key from the pane (focus not in the prompt box)?
    let still_pending = panel.entries().iter().any(
        |e| matches!(e, Entry::Permission(c) if c.request == 9 && c.answer == PermissionAnswer::Pending),
    );
    assert!(
        answered || still_pending,
        "after Clear the request is neither answered nor shown; the turn hangs (running={})",
        f.agent().is_running()
    );
}

// ----- C-4: a refused prompt's error restores the prompt being sent -----

/// While the first prompt waits for the agent to start, a second one (`:Agent …`) is refused
/// with an Error event; the panel then puts the first prompt, which is still being sent, back
/// into the box.
fn refused_prompt_keeps_the_sent_text_out_of_the_box() {
    let (f, rx) = starting_fixture("refused-restore");
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    view.buffer().set_text("Please tighten the abstract");
    panel.send();
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    common::pump_for(Duration::from_millis(50));
    assert_eq!(view.buffer().char_count(), 0, "the box is cleared on send");
    let second = quick::agent_command(&f.state, "explain the notation");
    common::pump_for(Duration::from_millis(50));
    bridge::handle_event(&f.agent(), ready_event());
    common::pump_until(TIMEOUT, || f.agent().is_running());
    let cmds = drain(&rx);
    let buffer = view.buffer();
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    println!(
        "    second prompt: {second:?}; sent prompts: {}",
        sent_prompts(&cmds)
    );
    assert_eq!(
        text.as_str(),
        "",
        "the prompt being sent was put back into the box"
    );
}

// ----- C-5: pickers -----

fn picker_resyncs_after_a_refused_change() {
    let f = fixture("picker-refused", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.event(ready_event());
    f.agent().set_state(AgentState::Idle);
    let model = panel.model_picker();
    assert_eq!(model.selected(), 0);
    model.set_selected(2);
    assert!(f.take_commands().contains(&PanelCommand::SetConfig {
        id: "model".into(),
        value: "sonnet".into()
    }));
    // The adapter refuses (acp.rs `request_failed`).
    f.event(AgentEvent::Error(
        "Could not set model to \"sonnet\": Internal error".into(),
    ));
    common::pump_for(Duration::from_millis(20));
    assert_eq!(
        model.selected(),
        0,
        "the picker shows Sonnet although the session stays on the default model"
    );
}

fn picker_choice_while_stopped_is_not_lost() {
    let f = fixture("picker-stopped", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    let agent = f.agent();
    f.event(ready_event());
    agent.set_state(AgentState::Idle);
    // "Stop Agent (Free Memory)".
    panel.actions().activate_action("shutdown", None);
    agent.set_state(AgentState::Stopped);
    let model = panel.model_picker();
    let interactive = model.is_visible() && model.is_sensitive();
    println!("    after Stop Agent: picker interactive = {interactive}");
    if interactive {
        model.set_selected(2);
        // Sent to an agent that does not exist (`bridge::set_config` drops it).
        f.take_commands();
        // The next start: the agent comes back on its default model.
        agent.set_state(AgentState::Starting);
        f.event(ready_event());
        agent.set_state(AgentState::Idle);
        let applied = f.take_commands().contains(&PanelCommand::SetConfig {
            id: "model".into(),
            value: "sonnet".into(),
        });
        assert!(
            applied && model.selected() == 2,
            "the model chosen while the agent was stopped was dropped (picker now {}, applied \
             {applied})",
            model.selected()
        );
    }
}

// ----- C-9: the usage ring after New Session -----

fn usage_ring_resets_on_new_session() {
    let f = fixture("ring-new-session", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.event(AgentEvent::Usage {
        used: 190_000,
        size: 200_000,
    });
    assert!(panel.usage_ring().is_visible());
    panel.actions().activate_action("new-session", None);
    common::pump_for(Duration::from_millis(20));
    assert!(panel.entries().is_empty(), "the transcript is cleared");
    assert!(
        panel.usage().is_none() && !panel.usage_ring().is_visible(),
        "the new session shows the old one's usage: {:?}, {:?}",
        panel.usage(),
        panel.usage_ring().tooltip_text()
    );
}

// ----- C-10: Stop Agent during the start -----

fn stop_agent_while_starting_shows_stopped() {
    let (f, _rx) = starting_fixture("stop-while-starting");
    let panel = f.panel();
    panel.actions().activate_action("start", None);
    assert_eq!(
        panel.setup().map(|s| s.0).as_deref(),
        Some("Starting Claude…")
    );
    panel.actions().activate_action("shutdown", None);
    assert_eq!(f.agent().state(), AgentState::Stopped);
    common::pump_for(Duration::from_millis(300));
    let card = panel.setup();
    assert_eq!(
        card.as_ref().map(|s| s.0.as_str()),
        Some("Claude is stopped"),
        "the stale start attempt replaced the stopped card: {card:?}"
    );
}

// ----- C-15: Caps Lock -----

/// With Caps Lock the `y` key gives the keyval `Y` with LOCK_MASK (no Shift): it must allow
/// once, not "don't ask again".
fn caps_lock_y_allows_once() {
    let f = fixture("caps-lock", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compile it", PromptOrigin::Panel);
    f.event(bash_permission(4));
    panel.flush();
    common::pump_until(TIMEOUT, || !panel.permission_buttons(4).is_empty());
    assert!(panel.permission_buttons(4)[0].grab_focus());
    panel.key_pressed(Key::Y, ModifierType::LOCK_MASK);
    assert_eq!(
        answers(&f.commands()),
        [(4, Some("allow-once".to_owned()))],
        "Caps Lock + y granted a permanent permission"
    );
}

// ----- C-16: accessibility -----

fn prompt_box_has_an_accessible_name() {
    let f = fixture("a11y", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let prompt = f.panel().prompt_view();
    let named = gtk::test_accessible_has_property(&prompt, gtk::AccessibleProperty::Label)
        || gtk::test_accessible_has_relation(&prompt, gtk::AccessibleRelation::LabelledBy);
    assert!(named, "the Claude prompt box has no accessible name");
}

// ----- C-30: "Ask agent to fix" context size -----

/// A build error on a huge generated line (pgfplots coordinates): the ±20 lines go into the
/// prompt whole, unlike a quick action's selection (at most MAX_SELECTION_CHARS).
fn fix_diagnostic_context_is_bounded() {
    let huge: String = (0..40_000).map(|i| format!("({i},{i}) ")).collect();
    let text =
        format!("\\begin{{document}}\n\\addplot coordinates {{{huge}}};\n\\end{{document}}\n");
    let f = fixture("fix-huge", &[("main.tex", &text)], Hook::Swallow);
    let path = f.dir.join("main.tex").canonicalize().unwrap();
    let problem = Problem {
        path,
        line: Some(2),
        severity: Severity::Error,
        message: "Package pgfplots Error: coordinates".into(),
        context: None,
        source: DiagSource::Tex,
    };
    quick::fix_diagnostic(&f.state, &problem);
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, _) = prompts(&f.commands()).pop().unwrap();
    let len = text_of(&blocks).chars().count();
    println!(
        "    prompt length: {len} characters (the line has {})",
        huge.len()
    );
    assert!(
        len <= quick::MAX_SELECTION_CHARS + 2_000,
        "Ask agent to fix sends {len} characters"
    );
}

// ----- C-33: [keys] override of a quick action -----

fn quick_key_override_survives_prompts_reload() {
    let mut keys = lemma_config::KeysCfg::default();
    keys.bindings
        .insert("win.quick-proofread".into(), "<Control><Alt>r".into());
    let bindings = keys.clone();
    let f = fixture_with("quick-keys", &[("main.tex", MAIN)], Hook::Swallow, |c| {
        c.keys = bindings;
    });
    lemma_app::app::set_accels(&f.state.app, &keys);
    let before = f.state.app.accels_for_action("win.quick-proofread");
    assert_eq!(before, ["<Control><Alt>r"]);
    // prompts.toml changes (a new template): the quick actions are rebuilt.
    let user = parse_prompts("[[prompt]]\nname = \"Shorten\"\nprompt = \"Shorten {selection}\"\n")
        .unwrap();
    let merged = lemma_config::merge_prompts(lemma_config::default_prompts(), user);
    f.state.set_prompts(Rc::new(merged));
    let after = f.state.app.accels_for_action("win.quick-proofread");
    lemma_app::app::set_accels(&f.state.app, &lemma_config::KeysCfg::default());
    assert_eq!(
        after,
        ["<Control><Alt>r"],
        "the [keys] override was dropped by the prompts.toml reload"
    );
}

// ----- C-37: the "N changes to review" row -----

fn summary_row_follows_the_review() {
    let f = fixture(
        "summary-row",
        &[("main.tex", MAIN), ("other.tex", "alpha\nbeta\n")],
        Hook::Swallow,
    );
    f.open("main.tex");
    f.show_panel();
    let panel = f.panel();
    let agent = f.agent();
    let (turn, _) = block_on(bridge::start_turn(
        &agent,
        vec![PromptBlock::Text("edit both".into())],
        PromptOrigin::Panel,
    ))
    .unwrap();
    f.edit(
        1,
        "main.tex",
        "A second paragraph",
        "A better second paragraph",
    );
    f.edit(2, "other.tex", "beta", "gamma");
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::EndTurn));
    assert_eq!(review_ui::summary(&f.state.workspace), (2, 2));
    let summary = |p: &panel::Panel| {
        p.entries().into_iter().find_map(|e| match e {
            Entry::Summary { files, hunks } => Some((files, hunks)),
            _ => None,
        })
    };
    assert_eq!(summary(&panel), Some((2, 2)));
    let outcome = review_ui::decide_turn(&f.state, turn, Decision::Reject);
    assert_eq!(outcome.decided, 2);
    assert_eq!(
        review_ui::summary(&f.state.workspace).1,
        0,
        "nothing pending"
    );
    panel.flush();
    let row = summary(&panel);
    let texts = panel.row_texts();
    assert!(
        row.is_none_or(|(_, hunks)| hunks == 0),
        "the panel still says {row:?} changes to review after Reject Turn: {:?}",
        texts.last()
    );
}

// ----- C-38: a start failure in the transcript -----

/// One failed start with a queued prompt: the same message shows in the set-up card and twice
/// in the transcript (the bridge's Error event, then run_prompt's error).
fn start_failure_reported_once() {
    let (f, _rx) = starting_fixture("start-failure-once");
    f.show_panel();
    let panel = f.panel();
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("hello".into())],
        PromptOrigin::Panel
    ));
    common::pump_for(Duration::from_millis(50));
    let message = "the scenario is not valid JSON: expected value at line 1";
    bridge::handle_event(&f.agent(), AgentEvent::Error(message.into()));
    common::pump_for(Duration::from_millis(200));
    let rows = panel
        .entries()
        .iter()
        .filter(|e| **e == Entry::Error(message.into()))
        .count();
    let card = panel.setup();
    println!("    card {card:?}; transcript rows with the message: {rows}");
    assert!(
        rows <= 1,
        "one start failure is reported {rows} times in the transcript (and in the card)"
    );
}

// ----- C-18: streaming a code block -----

/// Frame cost (frame clock `update` → `after-paint`) while a message streams, at `size` bytes:
/// the median of the frames after appending a chunk.
fn streaming_frame_cost(f: &Fixture, code: bool, size: usize) -> Duration {
    let panel = f.panel();
    f.begin("write it", PromptOrigin::Panel);
    let line = "\\item $x_i \\le y_i$ for every $i$ in the index set, as required.\n";
    let mut text = String::new();
    if code {
        text.push_str("```latex\n");
    }
    while text.len() < size {
        if !code && text.len() % 600 < line.len() {
            text.push('\n');
        }
        text.push_str(line);
    }
    f.event(AgentEvent::MessageChunk(text));
    panel.flush();
    f.frames(3);
    let clock = f.state.window.frame_clock().expect("a frame clock");
    let started: Rc<RefCell<Option<Instant>>> = Rc::default();
    let costs: Rc<RefCell<Vec<Duration>>> = Rc::default();
    let s = started.clone();
    let update = clock.connect_update(move |_| {
        s.replace(Some(Instant::now()));
    });
    let (s, c) = (started.clone(), costs.clone());
    let paint = clock.connect_after_paint(move |_| {
        if let Some(t) = s.take() {
            c.borrow_mut().push(t.elapsed());
        }
    });
    for _ in 0..12 {
        f.event(AgentEvent::MessageChunk(line.into()));
        f.frames(1);
    }
    clock.disconnect(update);
    clock.disconnect(paint);
    f.end(StopReason::EndTurn);
    let mut costs = costs.take();
    costs.sort();
    costs.get(costs.len() / 2).copied().unwrap_or_default()
}

/// A 20 kB fenced code block that streams costs a full re-layout of the whole block per frame
/// (one wrapping, selectable label set to the whole text); paragraphs of the same size do not.
fn code_block_streaming_frame_cost() {
    let f = fixture("code-stream", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let paragraphs = streaming_frame_cost(&f, false, 20_000);
    let g = fixture("code-stream-2", &[("main.tex", MAIN)], Hook::Swallow);
    g.show_panel();
    let code = streaming_frame_cost(&g, true, 20_000);
    println!("    median frame at 20 kB: paragraphs {paragraphs:?}, code block {code:?}");
    assert!(
        code <= Duration::from_millis(8).max(paragraphs * 3),
        "a streaming 20 kB code block costs {code:?} per frame (paragraphs {paragraphs:?})"
    );
}

// ----- C-39: transcript rows -----

/// 60 turns with a 20-paragraph answer each: the list keeps a row (with a label per block)
/// for every item, on screen or not.
fn transcript_rows_are_recycled() {
    let f = fixture("rows", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    let paragraph = "A paragraph of an answer about $\\mathcal{C}$ and its objects, with *some* \
                     emphasis and `code`.\n\n";
    let rss = || {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        pages * 4096 / (1 << 20)
    };
    let before = rss();
    for i in 0..60 {
        f.begin(&format!("question {i}"), PromptOrigin::Panel);
        f.event(AgentEvent::MessageChunk(paragraph.repeat(20)));
        f.end(StopReason::EndTurn);
        panel.flush();
        f.frames(1);
    }
    panel.scroll_to_end();
    f.frames(3);
    let labels = descendants(&panel.widget())
        .into_iter()
        .filter(|w| w.is::<gtk::Label>())
        .count();
    let entries = panel.entries().len();
    println!(
        "    {entries} transcript items, {labels} labels alive; RSS {before} → {} MiB",
        rss()
    );
    assert!(
        labels < 400,
        "{labels} labels for {entries} items: every row stays alive"
    );
}
