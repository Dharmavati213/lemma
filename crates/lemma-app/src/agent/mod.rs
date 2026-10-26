//! The agent (M2; ARCHITECTURE §4.2, §10.4): Claude over ACP, editing through our MCP tools.
//!
//! One [`AgentController`] per window, stored in `state.agent.controller` (the window's
//! `agent` field has held the agent *pane* since M0, so the controller lives next to its
//! `split` and `slot`; [`WindowState::agent_controller`] returns it). The controller is the
//! one API the rest of the app uses; its generic parts (state, turn counter, callback lists)
//! are implemented here, and every operation that talks to the agent process is delegated to
//! [`bridge`] (WP2.4), whose functions keep the signatures documented there.
//!
//! | Module | Owner | Role |
//! |---|---|---|
//! | `bridge` | WP2.4 | process lifecycle, turn bookkeeping, event pump, tool dispatch |
//! | `tools` | WP2.4 | main-thread handlers of the MCP tools (`EditorRequest`) |
//! | `watcher` | WP2.4 | edits that bypass our tools (`gio::FileMonitor`, §4.2 item 4) |
//! | `history` | WP2.4 | local history (QoL P1) |
//! | `panel` | WP2.5 | the agent pane (`state.agent.slot`) |
//! | `markdown` | WP2.5 | agent messages → Pango markup |
//! | `install` | WP2.5 | finding node/claude, installing the ACP adapter |
//! | `inline` | WP2.5 | Ctrl+K inline edit |
//! | `quick` | WP2.5 | quick actions (prompt templates) and "Ask agent to fix" |
//! | `attach` | — | images and files attached to a prompt (button, drop, paste) |
//! | `linked_files` | — | files and folders linked to the agent for every prompt of a project |
//!
//! The shell calls, once per window and in this order, after the M1 install hooks:
//! [`review_persist::install`](crate::review_persist::install), [`AgentController::new`] (stored
//! in `state.agent.controller`), then [`panel::install`], [`inline::install`],
//! [`quick::install`], [`watcher::install`] and [`history::install`]. Each may assume the
//! controller exists. When the window leaves the application, and when the controller is
//! dropped, [`AgentController::stop`] / `bridge::stop` runs.
//!
//! [`WindowState::agent_controller`]: crate::window::WindowState::agent_controller

pub mod attach;
pub mod bridge;
pub mod history;
pub mod inline;
pub mod install;
pub mod linked_files;
pub mod markdown;
pub mod panel;
pub mod quick;
pub mod tools;
pub mod watcher;

use crate::hooks::{HookId, Hooks};
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

pub use lemma_agent::{AgentEvent, PromptBlock, StopReason};

/// Identifies a callback connected with an `AgentController::connect_*` method.
pub type SignalId = HookId;

/// What the agent is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// No agent process (never started, stopped, or exited).
    Stopped,
    /// [`AgentController::ensure_started`] is resolving the toolchain, starting the MCP server and
    /// the agent, and waiting for its `Ready`.
    Starting,
    /// Ready for a prompt.
    Idle,
    /// Turn `turn` is running (from [`AgentController::prompt`] to its `TurnEnded`).
    Running { turn: u32 },
    /// Starting failed or the agent died; the message is for the user. A later
    /// `ensure_started` tries again.
    Failed(String),
}

/// Where a prompt comes from; decides how the bridge decorates it and how the UI shows the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptOrigin {
    /// Typed in the agent panel (or `:Agent <prompt>`).
    Panel,
    /// Ctrl+K inline edit of `range` (UTF-8 byte offsets in the text at the time of the prompt)
    /// of the file at `path` (absolute, normalized).
    Inline { path: PathBuf, range: Range<usize> },
    /// A quick action; the string is the template's name.
    QuickAction(String),
    /// "Ask agent to fix" on a build diagnostic.
    FixDiagnostic,
}

/// A prompt whose turn has started, as announced to [`AgentController::connect_prompt`]
/// callbacks: the blocks given to [`AgentController::prompt`] (the user's text and any resources
/// the caller attached), without the decorations the bridge adds (review feedback, the current
/// file as a resource).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentPrompt {
    pub turn: u32,
    pub origin: PromptOrigin,
    pub blocks: Vec<PromptBlock>,
}

/// The agent of one window (see the module docs).
pub struct AgentController {
    window: Weak<WindowState>,
    state: RefCell<AgentState>,
    /// The current (while running) or last turn; 0 before the first.
    turn: Cell<u32>,
    /// The `lemma-fake-agent` scenario given with `--agent-scenario`.
    scenario: RefCell<Option<PathBuf>>,
    events: Hooks<dyn Fn(&AgentEvent)>,
    states: Hooks<dyn Fn(&AgentState)>,
    prompts: Hooks<dyn Fn(&SentPrompt)>,
    bridge: bridge::Bridge,
    /// The files and folders linked to the agent in the window's project.
    linked_files: Rc<linked_files::LinkedFiles>,
    /// The application's `window-removed` handler that stops the agent with its window.
    removed_handler: RefCell<Option<(glib::WeakRef<adw::Application>, glib::SignalHandlerId)>>,
}

impl AgentController {
    /// The controller of `state`'s window, `Stopped`, with no agent process. It stops the agent
    /// when the window leaves the application (closed or destroyed) and when it is dropped. The
    /// shell stores it in `state.agent.controller`.
    pub fn new(state: &Rc<WindowState>) -> Rc<Self> {
        // Kept like the review state: not for windows without `paths` or runs that quit by
        // themselves.
        let dir = state
            .paths
            .as_ref()
            .filter(|_| !state.scripted)
            .map(lemma_config::Paths::linked_files_dir);
        let linked_files = linked_files::LinkedFiles::new(dir, state.workspace.root().path());
        let weak = Rc::downgrade(&linked_files);
        state.connect_root_changed(move |state| {
            if let Some(linked) = weak.upgrade() {
                linked.set_root(state.workspace.root().path());
            }
        });
        let controller = Rc::new(Self {
            window: Rc::downgrade(state),
            state: RefCell::new(AgentState::Stopped),
            turn: Cell::new(0),
            scenario: RefCell::new(None),
            events: Hooks::default(),
            states: Hooks::default(),
            prompts: Hooks::default(),
            bridge: bridge::Bridge::default(),
            linked_files,
            removed_handler: RefCell::new(None),
        });
        let (weak, window) = (Rc::downgrade(&controller), state.window.downgrade());
        let handler = state.app.connect_window_removed(move |_, removed| {
            let ours = window
                .upgrade()
                .is_some_and(|w| w.upcast_ref::<gtk::Window>() == removed);
            if ours && let Some(controller) = weak.upgrade() {
                controller.stop();
            }
        });
        controller
            .removed_handler
            .replace(Some((state.app.downgrade(), handler)));
        controller
    }

    /// The window, while it exists.
    pub fn window(&self) -> Option<Rc<WindowState>> {
        self.window.upgrade()
    }

    /// The bridge's private state (WP2.4).
    pub fn bridge(&self) -> &bridge::Bridge {
        &self.bridge
    }

    /// The files and folders linked to the agent in the window's project.
    pub fn linked_files(&self) -> &Rc<linked_files::LinkedFiles> {
        &self.linked_files
    }

    // ----- operations (delegated to the bridge) -----

    /// Starts the agent unless it runs: resolves when it is `Idle` (or already running a turn),
    /// with an error message for the user otherwise (the state is then `Failed`). Concurrent
    /// calls share one start. The panel calls it when it is first shown.
    pub fn ensure_started(self: &Rc<Self>) -> impl Future<Output = Result<(), String>> + use<> {
        bridge::ensure_started(self)
    }

    /// Starts a turn with `blocks` (the user's text first) on behalf of `origin`: the bridge
    /// starts the agent if needed, runs the turn-start steps of §4.2.1 (save, snapshot,
    /// history, `review_ui::on_turn_started`), announces the prompt to the
    /// [`connect_prompt`](Self::connect_prompt) callbacks and sends it. A prompt while a turn
    /// runs is refused with an `AgentEvent::Error`.
    pub fn prompt(self: &Rc<Self>, blocks: Vec<PromptBlock>, origin: PromptOrigin) {
        bridge::prompt(self, blocks, origin);
    }

    /// Cancels the running turn (pending permission requests are answered as cancelled); the turn
    /// then ends with `TurnEnded(Cancelled)`. Nothing happens without a running turn.
    pub fn cancel(&self) {
        bridge::cancel(self);
    }

    /// Answers permission request `request` with the option `option_id` (`None` = cancelled).
    pub fn permission(&self, request: u64, option_id: Option<String>) {
        bridge::permission(self, request, option_id);
    }

    /// Switches the session mode (`"default"` or `"plan"`; the UI never offers others). The
    /// agent confirms with `AgentEvent::ModeChanged`.
    pub fn set_mode(&self, mode: &str) {
        bridge::set_mode(self, mode);
    }

    /// Sets the session config option `id` (`"model"`, `"effort"`, …) to `value`. The agent
    /// confirms with `AgentEvent::ConfigChanged`.
    pub fn set_config(&self, id: &str, value: &str) {
        bridge::set_config(self, id, value);
    }

    /// Stops the agent process (cancelling a running turn) and its MCP server, freeing their
    /// memory; the state becomes `Stopped`. Safe to call in any state, also during start-up.
    pub fn stop(&self) {
        bridge::stop(self);
    }

    // ----- state -----

    /// What the agent is doing.
    pub fn state(&self) -> AgentState {
        self.state.borrow().clone()
    }

    /// True while a turn runs.
    pub fn is_running(&self) -> bool {
        matches!(*self.state.borrow(), AgentState::Running { .. })
    }

    /// The running turn's number, else the last turn's; 0 before the first turn. Turns are
    /// numbered from 1 per window.
    pub fn turn_id(&self) -> u32 {
        self.turn.get()
    }

    /// The `lemma-fake-agent` scenario set by `--agent-scenario` (absolute path), if any. With
    /// one, the bridge runs the fake agent (see [`crate::testmode::fake_agent_binary`]) with the
    /// scenario instead of node and the ACP adapter.
    pub fn scenario(&self) -> Option<PathBuf> {
        self.scenario.borrow().clone()
    }

    /// Sets the `--agent-scenario` file (takes effect at the next start).
    pub fn set_scenario(&self, scenario: Option<PathBuf>) {
        self.scenario.replace(scenario);
    }

    // ----- callbacks -----

    /// Calls `f` for every event from the agent, in the order the agent sent them, after the
    /// bridge's own bookkeeping for that event (e.g. a `TurnEnded` has already run
    /// `review_ui::on_turn_ended`).
    pub fn connect_event(&self, f: impl Fn(&AgentEvent) + 'static) -> SignalId {
        self.events.add(Rc::new(f))
    }

    /// Calls `f` after every change of [`state`](Self::state), with the new state.
    pub fn connect_state_changed(&self, f: impl Fn(&AgentState) + 'static) -> SignalId {
        self.states.add(Rc::new(f))
    }

    /// Calls `f` when a turn starts, with the prompt as given to [`prompt`](Self::prompt), from
    /// any origin (panel, inline edit, quick action, diagnostic): the panel shows it as the
    /// user's message.
    pub fn connect_prompt(&self, f: impl Fn(&SentPrompt) + 'static) -> SignalId {
        self.prompts.add(Rc::new(f))
    }

    /// Removes a callback added by any `connect_*` method.
    pub fn disconnect(&self, id: SignalId) {
        let _ = self.events.remove(id) || self.states.remove(id) || self.prompts.remove(id);
    }

    // ----- for the bridge (and tests that drive the UI without an agent process) -----

    /// Delivers `event` to the [`connect_event`](Self::connect_event) callbacks.
    pub fn emit_event(&self, event: &AgentEvent) {
        for f in self.events.snapshot() {
            f(event);
        }
    }

    /// Sets the state and tells the [`connect_state_changed`](Self::connect_state_changed)
    /// callbacks (only when it changed).
    pub fn set_state(&self, state: AgentState) {
        if *self.state.borrow() == state {
            return;
        }
        self.state.replace(state.clone());
        for f in self.states.snapshot() {
            f(&state);
        }
    }

    /// Numbers a new turn: returns the next turn id, which [`turn_id`](Self::turn_id) reports
    /// from now on.
    pub fn begin_turn(&self) -> u32 {
        let turn = self.turn.get() + 1;
        self.turn.set(turn);
        turn
    }

    /// Delivers `prompt` to the [`connect_prompt`](Self::connect_prompt) callbacks.
    pub fn emit_prompt(&self, prompt: &SentPrompt) {
        for f in self.prompts.snapshot() {
            f(prompt);
        }
    }
}

impl Drop for AgentController {
    fn drop(&mut self) {
        if let Some((app, handler)) = self.removed_handler.take()
            && let Some(app) = app.upgrade()
        {
            app.disconnect(handler);
        }
        // The agent process never outlives its controller.
        bridge::stop(self);
    }
}

impl std::fmt::Debug for AgentController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentController")
            .field("state", &self.state.borrow())
            .field("turn", &self.turn.get())
            .finish_non_exhaustive()
    }
}
