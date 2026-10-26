//! The agent pane and the agent QoL features (WP2.5): streaming transcript, Markdown rows,
//! permission cards and their keys, Stop, the mode/model/effort pickers, the usage ring, `/` and
//! `@` suggestions, the selection chip, Ctrl+K inline edits, quick actions, "Ask agent to fix",
//! tool cards that link to hunks, the set-up card (every install branch, with a fake `npm` and
//! `claude` on a temporary `PATH`), and a snapshot of the pane.
//!
//! Most tests drive the panel without an agent process: a command hook (see
//! `agent::panel::set_command_hook`) takes the panel's commands, and the test emulates the agent
//! through `AgentController::{begin_turn, set_state, emit_prompt, emit_event}`. Tests named
//! `e2e_*` run the real bridge with `tools/lemma-fake-agent` (built next to the test first, see
//! `common::require_agent_binaries`).
//!
//! The process environment is isolated before GTK starts: `HOME` and the `XDG_*` directories
//! point into a temporary directory, and `PATH` is a temporary `bin` directory plus `/usr/bin`
//! and `/bin`, so nothing here can find or run the real node, npm or claude. Text views use
//! GTK's own input method (`GTK_IM_MODULE`), never the desktop's input method daemon.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_agent_panel`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `f-agent-panel.png` there. The ignored `perf_streamed_table`
//! times a long table while it streams (release: `cargo test --release -p lemma-app --test
//! gtk_agent_panel -- --ignored perf`).

mod broadway_input;
mod common;

use adw::prelude::*;
use gtk::gdk::{Key, ModifierType};
use lemma_agent::{
    AgentCmd, AgentEvent, ConfigOpt, EditorRequest, PermOption, PromptBlock, StopReason, ToolStatus,
};
use lemma_app::agent::panel::{self, Entry, PanelCommand, PermissionAnswer};
use lemma_app::agent::{
    AgentController, AgentState, PromptOrigin, SentPrompt, bridge, inline, install, quick,
};
use lemma_app::diagnostics_panel::Problem;
use lemma_app::editor::Editor;
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{AgentBackend, Config, Paths, parse_prompts};
use lemma_core::{DiagSource, ProjectRoot, Severity};
use std::cell::RefCell;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    if broadway_input::is_child() {
        isolate_environment();
        return common::run(&[(real_keys::CHILD, real_keys::child)]);
    }
    let ignored = std::env::args().any(|a| a == "--ignored" || a == "--include-ignored");
    // `e2e_*` need the fake agent and the MCP bridge; cargo needs the real HOME and PATH.
    common::require_agent_binaries();
    isolate_environment();
    let mut tests: Vec<common::Test> = vec![
        ("streaming_renders_in_order", streaming_renders_in_order),
        ("streaming_cost_per_chunk", streaming_cost_per_chunk),
        ("markdown_rows", markdown_rows),
        ("streaming_table_updates_in_place", streaming_table),
        ("permission_card_keys", permission_card_keys),
        (
            "permission_card_buttons_and_plan_options",
            permission_buttons,
        ),
        (
            "permission_card_withdrawn_by_the_agent",
            permission_withdrawn,
        ),
        ("stop_gives_a_cancelled_turn", stop_cancels),
        ("pickers_send_mode_and_config", pickers),
        ("usage_ring_updates", usage_ring),
        ("prompt_box_enter_and_shift_enter", prompt_box),
        ("prompt_history_up_and_down", prompt_history),
        (
            "prompt_keys_go_to_the_input_method_while_it_composes",
            prompt_preedit,
        ),
        ("prompt_box_on_real_key_events", real_keys::parent),
        (
            "prompt_is_restored_when_the_turn_cannot_start",
            prompt_restored,
        ),
        ("slash_and_file_suggestions", suggestions),
        ("selection_chip", selection_chip),
        ("attachments_from_files_and_paste", attachments),
        ("attachments_too_large_to_include", large_attachments),
        ("tool_cards_link_to_hunks", tool_cards),
        (
            "cli_tools_have_names_and_tool_search_is_quiet",
            cli_tool_rows,
        ),
        ("agent_text_with_nul_is_shown", nul_text),
        (
            "permission_while_typing_in_a_narrow_window_waits_for_ctrl_l",
            permission_while_typing,
        ),
        (
            "permission_while_typing_in_a_wide_window_shows_the_pane",
            permission_while_typing_wide,
        ),
        ("inline_edit_sends_the_region", inline_edit),
        ("inline_edit_escape_cancels", inline_escape),
        (
            "regress3_inline_popover_of_a_closed_tab_leaves_no_critical",
            inline_closed_tab,
        ),
        ("quick_actions_fill_templates", quick_actions),
        ("quick_actions_reload_with_prompts_toml", quick_reload),
        ("ask_agent_to_fix_sends_the_diagnostic", ask_agent_to_fix),
        ("lazy_start_and_retry", lazy_start),
        ("stop_agent_and_new_session", stop_and_new_session),
        ("idle_stop_waits_for_the_user", idle_stop_waits),
        (
            "idle_stop_never_for_zero_minutes_or_the_api",
            idle_stop_never,
        ),
        ("install_adapter_with_fake_npm", install_success),
        ("install_failure_and_cancel", install_failure),
        ("diagnose_missing_node_and_claude", diagnose_missing),
        ("claude_update_hint", update_hint),
        ("snapshot_agent_panel", snapshot),
        ("e2e_basic_scenario", e2e_basic),
        ("e2e_permission_keys", e2e_permission),
        ("e2e_stop_cancels", e2e_cancel),
        ("e2e_pickers_switch_mode_and_model", e2e_pickers),
        ("e2e_inline_edit_hunks_only_in_region", e2e_inline),
        ("e2e_notification_when_unfocused", e2e_notification),
        (
            "e2e_idle_stop_resumes_the_conversation",
            e2e_idle_stop_resumes,
        ),
        (
            "e2e_idle_stop_then_another_folder_is_a_new_conversation",
            e2e_idle_stop_elsewhere,
        ),
    ];
    if ignored {
        tests.push(("perf_streamed_table", perf_streamed_table));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(20);

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

/// Points HOME, XDG_* and PATH into a temporary directory (see the module docs). Runs before
/// GTK starts any thread.
fn isolate_environment() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-panel-env-{}", std::process::id()));
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
        // GTK's own input method, never the desktop's (IBus or Fcitx, which GTK would pick
        // when their module is installed, and which answer keys asynchronously): the real-key
        // test composes a dead key with it.
        std::env::set_var("GTK_IM_MODULE", "gtk-im-context-simple");
    }
    let _ = ENV.set(TestEnv { home, bin, config });
}

/// Writes an executable shell script into the temporary `bin`.
fn fake_program(name: &str, body: &str) -> PathBuf {
    let path = env().bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn remove_program(name: &str) {
    let _ = std::fs::remove_file(env().bin.join(name));
}

// ----- fixtures -----

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// How the command hook treats commands.
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
    state.window.set_default_size(1000, 740);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    // Broadway activates a new window a moment later, and GTK then moves the focus (which
    // closes a collapsed overlay pane); let that happen first.
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

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file).canonicalize().unwrap()
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

    /// Starts an emulated turn with `text` from `origin`; returns its number.
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

    /// An agent `edit` call, answered as the MCP server's calls are (`agent::tools::handle`,
    /// which also tells the panel through the bridge's tool record).
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

    /// Lets the window draw (rows are created during layout).
    fn frames(&self, n: u32) {
        block_on(testmode::wait_for_frames(&self.state.window, n));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        panel::set_command_hook(&self.state, None);
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn kinds(entries: &[Entry]) -> Vec<&'static str> {
    entries
        .iter()
        .map(|e| match e {
            Entry::User { .. } => "user",
            Entry::Agent { .. } => "agent",
            Entry::Thought { .. } => "thought",
            Entry::Tool(_) => "tool",
            Entry::Plan(_) => "plan",
            Entry::Permission(_) => "permission",
            Entry::Notice(_) => "notice",
            Entry::Error(_) => "error",
            Entry::Summary { .. } => "summary",
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
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        title: "latexmk -pdf main.tex".into(),
        options: vec![
            perm("allow-once", "Yes", "allow_once"),
            perm(
                "allow-with-updates",
                "Yes, and don't ask again for latexmk commands",
                "allow_always",
            ),
            perm("reject", "No", "reject_once"),
        ],
    }
}

fn select(editor: &Editor, from: (i32, i32), to: (i32, i32)) {
    let buffer = editor.buffer();
    let a = buffer.iter_at_line_offset(from.0, from.1).unwrap();
    let b = buffer.iter_at_line_offset(to.0, to.1).unwrap();
    buffer.select_range(&a, &b);
}

const MAIN: &str = "\\documentclass{article}\n\\begin{document}\nFirst line of text.\n\nA second paragraph\nthat spans two lines.\n\n\\end{document}\n";

// ----- transcript -----

fn streaming_renders_in_order() {
    let f = fixture("stream", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("Please fix the abstract", PromptOrigin::Panel);
    for piece in ["I am ", "thinking ", "hard."] {
        f.event(AgentEvent::ThoughtChunk(piece.into()));
    }
    let message =
        "## Plan\n\nI will **tighten** the abstract and keep $\\alpha < \\beta$ as it is.";
    for chunk in message.as_bytes().chunks(3) {
        f.event(AgentEvent::MessageChunk(
            String::from_utf8_lossy(chunk).into(),
        ));
    }
    f.event(AgentEvent::ToolCall {
        id: "t1".into(),
        title: "mcp__lemma__read_file".into(),
        kind: "other".into(),
        status: ToolStatus::Pending,
    });
    f.event(AgentEvent::ToolUpdate {
        id: "t1".into(),
        title: None,
        status: Some(ToolStatus::Completed),
    });
    for chunk in ["Done", ", nothing ", "else."] {
        f.event(AgentEvent::MessageChunk(chunk.into()));
    }
    f.end(StopReason::EndTurn);

    let entries = panel.entries();
    assert_eq!(
        kinds(&entries),
        ["user", "thought", "agent", "tool", "agent"]
    );
    assert_eq!(
        entries[1],
        Entry::Thought {
            text: "I am thinking hard.".into(),
            done: true
        }
    );
    assert_eq!(
        entries[2],
        Entry::Agent {
            text: message.into()
        }
    );
    assert_eq!(
        entries[4],
        Entry::Agent {
            text: "Done, nothing else.".into()
        }
    );
    let Entry::User { heading, text, .. } = &entries[0] else {
        panic!()
    };
    assert_eq!(
        (heading.as_str(), text.as_str()),
        ("You", "Please fix the abstract")
    );

    // The rows show the same, in order.
    panel.flush();
    f.frames(2);
    let rows = panel.row_texts();
    assert_eq!(rows.len(), 5, "{rows:#?}");
    assert!(rows[0].contains("You") && rows[0].contains("Please fix the abstract"));
    assert!(rows[1].contains("Thought"), "{}", rows[1]);
    assert!(
        rows[2].contains("Plan") && rows[2].contains("$\\alpha < \\beta$"),
        "{}",
        rows[2]
    );
    assert!(rows[3].contains("Read file"), "{}", rows[3]);
    assert_eq!(rows[4], "Done, nothing else.");
    assert!(
        panel.status_text().ends_with("idle"),
        "{}",
        panel.status_text()
    );
}

/// The per-chunk main-thread cost stays small (ARCHITECTURE §7: ≤ 2 ms per keystroke; the
/// streaming path appends in O(1) and renders once per frame).
fn streaming_cost_per_chunk() {
    let f = fixture("stream-cost", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("long", PromptOrigin::Panel);
    let agent = f.agent();
    let mut times = Vec::with_capacity(2000);
    let chunk = "Some words of a long answer, with `code` and $x^2$. ";
    for i in 0..2000 {
        let event = AgentEvent::MessageChunk(if i % 40 == 39 {
            "\n\n".into()
        } else {
            chunk.into()
        });
        let t = Instant::now();
        agent.emit_event(&event);
        times.push(t.elapsed());
    }
    // One frame's re-render of the whole ~100 kB message.
    let t = Instant::now();
    panel.flush();
    let flush = t.elapsed();
    times.sort();
    let p99 = times[times.len() * 99 / 100];
    println!(
        "    per chunk p99 {p99:?}, max {:?}; flush {flush:?}",
        times.last().unwrap()
    );
    assert!(p99 < Duration::from_millis(2), "p99 {p99:?}");
    let entries = panel.entries();
    assert_eq!(kinds(&entries), ["user", "agent"]);
    let Entry::Agent { text } = &entries[1] else {
        panic!()
    };
    assert_eq!(text.len(), 1950 * chunk.len() + 50 * 2);
    f.end(StopReason::EndTurn);
}

fn markdown_rows() {
    let f = fixture("markdown", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("show me", PromptOrigin::Panel);
    let message = "# Title\n\n- one\n- two with `\\frac`\n\n```latex\n\\begin{align}\n  a &< b\n\\end{align}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\n---\n\nSee [the docs](https://example.org).";
    f.event(AgentEvent::MessageChunk(message.into()));
    f.end(StopReason::EndTurn);
    panel.flush();
    f.frames(2);
    let rows = panel.row_texts();
    let agent = &rows[1];
    for expected in [
        "Title",
        "•",
        "one",
        "two with \\frac",
        "latex",
        "\\begin{align}\n  a &< b\n\\end{align}",
        "quoted",
        "See the docs.",
    ] {
        assert!(
            agent.contains(expected),
            "{expected:?} missing in {agent:?}"
        );
    }
    // The code block has a copy button that copies the code.
    let root = panel.widget();
    let copy = descendants(&root)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .find(|b| b.has_css_class("lemma-md-copy"))
        .expect("a copy button");
    copy.emit_clicked();
    let clipboard = copy.clipboard();
    let copied = block_on(clipboard.read_text_future()).ok().flatten();
    assert_eq!(
        copied.as_deref(),
        Some("\\begin{align}\n  a &< b\n\\end{align}")
    );
}

/// An answer with a table of `rows` rows of `columns` cells (with code, bold and math) between
/// two paragraphs, and the offset where the table's first row starts. Plain ASCII, so it can be
/// streamed in byte chunks.
fn table_message(rows: usize, columns: usize) -> (String, usize) {
    let mut message = String::from("Here is how the packages compare:\n\n|");
    for c in 0..columns {
        message.push_str(&format!(" Column {c} |"));
    }
    message.push_str("\n|");
    message.push_str(&"---|".repeat(columns));
    message.push('\n');
    let body = message.len();
    for r in 0..rows {
        message.push('|');
        for c in 0..columns {
            let cell = match c % 4 {
                0 => format!("`pkg{r}`"),
                1 => format!("sets **{r}** things in $x_{r}$"),
                2 => format!("main.tex:{r}"),
                _ => format!("draft, final, and {r} more options"),
            };
            message.push_str(&format!(" {cell} |"));
        }
        message.push('\n');
    }
    message.push_str("\nThat is all.");
    (message, body)
}

/// The tables in the pane, each with its cell labels in the order they were attached.
fn tables(panel: &panel::Panel) -> Vec<(gtk::Grid, Vec<gtk::Label>)> {
    descendants(&panel.widget())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Grid>().ok())
        .filter(|g| g.has_css_class("lemma-md-table"))
        .map(|grid| {
            let mut labels = Vec::new();
            let mut child = grid.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                labels.extend(c.downcast::<gtk::Label>().ok());
            }
            (grid, labels)
        })
        .collect()
}

/// A table that is still streaming keeps its grid and its cells' labels: a frame sets the
/// markup of the cells that changed and attaches the rows that came, instead of building every
/// cell again; a row that turns out not to be one (the start of a code fence) goes again.
fn streaming_table() {
    let f = fixture("stream-table", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compare the packages", PromptOrigin::Panel);
    let (table, body) = table_message(20, 3);
    let end = table.find("\n\nThat").unwrap() + 1;
    let table = &table[..end];
    // The header first; the rows are created during layout.
    f.event(AgentEvent::MessageChunk(table[..body].into()));
    panel.flush();
    f.frames(2);
    let [(grid, mut shown)] = <[_; 1]>::try_from(tables(&panel)).expect("one table");
    assert_eq!(shown.len(), 3, "the header's cells");
    let stream = |text: &str| {
        f.event(AgentEvent::MessageChunk(text.into()));
        panel.flush();
        let [(now, labels)] = <[_; 1]>::try_from(tables(&panel)).expect("one table");
        assert_eq!(now, grid, "the table keeps its grid");
        labels
    };
    for chunk in table.as_bytes()[body..].chunks(7) {
        let labels = stream(std::str::from_utf8(chunk).unwrap());
        assert!(
            labels.len() >= shown.len() && labels[..shown.len()] == shown[..],
            "the table keeps its cells"
        );
        shown = labels;
    }
    assert_eq!(shown.len(), 21 * 3);
    // "`" and "``" are a row of their own, "```" opens a code block after the table.
    let labels = stream("`");
    assert_eq!(labels.len(), 22 * 3);
    assert_eq!(labels[..shown.len()], shown[..]);
    assert_eq!(stream("`"), labels);
    assert_eq!(stream("`"), shown);
    let rest = "\n\\frac{a}{b}\n```\n\nThat is all.";
    assert_eq!(stream(rest), shown);
    f.end(StopReason::EndTurn);
    panel.flush();
    f.frames(1);
    let message = format!("{table}```{rest}");
    let blocks = lemma_app::agent::markdown::render(&message);
    let Some(lemma_app::agent::markdown::Block::Table { header, rows }) = blocks.get(1) else {
        panic!("{blocks:#?}")
    };
    assert_eq!(rows.len(), 20);
    let expected: Vec<String> = header
        .iter()
        .map(|h| format!("<b>{h}</b>"))
        .chain(rows.iter().flatten().cloned())
        .collect();
    let [(now, labels)] = <[_; 1]>::try_from(tables(&panel)).expect("one table");
    assert_eq!(now, grid);
    let cells: Vec<String> = labels.iter().map(|l| l.label().to_string()).collect();
    assert_eq!(cells, expected);
    let text = &panel.row_texts()[1];
    assert!(
        text.contains("\\frac{a}{b}") && text.ends_with("That is all."),
        "{text}"
    );
}

/// A long table while it streams: 60 rows of 5 cells in chunks of 10 bytes, each chunk followed
/// by a frame's re-render (`flush`) and by measuring the table at the pane's width, which is
/// the layout that frame does (Broadway draws about one frame a second, so the test does not
/// wait for frames). Prints the cost per chunk and how often the table was built again. Release
/// builds check that the table was never built again and that a chunk costs at most 2 ms p99
/// (ARCHITECTURE §7, as for `perf_render_long_message`). Measured (release, Broadway, a busy
/// machine): 0.4–0.6 ms a chunk, 0.8–1.9 ms p99, 0.3 s for the whole table; building it again
/// on every frame cost 23 ms a chunk, 53 ms p99 and 15 s.
fn perf_streamed_table() {
    let f = fixture("perf-table", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compare the packages", PromptOrigin::Panel);
    let (message, body) = table_message(60, 5);
    f.event(AgentEvent::MessageChunk(message[..body].into()));
    panel.flush();
    f.frames(2);
    let width = panel.widget().width();
    let mut grid = tables(&panel).remove(0).0;
    let (mut costs, mut flushes, mut layouts) = (Vec::new(), Vec::new(), Vec::new());
    let mut rebuilt = 0;
    let end = message.find("\n\nThat").unwrap();
    for chunk in message.as_bytes()[body..end].chunks(10) {
        f.event(AgentEvent::MessageChunk(
            std::str::from_utf8(chunk).unwrap().into(),
        ));
        let t = Instant::now();
        panel.flush();
        let flush = t.elapsed();
        let now = tables(&panel).remove(0).0;
        if now != grid {
            rebuilt += 1;
            grid = now;
        }
        let t = Instant::now();
        grid.measure(gtk::Orientation::Horizontal, -1);
        grid.measure(gtk::Orientation::Vertical, width);
        let layout = t.elapsed();
        flushes.push(flush);
        layouts.push(layout);
        costs.push(flush + layout);
    }
    let cells = tables(&panel).remove(0).1.len();
    f.end(StopReason::EndTurn);
    let p = |v: &mut Vec<Duration>, q: usize| {
        v.sort();
        v[(v.len() * q / 100).min(v.len() - 1)]
    };
    let chunks = costs.len();
    let total: Duration = costs.iter().sum();
    println!(
        "perf_streamed_table: {cells} cells, {chunks} chunks, built again {rebuilt} times; per \
         chunk: flush p50 {:.2?} p99 {:.2?}, layout p50 {:.2?} p99 {:.2?}, both p50 {:.2?} p99 \
         {:.2?} max {:.2?}; {total:.2?} in all",
        p(&mut flushes, 50),
        p(&mut flushes, 99),
        p(&mut layouts, 50),
        p(&mut layouts, 99),
        p(&mut costs, 50),
        p(&mut costs, 99),
        costs.last().unwrap(),
    );
    assert_eq!(cells, 61 * 5);
    if cfg!(debug_assertions) {
        println!("    (debug build: budgets not checked)");
        return;
    }
    assert_eq!(rebuilt, 0, "the table is updated in place");
    let p99 = p(&mut costs, 99);
    assert!(p99 <= Duration::from_millis(2), "p99 {p99:?}");
}

// ----- permissions -----

fn permission_card_keys() {
    let f = fixture("perm-keys", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compile it", PromptOrigin::Panel);
    let answers = |f: &Fixture| -> Vec<(u64, Option<String>)> {
        f.commands()
            .into_iter()
            .filter_map(|c| match c {
                PanelCommand::Permission { request, option_id } => Some((request, option_id)),
                _ => None,
            })
            .collect()
    };
    // Focus in the prompt box: y types, it does not answer.
    f.event(bash_permission(1));
    panel.prompt_view().grab_focus();
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Proceed
    );
    assert!(answers(&f).is_empty());
    // Focus on the card's buttons: y, Y and n answer.
    panel.flush();
    common::pump_until(TIMEOUT, || !panel.permission_buttons(1).is_empty());
    assert!(panel.permission_buttons(1)[0].grab_focus());
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Stop
    );
    // The focus stays in the pane, so the next cards are answered with keys too.
    let window = f.state.window.clone();
    let focus_in_panel = || {
        let focus = gtk::prelude::RootExt::focus(&window);
        focus.is_some_and(|w| w.is_ancestor(&panel.widget()))
    };
    // A card takes the focus from the empty prompt box: the card itself, never a button that
    // Space or Enter would press.
    let card_focused = |request: u64| {
        gtk::prelude::RootExt::focus(&window).is_some_and(|w| {
            w.widget_name() == format!("permission-{request}") && !w.is::<gtk::Button>()
        })
    };
    common::pump_until(TIMEOUT, focus_in_panel);
    f.event(bash_permission(2));
    common::pump_until(TIMEOUT, || card_focused(2));
    // Keys typed right then were meant for the prompt box: they do not answer yet.
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Stop
    );
    assert_eq!(answers(&f).len(), 1, "a key within the grace answered");
    common::pump_for(panel::AUTO_FOCUS_GRACE);
    assert_eq!(
        panel.key_pressed(Key::Y, ModifierType::SHIFT_MASK),
        glib::Propagation::Stop
    );
    common::pump_until(TIMEOUT, focus_in_panel);
    f.event(bash_permission(3));
    common::pump_until(TIMEOUT, || card_focused(3));
    common::pump_for(panel::AUTO_FOCUS_GRACE);
    assert_eq!(
        panel.key_pressed(Key::n, ModifierType::empty()),
        glib::Propagation::Stop
    );
    assert_eq!(
        answers(&f),
        [
            (1, Some("allow-once".to_owned())),
            (2, Some("allow-with-updates".to_owned())),
            (3, Some("reject".to_owned())),
        ]
    );
    // Nothing pending: the keys type again; with modifiers they never answer.
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Proceed
    );
    f.event(bash_permission(4));
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::CONTROL_MASK),
        glib::Propagation::Proceed
    );
    // The answered cards say what was chosen.
    let cards: Vec<PermissionAnswer> = panel
        .entries()
        .into_iter()
        .filter_map(|e| match e {
            Entry::Permission(card) => Some(card.answer),
            _ => None,
        })
        .collect();
    assert_eq!(
        cards,
        [
            PermissionAnswer::Chosen("allow-once".into()),
            PermissionAnswer::Chosen("allow-with-updates".into()),
            PermissionAnswer::Chosen("reject".into()),
            PermissionAnswer::Pending,
        ]
    );
    // The turn's end cancels what is still pending.
    f.end(StopReason::EndTurn);
    let last = panel.entries().into_iter().rev().find_map(|e| match e {
        Entry::Permission(card) => Some(card.answer),
        _ => None,
    });
    assert_eq!(last, Some(PermissionAnswer::Cancelled));
}

fn permission_buttons() {
    let f = fixture("perm-buttons", &[("main.tex", MAIN)], Hook::Swallow);
    let panel = f.panel();
    assert!(!f.state.agent.split.shows_sidebar());
    f.begin("plan it", PromptOrigin::Panel);
    // The adapter's plan-exit options: only the default mode and "keep planning" are offered.
    f.event(AgentEvent::Permission {
        request: 9,
        title: "Ready to code?".into(),
        options: vec![
            perm(
                "exit-plan-clear-auto",
                "Yes, clear context and use auto mode",
                "allow_always",
            ),
            perm("exit-plan-auto", "Yes, and use auto mode", "allow_always"),
            perm(
                "exit-plan-bypass",
                "Yes, and bypass permissions",
                "allow_always",
            ),
            perm(
                "exit-plan-default",
                "Yes, manually approve edits",
                "allow_once",
            ),
            perm("reject", "No, keep planning", "reject_once"),
        ],
    });
    // A request shows the pane.
    assert!(f.state.agent.split.shows_sidebar());
    let Some(Entry::Permission(card)) = panel.entries().pop() else {
        panic!()
    };
    let ids: Vec<&str> = card.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["exit-plan-default", "reject"]);
    panel.flush();
    let widget = panel.widget();
    common::pump_until(TIMEOUT, || widget.is_mapped());
    f.frames(2);
    let buttons = panel.permission_buttons(9);
    let labels: Vec<String> = buttons
        .iter()
        .map(|b| b.label().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        labels,
        ["Yes, manually approve edits (y)", "No, keep planning (n)"]
    );
    // Y has no allow_always option here: the bell, no answer.
    panel.widget().grab_focus();
    panel.key_pressed(Key::Y, ModifierType::SHIFT_MASK);
    buttons[1].emit_clicked();
    assert_eq!(
        f.commands().last(),
        Some(&PanelCommand::Permission {
            request: 9,
            option_id: Some("reject".into())
        })
    );
    assert_eq!(
        f.commands()
            .iter()
            .filter(|c| matches!(c, PanelCommand::Permission { .. }))
            .count(),
        1
    );
}

/// The agent withdrew a request (`$/cancel_request`, the tool use was aborted): its card ends
/// cancelled without an answer, the focus moves on to the next card, and y answers that one.
fn permission_withdrawn() {
    let f = fixture("perm-withdrawn", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compile it", PromptOrigin::Panel);
    f.event(bash_permission(1));
    f.event(bash_permission(2));
    panel.flush();
    common::pump_until(TIMEOUT, || {
        !panel.permission_buttons(1).is_empty() && !panel.permission_buttons(2).is_empty()
    });
    assert!(panel.permission_buttons(1)[0].grab_focus());
    f.event(AgentEvent::PermissionWithdrawn { request: 1 });
    let cards = |panel: &panel::Panel| -> Vec<(u64, PermissionAnswer)> {
        panel
            .entries()
            .into_iter()
            .filter_map(|e| match e {
                Entry::Permission(card) => Some((card.request, card.answer)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        cards(&panel),
        [
            (1, PermissionAnswer::Cancelled),
            (2, PermissionAnswer::Pending)
        ]
    );
    let window = f.state.window.clone();
    let focused = || gtk::prelude::RootExt::focus(&window).map(|w| w.widget_name().to_string());
    common::pump_until(TIMEOUT, || focused().as_deref() == Some("permission-2"));
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Stop
    );
    let answers: Vec<PanelCommand> = f
        .commands()
        .into_iter()
        .filter(|c| matches!(c, PanelCommand::Permission { .. }))
        .collect();
    assert_eq!(
        answers,
        [PanelCommand::Permission {
            request: 2,
            option_id: Some("allow-once".into())
        }],
        "nothing is sent for the withdrawn card"
    );
    // A withdrawal that comes after the answer changes nothing.
    f.event(AgentEvent::PermissionWithdrawn { request: 2 });
    assert_eq!(
        cards(&panel),
        [
            (1, PermissionAnswer::Cancelled),
            (2, PermissionAnswer::Chosen("allow-once".into()))
        ]
    );
}

fn stop_cancels() {
    let f = fixture("stop", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    assert!(!panel.stop_button().is_visible());
    f.begin("long task", PromptOrigin::Panel);
    f.event(AgentEvent::MessageChunk("Working…".into()));
    f.event(bash_permission(5));
    assert!(panel.stop_button().is_visible(), "Stop while running");
    assert!(panel.status_text().ends_with("running"));
    panel.stop_button().emit_clicked();
    assert!(f.commands().contains(&PanelCommand::Cancel));
    // The agent answers with a cancelled turn.
    f.end(StopReason::Cancelled);
    let entries = panel.entries();
    assert!(
        entries.contains(&Entry::Notice("Stopped.".into())),
        "{entries:#?}"
    );
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, Entry::Permission(c) if c.answer == PermissionAnswer::Cancelled))
    );
    assert!(!panel.stop_button().is_visible());
    assert!(panel.status_text().ends_with("idle"));
    // Stop without a running turn sends nothing to the agent.
    f.take_commands();
    WidgetExt::activate_action(&f.state.window, "win.agent-stop", None).unwrap();
    assert_eq!(
        f.commands(),
        [PanelCommand::Cancel],
        "only the hook sees it"
    );
}

fn ready_event() -> AgentEvent {
    AgentEvent::Ready {
        session: "s1".into(),
        agent: "claude-agent-acp 0.89.1".into(),
        modes: vec![
            ("default".into(), "Default".into()),
            ("acceptEdits".into(), "Accept Edits".into()),
            ("plan".into(), "Plan Mode".into()),
            ("bypassPermissions".into(), "Bypass Permissions".into()),
            ("auto".into(), "Auto".into()),
        ],
        mode: "default".into(),
        options: vec![
            ConfigOpt {
                id: "model".into(),
                name: "Model".into(),
                current: "default".into(),
                values: vec![
                    ("default".into(), "Default (recommended)".into()),
                    ("opus".into(), "Opus".into()),
                    ("sonnet".into(), "Sonnet".into()),
                ],
            },
            ConfigOpt {
                id: "effort".into(),
                name: "Effort".into(),
                current: "high".into(),
                values: vec![
                    ("low".into(), "Low".into()),
                    ("high".into(), "High".into()),
                    ("max".into(), "Max".into()),
                ],
            },
        ],
        loadable: false,
    }
}

fn picker_items(dd: &gtk::DropDown) -> Vec<String> {
    let model = dd.model().and_downcast::<gtk::StringList>().unwrap();
    (0..model.n_items())
        .map(|i| model.string(i).unwrap().to_string())
        .collect()
}

fn pickers() {
    let f = fixture("pickers", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    assert!(!panel.mode_picker().is_visible());
    f.event(ready_event());
    f.agent().set_state(AgentState::Idle);
    let (mode, model, effort) = (
        panel.mode_picker(),
        panel.model_picker(),
        panel.effort_picker(),
    );
    assert_eq!(
        picker_items(&mode),
        ["Default", "Plan Mode"],
        "only default and plan"
    );
    assert_eq!(mode.selected(), 0);
    assert_eq!(
        picker_items(&model),
        ["Default (recommended)", "Opus", "Sonnet"]
    );
    assert_eq!(effort.selected(), 1);
    assert!(
        f.take_commands()
            .iter()
            .all(|c| !matches!(c, PanelCommand::SetMode(_) | PanelCommand::SetConfig { .. }))
    );

    mode.set_selected(1);
    model.set_selected(2);
    effort.set_selected(2);
    assert_eq!(
        f.take_commands(),
        [
            PanelCommand::SetMode("plan".into()),
            PanelCommand::SetConfig {
                id: "model".into(),
                value: "sonnet".into()
            },
            PanelCommand::SetConfig {
                id: "effort".into(),
                value: "max".into()
            },
        ]
    );
    // The agent's confirmations update the pickers without sending anything back.
    f.event(AgentEvent::ModeChanged("plan".into()));
    let AgentEvent::Ready { mut options, .. } = ready_event() else {
        unreachable!()
    };
    options[0].current = "opus".into();
    f.event(AgentEvent::ConfigChanged(options));
    assert_eq!((mode.selected(), model.selected()), (1, 1));
    assert!(f.take_commands().is_empty());
    // A mode Lemma does not offer, set by the agent itself, is shown and reported, never
    // offered: choosing it again sends nothing.
    f.event(AgentEvent::ModeChanged("acceptEdits".into()));
    assert_eq!(
        picker_items(&mode),
        ["Default", "Plan Mode", "Accept Edits"]
    );
    assert_eq!(mode.selected(), 2);
    mode.set_selected(2);
    assert!(panel.entries().contains(&Entry::Notice(
        "The agent switched to the mode “acceptEdits”.".into()
    )));
    assert!(f.take_commands().is_empty());
}

fn usage_ring() {
    let f = fixture("usage", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    assert_eq!(panel.usage(), None);
    assert!(!panel.usage_ring().is_visible());
    f.event(AgentEvent::Usage {
        used: 41_250,
        size: 200_000,
    });
    let ring = panel.usage_ring();
    assert_eq!(panel.usage(), Some((41_250, 200_000)));
    assert!(ring.is_visible());
    assert_eq!(
        ring.tooltip_text().as_deref(),
        Some("Context: 41,250 of 200,000 tokens (21%)")
    );
    assert!(!ring.has_css_class("warning"));
    f.event(AgentEvent::Usage {
        used: 170_000,
        size: 200_000,
    });
    assert!(ring.has_css_class("warning"));
    f.event(AgentEvent::Usage {
        used: 199_000,
        size: 200_000,
    });
    assert!(ring.has_css_class("error") && !ring.has_css_class("warning"));
    assert!(descendants(&panel.widget()).iter().any(|w| {
        w.downcast_ref::<gtk::Label>()
            .is_some_and(|l| l.text().ends_with('%') && l.is_visible())
    }));
    f.frames(1);
}

// ----- the prompt box -----

fn type_into(view: &gtk::TextView, text: &str) {
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, text);
    buffer.place_cursor(&buffer.end_iter());
}

fn prompt_box() {
    let f = fixture("prompt-box", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    assert!(!panel.send_button().is_sensitive());
    type_into(&view, "first line");
    assert!(panel.send_button().is_sensitive());
    // Shift+Enter is a newline (the text view inserts it), Enter sends.
    assert_eq!(
        panel.prompt_key(Key::Return, ModifierType::SHIFT_MASK),
        glib::Propagation::Proceed
    );
    type_into(&view, "\nsecond line  ");
    assert_eq!(
        panel.prompt_key(Key::Return, ModifierType::empty()),
        glib::Propagation::Stop
    );
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let sent = prompts(&f.commands());
    assert_eq!(
        sent,
        [(
            vec![PromptBlock::Text("first line\nsecond line".into())],
            PromptOrigin::Panel
        )]
    );
    let buffer = view.buffer();
    assert_eq!(buffer.char_count(), 0, "the box is cleared");
    // While a turn runs, Enter does not send.
    f.begin("first line\nsecond line", PromptOrigin::Panel);
    type_into(&view, "follow-up");
    assert!(!panel.send_button().is_sensitive());
    panel.prompt_key(Key::Return, ModifierType::empty());
    common::pump_for(Duration::from_millis(50));
    assert_eq!(prompts(&f.commands()).len(), 1);
    assert_eq!(buffer.char_count(), 9, "the text stays");
    f.end(StopReason::EndTurn);
    assert!(panel.send_button().is_sensitive());
}

/// The prompt box's text.
fn prompt_text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// The cursor's line and offset in the prompt box.
fn prompt_cursor(view: &gtk::TextView) -> (i32, i32) {
    let buffer = view.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    (cursor.line(), cursor.offset())
}

/// Up or Down in the prompt box as the text view gets it: the panel's key handler first, then,
/// when that lets the key through, the text view's own cursor move.
fn press(panel: &panel::Panel, key: Key) -> glib::Propagation {
    let propagation = panel.prompt_key(key, ModifierType::empty());
    if propagation == glib::Propagation::Proceed {
        let count = if key == Key::Up { -1 } else { 1 };
        panel
            .prompt_view()
            .emit_move_cursor(gtk::MovementStep::DisplayLines, count, false);
    }
    propagation
}

/// Up and Down recall the prompts sent from the box: in the empty box or a recalled prompt
/// nobody edited, on its first (last) line; Down past the newest empties the box again. The
/// box keeps the last `HISTORY_LEN`, each once in a row.
fn prompt_history() {
    use glib::Propagation::{Proceed, Stop};
    let f = fixture("prompt-history", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    let buffer = view.buffer();
    f.event(AgentEvent::Commands(vec![(
        "compact".into(),
        "Compact the conversation".into(),
    )]));
    let send = |text: &str| {
        buffer.set_text(text);
        panel.send();
    };
    send("Fix the abstract");
    send("/compact");
    send("First line\nsecond line");
    // The same once trimmed: not kept twice.
    send("  First line\nsecond line\n");
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 4);
    let text = || prompt_text(&view);
    let two_lines = "First line\nsecond line";
    let end = two_lines.chars().count() as i32;

    // Up in the empty box: the newest prompt, the cursor at its end. Up then moves through its
    // lines before it goes on.
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), two_lines);
    assert_eq!(prompt_cursor(&view), (1, end));
    assert_eq!(press(&panel, Key::Up), Proceed);
    assert_eq!(prompt_cursor(&view).0, 0);
    // A recalled `/command` shows no suggestions, so Up goes on.
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), "/compact");
    assert!(panel.suggestions().is_empty());
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), "Fix the abstract");
    // Nothing older: the text view's Up.
    assert_eq!(press(&panel, Key::Up), Proceed);
    assert_eq!(text(), "Fix the abstract");
    // Down: the newer ones, the cursor at their start, through their lines.
    assert_eq!(press(&panel, Key::Down), Stop);
    assert_eq!(text(), "/compact");
    assert_eq!(press(&panel, Key::Down), Stop);
    assert_eq!(text(), two_lines);
    assert_eq!(prompt_cursor(&view), (0, 0));
    assert_eq!(press(&panel, Key::Down), Proceed);
    assert_eq!(prompt_cursor(&view).0, 1);
    // Past the newest: the empty box, and nothing newer.
    assert_eq!(press(&panel, Key::Down), Stop);
    assert_eq!(text(), "");
    assert_eq!(press(&panel, Key::Down), Proceed);
    assert_eq!(text(), "");
    // The keypad's arrows (Num Lock off) too.
    assert_eq!(panel.prompt_key(Key::KP_Up, ModifierType::empty()), Stop);
    assert_eq!(text(), two_lines);
    assert_eq!(panel.prompt_key(Key::KP_Down, ModifierType::empty()), Stop);
    assert_eq!(text(), "");

    // Editing a recalled prompt makes it a draft: Up and Down move the cursor again.
    assert_eq!(press(&panel, Key::Up), Stop);
    type_into(&view, "!");
    buffer.place_cursor(&buffer.start_iter());
    assert_eq!(press(&panel, Key::Up), Proceed);
    assert_eq!(press(&panel, Key::Down), Proceed);
    assert_eq!(press(&panel, Key::Down), Proceed);
    assert_eq!(text(), "First line\nsecond line!");
    // So do typed text and Up with a modifier (Shift+Up selects).
    buffer.set_text("typed");
    buffer.place_cursor(&buffer.start_iter());
    assert_eq!(press(&panel, Key::Up), Proceed);
    assert_eq!(text(), "typed");
    buffer.set_text("");
    assert_eq!(panel.prompt_key(Key::Up, ModifierType::SHIFT_MASK), Proceed);
    assert_eq!(text(), "");
    // While suggestions show, Up and Down choose among them.
    type_into(&view, "/co");
    assert_eq!(panel.suggestions(), ["/compact"]);
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), "/co");
    assert_eq!(panel.suggestions(), ["/compact"]);
    panel.prompt_key(Key::Escape, ModifierType::empty());
    buffer.set_text("");

    // A recalled prompt sent again is not kept twice; an older one becomes the newest.
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(press(&panel, Key::Up), Proceed);
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), "/compact");
    panel.send();
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), "/compact");
    assert_eq!(press(&panel, Key::Up), Stop);
    assert_eq!(text(), two_lines);
    buffer.set_text("");
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 5);

    // The last `HISTORY_LEN` prompts.
    for i in 0..60 {
        send(&format!("prompt {i}"));
    }
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 65);
    let mut recalled = Vec::new();
    while panel.prompt_key(Key::Up, ModifierType::empty()) == Stop {
        recalled.push(text());
        assert!(recalled.len() <= panel::HISTORY_LEN, "{recalled:?}");
    }
    assert_eq!(recalled.len(), panel::HISTORY_LEN);
    assert_eq!(recalled[0], "prompt 59");
    assert_eq!(recalled.last().map(String::as_str), Some("prompt 10"));
}

/// While the input method composes (preedit text shows), the prompt box's keys are its own:
/// Enter confirms the conversion instead of sending, and Up, Down, Tab and Escape neither
/// choose suggestions nor recall prompts. No input method runs here, so the test emits the
/// text view's `preedit-changed` as one would.
fn prompt_preedit() {
    use glib::Propagation::{Proceed, Stop};
    let f = fixture("prompt-preedit", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    let preedit = |text: &str| view.emit_by_name::<()>("preedit-changed", &[&text]);
    f.event(AgentEvent::Commands(vec![
        ("compact".into(), "Compact the conversation".into()),
        ("context".into(), "Show the context usage".into()),
    ]));
    view.buffer().set_text("Fix the abstract");
    panel.send();
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 1);

    // Composing in the empty box: Up is a candidate key, not the last prompt.
    preedit("にほん");
    assert_eq!(panel.prompt_key(Key::Up, ModifierType::empty()), Proceed);
    assert_eq!(prompt_text(&view), "");
    // Composing after some text: no key sends.
    type_into(&view, "日本語の");
    preedit("ぶんしょう");
    for key in [
        Key::Return,
        Key::KP_Enter,
        Key::Up,
        Key::Down,
        Key::Tab,
        Key::Escape,
    ] {
        assert_eq!(
            panel.prompt_key(key, ModifierType::empty()),
            Proceed,
            "{key:?}"
        );
    }
    common::pump_for(Duration::from_millis(50));
    assert_eq!(prompts(&f.commands()).len(), 1, "nothing was sent");
    // The conversion is committed and the preedit ends: Enter sends all of it.
    type_into(&view, "文章");
    preedit("");
    assert_eq!(panel.prompt_key(Key::Return, ModifierType::empty()), Stop);
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 2);
    assert_eq!(text_of(&prompts(&f.commands())[1].0), "日本語の文章");

    // Suggestions wait for the input method too.
    type_into(&view, "/co");
    assert_eq!(panel.suggestions(), ["/compact", "/context"]);
    preedit("´");
    for key in [Key::Down, Key::Tab, Key::Return, Key::Escape] {
        assert_eq!(
            panel.prompt_key(key, ModifierType::empty()),
            Proceed,
            "{key:?}"
        );
    }
    assert_eq!(prompt_text(&view), "/co");
    assert_eq!(panel.suggestions(), ["/compact", "/context"]);
    preedit("");
    panel.prompt_key(Key::Down, ModifierType::empty());
    assert_eq!(panel.prompt_key(Key::Tab, ModifierType::empty()), Stop);
    assert_eq!(prompt_text(&view), "/context ");

    // A box that shows only preedit text is not empty: a permission card does not take the
    // focus from it, where the next keys of the conversion (`n`, `y`) would answer the card.
    let window = f.state.window.clone();
    let prompt_focused = || gtk::prelude::RootExt::focus(&window) == Some(view.clone().upcast());
    view.buffer().set_text("");
    view.grab_focus();
    common::pump_until(TIMEOUT, prompt_focused);
    preedit("にゅ");
    f.begin("compile it", PromptOrigin::Panel);
    f.event(bash_permission(1));
    panel.flush();
    common::pump_until(TIMEOUT, || !panel.permission_buttons(1).is_empty());
    // Longer than the card waits for its row (30 tries 16 ms apart).
    common::pump_for(Duration::from_millis(700));
    assert!(
        prompt_focused(),
        "the card took the focus from the composing box"
    );
    preedit("");
    f.end(StopReason::EndTurn);
}

// ----- real key events -----

mod real_keys {
    //! The prompt box on real key events (a private Broadway daemon, see `tests/broadway_input`):
    //! the keys take GDK's real path through the pane's and the prompt box's key controllers to
    //! the text view and its input method (GTK's own, which composes dead keys).

    use super::*;
    use broadway_input::{Browser, SHIFT};

    pub const CHILD: &str = "prompt_real_keys_child";

    pub fn parent() {
        broadway_input::run_child(CHILD, 43);
    }

    pub fn child() {
        let f = fixture("real-keys", &[("main.tex", MAIN)], Hook::Swallow);
        f.show_panel();
        let panel = f.panel();
        let view = panel.prompt_view();
        common::pump_until(TIMEOUT, || f.state.window.is_active());
        view.grab_focus();
        common::pump_until(TIMEOUT, || view.has_focus());
        // Frames are drawn: Up and Down look at the lines as laid out.
        let mut browser = Browser::connect_drawing();
        browser.settle = Duration::from_millis(20);
        let sent = || prompts(&f.commands());

        // A dead key composes (the input method shows it as preedit), so Enter is the input
        // method's and sends nothing (before, it sent "abc" and the é went into the empty
        // box); `e` completes the composition, and Enter then sends.
        browser.text("abc");
        browser.key(Key::dead_acute, 0);
        browser.key(Key::Return, 0);
        common::pump_for(Duration::from_millis(100));
        assert!(sent().is_empty(), "Enter while composing sent {:?}", sent());
        browser.text("e");
        common::pump_until(TIMEOUT, || prompt_text(&view) == "abcé");
        browser.key(Key::Return, 0);
        common::pump_until(TIMEOUT, || sent().len() == 1);
        assert_eq!(text_of(&sent()[0].0), "abcé");

        // Up and Down: a two-line prompt (Shift+Enter), then back to the first one.
        browser.text("one");
        browser.key(Key::Return, SHIFT);
        browser.text("two");
        browser.key(Key::Return, 0);
        common::pump_until(TIMEOUT, || sent().len() == 2);
        assert_eq!(text_of(&sent()[1].0), "one\ntwo");
        assert_eq!(prompt_text(&view), "");
        browser.key(Key::Up, 0);
        common::pump_until(TIMEOUT, || prompt_text(&view) == "one\ntwo");
        assert_eq!(prompt_cursor(&view), (1, 7));
        browser.key(Key::Up, 0);
        common::pump_until(TIMEOUT, || prompt_cursor(&view).0 == 0);
        assert_eq!(prompt_text(&view), "one\ntwo");
        browser.key(Key::Up, 0);
        common::pump_until(TIMEOUT, || prompt_text(&view) == "abcé");
        browser.key(Key::Down, 0);
        common::pump_until(TIMEOUT, || prompt_text(&view) == "one\ntwo");
        assert_eq!(prompt_cursor(&view), (0, 0));
        browser.key(Key::Down, 0);
        common::pump_until(TIMEOUT, || prompt_cursor(&view).0 == 1);
        browser.key(Key::Down, 0);
        common::pump_until(TIMEOUT, || prompt_text(&view).is_empty());
        // Sending a recalled prompt.
        browser.key(Key::Up, 0);
        common::pump_until(TIMEOUT, || prompt_text(&view) == "one\ntwo");
        browser.key(Key::Return, 0);
        common::pump_until(TIMEOUT, || sent().len() == 3);
        assert_eq!(text_of(&sent()[2].0), "one\ntwo");
    }
}

fn prompt_restored() {
    // Forwarded to the controller: without a bridge or with one that cannot start the agent,
    // the turn never starts and the text comes back into the box.
    let f = fixture("prompt-restore", &[("main.tex", MAIN)], Hook::Forward);
    let panel = f.panel();
    let view = panel.prompt_view();
    // Never start a real agent from a test: no scenario, an isolated PATH and HOME (see the
    // module docs), so starting fails.
    type_into(&view, "will not start");
    panel.send();
    common::pump_until(TIMEOUT, || {
        view.buffer().char_count() > 0
            && panel.entries().iter().any(|e| matches!(e, Entry::Error(_)))
            || matches!(f.agent().state(), AgentState::Failed(_)) && view.buffer().char_count() > 0
    });
    let buffer = view.buffer();
    assert_eq!(
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), false),
        "will not start"
    );
    f.agent().stop();
}

fn suggestions() {
    let f = fixture(
        "suggest",
        &[
            ("main.tex", MAIN),
            ("chapters/yoneda.tex", "\\section{Yoneda}\nThe lemma.\n"),
            ("refs.bib", "@book{x, title={X}}\n"),
            (".hidden/secret.tex", "no\n"),
            ("figure.pdf", "%PDF"),
        ],
        Hook::Swallow,
    );
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    f.event(AgentEvent::Commands(vec![
        ("compact".into(), "Compact the conversation".into()),
        ("context".into(), "Show the context usage".into()),
        ("init".into(), "Write CLAUDE.md".into()),
    ]));
    type_into(&view, "/co");
    assert_eq!(panel.suggestions(), ["/compact", "/context"]);
    panel.prompt_key(Key::Down, ModifierType::empty());
    panel.prompt_key(Key::Tab, ModifierType::empty());
    let text = |v: &gtk::TextView| {
        let b = v.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    };
    assert_eq!(text(&view), "/context ");
    assert!(panel.suggestions().is_empty());
    // `/` only at the start of the prompt.
    view.buffer().set_text("say /co");
    view.buffer().place_cursor(&view.buffer().end_iter());
    assert!(panel.suggestions().is_empty());

    // @ offers project text files (loaded on a worker), fuzzily matched.
    view.buffer().set_text("Look at @yon");
    view.buffer().place_cursor(&view.buffer().end_iter());
    common::pump_until(TIMEOUT, || !panel.suggestions().is_empty());
    assert_eq!(panel.suggestions(), ["chapters/yoneda.tex"]);
    panel.prompt_key(Key::Return, ModifierType::empty());
    assert_eq!(text(&view), "Look at @chapters/yoneda.tex ");
    type_into(&view, "and @");
    let files = panel.suggestions();
    assert!(files.contains(&"main.tex".to_owned()) && files.contains(&"refs.bib".to_owned()));
    assert!(
        !files
            .iter()
            .any(|p| p.contains("secret") || p.ends_with(".pdf")),
        "{files:?}"
    );
    panel.prompt_key(Key::Escape, ModifierType::empty());
    assert!(panel.suggestions().is_empty());
    // Sending attaches the mentioned files as resources (the live text when open). (main.tex
    // first: the first file opened decides the project root.)
    f.open("main.tex");
    let editor = f.open("chapters/yoneda.tex");
    editor
        .buffer()
        .insert(&mut editor.buffer().end_iter(), "Unsaved.\n");
    view.buffer()
        .set_text("Compare @chapters/yoneda.tex with @main.tex, please. @nobody");
    panel.send();
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, origin) = prompts(&f.commands()).remove(0);
    assert_eq!(origin, PromptOrigin::Panel);
    assert_eq!(blocks.len(), 3, "{blocks:#?}");
    let PromptBlock::Resource { uri, mime, text } = &blocks[1] else {
        panic!()
    };
    assert!(
        uri.starts_with("file://") && uri.ends_with("/chapters/yoneda.tex"),
        "{uri}"
    );
    assert_eq!(mime, "text/x-tex");
    assert_eq!(text, "\\section{Yoneda}\nThe lemma.\nUnsaved.\n");
    let PromptBlock::Resource { text, .. } = &blocks[2] else {
        panic!()
    };
    assert_eq!(text, MAIN);
}

/// A PNG of `w` × `h` noise pixels (noise, so that it does not compress).
fn noise_png(w: i32, h: i32, seed: u32) -> Vec<u8> {
    let mut x = seed;
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        for _ in 0..3 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 24) as u8);
        }
        data.push(255);
    }
    gdk::MemoryTexture::new(
        w,
        h,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(data),
        (w * 4) as usize,
    )
    .save_to_png_bytes()
    .to_vec()
}

/// Images, PDFs and text files attached with the paperclip (what a drop runs too), pasted
/// images and copied files, refusals, removal, a message of attachments only, and the
/// transcript entry with image previews.
fn attachments() {
    let pdf = "%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
    let f = fixture(
        "attach",
        &[
            ("main.tex", MAIN),
            ("notes.txt", "Some notes.\n"),
            ("paper.pdf", pdf),
        ],
        Hook::Swallow,
    );
    std::fs::write(f.dir.join("fig.png"), noise_png(40, 30, 1)).unwrap();
    std::fs::write(f.dir.join("huge.png"), noise_png(2200, 1400, 2)).unwrap();
    std::fs::write(f.dir.join("blob.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    std::fs::create_dir_all(f.dir.join("figs")).unwrap();
    f.show_panel();
    let panel = f.panel();
    let view = panel.prompt_view();
    let file = |name: &str| gio::File::for_path(f.dir.join(name));
    let names = |panel: &panel::Panel| -> Vec<String> {
        panel.attachments().into_iter().map(|(n, _)| n).collect()
    };
    let text = |v: &gtk::TextView| {
        let b = v.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    };
    assert!(panel.attach_button().is_visible());
    assert!(!panel.send_button().is_sensitive());

    // A binary file and a folder are refused; the rest are attached in order.
    panel.attach_files(vec![
        file("fig.png"),
        file("blob.bin"),
        file("notes.txt"),
        file("figs"),
        file("paper.pdf"),
    ]);
    common::pump_until(TIMEOUT, || !panel.is_attaching());
    assert_eq!(names(&panel), ["fig.png", "notes.txt", "paper.pdf"]);
    let details = panel.attachments();
    assert!(
        details[0].1.starts_with("PNG image · 40 × 30 · "),
        "{details:?}"
    );
    assert!(details[1].1.starts_with("Text · "), "{details:?}");
    assert!(details[2].1.starts_with("PDF document · "), "{details:?}");
    assert!(
        panel.send_button().is_sensitive(),
        "attachments alone can be sent"
    );
    // The same file again is attached once.
    panel.attach_files(vec![file("fig.png")]);
    common::pump_until(TIMEOUT, || !panel.is_attaching());
    assert_eq!(names(&panel).len(), 3);
    // An image over the size limit is scaled down and sent as PNG.
    panel.attach_files(vec![file("huge.png")]);
    common::pump_until(Duration::from_secs(90), || !panel.is_attaching());
    let (name, detail) = panel.attachments()[3].clone();
    assert_eq!(name, "huge.png");
    assert!(
        detail.starts_with("PNG image") && detail.ends_with("(scaled down from 2200 × 1400)"),
        "{detail}"
    );

    // Ctrl+V with an image on the clipboard attaches it (it is not pasted as text).
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(noise_png(20, 10, 3))).unwrap();
    view.clipboard().set_texture(&texture);
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || names(&panel).len() == 5);
    assert_eq!(names(&panel)[4], "Pasted image");
    assert_eq!(text(&view), "");
    // Removing one; pasting copied files attaches them; text still pastes as text.
    panel.remove_attachment(1);
    assert_eq!(
        names(&panel),
        ["fig.png", "paper.pdf", "huge.png", "Pasted image"]
    );
    view.clipboard()
        .set(&gdk::FileList::from_array(&[file("notes.txt")]));
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || names(&panel).len() == 5);
    assert_eq!(names(&panel)[4], "notes.txt");
    assert_eq!(text(&view), "");
    view.clipboard().set_text("Describe");
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || text(&view) == "Describe");
    type_into(&view, " these");

    // Sending: the text, then the attachments in order; the chips go.
    panel.send();
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, origin) = prompts(&f.take_commands()).remove(0);
    assert_eq!(origin, PromptOrigin::Panel);
    assert!(panel.attachments().is_empty());
    assert_eq!(blocks.len(), 6, "{blocks:#?}");
    assert_eq!(blocks[0], PromptBlock::Text("Describe these".into()));
    let PromptBlock::Image { uri, mime, data } = &blocks[1] else {
        panic!("{:?}", blocks[1])
    };
    assert!(
        uri.starts_with("file://") && uri.ends_with("/fig.png"),
        "{uri}"
    );
    assert_eq!(mime, "image/png");
    assert_eq!(*data, std::fs::read(f.dir.join("fig.png")).unwrap());
    let PromptBlock::Document { uri, mime, data } = &blocks[2] else {
        panic!("{:?}", blocks[2])
    };
    assert!(uri.ends_with("/paper.pdf"), "{uri}");
    assert_eq!(mime, "application/pdf");
    assert_eq!(data, pdf.as_bytes());
    let PromptBlock::Image { mime, data, .. } = &blocks[3] else {
        panic!("{:?}", blocks[3])
    };
    assert_eq!(mime, "image/png");
    assert!(data.len() <= lemma_app::agent::attach::MAX_IMAGE_BYTES);
    let scaled = gdk::Texture::from_bytes(&glib::Bytes::from(data.as_slice())).unwrap();
    assert!(scaled.width() <= 1568);
    assert!((scaled.width() * 1400 / 2200 - scaled.height()).abs() <= 1);
    let PromptBlock::Image { uri, mime, .. } = &blocks[4] else {
        panic!("{:?}", blocks[4])
    };
    assert_eq!((uri.as_str(), mime.as_str()), ("", "image/png"));
    let PromptBlock::Resource {
        text: notes, mime, ..
    } = &blocks[5]
    else {
        panic!("{:?}", blocks[5])
    };
    assert_eq!(
        (notes.as_str(), mime.as_str()),
        ("Some notes.\n", "text/plain")
    );

    // The transcript lists the attachments and shows previews of the images.
    let agent = f.agent();
    let turn = agent.begin_turn();
    agent.set_state(AgentState::Running { turn });
    agent.emit_prompt(&SentPrompt {
        turn,
        origin: PromptOrigin::Panel,
        blocks,
    });
    let user = || {
        panel
            .entries()
            .into_iter()
            .find_map(|e| match e {
                Entry::User {
                    attachments,
                    images,
                    ..
                } => Some((attachments, images)),
                _ => None,
            })
            .expect("the prompt's entry")
    };
    assert_eq!(
        user().0,
        [
            "fig.png",
            "paper.pdf",
            "huge.png",
            "Pasted image",
            "notes.txt"
        ]
    );
    common::pump_until(TIMEOUT, || user().1.len() == 3);
    assert!(user().1.iter().all(|t| t.width().max(t.height()) <= 160));
    f.frames(2);
    f.end(StopReason::EndTurn);
    // `LEMMA_TEST_SNAPSHOTS=dir` saves the pane with previews and chips as `f-agent-attach.png`.
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS").map(PathBuf::from) {
        panel.attach_files(vec![file("fig.png"), file("paper.pdf"), file("notes.txt")]);
        common::pump_until(TIMEOUT, || names(&panel).len() == 3);
        type_into(&view, "What does the figure show?");
        panel.flush();
        f.frames(3);
        panel.scroll_to_end();
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("f-agent-attach.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &png)).unwrap();
        println!("    snapshot {}", png.display());
        view.buffer().set_text("");
        for _ in 0..3 {
            panel.remove_attachment(0);
        }
    }

    // A message of attachments only.
    panel.attach_files(vec![file("notes.txt")]);
    common::pump_until(TIMEOUT, || {
        !panel.is_attaching() && names(&panel).len() == 1
    });
    assert_eq!(text(&view), "");
    panel.send();
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, _) = prompts(&f.take_commands()).remove(0);
    assert!(
        matches!(blocks.as_slice(), [PromptBlock::Resource { text, .. }] if text == "Some notes.\n"),
        "{blocks:#?}"
    );
}

/// Writes a sparse file of `len` bytes that starts with `head`.
fn sparse_file(path: &std::path::Path, head: &[u8], len: u64) {
    std::fs::write(path, head).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

/// Files too large to include: Claude Code (and Pi) get large PDFs and text files as links
/// (the files are never read whole), the API backend refuses them; an image file over the
/// image limit is refused by both.
fn large_attachments() {
    let files = [("main.tex", MAIN), ("notes.txt", "Some notes.\n")];
    let write_large = |dir: &std::path::Path| {
        let line = "Line of a long build log, plain UTF-8 text: é ü ∀.\n";
        std::fs::write(dir.join("build.log"), line.repeat(12_000)).unwrap();
        sparse_file(&dir.join("thesis.pdf"), b"%PDF-1.7\n", 300 << 20);
        sparse_file(&dir.join("scan.png"), b"\x89PNG\r\n\x1a\n", 100 << 20);
        sparse_file(&dir.join("disk.img"), b"\0\0\0\x01", 4 << 20);
    };
    let names = |panel: &panel::Panel| -> Vec<String> {
        panel.attachments().into_iter().map(|(n, _)| n).collect()
    };

    let f = fixture("attach-large", &files, Hook::Swallow);
    write_large(&f.dir);
    let panel = f.panel();
    let file = |name: &str| gio::File::for_path(f.dir.join(name));
    panel.attach_files(vec![
        file("build.log"),
        file("thesis.pdf"),
        file("scan.png"),
        file("disk.img"),
    ]);
    common::pump_until(TIMEOUT, || !panel.is_attaching());
    assert_eq!(names(&panel), ["build.log", "thesis.pdf"]);
    for (name, detail) in panel.attachments() {
        assert!(detail.contains("sent as a link"), "{name}: {detail}");
    }
    panel.prompt_view().buffer().set_text("Summarize these");
    panel.send();
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, _) = prompts(&f.take_commands()).remove(0);
    let links: Vec<String> = blocks
        .iter()
        .filter_map(|b| match b {
            PromptBlock::Document { uri, mime, data } => {
                assert!(data.is_empty(), "{uri} was read");
                // `.log` is `text/x-log` or `text/plain`, after the system's MIME database.
                let kind = if mime.starts_with("text/") {
                    "text"
                } else {
                    mime
                };
                Some(format!("{} {kind}", PromptBlock::file_name(uri)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(links, ["build.log text", "thesis.pdf application/pdf"]);
    drop(f);

    // The API backend cannot open files: only what fits in the request is attached.
    let f = fixture_with("attach-large-api", &files, Hook::Swallow, |c| {
        c.agent.backend = AgentBackend::Api;
    });
    write_large(&f.dir);
    let panel = f.panel();
    let file = |name: &str| gio::File::for_path(f.dir.join(name));
    panel.attach_files(vec![
        file("build.log"),
        file("thesis.pdf"),
        file("notes.txt"),
    ]);
    common::pump_until(TIMEOUT, || !panel.is_attaching());
    assert_eq!(names(&panel), ["notes.txt"]);
}

fn selection_chip() {
    let f = fixture("chip", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    f.show_panel();
    let panel = f.panel();
    assert_eq!(panel.selection_chip(), None);
    select(&editor, (2, 0), (4, 3));
    assert_eq!(
        panel.selection_chip().as_deref(),
        Some("Selection · main.tex, lines 3–5")
    );
    select(&editor, (2, 0), (3, 0));
    assert_eq!(
        panel.selection_chip().as_deref(),
        Some("Selection · main.tex, line 3")
    );
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.start_iter());
    assert_eq!(panel.selection_chip(), None);
}

// ----- tool cards -----

fn tool_cards() {
    let f = fixture(
        "tools",
        &[("main.tex", MAIN), ("other.tex", "alpha\nbeta\n")],
        Hook::Swallow,
    );
    let editor = f.open("main.tex");
    f.show_panel();
    let panel = f.panel();
    f.begin("edit please", PromptOrigin::Panel);
    f.event(AgentEvent::ToolCall {
        id: "e1".into(),
        title: "mcp__lemma__edit".into(),
        kind: "other".into(),
        status: ToolStatus::Pending,
    });
    f.edit(
        1,
        "main.tex",
        "A second paragraph",
        "A better second paragraph",
    );
    f.event(AgentEvent::ToolUpdate {
        id: "e1".into(),
        title: None,
        status: Some(ToolStatus::Completed),
    });
    // An edit whose report comes after the edit itself still finds its hunk.
    f.edit(2, "other.tex", "beta", "gamma");
    f.event(AgentEvent::ToolCall {
        id: "e2".into(),
        title: "mcp__lemma__edit".into(),
        kind: "other".into(),
        status: ToolStatus::InProgress,
    });
    f.event(AgentEvent::ToolUpdate {
        id: "e2".into(),
        title: None,
        status: Some(ToolStatus::Completed),
    });
    f.event(AgentEvent::ToolCall {
        id: "c1".into(),
        title: "mcp__lemma__compile".into(),
        kind: "other".into(),
        status: ToolStatus::Pending,
    });
    f.event(AgentEvent::ToolUpdate {
        id: "c1".into(),
        title: None,
        status: Some(ToolStatus::Failed),
    });
    f.end(StopReason::EndTurn);
    let cards: Vec<(String, ToolStatus, Option<String>)> = panel
        .entries()
        .into_iter()
        .filter_map(|e| match e {
            Entry::Tool(card) => Some((card.id, card.status, card.link.map(|l| l.label))),
            _ => None,
        })
        .collect();
    assert_eq!(
        cards,
        [
            (
                "e1".into(),
                ToolStatus::Completed,
                Some("main.tex:5".into())
            ),
            (
                "e2".into(),
                ToolStatus::Completed,
                Some("other.tex:2".into())
            ),
            ("c1".into(), ToolStatus::Failed, None),
        ]
    );
    // The turn's summary counts what is left to review.
    assert!(
        panel
            .entries()
            .contains(&Entry::Summary { files: 2, hunks: 2 })
    );
    // Clicking a card shows its hunk.
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.start_iter());
    panel.activate_tool("e1");
    common::pump_until(TIMEOUT, || editor.cursor_position().0 == 5);
    panel.activate_tool("e2");
    common::pump_until(TIMEOUT, || {
        f.state.active_editor().is_some_and(|e| {
            e.document().path() == f.path("other.tex") && e.cursor_position().0 == 2
        })
    });
    // The compile card shows the Problems page.
    f.state
        .sidebar
        .stack
        .set_visible_child_name(lemma_app::window::PAGE_CHANGES);
    panel.activate_tool("c1");
    assert_eq!(
        f.state.sidebar.stack.visible_child_name().as_deref(),
        Some(lemma_app::window::PAGE_PROBLEMS)
    );
    panel.flush();
    f.frames(2);
    let rows = panel.row_texts().join("\n---\n");
    assert!(rows.contains("Edit\nmain.tex:5"), "{rows}");
    assert!(rows.contains("Compile"), "{rows}");
    assert!(rows.contains("2 changes in 2 files to review"), "{rows}");
    // The summary follows the review by itself (no flush): once everything is decided it says
    // so, without a Review button.
    lemma_app::review_ui::decide_everything(&f.state, lemma_app::review_ui::Decision::Accept);
    common::pump_until(TIMEOUT, || {
        panel
            .entries()
            .contains(&Entry::Summary { files: 0, hunks: 0 })
    });
    f.frames(2);
    let rows = panel.row_texts().join("\n---\n");
    assert!(rows.contains("No changes left to review"), "{rows}");
    assert!(!rows.contains("\nReview"), "{rows}");
}

/// The CLI's built-in tools have names of their own; its `ToolSearch` (loading the deferred
/// MCP tools at a turn's start) is a subdued row, not a card.
fn cli_tool_rows() {
    let f = fixture("cli-tools", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("compile it", PromptOrigin::Panel);
    let call = |id: &str, title: &str, kind: &str, status: ToolStatus| {
        f.event(AgentEvent::ToolCall {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            status,
        });
    };
    call("ts", "ToolSearch", "other", ToolStatus::Pending);
    call("b1", "Bash", "execute", ToolStatus::InProgress);
    call("w1", "WebFetch", "fetch", ToolStatus::Completed);
    call(
        "m1",
        "mcp__lemma__get_diagnostics",
        "other",
        ToolStatus::Completed,
    );
    panel.flush();
    f.frames(2);
    let root = panel.widget();
    let named = |name: &str| {
        descendants(&root)
            .into_iter()
            .find(|w| w.widget_name() == name)
            .unwrap_or_else(|| panic!("no widget {name}"))
    };
    let quiet = named("tool-ts");
    assert!(quiet.has_css_class("lemma-tool-quiet"));
    assert!(!quiet.has_css_class("lemma-tool-card"));
    let rows = panel.row_texts().join("\n---\n");
    for expected in [
        "Loading tools…",
        "Run a command",
        "Fetch a web page",
        "Read diagnostics",
    ] {
        assert!(rows.contains(expected), "{expected:?} missing in {rows}");
    }
    assert!(
        !rows.contains("ToolSearch") && !rows.contains("mcp__"),
        "{rows}"
    );
    assert!(named("tool-b1").has_css_class("lemma-tool-card"));
    f.event(AgentEvent::ToolUpdate {
        id: "ts".into(),
        title: None,
        status: Some(ToolStatus::Completed),
    });
    panel.flush();
    f.frames(1);
    assert!(panel.row_texts().join("\n").contains("Loaded tools"));
    f.end(StopReason::EndTurn);
}

/// A U+0000 in what the agent sends (a quoted line of a corrupted file) never reaches GLib,
/// which would abort a debug build and cut the text in a release one.
fn nul_text() {
    let f = fixture("nul", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    f.begin("what is at offset 12?", PromptOrigin::Panel);
    f.event(AgentEvent::ThoughtChunk("a \0 in the file".into()));
    f.event(AgentEvent::MessageChunk(
        "The byte is \0 here, and `x\0y`:\n\n```\nline\0\n```\n".into(),
    ));
    f.event(AgentEvent::ToolCall {
        id: "t\0".into(),
        title: "Read \0 file".into(),
        kind: "read".into(),
        status: ToolStatus::Completed,
    });
    f.event(AgentEvent::Permission {
        request: 1,
        title: "rm \0".into(),
        options: vec![perm("reject", "No \0", "reject_once")],
    });
    f.event(AgentEvent::Notice("notice \0".into()));
    f.event(AgentEvent::Error("error \0".into()));
    panel.flush();
    f.frames(2);
    let rows = panel.row_texts().join("\n");
    assert!(rows.contains("The byte is \u{FFFD} here"), "{rows}");
    assert!(rows.contains("line\u{FFFD}"), "{rows}");
    assert!(rows.contains("Read \u{FFFD} file"), "{rows}");
    f.end(StopReason::EndTurn);
}

/// Narrow window, the user writes in the editor while a turn runs: a permission request leaves
/// the keyboard focus in the editor (the overlay pane would take it, and the next `y` typed
/// there would answer); Ctrl+L (what the toast's Answer button does) goes to the card, and y
/// answers it.
fn permission_while_typing() {
    let f = fixture("perm-typing", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    let panel = f.panel();
    assert!(f.state.agent.split.is_collapsed(), "a narrow window");
    f.begin("compile and clean up", PromptOrigin::Panel);
    editor.view().grab_focus();
    common::pump_for(Duration::from_millis(50));
    f.event(bash_permission(7));
    common::pump_for(Duration::from_millis(200));
    let view: gtk::Widget = editor.view().clone().upcast();
    let focus = || gtk::prelude::RootExt::focus(&f.state.window);
    assert_eq!(focus(), Some(view.clone()), "the editor keeps the focus");
    assert!(!f.state.agent.split.shows_sidebar());
    // Ctrl+L: the pane opens with the focus on the card (not one of its buttons).
    WidgetExt::activate_action(&f.state.window, "win.agent-focus", None).unwrap();
    common::pump_until(TIMEOUT, || {
        focus().is_some_and(|w| w.widget_name() == "permission-7" && !w.is::<gtk::Button>())
    });
    assert!(f.state.agent.split.shows_sidebar());
    // Asked for: the keys answer at once.
    assert_eq!(
        panel.key_pressed(Key::y, ModifierType::empty()),
        glib::Propagation::Stop
    );
    assert!(f.commands().contains(&PanelCommand::Permission {
        request: 7,
        option_id: Some("allow-once".into())
    }));
    // The focus stays in the pane (on the prompt box), so the overlay stays open.
    common::pump_for(Duration::from_millis(100));
    assert!(focus().is_some_and(|w| w.is_ancestor(&panel.widget())));
    assert!(f.state.agent.split.shows_sidebar());
    f.end(StopReason::EndTurn);
}

/// A wide window shows the pane beside the editor: a request shows it there, and the keyboard
/// focus stays in the editor.
fn permission_while_typing_wide() {
    let f = fixture("perm-typing-wide", &[("main.tex", MAIN)], Hook::Swallow);
    // Broadway's screen is 1024 px wide, below the breakpoint: take the pane out of overlay
    // mode as a wide window does (the breakpoint applies again only when the width changes).
    f.state.agent.split.set_collapsed(false);
    common::pump_for(Duration::from_millis(50));
    assert!(!f.state.agent.split.is_collapsed());
    let editor = f.open("main.tex");
    f.begin("compile and clean up", PromptOrigin::Panel);
    editor.view().grab_focus();
    common::pump_for(Duration::from_millis(50));
    f.event(bash_permission(8));
    common::pump_for(Duration::from_millis(200));
    f.frames(2);
    assert!(
        f.state.agent.split.shows_sidebar(),
        "the request shows the pane"
    );
    let view: gtk::Widget = editor.view().clone().upcast();
    assert_eq!(
        gtk::prelude::RootExt::focus(&f.state.window),
        Some(view),
        "the editor keeps the focus"
    );
    f.end(StopReason::EndTurn);
}

// ----- inline edit -----

fn inline_edit() {
    let f = fixture("inline", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    // "First line of text." without its newline: bytes 41..60.
    select(&editor, (2, 0), (2, 19));
    WidgetExt::activate_action(&f.state.window, "win.agent-inline-edit", None).unwrap();
    let popover = inline::popover(&f.state).expect("the popover");
    common::pump_until(TIMEOUT, || popover.is_visible());
    let buffer = editor.buffer();
    let tag = buffer
        .tag_table()
        .lookup(inline::REGION_TAG)
        .expect("the region tag");
    assert!(buffer.iter_at_line_offset(2, 5).unwrap().has_tag(&tag));
    assert!(!buffer.iter_at_line_offset(4, 5).unwrap().has_tag(&tag));
    let entry = inline::entry(&f.state).unwrap();
    entry.set_text("make it shorter");
    entry.emit_activate();
    let sent = prompts(&f.commands());
    assert_eq!(sent.len(), 1);
    let (blocks, origin) = &sent[0];
    let start = MAIN.find("First line").unwrap();
    assert_eq!(
        *origin,
        PromptOrigin::Inline {
            path: f.path("main.tex"),
            range: start..start + "First line of text.".len()
        }
    );
    let text = text_of(blocks);
    assert!(
        text.starts_with("make it shorter\n\n(Inline edit of line 3 of main.tex"),
        "{text}"
    );
    assert!(text.contains("Change only this region") && text.contains("mcp__lemma__edit"));
    let PromptBlock::Resource { uri, text, mime } = &blocks[1] else {
        panic!("{blocks:?}")
    };
    assert!(uri.ends_with("/main.tex#L3-L3"), "{uri}");
    assert_eq!(text, "First line of text.");
    assert_eq!(mime, "text/x-tex");
    assert_eq!(
        inline::status(&f.state).as_deref(),
        Some("Claude is editing…")
    );
    assert!(!entry.is_editable());

    // The emulated turn: progress, then the popover closes and the tint goes.
    let turn = f.begin(&text_of(blocks), origin.clone());
    assert!(turn > 0);
    f.event(AgentEvent::ToolCall {
        id: "t".into(),
        title: "mcp__lemma__edit".into(),
        kind: "other".into(),
        status: ToolStatus::Pending,
    });
    assert_eq!(inline::status(&f.state).as_deref(), Some("Edit…"));
    f.end(StopReason::EndTurn);
    common::pump_until(TIMEOUT, || !popover.is_visible());
    assert!(inline::popover(&f.state).is_none());
    assert!(!buffer.iter_at_line_offset(2, 5).unwrap().has_tag(&tag));
    common::pump_until(TIMEOUT, || popover.parent().is_none());
    // The transcript shows the instruction with where it applies.
    let user = panel_user_heading(&f);
    assert_eq!(user, "Inline edit · main.tex:3");

    // Without a selection: the paragraph at the cursor (two lines).
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line_offset(5, 3).unwrap());
    inline::open(&f.state, &editor);
    inline::submit(&f.state, "polish");
    let (blocks, origin) = prompts(&f.commands()).pop().unwrap();
    let PromptBlock::Resource { text, uri, .. } = &blocks[1] else {
        panic!()
    };
    assert_eq!(text, "A second paragraph\nthat spans two lines.");
    assert!(uri.ends_with("#L5-L6"));
    let PromptOrigin::Inline { range, .. } = origin else {
        panic!()
    };
    assert_eq!(&MAIN[range], "A second paragraph\nthat spans two lines.");
    inline::cancel(&f.state);
    assert!(inline::popover(&f.state).is_none());
}

fn panel_user_heading(f: &Fixture) -> String {
    f.panel()
        .entries()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            Entry::User { heading, .. } => Some(heading),
            _ => None,
        })
        .unwrap()
}

fn inline_escape() {
    let f = fixture("inline-esc", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    select(&editor, (2, 0), (2, 5));
    inline::open(&f.state, &editor);
    // Esc before sending only closes.
    inline::cancel(&f.state);
    assert!(inline::popover(&f.state).is_none());
    assert!(!f.commands().contains(&PanelCommand::Cancel));
    // Esc while the turn runs cancels it.
    inline::open(&f.state, &editor);
    inline::submit(&f.state, "do it");
    let (blocks, origin) = prompts(&f.commands()).pop().unwrap();
    f.begin(&text_of(&blocks), origin);
    inline::cancel(&f.state);
    assert!(f.commands().contains(&PanelCommand::Cancel));
    assert!(inline::popover(&f.state).is_none());
    f.end(StopReason::Cancelled);
    // While another turn runs, Ctrl+K does not open.
    f.begin("other", PromptOrigin::Panel);
    inline::open(&f.state, &editor);
    assert!(inline::popover(&f.state).is_none());
    f.end(StopReason::EndTurn);
    // An error the agent answers the prompt with (an `Error`, then the turn's end) keeps the
    // popover open with the message, ready for another try.
    inline::open(&f.state, &editor);
    inline::submit(&f.state, "again");
    f.event(AgentEvent::Error("the agent crashed".into()));
    assert_eq!(
        inline::status(&f.state).as_deref(),
        Some("the agent crashed")
    );
    // An error alone does not end the edit (it may be about something else): Esc still
    // cancels the turn.
    assert!(!inline::entry(&f.state).unwrap().is_editable());
    f.event(AgentEvent::TurnEnded(StopReason::EndTurn));
    assert_eq!(
        inline::status(&f.state).as_deref(),
        Some("the agent crashed")
    );
    assert!(inline::popover(&f.state).is_some());
    assert!(inline::entry(&f.state).unwrap().is_editable());
    inline::cancel(&f.state);
    // Closing the tab while the popover is open takes it along (no GTK warnings).
    inline::open(&f.state, &editor);
    let popover = inline::popover(&f.state).unwrap();
    f.state.close_editor(&editor, true);
    common::pump_until(TIMEOUT, || f.state.editors().is_empty());
    common::pump_until(TIMEOUT, || popover.parent().is_none());
    assert!(inline::popover(&f.state).is_none());
}

/// WP3.10 (reported by WP3.2): closing a tab while its Ctrl+K popover is open used to pop the
/// autohide popover down as the view left the window; GTK then queued a draw on the view after it
/// was gone, a `gtk_widget_queue_draw: assertion 'GTK_IS_WIDGET (widget)' failed` critical when
/// the window was destroyed. The harness fails the test on that critical.
///
/// Also: an earlier session closed with Esc unparents its popover from an idle callback; when
/// the tab closes before that ran, that popover goes too (a GtkTextView disposed with a popover
/// still parented loops forever printing "GtkPopover is not a child of LemmaTexView").
fn inline_closed_tab() {
    let f = fixture("inline-closed-tab", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    select(&editor, (2, 0), (2, 5));
    // Esc, and no main-loop turn before the next Ctrl+K.
    inline::open(&f.state, &editor);
    inline::cancel(&f.state);
    inline::open(&f.state, &editor);
    let popover = inline::popover(&f.state).unwrap();
    common::pump_until(TIMEOUT, || popover.is_visible());
    inline::cancel(&f.state);
    inline::open(&f.state, &editor);
    let popover = inline::popover(&f.state).unwrap();
    f.state.close_editor(&editor, true);
    assert!(f.state.editors().is_empty());
    assert!(popover.parent().is_none());
    // Checked before the last reference to the view goes (its dispose would loop).
    let mut child = editor.view().first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        // (GtkSourceView's own hover assistant is a popover it removes itself.)
        assert!(
            !widget.has_css_class("lemma-inline-edit"),
            "an inline-edit popover is still on the closed tab's view"
        );
    }
    drop(editor);
    assert!(inline::popover(&f.state).is_none());
    // Shown and then closed with its tab: no critical when the window goes.
    let editor = f.open("main.tex");
    inline::open(&f.state, &editor);
    let popover = inline::popover(&f.state).unwrap();
    common::pump_until(TIMEOUT, || popover.is_visible());
    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_until(TIMEOUT, || f.state.editors().is_empty());
    common::pump_until(TIMEOUT, || popover.parent().is_none());
    common::pump_for(Duration::from_millis(100));
    f.state.window.destroy();
    common::pump_for(Duration::from_millis(300));
}

// ----- quick actions -----

fn quick_actions() {
    let f = fixture("quick", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    let window = &f.state.window;
    let actions = quick::actions(&f.state);
    assert_eq!(
        actions,
        [
            "quick-explain",
            "quick-proofread",
            "quick-make-this-proof-more-detailed",
            "quick-convert-to-align",
            "quick-translate-to-english"
        ],
        "every template but the diagnostic one"
    );
    let enabled = |name: &str| window.lookup_action(name).unwrap().is_enabled();
    assert!(!enabled("quick-proofread"), "needs a selection");
    select(&editor, (4, 0), (5, 21));
    assert!(enabled("quick-proofread"));
    WidgetExt::activate_action(window, "win.quick-proofread", None).unwrap();
    let (blocks, origin) = prompts(&f.commands()).pop().unwrap();
    assert_eq!(origin, PromptOrigin::QuickAction("Proofread".into()));
    let text = text_of(&blocks);
    assert!(
        text.starts_with("Proofread the following passage from main.tex (from line 5)"),
        "{text}"
    );
    assert!(
        text.ends_with("A second paragraph\nthat spans two lines.\n"),
        "{text}"
    );
    assert!(
        f.state.agent.split.shows_sidebar(),
        "the pane shows the answer"
    );
    // By slug, as `:Agent explain` does; unknown names fail.
    quick::run(&f.state, "explain").unwrap();
    assert_eq!(
        prompts(&f.commands()).pop().unwrap().1,
        PromptOrigin::QuickAction("Explain".into())
    );
    assert!(quick::run(&f.state, "nonsense").is_err());
    // `:Agent <text>` sends text; `:Agent` alone focuses the prompt box.
    quick::agent_command(&f.state, "  hello there ").unwrap();
    assert_eq!(
        prompts(&f.commands()).pop().unwrap(),
        (
            vec![PromptBlock::Text("hello there".into())],
            PromptOrigin::Panel
        )
    );
    quick::agent_command(&f.state, "").unwrap();
    // Without a selection the selection templates refuse.
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.start_iter());
    assert!(!enabled("quick-explain"));
    assert!(
        quick::run(&f.state, "Explain")
            .unwrap_err()
            .contains("selection")
    );
    // The editor's context menu has a Claude submenu with Ctrl+K and the actions.
    let menu = &f.state.editor_menu;
    assert_eq!(menu.n_items(), 1);
    let claude = menu
        .item_link(0, gio::MENU_LINK_SUBMENU)
        .and_downcast::<gio::Menu>()
        .expect("a submenu");
    let edit = claude
        .item_link(0, gio::MENU_LINK_SECTION)
        .and_downcast::<gio::MenuModel>()
        .unwrap();
    assert_eq!(
        edit.item_attribute_value(0, gio::MENU_ATTRIBUTE_ACTION, None)
            .and_then(|v| v.get::<String>())
            .as_deref(),
        Some("win.agent-inline-edit")
    );
    let items = claude
        .item_link(1, gio::MENU_LINK_SECTION)
        .and_downcast::<gio::MenuModel>()
        .unwrap();
    assert_eq!(items.n_items(), 5);
    // The panel's Quick Actions menu has the same items.
    assert_eq!(f.panel().quick_menu().n_items(), 1);
    // `:Agent fix-this-error` without a build error says so.
    assert!(quick::run(&f.state, "fix-this-error").is_err());
}

fn quick_reload() {
    let f = fixture("quick-reload", &[("main.tex", MAIN)], Hook::Swallow);
    let editor = f.open("main.tex");
    let user = parse_prompts(
        "[[prompt]]\nname = \"Explain\"\nprompt = \"\"\n\
         [[prompt]]\nname = \"Shorten\"\nkey = \"<Control><Alt>h\"\nprompt = \"Shorten {selection} ({file}:{line})\"\n\
         [[prompt]]\nname = \"Summarize file\"\ncontext = \"file\"\nprompt = \"Summarize {file}\"\n",
    )
    .unwrap();
    let merged = lemma_config::merge_prompts(lemma_config::default_prompts(), user);
    f.state.set_prompts(Rc::new(merged));
    let actions = quick::actions(&f.state);
    assert!(!actions.contains(&"quick-explain".to_owned()));
    assert!(
        f.state.window.lookup_action("quick-explain").is_none(),
        "removed"
    );
    assert!(actions.contains(&"quick-shorten".to_owned()));
    assert_eq!(
        f.state.app.accels_for_action("win.quick-shorten"),
        ["<Control><Alt>h"]
    );
    // A template that needs no selection works on the paragraph at the cursor.
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line_offset(2, 2).unwrap());
    WidgetExt::activate_action(&f.state.window, "win.quick-shorten", None).unwrap();
    let text = text_of(&prompts(&f.commands()).pop().unwrap().0);
    assert_eq!(
        text,
        "Shorten \\documentclass{article}\n\\begin{document}\nFirst line of text. (main.tex:1)"
    );
    WidgetExt::activate_action(&f.state.window, "win.quick-summarize-file", None).unwrap();
    assert_eq!(
        prompts(&f.commands()).pop().unwrap(),
        (
            vec![PromptBlock::Text("Summarize main.tex".into())],
            PromptOrigin::QuickAction("Summarize file".into())
        )
    );
    // Back to the defaults: the accelerator goes with the action.
    f.state
        .set_prompts(Rc::new(lemma_config::default_prompts()));
    assert!(f.state.window.lookup_action("quick-shorten").is_none());
    assert!(
        f.state
            .app
            .accels_for_action("win.quick-shorten")
            .is_empty()
    );
}

fn ask_agent_to_fix() {
    let lines: String = (1..=60).map(|i| format!("line {i}\n")).collect();
    let f = fixture(
        "fix",
        &[("main.tex", &lines), ("ch/closed.tex", "a\nb\n\\foo\n")],
        Hook::Swallow,
    );
    let build = f.state.build.get().unwrap();
    let main = f.path("main.tex");
    let problem = Problem {
        path: main.clone(),
        line: Some(30),
        severity: Severity::Error,
        message: "Undefined control sequence.".into(),
        context: Some("l.30 \\foo".into()),
        source: DiagSource::Tex,
    };
    let closed = Problem {
        path: f.path("ch/closed.tex"),
        line: Some(3),
        ..problem.clone()
    };
    build
        .diagnostics()
        .set(vec![problem.clone(), closed], &main);
    // The open document's live text is quoted.
    let editor = f.open("main.tex");
    editor
        .buffer()
        .insert(&mut editor.buffer().start_iter(), "inserted\n");
    build.diagnostics().ask_agent(0);
    common::pump_until(TIMEOUT, || !prompts(&f.commands()).is_empty());
    let (blocks, origin) = prompts(&f.commands()).pop().unwrap();
    assert_eq!(origin, PromptOrigin::FixDiagnostic);
    let text = text_of(&blocks);
    assert!(
        text.starts_with("Fix this LaTeX build error in main.tex at line 31:\n\nUndefined control sequence.\nl.30 \\foo"),
        "{text}"
    );
    assert!(text.contains(">   31  line 30\n"), "{text}");
    assert!(
        text.contains("    11  line 10\n") && text.contains("    51  line 50\n"),
        "{text}"
    );
    assert!(!text.contains("    10  line 9\n") && !text.contains("    52  line 51\n"));
    assert!(f.state.agent.split.shows_sidebar());
    // A closed file is read from disk.
    build.diagnostics().ask_agent(1);
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 2);
    let text = text_of(&prompts(&f.commands()).pop().unwrap().0);
    assert!(text.contains("in ch/closed.tex at line 3"), "{text}");
    assert!(text.contains(">    3  \\foo\n"), "{text}");
    // `:Agent fix-this-error` takes the problem nearest the cursor in the active file.
    quick::run(&f.state, "fix-this-error").unwrap();
    common::pump_until(TIMEOUT, || prompts(&f.commands()).len() == 3);
    let text = text_of(&prompts(&f.commands()).pop().unwrap().0);
    assert!(text.contains("main.tex at line 31"), "{text}");
    // The transcript shows the prompt, collapsed, under its origin.
    f.begin(&text, PromptOrigin::FixDiagnostic);
    assert_eq!(panel_user_heading(&f), "Ask agent to fix");
    f.end(StopReason::EndTurn);
}

// ----- lifecycle and set-up -----

fn lazy_start() {
    let f = fixture("lazy", &[("main.tex", MAIN)], Hook::Forward);
    let panel = f.panel();
    assert!(
        f.commands().is_empty(),
        "nothing starts while the pane is hidden"
    );
    assert_eq!(panel.setup(), None);
    // Showing the pane starts the agent. This test's agent cannot start (no bridge, or no
    // toolchain in the isolated environment).
    install::clear_last_result();
    f.show_panel();
    assert_eq!(f.commands(), [PanelCommand::Start]);
    common::pump_until(TIMEOUT, || {
        panel
            .setup()
            .is_some_and(|(title, _)| title == "Claude could not start")
    });
    assert!(panel.retry_button().is_visible());
    assert!(f.agent().state() != AgentState::Idle);
    panel.retry_button().emit_clicked();
    assert_eq!(f.commands(), [PanelCommand::Start, PanelCommand::Start]);
    common::pump_until(TIMEOUT, || {
        panel
            .setup()
            .is_some_and(|(title, _)| title == "Claude could not start")
    });
    f.agent().stop();
}

fn stop_and_new_session() {
    let f = fixture("stop-agent", &[("main.tex", MAIN)], Hook::Swallow);
    f.show_panel();
    let panel = f.panel();
    assert_eq!(f.take_commands(), [PanelCommand::Start]);
    f.agent().set_state(AgentState::Idle);
    f.begin("hi", PromptOrigin::Panel);
    f.event(AgentEvent::MessageChunk("hello".into()));
    f.end(StopReason::EndTurn);
    panel.actions().activate_action("shutdown", None);
    assert_eq!(f.take_commands(), [PanelCommand::Stop]);
    f.agent().set_state(AgentState::Stopped);
    assert_eq!(
        panel.setup().map(|(t, _)| t).as_deref(),
        Some("Claude is stopped")
    );
    // Hiding and showing the pane does not start it again …
    f.state.agent.split.set_show_sidebar(false);
    f.state.agent.split.set_show_sidebar(true);
    assert!(f.commands().is_empty());
    // … a prompt or Start does.
    panel.actions().activate_action("start", None);
    assert_eq!(f.take_commands(), [PanelCommand::Start]);
    // New Session: a fresh transcript and a new start.
    assert!(!panel.entries().is_empty());
    panel.actions().activate_action("new-session", None);
    assert_eq!(
        f.take_commands(),
        [PanelCommand::NewSession, PanelCommand::Start]
    );
    assert!(panel.entries().is_empty());
    panel.actions().activate_action("clear", None);
}

// ----- the idle stop -----

/// How long the idle stop waits in these tests.
const IDLE: Duration = Duration::from_millis(150);

/// `ready_event` of session `session`, from an agent that can (or cannot) load sessions.
fn ready_of(session: &str, loadable: bool) -> AgentEvent {
    let AgentEvent::Ready {
        agent,
        modes,
        mode,
        options,
        ..
    } = ready_event()
    else {
        unreachable!()
    };
    AgentEvent::Ready {
        session: session.into(),
        agent,
        modes,
        mode,
        options,
        loadable,
    }
}

/// A session on a command channel (no process), starting.
fn channel_session(f: &Fixture) -> async_channel::Receiver<AgentCmd> {
    let (tx, rx) = async_channel::unbounded();
    bridge::attach_channel(&f.agent(), tx);
    rx
}

/// A session on a command channel, `Ready` and idle.
fn idle_session(f: &Fixture, session: &str, loadable: bool) -> async_channel::Receiver<AgentCmd> {
    let rx = channel_session(f);
    bridge::handle_event(&f.agent(), ready_of(session, loadable));
    assert_eq!(f.agent().state(), AgentState::Idle);
    rx
}

fn notices(panel: &panel::Panel) -> Vec<String> {
    panel
        .entries()
        .into_iter()
        .filter_map(|e| match e {
            Entry::Notice(n) => Some(n),
            _ => None,
        })
        .collect()
}

fn idle_stop_waits() {
    let f = fixture("idle-stop", &[("main.tex", MAIN)], Hook::Forward);
    let panel = f.panel();
    let agent = f.agent();
    let bridge = agent.bridge();
    bridge.set_idle_stop(Some(IDLE));
    let idle_for = |n: u32| common::pump_for(IDLE * n);

    // The pane shown in the focused window holds the agent: the timer starts again. (Shown
    // while the agent starts, so that showing it starts nothing; and beside the editor, as in
    // a wide window, so that it stays shown wherever the focus goes.)
    f.state.agent.split.set_collapsed(false);
    let rx = channel_session(&f);
    f.show_panel();
    f.state.set_focus_override(Some(true));
    bridge::handle_event(&agent, ready_of("s1", true));
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(bridge.idle_stop_pending());
    idle_for(3);
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(bridge.idle_stop_pending());
    // In a window without the focus it does not: stopped. Nothing went to this session, so
    // there is nothing to resume, and the user did not stop it.
    f.state.set_focus_override(Some(false));
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    assert!(!bridge.has_session());
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|c| c == AgentCmd::Shutdown));
    assert_eq!(bridge.resumable_session(), None);
    assert_eq!(
        notices(&panel),
        [
            "Claude was stopped after 1 idle minute to free memory; it starts again with your next \
          message."
        ]
    );
    assert_eq!(panel.setup(), None, "no \"Claude is stopped\" card");
    f.state.agent.split.set_show_sidebar(false);

    // Text in the prompt box holds it too (the window keeps without the focus from here on,
    // wherever the permission card puts the pane).
    let rx = idle_session(&f, "s2", true);
    panel.prompt_view().buffer().set_text("a draft");
    idle_for(3);
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(bridge.idle_stop_pending());
    panel.prompt_view().buffer().set_text("");
    // A permission request ends the timer, its answer starts it again.
    bridge::handle_event(
        &agent,
        AgentEvent::Permission {
            request: 7,
            title: "Run ls".into(),
            options: vec![perm("allow", "Allow", "allow_once")],
        },
    );
    assert!(!bridge.idle_stop_pending());
    idle_for(2);
    assert_eq!(agent.state(), AgentState::Idle);
    agent.permission(7, Some("allow".into()));
    assert!(bridge.idle_stop_pending());
    // A turn ends it; its end starts it again.
    agent.prompt(vec![PromptBlock::Text("hi".into())], PromptOrigin::Panel);
    assert!(!bridge.idle_stop_pending());
    common::pump_until(TIMEOUT, || {
        std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, AgentCmd::Prompt(_)))
    });
    idle_for(3);
    assert!(agent.is_running());
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::EndTurn));
    assert!(bridge.idle_stop_pending());
    // Stopped once idle long enough; this session's conversation is resumed with the next start.
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    assert_eq!(bridge.resumable_session().as_deref(), Some("s2"));
    assert_eq!(
        notices(&panel).last().map(String::as_str),
        Some(
            "Claude was stopped after 1 idle minute to free memory; it resumes this \
             conversation with your next message."
        )
    );

    // An agent that cannot load sessions is stopped only before its first prompt.
    let rx = idle_session(&f, "s3", false);
    assert!(bridge.idle_stop_pending());
    agent.prompt(vec![PromptBlock::Text("hi".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || {
        std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, AgentCmd::Prompt(_)))
    });
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::EndTurn));
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(!bridge.idle_stop_pending());
    idle_for(3);
    assert!(bridge.has_session());
    // A session that continues the last one keeps its context usage in the ring; a new one
    // does not.
    f.event(AgentEvent::Usage {
        used: 1000,
        size: 200_000,
    });
    bridge::handle_event(&agent, ready_of("s3", false));
    assert_eq!(panel.usage(), Some((1000, 200_000)));
    bridge::handle_event(&agent, ready_of("s4", false));
    assert_eq!(panel.usage(), None);

    // A conversation whose backend config.toml replaced while it ran (the switch waits for the
    // session to end) is not stopped either: the next start would run the other backend.
    let rx = idle_session(&f, "s5", true);
    agent.prompt(vec![PromptBlock::Text("hi".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || {
        std::iter::from_fn(|| rx.try_recv().ok()).any(|c| matches!(c, AgentCmd::Prompt(_)))
    });
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::EndTurn));
    assert!(bridge.idle_stop_pending());
    let mut config = (*f.state.config.get()).clone();
    config.agent.backend = AgentBackend::Pi;
    f.state.set_config(Rc::new(config));
    idle_for(3);
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(bridge.has_session());
    assert!(!bridge.idle_stop_pending());
    bridge.set_idle_stop(None);
    f.state.set_focus_override(None);
    agent.stop();
}

fn idle_stop_never() {
    // `agent.idle_stop_min = 0`: never.
    let f = fixture_with("idle-never", &[("main.tex", MAIN)], Hook::Forward, |c| {
        c.agent.idle_stop_min = 0;
    });
    let _rx = idle_session(&f, "s1", true);
    assert!(!f.agent().bridge().idle_stop_pending());
    f.agent().stop();
    // Nor does a timer that started under another setting (a reload of config.toml).
    let agent = f.agent();
    agent.bridge().set_idle_stop(Some(IDLE));
    let _rx = idle_session(&f, "s2", true);
    assert!(agent.bridge().idle_stop_pending());
    agent.bridge().set_idle_stop(None);
    common::pump_for(IDLE * 3);
    assert_eq!(agent.state(), AgentState::Idle);
    assert!(!agent.bridge().idle_stop_pending());
    agent.stop();
    // The API backend runs in Lemma's process: nothing to free.
    let f = fixture_with("idle-api", &[("main.tex", MAIN)], Hook::Forward, |c| {
        c.agent.backend = AgentBackend::Api;
    });
    f.agent().bridge().set_idle_stop(Some(IDLE));
    let _rx = idle_session(&f, "api-1", false);
    assert!(!f.agent().bridge().idle_stop_pending());
    f.agent().stop();
}

const FAKE_NPM: &str = r#"echo "$@" > "$HOME/npm-args.txt"
prefix=""; pkg=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2;;
    @agentclientprotocol/*) pkg="$1"; shift;;
    *) shift;;
  esac
done
ver="${pkg##*@}"
echo "npm notice fake install of $pkg"
dir="$prefix/node_modules/@agentclientprotocol/claude-agent-acp"
mkdir -p "$dir/dist"
printf '{"name":"@agentclientprotocol/claude-agent-acp","version":"%s"}' "$ver" > "$dir/package.json"
echo "// fake" > "$dir/dist/index.js"
echo "added 42 packages in 1s""#;

fn adapter_dir() -> PathBuf {
    env().home.join(".local/share/lemma/acp")
}

fn install_success() {
    let _ = std::fs::remove_dir_all(adapter_dir());
    let _ = std::fs::remove_file(&env().config);
    let node = fake_program("node", "exit 0");
    let claude = fake_program("claude", "echo '2.1.296 (Claude Code)'");
    fake_program("npm", FAKE_NPM);
    let f = fixture("install", &[("main.tex", MAIN)], Hook::Swallow);
    let panel = f.panel();
    let paths = f.state.paths.clone().unwrap();
    // The bridge's start found no adapter.
    let error = install::detect(&f.state.config.agent, &paths).unwrap_err();
    assert_eq!(
        error,
        install::ToolchainError::Adapter {
            found: None,
            wanted: "0.89.1".into()
        }
    );
    // Nothing is stored in the config while the toolchain is incomplete.
    assert!(!env().config.exists());
    f.agent().set_state(AgentState::Failed(error.to_string()));
    let (title, message) = panel.setup().expect("the set-up card");
    assert_eq!(title, "Claude could not start");
    assert!(
        message.contains("0.89.1, which is not installed"),
        "{message}"
    );
    assert!(panel.install_button().is_visible());
    // The install runs only on the click.
    assert!(!env().home.join("npm-args.txt").exists());
    panel.install_button().emit_clicked();
    assert!(panel.installing());
    assert_eq!(
        panel.setup().map(|(t, _)| t).as_deref(),
        Some("Installing the Claude ACP adapter…")
    );
    common::pump_until(TIMEOUT, || !panel.installing());
    let log = panel.install_log();
    assert!(
        log.contains("npm notice fake install of @agentclientprotocol/claude-agent-acp@0.89.1"),
        "{log}"
    );
    assert!(
        log.contains("added 42 packages in 1s") && log.ends_with("Installed."),
        "{log}"
    );
    let args = std::fs::read_to_string(env().home.join("npm-args.txt")).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "i --prefix {} @agentclientprotocol/claude-agent-acp@0.89.1 --omit=optional \
             --no-audit --no-fund",
            adapter_dir().display()
        )
    );
    // Afterwards the agent starts again, and the toolchain is complete.
    assert_eq!(f.commands().last(), Some(&PanelCommand::Start));
    let toolchain = install::detect(&f.state.config.agent, &paths).unwrap();
    assert_eq!(
        toolchain.adapter_js,
        adapter_dir().join("node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js")
    );
    // node and claude were stored in the config on this first success.
    let stored = lemma_config::parse(&std::fs::read_to_string(&env().config).unwrap()).unwrap();
    assert_eq!((stored.agent.node, stored.agent.claude), (node, claude));
}

fn install_failure() {
    let _ = std::fs::remove_dir_all(adapter_dir());
    fake_program("node", "exit 0");
    fake_program("claude", "echo '2.1.296 (Claude Code)'");
    fake_program(
        "npm",
        "echo 'npm ERR! code ENOTFOUND' >&2\necho 'npm ERR! network' >&2\nexit 1",
    );
    let f = fixture("install-fail", &[("main.tex", MAIN)], Hook::Swallow);
    let panel = f.panel();
    let paths = f.state.paths.clone().unwrap();
    let error = install::detect(&f.state.config.agent, &paths).unwrap_err();
    f.agent().set_state(AgentState::Failed(error.to_string()));
    panel.install_button().emit_clicked();
    common::pump_until(TIMEOUT, || !panel.installing());
    let (title, message) = panel.setup().unwrap();
    assert_eq!(title, "The installation failed");
    assert!(message.starts_with("npm failed"), "{message}");
    assert!(panel.install_log().contains("npm ERR! code ENOTFOUND"));
    assert!(panel.install_button().is_visible(), "try again");
    assert!(!f.commands().contains(&PanelCommand::Start));
    // An npm that never finishes can be cancelled.
    fake_program("npm", "echo 'resolving…'\nexec sleep 30");
    panel.install_button().emit_clicked();
    common::pump_until(TIMEOUT, || panel.install_log().contains("resolving"));
    let started = Instant::now();
    descendants(&panel.widget())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .find(|b| b.label().as_deref() == Some("_Cancel") && b.is_visible())
        .expect("a Cancel button")
        .emit_clicked();
    common::pump_until(TIMEOUT, || !panel.installing());
    assert!(started.elapsed() < Duration::from_secs(10));
    let (title, message) = panel.setup().unwrap();
    assert_eq!(
        (title.as_str(), message.as_str()),
        ("The installation failed", "the installation was cancelled")
    );
    // npm missing next to node and on PATH.
    remove_program("npm");
    panel.install_button().emit_clicked();
    common::pump_until(TIMEOUT, || !panel.installing());
    assert!(panel.setup().unwrap().1.starts_with("npm was not found"));
}

fn diagnose_missing() {
    let _ = std::fs::remove_file(&env().config);
    remove_program("node");
    remove_program("claude");
    let f = fixture("diagnose", &[("main.tex", MAIN)], Hook::Swallow);
    let panel = f.panel();
    let paths = f.state.paths.clone().unwrap();
    let error = install::detect(&f.state.config.agent, &paths).unwrap_err();
    assert_eq!(error, install::ToolchainError::NodeNotFound);
    f.agent().set_state(AgentState::Failed(error.to_string()));
    let (_, message) = panel.setup().unwrap();
    assert!(message.starts_with("Node.js was not found"), "{message}");
    assert!(!panel.install_button().is_visible());
    assert!(panel.retry_button().is_visible());
    fake_program("node", "exit 0");
    let error = install::detect(&f.state.config.agent, &paths).unwrap_err();
    assert_eq!(error, install::ToolchainError::ClaudeNotFound);
    f.agent().set_state(AgentState::Failed(error.to_string()));
    assert!(
        panel
            .setup()
            .unwrap()
            .1
            .starts_with("Claude Code was not found")
    );
    // Another failure (the agent died during start-up) shows the bridge's message.
    install::clear_last_result();
    f.agent().set_state(AgentState::Failed(
        "claude exited with status 1: login required".into(),
    ));
    assert_eq!(
        panel.setup().unwrap().1,
        "claude exited with status 1: login required"
    );
}

fn update_hint() {
    fake_program("node", "exit 0");
    fake_program("claude", "echo '2.1.288 (Claude Code)'");
    // An installed adapter.
    let pkg = adapter_dir().join("node_modules/@agentclientprotocol/claude-agent-acp/package.json");
    std::fs::create_dir_all(pkg.parent().unwrap()).unwrap();
    std::fs::write(&pkg, r#"{"version":"0.89.1"}"#).unwrap();
    let f = fixture("hint", &[("main.tex", MAIN)], Hook::Swallow);
    let panel = f.panel();
    let paths = f.state.paths.clone().unwrap();
    let toolchain = install::detect(&f.state.config.agent, &paths).unwrap();
    assert!(toolchain.needs_claude_update());
    assert_eq!(panel.update_hint(), None);
    f.agent().set_state(AgentState::Starting);
    f.agent().set_state(AgentState::Idle);
    let hint = panel.update_hint().expect("the hint");
    assert!(
        hint.contains("2.1.288") && hint.contains("2.1.293") && hint.contains("claude update"),
        "{hint}"
    );
    // Up to date: no hint.
    fake_program("claude", "echo '2.1.296 (Claude Code)'");
    let g = fixture("hint-new", &[("main.tex", MAIN)], Hook::Swallow);
    install::detect(&g.state.config.agent, &paths).unwrap();
    g.agent().set_state(AgentState::Idle);
    assert_eq!(g.panel().update_hint(), None);
}

// ----- snapshot -----

fn snapshot() {
    let fixtures = common::fixtures().join("proj");
    let main = std::fs::read_to_string(fixtures.join("main.tex")).unwrap();
    let yoneda = std::fs::read_to_string(fixtures.join("chapters/yoneda.tex")).unwrap();
    let f = fixture(
        "snapshot",
        &[("main.tex", &main), ("chapters/yoneda.tex", &yoneda)],
        Hook::Swallow,
    );
    let editor = f.open("main.tex");
    f.show_panel();
    let panel = f.panel();
    f.event(ready_event());
    f.agent().set_state(AgentState::Idle);
    f.begin(
        "Polish the abstract and the introduction, then compile.",
        PromptOrigin::Panel,
    );
    f.event(AgentEvent::ThoughtChunk(
        "The abstract is a little terse.".into(),
    ));
    f.event(AgentEvent::Plan(vec![
        ("Polish the abstract".into(), "completed".into()),
        ("Tighten the introduction".into(), "in_progress".into()),
        ("Compile the document".into(), "pending".into()),
    ]));
    f.event(AgentEvent::MessageChunk(
        "I'll reword the abstract and keep the notation $\\cC(-, X)$ unchanged.".into(),
    ));
    f.event(AgentEvent::ToolCall {
        id: "e1".into(),
        title: "mcp__lemma__edit".into(),
        kind: "other".into(),
        status: ToolStatus::Pending,
    });
    f.edit(
        1,
        "main.tex",
        "These notes serve as a test document.",
        "These notes are a short test document for Lemma.",
    );
    f.event(AgentEvent::ToolUpdate {
        id: "e1".into(),
        title: None,
        status: Some(ToolStatus::Completed),
    });
    f.event(AgentEvent::ToolCall {
        id: "r1".into(),
        title: "mcp__lemma__read_file".into(),
        kind: "other".into(),
        status: ToolStatus::InProgress,
    });
    f.event(bash_permission(1));
    f.event(AgentEvent::Usage {
        used: 41_250,
        size: 200_000,
    });
    // Show the hunk in the editor.
    let line = editor
        .document()
        .review
        .borrow()
        .as_ref()
        .and_then(|r| r.hunks().first().map(|h| h.cur_lines.start))
        .expect("a hunk");
    lemma_app::review_ui::show_line(&f.state, editor.document(), line);
    panel.flush();
    f.frames(3);
    panel.scroll_to_end();
    f.frames(1);
    let dir = std::env::var_os("LEMMA_TEST_SNAPSHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| common::temp_dir("snapshots"));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("f-agent-panel.png");
    let (w, h) = block_on(testmode::snapshot_next_frame(&f.state.window, &png)).unwrap();
    println!("    snapshot {} ({w}x{h})", png.display());
    assert!(w > 600 && h > 400);
    assert!(panel.mode_picker().is_visible() && panel.model_picker().is_visible());
    // The end of the transcript is on screen.
    let rows = panel.row_texts().join("\n");
    for expected in ["Edit", "main.tex:", "latexmk -pdf main.tex", "Yes (y)"] {
        assert!(
            rows.contains(expected),
            "{expected:?} not on screen:\n{rows}"
        );
    }
    assert_eq!(
        kinds(&panel.entries()),
        [
            "user",
            "thought",
            "plan",
            "agent",
            "tool",
            "tool",
            "permission"
        ]
    );
}

// ----- end to end with the fake agent -----

/// Starts the real bridge with `scenario` (a file in `tests/fixtures/agent/`), building
/// `lemma-fake-agent` and `lemma-mcp` first when needed. Always `Some`: a missing piece fails
/// the test (it used to print SKIP while the M2 work packages were in progress).
fn start_fake_agent(f: &Fixture, scenario: &str) -> Option<()> {
    common::require_agent_binaries();
    let path = common::fixtures().join("agent").join(scenario);
    assert!(path.is_file(), "{} is missing", path.display());
    let agent = f.agent();
    agent.set_scenario(Some(path.canonicalize().unwrap()));
    let result = block_on(async {
        glib::future_with_timeout(Duration::from_secs(15), agent.ensure_started()).await
    });
    match result {
        Ok(Ok(())) => Some(()),
        Ok(Err(e)) => panic!("the bridge cannot start the fake agent: {e}"),
        Err(_) => {
            agent.stop();
            panic!("the fake agent did not start within 15 s");
        }
    }
}

fn proj_fixture(name: &str) -> Fixture {
    let src = common::fixtures().join("proj");
    let main = std::fs::read_to_string(src.join("main.tex")).unwrap();
    let yoneda = std::fs::read_to_string(src.join("chapters/yoneda.tex")).unwrap();
    let refs = std::fs::read_to_string(src.join("refs.bib")).unwrap();
    fixture(
        name,
        &[
            ("main.tex", &main),
            ("chapters/yoneda.tex", &yoneda),
            ("refs.bib", &refs),
        ],
        Hook::Forward,
    )
}

fn wait_turn_end(f: &Fixture, timeout: Duration) -> Option<StopReason> {
    let ended: Rc<RefCell<Option<StopReason>>> = Rc::default();
    let sink = ended.clone();
    let id = f.agent().connect_event(move |e| {
        if let AgentEvent::TurnEnded(r) = e {
            sink.replace(Some(*r));
        }
    });
    let deadline = Instant::now() + timeout;
    while ended.borrow().is_none() && Instant::now() < deadline {
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    f.agent().disconnect(id);
    ended.take()
}

fn e2e_basic() {
    let f = proj_fixture("e2e-basic");
    f.open("main.tex");
    if start_fake_agent(&f, "basic.json").is_none() {
        return;
    }
    f.show_panel();
    let panel = f.panel();
    panel.prompt_view().buffer().set_text("Polish the document");
    panel.send();
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    panel.flush();
    let entries = panel.entries();
    let k = kinds(&entries);
    for kind in ["user", "thought", "plan", "agent", "tool"] {
        assert!(k.contains(&kind), "{kind} missing: {k:?}");
    }
    let links: Vec<String> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Tool(c) => c.link.as_ref().map(|l| l.label.clone()),
            _ => None,
        })
        .collect();
    // Each edit card links to its own call's hunk, in the scenario's order.
    assert_eq!(
        links,
        ["main.tex:34", "main.tex:41", "chapters/yoneda.tex:2"],
        "{links:?}"
    );
    assert_eq!(panel.usage(), Some((41_250, 200_000)));
    f.agent().stop();
}

fn e2e_permission() {
    let f = proj_fixture("e2e-perm");
    if start_fake_agent(&f, "permission.json").is_none() {
        return;
    }
    f.show_panel();
    let panel = f.panel();
    for (prompt, key, state) in [
        ("once", Key::y, ModifierType::empty()),
        ("always", Key::Y, ModifierType::SHIFT_MASK),
        ("reject", Key::n, ModifierType::empty()),
    ] {
        panel
            .prompt_view()
            .buffer()
            .set_text(&format!("run it {prompt}"));
        panel.send();
        let pending = || {
            panel.entries().into_iter().find_map(|e| match e {
                Entry::Permission(c) if c.answer == PermissionAnswer::Pending => Some(c.request),
                _ => None,
            })
        };
        let deadline = Instant::now() + TIMEOUT;
        while pending().is_none() {
            assert!(Instant::now() < deadline, "no permission card for {prompt}");
            glib::MainContext::default().iteration(false);
        }
        let request = pending().unwrap();
        panel.flush();
        common::pump_until(TIMEOUT, || !panel.permission_buttons(request).is_empty());
        assert!(panel.permission_buttons(request)[0].grab_focus());
        assert_eq!(panel.key_pressed(key, state), glib::Propagation::Stop);
        // The fake agent fails the turn unless the expected option arrives.
        assert_eq!(
            wait_turn_end(&f, TIMEOUT),
            Some(StopReason::EndTurn),
            "{prompt}"
        );
        assert!(
            !panel.entries().iter().any(|e| matches!(e, Entry::Error(_))),
            "{:#?}",
            panel.entries()
        );
    }
    f.agent().stop();
}

fn e2e_pickers() {
    let f = proj_fixture("e2e-pickers");
    if start_fake_agent(&f, "basic.json").is_none() {
        return;
    }
    f.show_panel();
    let panel = f.panel();
    let (mode, model) = (panel.mode_picker(), panel.model_picker());
    common::pump_until(TIMEOUT, || mode.get_visible() && model.get_visible());
    let items = picker_items(&mode);
    assert_eq!(items.len(), 2, "only default and plan: {items:?}");
    let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
    let sink = events.clone();
    let id = f
        .agent()
        .connect_event(move |e| sink.borrow_mut().push(e.clone()));
    // Plan mode, confirmed by the agent.
    mode.set_selected(1);
    common::pump_until(TIMEOUT, || {
        events.borrow().iter().any(|e| match e {
            AgentEvent::ModeChanged(m) => m == "plan",
            AgentEvent::ConfigChanged(o) => o.iter().any(|o| o.id == "mode" && o.current == "plan"),
            _ => false,
        })
    });
    assert_eq!(mode.selected(), 1);
    // Another model.
    let target = (model.selected() + 1) % picker_items(&model).len() as u32;
    model.set_selected(target);
    common::pump_until(TIMEOUT, || {
        events.borrow().iter().any(|e| matches!(e, AgentEvent::ConfigChanged(o)
            if o.iter().any(|o| o.id == "model" && o.values.iter().position(|(v, _)| *v == o.current) == Some(target as usize))))
    });
    assert_eq!(model.selected(), target);
    // And back to the default mode.
    mode.set_selected(0);
    common::pump_until(TIMEOUT, || {
        events
            .borrow()
            .iter()
            .rev()
            .find_map(|e| match e {
                AgentEvent::ModeChanged(m) => Some(m.clone()),
                AgentEvent::ConfigChanged(o) => {
                    o.iter().find(|o| o.id == "mode").map(|o| o.current.clone())
                }
                _ => None,
            })
            .as_deref()
            == Some("default")
    });
    f.agent().disconnect(id);
    f.agent().stop();
}

fn e2e_cancel() {
    let f = proj_fixture("e2e-cancel");
    if start_fake_agent(&f, "cancel.json").is_none() {
        return;
    }
    f.show_panel();
    let panel = f.panel();
    panel
        .prompt_view()
        .buffer()
        .set_text("long task with a permission");
    panel.send();
    common::pump_until(TIMEOUT, || panel.stop_button().is_visible());
    common::pump_until(TIMEOUT, || {
        panel
            .entries()
            .iter()
            .any(|e| matches!(e, Entry::Permission(_)))
    });
    let started = Instant::now();
    panel.stop_button().emit_clicked();
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::Cancelled));
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert!(
        panel
            .entries()
            .iter()
            .any(|e| matches!(e, Entry::Permission(c) if c.answer == PermissionAnswer::Cancelled))
    );
    f.agent().stop();
}

fn e2e_inline() {
    let f = proj_fixture("e2e-inline");
    let editor = f.open("main.tex");
    if start_fake_agent(&f, "basic.json").is_none() {
        return;
    }
    let buffer = editor.buffer();
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    let at = text
        .find("These notes serve as a test document.")
        .expect("the abstract");
    let start = buffer.iter_at_offset(text[..at].chars().count() as i32);
    let mut end = start;
    end.forward_chars("These notes serve as a test document.".chars().count() as i32);
    buffer.select_range(&start, &end);
    let region_line = start.line() as u32;
    inline::open(&f.state, &editor);
    inline::submit(&f.state, "inline: make this sentence shorter");
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    common::pump_until(TIMEOUT, || inline::popover(&f.state).is_none());
    let doc = editor.document();
    let review = doc.review.borrow();
    let hunks = review.as_ref().expect("a review").hunks();
    assert!(!hunks.is_empty());
    for hunk in hunks {
        assert!(
            hunk.cur_lines.start <= region_line && hunk.cur_lines.end <= region_line + 1,
            "hunk {:?} outside line {region_line}",
            hunk.cur_lines
        );
    }
    drop(review);
    f.agent().stop();
}

fn e2e_notification() {
    let f = proj_fixture("e2e-notify");
    if start_fake_agent(&f, "basic.json").is_none() {
        return;
    }
    let sent: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = sent.clone();
    f.state.set_notification_hook(Some(Box::new(move |id, _| {
        sink.borrow_mut().push(id.into())
    })));
    // Focused: no notification.
    f.state.set_focus_override(Some(true));
    quick::agent_command(&f.state, "Polish the document").unwrap();
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    common::pump_for(Duration::from_millis(100));
    assert!(sent.borrow().is_empty());
    // Not focused: one notification at the turn's end.
    f.state.set_focus_override(Some(false));
    quick::agent_command(&f.state, "Polish the document again").unwrap();
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    common::pump_until(TIMEOUT, || !sent.borrow().is_empty());
    assert_eq!(sent.borrow().len(), 1);
    f.state.set_notification_hook(None);
    f.agent().stop();
}

/// The fake agents playing `scenario` that run as children of this test.
#[cfg(target_os = "linux")]
fn agents_running(scenario: &std::path::Path) -> usize {
    let me = std::process::id().to_string();
    let needle = scenario.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|entry| {
            let dir = entry.path();
            let stat = std::fs::read_to_string(dir.join("stat")).unwrap_or_default();
            // `pid (comm) state ppid …`
            let ppid = stat
                .rfind(')')
                .and_then(|end| stat[end + 1..].split_whitespace().nth(1));
            ppid == Some(me.as_str())
                && std::fs::read(dir.join("cmdline"))
                    .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&needle))
        })
        .count()
}

fn e2e_idle_stop_resumes() {
    let f = proj_fixture("e2e-idle");
    f.open("main.tex");
    let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
    let sink = events.clone();
    let id = f
        .agent()
        .connect_event(move |e| sink.borrow_mut().push(e.clone()));
    if start_fake_agent(&f, "resume.json").is_none() {
        return;
    }
    let agent = f.agent();
    let bridge = agent.bridge();
    let panel = f.panel();
    let sessions = |events: &[AgentEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Ready { session, .. } => Some(session.clone()),
                _ => None,
            })
            .collect()
    };
    let in_plan = |events: &RefCell<Vec<AgentEvent>>| {
        events
            .borrow()
            .iter()
            .any(|e| *e == AgentEvent::ModeChanged("plan".into()))
    };
    let first = sessions(&events.borrow())[0].clone();
    // The user chose plan mode in this conversation (the configured mode is `default`).
    agent.set_mode("plan");
    common::pump_until(TIMEOUT, || in_plan(&events));
    // A turn that edits the abstract, with the pane hidden.
    bridge.set_idle_stop(Some(IDLE));
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("Polish the abstract".into())],
        PromptOrigin::Panel
    ));
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    assert!(bridge.idle_stop_pending());
    let hunks = lemma_app::review_ui::summary(&f.state.workspace).1;
    assert_eq!(hunks, 1);
    let scenario = common::fixtures()
        .join("agent/resume.json")
        .canonicalize()
        .unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(agents_running(&scenario), 1);
    // Idle long enough: stopped and its process gone, the conversation kept to continue.
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    assert_eq!(bridge.resumable_session(), Some(first.clone()));
    #[cfg(target_os = "linux")]
    common::pump_until(TIMEOUT, || agents_running(&scenario) == 0);
    assert!(
        notices(&panel).contains(
            &"Claude was stopped after 1 idle minute to free memory; it resumes this conversation \
          with your next message."
                .to_owned()
        )
    );
    assert_eq!(panel.setup(), None, "no \"Claude is stopped\" card");
    // A start that fails (this agent exits at once) leaves the conversation to the next one.
    let exits = common::temp_dir("e2e-idle-exits").join("exits.json");
    std::fs::write(
        &exits,
        r#"{"_options": {"exit_on": "initialize"}, "default": [{"say": "never"}]}"#,
    )
    .unwrap();
    agent.set_scenario(Some(exits));
    let failed = block_on(async {
        glib::future_with_timeout(Duration::from_secs(15), agent.ensure_started()).await
    });
    assert!(matches!(failed, Ok(Err(_))), "{failed:?}");
    assert_eq!(bridge.resumable_session(), Some(first.clone()));
    agent.set_scenario(Some(scenario));
    // The next message starts the agent again, which loads the session: the same session,
    // nothing of the conversation it replays, and plan mode again.
    events.borrow_mut().clear();
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("Are you still there?".into())],
        PromptOrigin::Panel
    ));
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    common::pump_until(TIMEOUT, || in_plan(&events));
    let seen = events.borrow().clone();
    assert_eq!(sessions(&seen), [first]);
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            AgentEvent::ThoughtChunk(_)
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolUpdate { .. }
                | AgentEvent::Plan(_)
        )),
        "{seen:#?}"
    );
    panel.flush();
    let entries = panel.entries();
    let count = |kind: &str| kinds(&entries).iter().filter(|k| **k == kind).count();
    assert_eq!(
        (
            count("user"),
            count("agent"),
            count("tool"),
            count("thought")
        ),
        (2, 2, 1, 0),
        "{:?}",
        kinds(&entries)
    );
    assert!(
        !entries
            .iter()
            .any(|e| format!("{e:?}").contains("REPLAYED")),
        "{entries:#?}"
    );
    assert_eq!(lemma_app::review_ui::summary(&f.state.workspace).1, hunks);
    assert_eq!(panel.usage(), Some((53_000, 200_000)));
    f.agent().disconnect(id);
    bridge.set_idle_stop(None);
    agent.stop();
}

/// The idle stop promised to resume the conversation, but the project root moved while the
/// agent was stopped: the next start, in the new root, is a new conversation, and the transcript
/// says why before the message. Then a switch of backend says that the other agent starts a
/// new conversation.
fn e2e_idle_stop_elsewhere() {
    let f = proj_fixture("e2e-idle-elsewhere");
    let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
    let sink = events.clone();
    let id = f
        .agent()
        .connect_event(move |e| sink.borrow_mut().push(e.clone()));
    if start_fake_agent(&f, "resume.json").is_none() {
        return;
    }
    let agent = f.agent();
    let bridge = agent.bridge();
    let panel = f.panel();
    let sessions = |events: &[AgentEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Ready { session, .. } => Some(session.clone()),
                _ => None,
            })
            .collect()
    };
    let first = sessions(&events.borrow())[0].clone();
    let old = f.state.workspace.root().path().to_path_buf();
    bridge.set_idle_stop(Some(IDLE));
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("Hello".into())],
        PromptOrigin::Panel
    ));
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    assert_eq!(bridge.resumable_session(), Some(first.clone()));
    // No document is open: the first one opened elsewhere moves the root there. Nothing runs,
    // so nothing is stopped or said yet.
    let other = common::temp_dir("e2e-idle-elsewhere-other")
        .canonicalize()
        .unwrap();
    std::fs::write(other.join("notes.tex"), "Notes.\n").unwrap();
    let before = notices(&panel).len();
    block_on(f.state.open_file(&other.join("notes.tex"))).unwrap();
    assert_eq!(f.state.workspace.root().path(), other);
    assert_eq!(notices(&panel).len(), before);

    events.borrow_mut().clear();
    assert!(panel::send_prompt(
        &f.state,
        vec![PromptBlock::Text("Are you still there?".into())],
        PromptOrigin::Panel
    ));
    assert_eq!(wait_turn_end(&f, TIMEOUT), Some(StopReason::EndTurn));
    let second = sessions(&events.borrow());
    assert_eq!(second.len(), 1);
    assert_ne!(second[0], first, "a new session");
    panel.flush();
    let entries = panel.entries();
    let notice = format!(
        "The conversation was in {}; the project folder is now {}, so this is a new conversation.",
        old.display(),
        other.display()
    );
    let said = entries
        .iter()
        .position(|e| *e == Entry::Notice(notice.clone()))
        .unwrap_or_else(|| panic!("{notice:?} in {entries:#?}"));
    let asked = entries
        .iter()
        .rposition(|e| matches!(e, Entry::User { .. }))
        .unwrap();
    assert!(said < asked, "said before the message: {entries:#?}");
    assert_eq!(
        notices(&panel).iter().filter(|n| **n == notice).count(),
        1,
        "said once"
    );

    // Idle again, with the new conversation to resume; another backend cannot.
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    assert_eq!(bridge.resumable_session(), Some(second[0].clone()));
    let mut config = (*f.state.config.get()).clone();
    config.agent.backend = AgentBackend::Pi;
    f.state.set_config(Rc::new(config));
    assert_eq!(bridge.resumable_session(), None);
    assert_eq!(
        notices(&panel).last().map(String::as_str),
        Some("Switched to Pi. It starts a new conversation with your next prompt.")
    );
    agent.disconnect(id);
    bridge.set_idle_stop(None);
    agent.stop();
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
