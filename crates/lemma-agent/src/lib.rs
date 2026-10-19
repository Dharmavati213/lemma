//! Agent thread (ACP client over futures_executor::block_on, no tokio) + in-process MCP server on a unix socket.
//! ALL agent-client-protocol usage lives in acp.rs; nothing else in the workspace imports that crate.
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

mod acp;
pub mod keys;
pub mod mcp;
/// The API-key backend (WP5.1): Anthropic's Messages API, same [`AgentHandle`] as [`spawn`].
pub mod messages;

pub use acp::{
    ELEVATED_MODES, EXIT_GRACE, INSTRUCTIONS_TAG, OFFERED_MODES, STDERR_RING_BYTES, TERM_GRACE,
};

#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// The program that speaks ACP on stdio (absolute): `node` for an npm adapter (Claude Code,
    /// Pi, Codex), the agent's own command line tool (`grok`, `gemini`, …), or
    /// `lemma-fake-agent` in tests.
    pub program: PathBuf,
    /// Its arguments: the adapter's script
    /// (`~/.local/share/lemma/acp/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js`),
    /// `agent stdio` for Grok Build, a scenario for the fake agent.
    pub args: Vec<OsString>,
    /// Environment variables set for it (`CLAUDE_CODE_EXECUTABLE`, `CODEX_PATH`, the user's
    /// `[agent.<backend>] env`).
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,        // project root
    pub mcp_bridge: PathBuf, // the lemma-mcp binary next to `lemma`
    pub mcp_socket: PathBuf, // $XDG_RUNTIME_DIR/lemma/mcp-<pid>.sock (dir 0700)
    pub mcp_token: String,   // 32 hex chars from /dev/urandom
    /// Lemma's instructions for the agent (see [`AcpFlavor`] for how they reach it).
    pub system_prompt_append: String,
    pub raw_log: Option<PathBuf>, // debug only; contains account info, never commit
    /// Which kind of ACP agent `program` is (WP5.2): see [`AcpFlavor`].
    pub flavor: AcpFlavor,
}
/// The kind of ACP agent [`spawn`] runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AcpFlavor {
    /// Claude Code's adapter: its own `Edit`/`Write` are disallowed in `session/new`, so every
    /// change goes through Lemma's MCP tools; `system_prompt_append` goes in `session/new`'s
    /// `_meta.systemPrompt.append`; the client advertises no `fs` capability; only the modes
    /// [`OFFERED_MODES`] are offered; `ANTHROPIC_API_KEY` is removed from its environment.
    #[default]
    Claude,
    /// Any other agent (Pi, Codex, Grok Build, Gemini CLI, …): the client also advertises
    /// `fs.readTextFile`/`fs.writeTextFile`, so an agent that delegates its own read/write/edit
    /// tools reaches the editor as [`EditorRequest::ReadText`] and [`EditorRequest::WriteText`]
    /// calls on `handle.tools`. ACP has no system prompt, so `system_prompt_append` is sent as
    /// the first block of the session's first prompt. Every mode the agent has is offered
    /// except [`ELEVATED_MODES`].
    Generic,
}

impl AcpFlavor {
    /// True when Lemma offers the session mode `mode` with this kind of agent (risk §9.6):
    /// with Claude Code only [`OFFERED_MODES`], with other agents any but [`ELEVATED_MODES`].
    pub fn offers_mode(self, mode: &str) -> bool {
        match self {
            Self::Claude => OFFERED_MODES.contains(&mode),
            Self::Generic => !mode.is_empty() && !ELEVATED_MODES.contains(&mode),
        }
    }
}
/// Appended to an `Error` message when the agent answered with ACP's "authentication required"
/// error (-32000): the agent needs its own login (Pi: run `pi` once in a terminal).
pub const LOGIN_REQUIRED: &str = "(the agent needs a login)";
/// True when `message` (an [`AgentEvent::Error`] or a failed start) says the agent needs a login.
pub fn needs_login(message: &str) -> bool {
    message.contains(LOGIN_REQUIRED)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptBlock {
    Text(String),
    Resource {
        uri: String,
        mime: String,
        text: String,
    },
    /// An image the user attached (PNG, JPEG, GIF or WebP): `data` holds the encoded bytes,
    /// `uri` the file it came from (`file://…`; empty for a pasted image).
    ///
    /// ACP: an `image` block when the agent advertises the `image` prompt capability, otherwise a
    /// `resource_link` to the file (or a note for a pasted image). API backend: an `image` block.
    Image {
        uri: String,
        mime: String,
        data: Vec<u8>,
    },
    /// A file the user attached that the agent gets by reference, at `uri` (`file://…`): a PDF
    /// (`data` holds its bytes when it is small enough to include) or a text file too large to
    /// include (`data` empty).
    ///
    /// ACP: a `resource_link`, so the agent reads the file with its own tools (Claude Code's Read
    /// handles PDFs and pages through long files). API backend: a PDF with `data` becomes a
    /// `document` block; anything else a note naming the file.
    Document {
        uri: String,
        mime: String,
        data: Vec<u8>,
    },
}

impl PromptBlock {
    /// The file name at the end of `uri`, percent-decoded (`file:///p/my%20fig.png` →
    /// `my fig.png`), or `uri` itself when it has none.
    pub fn file_name(uri: &str) -> String {
        let path = uri.split(['#', '?']).next().unwrap_or(uri);
        match path.rsplit('/').next() {
            Some(name) if !name.is_empty() => percent_decode(name),
            _ => uri.to_owned(),
        }
    }
}

/// `%XX` escapes decoded (invalid ones kept as they are; invalid UTF-8 replaced).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'?'));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentCmd {
    Prompt(Vec<PromptBlock>),
    /// session/cancel; also answers every pending permission request with Cancelled.
    Cancel,
    /// None = Cancelled outcome.
    Permission {
        request: u64,
        option_id: Option<String>,
    },
    SetMode(String),
    SetConfig {
        id: String,
        value: String,
    },
    Shutdown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}
/// A session config option (ACP `configOptions`): `values` are `(value, display name)`. The
/// `mode` option lists only the modes Lemma offers ([`OFFERED_MODES`]). A boolean option has the
/// values `"true"`/`"false"` (send them as strings in [`AgentCmd::SetConfig`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigOpt {
    pub id: String,
    pub name: String,
    pub current: String,
    pub values: Vec<(String, String)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermOption {
    pub id: String,
    pub name: String,
    pub kind: String, /*allow_once|allow_always|reject_once|reject_always*/
}
/// What the agent thread reports, in order. `Ready` comes first (updates that arrive during
/// start-up follow it), `Exited` comes last and the channel then closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// The session exists. `agent` is the agent's `"name version"` from `initialize`; `modes` are
    /// `(id, name)` limited to [`OFFERED_MODES`]; `mode` is the current one (a session that starts
    /// in another mode is switched to `default` first; if the agent switched to such a mode during
    /// start-up, `mode` says so, and a `Notice` and the switch back to `default` follow).
    Ready {
        session: String,
        agent: String,
        modes: Vec<(String, String)>,
        mode: String,
        options: Vec<ConfigOpt>,
    },
    MessageChunk(String),
    ThoughtChunk(String),
    /// `kind` is the ACP tool kind: `read`, `edit`, `delete`, `move`, `search`, `execute`,
    /// `think`, `fetch`, `switch_mode` or `other` (Lemma's MCP tools are `other`, titled
    /// `mcp__lemma__<tool>`).
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: ToolStatus,
    },
    /// Only sent when the title or the status changed.
    ToolUpdate {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
    },
    /// The whole plan, `(content, status)` with status `pending`, `in_progress` or `completed`.
    Plan(Vec<(String, String)>),
    Commands(Vec<(String, String)>), // (name, description)
    /// The current mode changed, or a `SetMode` was confirmed.
    ModeChanged(String),
    /// The options changed, or a `SetConfig` was confirmed (the agent's full list).
    ConfigChanged(Vec<ConfigOpt>),
    /// Answer with [`AgentCmd::Permission`] and this `request` (unique in the session). Options
    /// that would switch to a mode Lemma does not offer are left out.
    Permission {
        request: u64,
        title: String,
        options: Vec<PermOption>,
    },
    /// The agent withdrew permission request `request` before it was answered (ACP
    /// `$/cancel_request`, e.g. when the tool use was aborted); Lemma answered it as cancelled.
    /// Its card can no longer be answered: an [`AgentCmd::Permission`] for it is ignored.
    PermissionWithdrawn {
        request: u64,
    },
    /// Exactly once per accepted `Prompt`: the agent's stop reason, `Cancelled` after `Cancel` or
    /// `Shutdown`, `EndTurn` when the turn ended without one (an `Error` comes first when the agent
    /// answered the prompt with an error).
    TurnEnded(StopReason),
    /// A message for the user (start-up failures carry the agent's stderr tail).
    Error(String),
    /// The agent process ended: its exit code, `None` when a signal ended it. The last event.
    Exited(Option<i32>),
    /// Context-window usage from ACP `usage_update`: `used` of `size` tokens.
    Usage {
        used: u64,
        size: u64,
    },
    /// An informational message from the agent (ACP `notice`), shown in the transcript.
    Notice(String),
    /// What one API response cost (the API-key backend, [`messages`]): its token counts and an
    /// estimate from the model's list prices. A turn may make several requests: a turn's cost
    /// is the sum of the `Cost`s after its `Prompt`.
    Cost {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        usd_estimate: Usd,
    },
}

/// An amount of US dollars in millionths (integer, so events stay `Eq`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Usd(pub u64);

impl Usd {
    /// The amount in dollars.
    #[allow(clippy::cast_precision_loss)]
    pub fn dollars(self) -> f64 {
        self.0 as f64 / 1e6
    }
}

impl std::ops::Add for Usd {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl std::ops::AddAssign for Usd {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

impl std::fmt::Display for Usd {
    /// `$1.23` from a dollar up, `$0.0123` below.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 >= 1_000_000 {
            write!(f, "${:.2}", self.dollars())
        } else {
            write!(f, "${:.4}", self.dollars())
        }
    }
}

/// Requests from MCP tools (and ACP fs/* for other agents) to the GTK main thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorRequest {
    ReadFile {
        path: PathBuf,
        offset: Option<u32>,
        limit: Option<u32>,
    },
    Edit {
        path: PathBuf,
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    ListFiles,
    Outline {
        path: Option<PathBuf>,
    },
    Compile,
    Diagnostics {
        path: Option<PathBuf>,
    },
    Selection,
    RenderPage {
        page: u32,
    },
    ReviewState,
    /// ACP `fs/read_text_file` ([`AcpFlavor::Generic`]): the live text, raw (no line numbers,
    /// no review feedback), from the 1-based `line`, at most `limit` lines.
    ReadText {
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
    },
    /// ACP `fs/write_text_file` ([`AcpFlavor::Generic`]): like [`EditorRequest::Write`] (minimal
    /// edits as reviewable hunks), but its reply carries no review feedback (ACP has no place
    /// for it; it stays queued for the next tool reply or prompt).
    WriteText {
        path: PathBuf,
        content: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolContent {
    Text(String),
    Png(Vec<u8>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolReply {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
}
/// `call` is unique per MCP tools/call and becomes Origin::Agent { call }.
/// The main thread must send exactly one reply (std SyncSender, capacity 1: never blocks the sender).
pub struct ToolCall {
    pub call: u64,
    pub req: EditorRequest,
    pub reply: SyncSender<ToolReply>,
}

pub struct AgentHandle {
    pub cmd: async_channel::Sender<AgentCmd>,
    pub events: async_channel::Receiver<AgentEvent>,
    pub tools: async_channel::Receiver<ToolCall>,
}
/// Starts the MCP socket server and the "agent" thread (ACP connect_with inside futures_executor::block_on).
/// session/new: cwd, mcpServers=[Stdio{name:"lemma", command: mcp_bridge, env:[LEMMA_MCP_SOCKET, LEMMA_MCP_TOKEN]}],
/// _meta={"claudeCode":{"options":{"disallowedTools":["Edit","Write","NotebookEdit"],"allowedTools":["mcp__lemma__*"]}},
///        "systemPrompt":{"append": system_prompt_append}}.
///
/// - **Process:** `program args` with `env` (and without `CLAUDECODE`; for [`AcpFlavor::Claude`] also without
///   `ANTHROPIC_API_KEY`), in `cwd`, as the leader of its own process group. A process that cannot be started
///   is an `Err` here.
/// - **MCP server:** started on `mcp_socket` with `mcp_token`, its tool calls arriving on `handle.tools`, and
///   stopped when the session ends. If something already accepts connections on `mcp_socket` (the caller
///   started its own [`mcp::McpServer`]), none is started: the caller's server gets the calls and
///   `handle.tools` is closed at once.
/// - **Events:** `Ready` first, `Exited(code)` last, then the channel closes; one `TurnEnded` per accepted
///   `Prompt`. Commands sent after `Exited` fail (the command channel is closed). Dropping every command
///   sender shuts the agent down like `Shutdown`. Details (modes and permission options Lemma does not
///   offer, the stderr ring, the shutdown sequence): the module docs of `src/acp.rs`.
pub fn spawn(cfg: AgentConfig) -> std::io::Result<AgentHandle> {
    acp::spawn(cfg)
}

#[cfg(test)]
mod tests {
    use super::PromptBlock;

    #[test]
    fn file_names_of_uris() {
        assert_eq!(PromptBlock::file_name("file:///p/fig.png"), "fig.png");
        assert_eq!(
            PromptBlock::file_name("file:///p/my%20fig%C3%A9.pdf#page=2"),
            "my figé.pdf"
        );
        assert_eq!(PromptBlock::file_name("file:///p/100%.txt"), "100%.txt");
        assert_eq!(PromptBlock::file_name(""), "");
        assert_eq!(PromptBlock::file_name("file:///p/"), "file:///p/");
    }
}
