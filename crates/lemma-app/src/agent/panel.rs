//! The agent pane (WP2.5): transcript, prompt box, permission cards, mode/model/effort pickers,
//! the context-usage ring, and the set-up card that starts Claude, explains why it could not
//! start and installs the ACP adapter.
//!
//! # Layout
//!
//! ```text
//! ┌ Claude · idle                         (ring) [Stop] [⋮] ┐  header; Stop only while running
//! │ [Default ▾] [Opus ▾] [High ▾]                           │  pickers (after Ready)
//! ├ set-up card: "Starting Claude…" / why it failed + Install / Retry, npm output ┤
//! ├ "Run `claude update`" hint (dismissable)                                       ┤
//! │ transcript: a virtualised gtk::ListView of turn items                          │
//! ├ / and @ suggestions                                                            ┤
//! │ [Selection · main.tex 12–14]                                                   │
//! │ ┌ prompt (Enter sends, Shift+Enter newline) ┐ [➤]                              │
//! └ [Quick actions ▾]                                                              ┘
//! ```
//!
//! # Contract
//!
//! - [`install()`] (once per window, after the controller exists) puts the panel into
//!   `state.agent.slot` and adds `win.agent-focus` (Ctrl+L; Vim `<Space>a`: show the pane and
//!   focus the prompt box, or the pending permission card) and `win.agent-stop`
//!   (Ctrl+Shift+Backspace: cancel the running turn, or drop a prompt that waits for the agent
//!   to start; [`is_busy`] covers both, and no second prompt is sent meanwhile). The pane opens
//!   with `win.toggle-agent`
//!   (F10); each time it is shown while the agent is stopped (and the user did not stop it from
//!   the menu) it calls `AgentController::ensure_started` and shows "Starting Claude…".
//! - The transcript follows `AgentController::connect_prompt`, `connect_event` and
//!   `connect_state_changed`, so prompts from inline edits, quick actions and the Problems list
//!   show up too. Every prompt Lemma sends goes through [`send_prompt`].
//! - Streaming chunks append to the last item in O(1); the rows re-render at most once per frame
//!   (a tick callback), and only the Markdown blocks that changed are touched. An agent message
//!   is a list row per Markdown block, so GtkListView (which keeps about 200 rows alive) bounds
//!   the widgets of a long session; a code block or an open thought is a column of labels of a
//!   few lines each, so a frame lays out its last lines only.
//! - Permission cards: buttons per option, and `y` (first `allow_once`), `Shift+Y`
//!   (`allow_always`; Caps Lock alone never makes `y` one), `n` (`reject_once`) while the focus
//!   is in the pane but not in the prompt box. Options that would switch the session to
//!   `bypassPermissions`, `auto` or `acceptEdits` (the adapter's plan-exit variants) are never
//!   offered (ARCHITECTURE §9.6); neither are those modes in the mode picker. A card appearing
//!   while the prompt box has the focus and is empty takes the focus (the card itself, never a
//!   button that Space or Enter would press; and only if the box is still empty when the card
//!   is on screen), so `y`/`n` answer it, after [`AUTO_FOCUS_GRACE`]. A request shows the pane,
//!   except in a narrow window while the user types in the editor (or another text field): the
//!   overlay would take the keyboard focus, so a toast with an Answer button (Ctrl+L) tells
//!   about it instead.
//! - Tool cards have a name and icon for Lemma's MCP tools and the CLI's built-in ones
//!   ([`tool_display`]); the CLI's `ToolSearch` (loading the deferred MCP tools) is a subdued
//!   "Loading tools" row.
//! - The "N changes to review" entry after a turn follows the review (the latest one only; it
//!   counts what the banner and the header count).
//! - **Backends** (WP5.2): a dropdown in the header picks `agent.backend` (Claude Code, the
//!   Anthropic API, Pi); the choice is written to `config.toml` (or kept for this window when the
//!   file cannot be written). Switching stops the running agent (asking first while a turn runs)
//!   and the chosen one starts with the next prompt. A backend that `config.toml` names after a
//!   reload (another Lemma window's choice, a hand edit) never stops a session that is up here:
//!   it applies once that session is gone. Names in the header, the set-up card, the prompt box
//!   and the messages follow the backend. For the API backend a key row (looked up once the pane
//!   is first shown) shows where the key comes from ("Key:
//!   from GNOME keyring") with **Set API key…**, whose dialog stores the key through
//!   `lemma_agent::keys::store` (never in `config.toml`); the header shows the cost of the turn
//!   and the session (`AgentEvent::Cost`). For Pi, the set-up card installs `@openma/pi-acp`
//!   ([`install::install_pi_adapter`]), and an "authentication required" answer
//!   ([`lemma_agent::needs_login`]) shows the hint to run `pi` once to log in.
//! - Tool cards for `edit`/`write` link to the hunk the call made (the review's hunks before the
//!   call and after it completes are compared); clicking one shows it. A `compile` card shows
//!   the Problems page.

use super::bridge::ToolRecord;
use super::markdown::{self, Block};
use super::{AgentController, AgentState, PromptBlock, PromptOrigin, SentPrompt, install};
use crate::review_ui;
use crate::window::WindowState;
use adw::prelude::*;
use adw::subclass::prelude::*;
use lemma_agent::{AgentEvent, ConfigOpt, PermOption, StopReason, ToolStatus};
use lemma_config::AgentBackend;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Session modes Lemma offers (risk §9.6: never `acceptEdits`, `auto` or `bypassPermissions`).
pub const OFFERED_MODES: &[&str] = &["default", "plan"];
/// Permission options that would switch the session to a mode Lemma does not offer.
const HIDDEN_OPTIONS: &[&str] = &[
    "exit-plan-bypass",
    "exit-plan-auto",
    "exit-plan-accept-edits",
    "exit-plan-clear-auto",
    "exit-plan-clear-bypass",
    "exit-plan-clear-accept-edits",
];
/// Attachments (`@file`) larger than this are not sent.
const MAX_ATTACHMENT: usize = 512 * 1024;
/// Files offered after `@`.
const MAX_FILES: usize = 2000;
/// Suggestions shown at once.
const MAX_SUGGESTIONS: usize = 8;
/// Lines of npm output kept in the set-up card.
const MAX_LOG_LINES: usize = 400;
/// How long a permission card that took the focus by itself (from the empty prompt box) does
/// not take `y`, `Y` or `n`: keys typed in that moment were meant for the prompt box.
pub const AUTO_FOCUS_GRACE: Duration = Duration::from_millis(500);
/// Extensions of the files offered after `@` (text files a LaTeX project has).
const TEXT_EXTENSIONS: &[&str] = &[
    "tex", "ltx", "sty", "cls", "bib", "bst", "dtx", "ins", "cfg", "def", "bbx", "cbx", "lbx",
    "tikz", "txt", "md", "csv", "dat", "lua", "py", "sh", "toml", "yaml", "yml", "json", "mk",
];

// ----- public API -----

/// What the panel (and the inline edit and quick actions, through [`send_prompt`]) asks the
/// agent to do; reported to the hook of [`set_command_hook`] before it is passed on to the
/// controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelCommand {
    /// `ensure_started` (the pane was shown, Retry, after an install).
    Start,
    Prompt {
        blocks: Vec<PromptBlock>,
        origin: PromptOrigin,
    },
    Cancel,
    Permission {
        request: u64,
        option_id: Option<String>,
    },
    SetMode(String),
    SetConfig {
        id: String,
        value: String,
    },
    /// "Stop Agent (Free Memory)".
    Stop,
    /// "New Session".
    NewSession,
    /// The backend picker chose another backend (WP5.2): the running agent stops.
    SwitchBackend(AgentBackend),
}

/// A test hook that sees the panel's commands first: it returns `true` when it handled one
/// itself (the controller then never sees it; tests emulate the agent that way), `false` to
/// let it through.
pub type CommandHook = dyn Fn(&PanelCommand) -> bool;

/// One transcript item's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A prompt: its turn, origin, user-visible text and attachment names.
    User {
        turn: u32,
        origin: PromptOrigin,
        /// The origin as shown ("You", "Inline edit · main.tex:12–14", …).
        heading: String,
        text: String,
        attachments: Vec<String>,
    },
    /// The agent's Markdown, as streamed so far.
    Agent {
        text: String,
    },
    /// The agent's reasoning; `done` once something else followed.
    Thought {
        text: String,
        done: bool,
    },
    Tool(ToolCard),
    /// The plan as (content, status) pairs.
    Plan(Vec<(String, String)>),
    Permission(PermissionCard),
    Notice(String),
    Error(String),
    /// After a turn that left changes to review: files and hunks pending now (the latest
    /// summary follows the review; 0 once everything was decided).
    Summary {
        files: usize,
        hunks: usize,
    },
}

/// A tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCard {
    pub id: String,
    /// The title the agent sent (`mcp__lemma__edit`, …).
    pub title: String,
    pub kind: String,
    pub status: ToolStatus,
    /// Where an edit tool's change is.
    pub link: Option<ToolLink>,
}

/// The hunk an edit tool made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolLink {
    pub path: PathBuf,
    /// 0-based line of the hunk when the call completed.
    pub line: u32,
    /// The hunk's id then (0 when unknown).
    pub hunk: u64,
    /// `main.tex:12`.
    pub label: String,
}

/// What tells a hunk's change apart: where it is and how large it is on both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HunkShape {
    line: u32,
    base: std::ops::Range<usize>,
    cur_len: usize,
}

/// A permission request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionCard {
    pub request: u64,
    pub title: String,
    /// The options offered (see the module docs for the ones that are hidden).
    pub options: Vec<PermOption>,
    pub answer: PermissionAnswer,
}

/// What became of a permission request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionAnswer {
    Pending,
    /// The user chose this option (id).
    Chosen(String),
    /// Cancelled (by Stop or the turn's end).
    Cancelled,
}

/// The agent pane of a window (a cheap handle).
#[derive(Clone)]
pub struct Panel {
    inner: Rc<Inner>,
}

/// The panel of `state`'s window.
pub fn get(state: &WindowState) -> Option<Panel> {
    let widget = state.agent.slot.child().and_downcast::<PanelWidget>()?;
    let inner = widget.imp().inner.borrow().clone()?;
    Some(Panel { inner })
}

/// Fills `state.agent.slot` (see the module docs).
pub fn install(state: &Rc<WindowState>) {
    let Some(controller) = state.agent_controller().cloned() else {
        tracing::warn!("agent panel: no controller");
        return;
    };
    install_css();
    let inner = Inner::new(state, &controller);
    let widget = PanelWidget::new(&inner);
    state.agent.slot.set_child(Some(&widget));
    inner.connect(state, &controller);
    add_actions(state);
}

/// Shows the agent pane and focuses its prompt box (or the pending permission card).
pub fn focus_prompt(state: &Rc<WindowState>) {
    state.agent.split.set_show_sidebar(true);
    if let Some(panel) = get(state) {
        panel.inner.focus_best();
    }
}

/// Sends a prompt on behalf of `origin` (the panel's own box, inline edits, quick actions,
/// "Ask agent to fix"). Refused with a toast while the agent is busy ([`is_busy`]: a turn runs,
/// or a prompt waits for the agent to start); returns whether it was sent.
pub fn send_prompt(
    state: &Rc<WindowState>,
    blocks: Vec<PromptBlock>,
    origin: PromptOrigin,
) -> bool {
    let Some(controller) = state.agent_controller().cloned() else {
        return false;
    };
    if is_busy(&controller) {
        state.toast(&format!(
            "{} is still working on the last request; wait for it or press Stop",
            super::bridge::agent_label(state.config.agent.backend)
        ));
        return false;
    }
    let panel = get(state);
    if let Some(panel) = &panel {
        panel.inner.user_stopped.set(false);
        let handled = !panel.inner.command(&PanelCommand::Prompt {
            blocks: blocks.clone(),
            origin: origin.clone(),
        });
        if handled {
            return true;
        }
    }
    controller.prompt(blocks, origin);
    // Stop shows while the prompt waits.
    if let Some(panel) = panel {
        panel.inner.refresh_header();
    }
    true
}

/// True while the agent is busy with a prompt: a turn runs, or a prompt is being prepared (the
/// agent starting for it, the save and the snapshot), which Stop and Esc cancel too and which no
/// other prompt may overtake.
pub fn is_busy(controller: &AgentController) -> bool {
    controller.is_running() || controller.bridge().is_preparing()
}

/// Shows every [`PanelCommand`] to `hook` first (tests; see [`CommandHook`]); `None` removes
/// it.
pub fn set_command_hook(state: &WindowState, hook: Option<Box<CommandHook>>) {
    if let Some(panel) = get(state) {
        panel.inner.command_hook.replace(hook);
    }
}

impl Panel {
    /// The root widget.
    pub fn widget(&self) -> gtk::Widget {
        self.inner
            .root
            .upgrade()
            .map(|w| w.upcast())
            .unwrap_or_else(|| self.inner.content.clone().upcast())
    }

    /// The transcript's entries, in order.
    pub fn entries(&self) -> Vec<Entry> {
        let store = &self.inner.store;
        (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<TranscriptItem>())
            .map(|item| item.entry())
            .collect()
    }

    /// Applies pending streaming updates now (they otherwise wait for the next frame).
    pub fn flush(&self) {
        self.inner.flush();
    }

    /// The text of every transcript entry on screen, in order: the texts of its rows' labels
    /// joined with newlines (an agent message has a row per Markdown block).
    pub fn row_texts(&self) -> Vec<String> {
        let mut rows: Vec<(u32, u32, String)> = descendants(&self.inner.list)
            .into_iter()
            .filter_map(|w| w.downcast::<TranscriptRow>().ok())
            .filter_map(|row| {
                let item = row.imp().item.borrow().clone()?;
                let position = self.inner.position_of(&item.owner()?)?;
                let row_position = self.inner.rows.find(&item)?;
                Some((position, row_position, widget_text(row.upcast_ref())))
            })
            .collect();
        rows.sort_by_key(|(p, r, _)| (*p, *r));
        rows.dedup_by_key(|(_, r, _)| *r);
        let mut entries: Vec<(u32, String)> = Vec::new();
        for (position, _, text) in rows {
            match entries.last_mut() {
                Some((p, joined)) if *p == position => {
                    if !text.is_empty() {
                        if !joined.is_empty() {
                            joined.push('\n');
                        }
                        joined.push_str(&text);
                    }
                }
                _ => entries.push((position, text)),
            }
        }
        entries.into_iter().map(|(_, t)| t).collect()
    }

    /// What a key press does in the pane (the panel's key controller calls it; tests too): `y`,
    /// `Y`, `n` answer the pending permission request unless the prompt box has the focus.
    pub fn key_pressed(
        &self,
        keyval: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        self.inner.key_pressed(keyval, state)
    }

    /// What a key press in the prompt box does (its key controller calls it; tests too): Enter
    /// sends, Shift+Enter is a newline, and while suggestions show, Up/Down choose, Tab/Enter
    /// accept and Esc closes them.
    pub fn prompt_key(
        &self,
        keyval: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        self.inner.prompt_key(keyval, state)
    }

    /// Answers permission request `request` with `option_id` (`None` = cancelled), as its
    /// buttons do.
    pub fn answer_permission(&self, request: u64, option_id: Option<String>) {
        self.inner.answer_permission(request, option_id);
    }

    /// The buttons of permission request `request`'s card while it is on screen.
    pub fn permission_buttons(&self, request: u64) -> Vec<gtk::Button> {
        descendants(&self.inner.list)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .filter(|b| b.widget_name() == format!("permission-{request}"))
            .collect()
    }

    /// Clicks the card of tool call `id` (jumps to its hunk, or shows the Problems page).
    pub fn activate_tool(&self, id: &str) {
        self.inner.activate_tool(id);
    }

    pub fn mode_picker(&self) -> gtk::DropDown {
        self.inner.mode_picker.clone()
    }

    pub fn model_picker(&self) -> gtk::DropDown {
        self.inner.model_picker.clone()
    }

    pub fn effort_picker(&self) -> gtk::DropDown {
        self.inner.effort_picker.clone()
    }

    /// The last `Usage` (used, size).
    pub fn usage(&self) -> Option<(u64, u64)> {
        self.inner.usage.get()
    }

    /// The context-usage ring.
    pub fn usage_ring(&self) -> gtk::DrawingArea {
        self.inner.ring.clone()
    }

    /// "Claude · idle" and so on.
    pub fn status_text(&self) -> String {
        self.inner.status.text().to_string()
    }

    pub fn stop_button(&self) -> gtk::Button {
        self.inner.stop_button.clone()
    }

    pub fn send_button(&self) -> gtk::Button {
        self.inner.send_button.clone()
    }

    /// The prompt box.
    pub fn prompt_view(&self) -> gtk::TextView {
        self.inner.prompt.clone()
    }

    /// Sends the prompt box's text, as Enter does.
    pub fn send(&self) {
        self.inner.send_from_box();
    }

    /// The suggestions shown under the transcript (`/` commands, `@` files) as their labels.
    pub fn suggestions(&self) -> Vec<String> {
        self.inner
            .suggestions
            .borrow()
            .iter()
            .map(|s| s.label.clone())
            .collect()
    }

    /// The selection chip's text while it is shown.
    pub fn selection_chip(&self) -> Option<String> {
        self.inner
            .chip
            .is_visible()
            .then(|| self.inner.chip_label.text().to_string())
    }

    /// The set-up card: (title, message) while it is shown.
    pub fn setup(&self) -> Option<(String, String)> {
        self.inner.setup.revealer.reveals_child().then(|| {
            (
                self.inner.setup.title.text().to_string(),
                self.inner.setup.message.text().to_string(),
            )
        })
    }

    /// The set-up card's Install button.
    pub fn install_button(&self) -> gtk::Button {
        self.inner.setup.install.clone()
    }

    /// The set-up card's Retry button.
    pub fn retry_button(&self) -> gtk::Button {
        self.inner.setup.retry.clone()
    }

    /// npm's output as shown in the set-up card.
    pub fn install_log(&self) -> String {
        let buffer = self.inner.setup.log.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// True while an install runs.
    pub fn installing(&self) -> bool {
        self.inner.install_cancel.borrow().is_some()
    }

    /// The "Run `claude update`" hint while it is shown.
    /// The backend picker in the header (WP5.2).
    pub fn backend_picker(&self) -> gtk::DropDown {
        self.inner.backend_ui.picker.clone()
    }

    /// The API key row's text ("Key: from GNOME keyring", "No API key", …), `None` while the row
    /// is hidden (another backend).
    pub fn key_status(&self) -> Option<String> {
        let ui = &self.inner.backend_ui;
        ui.key_row
            .is_visible()
            .then(|| ui.key_label.text().to_string())
    }

    /// The API key row's "Set API key…" button.
    pub fn key_button(&self) -> gtk::Button {
        self.inner.backend_ui.key_button.clone()
    }

    /// The open "Set API key…" dialog, if any.
    pub fn key_dialog(&self) -> Option<adw::AlertDialog> {
        self.inner.backend_ui.key_dialog.borrow().clone()
    }

    /// The open "Switch backend?" confirmation, if any.
    pub fn switch_dialog(&self) -> Option<adw::AlertDialog> {
        self.inner.backend_ui.switch_dialog.borrow().clone()
    }

    /// The header's cost label (`None` while hidden): turn cost, with the session's in the
    /// tooltip.
    pub fn cost(&self) -> Option<(String, String)> {
        let label = &self.inner.backend_ui.cost_label;
        label.is_visible().then(|| {
            (
                label.text().to_string(),
                label.tooltip_text().unwrap_or_default().to_string(),
            )
        })
    }

    pub fn update_hint(&self) -> Option<String> {
        self.inner
            .hint
            .reveals_child()
            .then(|| self.inner.hint_label.text().to_string())
    }

    /// The quick-actions menu model (filled by `quick::install`).
    pub fn quick_menu(&self) -> gio::Menu {
        self.inner.quick_menu.clone()
    }

    /// The panel's own actions (`agentpanel.new-session`, `.shutdown`, `.clear`, `.start`).
    pub fn actions(&self) -> gio::SimpleActionGroup {
        self.inner.actions.clone()
    }

    /// Shows `command` to the command hook (for code that acts on the controller directly);
    /// false when the hook handled it, so the caller must not pass it on.
    pub fn command(&self, command: &PanelCommand) -> bool {
        self.inner.command(command)
    }

    /// Shows the transcript's end.
    pub fn scroll_to_end(&self) {
        self.inner.scroll_to_end();
    }
}

// ----- the transcript model -----
//
// The transcript is two list models: `Inner::store` holds one `TranscriptItem` per entry (what
// `Panel::entries` reports), and `Inner::rows`, the list view's model, one `RowItem` per row
// shown: an agent message gets a row per Markdown block, every other entry one row. GtkListView
// keeps about 200 rows alive whatever is on screen, so a row per block (not per message) is what
// bounds the widgets of a long session.

mod item_imp {
    use super::{Entry, RowItem};
    use crate::agent::markdown::Incremental;
    use gtk::glib;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, RefCell};

    pub struct TranscriptItem {
        pub entry: RefCell<Entry>,
        pub dirty: Cell<bool>,
        /// The rows that show the entry: one per Markdown block of an agent message (none while
        /// it has no block), one for any other entry.
        pub(crate) rows: RefCell<Vec<RowItem>>,
        /// An agent message's Markdown, re-parsed incrementally while it streams.
        pub markdown: RefCell<Option<Incremental>>,
    }

    impl Default for TranscriptItem {
        fn default() -> Self {
            Self {
                entry: RefCell::new(Entry::Notice(String::new())),
                dirty: Cell::new(false),
                rows: RefCell::new(Vec::new()),
                markdown: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TranscriptItem {
        const NAME: &'static str = "LemmaTranscriptItem";
        type Type = super::TranscriptItem;
    }

    impl ObjectImpl for TranscriptItem {}
}

glib::wrapper! {
    /// One transcript entry in the list model.
    pub struct TranscriptItem(ObjectSubclass<item_imp::TranscriptItem>);
}

impl TranscriptItem {
    fn new(entry: Entry) -> Self {
        let item: Self = glib::Object::new();
        item.imp().entry.replace(entry);
        item
    }

    /// A copy of the entry.
    pub fn entry(&self) -> Entry {
        self.imp().entry.borrow().clone()
    }

    fn with_entry<R>(&self, f: impl FnOnce(&mut Entry) -> R) -> R {
        f(&mut self.imp().entry.borrow_mut())
    }

    fn is_agent(&self) -> bool {
        matches!(*self.imp().entry.borrow(), Entry::Agent { .. })
    }

    /// Updates the bound rows of a (non-agent) entry that changed.
    fn notify_changed(&self) {
        let rows = self.imp().rows.borrow().clone();
        for row in rows {
            row.notify_changed();
        }
    }
}

mod row_item_imp {
    use super::TranscriptItem;
    use crate::agent::markdown::Block;
    use gtk::glib;
    use gtk::subclass::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    pub(crate) struct RowItem {
        /// The entry the row belongs to.
        pub(crate) owner: glib::WeakRef<TranscriptItem>,
        /// The Markdown block the row shows (agent messages); `None` for a whole entry.
        pub(crate) block: RefCell<Option<Block>>,
        /// The bound row's update callback.
        pub(crate) observer: RefCell<Option<Rc<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowItem {
        const NAME: &'static str = "LemmaTranscriptRowItem";
        type Type = super::RowItem;
    }

    impl ObjectImpl for RowItem {}
}

glib::wrapper! {
    /// One row of the transcript's list view (see "the transcript model").
    pub(crate) struct RowItem(ObjectSubclass<row_item_imp::RowItem>);
}

impl RowItem {
    fn new(owner: &TranscriptItem, block: Option<Block>) -> Self {
        let row: Self = glib::Object::new();
        row.imp().owner.set(Some(owner));
        row.imp().block.replace(block);
        row
    }

    fn owner(&self) -> Option<TranscriptItem> {
        self.imp().owner.upgrade()
    }

    /// Shows `block` from now on (the bound row updates when it changed).
    fn set_block(&self, block: Block) {
        if self.imp().block.borrow().as_ref() == Some(&block) {
            return;
        }
        self.imp().block.replace(Some(block));
        self.notify_changed();
    }

    fn notify_changed(&self) {
        let observer = self.imp().observer.borrow().clone();
        if let Some(f) = observer {
            f();
        }
    }
}

// ----- the root widget -----

mod widget_imp {
    use super::Inner;
    use adw::subclass::prelude::*;
    use gtk::glib;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    pub struct PanelWidget {
        pub(in crate::agent::panel) inner: RefCell<Option<Rc<Inner>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PanelWidget {
        const NAME: &'static str = "LemmaAgentPanel";
        type Type = super::PanelWidget;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for PanelWidget {
        fn dispose(&self) {
            // The panel state holds widgets of this tree; let both go.
            if let Some(inner) = self.inner.take() {
                inner.dispose();
            }
        }
    }
    impl WidgetImpl for PanelWidget {}
    impl BinImpl for PanelWidget {}
}

glib::wrapper! {
    /// The pane's root widget; it owns the panel state.
    pub struct PanelWidget(ObjectSubclass<widget_imp::PanelWidget>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PanelWidget {
    fn new(inner: &Rc<Inner>) -> Self {
        let widget: Self = glib::Object::new();
        widget.add_css_class("lemma-agent-panel");
        widget.set_child(Some(&inner.content));
        widget.imp().inner.replace(Some(inner.clone()));
        inner.root.set(Some(&widget));
        widget.insert_action_group("agentpanel", Some(&inner.actions));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(inner);
        keys.connect_key_pressed(move |_, keyval, _, state| match weak.upgrade() {
            Some(inner) => inner.key_pressed(keyval, state),
            None => glib::Propagation::Proceed,
        });
        widget.add_controller(keys);
        widget
    }
}

// ----- rows -----

mod row_imp {
    use super::{Inner, RowItem, RowView};
    use adw::subclass::prelude::*;
    use gtk::glib;
    use std::cell::RefCell;
    use std::rc::Weak;

    #[derive(Default)]
    pub struct TranscriptRow {
        pub(in crate::agent::panel) item: RefCell<Option<RowItem>>,
        pub(in crate::agent::panel) view: RefCell<Option<RowView>>,
        pub(in crate::agent::panel) panel: RefCell<Weak<Inner>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TranscriptRow {
        const NAME: &'static str = "LemmaTranscriptRow";
        type Type = super::TranscriptRow;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for TranscriptRow {}
    impl WidgetImpl for TranscriptRow {}
    impl BinImpl for TranscriptRow {}
}

glib::wrapper! {
    /// A transcript row (recycled by the list view for any row item).
    pub struct TranscriptRow(ObjectSubclass<row_imp::TranscriptRow>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// The widgets of a bound row that updates in place.
enum RowView {
    /// A Markdown block of an agent message, and the block it shows.
    Block { widget: BlockWidget, block: Block },
    /// The agent's reasoning: its text goes into the labels only while the expander is open.
    Thought {
        expander: gtk::Expander,
        chunks: Chunks,
    },
    /// Rebuilt on every change (they change rarely).
    Other,
}

/// A rendered Markdown block: its root widget and the labels an update rewrites.
struct BlockWidget {
    root: gtk::Widget,
    labels: Vec<gtk::Label>,
    /// A code block's text in chunks, and the whole text for its copy button.
    code: Option<(Chunks, Rc<RefCell<String>>)>,
}

/// Lines per label of a code block or a thought (see [`Chunks`]).
const CHUNK_LINES: usize = 16;
/// Bytes after which a chunk ends at the next line break.
const CHUNK_BYTES: usize = 1500;
/// The narrowest a code label gets, in characters: a wrapping label measures its smallest
/// layout at this width, and at one character a line that costs a layout line per character.
const CODE_MIN_CHARS: i32 = 20;

/// Text in a column of labels of at most [`CHUNK_LINES`] lines each: while it grows, only the
/// last label changes, so a frame lays out that label alone, not the whole text (a 20 kB code
/// block in one wrapping label costs a full re-layout every frame).
struct Chunks {
    container: gtk::Box,
    /// The labels with the text each shows.
    labels: Vec<(gtk::Label, String)>,
    make: fn() -> gtk::Label,
}

impl Chunks {
    fn new(make: fn() -> gtk::Label) -> Self {
        Self {
            container: gtk::Box::new(gtk::Orientation::Vertical, 0),
            labels: Vec::new(),
            make,
        }
    }

    /// Shows `text`, touching only the labels whose part changed.
    fn set_text(&mut self, text: &str) {
        let parts = chunk_text(text, CHUNK_LINES, CHUNK_BYTES);
        for (i, part) in parts.iter().enumerate() {
            if let Some((label, shown)) = self.labels.get_mut(i) {
                if shown != part {
                    label.set_text(part);
                    *shown = (*part).to_owned();
                }
            } else {
                let label = (self.make)();
                label.set_text(part);
                self.container.append(&label);
                self.labels.push((label, (*part).to_owned()));
            }
        }
        while self.labels.len() > parts.len() {
            if let Some((label, _)) = self.labels.pop() {
                self.container.remove(&label);
            }
        }
    }
}

/// `text` cut into parts of at most `max_lines` lines, a part also ending once it holds
/// `max_bytes` (at the next line break); the line break between two parts is not in either. A
/// part's end depends only on the lines before it, so a growing text only changes its last part.
fn chunk_text(text: &str, max_lines: usize, max_bytes: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut offset, mut lines, mut bytes) = (0, 0, 0, 0);
    for line in text.split_inclusive('\n') {
        if lines > 0 && (lines >= max_lines || bytes >= max_bytes) {
            let part = &text[start..offset];
            parts.push(part.strip_suffix('\n').unwrap_or(part));
            start = offset;
            lines = 0;
            bytes = 0;
        }
        lines += 1;
        bytes += line.len();
        offset += line.len();
    }
    parts.push(&text[start..]);
    parts
}

impl TranscriptRow {
    fn new(panel: Weak<Inner>) -> Self {
        let row: Self = glib::Object::new();
        row.add_css_class("lemma-agent-row");
        row.imp().panel.replace(panel);
        row
    }

    fn bind(&self, item: &RowItem) {
        self.unbind();
        self.imp().item.replace(Some(item.clone()));
        let weak = self.downgrade();
        let observer: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(row) = weak.upgrade() {
                row.update();
            }
        });
        item.imp().observer.replace(Some(observer));
        self.imp().view.replace(None);
        if item.imp().block.borrow().is_some() {
            self.add_css_class("lemma-agent-message");
        } else {
            self.remove_css_class("lemma-agent-message");
        }
        self.update();
    }

    fn unbind(&self) {
        if let Some(item) = self.imp().item.take() {
            item.imp().observer.replace(None);
        }
    }

    fn update(&self) {
        let Some(item) = self.imp().item.borrow().clone() else {
            return;
        };
        let panel = self.imp().panel.borrow().clone();
        let mut view = self.imp().view.borrow_mut();
        // A block of an agent message.
        if let Some(block) = item.imp().block.borrow().as_ref() {
            if let Some(RowView::Block {
                widget,
                block: shown,
            }) = view.as_mut()
            {
                if *shown == *block {
                    return;
                }
                if update_block_widget(widget, shown, block) {
                    *shown = block.clone();
                    return;
                }
            }
            let widget = block_widget(block, &panel);
            self.set_child(Some(&widget.root));
            *view = Some(RowView::Block {
                widget,
                block: block.clone(),
            });
            return;
        }
        let Some(owner) = item.owner() else {
            return;
        };
        let entry = owner.imp().entry.borrow();
        match (&*entry, view.as_mut()) {
            (Entry::Thought { text, done }, Some(RowView::Thought { expander, chunks })) => {
                expander.set_label(Some(if *done { "Thought" } else { "Thinking…" }));
                if expander.is_expanded() {
                    chunks.set_text(text.trim());
                }
            }
            (Entry::Thought { done, .. }, _) => {
                let chunks = Chunks::new(thought_label);
                let expander = gtk::Expander::builder()
                    .label(if *done { "Thought" } else { "Thinking…" })
                    .child(&chunks.container)
                    .expanded(false)
                    .build();
                expander.add_css_class("lemma-agent-thought-expander");
                // Opening it fills the labels (a closed one is not updated while it streams).
                let weak = self.downgrade();
                expander.connect_expanded_notify(move |_| {
                    if let Some(row) = weak.upgrade() {
                        row.update();
                    }
                });
                self.set_child(Some(&expander));
                *view = Some(RowView::Thought { expander, chunks });
            }
            (entry, _) => {
                let widget = build_row(entry, &panel);
                self.set_child(Some(&widget));
                *view = Some(RowView::Other);
            }
        }
    }
}

/// A selectable, wrapping, left-aligned label.
fn wrapped_label(text: &str, markup: bool) -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Fill)
        .hexpand(true)
        .selectable(true)
        .width_chars(1)
        .build();
    if markup {
        set_markup(&label, text);
    } else {
        label.set_text(&markdown::without_nul(text));
    }
    label
}

/// A label of a thought's text.
fn thought_label() -> gtk::Label {
    let label = wrapped_label("", false);
    label.add_css_class("dim-label");
    label.add_css_class("lemma-agent-thought");
    label
}

/// A label of a code block's text.
fn code_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::Char)
        .xalign(0.0)
        .selectable(true)
        .width_chars(CODE_MIN_CHARS)
        .build();
    label.add_css_class("monospace");
    label.add_css_class("lemma-md-code-text");
    label
}

/// Sets `markup` on `label`, or its plain text when it is not valid markup (a label would log a
/// warning).
fn set_markup(label: &gtk::Label, markup: &str) {
    if markdown::is_valid_markup(markup) {
        label.set_markup(markup);
    } else {
        label.set_text(&markdown::plain_text(markup));
    }
}

/// Updates `widget` (showing `old`) to show `new` in place; false when it cannot.
fn update_block_widget(widget: &mut BlockWidget, old: &Block, new: &Block) -> bool {
    match (old, new) {
        (Block::Markup(_), Block::Markup(m)) | (Block::Quote(_), Block::Quote(m)) => {
            set_markup(&widget.labels[0], m);
            true
        }
        (Block::Code { .. }, Block::Code { lang, text }) => {
            widget.labels[0].set_text(lang.as_deref().unwrap_or(""));
            widget.labels[0].set_visible(lang.is_some());
            if let Some((chunks, full)) = widget.code.as_mut() {
                chunks.set_text(text);
                full.replace(text.clone());
            }
            true
        }
        (
            Block::Item { depth: d0, .. },
            Block::Item {
                depth,
                marker,
                markup,
            },
        ) if d0 == depth => {
            widget.labels[0].set_text(marker);
            set_markup(&widget.labels[1], markup);
            true
        }
        _ => false,
    }
}

fn block_widget(block: &Block, panel: &Weak<Inner>) -> BlockWidget {
    match block {
        Block::Markup(markup) => {
            let label = wrapped_label(markup, true);
            connect_links(&label, panel);
            BlockWidget {
                root: label.clone().upcast(),
                labels: vec![label],
                code: None,
            }
        }
        Block::Quote(markup) => {
            let label = wrapped_label(markup, true);
            connect_links(&label, panel);
            let quote = gtk::Box::new(gtk::Orientation::Vertical, 0);
            quote.add_css_class("lemma-md-quote");
            quote.append(&label);
            BlockWidget {
                root: quote.upcast(),
                labels: vec![label],
                code: None,
            }
        }
        Block::Item {
            depth,
            marker,
            markup,
        } => {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_start(i32::from(*depth) * 16 + 4);
            let bullet = gtk::Label::builder()
                .label(marker)
                .valign(gtk::Align::Start)
                .xalign(1.0)
                .width_chars(2)
                .build();
            bullet.add_css_class("lemma-md-marker");
            let label = wrapped_label(markup, true);
            connect_links(&label, panel);
            row.append(&bullet);
            row.append(&label);
            BlockWidget {
                root: row.upcast(),
                labels: vec![bullet, label],
                code: None,
            }
        }
        Block::Code { lang, text } => {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
            card.add_css_class("lemma-md-code");
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let lang_label = gtk::Label::builder()
                .label(lang.as_deref().unwrap_or(""))
                .xalign(0.0)
                .hexpand(true)
                .visible(lang.is_some())
                .build();
            lang_label.add_css_class("dim-label");
            lang_label.add_css_class("caption");
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .tooltip_text("Copy")
                .valign(gtk::Align::Start)
                .build();
            copy.add_css_class("flat");
            copy.add_css_class("circular");
            copy.add_css_class("lemma-md-copy");
            header.append(&lang_label);
            header.append(&spacer);
            header.append(&copy);
            let mut chunks = Chunks::new(code_label);
            chunks.set_text(text);
            let full = Rc::new(RefCell::new(text.clone()));
            let source = full.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&source.borrow());
            });
            card.append(&header);
            card.append(&chunks.container);
            BlockWidget {
                root: card.upcast(),
                labels: vec![lang_label],
                code: Some((chunks, full)),
            }
        }
        Block::Table { header, rows } => {
            let grid = gtk::Grid::builder()
                .column_spacing(0)
                .row_spacing(0)
                .halign(gtk::Align::Start)
                .build();
            grid.add_css_class("lemma-md-table");
            let mut labels = Vec::new();
            let cell = |markup: &str, bold: bool| {
                let label = gtk::Label::builder()
                    .wrap(true)
                    .wrap_mode(pango::WrapMode::WordChar)
                    .xalign(0.0)
                    .selectable(true)
                    .build();
                if bold {
                    set_markup(&label, &format!("<b>{markup}</b>"));
                } else {
                    set_markup(&label, markup);
                }
                label
            };
            for (c, h) in header.iter().enumerate() {
                let label = cell(h, true);
                grid.attach(&label, c as i32, 0, 1, 1);
                labels.push(label);
            }
            for (r, row) in rows.iter().enumerate() {
                for (c, text) in row.iter().enumerate() {
                    let label = cell(text, false);
                    grid.attach(&label, c as i32, r as i32 + 1, 1, 1);
                    labels.push(label);
                }
            }
            BlockWidget {
                root: grid.upcast(),
                labels,
                code: None,
            }
        }
        Block::Rule => BlockWidget {
            root: gtk::Separator::new(gtk::Orientation::Horizontal).upcast(),
            labels: Vec::new(),
            code: None,
        },
    }
}

/// Links in agent messages: web links open in the browser (on click only); anything else is
/// taken as a project file, optionally with `:line` or `#Lline`.
fn connect_links(label: &gtk::Label, panel: &Weak<Inner>) {
    let panel = panel.clone();
    label.connect_activate_link(move |label, uri| {
        if let Some(inner) = panel.upgrade() {
            inner.open_link(label.upcast_ref(), uri);
        }
        glib::Propagation::Stop
    });
}

/// The widget of a non-streaming entry.
fn build_row(entry: &Entry, panel: &Weak<Inner>) -> gtk::Widget {
    match entry {
        Entry::User {
            origin,
            heading,
            text,
            attachments,
            ..
        } => {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 3);
            card.add_css_class("lemma-agent-user");
            let head = gtk::Label::builder()
                .label(heading.as_str())
                .xalign(0.0)
                .build();
            head.add_css_class("caption-heading");
            head.add_css_class("dim-label");
            card.append(&head);
            match origin {
                PromptOrigin::Panel => card.append(&wrapped_label(text, false)),
                PromptOrigin::Inline { .. } => {
                    let (first, rest) = text.split_once("\n\n").unwrap_or((text, ""));
                    card.append(&wrapped_label(first.trim(), false));
                    if !rest.trim().is_empty() {
                        card.append(&collapsed("Full prompt", rest.trim()));
                    }
                }
                PromptOrigin::QuickAction(_) | PromptOrigin::FixDiagnostic => {
                    card.append(&collapsed("Prompt", text.trim()));
                }
            }
            for name in attachments {
                let label = gtk::Label::builder()
                    .label(format!("📎 {name}"))
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::Middle)
                    .build();
                label.add_css_class("caption");
                label.add_css_class("dim-label");
                card.append(&label);
            }
            card.upcast()
        }
        Entry::Tool(card) => tool_row(card, panel),
        Entry::Plan(entries) => {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
            card.add_css_class("lemma-agent-plan");
            let head = gtk::Label::builder().label("Plan").xalign(0.0).build();
            head.add_css_class("caption-heading");
            card.append(&head);
            for (content, status) in entries {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let icon = match status.as_str() {
                    "completed" => "checkbox-checked-symbolic",
                    "in_progress" => "checkbox-mixed-symbolic",
                    _ => "checkbox-symbolic",
                };
                let image = gtk::Image::from_icon_name(icon);
                image.set_valign(gtk::Align::Start);
                let label = wrapped_label(content, false);
                if status == "completed" {
                    label.add_css_class("dim-label");
                }
                if status == "in_progress" {
                    label.add_css_class("heading");
                }
                row.append(&image);
                row.append(&label);
                card.append(&row);
            }
            card.upcast()
        }
        Entry::Permission(card) => permission_row(card, panel),
        Entry::Notice(text) => {
            notice_row("dialog-information-symbolic", text, "lemma-agent-notice")
        }
        Entry::Error(text) => notice_row("dialog-error-symbolic", text, "lemma-agent-error"),
        Entry::Summary { files, hunks } => {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("lemma-agent-summary");
            let label = gtk::Label::builder()
                .label(summary_text(*files, *hunks))
                .xalign(0.0)
                .hexpand(true)
                .wrap(true)
                .build();
            label.add_css_class("dim-label");
            row.append(&label);
            if *hunks == 0 {
                return row.upcast();
            }
            let button = gtk::Button::with_label("Review");
            button.add_css_class("flat");
            let panel = panel.clone();
            button.connect_clicked(move |_| {
                if let Some(state) = panel.upgrade().and_then(|p| p.state.upgrade()) {
                    review_ui::show_changes_panel(&state);
                }
            });
            row.append(&button);
            row.upcast()
        }
        Entry::Agent { .. } | Entry::Thought { .. } => {
            gtk::Box::new(gtk::Orientation::Vertical, 0).upcast()
        }
    }
}

/// The summary row's text: what is left to review now (the row follows the review).
fn summary_text(files: usize, hunks: usize) -> String {
    if hunks == 0 {
        return "No changes left to review".to_owned();
    }
    let changes = if hunks == 1 { "change" } else { "changes" };
    let in_files = if files == 1 { "file" } else { "files" };
    format!("{hunks} {changes} in {files} {in_files} to review")
}

fn collapsed(title: &str, text: &str) -> gtk::Expander {
    let label = wrapped_label(text, false);
    label.add_css_class("dim-label");
    gtk::Expander::builder()
        .label(title)
        .child(&label)
        .expanded(false)
        .build()
}

fn notice_row(icon: &str, text: &str, class: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class(class);
    let image = gtk::Image::from_icon_name(icon);
    image.set_valign(gtk::Align::Start);
    let label = wrapped_label(text, false);
    row.append(&image);
    row.append(&label);
    row.upcast()
}

/// Lemma's MCP tools (`mcp__lemma__<tool>`): the name and icon their cards show.
const LEMMA_TOOLS: &[(&str, &str, &str)] = &[
    ("read_file", "Read file", "document-open-symbolic"),
    ("edit", "Edit", "document-edit-symbolic"),
    ("write", "Write file", "document-new-symbolic"),
    ("list_files", "List files", "folder-open-symbolic"),
    ("outline", "Outline", "view-list-bullet-symbolic"),
    ("compile", "Compile", "media-playback-start-symbolic"),
    (
        "get_diagnostics",
        "Read diagnostics",
        "dialog-warning-symbolic",
    ),
    ("get_selection", "Read selection", "edit-select-symbolic"),
    (
        "render_page",
        "Look at a PDF page",
        "image-x-generic-symbolic",
    ),
    (
        "get_review_state",
        "Read review state",
        "view-reveal-symbolic",
    ),
];

/// The Claude CLI's built-in tools, as the adapter may report them by their bare name: the
/// name and icon their cards show.
const BUILTIN_TOOLS: &[(&str, &str, &str)] = &[
    ("ToolSearch", "Loading tools", "system-search-symbolic"),
    ("Bash", "Run a command", "utilities-terminal-symbolic"),
    (
        "BashOutput",
        "Read a command's output",
        "utilities-terminal-symbolic",
    ),
    ("KillShell", "Stop a command", "process-stop-symbolic"),
    ("KillBash", "Stop a command", "process-stop-symbolic"),
    ("Read", "Read a file", "document-open-symbolic"),
    ("Write", "Write a file", "document-new-symbolic"),
    ("Edit", "Edit a file", "document-edit-symbolic"),
    ("MultiEdit", "Edit a file", "document-edit-symbolic"),
    ("NotebookEdit", "Edit a notebook", "document-edit-symbolic"),
    ("NotebookRead", "Read a notebook", "document-open-symbolic"),
    ("Grep", "Search in files", "edit-find-symbolic"),
    ("Glob", "Find files", "edit-find-symbolic"),
    ("LS", "List a folder", "folder-open-symbolic"),
    ("WebFetch", "Fetch a web page", "web-browser-symbolic"),
    ("WebSearch", "Search the web", "system-search-symbolic"),
    (
        "TodoWrite",
        "Update the to-do list",
        "checkbox-checked-symbolic",
    ),
    ("Task", "Run a subagent", "network-workgroup-symbolic"),
    ("Agent", "Run a subagent", "network-workgroup-symbolic"),
    (
        "ExitPlanMode",
        "Finish planning",
        "view-list-bullet-symbolic",
    ),
    (
        "SlashCommand",
        "Run a slash command",
        "utilities-terminal-symbolic",
    ),
    ("Skill", "Use a skill", "applications-science-symbolic"),
];

/// The display name and icon of a tool call: Lemma's MCP tools (`mcp__lemma__edit` → "Edit"),
/// the CLI's built-in tools reported by their bare name (`ToolSearch` → "Loading tools", `Bash`
/// → "Run a command"), other MCP tools (`mcp__server__tool` → "server: tool"), and the titles the
/// adapter writes itself ("`latexmk -pdf`", "Read /path/main.tex") as they are, with the icon of
/// their kind.
pub fn tool_display(title: &str, kind: &str) -> (String, &'static str) {
    let title = markdown::without_nul(title);
    let known = match title.strip_prefix("mcp__lemma__") {
        Some(tool) => LEMMA_TOOLS.iter().find(|(id, _, _)| *id == tool),
        None => BUILTIN_TOOLS.iter().find(|(id, _, _)| *id == title),
    };
    if let Some((_, name, icon)) = known {
        return ((*name).to_owned(), icon);
    }
    let name = match title.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", ": ", 1),
        None => title.into_owned(),
    };
    let icon = match kind {
        "read" => "document-open-symbolic",
        "edit" => "document-edit-symbolic",
        "delete" => "edit-delete-symbolic",
        "move" => "go-jump-symbolic",
        "search" => "edit-find-symbolic",
        "execute" => "utilities-terminal-symbolic",
        "think" => "dialog-question-symbolic",
        "fetch" => "image-x-generic-symbolic",
        "switch_mode" => "view-list-bullet-symbolic",
        _ => "system-run-symbolic",
    };
    (name, icon)
}

/// True for the CLI's bookkeeping tools that are shown as a subdued row, not a card
/// (`ToolSearch`: the CLI loading the deferred MCP tools at the start of a turn).
pub fn is_quiet_tool(title: &str) -> bool {
    title == "ToolSearch"
}

/// True for tools that change files (their cards link to the hunk).
fn is_edit_tool(title: &str, kind: &str) -> bool {
    matches!(title, "mcp__lemma__edit" | "mcp__lemma__write") || kind == "edit"
}

/// The subdued row of a bookkeeping tool ([`is_quiet_tool`]).
fn quiet_tool_row(card: &ToolCard) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("lemma-tool-quiet");
    row.set_widget_name(&format!("tool-{}", card.id));
    let (status, text): (gtk::Widget, &str) = match card.status {
        ToolStatus::Pending | ToolStatus::InProgress => {
            (adw::Spinner::new().upcast(), "Loading tools…")
        }
        ToolStatus::Completed => (
            gtk::Image::from_icon_name("object-select-symbolic").upcast(),
            "Loaded tools",
        ),
        ToolStatus::Failed => (
            gtk::Image::from_icon_name("window-close-symbolic").upcast(),
            "Could not load tools",
        ),
    };
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    row.append(&status);
    row.append(&label);
    row.upcast()
}

fn tool_row(card: &ToolCard, panel: &Weak<Inner>) -> gtk::Widget {
    if is_quiet_tool(&card.title) {
        return quiet_tool_row(card);
    }
    let (name, icon) = tool_display(&card.title, &card.kind);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&gtk::Image::from_icon_name(icon));
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_hexpand(true);
    let title = gtk::Label::builder()
        .label(name.as_str())
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .build();
    text.append(&title);
    if let Some(link) = &card.link {
        let sub = gtk::Label::builder()
            .label(link.label.as_str())
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        sub.add_css_class("caption");
        sub.add_css_class("lemma-tool-link");
        text.append(&sub);
    }
    row.append(&text);
    let status: gtk::Widget = match card.status {
        ToolStatus::Pending | ToolStatus::InProgress => adw::Spinner::new().upcast(),
        ToolStatus::Completed => gtk::Image::from_icon_name("object-select-symbolic").upcast(),
        ToolStatus::Failed => {
            let image = gtk::Image::from_icon_name("window-close-symbolic");
            image.add_css_class("error");
            image.upcast()
        }
    };
    status.set_tooltip_text(Some(match card.status {
        ToolStatus::Pending => "Pending",
        ToolStatus::InProgress => "Running",
        ToolStatus::Completed => "Done",
        ToolStatus::Failed => "Failed",
    }));
    row.append(&status);
    let linkable = card.link.is_some()
        || (card.title == "mcp__lemma__compile" && card.status != ToolStatus::Pending);
    if !linkable {
        row.add_css_class("lemma-tool-card");
        row.set_widget_name(&format!("tool-{}", card.id));
        return row.upcast();
    }
    let button = gtk::Button::builder().child(&row).build();
    button.add_css_class("flat");
    button.add_css_class("lemma-tool-card");
    button.set_widget_name(&format!("tool-{}", card.id));
    button.set_tooltip_text(Some(&match &card.link {
        Some(link) => format!("Show the change in {}", link.label),
        None => "Show the build's problems".to_owned(),
    }));
    let (panel, id) = (panel.clone(), card.id.clone());
    button.connect_clicked(move |_| {
        if let Some(inner) = panel.upgrade() {
            inner.activate_tool(&id);
        }
    });
    button.upcast()
}

fn permission_row(card: &PermissionCard, panel: &Weak<Inner>) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .accessible_role(gtk::AccessibleRole::Group)
        .build();
    root.add_css_class("lemma-permission");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let title = wrapped_label(&card.title, false);
    title.add_css_class("heading");
    head.append(&title);
    root.append(&head);
    match &card.answer {
        PermissionAnswer::Pending => {
            // The card itself takes the focus that moves to it (never one of its buttons, which
            // Space or Enter would press): only y, Y and n answer it.
            root.set_focusable(true);
            root.set_widget_name(&format!("permission-{}", card.request));
            root.update_property(&[gtk::accessible::Property::Label(&format!(
                "Permission request: {}. Press y to allow once, Shift+Y to always allow, n to \
                 reject.",
                markdown::without_nul(&card.title)
            ))]);
            let buttons = gtk::FlowBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .column_spacing(6)
                .row_spacing(6)
                .max_children_per_line(4)
                .homogeneous(false)
                .build();
            let keys = permission_keys(&card.options);
            for option in &card.options {
                let key = keys
                    .iter()
                    .find(|(_, id)| *id == option.id)
                    .map(|(k, _)| *k);
                let label = match key {
                    Some(k) => format!("{} ({k})", option.name),
                    None => option.name.clone(),
                };
                let button = gtk::Button::with_label(&label);
                button.set_widget_name(&format!("permission-{}", card.request));
                if option.kind.starts_with("allow") {
                    button.add_css_class("suggested-action");
                } else if option.kind.starts_with("reject") {
                    button.add_css_class("destructive-action");
                }
                let (panel, request, id) = (panel.clone(), card.request, option.id.clone());
                button.connect_clicked(move |_| {
                    if let Some(inner) = panel.upgrade() {
                        inner.answer_permission(request, Some(id.clone()));
                    }
                });
                buttons.append(&button);
            }
            root.append(&buttons);
        }
        answer => {
            let text = match answer {
                PermissionAnswer::Chosen(id) => {
                    card.options.iter().find(|o| o.id == *id).map_or_else(
                        || format!("Answered: {id}"),
                        |o| format!("Answered: {}", o.name),
                    )
                }
                _ => "Cancelled".to_owned(),
            };
            let label = gtk::Label::builder().label(text).xalign(0.0).build();
            label.add_css_class("dim-label");
            root.append(&label);
        }
    }
    root.upcast()
}

/// The option ids the keys `y`, `Y` and `n` choose (§10.4): the first `allow_once`, the first
/// `allow_always`, the first `reject_once`.
pub fn permission_keys(options: &[PermOption]) -> Vec<(char, String)> {
    [
        ('y', "allow_once"),
        ('Y', "allow_always"),
        ('n', "reject_once"),
    ]
    .into_iter()
    .filter_map(|(key, kind)| {
        options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| (key, o.id.clone()))
    })
    .collect()
}

/// The options a card offers: all but those that leave the modes Lemma offers.
pub fn offered_options(options: &[PermOption]) -> Vec<PermOption> {
    options
        .iter()
        .filter(|o| !HIDDEN_OPTIONS.contains(&o.id.as_str()) && !o.id.contains("bypass"))
        .cloned()
        .collect()
}

// ----- the panel state -----

/// The set-up card's widgets.
struct Setup {
    revealer: gtk::Revealer,
    spinner: adw::Spinner,
    icon: gtk::Image,
    title: gtk::Label,
    message: gtk::Label,
    install: gtk::Button,
    retry: gtk::Button,
    cancel: gtk::Button,
    start: gtk::Button,
    log_expander: gtk::Expander,
    log: gtk::TextView,
}

/// One suggestion under the transcript.
#[derive(Clone, Debug)]
struct Suggestion {
    label: String,
    detail: String,
    /// What replaces the token.
    insert: String,
}

/// The token the suggestions complete: its start offset (chars) in the buffer and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Command,
    File,
}

pub(crate) struct Inner {
    pub(crate) state: Weak<WindowState>,
    controller: Weak<AgentController>,
    root: glib::WeakRef<PanelWidget>,
    content: gtk::Box,
    actions: gio::SimpleActionGroup,
    // header
    status: gtk::Label,
    pickers: gtk::Box,
    mode_picker: gtk::DropDown,
    model_picker: gtk::DropDown,
    effort_picker: gtk::DropDown,
    ring: gtk::DrawingArea,
    ring_label: gtk::Label,
    stop_button: gtk::Button,
    setup: Setup,
    hint: gtk::Revealer,
    hint_label: gtk::Label,
    backend_ui: BackendUi,
    // transcript
    stack: gtk::Stack,
    /// One `TranscriptItem` per entry.
    store: gio::ListStore,
    /// The list view's model: one `RowItem` per row (see "the transcript model").
    rows: gio::ListStore,
    list: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    // prompt area
    suggest_revealer: gtk::Revealer,
    suggest_list: gtk::ListBox,
    chip: gtk::Box,
    chip_label: gtk::Label,
    prompt: gtk::TextView,
    placeholder: gtk::Label,
    send_button: gtk::Button,
    quick_menu: gio::Menu,
    // agent data
    agent_name: RefCell<String>,
    modes: RefCell<Vec<(String, String)>>,
    mode: RefCell<String>,
    options: RefCell<Vec<ConfigOpt>>,
    commands: RefCell<Vec<(String, String)>>,
    usage: Cell<Option<(u64, u64)>>,
    updating: Cell<bool>,
    // transcript data
    stream: RefCell<Option<TranscriptItem>>,
    dirty: RefCell<Vec<TranscriptItem>>,
    flush_scheduled: Cell<bool>,
    tools: RefCell<HashMap<String, TranscriptItem>>,
    permissions: RefCell<Vec<TranscriptItem>>,
    plan: RefCell<Option<TranscriptItem>>,
    /// The latest "N changes to review" entry, which follows the review.
    summary: RefCell<Option<TranscriptItem>>,
    turn: Cell<u32>,
    /// Every hunk at the last edit tool's reply (or the turn's start), with its shape.
    known_hunks: RefCell<HashMap<(PathBuf, u64), HunkShape>>,
    /// The hunks the edit tools changed, one per answered `edit`/`write` call in reply order
    /// (`None` for a failed call or one that changed nothing), waiting for their cards.
    edit_links: RefCell<VecDeque<Option<ToolLink>>>,
    tool_done_id: Cell<Option<crate::hooks::HookId>>,
    /// Edit cards that took their record from `edit_links`.
    linked: RefCell<HashSet<String>>,
    stick: Cell<bool>,
    scrolling: Cell<bool>,
    // prompt data
    pending_prompt: RefCell<Option<String>>,
    /// The widget a permission card moved the focus to by itself, and when (see
    /// [`AUTO_FOCUS_GRACE`]).
    auto_focus: RefCell<Option<(glib::WeakRef<gtk::Widget>, Instant)>>,
    /// The toast that tells about a permission request the pane could not show.
    permission_toast: RefCell<Option<adw::Toast>>,
    suggestions: RefCell<Vec<Suggestion>>,
    token: Cell<Option<(i32, TokenKind)>>,
    files: RefCell<Option<(Instant, Rc<Vec<String>>)>>,
    files_loading: Cell<bool>,
    selection_watch: RefCell<
        Option<(
            glib::WeakRef<sourceview5::Buffer>,
            Vec<glib::SignalHandlerId>,
        )>,
    >,
    // lifecycle
    pub(crate) user_stopped: Cell<bool>,
    starting: Cell<bool>,
    /// Counts start attempts: the result of an attempt that a stop or a newer start overtook is
    /// ignored.
    start_gen: Cell<u64>,
    /// The agent was ready (Idle or Running) since it last began to start.
    session_up: Cell<bool>,
    /// The header is refreshed until a prompt being prepared is sent or dropped.
    header_poll: Cell<bool>,
    hint_dismissed: Cell<bool>,
    install_cancel: RefCell<Option<gio::Cancellable>>,
    command_hook: RefCell<Option<Box<CommandHook>>>,
    signal_ids: RefCell<Vec<super::SignalId>>,
}

impl Inner {
    fn new(state: &Rc<WindowState>, controller: &Rc<AgentController>) -> Rc<Self> {
        // ----- header -----
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        status.add_css_class("lemma-agent-status");
        let ring = gtk::DrawingArea::builder()
            .content_width(18)
            .content_height(18)
            .valign(gtk::Align::Center)
            .visible(false)
            .accessible_role(gtk::AccessibleRole::Meter)
            .build();
        ring.add_css_class("lemma-agent-ring");
        let ring_label = gtk::Label::builder().visible(false).build();
        ring_label.add_css_class("caption");
        ring_label.add_css_class("dim-label");
        ring_label.add_css_class("numeric");
        let stop_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Stop (Ctrl+Shift+Backspace)")
            .action_name("win.agent-stop")
            .visible(false)
            .build();
        stop_button.add_css_class("flat");
        stop_button.add_css_class("destructive-action");
        let menu = gio::Menu::new();
        let quick_menu = gio::Menu::new();
        menu.append_submenu(Some("_Quick Actions"), &quick_menu);
        let session = gio::Menu::new();
        session.append(Some("_New Session"), Some("agentpanel.new-session"));
        session.append(
            Some("_Stop Agent (Free Memory)"),
            Some("agentpanel.shutdown"),
        );
        session.append(Some("_Clear Transcript"), Some("agentpanel.clear"));
        menu.append_section(None, &session);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Agent Menu")
            .menu_model(&menu)
            .build();
        menu_button.add_css_class("flat");
        let backend_ui = build_backend_ui();
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        top.append(&status);
        top.append(&backend_ui.picker);
        top.append(&backend_ui.cost_label);
        top.append(&ring_label);
        top.append(&ring);
        top.append(&stop_button);
        top.append(&menu_button);

        let picker = |tooltip: &str| {
            let dd = gtk::DropDown::from_strings(&[]);
            dd.set_tooltip_text(Some(tooltip));
            dd.add_css_class("flat");
            dd.set_visible(false);
            dd
        };
        let mode_picker = picker("Mode");
        let model_picker = picker("Model");
        let effort_picker = picker("Effort");
        let pickers = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        pickers.append(&mode_picker);
        pickers.append(&model_picker);
        pickers.append(&effort_picker);
        pickers.set_visible(false);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header.add_css_class("lemma-agent-header");
        header.append(&top);
        header.append(&pickers);
        header.append(&backend_ui.key_row);

        // ----- set-up card -----
        let setup = build_setup();
        let hint_label = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .use_markup(true)
            .build();
        let hint_close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Dismiss")
            .valign(gtk::Align::Start)
            .build();
        hint_close.add_css_class("flat");
        hint_close.add_css_class("circular");
        let hint_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        hint_box.add_css_class("lemma-agent-hint");
        hint_box.append(&gtk::Image::from_icon_name(
            "software-update-available-symbolic",
        ));
        hint_box.append(&hint_label);
        hint_box.append(&hint_close);
        let hint = gtk::Revealer::builder()
            .child(&hint_box)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .build();

        // ----- transcript -----
        let store = gio::ListStore::new::<TranscriptItem>();
        let rows = gio::ListStore::new::<RowItem>();
        let factory = gtk::SignalListItemFactory::new();
        let list = gtk::ListView::builder()
            .model(&gtk::NoSelection::new(Some(rows.clone())))
            .factory(&factory)
            .single_click_activate(false)
            .build();
        list.add_css_class("lemma-agent-transcript");
        list.add_css_class("navigation-sidebar");
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("Ask Claude")
            .description(
                "Claude's changes show up as red/green hunks to accept or reject.\n\
                 Type / for commands and @ to attach a file.",
            )
            .build();
        empty.add_css_class("compact");
        empty.add_css_class("dim-label");
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroller, Some("transcript"));
        stack.set_visible_child_name("empty");

        // ----- prompt area -----
        let suggest_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        suggest_list.add_css_class("lemma-agent-suggest");
        suggest_list.add_css_class("boxed-list");
        let suggest_revealer = gtk::Revealer::builder()
            .child(&suggest_list)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();
        let chip_label = gtk::Label::builder()
            .ellipsize(pango::EllipsizeMode::Middle)
            .xalign(0.0)
            .build();
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chip.add_css_class("lemma-agent-chip");
        chip.set_halign(gtk::Align::Start);
        chip.append(&gtk::Image::from_icon_name("edit-select-symbolic"));
        chip.append(&chip_label);
        chip.set_tooltip_text(Some(
            "The selection and the cursor line are sent with your prompt",
        ));
        chip.set_visible(false);
        let prompt = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(4)
            .bottom_margin(4)
            .left_margin(4)
            .right_margin(4)
            .build();
        prompt.add_css_class("lemma-agent-prompt-text");
        // The visible placeholder is a separate label a screen reader does not connect.
        prompt.update_property(&[
            gtk::accessible::Property::Label("Ask Claude"),
            gtk::accessible::Property::Placeholder("Ask Claude…"),
        ]);
        let placeholder = gtk::Label::builder()
            .label("Ask Claude…")
            .xalign(0.0)
            .yalign(0.0)
            .margin_start(6)
            .margin_top(4)
            .can_target(false)
            .build();
        placeholder.add_css_class("dim-label");
        let prompt_scroller = gtk::ScrolledWindow::builder()
            .child(&prompt)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(200)
            .min_content_height(40)
            .hexpand(true)
            .build();
        let prompt_overlay = gtk::Overlay::builder().child(&prompt_scroller).build();
        prompt_overlay.add_overlay(&placeholder);
        prompt_overlay.add_css_class("lemma-agent-prompt");
        let send_button = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Send (Enter; Shift+Enter for a new line)")
            .valign(gtk::Align::End)
            .sensitive(false)
            .build();
        send_button.add_css_class("circular");
        send_button.add_css_class("suggested-action");
        let prompt_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        prompt_row.append(&prompt_overlay);
        prompt_row.append(&send_button);
        let quick_button = gtk::MenuButton::builder()
            .label("Quick Actions")
            .menu_model(&quick_menu)
            .always_show_arrow(true)
            .build();
        quick_button.add_css_class("flat");
        quick_button.add_css_class("caption");
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bottom.append(&quick_button);
        let prompt_area = gtk::Box::new(gtk::Orientation::Vertical, 4);
        prompt_area.add_css_class("lemma-agent-prompt-area");
        prompt_area.append(&suggest_revealer);
        prompt_area.append(&chip);
        prompt_area.append(&prompt_row);
        prompt_area.append(&bottom);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&setup.revealer);
        content.append(&hint);
        content.append(&stack);
        content.append(&prompt_area);

        let inner = Rc::new(Self {
            state: Rc::downgrade(state),
            controller: Rc::downgrade(controller),
            root: glib::WeakRef::new(),
            content,
            actions: gio::SimpleActionGroup::new(),
            status,
            pickers,
            mode_picker,
            model_picker,
            effort_picker,
            ring,
            ring_label,
            stop_button,
            setup,
            hint,
            hint_label,
            backend_ui,
            stack,
            store,
            rows,
            list,
            scroller,
            suggest_revealer,
            suggest_list,
            chip,
            chip_label,
            prompt,
            placeholder,
            send_button,
            quick_menu,
            agent_name: RefCell::new(String::new()),
            modes: RefCell::new(Vec::new()),
            mode: RefCell::new(String::new()),
            options: RefCell::new(Vec::new()),
            commands: RefCell::new(Vec::new()),
            usage: Cell::new(None),
            updating: Cell::new(false),
            stream: RefCell::new(None),
            dirty: RefCell::new(Vec::new()),
            flush_scheduled: Cell::new(false),
            tools: RefCell::new(HashMap::new()),
            permissions: RefCell::new(Vec::new()),
            plan: RefCell::new(None),
            summary: RefCell::new(None),
            turn: Cell::new(0),
            known_hunks: RefCell::new(HashMap::new()),
            edit_links: RefCell::new(VecDeque::new()),
            tool_done_id: Cell::new(None),
            linked: RefCell::new(HashSet::new()),
            stick: Cell::new(true),
            scrolling: Cell::new(false),
            pending_prompt: RefCell::new(None),
            auto_focus: RefCell::new(None),
            permission_toast: RefCell::new(None),
            suggestions: RefCell::new(Vec::new()),
            token: Cell::new(None),
            files: RefCell::new(None),
            files_loading: Cell::new(false),
            selection_watch: RefCell::new(None),
            user_stopped: Cell::new(false),
            starting: Cell::new(false),
            start_gen: Cell::new(0),
            session_up: Cell::new(false),
            header_poll: Cell::new(false),
            hint_dismissed: Cell::new(false),
            install_cancel: RefCell::new(None),
            command_hook: RefCell::new(None),
            signal_ids: RefCell::new(Vec::new()),
        });

        let weak = Rc::downgrade(&inner);
        factory.connect_setup(move |_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            item.set_activatable(false);
            item.set_selectable(false);
            item.set_focusable(false);
            item.set_child(Some(&TranscriptRow::new(weak.clone())));
        });
        factory.connect_bind(|_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let (Some(row), Some(entry)) = (
                item.child().and_downcast::<TranscriptRow>(),
                item.item().and_downcast::<RowItem>(),
            ) {
                row.bind(&entry);
            }
        });
        factory.connect_unbind(|_, obj| {
            if let Some(row) = obj
                .downcast_ref::<gtk::ListItem>()
                .and_then(|i| i.child().and_downcast::<TranscriptRow>())
            {
                row.unbind();
            }
        });

        let weak = Rc::downgrade(&inner);
        inner.ring.set_draw_func(move |area, cr, w, h| {
            if let Some(inner) = weak.upgrade() {
                inner.draw_ring(area, cr, w, h);
            }
        });
        let weak = Rc::downgrade(&inner);
        hint_close.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.hint_dismissed.set(true);
                inner.hint.set_reveal_child(false);
            }
        });
        inner.connect_widgets();
        inner.connect_backend_ui();
        inner.add_panel_actions();
        inner.refresh_header();
        inner
    }

    /// Drops what the widget tree holds on to (the root widget is being disposed).
    fn dispose(&self) {
        if let Some(cancel) = self.install_cancel.take() {
            cancel.cancel();
        }
        if let Some(controller) = self.controller.upgrade() {
            for id in self.signal_ids.take() {
                controller.disconnect(id);
            }
            if let Some(id) = self.tool_done_id.take() {
                controller.bridge().disconnect_tool_done(id);
            }
        }
        self.unwatch_selection();
        for row in self.row_items() {
            row.imp().observer.replace(None);
        }
        if let Some(toast) = self.permission_toast.take() {
            toast.dismiss();
        }
    }

    fn connect(self: &Rc<Self>, state: &Rc<WindowState>, controller: &Rc<AgentController>) {
        let mut ids = Vec::new();
        let weak = Rc::downgrade(self);
        ids.push(controller.connect_event(move |event| {
            if let Some(inner) = weak.upgrade() {
                inner.on_event(event);
            }
        }));
        let weak = Rc::downgrade(self);
        ids.push(controller.connect_state_changed(move |s| {
            if let Some(inner) = weak.upgrade() {
                inner.on_state(s);
            }
        }));
        let weak = Rc::downgrade(self);
        ids.push(controller.connect_prompt(move |prompt| {
            if let Some(inner) = weak.upgrade() {
                inner.on_prompt(prompt);
            }
        }));
        self.signal_ids.replace(ids);
        let weak = Rc::downgrade(self);
        self.tool_done_id
            .set(Some(controller.bridge().connect_tool_done(move |record| {
                if let Some(inner) = weak.upgrade() {
                    inner.on_tool_done(record);
                }
            })));

        let weak = Rc::downgrade(self);
        state.agent.split.connect_show_sidebar_notify(move |split| {
            if split.shows_sidebar()
                && let Some(inner) = weak.upgrade()
            {
                inner.on_shown();
            }
        });
        let weak = Rc::downgrade(self);
        state.tab_view.connect_selected_page_notify(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.watch_selection();
            }
        });
        // The review's summary button changes with every change of what is under review: the
        // "N changes to review" entry follows it.
        if let Some(review) = state.review.get() {
            for property in ["visible", "tooltip-text"] {
                let weak = Rc::downgrade(self);
                review
                    .summary_button()
                    .connect_notify_local(Some(property), move |_, _| {
                        if let Some(inner) = weak.upgrade() {
                            inner.refresh_summary();
                        }
                    });
            }
        }
        if state.agent.split.shows_sidebar() {
            self.on_shown();
        }
    }

    fn connect_widgets(self: &Rc<Self>) {
        // Pickers.
        for (picker, which) in [
            (&self.mode_picker, "mode"),
            (&self.model_picker, "model"),
            (&self.effort_picker, "effort"),
        ] {
            let weak = Rc::downgrade(self);
            picker.connect_selected_notify(move |dd| {
                if let Some(inner) = weak.upgrade()
                    && !inner.updating.get()
                {
                    inner.picker_changed(which, dd.selected());
                }
            });
        }
        // Scrolling: follow the end while the view is at the end.
        let adj = self.scroller.vadjustment();
        let weak = Rc::downgrade(self);
        adj.connect_value_changed(move |adj| {
            if let Some(inner) = weak.upgrade()
                && !inner.scrolling.get()
            {
                inner
                    .stick
                    .set(adj.value() + adj.page_size() >= adj.upper() - 32.0);
            }
        });
        let weak = Rc::downgrade(self);
        adj.connect_upper_notify(move |_| {
            if let Some(inner) = weak.upgrade()
                && inner.stick.get()
            {
                inner.scroll_to_end();
            }
        });
        // The prompt box.
        let buffer = self.prompt.buffer();
        let weak = Rc::downgrade(self);
        buffer.connect_changed(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.prompt_changed();
            }
        });
        let weak = Rc::downgrade(self);
        buffer.connect_mark_set(move |_, _, mark| {
            if mark.name().as_deref() == Some("insert")
                && let Some(inner) = weak.upgrade()
            {
                inner.update_suggestions();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, keyval, _, state| match weak.upgrade() {
            Some(inner) => inner.prompt_key(keyval, state),
            None => glib::Propagation::Proceed,
        });
        self.prompt.add_controller(keys);
        let weak = Rc::downgrade(self);
        self.send_button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.send_from_box();
            }
        });
        let weak = Rc::downgrade(self);
        self.suggest_list.connect_row_activated(move |_, row| {
            if let Some(inner) = weak.upgrade() {
                inner.accept_suggestion(usize::try_from(row.index()).unwrap_or(0));
            }
        });
        // Set-up card.
        let weak = Rc::downgrade(self);
        self.setup.install.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.run_install();
            }
        });
        let weak = Rc::downgrade(self);
        self.setup.retry.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.user_stopped.set(false);
                inner.start();
            }
        });
        let weak = Rc::downgrade(self);
        self.setup.start.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.user_stopped.set(false);
                inner.start();
            }
        });
        let weak = Rc::downgrade(self);
        self.setup.cancel.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade()
                && let Some(cancel) = inner.install_cancel.borrow().as_ref()
            {
                cancel.cancel();
            }
        });
    }

    fn add_panel_actions(self: &Rc<Self>) {
        let add = |name: &str, run: fn(&Rc<Inner>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    run(&inner);
                }
            });
            self.actions.add_action(&action);
        };
        add("new-session", |inner| inner.new_session());
        add("shutdown", |inner| inner.shutdown());
        add("clear", |inner| inner.clear());
        add("start", |inner| {
            inner.user_stopped.set(false);
            inner.start();
        });
    }

    fn controller(&self) -> Option<Rc<AgentController>> {
        self.controller.upgrade()
    }

    /// Shows `command` to the test hook; true when it goes on to the controller.
    fn command(&self, command: &PanelCommand) -> bool {
        let hook = self.command_hook.borrow();
        hook.as_ref().is_none_or(|hook| !hook(command))
    }

    // ----- lifecycle -----

    /// The pane was shown: start the agent unless it runs, failed (the card says why) or the
    /// user stopped it.
    fn on_shown(self: &Rc<Self>) {
        if !self.backend_ui.key_wanted.replace(true) {
            self.refresh_key_status();
        }
        let Some(controller) = self.controller() else {
            return;
        };
        self.watch_selection();
        // The pane shows the requests now.
        self.dismiss_permission_toast(true);
        // A collapsed (overlay) pane closes as soon as the focus is outside it, and libadwaita
        // gives it to the pane's first focusable widget (Stop, while a turn runs): move it to the
        // prompt box or the pending card, also once libadwaita is done.
        if let Some(state) = self.state.upgrade()
            && state.agent.split.is_collapsed()
        {
            if !self.focus_in_panel() || self.stop_focused() {
                self.focus_best();
            }
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(inner) = weak.upgrade()
                    && inner.stop_focused()
                {
                    inner.focus_best();
                }
            });
        }
        if controller.state() == AgentState::Stopped && !self.user_stopped.get() {
            self.start();
        }
    }

    fn start(self: &Rc<Self>) {
        let Some(controller) = self.controller() else {
            return;
        };
        if self.starting.get() {
            return;
        }
        if !self.command(&PanelCommand::Start) {
            return;
        }
        self.starting.set(true);
        let generation = self.start_gen.get() + 1;
        self.start_gen.set(generation);
        // The bridge's toolchain check of this attempt explains a failure, not an older one.
        install::clear_last_result();
        self.show_setup(SetupState::Starting);
        let weak = Rc::downgrade(self);
        let start = controller.ensure_started();
        glib::spawn_future_local(async move {
            let result = start.await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            // A stop (Stop Agent, New Session) or a newer start overtook this attempt: the card
            // already says what happened since.
            if inner.start_gen.get() != generation || inner.user_stopped.get() {
                return;
            }
            inner.starting.set(false);
            let failed = inner
                .controller()
                .is_some_and(|c| matches!(c.state(), AgentState::Failed(_)));
            match result {
                Ok(()) => inner.after_started(),
                // A start the bridge gave up because the agent was stopped is no failure.
                Err(message) if failed => inner.show_failure(&message),
                Err(_) => {}
            }
        });
    }

    fn after_started(&self) {
        self.show_setup(SetupState::Hidden);
        self.maybe_show_hint();
    }

    /// The non-blocking "Run `claude update`" hint, when the toolchain the agent started with
    /// has an old claude.
    fn maybe_show_hint(&self) {
        if self.hint_dismissed.get() || self.hint.reveals_child() {
            return;
        }
        if let Some(Ok(toolchain)) = install::last_result()
            && toolchain.needs_claude_update()
        {
            let (a, b, c) = install::MIN_CLAUDE_VERSION;
            self.hint_label.set_markup(&format!(
                "Claude Code {} is older than {a}.{b}.{c}, which the adapter needs. Run \
                 <tt>claude update</tt> in a terminal.",
                markdown::escape(toolchain.claude_version.as_deref().unwrap_or("?"))
            ));
            self.hint.set_reveal_child(true);
        }
    }

    fn show_failure(&self, message: &str) {
        self.maybe_show_login_hint(message);
        let toolchain = install::last_result();
        self.show_setup(SetupState::Failed {
            message: message.to_owned(),
            toolchain: toolchain.and_then(Result::err),
        });
    }

    fn new_session(self: &Rc<Self>) {
        // A start still running belongs to the old session.
        self.start_gen.set(self.start_gen.get() + 1);
        if self.command(&PanelCommand::NewSession)
            && let Some(controller) = self.controller()
        {
            controller.stop();
        }
        self.clear();
        self.reset_usage();
        self.user_stopped.set(false);
        self.start();
    }

    fn shutdown(self: &Rc<Self>) {
        // Named before it stops: a backend chosen meanwhile in config.toml applies then.
        let name = self.agent_label();
        self.user_stopped.set(true);
        self.start_gen.set(self.start_gen.get() + 1);
        if self.command(&PanelCommand::Stop)
            && let Some(controller) = self.controller()
        {
            controller.stop();
        }
        self.reset_usage();
        self.push(Entry::Notice(format!(
            "{name} was stopped and its memory freed. Send a prompt to start it again."
        )));
        self.show_setup(SetupState::Stopped);
    }

    /// Clear Transcript. While a turn runs (or a prompt is being prepared), its open permission
    /// requests stay (still answerable) and so does the bookkeeping of its tool calls.
    fn clear(&self) {
        let busy = self.controller().is_some_and(|c| is_busy(&c));
        let had_rows = self.rows.n_items() > 0;
        let keep: Vec<TranscriptItem> = if busy {
            self.permissions
                .borrow()
                .iter()
                .filter(|item| {
                    matches!(item.entry(), Entry::Permission(card)
                        if card.answer == PermissionAnswer::Pending)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        for row in self.row_items() {
            row.imp().observer.replace(None);
        }
        for item in self.items() {
            item.imp().rows.borrow_mut().clear();
        }
        self.rows.remove_all();
        self.store.remove_all();
        self.stream.replace(None);
        self.dirty.borrow_mut().clear();
        if !busy {
            self.tools.borrow_mut().clear();
        }
        self.permissions.replace(Vec::new());
        self.plan.replace(None);
        self.summary.replace(None);
        self.stick.set(true);
        for item in keep {
            self.append_item(&item);
            self.permissions.borrow_mut().push(item);
        }
        self.stack
            .set_visible_child_name(if self.store.n_items() == 0 {
                "empty"
            } else {
                "transcript"
            });
        // The rows' widgets go once the list view has caught up; then give the freed heap back
        // (a long session's transcript is many megabytes).
        if had_rows {
            glib::timeout_add_local_once(Duration::from_millis(500), || {
                // SAFETY: malloc_trim only returns free heap pages to the OS.
                unsafe {
                    libc::malloc_trim(0);
                }
            });
        }
    }

    fn reset_usage(&self) {
        self.usage.set(None);
        self.refresh_ring();
        self.backend_ui.turn_cost.set(0);
        self.backend_ui.session_cost.set(0);
        self.backend_ui.cost_label.set_visible(false);
    }

    fn on_state(self: &Rc<Self>, state: &AgentState) {
        if matches!(state, AgentState::Stopped | AgentState::Failed(_))
            && self.backend_ui.deferred.get()
        {
            self.adopt_config_backend();
        }
        self.refresh_header();
        // A start attempt is over once the state says how it went (its future may resolve a
        // little later); Retry must work from then on.
        if !matches!(state, AgentState::Starting) {
            self.starting.set(false);
        }
        match state {
            AgentState::Starting => {
                self.session_up.set(false);
                self.show_setup(SetupState::Starting);
            }
            AgentState::Failed(message) => {
                self.finish_turn_items();
                self.restore_pending_prompt();
                self.show_failure(message);
            }
            AgentState::Idle | AgentState::Running { .. } => {
                self.session_up.set(true);
                if !self.starting.get() && self.install_cancel.borrow().is_none() {
                    self.show_setup(SetupState::Hidden);
                }
                self.maybe_show_hint();
            }
            AgentState::Stopped => {
                self.finish_turn_items();
                // A prompt that was still being prepared was not sent.
                self.restore_pending_prompt();
                if self.install_cancel.borrow().is_none() && !self.starting.get() {
                    self.show_setup(if self.user_stopped.get() {
                        SetupState::Stopped
                    } else {
                        SetupState::Hidden
                    });
                }
            }
        }
    }

    fn refresh_header(&self) {
        let state = self.controller().map_or(AgentState::Stopped, |c| c.state());
        let what = match &state {
            AgentState::Stopped => "stopped",
            AgentState::Starting => "starting…",
            AgentState::Idle => "idle",
            AgentState::Running { .. } => "running",
            AgentState::Failed(_) => "not running",
        };
        let name = super::bridge::backend_name(self.backend());
        self.status.set_markup(&format!("<b>{name}</b> · {what}"));
        self.refresh_names();
        let name = self.agent_name.borrow();
        self.status
            .set_tooltip_text((!name.is_empty()).then_some(name.as_str()));
        let running = matches!(state, AgentState::Running { .. });
        let busy = self.controller().is_some_and(|c| is_busy(&c));
        // Stop also cancels a prompt that waits for the agent to start.
        self.stop_button.set_visible(busy);
        self.send_button
            .set_sensitive(!busy && !self.prompt_text().trim().is_empty());
        // A choice needs a session to go to.
        self.pickers.set_sensitive(matches!(
            state,
            AgentState::Idle | AgentState::Running { .. }
        ));
        // Nothing tells when a prompt being prepared is sent or dropped: look again shortly.
        if busy && !running && !self.header_poll.replace(true) {
            let weak = self.root.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(inner) = weak.upgrade().and_then(|w| w.imp().inner.borrow().clone())
                else {
                    return glib::ControlFlow::Break;
                };
                let preparing = inner
                    .controller()
                    .is_some_and(|c| is_busy(&c) && !c.is_running());
                if preparing {
                    return glib::ControlFlow::Continue;
                }
                inner.header_poll.set(false);
                inner.refresh_header();
                glib::ControlFlow::Break
            });
        }
    }

    // ----- set-up card -----

    fn show_setup(&self, what: SetupState) {
        let s = &self.setup;
        for button in [&s.install, &s.retry, &s.cancel, &s.start] {
            button.set_visible(false);
        }
        s.spinner.set_visible(false);
        s.icon.set_visible(false);
        s.message.set_visible(true);
        s.log_expander.set_visible(false);
        match what {
            SetupState::Hidden => {
                s.revealer.set_reveal_child(false);
                return;
            }
            SetupState::Starting => {
                s.spinner.set_visible(true);
                s.title
                    .set_text(&format!("Starting {}…", self.backend_name()));
                s.message.set_visible(false);
            }
            SetupState::Stopped => {
                s.icon.set_icon_name(Some("media-playback-stop-symbolic"));
                s.icon.set_visible(true);
                s.title
                    .set_text(&format!("{} is stopped", self.backend_name()));
                s.message
                    .set_text("Its memory is freed. It starts again with your next prompt.");
                s.start.set_visible(true);
            }
            SetupState::Failed { message, toolchain } => {
                s.icon.set_icon_name(Some("dialog-warning-symbolic"));
                s.icon.set_visible(true);
                s.title
                    .set_text(&format!("{} could not start", self.backend_name()));
                let state = self.state.upgrade();
                let detail = match &toolchain {
                    Some(install::ToolchainError::PiAdapter { found, wanted }) => {
                        s.install.set_visible(true);
                        let place = state
                            .as_ref()
                            .map(|st| st.config.agent.pi_adapter_dir.display().to_string())
                            .unwrap_or_default();
                        let package = install::PI_ADAPTER_PACKAGE;
                        match found {
                            None => format!(
                                "Lemma talks to Pi through the ACP adapter {package} {wanted}, \
                                 which is not installed. Install it with npm into {place}."
                            ),
                            Some(found) => format!(
                                "The Pi ACP adapter in {place} is version {found}; Lemma needs \
                                 {wanted}. Install that version with npm."
                            ),
                        }
                    }
                    Some(install::ToolchainError::Adapter { found, wanted }) => {
                        s.install.set_visible(true);
                        let size = "about 60 MB";
                        let place = state
                            .as_ref()
                            .map(|st| st.config.agent.adapter_dir.display().to_string())
                            .unwrap_or_default();
                        match found {
                            None => format!(
                                "Lemma talks to Claude Code through the ACP adapter {wanted}, \
                                 which is not installed. Install it with npm into {place} \
                                 ({size})."
                            ),
                            Some(found) => format!(
                                "The ACP adapter in {place} is version {found}; Lemma needs \
                                 {wanted}. Install that version with npm ({size})."
                            ),
                        }
                    }
                    Some(install::ToolchainError::NodeNotFound) => {
                        "Node.js was not found on PATH or in mise's installs. Install it (for \
                         example `mise use -g node@lts`) or set agent.node in config.toml."
                            .to_owned()
                    }
                    Some(install::ToolchainError::ClaudeNotFound) => {
                        "Claude Code was not found on PATH, in mise's installs or in \
                         ~/.local/bin. Install it, or set agent.claude in config.toml."
                            .to_owned()
                    }
                    _ => message.clone(),
                };
                s.message.set_text(&markdown::without_nul(&detail));
                s.retry.set_visible(true);
                let log = s.log.buffer();
                s.log_expander.set_visible(log.char_count() > 0);
            }
            SetupState::Installing => {
                s.spinner.set_visible(true);
                let pi = self.backend() == AgentBackend::Pi;
                s.title.set_text(if pi {
                    "Installing the Pi ACP adapter…"
                } else {
                    "Installing the Claude ACP adapter…"
                });
                let line = self
                    .state
                    .upgrade()
                    .and_then(|st| {
                        st.paths.as_ref().map(|p| {
                            if pi {
                                install::pi_install_command_line(&st.config.agent, p)
                            } else {
                                install::install_command_line(&st.config.agent, p)
                            }
                        })
                    })
                    .unwrap_or_default();
                s.message.set_text(&line);
                s.cancel.set_visible(true);
                s.log_expander.set_visible(true);
                s.log_expander.set_expanded(true);
            }
            SetupState::InstallFailed(error) => {
                s.icon.set_icon_name(Some("dialog-error-symbolic"));
                s.icon.set_visible(true);
                s.title.set_text("The installation failed");
                s.message.set_text(&error);
                s.install.set_visible(true);
                s.log_expander.set_visible(true);
            }
        }
        s.revealer.set_reveal_child(true);
    }

    fn run_install(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if self.install_cancel.borrow().is_some() {
            return;
        }
        let Some(paths) = state.paths.clone() else {
            self.show_setup(SetupState::InstallFailed(
                "The home directory is unknown, so the adapter has nowhere to go.".into(),
            ));
            return;
        };
        let cfg = state.config.agent.clone();
        let env = install::Env::from_process(&paths);
        let cancel = gio::Cancellable::new();
        self.install_cancel.replace(Some(cancel.clone()));
        self.setup.log.buffer().set_text("");
        self.show_setup(SetupState::Installing);
        let weak = Rc::downgrade(self);
        let on_line = move |line: &str| {
            if let Some(inner) = weak.upgrade() {
                inner.append_log(line);
            }
        };
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>> =
            if cfg.backend == AgentBackend::Pi {
                Box::pin(install::install_pi_adapter(
                    &cfg, &paths, &env, &cancel, on_line,
                ))
            } else {
                Box::pin(install::install_adapter(
                    &cfg, &paths, &env, &cancel, on_line,
                ))
            };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = future.await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.install_cancel.replace(None);
            match result {
                Ok(()) => {
                    inner.append_log("Installed.");
                    install::clear_last_result();
                    inner.user_stopped.set(false);
                    inner.start();
                }
                Err(e) => inner.show_setup(SetupState::InstallFailed(e)),
            }
        });
    }

    fn append_log(&self, line: &str) {
        let buffer = self.setup.log.buffer();
        let mut end = buffer.end_iter();
        if buffer.char_count() > 0 {
            buffer.insert(&mut end, "\n");
        }
        buffer.insert(&mut end, line);
        let excess = buffer.line_count() - MAX_LOG_LINES as i32;
        if excess > 0
            && let Some(mut cut) = buffer.iter_at_line(excess)
        {
            buffer.delete(&mut buffer.start_iter(), &mut cut);
        }
        let end = buffer.end_iter();
        let mark = buffer.create_mark(None, &end, false);
        self.setup.log.scroll_mark_onscreen(&mark);
        buffer.delete_mark(&mark);
    }

    // ----- events -----

    fn on_event(self: &Rc<Self>, event: &AgentEvent) {
        match event {
            AgentEvent::Ready {
                agent,
                modes,
                mode,
                options,
                ..
            } => {
                self.agent_name.replace(agent.clone());
                self.modes.replace(modes.clone());
                self.mode.replace(mode.clone());
                self.options.replace(options.clone());
                // A new session: nothing of its context is used yet.
                self.reset_usage();
                self.sync_mode_option();
                self.refresh_pickers();
                self.refresh_header();
            }
            // Agent text never carries a NUL into GLib (see `markdown::without_nul`).
            AgentEvent::MessageChunk(text) => {
                self.append_stream(&markdown::without_nul(text), false);
            }
            AgentEvent::ThoughtChunk(text) => {
                self.append_stream(&markdown::without_nul(text), true);
            }
            AgentEvent::ToolCall {
                id,
                title,
                kind,
                status,
            } => self.tool_call(
                &markdown::without_nul(id),
                &markdown::without_nul(title),
                kind,
                *status,
            ),
            AgentEvent::ToolUpdate { id, title, status } => {
                let title = title.as_deref().map(markdown::without_nul);
                self.tool_update(&markdown::without_nul(id), title.as_deref(), *status);
            }
            AgentEvent::Plan(entries) => self.set_plan(entries),
            AgentEvent::Commands(commands) => {
                self.commands.replace(commands.clone());
            }
            AgentEvent::ModeChanged(mode) => {
                self.set_mode_value(mode);
                self.refresh_pickers();
            }
            AgentEvent::ConfigChanged(options) => {
                self.options.replace(options.clone());
                self.sync_mode_option();
                self.refresh_pickers();
            }
            AgentEvent::Permission {
                request,
                title,
                options,
            } => {
                let options: Vec<PermOption> = options
                    .iter()
                    .map(|o| PermOption {
                        id: o.id.clone(),
                        name: markdown::without_nul(&o.name).into_owned(),
                        kind: o.kind.clone(),
                    })
                    .collect();
                self.permission_request(*request, &markdown::without_nul(title), &options);
            }
            AgentEvent::PermissionWithdrawn { request } => self.withdraw_permission(*request),
            AgentEvent::TurnEnded(reason) => self.turn_ended(*reason),
            AgentEvent::Error(message) => {
                self.maybe_show_login_hint(message);
                let message = markdown::without_nul(message).into_owned();
                // An error may be the agent refusing a mode or model: the pickers go back to
                // what the session confirmed.
                self.refresh_pickers();
                // A failed start is explained by the set-up card (the bridge reports it once
                // for the start and once more for the prompt that waited for it), and the same
                // error twice in a row is said once. The prompt being sent comes back into the
                // box only when the agent's state says it failed (an error can be about anything
                // else, such as another prompt that was refused).
                let start_failure = self
                    .controller()
                    .is_some_and(|c| c.state() == AgentState::Failed(message.clone()));
                let repeated = self
                    .store
                    .item(self.store.n_items().saturating_sub(1))
                    .and_downcast::<TranscriptItem>()
                    .is_some_and(|last| last.entry() == Entry::Error(message.clone()));
                if !start_failure && !repeated {
                    self.push(Entry::Error(message));
                }
            }
            AgentEvent::Exited(code) => {
                self.finish_turn_items();
                // An exit during the start is the start's failure, which the card shows.
                if !self.user_stopped.get() && self.session_up.get() {
                    let text = match code {
                        Some(code) => format!("{} exited (status {code}).", self.agent_label()),
                        None => format!("{} exited.", self.agent_label()),
                    };
                    self.push(Entry::Error(text));
                }
            }
            AgentEvent::Usage { used, size } => {
                self.usage.set(Some((*used, *size)));
                self.refresh_ring();
            }
            AgentEvent::Cost { usd_estimate, .. } => self.add_cost(usd_estimate.0),
            AgentEvent::Notice(text) => {
                self.push(Entry::Notice(markdown::without_nul(text).into_owned()));
            }
        }
    }

    fn on_prompt(self: &Rc<Self>, prompt: &SentPrompt) {
        self.pending_prompt.replace(None);
        self.backend_ui.turn_cost.set(0);
        self.turn.set(prompt.turn);
        self.known_hunks.replace(self.hunk_shapes());
        self.edit_links.borrow_mut().clear();
        self.linked.borrow_mut().clear();
        self.plan.replace(None);
        let text = prompt
            .blocks
            .iter()
            .filter_map(|b| match b {
                PromptBlock::Text(t) => Some(t.as_str()),
                PromptBlock::Resource { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let attachments = prompt
            .blocks
            .iter()
            .filter_map(|b| match b {
                PromptBlock::Resource { uri, .. } => Some(self.resource_name(uri)),
                PromptBlock::Text(_) => None,
            })
            .collect();
        let heading = self.origin_heading(&prompt.origin);
        self.stick.set(true);
        self.push(Entry::User {
            turn: prompt.turn,
            origin: prompt.origin.clone(),
            heading,
            text,
            attachments,
        });
        self.refresh_header();
    }

    fn origin_heading(&self, origin: &PromptOrigin) -> String {
        match origin {
            PromptOrigin::Panel => "You".into(),
            PromptOrigin::Inline { path, range } => {
                let name = self.relative(path);
                match self
                    .state
                    .upgrade()
                    .and_then(|s| s.workspace.get(path))
                    .map(|doc| {
                        let index = doc.line_index();
                        (
                            index.line_of(range.start) + 1,
                            index.line_of(range.end.saturating_sub(1).max(range.start)) + 1,
                        )
                    }) {
                    Some((a, b)) if a == b => format!("Inline edit · {name}:{a}"),
                    Some((a, b)) => format!("Inline edit · {name}:{a}–{b}"),
                    None => format!("Inline edit · {name}"),
                }
            }
            PromptOrigin::QuickAction(name) => format!("Quick action · {name}"),
            PromptOrigin::FixDiagnostic => "Ask agent to fix".into(),
        }
    }

    fn relative(&self, path: &Path) -> String {
        match self.state.upgrade() {
            Some(state) => review_ui::relative_name(&state, path),
            None => path.display().to_string(),
        }
    }

    fn resource_name(&self, uri: &str) -> String {
        let (base, fragment) = uri.split_once('#').unwrap_or((uri, ""));
        let name = glib::filename_from_uri(base)
            .map(|(p, _)| self.relative(&p))
            .unwrap_or_else(|_| base.to_owned());
        match fragment.strip_prefix('L') {
            Some(lines) => format!("{name} (lines {})", lines.replace("-L", "–")),
            None => name,
        }
    }

    /// Appends `item` (and shows the transcript).
    fn push(&self, entry: Entry) -> TranscriptItem {
        let item = TranscriptItem::new(entry);
        // A new item ends a streaming message or thought.
        if let Some(stream) = self.stream.take() {
            let finished = stream.with_entry(|e| match e {
                Entry::Thought { done, .. } => {
                    *done = true;
                    true
                }
                _ => false,
            });
            if finished {
                self.mark_dirty(&stream);
            } else if !stream.imp().dirty.get() {
                // A finished message's parser state can go (a dirty one's goes at the flush).
                stream.imp().markdown.replace(None);
            }
        }
        self.append_item(&item);
        self.stack.set_visible_child_name("transcript");
        if self.stick.get() {
            self.scroll_to_end();
        }
        item
    }

    /// Puts `item` at the end of the transcript with its rows: an agent message's rows come
    /// with the next flush (one per Markdown block), any other entry has one row.
    fn append_item(&self, item: &TranscriptItem) {
        self.store.append(item);
        if item.is_agent() {
            self.mark_dirty(item);
        } else {
            let row = RowItem::new(item, None);
            item.imp().rows.replace(vec![row.clone()]);
            self.rows.append(&row);
        }
    }

    /// Takes `item` and its rows out of the transcript.
    fn remove_item(&self, item: &TranscriptItem) {
        for row in item.imp().rows.take() {
            row.imp().observer.replace(None);
            if let Some(at) = self.rows.find(&row) {
                self.rows.remove(at);
            }
        }
        if let Some(at) = self.store.find(item) {
            self.store.remove(at);
        }
    }

    fn items(&self) -> Vec<TranscriptItem> {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<TranscriptItem>())
            .collect()
    }

    fn row_items(&self) -> Vec<RowItem> {
        (0..self.rows.n_items())
            .filter_map(|i| self.rows.item(i).and_downcast::<RowItem>())
            .collect()
    }

    /// Where new rows of `item`, which has none yet, go: after the last row of the nearest
    /// entry before it that has rows.
    fn rows_position(&self, item: &TranscriptItem) -> u32 {
        let Some(at) = self.store.find(item) else {
            return self.rows.n_items();
        };
        (0..at)
            .rev()
            .filter_map(|p| self.store.item(p).and_downcast::<TranscriptItem>())
            .find_map(|before| {
                let last = before.imp().rows.borrow().last().cloned()?;
                self.rows.find(&last).map(|p| p + 1)
            })
            .unwrap_or(0)
    }

    /// Brings an agent message's rows in line with its text: re-parses only the changed end of
    /// the Markdown, updates the rows whose block changed, and adds or removes rows at its end.
    fn sync_agent(&self, item: &TranscriptItem) {
        let update = {
            let entry = item.imp().entry.borrow();
            let Entry::Agent { text } = &*entry else {
                return;
            };
            let mut parsed = item.imp().markdown.borrow_mut();
            parsed
                .get_or_insert_with(markdown::Incremental::default)
                .update(text)
        };
        let markdown::Update { unchanged, tail } = update;
        let total = unchanged + tail.len();
        let rows = item.imp().rows.borrow().clone();
        let mut added = Vec::new();
        for (i, block) in tail.into_iter().enumerate() {
            match rows.get(unchanged + i) {
                Some(row) => row.set_block(block),
                None => added.push(RowItem::new(item, Some(block))),
            }
        }
        // A message can also lose blocks (`Title` + `---` turns a rule into a heading).
        if rows.len() > total {
            for row in &rows[total..] {
                row.imp().observer.replace(None);
            }
            if let Some(first) = self.rows.find(&rows[total]) {
                let n = u32::try_from(rows.len() - total).unwrap_or(u32::MAX);
                self.rows.splice(first, n, &[] as &[RowItem]);
            }
            item.imp().rows.borrow_mut().truncate(total);
        }
        if !added.is_empty() {
            let at = match rows
                .get(..total.min(rows.len()))
                .and_then(<[RowItem]>::last)
            {
                Some(last) => self.rows.find(last).map_or(self.rows.n_items(), |p| p + 1),
                None => self.rows_position(item),
            };
            item.imp().rows.borrow_mut().extend(added.iter().cloned());
            self.rows.splice(at, 0, &added);
        }
    }

    fn position_of(&self, item: &TranscriptItem) -> Option<u32> {
        self.store.find(item)
    }

    fn append_stream(&self, text: &str, thought: bool) {
        let current = self.stream.borrow().clone();
        if let Some(item) = current {
            let appended = item.with_entry(|e| match e {
                Entry::Agent { text: t } if !thought => {
                    t.push_str(text);
                    true
                }
                Entry::Thought { text: t, .. } if thought => {
                    t.push_str(text);
                    true
                }
                _ => false,
            });
            if appended {
                self.mark_dirty(&item);
                return;
            }
        }
        let entry = if thought {
            Entry::Thought {
                text: text.to_owned(),
                done: false,
            }
        } else {
            Entry::Agent {
                text: text.to_owned(),
            }
        };
        let item = self.push(entry);
        self.stream.replace(Some(item));
    }

    fn mark_dirty(&self, item: &TranscriptItem) {
        if !item.imp().dirty.replace(true) {
            self.dirty.borrow_mut().push(item.clone());
        }
        if self.flush_scheduled.replace(true) {
            return;
        }
        let root = self.root.upgrade();
        let weak = root.as_ref().map(|r| r.downgrade());
        let flush = move || {
            if let Some(widget) = weak.as_ref().and_then(|w| w.upgrade())
                && let Some(inner) = widget.imp().inner.borrow().clone()
            {
                inner.flush();
            }
        };
        match root {
            Some(root) if root.is_mapped() => {
                root.add_tick_callback(move |_, _| {
                    flush();
                    glib::ControlFlow::Break
                });
            }
            _ => {
                glib::idle_add_local_once(flush);
            }
        }
    }

    /// Re-renders the items that changed since the last frame (and the review summary).
    fn flush(&self) {
        self.flush_scheduled.set(false);
        let dirty = self.dirty.take();
        for item in dirty {
            item.imp().dirty.set(false);
            if item.is_agent() {
                self.sync_agent(&item);
                // A finished message never changes again: its rows hold its blocks, and the
                // parser's copy of its text and blocks can go.
                if self.stream.borrow().as_ref() != Some(&item) {
                    item.imp().markdown.replace(None);
                }
            } else {
                item.notify_changed();
            }
        }
        self.refresh_summary();
        if self.stick.get() {
            self.scroll_to_end();
        }
    }

    /// The latest "N changes to review" entry says what is under review now.
    fn refresh_summary(&self) {
        let (Some(item), Some(state)) = (self.summary.borrow().clone(), self.state.upgrade())
        else {
            return;
        };
        let (files, hunks) = review_ui::summary(&state.workspace);
        let changed = item.with_entry(|e| match e {
            Entry::Summary { files: f, hunks: h } if (*f, *h) != (files, hunks) => {
                (*f, *h) = (files, hunks);
                true
            }
            _ => false,
        });
        if changed {
            item.notify_changed();
        }
    }

    fn scroll_to_end(&self) {
        let n = self.rows.n_items();
        if n == 0 {
            return;
        }
        self.scrolling.set(true);
        self.list.scroll_to(n - 1, gtk::ListScrollFlags::NONE, None);
        let adj = self.scroller.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
        self.scrolling.set(false);
    }

    // ----- tools -----

    /// Every hunk of the window's reviews with its shape, after the recomputes that are due.
    fn hunk_shapes(&self) -> HashMap<(PathBuf, u64), HunkShape> {
        let Some(state) = self.state.upgrade() else {
            return HashMap::new();
        };
        // An agent edit's recompute runs from an idle, which the tool's reply can beat.
        for doc in state.workspace.documents() {
            review_ui::recompute_if_stale(&doc);
        }
        let mut shapes = HashMap::new();
        for doc in review_ui::reviewed_documents(&state.workspace) {
            if let Some(review) = doc.review.borrow().as_ref() {
                for hunk in review.hunks() {
                    shapes.insert(
                        (doc.path().to_path_buf(), hunk.id.0),
                        HunkShape {
                            line: hunk.cur_lines.start,
                            base: hunk.base_bytes.clone(),
                            cur_len: hunk.cur_bytes.len(),
                        },
                    );
                }
            }
        }
        shapes
    }

    /// The editor answered a tool call (the bridge's [`ToolRecord`], emitted right after the
    /// reply went out, so always before the agent reports the call as done): an `edit` or
    /// `write` remembers the hunk it made or changed for its card.
    fn on_tool_done(&self, record: &ToolRecord) {
        // `fs/write_text_file`: the edits of agents with ACP `fs` routing (Pi).
        if !matches!(record.tool, "edit" | "write" | "fs/write_text_file") {
            return;
        }
        let shapes = self.hunk_shapes();
        let link = if record.is_error {
            None
        } else {
            let known = self.known_hunks.borrow();
            let target = self.state.upgrade().and_then(|state| {
                let path = record.path.as_deref()?;
                state.workspace.root().resolve(path).ok()
            });
            let mut changed: Vec<(&PathBuf, u32, u64)> = shapes
                .iter()
                .filter(|(key, shape)| known.get(*key) != Some(*shape))
                .map(|((path, id), shape)| (path, shape.line, *id))
                .collect();
            // The named file's hunks first, then by position.
            changed.sort_by_key(|(path, line, id)| {
                (
                    target.as_deref() != Some(path.as_path()),
                    (*path).clone(),
                    *line,
                    *id,
                )
            });
            changed
                .first()
                .and_then(|(path, line, id)| self.tool_link(path, *line, *id))
        };
        self.known_hunks.replace(shapes);
        self.edit_links.borrow_mut().push_back(link);
    }

    fn tool_link(&self, path: &Path, line: u32, hunk: u64) -> Option<ToolLink> {
        let state = self.state.upgrade()?;
        let label = format!("{}:{}", review_ui::relative_name(&state, path), line + 1);
        Some(ToolLink {
            path: path.to_path_buf(),
            line,
            hunk,
            label,
        })
    }

    fn tool_call(self: &Rc<Self>, id: &str, title: &str, kind: &str, status: ToolStatus) {
        if let Some(item) = self.tools.borrow().get(id).cloned() {
            // A repeated report of the same call.
            item.with_entry(|e| {
                if let Entry::Tool(card) = e {
                    card.title = title.to_owned();
                    card.kind = kind.to_owned();
                    card.status = status;
                }
            });
            item.notify_changed();
            return;
        }
        let item = self.push(Entry::Tool(ToolCard {
            id: id.to_owned(),
            title: title.to_owned(),
            kind: kind.to_owned(),
            status,
            link: None,
        }));
        self.tools.borrow_mut().insert(id.to_owned(), item);
        if matches!(status, ToolStatus::Completed | ToolStatus::Failed) {
            self.tool_update(id, None, Some(status));
        }
    }

    fn tool_update(self: &Rc<Self>, id: &str, title: Option<&str>, status: Option<ToolStatus>) {
        let Some(item) = self.tools.borrow().get(id).cloned() else {
            // An update of a call we never saw: show it as a card.
            if let Some(status) = status {
                self.tool_call(id, title.unwrap_or(id), "other", status);
            }
            return;
        };
        let (edit, status) = item.with_entry(|e| match e {
            Entry::Tool(card) => {
                if let Some(title) = title {
                    card.title = title.to_owned();
                }
                if let Some(status) = status {
                    card.status = status;
                }
                (is_edit_tool(&card.title, &card.kind), Some(card.status))
            }
            _ => (false, None),
        });
        // A finished edit card takes its call's record once (a failed call's too, so the
        // records stay in step with the cards).
        if edit
            && matches!(status, Some(ToolStatus::Completed | ToolStatus::Failed))
            && self.linked.borrow_mut().insert(id.to_owned())
        {
            let link = self.link_for(id);
            if status == Some(ToolStatus::Completed) {
                item.with_entry(|e| {
                    if let Entry::Tool(card) = e {
                        card.link = link;
                    }
                });
            }
        }
        item.notify_changed();
    }

    /// The hunk an edit card's call made: the oldest answered `edit`/`write` call no card has
    /// taken yet. The editor answers a call before the agent can report it as done, so for
    /// calls made one after another (what Claude does for edits) this is the card's own call;
    /// for parallel calls it is a sibling's, which still opens a change of the same turn.
    fn link_for(&self, _id: &str) -> Option<ToolLink> {
        self.edit_links.borrow_mut().pop_front().flatten()
    }

    fn activate_tool(self: &Rc<Self>, id: &str) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(Entry::Tool(card)) = self.tools.borrow().get(id).map(|i| i.entry()) else {
            return;
        };
        if card.title == "mcp__lemma__compile" && card.link.is_none() {
            crate::diagnostics_panel::reveal(&state);
            return;
        }
        let Some(link) = card.link else {
            return;
        };
        let line = state
            .workspace
            .get(&link.path)
            .and_then(|doc| {
                let review = doc.review.borrow();
                review
                    .as_ref()?
                    .hunks()
                    .iter()
                    .find(|h| h.id.0 == link.hunk)
                    .map(|h| h.cur_lines.start)
            })
            .unwrap_or(link.line);
        match state.workspace.get(&link.path) {
            Some(doc) => review_ui::show_line(&state, &doc, line),
            None => {
                let path = link.path.clone();
                glib::spawn_future_local(async move {
                    if let Err(e) =
                        crate::diagnostics_panel::open_at_line(&state, &path, Some(line + 1)).await
                    {
                        state.toast(&e);
                    }
                });
            }
        }
    }

    fn set_plan(&self, entries: &[(String, String)]) {
        let current = self.plan.borrow().clone();
        if let Some(item) = current
            && self.position_of(&item).is_some()
        {
            item.with_entry(|e| *e = Entry::Plan(entries.to_vec()));
            item.notify_changed();
            return;
        }
        let item = self.push(Entry::Plan(entries.to_vec()));
        self.plan.replace(Some(item));
    }

    // ----- permissions -----

    fn permission_request(self: &Rc<Self>, request: u64, title: &str, options: &[PermOption]) {
        let mut offered = offered_options(options);
        if offered.is_empty() {
            offered = options
                .iter()
                .filter(|o| o.kind.starts_with("reject"))
                .cloned()
                .collect();
        }
        let item = self.push(Entry::Permission(PermissionCard {
            request,
            title: title.to_owned(),
            options: offered,
            answer: PermissionAnswer::Pending,
        }));
        self.permissions.borrow_mut().push(item);
        self.list.announce(
            &format!("{} asks for permission: {title}", self.agent_label()),
            gtk::AccessibleAnnouncementPriority::Medium,
        );
        if let Some(state) = self.state.upgrade() {
            let split = &state.agent.split;
            if split.is_collapsed() && !split.shows_sidebar() && self.typing_outside() {
                // In a narrow window the pane is an overlay that takes the keyboard focus when
                // it opens, so the next keys the user types into the editor would go to the
                // card (or press Stop): a toast says so instead, and Ctrl+L or its Answer
                // button goes to the card.
                self.show_permission_toast(&state, title);
            } else {
                split.set_show_sidebar(true);
                if split.is_collapsed() && (!self.focus_in_panel() || self.stop_focused()) {
                    self.focus_best();
                }
            }
        }
        // Take the focus from an empty prompt box, so y/n answer at once: once the card is on
        // screen, unless the user typed meanwhile; the card itself takes it, never one of its
        // buttons.
        if self.prompt_focused() && self.prompt_text().is_empty() {
            self.schedule_card_focus(request, true);
        }
    }

    /// Moves the focus from the empty prompt box to permission card `request` once its row is
    /// on screen (a few frames at most), unless the prompt box lost the focus or got text
    /// meanwhile. `auto`: the card took the focus by itself (see [`AUTO_FOCUS_GRACE`]).
    fn schedule_card_focus(&self, request: u64, auto: bool) {
        let weak = self.root.clone();
        let mut tries = 0;
        glib::timeout_add_local(Duration::from_millis(16), move || {
            tries += 1;
            let Some(inner) = weak.upgrade().and_then(|w| w.imp().inner.borrow().clone()) else {
                return glib::ControlFlow::Break;
            };
            let pending = inner
                .pending_permissions()
                .iter()
                .any(|c| c.request == request);
            if !pending || !inner.prompt_focused() || !inner.prompt_text().is_empty() {
                return glib::ControlFlow::Break;
            }
            if inner.focus_card(request) {
                if auto && let Some(focus) = inner.focus_widget() {
                    inner
                        .auto_focus
                        .replace(Some((focus.downgrade(), Instant::now())));
                }
                return glib::ControlFlow::Break;
            }
            if tries < 30 {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    /// The pending permission requests, oldest first.
    fn pending_permissions(&self) -> Vec<PermissionCard> {
        self.permissions
            .borrow()
            .iter()
            .filter_map(|item| match item.entry() {
                Entry::Permission(card) if card.answer == PermissionAnswer::Pending => Some(card),
                _ => None,
            })
            .collect()
    }

    /// Tells about a permission request with a toast (see [`Self::permission_request`]); it
    /// stays until the request is answered or the pane shows it.
    fn show_permission_toast(&self, state: &WindowState, title: &str) {
        if let Some(old) = self.permission_toast.take() {
            old.dismiss();
        }
        let mut short: String = title.chars().take(80).collect();
        if short.len() < title.len() {
            short.push('…');
        }
        let toast = adw::Toast::builder()
            .title(format!(
                "{} asks for permission: {short}",
                self.agent_label()
            ))
            .use_markup(false)
            .button_label("Answer")
            .action_name("win.agent-focus")
            .timeout(0)
            .priority(adw::ToastPriority::High)
            .build();
        state.toasts.add_toast(toast.clone());
        self.permission_toast.replace(Some(toast));
    }

    /// Dismisses the permission toast once no request waits (or the pane shows them).
    fn dismiss_permission_toast(&self, always: bool) {
        if (always || self.pending_permission().is_none())
            && let Some(toast) = self.permission_toast.take()
        {
            toast.dismiss();
        }
    }

    /// The oldest pending permission request.
    fn pending_permission(&self) -> Option<PermissionCard> {
        self.permissions
            .borrow()
            .iter()
            .find_map(|item| match item.entry() {
                Entry::Permission(card) if card.answer == PermissionAnswer::Pending => Some(card),
                _ => None,
            })
    }

    /// Moves the focus to permission card `request` (the user asked for it: Ctrl+L, or after
    /// answering the card before it); while its row is not on screen yet, to the prompt box,
    /// and to the card a moment later.
    fn focus_permission(&self, request: u64) {
        self.auto_focus.replace(None);
        if !self.focus_card(request) {
            self.prompt.grab_focus();
            self.schedule_card_focus(request, false);
        }
    }

    /// Gives the focus to the card of permission request `request` (the card, not a button:
    /// Space and Enter then press nothing); false while its row is not on screen.
    fn focus_card(&self, request: u64) -> bool {
        let row = self
            .permissions
            .borrow()
            .iter()
            .find_map(|item| match item.entry() {
                Entry::Permission(card) if card.request == request => {
                    item.imp().rows.borrow().first().cloned()
                }
                _ => None,
            });
        if let Some(at) = row.and_then(|row| self.rows.find(&row)) {
            self.list.scroll_to(at, gtk::ListScrollFlags::NONE, None);
        }
        let name = format!("permission-{request}");
        descendants(&self.list)
            .into_iter()
            .find(|w| w.widget_name() == name && w.is_focusable() && !w.is::<gtk::Button>())
            .is_some_and(|card| card.grab_focus())
    }

    fn answer_permission(&self, request: u64, option_id: Option<String>) {
        let item = self
            .permissions
            .borrow()
            .iter()
            .find(|item| {
                matches!(item.entry(), Entry::Permission(card)
                if card.request == request && card.answer == PermissionAnswer::Pending)
            })
            .cloned();
        let Some(item) = item else {
            return;
        };
        // The card's buttons go: first move the focus to the next pending card, or the prompt
        // box, so it stays in the pane (a removed focus widget would take it out).
        if self.focus_in_panel() && !self.prompt_focused() {
            let next = self
                .permissions
                .borrow()
                .iter()
                .find_map(|i| match i.entry() {
                    Entry::Permission(c)
                        if c.answer == PermissionAnswer::Pending && c.request != request =>
                    {
                        Some(c.request)
                    }
                    _ => None,
                });
            match next {
                Some(next) => self.focus_permission(next),
                None => {
                    self.prompt.grab_focus();
                }
            }
        }
        item.with_entry(|e| {
            if let Entry::Permission(card) = e {
                card.answer = match &option_id {
                    Some(id) => PermissionAnswer::Chosen(id.clone()),
                    None => PermissionAnswer::Cancelled,
                };
            }
        });
        item.notify_changed();
        self.dismiss_permission_toast(false);
        let forward = self.command(&PanelCommand::Permission {
            request,
            option_id: option_id.clone(),
        });
        if forward && let Some(controller) = self.controller() {
            controller.permission(request, option_id);
        }
    }

    fn key_pressed(
        &self,
        keyval: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        use gtk::gdk::ModifierType as M;
        if state.intersects(M::CONTROL_MASK | M::ALT_MASK | M::SUPER_MASK) {
            return glib::Propagation::Proceed;
        }
        // Typing in the prompt box is typing.
        if self.prompt_focused() {
            return glib::Propagation::Proceed;
        }
        let focus = self.focus_widget();
        // The key, not the keyval's case: Caps Lock turns the `y` key into `Y` without Shift,
        // and "always allow" takes Shift.
        let key = match keyval.to_lower().to_unicode() {
            Some('y') if state.contains(M::SHIFT_MASK) => 'Y',
            Some(c @ ('y' | 'n')) => c,
            _ => return glib::Propagation::Proceed,
        };
        // A card that just took the focus by itself does not take keys yet: they were typed
        // for the prompt box.
        let fresh = self
            .auto_focus
            .borrow()
            .as_ref()
            .is_some_and(|(widget, at)| {
                at.elapsed() < AUTO_FOCUS_GRACE
                    && widget.upgrade().is_some_and(|w| focus.as_ref() == Some(&w))
            });
        if fresh {
            self.list.error_bell();
            return glib::Propagation::Stop;
        }
        // The card holding the focus, else the oldest pending one.
        let focused_request = focus.and_then(|f| {
            f.widget_name()
                .strip_prefix("permission-")
                .and_then(|r| r.parse::<u64>().ok())
        });
        let card = match focused_request {
            Some(request) => self
                .permissions
                .borrow()
                .iter()
                .find_map(|i| match i.entry() {
                    Entry::Permission(c)
                        if c.request == request && c.answer == PermissionAnswer::Pending =>
                    {
                        Some(c)
                    }
                    _ => None,
                }),
            None => self.pending_permission(),
        };
        let Some(card) = card else {
            return glib::Propagation::Proceed;
        };
        match permission_keys(&card.options)
            .into_iter()
            .find(|(k, _)| *k == key)
        {
            Some((_, id)) => self.answer_permission(card.request, Some(id)),
            None => self.list.error_bell(),
        }
        glib::Propagation::Stop
    }

    /// The agent withdrew permission request `request` (the tool use was aborted): its card ends
    /// cancelled, without an answer.
    fn withdraw_permission(&self, request: u64) {
        let item = self
            .permissions
            .borrow()
            .iter()
            .find(|item| {
                matches!(item.entry(), Entry::Permission(card)
                if card.request == request && card.answer == PermissionAnswer::Pending)
            })
            .cloned();
        let Some(item) = item else {
            return;
        };
        // The card's buttons go: if one has the focus, move it on as answering a card does.
        let name = format!("permission-{request}");
        if self.focus_widget().is_some_and(|f| f.widget_name() == name) {
            let next = self
                .permissions
                .borrow()
                .iter()
                .find_map(|i| match i.entry() {
                    Entry::Permission(c)
                        if c.answer == PermissionAnswer::Pending && c.request != request =>
                    {
                        Some(c.request)
                    }
                    _ => None,
                });
            match next {
                Some(next) => self.focus_permission(next),
                None => {
                    self.prompt.grab_focus();
                }
            }
        }
        item.with_entry(|e| {
            if let Entry::Permission(card) = e {
                card.answer = PermissionAnswer::Cancelled;
            }
        });
        item.notify_changed();
        self.dismiss_permission_toast(false);
    }

    /// Ends the visible state of a turn: thoughts are done, open permission cards cancelled.
    fn finish_turn_items(&self) {
        if let Some(stream) = self.stream.take() {
            stream.with_entry(|e| {
                if let Entry::Thought { done, .. } = e {
                    *done = true;
                }
            });
            self.mark_dirty(&stream);
        }
        let pending = self.permissions.borrow().iter().any(|item| {
            matches!(item.entry(), Entry::Permission(card) if card.answer == PermissionAnswer::Pending)
        });
        // The cards' buttons go: keep the focus in the pane, as answering a card does (else
        // GTK moves it to the card's selectable title and selects it).
        if pending && self.focus_in_panel() && !self.prompt_focused() {
            self.prompt.grab_focus();
        }
        self.dismiss_permission_toast(true);
        for item in self.permissions.borrow().iter() {
            let changed = item.with_entry(|e| match e {
                Entry::Permission(card) if card.answer == PermissionAnswer::Pending => {
                    card.answer = PermissionAnswer::Cancelled;
                    true
                }
                _ => false,
            });
            if changed {
                item.notify_changed();
            }
        }
    }

    fn turn_ended(&self, reason: StopReason) {
        self.finish_turn_items();
        let note = match reason {
            StopReason::EndTurn => None,
            StopReason::Cancelled => Some("Stopped.".to_owned()),
            StopReason::MaxTokens => Some("Stopped: the answer reached the token limit.".into()),
            StopReason::MaxTurnRequests => {
                Some("Stopped: the turn reached its request limit.".into())
            }
            StopReason::Refusal => Some(format!("{} declined to continue.", self.agent_label())),
        };
        if let Some(note) = note {
            self.push(Entry::Notice(note));
        }
        if let Some(state) = self.state.upgrade() {
            let (files, hunks) = review_ui::summary(&state.workspace);
            if hunks > 0 {
                // One live summary: the latest turn's, which counts everything still under
                // review (as the banner and the header do) and follows the review from now on.
                if let Some(old) = self.summary.take() {
                    self.remove_item(&old);
                }
                let item = self.push(Entry::Summary { files, hunks });
                self.summary.replace(Some(item));
            }
        }
        self.flush();
        self.refresh_header();
    }

    // ----- pickers and usage -----

    /// The session mode is also a config option (`mode`); an update of the options alone (the
    /// adapter's answer to `session/set_mode`) moves the mode too.
    fn sync_mode_option(&self) {
        let current = self
            .options
            .borrow()
            .iter()
            .find(|o| o.id == "mode")
            .map(|o| o.current.clone());
        if let Some(mode) = current {
            self.set_mode_value(&mode);
        }
    }

    /// Records the session's mode; a change to a mode Lemma does not offer is reported.
    fn set_mode_value(&self, mode: &str) {
        let changed = *self.mode.borrow() != mode;
        self.mode.replace(mode.to_owned());
        if changed && !OFFERED_MODES.contains(&mode) {
            self.push(Entry::Notice(format!(
                "The agent switched to the mode “{mode}”."
            )));
        }
    }

    fn refresh_pickers(&self) {
        self.updating.set(true);
        let modes: Vec<(String, String)> = self
            .modes
            .borrow()
            .iter()
            .filter(|(id, _)| OFFERED_MODES.contains(&id.as_str()))
            .cloned()
            .collect();
        let current = self.mode.borrow().clone();
        let mut shown = modes;
        if !shown.is_empty() && !shown.iter().any(|(id, _)| *id == current) && !current.is_empty() {
            // A mode the agent switched to by itself: shown, never offered.
            let name = self
                .modes
                .borrow()
                .iter()
                .find(|(id, _)| *id == current)
                .map_or_else(|| current.clone(), |(_, name)| name.clone());
            shown.push((current.clone(), name));
        }
        fill_picker(&self.mode_picker, &shown, &current);
        let options = self.options.borrow();
        for (picker, id) in [
            (&self.model_picker, "model"),
            (&self.effort_picker, "effort"),
        ] {
            match options.iter().find(|o| o.id == id) {
                Some(opt) => fill_picker(picker, &opt.values, &opt.current),
                None => fill_picker(picker, &[], ""),
            }
        }
        self.pickers.set_visible(
            self.mode_picker.get_visible()
                || self.model_picker.get_visible()
                || self.effort_picker.get_visible(),
        );
        self.updating.set(false);
    }

    fn picker_changed(&self, which: &str, selected: u32) {
        let Ok(index) = usize::try_from(selected) else {
            return;
        };
        let Some(controller) = self.controller() else {
            return;
        };
        if which == "mode" {
            let modes: Vec<String> = self
                .modes
                .borrow()
                .iter()
                .filter(|(id, _)| OFFERED_MODES.contains(&id.as_str()))
                .map(|(id, _)| id.clone())
                .collect();
            // The extra item of a mode Lemma does not offer is past the offered ones.
            if let Some(mode) = modes.get(index)
                && *mode != *self.mode.borrow()
                && self.command(&PanelCommand::SetMode(mode.clone()))
            {
                controller.set_mode(mode);
            }
            return;
        }
        let value = self
            .options
            .borrow()
            .iter()
            .find(|o| o.id == which)
            .and_then(|o| {
                let (value, _) = o.values.get(index)?;
                (*value != o.current).then(|| value.clone())
            });
        if let Some(value) = value {
            let forward = self.command(&PanelCommand::SetConfig {
                id: which.to_owned(),
                value: value.clone(),
            });
            if forward {
                controller.set_config(which, &value);
            }
        }
    }

    fn refresh_ring(&self) {
        let Some((used, size)) = self.usage.get() else {
            self.ring.set_visible(false);
            self.ring_label.set_visible(false);
            return;
        };
        let fraction = if size == 0 {
            0.0
        } else {
            (used as f64 / size as f64).clamp(0.0, 1.0)
        };
        self.ring.set_visible(true);
        let tooltip = format!(
            "Context: {} of {} tokens ({:.0}%)",
            group_digits(used),
            group_digits(size),
            fraction * 100.0
        );
        self.ring.set_tooltip_text(Some(&tooltip));
        self.ring.update_property(&[
            gtk::accessible::Property::Label("Context used"),
            gtk::accessible::Property::ValueMin(0.0),
            gtk::accessible::Property::ValueMax(100.0),
            gtk::accessible::Property::ValueNow((fraction * 100.0).round()),
            gtk::accessible::Property::ValueText(&tooltip),
        ]);
        self.ring_label
            .set_text(&format!("{:.0}%", fraction * 100.0));
        self.ring_label.set_tooltip_text(Some(&tooltip));
        self.ring_label.set_visible(true);
        for class in ["warning", "error"] {
            self.ring.remove_css_class(class);
        }
        if fraction >= 0.95 {
            self.ring.add_css_class("error");
        } else if fraction >= 0.8 {
            self.ring.add_css_class("warning");
        }
        self.ring.queue_draw();
    }

    fn draw_ring(&self, area: &gtk::DrawingArea, cr: &cairo::Context, w: i32, h: i32) {
        let Some((used, size)) = self.usage.get() else {
            return;
        };
        let fraction = if size == 0 {
            0.0
        } else {
            (used as f64 / size as f64).clamp(0.0, 1.0)
        };
        let color = area.color();
        let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
        let radius = cx.min(cy) - 2.0;
        cr.set_line_width(3.0);
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            0.2,
        );
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        let start = -std::f64::consts::FRAC_PI_2;
        cr.arc(
            cx,
            cy,
            radius,
            start,
            start + std::f64::consts::TAU * fraction,
        );
        let _ = cr.stroke();
    }

    // ----- the prompt box -----

    fn prompt_text(&self) -> String {
        let buffer = self.prompt.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    fn prompt_changed(self: &Rc<Self>) {
        let text = self.prompt_text();
        self.placeholder.set_visible(text.is_empty());
        let busy = self.controller().is_some_and(|c| is_busy(&c));
        self.send_button
            .set_sensitive(!busy && !text.trim().is_empty());
        self.update_suggestions();
    }

    fn prompt_key(
        self: &Rc<Self>,
        keyval: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        use gtk::gdk::{Key, ModifierType as M};
        let suggesting =
            self.suggest_revealer.reveals_child() && !self.suggestions.borrow().is_empty();
        if suggesting {
            match keyval {
                Key::Up | Key::Down => {
                    let n = self.suggestions.borrow().len() as i32;
                    let current = self.suggest_list.selected_row().map_or(-1, |r| r.index());
                    let next = if keyval == Key::Down {
                        (current + 1).rem_euclid(n)
                    } else {
                        (current - 1).rem_euclid(n)
                    };
                    if let Some(row) = self.suggest_list.row_at_index(next) {
                        self.suggest_list.select_row(Some(&row));
                    }
                    return glib::Propagation::Stop;
                }
                Key::Tab | Key::Return | Key::KP_Enter | Key::ISO_Enter => {
                    let index = self
                        .suggest_list
                        .selected_row()
                        .map_or(0, |r| usize::try_from(r.index()).unwrap_or(0));
                    self.accept_suggestion(index);
                    return glib::Propagation::Stop;
                }
                Key::Escape => {
                    self.hide_suggestions();
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        if matches!(keyval, Key::Return | Key::KP_Enter | Key::ISO_Enter)
            && !state.contains(M::SHIFT_MASK)
        {
            self.send_from_box();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    fn send_from_box(self: &Rc<Self>) {
        let text = self.prompt_text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if self.controller().is_some_and(|c| is_busy(&c)) {
            state.toast(&format!(
                "{} is still working on the last request; wait for it or press Stop",
                self.agent_label()
            ));
            return;
        }
        let text = trimmed.to_owned();
        self.prompt.buffer().set_text("");
        self.hide_suggestions();
        self.pending_prompt.replace(Some(text.clone()));
        let mentions = mentions(&text);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut blocks = vec![PromptBlock::Text(text)];
            let mut skipped = Vec::new();
            for mention in mentions {
                match attachment(&state, &mention).await {
                    Ok(Some(block)) => blocks.push(block),
                    Ok(None) => {}
                    Err(e) => skipped.push(e),
                }
            }
            for e in skipped {
                state.toast(&e);
            }
            if let Some(inner) = weak.upgrade()
                && !send_prompt(&state, blocks, PromptOrigin::Panel)
            {
                // Refused (the agent got busy meanwhile): the text goes back.
                inner.restore_pending_prompt();
            }
        });
    }

    /// Puts an unsent prompt back into the empty box after its turn failed to start.
    fn restore_pending_prompt(&self) {
        if let Some(text) = self.pending_prompt.take()
            && self.prompt_text().is_empty()
        {
            self.prompt.buffer().set_text(&text);
        }
    }

    // ----- suggestions -----

    /// The token before the cursor that suggestions complete: `/command` at the start of the
    /// prompt, or `@path` anywhere.
    fn current_token(&self) -> Option<(i32, TokenKind, String)> {
        let buffer = self.prompt.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let before = buffer
            .text(&buffer.start_iter(), &cursor, false)
            .to_string();
        if let Some(rest) = before.strip_prefix('/')
            && !rest.contains(char::is_whitespace)
        {
            return Some((0, TokenKind::Command, rest.to_owned()));
        }
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let token = &before[start..];
        let query = token.strip_prefix('@')?;
        let offset = before[..start].chars().count() as i32;
        Some((offset, TokenKind::File, query.to_owned()))
    }

    fn update_suggestions(self: &Rc<Self>) {
        let Some((offset, kind, query)) = self.current_token() else {
            self.hide_suggestions();
            return;
        };
        let suggestions = match kind {
            TokenKind::Command => {
                let query = query.to_lowercase();
                self.commands
                    .borrow()
                    .iter()
                    .filter(|(name, _)| name.to_lowercase().starts_with(&query))
                    .take(MAX_SUGGESTIONS)
                    .map(|(name, description)| Suggestion {
                        label: format!("/{name}"),
                        detail: description.clone(),
                        insert: format!("/{name} "),
                    })
                    .collect::<Vec<_>>()
            }
            TokenKind::File => match self.project_files() {
                Some(files) => match_files(&files, &query)
                    .into_iter()
                    .map(|path| Suggestion {
                        label: path.clone(),
                        detail: String::new(),
                        insert: format!("@{path} "),
                    })
                    .collect(),
                None => Vec::new(),
            },
        };
        self.token.set(Some((offset, kind)));
        self.show_suggestions(suggestions);
    }

    fn show_suggestions(&self, suggestions: Vec<Suggestion>) {
        while let Some(row) = self.suggest_list.row_at_index(0) {
            self.suggest_list.remove(&row);
        }
        for s in &suggestions {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let label = gtk::Label::builder()
                .label(s.label.as_str())
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build();
            row.append(&label);
            if !s.detail.is_empty() {
                let detail = gtk::Label::builder()
                    .label(s.detail.as_str())
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(pango::EllipsizeMode::End)
                    .build();
                detail.add_css_class("dim-label");
                detail.add_css_class("caption");
                row.append(&detail);
            }
            self.suggest_list.append(&row);
        }
        if let Some(first) = self.suggest_list.row_at_index(0) {
            self.suggest_list.select_row(Some(&first));
        }
        let any = !suggestions.is_empty();
        self.suggestions.replace(suggestions);
        self.suggest_revealer.set_reveal_child(any);
    }

    fn hide_suggestions(&self) {
        self.token.set(None);
        if !self.suggestions.borrow().is_empty() {
            self.show_suggestions(Vec::new());
        }
        self.suggest_revealer.set_reveal_child(false);
    }

    fn accept_suggestion(self: &Rc<Self>, index: usize) {
        let Some(suggestion) = self.suggestions.borrow().get(index).cloned() else {
            return;
        };
        let Some((offset, _)) = self.token.get() else {
            return;
        };
        let buffer = self.prompt.buffer();
        let mut start = buffer.iter_at_offset(offset);
        let mut cursor = buffer.iter_at_mark(&buffer.get_insert());
        buffer.delete(&mut start, &mut cursor);
        buffer.insert(&mut start, &suggestion.insert);
        self.hide_suggestions();
        self.prompt.grab_focus();
    }

    /// The project's text files relative to the root (cached for 30 s; loaded on a worker the
    /// first time, then the suggestions update).
    fn project_files(self: &Rc<Self>) -> Option<Rc<Vec<String>>> {
        let fresh = self
            .files
            .borrow()
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
            .map(|(_, files)| files.clone());
        if fresh.is_some() {
            return fresh;
        }
        let stale = self.files.borrow().as_ref().map(|(_, f)| f.clone());
        if !self.files_loading.replace(true)
            && let Some(state) = self.state.upgrade()
        {
            let root = state.workspace.root().path().to_path_buf();
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let files = gio::spawn_blocking(move || list_project_files(&root))
                    .await
                    .unwrap_or_default();
                if let Some(inner) = weak.upgrade() {
                    inner.files_loading.set(false);
                    inner.files.replace(Some((Instant::now(), Rc::new(files))));
                    if inner.token.get().is_some() {
                        inner.update_suggestions();
                    }
                }
            });
        }
        stale
    }

    // ----- the selection chip -----

    fn watch_selection(self: &Rc<Self>) {
        self.unwatch_selection();
        let Some(editor) = self.state.upgrade().and_then(|s| s.active_editor()) else {
            self.chip.set_visible(false);
            return;
        };
        let buffer = editor.buffer().clone();
        let mut handlers = Vec::new();
        let weak = Rc::downgrade(self);
        handlers.push(buffer.connect_has_selection_notify(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.refresh_chip();
            }
        }));
        let weak = Rc::downgrade(self);
        handlers.push(buffer.connect_mark_set(move |_, _, mark| {
            if matches!(mark.name().as_deref(), Some("insert" | "selection_bound"))
                && let Some(inner) = weak.upgrade()
            {
                inner.refresh_chip();
            }
        }));
        self.selection_watch
            .replace(Some((buffer.downgrade(), handlers)));
        self.refresh_chip();
    }

    fn unwatch_selection(&self) {
        if let Some((buffer, handlers)) = self.selection_watch.take()
            && let Some(buffer) = buffer.upgrade()
        {
            for id in handlers {
                buffer.disconnect(id);
            }
        }
    }

    fn refresh_chip(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(editor) = state.active_editor() else {
            self.chip.set_visible(false);
            return;
        };
        let buffer = editor.buffer();
        let Some((start, end)) = buffer.selection_bounds() else {
            self.chip.set_visible(false);
            return;
        };
        let (a, mut b) = (start.line() + 1, end.line() + 1);
        if end.starts_line() && b > a {
            b -= 1;
        }
        let name = editor.document().display_name();
        let lines = if a == b {
            format!("line {a}")
        } else {
            format!("lines {a}–{b}")
        };
        self.chip_label
            .set_text(&format!("Selection · {name}, {lines}"));
        self.chip.set_visible(true);
    }

    // ----- focus, links -----

    /// The window's focus widget (whether or not the window is active).
    fn focus_widget(&self) -> Option<gtk::Widget> {
        self.root.upgrade()?.root()?.focus()
    }

    /// True when the focus is somewhere in the pane.
    fn focus_in_panel(&self) -> bool {
        match (self.focus_widget(), self.root.upgrade()) {
            (Some(focus), Some(root)) => focus.is_ancestor(&root),
            _ => false,
        }
    }

    fn prompt_focused(&self) -> bool {
        self.focus_widget()
            .is_some_and(|f| f == *self.prompt.upcast_ref::<gtk::Widget>())
    }

    fn stop_focused(&self) -> bool {
        self.focus_widget()
            .is_some_and(|f| f == *self.stop_button.upcast_ref::<gtk::Widget>())
    }

    /// True while the focus is in a text field outside the pane (the editor, an entry): the
    /// user is typing there.
    fn typing_outside(&self) -> bool {
        !self.focus_in_panel()
            && self.focus_widget().is_some_and(|f| {
                f.is::<gtk::TextView>() || f.is::<gtk::Text>() || f.is::<gtk::Editable>()
            })
    }

    /// Focuses the pending permission card, else the prompt box (never Stop or a card's
    /// buttons, which Space and Enter would press).
    fn focus_best(&self) {
        self.dismiss_permission_toast(true);
        if let Some(card) = self.pending_permission() {
            self.focus_permission(card.request);
        } else {
            self.prompt.grab_focus();
        }
    }

    fn open_link(self: &Rc<Self>, widget: &gtk::Widget, uri: &str) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let lower = uri.to_ascii_lowercase();
        if ["http://", "https://", "mailto:"]
            .iter()
            .any(|p| lower.starts_with(p))
        {
            let window = widget.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(uri).launch(
                window.as_ref(),
                None::<&gio::Cancellable>,
                |result| {
                    if let Err(e) = result {
                        tracing::warn!("could not open a link: {e}");
                    }
                },
            );
            return;
        }
        // A project file, maybe with a line: `main.tex:12`, `main.tex#L12`, `file:///…`.
        let path = glib::filename_from_uri(uri)
            .map(|(p, _)| p.display().to_string())
            .unwrap_or_else(|_| uri.to_owned());
        let (path, line) = split_line(&path);
        let resolved = state.workspace.root().resolve(Path::new(path));
        match resolved {
            Ok(path) if path.is_file() => {
                glib::spawn_future_local(async move {
                    if let Err(e) =
                        crate::diagnostics_panel::open_at_line(&state, &path, line).await
                    {
                        state.toast(&e);
                    }
                });
            }
            _ => state.toast(&format!("Cannot open “{uri}”")),
        }
    }
}

/// What the set-up card shows.
enum SetupState {
    Hidden,
    Starting,
    Stopped,
    Failed {
        message: String,
        toolchain: Option<install::ToolchainError>,
    },
    Installing,
    InstallFailed(String),
}

/// The backend picker, the API key row and the cost label (WP5.2).
struct BackendUi {
    picker: gtk::DropDown,
    key_row: gtk::Box,
    key_label: gtk::Label,
    key_button: gtk::Button,
    cost_label: gtk::Label,
    /// Millionths of a dollar, this turn and this session (`AgentEvent::Cost`).
    turn_cost: Cell<u64>,
    session_cost: Cell<u64>,
    switch_dialog: RefCell<Option<adw::AlertDialog>>,
    key_dialog: RefCell<Option<adw::AlertDialog>>,
    /// Counts key lookups: only the latest one's answer is shown.
    key_gen: Cell<u64>,
    /// The backend the picker shows (the configuration's, or the one just chosen; the running
    /// session's while `deferred`).
    shown: Cell<AgentBackend>,
    /// `config.toml` names another backend than the session that keeps running here.
    deferred: Cell<bool>,
    /// The pane was shown: the key row may look the key up (the keyring may ask to be
    /// unlocked, so not while the window is built).
    key_wanted: Cell<bool>,
    /// True while the picker is set from the configuration (no switch).
    syncing: Cell<bool>,
}

/// The picker's labels, in [`AgentBackend::ALL`] order.
const BACKEND_LABELS: [&str; 3] = ["Claude Code", "Anthropic API", "Pi"];

fn build_backend_ui() -> BackendUi {
    let picker = gtk::DropDown::from_strings(&BACKEND_LABELS);
    picker.set_tooltip_text(Some("Agent"));
    picker.add_css_class("flat");
    picker.set_valign(gtk::Align::Center);
    let key_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(pango::EllipsizeMode::End)
        .build();
    key_label.add_css_class("caption");
    let key_button = gtk::Button::builder()
        .label("Set API key…")
        .valign(gtk::Align::Center)
        .build();
    key_button.add_css_class("flat");
    key_button.add_css_class("caption");
    let key_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    key_row.add_css_class("lemma-agent-key");
    key_row.append(&gtk::Image::from_icon_name("dialog-password-symbolic"));
    key_row.append(&key_label);
    key_row.append(&key_button);
    key_row.set_visible(false);
    let cost_label = gtk::Label::builder().visible(false).build();
    cost_label.add_css_class("caption");
    cost_label.add_css_class("dim-label");
    cost_label.add_css_class("numeric");
    BackendUi {
        picker,
        key_row,
        key_label,
        key_button,
        cost_label,
        turn_cost: Cell::new(0),
        session_cost: Cell::new(0),
        switch_dialog: RefCell::new(None),
        key_dialog: RefCell::new(None),
        key_gen: Cell::new(0),
        shown: Cell::new(AgentBackend::Claude),
        deferred: Cell::new(false),
        key_wanted: Cell::new(false),
        syncing: Cell::new(false),
    }
}

/// `micros` millionths of a dollar as `$0.0123` (four decimals below a dollar).
fn format_usd(micros: u64) -> String {
    let dollars = micros as f64 / 1e6;
    if dollars < 1.0 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

impl Inner {
    /// This window's backend: the configured one, except while a session that was running
    /// when `config.toml` chose another one goes on (see [`Self::config_backend_changed`]).
    fn backend(&self) -> AgentBackend {
        self.backend_ui.shown.get()
    }

    /// The backend `config.toml` (or this window's fallback setting) names.
    fn config_backend(&self) -> AgentBackend {
        self.state
            .upgrade()
            .map_or(AgentBackend::Claude, |s| s.config.agent.backend)
    }

    /// What messages call the agent ("Claude", "Pi").
    fn agent_label(&self) -> &'static str {
        super::bridge::agent_label(self.backend())
    }

    /// The prompt box and the empty transcript name the agent.
    fn refresh_names(&self) {
        let name = self.agent_label();
        let ask = format!("Ask {name}…");
        if self.placeholder.label() == ask {
            return;
        }
        self.placeholder.set_label(&ask);
        self.setup.start.set_label(&format!("_Start {name}"));
        self.prompt.update_property(&[
            gtk::accessible::Property::Label(&format!("Ask {name}")),
            gtk::accessible::Property::Placeholder(&ask),
        ]);
        if let Some(page) = self
            .stack
            .child_by_name("empty")
            .and_downcast::<adw::StatusPage>()
        {
            page.set_title(&format!("Ask {name}"));
            page.set_description(Some(&format!(
                "{name}'s changes show up as red/green hunks to accept or reject.\n\
                 Type / for commands and @ to attach a file."
            )));
        }
    }

    /// A session is up: a turn runs, or an idle one holds the conversation's context. (A start
    /// still on its way has nothing to lose yet.)
    fn session_live(&self) -> bool {
        self.controller()
            .is_some_and(|c| matches!(c.state(), AgentState::Idle | AgentState::Running { .. }))
    }

    /// The configured backend's name ("Claude", "Pi", …).
    fn backend_name(&self) -> &'static str {
        super::bridge::backend_name(self.backend())
    }

    fn connect_backend_ui(self: &Rc<Self>) {
        self.sync_backend_picker();
        let weak = Rc::downgrade(self);
        self.backend_ui
            .picker
            .connect_selected_notify(move |picker| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.backend_ui.syncing.get() {
                    return;
                }
                if let Some(backend) = AgentBackend::ALL.get(picker.selected() as usize) {
                    inner.request_backend(*backend);
                }
            });
        let weak = Rc::downgrade(self);
        self.backend_ui.key_button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.ask_api_key();
            }
        });
        if let Some(state) = self.state.upgrade() {
            let weak = Rc::downgrade(self);
            state.connect_config_changed(move |_, file| {
                if file == crate::settings::ConfigFile::Config
                    && let Some(inner) = weak.upgrade()
                {
                    inner.config_backend_changed();
                }
            });
        }
    }

    /// Shows the configured backend in the picker (and its key row), without switching.
    fn sync_backend_picker(self: &Rc<Self>) {
        let backend = self.config_backend();
        let ui = &self.backend_ui;
        ui.shown.set(backend);
        let index = AgentBackend::ALL
            .iter()
            .position(|b| *b == backend)
            .unwrap_or(0);
        ui.syncing.set(true);
        ui.picker.set_selected(index as u32);
        ui.syncing.set(false);
        self.refresh_key_status();
        self.refresh_header();
    }

    /// `config.toml` changed (in another Lemma window, or by hand): a new backend there applies
    /// at once only when no session runs here. A running one (a turn, or the context of an idle
    /// session) is never stopped without asking: the new backend applies when it is gone
    /// ([`Self::adopt_config_backend`]).
    fn config_backend_changed(self: &Rc<Self>) {
        let backend = self.config_backend();
        if backend == self.backend_ui.shown.get() {
            self.backend_ui.deferred.set(false);
            return;
        }
        if self.session_live() {
            self.backend_ui.deferred.set(true);
            self.push(Entry::Notice(format!(
                "config.toml now selects {new}. {old} goes on here; {new} starts after you stop \
                 it or start a new session.",
                new = super::bridge::backend_name(backend),
                old = self.backend_name(),
            )));
            return;
        }
        self.sync_backend_picker();
        self.backend_switched(backend);
    }

    /// The session that outlived a change of backend in `config.toml` is gone: the configured
    /// backend applies from now on (its next start, which reads the configuration).
    fn adopt_config_backend(self: &Rc<Self>) {
        self.backend_ui.deferred.set(false);
        let backend = self.config_backend();
        if backend == self.backend_ui.shown.get() {
            return;
        }
        install::clear_last_result();
        self.agent_name.replace(String::new());
        self.options.replace(Vec::new());
        self.modes.replace(Vec::new());
        self.pickers.set_visible(false);
        self.sync_backend_picker();
        self.push(Entry::Notice(format!(
            "Switched to {}. It starts with your next prompt.",
            super::bridge::backend_name(backend)
        )));
    }

    /// The user picked `backend`: switch, after asking while a turn runs.
    fn request_backend(self: &Rc<Self>, backend: AgentBackend) {
        if backend == self.backend_ui.shown.get() {
            return;
        }
        let busy = self.controller().is_some_and(|c| is_busy(&c));
        if !busy {
            self.switch_backend(backend);
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let to = super::bridge::backend_name(backend);
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Switch to {to}?"))
            .body(format!(
                "{} is working on a request. Switching stops it; the changes it made so far stay \
                 for review.",
                self.backend_name()
            ))
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "_Cancel"), ("switch", "_Switch")]);
        dialog.set_response_appearance("switch", adw::ResponseAppearance::Destructive);
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.backend_ui.switch_dialog.replace(None);
            if response == "switch" {
                inner.switch_backend(backend);
            } else {
                inner.sync_backend_picker();
            }
        });
        dialog.present(Some(&state.window));
        self.backend_ui.switch_dialog.replace(Some(dialog));
    }

    /// Writes `agent.backend` (or, when `config.toml` cannot be written, sets it for this window)
    /// and stops the running agent; the new one starts with the next prompt.
    fn switch_backend(self: &Rc<Self>, backend: AgentBackend) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.backend_ui.shown.set(backend);
        let written = state.edit_config_file(|text| {
            crate::prefs::set_keeping_comments(
                text,
                "agent",
                "backend",
                lemma_config::ConfigValue::Str(backend.as_str()),
            )
        });
        if let Err(e) = written {
            tracing::warn!("the agent backend was not saved: {e}");
            let mut config = (*state.config.get()).clone();
            config.agent.backend = backend;
            state.set_config(Rc::new(config));
        }
        self.backend_switched(backend);
    }

    /// The backend is now `backend`: stop the old agent and reset what belonged to it.
    fn backend_switched(self: &Rc<Self>, backend: AgentBackend) {
        self.backend_ui.deferred.set(false);
        self.backend_ui.shown.set(backend);
        // A start still running belongs to the old backend.
        self.start_gen.set(self.start_gen.get() + 1);
        self.starting.set(false);
        if self.command(&PanelCommand::SwitchBackend(backend))
            && let Some(controller) = self.controller()
        {
            controller.stop();
        }
        install::clear_last_result();
        self.reset_usage();
        self.agent_name.replace(String::new());
        self.options.replace(Vec::new());
        self.modes.replace(Vec::new());
        self.pickers.set_visible(false);
        self.user_stopped.set(false);
        self.hint.set_reveal_child(false);
        self.show_setup(SetupState::Hidden);
        self.push(Entry::Notice(format!(
            "Switched to {}. It starts with your next prompt.",
            super::bridge::backend_name(backend)
        )));
        self.refresh_header();
        self.refresh_key_status();
    }

    /// Shows the API key row for the API backend and looks up where the key comes from (off the
    /// main thread: the keyring may ask to be unlocked).
    fn refresh_key_status(self: &Rc<Self>) {
        let ui = &self.backend_ui;
        let api = ui.shown.get() == AgentBackend::Api;
        ui.key_row.set_visible(api);
        ui.key_gen.set(ui.key_gen.get() + 1);
        if !api {
            return;
        }
        if !ui.key_wanted.get() {
            ui.key_label.set_text("");
            return;
        }
        let generation = ui.key_gen.get();
        ui.key_label.set_text("Key: looking…");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let source = gio::spawn_blocking(|| lemma_agent::keys::lookup().map(|(_, s)| s))
                .await
                .ok()
                .flatten();
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let ui = &inner.backend_ui;
            if ui.key_gen.get() != generation {
                return;
            }
            ui.key_label.set_text(&match source {
                Some(source) => format!("Key: {}", source.describe()),
                None => "No API key".to_owned(),
            });
        });
    }

    /// "Set API key…": a dialog with a password entry; Save stores the key in the keyring.
    fn ask_api_key(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let entry = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .activates_default(true)
            .hexpand(true)
            .build();
        entry.set_widget_name("api-key-entry");
        let dialog = adw::AlertDialog::builder()
            .heading("Anthropic API key")
            .body(
                "Lemma keeps the key in the GNOME keyring (secret-tool), never in config.toml. \
                 ANTHROPIC_API_KEY, when set, is used first.",
            )
            .extra_child(&entry)
            .close_response("cancel")
            .default_response("save")
            .build();
        dialog.add_responses(&[("cancel", "_Cancel"), ("save", "_Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("save", false);
        let weak_dialog = dialog.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(dialog) = weak_dialog.upgrade() {
                let valid = lemma_agent::keys::ApiKey::new(&entry.text()).is_some();
                dialog.set_response_enabled("save", valid);
            }
        });
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.backend_ui.key_dialog.replace(None);
            let key = lemma_agent::keys::ApiKey::new(&entry.text());
            // The entry's text is not kept around.
            entry.set_text("");
            if response != "save" {
                return;
            }
            if let Some(key) = key {
                inner.store_api_key(key);
            }
        });
        dialog.present(Some(&state.window));
        self.backend_ui.key_dialog.replace(Some(dialog));
    }

    fn store_api_key(self: &Rc<Self>, key: lemma_agent::keys::ApiKey) {
        let ui = &self.backend_ui;
        ui.key_gen.set(ui.key_gen.get() + 1);
        ui.key_label.set_text("Key: storing…");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let stored = gio::spawn_blocking(move || lemma_agent::keys::store(&key))
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the keyring job panicked")));
            let Some(inner) = weak.upgrade() else {
                return;
            };
            match stored {
                Ok(()) => {
                    // A start that failed for want of a key can go now.
                    let failed = inner
                        .controller()
                        .is_some_and(|c| matches!(c.state(), AgentState::Failed(_)));
                    if failed && inner.backend() == AgentBackend::Api {
                        inner.user_stopped.set(false);
                        inner.start();
                    }
                }
                Err(e) => {
                    if let Some(state) = inner.state.upgrade() {
                        state.toast(&format!("The API key was not stored: {e}"));
                    }
                }
            }
            inner.refresh_key_status();
        });
    }

    /// Adds one API response's cost to the turn's and the session's.
    fn add_cost(&self, micros: u64) {
        let ui = &self.backend_ui;
        ui.turn_cost.set(ui.turn_cost.get().saturating_add(micros));
        ui.session_cost
            .set(ui.session_cost.get().saturating_add(micros));
        ui.cost_label.set_text(&format_usd(ui.turn_cost.get()));
        ui.cost_label.set_tooltip_text(Some(&format!(
            "This turn: {} · this session: {} (estimated from list prices)",
            format_usd(ui.turn_cost.get()),
            format_usd(ui.session_cost.get())
        )));
        ui.cost_label.set_visible(true);
    }

    /// Pi answered "authentication required": the hint to log in with `pi`.
    fn maybe_show_login_hint(&self, message: &str) {
        if self.backend() != AgentBackend::Pi || !lemma_agent::needs_login(message) {
            return;
        }
        let pi_found = self.state.upgrade().and_then(|st| {
            let paths = st.paths.clone()?;
            install::find_pi(&paths, &install::Env::from_process(&paths))
        });
        self.hint_label.set_markup(if pi_found.is_some() {
            "Pi needs a login: run <tt>pi</tt> once in a terminal and log in with \
             <tt>/login</tt> (or set up an API key there), then send your message again."
        } else {
            "Pi needs a login, and the <tt>pi</tt> command was not found: install it with \
             <tt>npm i -g @earendil-works/pi-coding-agent</tt>, run <tt>pi</tt> once in a \
             terminal to log in, then send your message again."
        });
        self.hint.set_reveal_child(true);
    }
}

fn build_setup() -> Setup {
    let spinner = adw::Spinner::new();
    spinner.set_visible(false);
    let icon = gtk::Image::new();
    icon.set_visible(false);
    let title = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    title.add_css_class("heading");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.append(&spinner);
    head.append(&icon);
    head.append(&title);
    let message = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .selectable(true)
        .width_chars(1)
        .build();
    let button = |label: &str, class: Option<&str>| {
        let b = gtk::Button::with_mnemonic(label);
        if let Some(class) = class {
            b.add_css_class(class);
        }
        b.set_visible(false);
        b
    };
    let install = button("_Install Adapter", Some("suggested-action"));
    let retry = button("_Retry", None);
    let cancel = button("_Cancel", None);
    let start = button("_Start Claude", Some("suggested-action"));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for b in [&install, &start, &retry, &cancel] {
        buttons.append(b);
    }
    let log = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::Char)
        .build();
    let log_scroller = gtk::ScrolledWindow::builder()
        .child(&log)
        .min_content_height(80)
        .max_content_height(160)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let log_expander = gtk::Expander::builder()
        .label("Output")
        .child(&log_scroller)
        .visible(false)
        .build();
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("lemma-agent-setup");
    card.append(&head);
    card.append(&message);
    card.append(&buttons);
    card.append(&log_expander);
    let revealer = gtk::Revealer::builder()
        .child(&card)
        .reveal_child(false)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    Setup {
        revealer,
        spinner,
        icon,
        title,
        message,
        install,
        retry,
        cancel,
        start,
        log_expander,
        log,
    }
}

/// Shows `values` (id, name) in `picker` with `current` selected; hides it without values.
fn fill_picker(picker: &gtk::DropDown, values: &[(String, String)], current: &str) {
    if values.is_empty() {
        picker.set_visible(false);
        return;
    }
    let names: Vec<&str> = values.iter().map(|(_, name)| name.as_str()).collect();
    let model = gtk::StringList::new(&names);
    let same = picker
        .model()
        .and_downcast::<gtk::StringList>()
        .is_some_and(|m| {
            m.n_items() as usize == names.len()
                && (0..m.n_items()).all(|i| m.string(i).as_deref() == Some(names[i as usize]))
        });
    if !same {
        picker.set_model(Some(&model));
    }
    let selected = values
        .iter()
        .position(|(id, _)| id == current)
        .map_or(gtk::INVALID_LIST_POSITION, |i| i as u32);
    picker.set_selected(selected);
    picker.set_visible(true);
}

/// `41250` → `41,250`.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `path:12` / `path#L12` → (`path`, `Some(12)`).
fn split_line(text: &str) -> (&str, Option<u32>) {
    if let Some((path, line)) = text.rsplit_once("#L")
        && let Ok(line) = line.split('-').next().unwrap_or("").parse()
    {
        return (path, Some(line));
    }
    if let Some((path, line)) = text.rsplit_once(':')
        && let Ok(line) = line.parse()
    {
        return (path, Some(line));
    }
    (text, None)
}

/// The `@path` mentions of a prompt (trailing punctuation dropped, duplicates removed).
pub fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'']);
        if !path.is_empty() && !out.iter().any(|p| p == path) {
            out.push(path.to_owned());
        }
    }
    out
}

/// The resource block for the mention `rel` (live text when the file is loaded); `Ok(None)` for
/// a mention that is no project file (an e-mail address, `@misc`, …).
async fn attachment(state: &Rc<WindowState>, rel: &str) -> Result<Option<PromptBlock>, String> {
    let path = match state.workspace.root().resolve(Path::new(rel)) {
        Ok(path) if path.is_file() => path,
        _ => return Ok(None),
    };
    let text = match state.workspace.get(&path) {
        Some(doc) => doc.text(),
        None => {
            let file = gio::File::for_path(&path);
            let (bytes, _) = file
                .load_contents_future()
                .await
                .map_err(|e| format!("Could not attach {rel}: {e}"))?;
            if bytes.len() > MAX_ATTACHMENT {
                return Err(format!("{rel} is too large to attach"));
            }
            match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(_) => return Err(format!("{rel} is not a text file; it was not attached")),
            }
        }
    };
    if text.len() > MAX_ATTACHMENT {
        return Err(format!("{rel} is too large to attach"));
    }
    Ok(Some(PromptBlock::Resource {
        uri: glib::filename_to_uri(&path, None)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| format!("file://{}", path.display())),
        mime: mime_for(&path).into(),
        text,
    }))
}

/// The MIME type of a resource.
pub fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tex" | "ltx" | "sty" | "cls" | "dtx") => "text/x-tex",
        Some("bib") => "text/x-bibtex",
        Some("md") => "text/markdown",
        _ => "text/plain",
    }
}

/// The project's text files (relative paths, sorted), skipping hidden directories, `.build` and
/// `node_modules`; at most `MAX_FILES`.
pub fn list_project_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            visited += 1;
            if visited > MAX_FILES * 10 || out.len() >= MAX_FILES {
                return out;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.display().to_string());
            }
        }
    }
    out.sort();
    out
}

/// The best matches of `query` among `files` (fuzzy, frizbee); the first files for an empty
/// query.
pub fn match_files(files: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        let mut first: Vec<&String> = files.iter().collect();
        // .tex files first.
        first.sort_by_key(|f| (!f.ends_with(".tex"), f.matches('/').count(), (*f).clone()));
        return first.into_iter().take(MAX_SUGGESTIONS).cloned().collect();
    }
    let mut matcher = frizbee::Matcher::new(query, &frizbee::Config::default());
    matcher
        .match_list(files)
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .filter_map(|m| files.get(m.index as usize).cloned())
        .collect()
}

/// Every descendant of `widget`, depth first.
fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![widget.as_ref().clone()];
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

/// The texts of the labels (and buttons, expanders) inside `widget`, in order.
fn widget_text(widget: &gtk::Widget) -> String {
    let mut parts = Vec::new();
    for w in descendants(widget) {
        if let Ok(label) = w.clone().downcast::<gtk::Label>() {
            let text = label.text();
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n")
}

/// Adds `win.agent-focus` and `win.agent-stop` with their default accelerators.
fn add_actions(state: &Rc<WindowState>) {
    let focus = gio::SimpleAction::new("agent-focus", None);
    let weak = Rc::downgrade(state);
    focus.connect_activate(move |_, _| {
        if let Some(state) = weak.upgrade() {
            focus_prompt(&state);
        }
    });
    state.window.add_action(&focus);
    let stop = gio::SimpleAction::new("agent-stop", None);
    let weak = Rc::downgrade(state);
    stop.connect_activate(move |_, _| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let Some(controller) = state.agent_controller().cloned() else {
            return;
        };
        let panel = get(&state);
        let forward = panel
            .as_ref()
            .is_none_or(|p| p.inner.command(&PanelCommand::Cancel));
        if !is_busy(&controller) || !forward {
            return;
        }
        let preparing = !controller.is_running();
        // A prompt that waits for the agent to start is dropped before it is sent.
        controller.cancel();
        if preparing && let Some(panel) = panel {
            panel.inner.restore_pending_prompt();
            panel.inner.refresh_header();
            state.toast("Stopped: the prompt was not sent");
        }
    });
    state.window.add_action(&stop);
    crate::app::set_default_accels(
        state,
        &[
            ("win.agent-focus", &["<Control>l"]),
            ("win.agent-stop", &["<Control><Shift>BackSpace"]),
        ],
    );
}

const CSS: &str = "
.lemma-agent-header { padding: 6px 6px 4px 12px; border-bottom: 1px solid alpha(currentColor, 0.1); }
.lemma-agent-header dropdown > button { padding: 2px 6px; min-height: 24px; font-size: 0.9em; }
.lemma-agent-transcript { background: none; }
.lemma-agent-transcript > row { padding: 4px 10px; background: none; }
.lemma-agent-transcript > row:hover, .lemma-agent-transcript > row:selected { background: none; }
.lemma-agent-user { background-color: alpha(@accent_bg_color, 0.12); border-radius: 10px; padding: 6px 10px; }
.lemma-agent-message label { line-height: 1.15; }
.lemma-agent-thought { font-style: italic; }
.lemma-md-code { background-color: alpha(currentColor, 0.06); border-radius: 6px; padding: 2px 8px 6px 8px; }
.lemma-md-code-text { font-size: 0.92em; }
.lemma-md-copy { min-height: 20px; min-width: 20px; padding: 0; }
.lemma-md-quote { border-left: 3px solid alpha(currentColor, 0.25); padding-left: 8px; }
.lemma-md-table { border: 1px solid alpha(currentColor, 0.15); border-radius: 4px; }
.lemma-md-table label { padding: 2px 6px; }
.lemma-tool-card { padding: 4px 8px; border-radius: 8px; background-color: alpha(currentColor, 0.04); }
.lemma-tool-link { color: @accent_color; }
.lemma-agent-plan { padding: 6px 10px; border-radius: 8px; background-color: alpha(currentColor, 0.04); }
.lemma-permission { padding: 8px 10px; border-radius: 10px; border: 1px solid alpha(@warning_color, 0.6);
                    background-color: alpha(@warning_color, 0.08); }
.lemma-permission:focus-visible { outline: 2px solid alpha(@accent_color, 0.8); outline-offset: -2px; }
.lemma-tool-quiet { padding: 0 8px; }
.lemma-agent-notice { font-size: 0.92em; opacity: 0.8; }
.lemma-agent-error { color: @error_color; }
.lemma-agent-setup { margin: 6px 8px; padding: 8px 10px; border-radius: 10px; background-color: alpha(currentColor, 0.05); }
.lemma-agent-setup textview { font-size: 0.85em; background: none; }
.lemma-agent-hint { margin: 0 8px 6px 8px; padding: 6px 8px; border-radius: 8px; background-color: alpha(@accent_bg_color, 0.12); }
.lemma-agent-prompt-area { padding: 6px 8px 6px 8px; border-top: 1px solid alpha(currentColor, 0.1); }
.lemma-agent-prompt { border-radius: 10px; border: 1px solid alpha(currentColor, 0.15); background-color: @view_bg_color; }
.lemma-agent-prompt:focus-within { border-color: alpha(@accent_color, 0.8); }
.lemma-agent-prompt textview, .lemma-agent-prompt textview text { background: none; }
.lemma-agent-chip { font-size: 0.85em; padding: 1px 8px; border-radius: 99px; background-color: alpha(@accent_bg_color, 0.15); }
.lemma-agent-suggest row { padding: 2px 8px; }
.lemma-agent-ring.warning { color: @warning_color; }
.lemma-agent-ring.error { color: @error_color; }
";

fn install_css() {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if DONE.replace(true) {
        return;
    }
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(id: &str, kind: &str) -> PermOption {
        PermOption {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn permission_keys_and_hidden_options() {
        let options = [
            opt("allow-once", "allow_once"),
            opt("allow-with-updates", "allow_always"),
            opt("reject", "reject_once"),
        ];
        assert_eq!(
            permission_keys(&options),
            [
                ('y', "allow-once".to_owned()),
                ('Y', "allow-with-updates".to_owned()),
                ('n', "reject".to_owned())
            ]
        );
        let plan = [
            opt("exit-plan-clear-auto", "allow_always"),
            opt("exit-plan-auto", "allow_always"),
            opt("exit-plan-bypass", "allow_always"),
            opt("exit-plan-accept-edits", "allow_always"),
            opt("exit-plan-default", "allow_once"),
            opt("reject", "reject_once"),
        ];
        let offered = offered_options(&plan);
        let ids: Vec<&str> = offered.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["exit-plan-default", "reject"]);
        assert_eq!(
            permission_keys(&offered),
            [
                ('y', "exit-plan-default".to_owned()),
                ('n', "reject".to_owned())
            ]
        );
    }

    #[test]
    fn tool_titles() {
        assert_eq!(
            tool_display("mcp__lemma__edit", "other"),
            ("Edit".to_owned(), "document-edit-symbolic")
        );
        assert_eq!(tool_display("mcp__lemma__compile", "other").0, "Compile");
        assert_eq!(
            tool_display("mcp__other__thing", "fetch"),
            ("other: thing".to_owned(), "image-x-generic-symbolic")
        );
        assert_eq!(
            tool_display("`ls -la`", "execute"),
            ("`ls -la`".to_owned(), "utilities-terminal-symbolic")
        );
        assert!(is_edit_tool("mcp__lemma__write", "other"));
        assert!(!is_edit_tool("mcp__lemma__read_file", "read"));
        // Every Lemma tool and every built-in tool the CLI may report by its bare name has a
        // name of its own and an icon of its own (not the generic "other" one).
        let lemma = [
            "read_file",
            "edit",
            "write",
            "list_files",
            "outline",
            "compile",
            "get_diagnostics",
            "get_selection",
            "render_page",
            "get_review_state",
        ];
        let builtin = [
            "Bash",
            "Read",
            "Grep",
            "Glob",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Task",
            "Agent",
            "ToolSearch",
            "ExitPlanMode",
        ];
        let titles = lemma
            .iter()
            .map(|t| format!("mcp__lemma__{t}"))
            .chain(builtin.iter().map(|t| (*t).to_owned()));
        for title in titles {
            let (name, icon) = tool_display(&title, "other");
            assert_ne!(name, title, "{title} has no name of its own");
            assert!(!name.contains("__"), "{title}: {name}");
            assert_ne!(icon, "system-run-symbolic", "{title} has the generic icon");
        }
        assert_eq!(
            tool_display("ToolSearch", "other"),
            ("Loading tools".to_owned(), "system-search-symbolic")
        );
        assert_eq!(tool_display("Bash", "execute").0, "Run a command");
        assert_eq!(
            tool_display("WebFetch", "fetch"),
            ("Fetch a web page".to_owned(), "web-browser-symbolic")
        );
        assert!(is_quiet_tool("ToolSearch") && !is_quiet_tool("Bash"));
        // The adapter's own titles stay, with the icon of their kind.
        assert_eq!(
            tool_display("Read /home/u/main.tex", "read"),
            ("Read /home/u/main.tex".to_owned(), "document-open-symbolic")
        );
        // A NUL never reaches a label.
        assert_eq!(tool_display("a\0b", "other").0, "a\u{FFFD}b");
    }

    #[test]
    fn chunks_of_text() {
        assert_eq!(chunk_text("", 2, 100), [""]);
        assert_eq!(chunk_text("a\nb\nc", 2, 100), ["a\nb", "c"]);
        assert_eq!(chunk_text("a\nb\nc\nd\n", 2, 100), ["a\nb", "c\nd\n"]);
        // A part also ends after `max_bytes`, at a line break.
        assert_eq!(chunk_text("aaaa\nb\nc", 9, 4), ["aaaa", "b\nc"]);
        // Growing text changes only the last part.
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        for end in 1..text.len() {
            let (a, b) = (
                chunk_text(&text[..end], 16, 1500),
                chunk_text(&text, 16, 1500),
            );
            assert_eq!(a[..a.len() - 1], b[..a.len() - 1], "{end}");
        }
    }

    #[test]
    fn mentions_and_lines() {
        assert_eq!(
            mentions("look at @main.tex, and @chapters/yoneda.tex. Also @main.tex! me@x.org"),
            ["main.tex", "chapters/yoneda.tex"]
        );
        assert!(mentions("no @ here").is_empty());
        assert_eq!(split_line("main.tex:12"), ("main.tex", Some(12)));
        assert_eq!(split_line("a/b.tex#L3-L5"), ("a/b.tex", Some(3)));
        assert_eq!(split_line("a/b.tex"), ("a/b.tex", None));
        assert_eq!(group_digits(41250), "41,250");
        assert_eq!(group_digits(200000), "200,000");
        assert_eq!(group_digits(999), "999");
        assert_eq!(summary_text(1, 1), "1 change in 1 file to review");
        assert_eq!(summary_text(2, 4), "4 changes in 2 files to review");
    }

    #[test]
    fn file_matching() {
        let files: Vec<String> = ["chapters/yoneda.tex", "main.tex", "refs.bib", "notes.md"]
            .map(String::from)
            .to_vec();
        assert_eq!(match_files(&files, "")[0], "main.tex");
        assert_eq!(match_files(&files, "yon")[0], "chapters/yoneda.tex");
        assert_eq!(match_files(&files, "bib")[0], "refs.bib");
        assert!(match_files(&files, "zzzzq").is_empty());
    }
}
