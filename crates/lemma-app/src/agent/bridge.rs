//! Agent lifecycle and turn bookkeeping (WP2.4; ARCHITECTURE §4.2, §10.4).
//!
//! [`AgentController`] delegates every operation that talks to the agent process here. The
//! bridge owns the process's handle (through `lemma_agent::spawn`), the editor's MCP server
//! (`lemma_agent::mcp::McpServer`), the pumps that bring agent events and tool calls to the main
//! thread, and the bookkeeping of turns.
//!
//! # Starting ([`ensure_started`])
//!
//! One start at a time; concurrent callers share it. `Stopped`/`Failed` → `Starting` → `Idle`
//! on the agent's `Ready`, or `Failed(msg)` (and `Err(msg)` for every caller):
//!
//! 1. resolve the `lemma_agent::AgentConfig` on the jobs thread: node, the adapter's `index.js`
//!    and claude from [`super::install::detect`] (with a scenario,
//!    [`AgentController::scenario`], `node` is the fake agent,
//!    [`crate::testmode::fake_agent_binary`], and `adapter_js` the scenario; the real adapter is
//!    never involved); `lemma-mcp` from [`crate::testmode::sibling_binary`]; a socket
//!    `mcp-<pid>-<n>.sock` in `paths.runtime_dir` (created with mode 0700; on Windows a file
//!    naming a loopback port, see `lemma_agent`'s transport); a 32-hex token from the system's
//!    random generator; [`APPEND`]. Sockets of that form left behind by a run that was killed
//!    (their process is gone) are removed first ([`remove_stale_sockets`]); ours goes when the
//!    session stops (`McpServer::stop`);
//! 2. start the `McpServer` on that socket;
//! 3. `lemma_agent::spawn`, then pump `handle.events` through [`handle_event`] (bookkeeping, then
//!    [`AgentController::emit_event`]) and the tool calls of both the MCP server and
//!    `handle.tools` into [`super::tools::handle`], each with `glib::spawn_future_local`;
//! 4. on `Ready`, apply `agent.default_mode` (`default` or `plan`; any other mode the agent
//!    starts in is switched to `default`), the model and the effort
//!    ([`lemma_config::AgentCfg::model_for`], [`lemma_config::AgentCfg::effort_for`]; the effort
//!    is the `effort` option, or `reasoning_effort` for agents that call it so) when set and
//!    offered.
//!
//! **Backends** (`agent.backend`, WP5.2): `claude` as above; every other ACP agent (`pi`,
//! `codex`, `grok`, `gemini`, `opencode`, `copilot`, `custom`) is resolved with
//! [`super::install::detect_agent`] (with a scenario, the fake agent as for Claude) and spawned
//! as [`lemma_agent::AcpFlavor::Generic`] with [`GENERIC_APPEND`] as its instructions, so the
//! `fs/*` reads and writes of an agent that delegates them arrive on `handle.tools` like MCP
//! tool calls (one that writes files itself is covered by the file monitor, below); `api`
//! starts `lemma_agent::messages::spawn` with the key from `lemma_agent::keys::lookup` (no
//! process, no MCP server: its tool calls come on `handle.tools`). Whichever runs, the rest of
//! this module is the same. [`backend_name`] and [`agent_label`] name it in messages.
//!
//! A start that sees no `Ready` within [`START_TIMEOUT`] fails. An `Error` or `Exited` before
//! `Ready` fails the start with the agent's message.
//!
//! The agent works in the directory it was started in (its `cwd`, the project root then). When
//! the window's project root moves (`WindowState::connect_root_changed`: the first file opened
//! in an empty window lies elsewhere), a running agent is stopped with a toast; the next prompt
//! starts it in the new root, so its own tools (Bash, Read) and Lemma's agree on the project.
//! [`stop`] also removes the `render_page` copies the window wrote.
//!
//! # Turns ([`prompt`])
//!
//! A prompt while a turn runs or is being prepared is refused with an `AgentEvent::Error`.
//! Otherwise the agent is started if needed, then [`start_turn`] runs the turn-start steps of
//! §4.2.1: number the turn ([`AgentController::begin_turn`], state `Running`), save the dirty
//! buffers of project files (`agent.save_before_prompt`; files outside the project root are left
//! alone), take the [`TurnSnapshot`] on the jobs thread, keep the text of the workspace's saved
//! project sources ([`turn_start_text`]: a document that leaves the workspace during the turn
//! still has its baseline), write a local-history snapshot ([`super::history::record_turn`]),
//! call [`review_ui::on_turn_started`], announce the prompt ([`AgentController::emit_prompt`])
//! and build the blocks: the `<review_feedback>` ([`tools::take_feedback`]), the user's blocks,
//! and a resource describing the current file, the cursor line and the selection. Then
//! `AgentCmd::Prompt` goes out. Feedback that goes into a prompt that is not sent is kept for the
//! next one.
//!
//! The turn ends with the agent's `TurnEnded`: [`review_ui::on_turn_ended`]; when files changed
//! during the turn and `build.on_agent_turn_end` is set, a `Reason::AgentTurnEnd` build, unless
//! the agent's own `compile` already built everything it changed; when the window is not focused
//! and `agent.notify_on_turn_end` is set (and the user did not cancel), a desktop notification
//! with the stop reason and the number of changes to review; state `Idle`. Event callbacks see
//! the `TurnEnded` after all of this.
//!
//! A turn that cannot end normally still ends: a cancel before the prompt was sent, a
//! [`stop`] during the turn, the agent process dying, or an agent that does not confirm a cancel
//! within [`CANCEL_GRACE`] all end it locally, and the event callbacks get a
//! `TurnEnded(Cancelled)` (before `Exited` when the agent died).
//!
//! An agent that did not confirm a cancel in time still owes the `TurnEnded` of that turn, and
//! `lemma-agent` refuses prompts until it comes. So the next turn's prompt is **held** until the
//! late `TurnEnded` arrives (that event closes nothing, and the event callbacks do not see it
//! while the new turn runs: its own turn already ended here), then sent; a cancel or stop
//! meanwhile ends the new turn without sending it. If
//! the agent still has not finished after [`HELD_PROMPT_TIMEOUT`], it is stopped and the user is
//! told to send the message again.
//!
//! # Changes outside our tools
//!
//! During a turn and for [`AFTER_TURN_GRACE`] after it ([`in_agent_window`]), the file monitor
//! ([`super::watcher`]) turns changes of project files into agent hunks, using the turn's
//! snapshot ([`turn_snapshot`]) as the baseline of files that were not loaded. The monitor
//! waits for writes to settle, so a change a program made just before the turn ended is often
//! reported after it: it still gets the turn-end build ([`note_file_changed`],
//! [`LATE_BUILD_DELAY`]).

use super::{
    AgentController, AgentState, PromptBlock, PromptOrigin, SentPrompt, history, install, tools,
    watcher,
};
use crate::build_ui::Reason;
use crate::hooks::{HookId, Hooks};
use crate::review_ui;
use crate::settings::ConfigFile;
use crate::testmode;
use crate::window::WindowState;
use crate::workspace::{self, TurnSnapshot};
use lemma_agent::mcp::{McpServer, Timeouts};
use lemma_agent::{AgentCmd, AgentConfig, AgentEvent, ConfigOpt, StopReason, ToolCall};
use lemma_config::{AgentBackend, AgentCfg, Paths};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// The error of operations that need an agent this build cannot provide (kept for the stubs of
/// other modules that report it).
pub const NOT_AVAILABLE: &str = "The agent is not available in this build yet";

/// The `systemPrompt.append` text of §10.4.
pub const APPEND: &str = "You are working inside Lemma, a LaTeX editor. The user reviews every \
change you make as red/green hunks. Make ALL file changes with mcp__lemma__edit or \
mcp__lemma__write; never write files with Bash or other tools. mcp__lemma__read_file returns the \
live editor buffer, including unsaved changes. Build with mcp__lemma__compile and read errors with \
mcp__lemma__get_diagnostics instead of running latexmk. Tool results may contain a \
<review_feedback> block listing hunks the user accepted, rejected or modified; do not re-apply \
rejected changes unless asked. Follow the project's existing macros and style.";

/// [`APPEND`] for the other ACP agents, which name MCP tools their own way and keep their own
/// file tools (`lemma-agent` sends it with the session's first prompt).
pub const GENERIC_APPEND: &str = "You are working inside Lemma, a LaTeX editor. The user \
reviews every change you make as red/green hunks. Lemma provides an MCP server named \"lemma\" \
whose tools work on the live editor buffers: prefer its edit and write tools for changing files \
and its read_file tool for reading them (it includes unsaved changes). Never change files with \
shell commands (sed, redirections, scripts). Build with its compile tool and read errors with \
its get_diagnostics tool instead of running latexmk. Tool results may contain a \
<review_feedback> block listing hunks the user accepted, rejected or modified; do not re-apply \
rejected changes unless asked. Follow the project's existing macros and style.";

/// How long a start may take until the agent's `Ready` (the real adapter needs about 4 s).
pub const START_TIMEOUT: Duration = Duration::from_secs(60);
/// After a turn ends, changes by other programs still count as the agent's this long (a
/// subagent or a shell command finishing late).
pub const AFTER_TURN_GRACE: Duration = Duration::from_secs(10);
/// Quiet time after the last change reported after a turn's end before its build starts
/// (see [`note_file_changed`]); the file monitor handles one batch of files well within it.
pub const LATE_BUILD_DELAY: Duration = Duration::from_millis(300);
/// How long a cancelled turn waits for the agent's `TurnEnded` before it is ended locally.
pub const CANCEL_GRACE: Duration = Duration::from_secs(3);
/// How long a prompt is held for an agent that has not finished a cancelled turn (the ACP
/// adapter gives an interrupt up to 30 s) before the agent is stopped.
pub const HELD_PROMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// The id of the turn-end desktop notification (a new one replaces the last).
pub const NOTIFICATION_ID: &str = "lemma-agent-turn-ended";

/// The MCP bridge binary the agent's CLI runs (next to `lemma`).
const MCP_BRIDGE: &str = "lemma-mcp";
/// Tool calls remembered by [`Bridge::tool_records`].
const TOOL_RECORDS_KEPT: usize = 64;
/// Event handling times remembered by [`Bridge::event_durations`].
const EVENT_DURATIONS_KEPT: usize = 4096;
/// Unix socket paths must be shorter than this (`sun_path`).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 107;

const STOPPED: &str = "The agent was stopped";
const WINDOW_GONE: &str = "The window was closed";

/// One tool call the editor answered (see [`Bridge::tool_records`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRecord {
    /// The MCP call id (`Origin::Agent { call }` of its edits).
    pub call: u64,
    /// The MCP tool name (`edit`, `compile`, …).
    pub tool: &'static str,
    /// The file the call named (as the agent wrote it), if any.
    pub path: Option<PathBuf>,
    pub is_error: bool,
    /// The reply's text parts, joined by blank lines.
    pub text: String,
    /// Number of images in the reply.
    pub images: usize,
    /// Time from the call's arrival on the main thread to its reply.
    pub elapsed: Duration,
}

/// The running turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenTurn {
    id: u32,
    /// `AgentCmd::Prompt` went out (a cancel must then go to the agent).
    sent: bool,
    /// A project file changed during the turn (our tools or another program).
    changed: bool,
    /// …and has not been built since by the agent's `compile`.
    unbuilt: bool,
}

/// A started agent: its command channels and the MCP server.
struct Session {
    generation: u64,
    /// The directory the agent was started in (the project root then).
    cwd: PathBuf,
    /// The agent thread's command channel (`Shutdown` goes here directly).
    cmd: async_channel::Sender<AgentCmd>,
    /// Commands in order, forwarded to `cmd` by a task, so sending never blocks.
    outbox: async_channel::Sender<AgentCmd>,
    mcp: Option<McpServer>,
    /// Which modes may be chosen ([`lemma_agent::AcpFlavor::offers_mode`]).
    flavor: lemma_agent::AcpFlavor,
}

impl Session {
    fn send(&self, cmd: AgentCmd) -> bool {
        self.outbox.try_send(cmd).is_ok()
    }

    /// Asks the agent thread to shut down (it closes the session, kills the agent's process
    /// group and ends) and stops the MCP server. Never blocks on the agent.
    fn shut_down(mut self) {
        tracing::info!("stopping agent session {}", self.generation);
        let _ = self.cmd.try_send(AgentCmd::Shutdown);
        self.cmd.close();
        self.outbox.close();
        if let Some(mcp) = self.mcp.take() {
            mcp.stop();
        }
    }
}

/// The turn snapshot and the texts of the workspace's documents at the turn's start (see
/// [`turn_start_text`]), shared with the timer that drops them after the grace period.
type SnapshotCell = Rc<RefCell<Option<(u32, Rc<TurnSnapshot>, Rc<HashMap<PathBuf, String>>)>>>;

/// A prompt waiting for the agent to finish a turn that was ended here (see the module docs).
struct HeldPrompt {
    turn: u32,
    blocks: Vec<PromptBlock>,
    /// The review feedback among `blocks` (kept for the next prompt if this one is not sent).
    feedback: Option<String>,
}

/// A turn whose prompt is ready to send.
struct Prepared {
    turn: u32,
    blocks: Vec<PromptBlock>,
    feedback: Option<String>,
}

/// The bridge's private state, owned by the [`AgentController`].
pub struct Bridge {
    /// Incremented by every start and stop: events and tool calls of an older session are
    /// dropped, and a start that was overtaken by a stop gives up.
    generation: Cell<u64>,
    session: RefCell<Option<Session>>,
    /// Callers of `ensure_started` waiting for the running start.
    waiters: RefCell<Vec<async_channel::Sender<Result<(), String>>>>,
    start_timer: RefCell<Option<glib::SourceId>>,
    /// A prompt is being prepared (starting the agent, saving, the snapshot): its token, so that
    /// a preparation that a stop overtook does not end a newer one's.
    preparing: Cell<Option<u64>>,
    /// The last prompt token handed out.
    prepare_tokens: Cell<u64>,
    /// `TurnEnded` events the agent still owes for turns ended here after their prompt was sent.
    stale_turn_ends: Cell<u32>,
    /// The prompt waiting for them.
    held: RefCell<Option<HeldPrompt>>,
    held_timer: RefCell<Option<glib::SourceId>>,
    /// Review feedback lines a reply or prompt could not deliver (see `tools::take_feedback`).
    undelivered_feedback: RefCell<String>,
    /// The `render_page` copies written for this window, with their size and time.
    render_copies: RefCell<Vec<(PathBuf, (u64, SystemTime))>>,
    /// The root hook that stops an agent working in another directory is connected.
    root_hooked: Cell<bool>,
    /// `cancel` was called while the prompt was being prepared.
    cancel_requested: Cell<bool>,
    turn: Cell<Option<OpenTurn>>,
    cancel_timer: RefCell<Option<glib::SourceId>>,
    last_turn_end: Cell<Option<Instant>>,
    grace: Cell<Duration>,
    snapshot: SnapshotCell,
    snapshot_timer: Rc<RefCell<Option<glib::SourceId>>>,
    records: RefCell<VecDeque<ToolRecord>>,
    tool_hooks: Hooks<dyn Fn(&ToolRecord)>,
    durations: RefCell<VecDeque<(&'static str, Duration)>>,
    /// Counts finished builds (for the `compile` tool); connected on its first use.
    builds_finished: RefCell<Option<Rc<Cell<u64>>>>,
    /// The config-reload hook that keeps the MCP server's `compile` timeout in step with
    /// `build.timeout_s` is connected.
    config_hooked: Cell<bool>,
    /// The build for changes that arrived after the turn ended ([`note_file_changed`]).
    late_build: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            generation: Cell::new(0),
            session: RefCell::new(None),
            waiters: RefCell::new(Vec::new()),
            start_timer: RefCell::new(None),
            preparing: Cell::new(None),
            prepare_tokens: Cell::new(0),
            stale_turn_ends: Cell::new(0),
            held: RefCell::new(None),
            held_timer: RefCell::new(None),
            undelivered_feedback: RefCell::new(String::new()),
            render_copies: RefCell::new(Vec::new()),
            root_hooked: Cell::new(false),
            cancel_requested: Cell::new(false),
            turn: Cell::new(None),
            cancel_timer: RefCell::new(None),
            last_turn_end: Cell::new(None),
            grace: Cell::new(AFTER_TURN_GRACE),
            snapshot: Rc::default(),
            snapshot_timer: Rc::default(),
            records: RefCell::new(VecDeque::new()),
            tool_hooks: Hooks::default(),
            durations: RefCell::new(VecDeque::new()),
            builds_finished: RefCell::new(None),
            config_hooked: Cell::new(false),
            late_build: Rc::default(),
        }
    }
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bridge")
            .field("generation", &self.generation.get())
            .field("session", &self.session.borrow().is_some())
            .field("turn", &self.turn.get())
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// True while an agent process is attached (from the start until a stop or its exit).
    pub fn has_session(&self) -> bool {
        self.session.borrow().is_some()
    }

    /// True while a prompt is being prepared (the agent starting for it, the save, the
    /// snapshot) and not yet sent; [`cancel`] drops it then.
    pub fn is_preparing(&self) -> bool {
        self.preparing.get().is_some()
    }

    /// The last tool calls answered (at most 64), oldest first.
    pub fn tool_records(&self) -> Vec<ToolRecord> {
        self.records.borrow().iter().cloned().collect()
    }

    /// Calls `f` after every tool call was answered (the panel can link edit cards to hunks).
    pub fn connect_tool_done(&self, f: impl Fn(&ToolRecord) + 'static) -> HookId {
        self.tool_hooks.add(Rc::new(f))
    }

    /// Removes a callback added by [`Bridge::connect_tool_done`].
    pub fn disconnect_tool_done(&self, id: HookId) {
        self.tool_hooks.remove(id);
    }

    /// How long the main thread spent on each of the last agent events of the running session
    /// (at most 4096, oldest first): the bridge's bookkeeping plus every `connect_event`
    /// callback, with the event's kind ([`event_kind`]).
    pub fn event_durations(&self) -> Vec<(&'static str, Duration)> {
        self.durations.borrow().iter().copied().collect()
    }

    /// Changes [`AFTER_TURN_GRACE`] for this window (tests).
    pub fn set_after_turn_grace(&self, grace: Duration) {
        self.grace.set(grace);
    }

    /// The number of the running turn, if one runs.
    pub fn open_turn(&self) -> Option<u32> {
        self.turn.get().map(|t| t.id)
    }

    /// True while the running turn's prompt waits for the agent to finish a turn that was ended
    /// here (see the module docs).
    pub fn prompt_held(&self) -> bool {
        self.held.borrow().is_some()
    }

    /// Takes the review feedback lines that could not be delivered so far.
    pub(crate) fn take_undelivered_feedback(&self) -> String {
        self.undelivered_feedback.take()
    }

    /// Keeps feedback `lines` that did not reach the agent, before any kept since.
    pub(crate) fn keep_undelivered_feedback(&self, lines: &str) {
        let mut kept = self.undelivered_feedback.borrow_mut();
        kept.insert_str(0, lines);
    }

    /// Records a `render_page` copy written for this window (removed by [`stop`]).
    pub(crate) fn note_render_copy(&self, path: PathBuf, stamp: (u64, SystemTime)) {
        let mut copies = self.render_copies.borrow_mut();
        copies.retain(|(p, _)| *p != path);
        copies.push((path, stamp));
    }

    pub(crate) fn record_tool(&self, record: ToolRecord) {
        {
            let mut records = self.records.borrow_mut();
            if records.len() == TOOL_RECORDS_KEPT {
                records.pop_front();
            }
            records.push_back(record.clone());
        }
        for f in self.tool_hooks.snapshot() {
            f(&record);
        }
    }

    fn record_duration(&self, kind: &'static str, duration: Duration) {
        let mut durations = self.durations.borrow_mut();
        if durations.len() == EVENT_DURATIONS_KEPT {
            durations.pop_front();
        }
        durations.push_back((kind, duration));
    }

    /// The counter of finished builds of `state`'s build controller (connected once).
    pub(crate) fn builds_finished(&self, state: &WindowState) -> Option<Rc<Cell<u64>>> {
        if let Some(counter) = self.builds_finished.borrow().as_ref() {
            return Some(counter.clone());
        }
        let build = state.build.get()?;
        let counter = Rc::new(Cell::new(0u64));
        let count = counter.clone();
        build.connect_finished(move |_| count.set(count.get() + 1));
        self.builds_finished.replace(Some(counter.clone()));
        Some(counter)
    }

    fn next_generation(&self) -> u64 {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        generation
    }

    fn send(&self, cmd: AgentCmd) -> bool {
        match self.session.borrow().as_ref() {
            Some(session) => session.send(cmd),
            None => false,
        }
    }

    fn clear_timer(slot: &RefCell<Option<glib::SourceId>>) {
        // Every stored id belongs to a pending source: callbacks take theirs out first.
        if let Some(id) = slot.take() {
            id.remove();
        }
    }

    fn resolve_waiters(&self, result: &Result<(), String>) {
        for waiter in self.waiters.take() {
            let _ = waiter.try_send(result.clone());
        }
    }
}

// ----- starting and stopping -----

/// See the module docs: resolves when the agent is `Idle` (or already running a turn).
pub fn ensure_started(
    controller: &Rc<AgentController>,
) -> impl Future<Output = Result<(), String>> + use<> {
    let bridge = controller.bridge();
    let (tx, rx) = async_channel::bounded(1);
    let ready = match controller.state() {
        AgentState::Idle | AgentState::Running { .. } if bridge.has_session() => Some(Ok(())),
        AgentState::Starting => {
            bridge.waiters.borrow_mut().push(tx);
            None
        }
        _ => {
            bridge.waiters.borrow_mut().push(tx);
            begin_start(controller);
            None
        }
    };
    async move {
        match ready {
            Some(result) => result,
            None => rx.recv().await.unwrap_or_else(|_| Err(STOPPED.to_owned())),
        }
    }
}

fn begin_start(controller: &Rc<AgentController>) {
    let bridge = controller.bridge();
    if let Some(session) = bridge.session.take() {
        session.shut_down();
    }
    let generation = bridge.next_generation();
    controller.set_state(AgentState::Starting);
    // A callback may have stopped the agent already.
    if bridge.generation.get() != generation {
        return;
    }
    let weak = Rc::downgrade(controller);
    let timer = glib::timeout_add_local_once(START_TIMEOUT, {
        let weak = weak.clone();
        move || {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            let bridge = controller.bridge();
            bridge.start_timer.take();
            if bridge.generation.get() == generation && controller.state() == AgentState::Starting {
                let name = controller.window().map_or_else(
                    || "The agent".to_owned(),
                    |s| backend_name(s.config.agent.backend, &s.config.agent),
                );
                fail_start(
                    &controller,
                    format!(
                        "{name} did not start within {} seconds",
                        START_TIMEOUT.as_secs()
                    ),
                );
            }
        }
    });
    Bridge::clear_timer(&bridge.start_timer);
    bridge.start_timer.replace(Some(timer));
    glib::spawn_future_local(async move {
        if let Err(message) = start_session(&weak, generation).await
            && let Some(controller) = weak.upgrade()
            && controller.bridge().generation.get() == generation
        {
            fail_start(&controller, message);
        }
    });
}

/// The name `[agent.<backend>] name` gives `backend`, if any.
fn configured_name(backend: AgentBackend, cfg: &AgentCfg) -> Option<String> {
    cfg.acp(backend)
        .map(|t| t.name.trim())
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// The name of `backend` in messages and the pane's header (`[agent.<backend>] name` when set).
pub fn backend_name(backend: AgentBackend, cfg: &AgentCfg) -> String {
    if let Some(name) = configured_name(backend, cfg) {
        return name;
    }
    match backend {
        AgentBackend::Claude => "Claude",
        AgentBackend::Api => "Claude (API)",
        AgentBackend::Pi => "Pi",
        AgentBackend::Codex => "Codex",
        AgentBackend::Grok => "Grok Build",
        AgentBackend::Gemini => "Gemini CLI",
        AgentBackend::OpenCode => "OpenCode",
        AgentBackend::Copilot => "Copilot",
        AgentBackend::Custom => "Custom agent",
    }
    .to_owned()
}

/// What messages call the agent of `backend`: "Claude" (Claude Code or the API), "Pi", "Codex",
/// "Grok", … (`[agent.<backend>] name` when set).
pub fn agent_label(backend: AgentBackend, cfg: &AgentCfg) -> String {
    if let Some(name) = configured_name(backend, cfg) {
        return name;
    }
    match backend {
        AgentBackend::Claude | AgentBackend::Api => "Claude",
        AgentBackend::Grok => "Grok",
        AgentBackend::Gemini => "Gemini",
        other => return backend_name(other, cfg),
    }
    .to_owned()
}

/// The backend picker's label of `backend` (`[agent.<backend>] name` when set).
pub fn picker_label(backend: AgentBackend, cfg: &AgentCfg) -> String {
    if let Some(name) = configured_name(backend, cfg) {
        return name;
    }
    match backend {
        AgentBackend::Claude => "Claude Code",
        AgentBackend::Api => "Anthropic API",
        AgentBackend::Copilot => "GitHub Copilot",
        other => return backend_name(other, cfg),
    }
    .to_owned()
}

/// The kind of ACP agent `backend` runs, which also decides the modes Lemma offers with it:
/// Claude Code's policy for Claude Code and the API, the generic one for the others.
pub fn acp_flavor(backend: AgentBackend) -> lemma_agent::AcpFlavor {
    match backend {
        AgentBackend::Claude | AgentBackend::Api => lemma_agent::AcpFlavor::Claude,
        _ => lemma_agent::AcpFlavor::Generic,
    }
}

/// [`agent_label`] for `controller`'s window ("The agent" without one).
fn label_of(controller: &AgentController) -> String {
    controller.window().map_or_else(
        || "The agent".to_owned(),
        |s| agent_label(s.config.agent.backend, &s.config.agent),
    )
}

/// What the start needs from the jobs thread.
struct Resolved {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    mcp_bridge: PathBuf,
    socket: PathBuf,
    token: String,
    flavor: lemma_agent::AcpFlavor,
}

async fn start_session(weak: &Weak<AgentController>, generation: u64) -> Result<(), String> {
    let (scenario, agent_cfg, paths, runtime, build_timeout) = {
        let controller = weak.upgrade().ok_or(WINDOW_GONE)?;
        let state = controller.window().ok_or(WINDOW_GONE)?;
        (
            controller.scenario(),
            state.config.agent.clone(),
            state.paths.clone(),
            tools::runtime_dir(&state),
            Duration::from_secs(state.config.build.timeout_s),
        )
    };
    let backend = agent_cfg.backend;
    if backend == AgentBackend::Api {
        return start_api_session(weak, generation, scenario, build_timeout).await;
    }
    let resolved =
        gio::spawn_blocking(move || resolve(scenario, &agent_cfg, paths.as_ref(), &runtime))
            .await
            .map_err(|_| "finding the agent's programs failed".to_owned())??;
    let controller = weak.upgrade().ok_or(WINDOW_GONE)?;
    let bridge = controller.bridge();
    if bridge.generation.get() != generation {
        // Stopped meanwhile.
        return Ok(());
    }
    // The root as it is now (it may have moved while the programs were looked for).
    let root = controller
        .window()
        .ok_or(WINDOW_GONE)?
        .workspace
        .root()
        .path()
        .to_path_buf();
    let (tool_tx, tool_rx) = async_channel::unbounded();
    // `compile` waits for the build: its timeout is the build's plus a grace period.
    let timeouts = Timeouts::for_build_timeout(build_timeout);
    let mcp = McpServer::start_with(&resolved.socket, resolved.token.clone(), tool_tx, timeouts)
        .map_err(|e| {
            format!(
                "cannot start the editor's tool server at {}: {e}",
                resolved.socket.display()
            )
        })?;
    let flavor = resolved.flavor;
    let config = AgentConfig {
        program: resolved.program,
        args: resolved.args,
        env: resolved.env,
        cwd: root.clone(),
        mcp_bridge: resolved.mcp_bridge,
        mcp_socket: resolved.socket,
        mcp_token: resolved.token,
        system_prompt_append: match flavor {
            lemma_agent::AcpFlavor::Claude => APPEND,
            lemma_agent::AcpFlavor::Generic => GENERIC_APPEND,
        }
        .to_owned(),
        raw_log: None,
        flavor,
    };
    tracing::info!(
        "starting the agent: {} {}",
        config.program.display(),
        config
            .args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let handle = match lemma_agent::spawn(config) {
        Ok(handle) => handle,
        Err(e) => {
            mcp.stop();
            let name = controller.window().map_or_else(
                || "the agent".to_owned(),
                |s| backend_name(backend, &s.config.agent),
            );
            return Err(format!("cannot start {name}: {e}"));
        }
    };
    attach_session(
        &controller,
        weak,
        generation,
        root,
        handle,
        Some(mcp),
        Some(tool_rx),
        flavor,
    );
    Ok(())
}

/// The `api` backend (see the module docs): the Messages API client on its own thread.
async fn start_api_session(
    weak: &Weak<AgentController>,
    generation: u64,
    scenario: Option<PathBuf>,
    build_timeout: Duration,
) -> Result<(), String> {
    if scenario.is_some() {
        return Err(
            "the API backend has no scripted test agent; use the claude or pi \
                    backend with a scenario"
                .to_owned(),
        );
    }
    let key = gio::spawn_blocking(lemma_agent::keys::lookup)
        .await
        .map_err(|_| "looking up the API key failed".to_owned())?
        .map(|(key, _source)| key)
        .ok_or("No Anthropic API key: set one with \"Set API key…\" in the agent pane")?;
    let controller = weak.upgrade().ok_or(WINDOW_GONE)?;
    if controller.bridge().generation.get() != generation {
        return Ok(());
    }
    let root = controller
        .window()
        .ok_or(WINDOW_GONE)?
        .workspace
        .root()
        .path()
        .to_path_buf();
    let mut config = lemma_agent::messages::MessagesConfig::new(key);
    // `compile` waits for the build: its timeout is the build's plus a grace period.
    config.timeouts = Timeouts::for_build_timeout(build_timeout);
    // After a 401, a key stored since ("Set API key…") is used without a restart.
    config.key_lookup = Some(stored_api_key);
    let handle = lemma_agent::messages::spawn(config).map_err(|e| {
        format!(
            "cannot start {}: {e}",
            backend_name(AgentBackend::Api, &AgentCfg::default())
        )
    })?;
    attach_session(
        &controller,
        weak,
        generation,
        root,
        handle,
        None,
        None,
        lemma_agent::AcpFlavor::Claude,
    );
    Ok(())
}

/// The API key as a new session would find it (on the API backend's thread).
fn stored_api_key() -> Option<lemma_agent::keys::ApiKey> {
    lemma_agent::keys::lookup().map(|(key, _source)| key)
}

/// Makes `handle` the running session and starts its pumps (the MCP server's tool calls too).
#[allow(clippy::too_many_arguments)]
fn attach_session(
    controller: &Rc<AgentController>,
    weak: &Weak<AgentController>,
    generation: u64,
    root: PathBuf,
    handle: lemma_agent::AgentHandle,
    mcp: Option<McpServer>,
    mcp_tools: Option<async_channel::Receiver<ToolCall>>,
    flavor: lemma_agent::AcpFlavor,
) {
    let bridge = controller.bridge();
    let (outbox, queued) = async_channel::unbounded();
    glib::spawn_future_local(forward(queued, handle.cmd.clone()));
    bridge.session.replace(Some(Session {
        generation,
        cwd: root,
        cmd: handle.cmd,
        outbox,
        mcp,
        flavor,
    }));
    if !bridge.root_hooked.replace(true)
        && let Some(state) = controller.window()
    {
        state.connect_root_changed(root_changed);
    }
    if !bridge.config_hooked.replace(true)
        && let Some(state) = controller.window()
    {
        state.connect_config_changed(|state, file| {
            if file == ConfigFile::Config
                && let Some(controller) = state.agent_controller()
                && let Some(session) = controller.bridge().session.borrow().as_ref()
                && let Some(mcp) = &session.mcp
            {
                let build = Duration::from_secs(state.config.build.timeout_s);
                mcp.set_timeouts(Timeouts::for_build_timeout(build));
            }
        });
    }
    glib::spawn_future_local(event_pump(weak.clone(), generation, handle.events));
    if let Some(tool_rx) = mcp_tools {
        glib::spawn_future_local(tool_pump(weak.clone(), generation, tool_rx));
    }
    glib::spawn_future_local(tool_pump(weak.clone(), generation, handle.tools));
}

/// Attaches `cmd` as the agent's command channel without starting a process or an MCP server:
/// the state becomes `Starting`, commands go to `cmd`, and whoever drives the session gives its
/// events to [`handle_event`] (a `Ready` makes it `Idle`). For tests of the bookkeeping; a stop
/// sends `Shutdown` and closes `cmd`.
pub fn attach_channel(controller: &Rc<AgentController>, cmd: async_channel::Sender<AgentCmd>) {
    let bridge = controller.bridge();
    if let Some(session) = bridge.session.take() {
        session.shut_down();
    }
    let generation = bridge.next_generation();
    let (outbox, queued) = async_channel::unbounded();
    glib::spawn_future_local(forward(queued, cmd.clone()));
    let cwd = controller
        .window()
        .map(|s| s.workspace.root().path().to_path_buf())
        .unwrap_or_default();
    bridge.session.replace(Some(Session {
        generation,
        cwd,
        cmd,
        outbox,
        mcp: None,
        flavor: controller
            .window()
            .map_or(lemma_agent::AcpFlavor::Claude, |s| {
                acp_flavor(s.config.agent.backend)
            }),
    }));
    forget_held(controller);
    controller.set_state(AgentState::Starting);
}

/// Finds the programs and makes the socket path and token (blocking; jobs thread).
fn resolve(
    scenario: Option<PathBuf>,
    cfg: &AgentCfg,
    paths: Option<&Paths>,
    runtime: &Path,
) -> Result<Resolved, String> {
    let flavor = acp_flavor(cfg.backend);
    let (program, args, env) = match scenario {
        Some(scenario) => {
            let fake = testmode::fake_agent_binary().ok_or(
                "the test agent lemma-fake-agent was not found next to lemma (build it, or set \
                 LEMMA_FAKE_AGENT)",
            )?;
            if !scenario.is_file() {
                return Err(format!(
                    "the agent scenario {} does not exist",
                    scenario.display()
                ));
            }
            (fake, vec![scenario.into_os_string()], Vec::new())
        }
        None => {
            let paths = paths.ok_or_else(|| {
                format!(
                    "the home directory is unknown, so {} cannot be found",
                    backend_name(cfg.backend, cfg)
                )
            })?;
            if cfg.backend == AgentBackend::Claude {
                let toolchain = install::detect(cfg, paths).map_err(|e| e.to_string())?;
                (
                    toolchain.node,
                    vec![toolchain.adapter_js.into_os_string()],
                    vec![(
                        OsString::from("CLAUDE_CODE_EXECUTABLE"),
                        toolchain.claude.into_os_string(),
                    )],
                )
            } else {
                let command =
                    install::detect_agent(cfg, cfg.backend, paths).map_err(|e| e.to_string())?;
                (command.program, command.args, command.env)
            }
        }
    };
    let mcp_bridge = testmode::sibling_binary(MCP_BRIDGE)
        .ok_or_else(|| format!("{MCP_BRIDGE} was not found next to lemma"))?;
    tools::create_private_dir(runtime)?;
    remove_stale_sockets(runtime);
    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);
    let socket = runtime.join(format!(
        "mcp-{}-{}.sock",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    ));
    #[cfg(unix)]
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(format!(
            "the socket path {} is too long for a unix socket",
            socket.display()
        ));
    }
    Ok(Resolved {
        program,
        args,
        env,
        mcp_bridge,
        socket,
        token: random_token()?,
        flavor,
    })
}

/// Removes the MCP sockets (`mcp-<pid>-<n>.sock`) in `dir` whose process is gone: a run that was
/// killed could not remove its own. Only sockets named that way are touched, and those of live
/// processes (other Lemma windows and instances) stay. Blocking (jobs thread).
pub fn remove_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(socket_pid) else {
            continue;
        };
        let alive = pid == std::process::id() || process_exists(pid);
        if alive || !is_socket(&entry) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!("removed the stale socket {}", entry.path().display()),
            Err(e) => tracing::debug!("cannot remove {}: {e}", entry.path().display()),
        }
    }
}

/// Whether `entry` is a socket (on Windows: a port file, which holds a loopback address).
fn is_socket(entry: &std::fs::DirEntry) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        entry.file_type().is_ok_and(|t| t.is_socket())
    }
    #[cfg(windows)]
    {
        entry.file_type().is_ok_and(|t| t.is_file())
            && std::fs::read_to_string(entry.path()).is_ok_and(|text| {
                text.trim()
                    .parse::<std::net::SocketAddr>()
                    .is_ok_and(|a| a.ip().is_loopback())
            })
    }
}

/// True if process `pid` exists and is running.
#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    lemma_core::process::is_running(pid)
}

/// True if process `pid` exists (`kill(pid, 0)`; works where there is no `/proc`, as on macOS).
/// Ids that `kill` would read as a process group (0, or too large for a `pid_t`) never do.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    let r = unsafe { libc::kill(pid, 0) };
    // EPERM: it exists but belongs to another user.
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The process id in an MCP socket name `mcp-<pid>-<n>.sock`.
fn socket_pid(name: &str) -> Option<u32> {
    let (pid, n) = name
        .strip_prefix("mcp-")?
        .strip_suffix(".sock")?
        .split_once('-')?;
    n.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// The window's project root moved: an agent started in another directory is stopped (see the
/// module docs).
fn root_changed(state: &Rc<WindowState>) {
    let Some(controller) = state.agent_controller() else {
        return;
    };
    let root = state.workspace.root().path().to_path_buf();
    let cwd = controller
        .bridge()
        .session
        .borrow()
        .as_ref()
        .map(|s| s.cwd.clone());
    if cwd.is_none_or(|cwd| cwd == root) {
        return;
    }
    tracing::info!(
        "the project root moved to {}; stopping the agent",
        root.display()
    );
    stop(controller);
    state.toast(&format!(
        "The project folder is now {}: {} was stopped, and starts there with your next \
         message",
        root.display(),
        agent_label(state.config.agent.backend, &state.config.agent)
    ));
}

/// 32 hex digits from `/dev/urandom`.
#[cfg(unix)]
fn random_token() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// 32 hex digits from the system's preferred random generator (`BCryptGenRandom`).
#[cfg(windows)]
fn random_token() -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for `bytes.len()` bytes; no algorithm handle is needed with
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(format!("cannot get random bytes (status {status:#x})"));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sends the queued commands to the agent thread, in order.
async fn forward(queued: async_channel::Receiver<AgentCmd>, cmd: async_channel::Sender<AgentCmd>) {
    while let Ok(command) = queued.recv().await {
        if cmd.send(command).await.is_err() {
            break;
        }
    }
}

async fn event_pump(
    weak: Weak<AgentController>,
    generation: u64,
    events: async_channel::Receiver<AgentEvent>,
) {
    while let Ok(event) = events.recv().await {
        let Some(controller) = weak.upgrade() else {
            return;
        };
        if controller.bridge().generation.get() != generation {
            // A stopped session: what it still says does not matter.
            return;
        }
        let kind = event_kind(&event);
        let started = Instant::now();
        handle_event(&controller, event);
        controller.bridge().record_duration(kind, started.elapsed());
    }
    // The agent thread ended without saying why.
    if let Some(controller) = weak.upgrade() {
        let bridge = controller.bridge();
        let current = bridge.generation.get() == generation && bridge.has_session();
        if current {
            handle_event(&controller, AgentEvent::Exited(None));
        }
    }
}

async fn tool_pump(
    weak: Weak<AgentController>,
    generation: u64,
    calls: async_channel::Receiver<ToolCall>,
) {
    while let Ok(call) = calls.recv().await {
        let state = weak
            .upgrade()
            .filter(|c| c.bridge().generation.get() == generation)
            .and_then(|c| c.window());
        match state {
            Some(state) => {
                glib::spawn_future_local(tools::handle(state, call));
            }
            None => {
                let _ = call.reply.send(tools::error_reply(STOPPED));
            }
        }
    }
}

fn fail_start(controller: &AgentController, message: String) {
    tracing::warn!("the agent did not start: {message}");
    let bridge = controller.bridge();
    bridge.next_generation();
    Bridge::clear_timer(&bridge.start_timer);
    if let Some(session) = bridge.session.take() {
        session.shut_down();
    }
    controller.set_state(AgentState::Failed(message.clone()));
    bridge.resolve_waiters(&Err(message));
}

/// See the module docs: stops the agent and its MCP server; `Stopped`. Safe in every state (also
/// while the controller is being dropped).
pub fn stop(controller: &AgentController) {
    let bridge = controller.bridge();
    bridge.next_generation();
    Bridge::clear_timer(&bridge.start_timer);
    Bridge::clear_timer(&bridge.cancel_timer);
    if let Some(session) = bridge.session.take() {
        session.shut_down();
    }
    bridge.resolve_waiters(&Err(STOPPED.to_owned()));
    bridge.preparing.set(None);
    forget_held(controller);
    if close_turn(controller, StopReason::Cancelled) {
        controller.emit_event(&AgentEvent::TurnEnded(StopReason::Cancelled));
    }
    controller.set_state(AgentState::Stopped);
    // The agent that could look at them is gone.
    tools::remove_render_copies(&bridge.render_copies.take());
}

/// Drops the held prompt (keeping its feedback for the next prompt) and forgets the `TurnEnded`
/// events the agent owed: its session is gone or new.
fn forget_held(controller: &AgentController) {
    let bridge = controller.bridge();
    bridge.stale_turn_ends.set(0);
    Bridge::clear_timer(&bridge.held_timer);
    if let Some(held) = bridge.held.take() {
        keep_feedback(controller, held.feedback);
    }
}

/// Keeps `feedback`, which a prompt that was not sent carried, for the next one.
fn keep_feedback(controller: &AgentController, feedback: Option<String>) {
    if let (Some(feedback), Some(state)) = (feedback, controller.window()) {
        tools::requeue_feedback(&state, feedback);
    }
}

// ----- events -----

/// The bridge's bookkeeping for `event` (see the module docs), then
/// [`AgentController::emit_event`]. The event pump calls it for every event of the running
/// session; tests call it to drive a window without an agent process.
pub fn handle_event(controller: &Rc<AgentController>, event: AgentEvent) {
    let bridge = controller.bridge();
    match &event {
        AgentEvent::Ready {
            agent,
            modes,
            mode,
            options,
            ..
        } => {
            tracing::info!("agent ready: {agent}");
            Bridge::clear_timer(&bridge.start_timer);
            if controller.state() == AgentState::Starting {
                controller.set_state(AgentState::Idle);
            }
            bridge.resolve_waiters(&Ok(()));
            apply_preferences(controller, modes, mode, options);
        }
        AgentEvent::TurnEnded(reason) => {
            let stale = bridge.stale_turn_ends.get();
            if stale > 0 {
                // The late end of a turn that was ended here: it closes nothing. While a newer
                // turn runs (its prompt was held for this), the callbacks must not see it: they
                // would end that turn.
                tracing::info!("the agent finished a turn ended earlier ({reason:?})");
                bridge.stale_turn_ends.set(stale - 1);
                if stale == 1 {
                    release_held(controller);
                }
                if bridge.turn.get().is_some() {
                    return;
                }
            } else {
                Bridge::clear_timer(&bridge.cancel_timer);
                close_turn(controller, *reason);
            }
        }
        AgentEvent::Error(message) if controller.state() == AgentState::Starting => {
            fail_start(controller, message.clone());
        }
        AgentEvent::Exited(code) => agent_exited(controller, *code),
        _ => {}
    }
    controller.emit_event(&event);
}

/// A short name of `event`'s kind (`message_chunk`, `tool_call`, …), as in ACP's updates.
pub fn event_kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::Ready { .. } => "ready",
        AgentEvent::MessageChunk(_) => "message_chunk",
        AgentEvent::ThoughtChunk(_) => "thought_chunk",
        AgentEvent::ToolCall { .. } => "tool_call",
        AgentEvent::ToolUpdate { .. } => "tool_call_update",
        AgentEvent::Plan(_) => "plan",
        AgentEvent::Commands(_) => "available_commands",
        AgentEvent::ModeChanged(_) => "mode",
        AgentEvent::ConfigChanged(_) => "config",
        AgentEvent::Permission { .. } => "permission",
        AgentEvent::PermissionWithdrawn { .. } => "permission_withdrawn",
        AgentEvent::TurnEnded(_) => "turn_ended",
        AgentEvent::Error(_) => "error",
        AgentEvent::Exited(_) => "exited",
        AgentEvent::Usage { .. } => "usage",
        AgentEvent::Cost { .. } => "cost",
        AgentEvent::Notice(_) => "notice",
    }
}

/// The process ended: a start fails; otherwise the session goes, a running turn ends and the
/// state becomes `Failed` (a later start tries again).
fn agent_exited(controller: &AgentController, code: Option<i32>) {
    let state = controller.state();
    // Stopped by us, or a start that already failed (with the agent's own message).
    if matches!(state, AgentState::Stopped | AgentState::Failed(_)) {
        return;
    }
    let code = code.map_or_else(String::new, |c| format!(" (exit code {c})"));
    if state == AgentState::Starting {
        let name = label_of(controller);
        fail_start(controller, format!("{name} exited while starting{code}"));
        return;
    }
    let bridge = controller.bridge();
    bridge.next_generation();
    Bridge::clear_timer(&bridge.cancel_timer);
    if let Some(session) = bridge.session.take() {
        session.shut_down();
    }
    forget_held(controller);
    if close_turn(controller, StopReason::Cancelled) {
        controller.emit_event(&AgentEvent::TurnEnded(StopReason::Cancelled));
    }
    controller.set_state(AgentState::Failed(format!(
        "{} exited unexpectedly{code}",
        label_of(controller)
    )));
}

/// `agent.default_mode`, `agent.model` and `agent.effort`, applied when the agent is ready.
fn apply_preferences(
    controller: &AgentController,
    modes: &[(String, String)],
    mode: &str,
    options: &[ConfigOpt],
) {
    let Some(state) = controller.window() else {
        return;
    };
    let cfg = state.config.agent.clone();
    let backend = cfg.backend;
    let bridge = controller.bridge();
    // Only offered modes (risk §9.6): an agent that starts in a mode Lemma does not offer
    // (`acceptEdits`, `bypassPermissions`, …) was switched by `lemma-agent`; here the configured
    // mode, when the agent has it.
    let wanted = if cfg.default_mode == "plan" {
        "plan"
    } else {
        "default"
    };
    if mode != wanted && modes.iter().any(|(id, _)| id == wanted) {
        bridge.send(AgentCmd::SetMode(wanted.to_owned()));
    }
    for (key, value) in [
        ("model", cfg.model_for(backend)),
        ("effort", cfg.effort_for(backend)),
    ] {
        if value.is_empty() {
            continue;
        }
        let ids = if key == "effort" {
            EFFORT_OPTIONS
        } else {
            &["model"][..]
        };
        let Some(option) = ids
            .iter()
            .find_map(|id| options.iter().find(|o| o.id == *id))
        else {
            tracing::warn!("agent.{key} = {value:?}: the agent has no such option; ignored");
            continue;
        };
        if option.current == value {
            continue;
        }
        if option.values.iter().any(|(v, _)| v == value) {
            bridge.send(AgentCmd::SetConfig {
                id: option.id.clone(),
                value: value.to_owned(),
            });
        } else {
            tracing::warn!("agent.{key} = {value:?} is not offered by the agent; ignored");
        }
    }
}

/// The ids of the session option that sets the reasoning effort: Claude Code's `effort`, and
/// `reasoning_effort` of Codex and Grok Build.
pub const EFFORT_OPTIONS: &[&str] = &["effort", "reasoning_effort"];

// ----- turns -----

/// See the module docs. Refused (with an `AgentEvent::Error`) while a turn runs or is being
/// prepared.
pub fn prompt(controller: &Rc<AgentController>, blocks: Vec<PromptBlock>, origin: PromptOrigin) {
    let bridge = controller.bridge();
    if controller.is_running() || bridge.preparing.get().is_some() {
        controller.emit_event(&AgentEvent::Error(format!(
            "{} is still working on the previous prompt: wait for it, or stop it first",
            label_of(controller)
        )));
        return;
    }
    let token = bridge.prepare_tokens.get() + 1;
    bridge.prepare_tokens.set(token);
    bridge.preparing.set(Some(token));
    bridge.cancel_requested.set(false);
    let controller = controller.clone();
    glib::spawn_future_local(async move {
        let result = run_prompt(&controller, blocks, origin).await;
        // A stop, and a newer prompt after it, may have come meanwhile: that one's preparation
        // is not over.
        let bridge = controller.bridge();
        if bridge.preparing.get() == Some(token) {
            bridge.preparing.set(None);
        }
        if let Err(message) = result {
            controller.emit_event(&AgentEvent::Error(message));
        }
    });
}

async fn run_prompt(
    controller: &Rc<AgentController>,
    blocks: Vec<PromptBlock>,
    origin: PromptOrigin,
) -> Result<(), String> {
    let bridge = controller.bridge();
    ensure_started(controller).await?;
    if bridge.cancel_requested.get() {
        return Ok(());
    }
    let generation = bridge.generation.get();
    let started = prepare_turn(controller, blocks, origin).await;
    // The turn may have ended meanwhile (stopped, or the agent died): nothing to report.
    if bridge.generation.get() != generation {
        if let Ok(prepared) = started {
            keep_feedback(controller, prepared.feedback);
        }
        return Ok(());
    }
    let prepared = started?;
    if bridge.open_turn() != Some(prepared.turn) {
        keep_feedback(controller, prepared.feedback);
        return Ok(());
    }
    if bridge.cancel_requested.get() {
        keep_feedback(controller, prepared.feedback);
        end_turn_locally(controller, StopReason::Cancelled);
        return Ok(());
    }
    if bridge.stale_turn_ends.get() > 0 {
        hold(controller, prepared);
        return Ok(());
    }
    send_prompt(controller, prepared.blocks, prepared.feedback)
}

/// Sends the running turn's prompt.
fn send_prompt(
    controller: &AgentController,
    blocks: Vec<PromptBlock>,
    feedback: Option<String>,
) -> Result<(), String> {
    let bridge = controller.bridge();
    if !bridge.send(AgentCmd::Prompt(blocks)) {
        keep_feedback(controller, feedback);
        end_turn_locally(controller, StopReason::Cancelled);
        return Err(format!("{} is not running", label_of(controller)));
    }
    if let Some(mut open) = bridge.turn.get() {
        open.sent = true;
        bridge.turn.set(Some(open));
    }
    Ok(())
}

/// Holds the prompt of `prepared` until the agent finished the turns it still owes a
/// `TurnEnded` for (see the module docs).
fn hold(controller: &Rc<AgentController>, prepared: Prepared) {
    let bridge = controller.bridge();
    tracing::info!(
        "turn {}: waiting for the agent to finish the turn it was asked to cancel",
        prepared.turn
    );
    let turn = prepared.turn;
    bridge.held.replace(Some(HeldPrompt {
        turn,
        blocks: prepared.blocks,
        feedback: prepared.feedback,
    }));
    Bridge::clear_timer(&bridge.held_timer);
    let weak = Rc::downgrade(controller);
    let id = glib::timeout_add_local_once(HELD_PROMPT_TIMEOUT, move || {
        let Some(controller) = weak.upgrade() else {
            return;
        };
        let bridge = controller.bridge();
        bridge.held_timer.take();
        if !bridge
            .held
            .borrow()
            .as_ref()
            .is_some_and(|h| h.turn == turn)
        {
            return;
        }
        tracing::warn!("the agent did not finish its cancelled turn; stopping it");
        stop(&controller);
        let name = label_of(&controller);
        controller.emit_event(&AgentEvent::Error(format!(
            "{name} did not finish stopping the previous request within {} seconds, so it was \
             stopped and your message was not sent. Send it again to start {name} anew.",
            HELD_PROMPT_TIMEOUT.as_secs()
        )));
    });
    bridge.held_timer.replace(Some(id));
}

/// Sends the held prompt, if its turn still runs.
fn release_held(controller: &AgentController) {
    let bridge = controller.bridge();
    Bridge::clear_timer(&bridge.held_timer);
    let Some(held) = bridge.held.take() else {
        return;
    };
    if bridge.open_turn() != Some(held.turn) {
        keep_feedback(controller, held.feedback);
        return;
    }
    tracing::info!("turn {}: the agent is ready; sending the prompt", held.turn);
    if let Err(message) = send_prompt(controller, held.blocks, held.feedback) {
        controller.emit_event(&AgentEvent::Error(message));
    }
}

/// The turn-start steps of §4.2.1 (see the module docs) without sending anything: numbers the
/// turn and sets `Running`, saves, snapshots, writes local history, tells the review UI and the
/// `connect_prompt` callbacks, and returns the turn and the blocks to send. [`prompt`] sends
/// them; tests use it to open a turn without an agent process (end it with
/// [`handle_event`]`(TurnEnded)`).
pub async fn start_turn(
    controller: &Rc<AgentController>,
    blocks: Vec<PromptBlock>,
    origin: PromptOrigin,
) -> Result<(u32, Vec<PromptBlock>), String> {
    prepare_turn(controller, blocks, origin)
        .await
        .map(|p| (p.turn, p.blocks))
}

async fn prepare_turn(
    controller: &Rc<AgentController>,
    blocks: Vec<PromptBlock>,
    origin: PromptOrigin,
) -> Result<Prepared, String> {
    let state = controller.window().ok_or(WINDOW_GONE)?;
    let bridge = controller.bridge();
    let turn = controller.begin_turn();
    bridge.turn.set(Some(OpenTurn {
        id: turn,
        sent: false,
        changed: false,
        unbuilt: false,
    }));
    bridge.last_turn_end.set(None);
    Bridge::clear_timer(&bridge.snapshot_timer);
    controller.set_state(AgentState::Running { turn });
    let still_open = || bridge.open_turn() == Some(turn);

    // 1. Save, so the agent's own Read and Grep, latexmk and git see what the user sees. Only
    //    project files: the agent cannot touch the others, and the user did not ask to save them.
    if state.config.agent.save_before_prompt {
        let root = state.workspace.root().path().to_path_buf();
        let saves: Vec<_> = state
            .workspace
            .dirty_documents()
            .into_iter()
            .filter(|d| d.path().starts_with(&root))
            .map(|d| (d.path().to_path_buf(), d.save()))
            .collect();
        let mut failures = Vec::new();
        for (path, save) in saves {
            if let Err(e) = save.await {
                failures.push(format!("{}: {e}", review_ui::relative_name(&state, &path)));
            }
        }
        if !failures.is_empty() {
            state.toast(&format!(
                "Could not save before the prompt: {}",
                failures.join(", ")
            ));
        }
    }
    if !still_open() {
        return Err(STOPPED.to_owned());
    }
    // 2. The baselines of files that other programs may change during the turn.
    let cap = usize::try_from(state.config.agent.snapshot_cap_mib)
        .unwrap_or(usize::MAX / 2)
        .saturating_mul(1 << 20);
    let snapshot = state
        .workspace
        .turn_snapshot_with(cap, tools::output_dirs(&state))
        .await;
    if !still_open() {
        return Err(STOPPED.to_owned());
    }
    let documents = Rc::new(document_texts(&state, &snapshot, cap));
    let snapshot = Rc::new(snapshot);
    bridge
        .snapshot
        .replace(Some((turn, snapshot.clone(), documents)));
    watcher::watch_documents(&state);
    // 3. Local history of every project file that changed since its last entry.
    history::record_turn(&state, turn, &snapshot);
    // 4. The review groups this turn's hunks.
    review_ui::on_turn_started(&state, turn);
    controller.emit_prompt(&SentPrompt {
        turn,
        origin,
        blocks: blocks.clone(),
    });
    // 5. Review feedback first, then the user's blocks, then where the user is.
    let mut out = Vec::with_capacity(blocks.len() + 2);
    let feedback = tools::take_feedback(&state);
    if let Some(feedback) = &feedback {
        out.push(PromptBlock::Text(feedback.clone()));
    }
    out.extend(blocks);
    if let Some(resource) = tools::editor_resource(&state) {
        out.push(resource);
    }
    Ok(Prepared {
        turn,
        blocks: out,
        feedback,
    })
}

/// The text of every saved project source in the workspace (open, or closed with a review), as
/// long as they fit in what `cap` leaves after the snapshot: the baseline of such a file if it
/// leaves the workspace during the turn (its tab is closed) and another program then changes it.
fn document_texts(
    state: &WindowState,
    snapshot: &TurnSnapshot,
    cap: usize,
) -> HashMap<PathBuf, String> {
    let root = state.workspace.root().path().to_path_buf();
    let mut used: usize = snapshot.files.values().map(String::len).sum();
    let mut texts = HashMap::new();
    for doc in state.workspace.documents() {
        let path = doc.path();
        if !path.starts_with(&root) || !workspace::is_source_file(path) || doc.is_dirty() {
            continue;
        }
        let len = doc.with_text(str::len);
        if used.saturating_add(len) > cap {
            continue;
        }
        used += len;
        texts.insert(path.to_path_buf(), doc.text());
    }
    texts
}

/// Ends the running turn here (no `TurnEnded` came from the agent): the turn-end bookkeeping,
/// then a `TurnEnded(reason)` for the event callbacks.
fn end_turn_locally(controller: &AgentController, reason: StopReason) {
    Bridge::clear_timer(&controller.bridge().cancel_timer);
    if close_turn(controller, reason) {
        controller.emit_event(&AgentEvent::TurnEnded(reason));
    }
}

/// The turn-end bookkeeping (see the module docs). Returns false when no turn was open.
fn close_turn(controller: &AgentController, reason: StopReason) -> bool {
    let bridge = controller.bridge();
    let turn = bridge.turn.take();
    if let Some(turn) = turn {
        bridge.last_turn_end.set(Some(Instant::now()));
        schedule_snapshot_drop(controller, turn.id);
        if let Some(state) = controller.window() {
            review_ui::on_turn_ended(&state, turn.id);
            if turn.changed && turn.unbuilt && state.config.build.on_agent_turn_end {
                state.request_build(Reason::AgentTurnEnd);
            }
            if reason != StopReason::Cancelled
                && state.config.agent.notify_on_turn_end
                && !state.window_focused()
            {
                notify_turn_ended(&state, reason);
            }
        }
    }
    if controller.is_running() {
        controller.set_state(AgentState::Idle);
    }
    turn.is_some()
}

/// Drops the turn's snapshot once the grace period after it is over (unless another turn took
/// a new one), with what the file monitor kept for the turn.
fn schedule_snapshot_drop(controller: &AgentController, turn: u32) {
    let bridge = controller.bridge();
    Bridge::clear_timer(&bridge.snapshot_timer);
    let cell = Rc::downgrade(&bridge.snapshot);
    let slot = Rc::downgrade(&bridge.snapshot_timer);
    let window = controller
        .window()
        .map_or_else(Weak::new, |s| Rc::downgrade(&s));
    let id = glib::timeout_add_local_once(bridge.grace.get(), move || {
        if let Some(slot) = slot.upgrade() {
            slot.take();
        }
        if let Some(cell) = cell.upgrade() {
            let ours = cell.borrow().as_ref().is_some_and(|(t, _, _)| *t == turn);
            if ours {
                cell.replace(None);
                if let Some(state) = window.upgrade() {
                    watcher::turn_over(&state);
                }
            }
        }
    });
    bridge.snapshot_timer.replace(Some(id));
}

fn notify_turn_ended(state: &WindowState, reason: StopReason) {
    let (title, body) = notification_text(state, reason);
    let notification = gio::Notification::new(&title);
    notification.set_body(Some(&body));
    state.send_notification(NOTIFICATION_ID, &notification);
}

/// The title and body of the turn-end notification: the stop reason, and how many changes wait
/// for review in how many files.
pub fn notification_text(state: &WindowState, reason: StopReason) -> (String, String) {
    let name = agent_label(state.config.agent.backend, &state.config.agent);
    let title = match reason {
        StopReason::EndTurn | StopReason::Cancelled => format!("{name} finished"),
        StopReason::MaxTokens => format!("{name} stopped: the reply reached its length limit"),
        StopReason::MaxTurnRequests => format!("{name} stopped: the turn reached its step limit"),
        StopReason::Refusal => format!("{name} declined the request"),
    };
    let (files, hunks) = review_ui::summary(&state.workspace);
    let body = match (hunks, files) {
        (0, _) => "No changes to review".to_owned(),
        (1, _) => "1 change to review".to_owned(),
        (n, 1) => format!("{n} changes to review in 1 file"),
        (n, f) => format!("{n} changes to review in {f} files"),
    };
    (title, body)
}

/// Records that a project file changed in the running turn (our tools after an edit or write,
/// the file monitor after a change by another program).
///
/// The file monitor waits for writes to settle ([`watcher::DEBOUNCE`]), so a program the agent
/// ran just before its turn ended is often reported after the end, within [`AFTER_TURN_GRACE`].
/// Such a change still belongs to the turn: it gets the turn-end build the turn would have
/// asked for (`build.on_agent_turn_end`), [`LATE_BUILD_DELAY`] after the last such change.
/// Nothing happens outside a turn and its grace period.
pub fn note_file_changed(controller: &AgentController) {
    let bridge = controller.bridge();
    if let Some(mut turn) = bridge.turn.get() {
        turn.changed = true;
        turn.unbuilt = true;
        bridge.turn.set(Some(turn));
        return;
    }
    let Some(state) = controller.window() else {
        return;
    };
    if !in_agent_window(controller) || !state.config.build.on_agent_turn_end {
        return;
    }
    Bridge::clear_timer(&bridge.late_build);
    let weak = Rc::downgrade(&state);
    let slot = Rc::downgrade(&bridge.late_build);
    let id = glib::timeout_add_local_once(LATE_BUILD_DELAY, move || {
        if let Some(slot) = slot.upgrade() {
            slot.take();
        }
        if let Some(state) = weak.upgrade() {
            state.request_build(Reason::AgentTurnEnd);
        }
    });
    bridge.late_build.replace(Some(id));
}

/// True while the build for changes reported after the turn's end waits to start (see
/// [`note_file_changed`]).
pub fn late_build_pending(controller: &AgentController) -> bool {
    controller.bridge().late_build.borrow().is_some()
}

/// Records that the agent's `compile` builds everything changed so far in the running turn.
pub fn note_compile(controller: &AgentController) {
    let bridge = controller.bridge();
    if let Some(mut turn) = bridge.turn.get() {
        turn.unbuilt = false;
        bridge.turn.set(Some(turn));
    }
}

/// True during a turn and for the grace period after it: changes by other programs then count
/// as the agent's (§4.2 item 4).
pub fn in_agent_window(controller: &AgentController) -> bool {
    let bridge = controller.bridge();
    bridge.turn.get().is_some()
        || bridge
            .last_turn_end
            .get()
            .is_some_and(|end| end.elapsed() < bridge.grace.get())
}

/// The snapshot of the running or last turn, while it is kept (until the grace period after the
/// turn is over).
pub fn turn_snapshot(controller: &AgentController) -> Option<Rc<TurnSnapshot>> {
    controller
        .bridge()
        .snapshot
        .borrow()
        .as_ref()
        .map(|(_, s, _)| s.clone())
}

/// The text `path` (normalized) had when the running or last turn started, while its snapshot
/// is kept: from the [`TurnSnapshot`] for a file that was not loaded, else the text of the saved
/// document the workspace had then.
pub fn turn_start_text(controller: &AgentController, path: &Path) -> Option<String> {
    let snapshot = controller.bridge().snapshot.borrow();
    let (_, files, documents) = snapshot.as_ref()?;
    files
        .files
        .get(path)
        .or_else(|| documents.get(path))
        .cloned()
}

// ----- commands -----

/// See the module docs: cancels the running turn. While the prompt is still being prepared, it
/// is not sent and the turn ends at once; otherwise `AgentCmd::Cancel` goes out, and the turn
/// is ended here if the agent does not confirm within [`CANCEL_GRACE`].
pub fn cancel(controller: &AgentController) {
    let bridge = controller.bridge();
    if let Some(held) = bridge.held.take() {
        // Never sent: nothing to cancel at the agent.
        Bridge::clear_timer(&bridge.held_timer);
        keep_feedback(controller, held.feedback);
        end_turn_locally(controller, StopReason::Cancelled);
        return;
    }
    if bridge.preparing.get().is_some() && !bridge.turn.get().is_some_and(|t| t.sent) {
        bridge.cancel_requested.set(true);
        return;
    }
    let Some(turn) = bridge.turn.get() else {
        return;
    };
    if !bridge.send(AgentCmd::Cancel) {
        end_turn_locally(controller, StopReason::Cancelled);
        return;
    }
    // The watchdog needs the controller again: find it through the window.
    let Some(state) = controller.window() else {
        return;
    };
    let window = Rc::downgrade(&state);
    Bridge::clear_timer(&bridge.cancel_timer);
    let id = glib::timeout_add_local_once(CANCEL_GRACE, move || {
        let Some(controller) = window.upgrade().and_then(|s| s.agent_controller().cloned()) else {
            return;
        };
        let bridge = controller.bridge();
        bridge.cancel_timer.take();
        if bridge.open_turn() == Some(turn.id) {
            tracing::warn!(
                "the agent did not confirm the cancel of turn {}; ending it",
                turn.id
            );
            // The agent still owes this turn's `TurnEnded` (see the module docs).
            if bridge.turn.get().is_some_and(|t| t.sent) {
                bridge.stale_turn_ends.set(bridge.stale_turn_ends.get() + 1);
            }
            end_turn_locally(&controller, StopReason::Cancelled);
        }
    });
    bridge.cancel_timer.replace(Some(id));
}

/// See the module docs: answers permission request `request` (`None` = cancelled).
pub fn permission(controller: &AgentController, request: u64, option_id: Option<String>) {
    if !controller
        .bridge()
        .send(AgentCmd::Permission { request, option_id })
    {
        tracing::warn!("permission answer for request {request} dropped: no agent");
    }
}

/// See the module docs: only `default` and `plan` are accepted (risk §9.6).
pub fn set_mode(controller: &AgentController, mode: &str) {
    let flavor = controller
        .bridge()
        .session
        .borrow()
        .as_ref()
        .map_or(lemma_agent::AcpFlavor::Claude, |s| s.flavor);
    if !flavor.offers_mode(mode) {
        tracing::warn!("mode {mode:?} refused: Lemma does not offer it");
        return;
    }
    if !controller.bridge().send(AgentCmd::SetMode(mode.to_owned())) {
        tracing::warn!("mode {mode:?} not set: no agent");
    }
}

/// See the module docs: sets the session config option `id` to `value`.
pub fn set_config(controller: &AgentController, id: &str, value: &str) {
    let sent = controller.bridge().send(AgentCmd::SetConfig {
        id: id.to_owned(),
        value: value.to_owned(),
    });
    if !sent {
        tracing::warn!("config option {id} = {value:?} not set: no agent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_carry_the_process() {
        assert_eq!(socket_pid("mcp-1234-7.sock"), Some(1234));
        assert_eq!(socket_pid("mcp-1234.sock"), None);
        assert_eq!(socket_pid("mcp-x-7.sock"), None);
        assert_eq!(socket_pid("mcp-1234-7.socket"), None);
        assert_eq!(socket_pid("page-2.png"), None);
    }

    #[cfg(unix)]
    #[test]
    fn stale_sockets_of_dead_processes_are_removed() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("lemma-sockets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A process that has ended.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let socket = |name: &str| {
            drop(UnixListener::bind(dir.join(name)).unwrap());
            dir.join(name)
        };
        // Another live process (another Lemma instance).
        let mut running = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let stale = socket(&format!("mcp-{dead}-3.sock"));
        let ours = socket(&format!("mcp-{}-1.sock", std::process::id()));
        let live = socket(&format!("mcp-{}-5.sock", running.id()));
        let other = socket("other-1-2.sock");
        // Named like a socket, but a plain file: not ours to remove.
        let plain = dir.join(format!("mcp-{dead}-4.sock"));
        std::fs::write(&plain, "").unwrap();
        remove_stale_sockets(&dir);
        running.kill().unwrap();
        running.wait().unwrap();
        assert!(!stale.exists(), "the dead process's socket goes");
        assert!(live.exists(), "a live process's socket stays");
        assert!(ours.exists() && other.exists() && plain.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
