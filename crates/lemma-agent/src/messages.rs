//! The API-key backend (WP5.1): Anthropic's Messages API over blocking HTTP (`ureq`, rustls), no
//! tokio. [`spawn`] returns the same [`AgentHandle`] as the ACP backend: the same
//! [`AgentEvent`]s, the same [`AgentCmd`]s, and Lemma's tools as [`ToolCall`]s on `handle.tools`
//! (served by the editor's main-thread handlers, exactly like MCP calls).
//!
//! # Threads
//!
//! | Thread | Job |
//! |---|---|
//! | `lemma-api` | the conversation: requests, the tool loop, commands |
//! | `lemma-api-cmd` | forwards `handle.cmd` into the `lemma-api` inbox |
//! | `lemma-api-http` | one per request: sends it and parses the SSE stream into the inbox |
//!
//! **Cancel** marks the running request's HTTP thread cancelled and ends the turn at once with
//! `TurnEnded(Cancelled)`; that thread drops the response (closing the connection) at its next
//! read, which the API's `ping` events keep short. A late event from it is ignored.
//!
//! # Requests
//!
//! `POST {base}/v1/messages` ([`DEFAULT_BASE_URL`], or [`BASE_URL_ENV`] for tests) with
//! `x-api-key` (a sensitive header, never logged), `anthropic-version: 2023-06-01` and
//! `content-type: application/json`; the body (see [`request_body`]) streams, asks for
//! [`MAX_TOKENS`], carries a top-level `cache_control`, the byte-stable [`tool_schemas`] and
//! [`SYSTEM_PROMPT`], `tool_choice: auto`, and per model ([`MODELS`]): adaptive summarized
//! thinking and `output_config.effort` (Opus and Sonnet 5.5; never for Haiku 4.5), and
//! `"fallbacks": "default"` with `anthropic-beta: server-side-fallback-2026-07-01`.
//!
//! # History
//!
//! Append-only: earlier messages are never edited or reordered, and each response is echoed
//! back as it arrived (thinking blocks with their signatures byte for byte), with these
//! exceptions, all at the tail:
//! - a response that was cut off (cancel, a broken connection) is never added;
//! - empty text blocks and thinking blocks without a signature (cut off) are left out, and so are
//!   the `fallback` marker and, before the last such marker, the declined model's thinking and
//!   tool-use blocks (the API's rule for echoing a mid-response fallback);
//! - a `tool_use` whose input is not valid JSON is echoed with input `{}`;
//! - a refusal is never added; when it answered the user's prompt itself, that prompt is
//!   dropped too, so that the next prompt starts clean.
//!
//! Every `tool_use` that is echoed gets its `tool_result` in the next user message, also when it
//! was not run (`max_tokens`, cancel), so the history always stays valid. A prompt after a turn
//! that ended without an answer (cancel, an error) is a second user message in a row, which the
//! API joins into one turn.
//!
//! # Tool loop
//!
//! On `stop_reason == "tool_use"` (never on `max_tokens` or `refusal`), each `tool_use` input,
//! accumulated from its `input_json_delta`s, is parsed strictly with `serde_json` and checked with
//! [`mcp::parse_tool_call`]; a failure is answered with an error `tool_result`
//! `{"INVALID_JSON": "<raw>"}` (plus `"error"` for a schema failure) and the tool is not run. The
//! valid calls run in order through `handle.tools`, each with [`mcp::Timeouts`]; all results go
//! back in one user message (images as base64 PNG `image` blocks).
//!
//! # Errors
//!
//! 429, 5xx (529 included), connection errors and a retryable `error` event before any output
//! are retried at most [`MAX_RETRIES`] times, honouring `retry-after-ms`/`retry-after`, with a
//! `Notice` each time; 4xx (401: "API key rejected") are not. A failed turn reports `Error`, then
//! `TurnEnded(EndTurn)`.

use crate::keys::ApiKey;
use crate::{
    AgentCmd, AgentEvent, AgentHandle, ConfigOpt, PromptBlock, StopReason, ToolCall, ToolContent,
    ToolReply, ToolStatus, Usd, mcp,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// The real API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Overrides [`DEFAULT_BASE_URL`] (tests point it at a local fake server).
pub const BASE_URL_ENV: &str = "LEMMA_ANTHROPIC_BASE_URL";
/// The `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The `anthropic-beta` header for `"fallbacks": "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// `max_tokens` of every request.
pub const MAX_TOKENS: u32 = 64_000;
/// Retries of one request (429, 5xx, connection errors).
pub const MAX_RETRIES: u32 = 3;
/// The longest wait before a retry, whatever `retry-after` says.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
/// Requests in one turn before it ends with `MaxTurnRequests`.
pub const MAX_REQUESTS_PER_TURN: u32 = 200;
/// The model used when the configuration names none (or one not in [`MODELS`]).
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// The effort used when the configuration names none (or an unknown one).
pub const DEFAULT_EFFORT: &str = "high";
/// Lemma's tools keep the names they have under Claude Code (`mcp__lemma__edit`, …), so the
/// prompts of Ctrl+K, quick actions and "Ask agent to fix" work unchanged.
pub const TOOL_PREFIX: &str = "mcp__lemma__";
/// The agent name in `Ready`.
pub const AGENT_NAME: &str = "Anthropic API";

/// The §10.4 APPEND text, adapted to this backend (no Bash, no built-in file tools). Byte-stable:
/// no timestamps, no paths, so the prompt cache holds.
pub const SYSTEM_PROMPT: &str = "You are working inside Lemma, a LaTeX editor, on the user's \
LaTeX project. The user reviews every change you make as red/green hunks. Make ALL file changes \
with mcp__lemma__edit or mcp__lemma__write. mcp__lemma__read_file returns the live editor buffer, \
including unsaved changes; paths are relative to the project root. Build with mcp__lemma__compile \
and read errors with mcp__lemma__get_diagnostics. Tool results may contain a <review_feedback> \
block listing hunks the user accepted, rejected or modified; do not re-apply rejected changes \
unless asked. Follow the project's existing macros and style.";

/// List prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// 5-minute cache writes: 1.25 × input.
    pub cache_write: f64,
}

/// A model this backend offers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    /// Takes `output_config.effort` (and is offered the `effort` option).
    pub effort: bool,
    /// Takes `thinking: {"type": "adaptive", "display": "summarized"}`.
    pub adaptive_thinking: bool,
    /// Takes `"fallbacks": "default"` (server-side refusal fallback).
    pub fallbacks: bool,
    /// The context window in tokens (`Usage.size`).
    pub context: u64,
    pub prices: Prices,
}

/// The models, in picker order; exact IDs without date suffixes.
pub const MODELS: &[Model] = &[
    Model {
        id: "claude-opus-5-5",
        name: "Opus 5.5",
        effort: true,
        adaptive_thinking: true,
        fallbacks: true,
        context: 1_000_000,
        prices: Prices {
            input: 4.0,
            output: 20.0,
            cache_read: 0.20,
            cache_write: 5.0,
        },
    },
    Model {
        id: "claude-sonnet-5-5",
        name: "Sonnet 5.5",
        effort: true,
        adaptive_thinking: true,
        fallbacks: true,
        context: 1_000_000,
        prices: Prices {
            input: 2.0,
            output: 10.0,
            cache_read: 0.20,
            cache_write: 2.5,
        },
    },
    Model {
        id: "claude-haiku-4-5",
        name: "Haiku 4.5",
        effort: false,
        adaptive_thinking: false,
        fallbacks: false,
        context: 200_000,
        prices: Prices {
            input: 1.0,
            output: 5.0,
            cache_read: 0.10,
            cache_write: 1.25,
        },
    },
];

/// The effort levels, `(value, display name)`.
pub const EFFORTS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
];

/// The model with this exact ID.
pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// Token counts of one response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    /// The estimated price of these tokens at `prices`.
    pub fn cost(&self, prices: &Prices) -> Usd {
        #[allow(clippy::cast_precision_loss)]
        let micros = (self.input as f64 * prices.input
            + self.output as f64 * prices.output
            + self.cache_read as f64 * prices.cache_read
            + self.cache_write as f64 * prices.cache_write)
            .round();
        // Tokens × dollars per million tokens = millionths of a dollar.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Usd(micros.max(0.0) as u64)
    }

    fn merge(&mut self, usage: &Value) {
        let set = |field: &mut u64, key: &str| {
            if let Some(n) = usage.get(key).and_then(Value::as_u64) {
                *field = n;
            }
        };
        set(&mut self.input, "input_tokens");
        set(&mut self.output, "output_tokens");
        set(&mut self.cache_read, "cache_read_input_tokens");
        set(&mut self.cache_write, "cache_creation_input_tokens");
    }
}

/// What [`spawn`] needs. `Debug` never shows the key.
#[derive(Clone, Debug)]
pub struct MessagesConfig {
    pub api_key: ApiKey,
    /// One of [`MODELS`] ([`DEFAULT_MODEL`] otherwise, with a `Notice`).
    pub model: String,
    /// One of [`EFFORTS`] ([`DEFAULT_EFFORT`] otherwise); ignored for models without effort.
    pub effort: String,
    /// The `system` prompt ([`SYSTEM_PROMPT`]).
    pub system_prompt: String,
    /// `None`: [`BASE_URL_ENV`] if set, else [`DEFAULT_BASE_URL`].
    pub base_url: Option<String>,
    /// How long a tool call may take in the editor.
    pub timeouts: mcp::Timeouts,
    /// The first retry's wait when the answer has no `retry-after`; it doubles per retry.
    pub retry_base: Duration,
    /// Debug only: request bodies and SSE lines (never headers, never the key).
    pub raw_log: Option<PathBuf>,
    /// A response that sends nothing (not even a `ping`) for this long is a broken connection
    /// ([`DEFAULT_IDLE_TIMEOUT`]).
    pub idle_timeout: Duration,
    /// Looks the key up again after "API key rejected (401)": a different key (one stored since
    /// the session started) is used from then on.
    pub key_lookup: Option<fn() -> Option<ApiKey>>,
}

/// How long a response may stay silent: the API sends `ping` events while it works.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

impl MessagesConfig {
    /// The defaults: [`DEFAULT_MODEL`], [`DEFAULT_EFFORT`], [`SYSTEM_PROMPT`], 1 s retry base.
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            api_key,
            model: DEFAULT_MODEL.to_owned(),
            effort: DEFAULT_EFFORT.to_owned(),
            system_prompt: SYSTEM_PROMPT.to_owned(),
            base_url: None,
            timeouts: mcp::Timeouts::default(),
            retry_base: Duration::from_secs(1),
            raw_log: None,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            key_lookup: None,
        }
    }

    /// The URL requests go to.
    pub fn url(&self) -> String {
        let base = self
            .base_url
            .clone()
            .or_else(|| std::env::var(BASE_URL_ENV).ok().filter(|b| !b.is_empty()))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        format!("{}/v1/messages", base.trim_end_matches('/'))
    }
}

/// [`mcp::tool_definitions`] as Anthropic tools, in [`mcp::TOOL_NAMES`] order:
/// `mcp__lemma__<name>`, `description`, `input_schema` and `"eager_input_streaming": true`.
pub fn tool_schemas() -> Value {
    let defs = mcp::tool_definitions();
    let tools = defs
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                    json!({
                        "name": format!("{TOOL_PREFIX}{name}"),
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "input_schema": t.get("inputSchema").cloned().unwrap_or(Value::Null),
                        "eager_input_streaming": true,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Value::Array(tools)
}

/// The request body; field order is fixed, so equal inputs give equal bytes.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    cache_control: Value,
    system: &'a str,
    tools: &'a Value,
    tool_choice: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallbacks: Option<&'static str>,
    messages: &'a [Value],
}

/// The JSON body of one request for `model_id` (thinking, effort and fallbacks per [`MODELS`]).
pub fn request_body(
    model_id: &str,
    effort: &str,
    system: &str,
    tools: &Value,
    messages: &[Value],
) -> Vec<u8> {
    let info = model(model_id);
    let body = Body {
        model: model_id,
        max_tokens: MAX_TOKENS,
        stream: true,
        cache_control: json!({"type": "ephemeral"}),
        system,
        tools,
        tool_choice: json!({"type": "auto"}),
        thinking: info
            .is_some_and(|m| m.adaptive_thinking)
            .then(|| json!({"type": "adaptive", "display": "summarized"})),
        output_config: info
            .is_some_and(|m| m.effort)
            .then(|| json!({"effort": effort})),
        fallbacks: info.is_some_and(|m| m.fallbacks).then_some("default"),
        messages,
    };
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Starts the `lemma-api` thread. `Ready` comes first (`modes` empty, `options` = `model` and,
/// for models that take it, `effort`), `Exited(Some(0))` last after `Shutdown` (or when every
/// command sender is dropped); one `TurnEnded` per accepted `Prompt`. Nothing is sent before the
/// first `Prompt`.
pub fn spawn(cfg: MessagesConfig) -> io::Result<AgentHandle> {
    let raw_log = cfg.raw_log.as_deref().map(RawLog::create).transpose()?;
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AgentCmd>();
    let (events_tx, events_rx) = async_channel::unbounded::<AgentEvent>();
    let (tools_tx, tools_rx) = async_channel::unbounded::<ToolCall>();
    let (inbox_tx, inbox) = mpsc::channel::<Input>();

    let forward_rx = cmd_rx.clone();
    let forward_tx = inbox_tx.clone();
    thread::Builder::new()
        .name("lemma-api-cmd".into())
        .spawn(move || {
            while let Ok(cmd) = forward_rx.recv_blocking() {
                if forward_tx.send(Input::Cmd(cmd)).is_err() {
                    return;
                }
            }
            let _ = forward_tx.send(Input::CmdsClosed);
        })?;

    let mut notes = Vec::new();
    let mut cfg = cfg;
    if model(&cfg.model).is_none() {
        if !cfg.model.is_empty() {
            notes.push(format!(
                "Unknown model \"{}\"; using {DEFAULT_MODEL}.",
                cfg.model
            ));
        }
        cfg.model = DEFAULT_MODEL.to_owned();
    }
    if !EFFORTS.iter().any(|(e, _)| *e == cfg.effort) {
        cfg.effort = DEFAULT_EFFORT.to_owned();
    }
    let http: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(format!("lemma/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .timeout_recv_body(Some(Duration::from_secs(3600)))
        .build()
        .into();
    let agent = Agent {
        url: cfg.url(),
        model: cfg.model.clone(),
        effort: cfg.effort.clone(),
        cfg,
        http,
        inbox,
        inbox_tx,
        events: events_tx,
        tools: tools_tx,
        tool_defs: tool_schemas(),
        history: Vec::new(),
        next_request: 0,
        stop: false,
        log: raw_log,
        served_by: None,
    };
    thread::Builder::new()
        .name("lemma-api".into())
        .spawn(move || agent.run(&cmd_rx, notes))?;
    Ok(AgentHandle {
        cmd: cmd_tx,
        events: events_rx,
        tools: tools_rx,
    })
}

// ============================================================================== agent thread

enum Input {
    Cmd(AgentCmd),
    /// Every command sender is gone: shut down.
    CmdsClosed,
    Http {
        request: u64,
        event: HttpEvent,
    },
}

enum HttpEvent {
    /// A non-2xx answer.
    Status {
        code: u16,
        retry_after: Option<Duration>,
        message: String,
    },
    Sse {
        event: String,
        data: String,
    },
    /// The request could not be sent, or the connection broke.
    Failed(String),
    /// The body ended.
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interrupt {
    Cancel,
    Shutdown,
}

/// How a request (with its retries) went wrong.
enum Ended {
    Interrupted(Interrupt),
    Failed(String),
}

/// How one attempt went wrong.
enum Attempt {
    Interrupted(Interrupt),
    Fatal(String),
    /// 401: fatal, unless the key was changed since.
    Unauthorized(String),
    Retry {
        message: String,
        after: Option<Duration>,
    },
}

/// One content block of a response: the block as it would be echoed, plus the raw
/// `input_json_delta` text of a `tool_use`.
struct Block {
    value: Value,
    json: String,
    /// A `tool_use` input that is not valid JSON: the raw text.
    invalid: Option<String>,
}

#[derive(Default)]
struct Response {
    blocks: BTreeMap<u64, Block>,
    model: String,
    tokens: Tokens,
    stop_reason: Option<String>,
    stop_details: Value,
    /// Something reached the user (a chunk or a tool card): a retry would repeat it.
    shown: bool,
    done: bool,
}

/// A `tool_use` to answer.
struct ToolUse {
    id: String,
    name: String,
    /// The parsed input, or the raw text that did not parse.
    input: Result<Value, String>,
}

struct Agent {
    cfg: MessagesConfig,
    url: String,
    http: ureq::Agent,
    inbox: mpsc::Receiver<Input>,
    inbox_tx: mpsc::Sender<Input>,
    events: async_channel::Sender<AgentEvent>,
    tools: async_channel::Sender<ToolCall>,
    tool_defs: Value,
    history: Vec<Value>,
    model: String,
    effort: String,
    next_request: u64,
    stop: bool,
    log: Option<RawLog>,
    /// The model last reported as serving a turn in place of the requested one.
    served_by: Option<String>,
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

impl Agent {
    fn run(mut self, cmd_rx: &async_channel::Receiver<AgentCmd>, notes: Vec<String>) {
        let session = format!(
            "api-{}-{}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        );
        self.emit(AgentEvent::Ready {
            session,
            agent: AGENT_NAME.to_owned(),
            modes: Vec::new(),
            mode: String::new(),
            options: self.options(),
        });
        for note in notes {
            self.emit(AgentEvent::Notice(note));
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            while !self.stop {
                let Ok(input) = self.inbox.recv() else { break };
                match input {
                    Input::Cmd(AgentCmd::Prompt(blocks)) => self.run_turn(blocks),
                    Input::Cmd(AgentCmd::Shutdown) | Input::CmdsClosed => self.stop = true,
                    Input::Cmd(cmd) => {
                        // Cancel without a turn, permissions (never asked), config, modes.
                        let _ = self.turn_cmd(cmd);
                    }
                    Input::Http { .. } => {}
                }
            }
        }));
        if outcome.is_err() {
            self.emit(AgentEvent::Error(
                "internal error in the API backend".to_owned(),
            ));
        }
        cmd_rx.close();
        self.tools.close();
        self.emit(AgentEvent::Exited(Some(0)));
        self.events.close();
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.try_send(event);
    }

    fn options(&self) -> Vec<ConfigOpt> {
        let mut options = vec![ConfigOpt {
            id: "model".to_owned(),
            name: "Model".to_owned(),
            current: self.model.clone(),
            values: MODELS
                .iter()
                .map(|m| (m.id.to_owned(), m.name.to_owned()))
                .collect(),
        }];
        if model(&self.model).is_some_and(|m| m.effort) {
            options.push(ConfigOpt {
                id: "effort".to_owned(),
                name: "Effort".to_owned(),
                current: self.effort.clone(),
                values: EFFORTS
                    .iter()
                    .map(|(v, n)| ((*v).to_owned(), (*n).to_owned()))
                    .collect(),
            });
        }
        options
    }

    /// A command that arrives while a turn runs (or while idle, except `Prompt`).
    fn turn_cmd(&mut self, cmd: AgentCmd) -> Option<Interrupt> {
        match cmd {
            AgentCmd::Prompt(_) => {
                self.emit(AgentEvent::Error(
                    "A prompt is already running; wait for it or cancel it.".to_owned(),
                ));
                None
            }
            AgentCmd::Cancel => Some(Interrupt::Cancel),
            AgentCmd::Shutdown => Some(Interrupt::Shutdown),
            AgentCmd::Permission { .. } => None,
            AgentCmd::SetMode(mode) => {
                self.emit(AgentEvent::Error(format!(
                    "The API backend has no modes (asked for \"{mode}\")."
                )));
                None
            }
            AgentCmd::SetConfig { id, value } => {
                self.set_config(&id, &value);
                None
            }
        }
    }

    fn set_config(&mut self, id: &str, value: &str) {
        match id {
            "model" if model(value).is_some() => self.model = value.to_owned(),
            "effort"
                if model(&self.model).is_some_and(|m| m.effort)
                    && EFFORTS.iter().any(|(e, _)| *e == value) =>
            {
                self.effort = value.to_owned();
            }
            "model" | "effort" => {
                self.emit(AgentEvent::Error(format!(
                    "The {id} \"{value}\" is not offered for {}.",
                    self.model
                )));
                return;
            }
            _ => {
                self.emit(AgentEvent::Error(format!("Unknown option \"{id}\".")));
                return;
            }
        }
        self.emit(AgentEvent::ConfigChanged(self.options()));
    }

    /// The next input, or `None` at `deadline`.
    fn next_input(&self, deadline: Option<Instant>) -> Option<Input> {
        match deadline {
            None => self.inbox.recv().ok(),
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                self.inbox.recv_timeout(wait).ok()
            }
        }
    }

    /// Handles inputs until `deadline`; `Some` when the turn was interrupted.
    fn sleep(&mut self, wait: Duration) -> Option<Interrupt> {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            match self.next_input(Some(deadline)) {
                Some(Input::Cmd(cmd)) => {
                    if let Some(i) = self.turn_cmd(cmd) {
                        return Some(i);
                    }
                }
                Some(Input::CmdsClosed) => return Some(Interrupt::Shutdown),
                Some(Input::Http { .. }) | None => {}
            }
        }
        None
    }

    // ----- turns -----

    fn run_turn(&mut self, blocks: Vec<PromptBlock>) {
        let start = self.history.len();
        self.history
            .push(json!({"role": "user", "content": user_content(blocks)}));
        let reason = self.turn(start);
        self.emit(AgentEvent::TurnEnded(reason));
    }

    fn turn(&mut self, start: usize) -> StopReason {
        for _ in 0..MAX_REQUESTS_PER_TURN {
            let response = match self.request() {
                Ok(response) => response,
                Err(Ended::Interrupted(interrupt)) => {
                    self.stop |= interrupt == Interrupt::Shutdown;
                    return StopReason::Cancelled;
                }
                Err(Ended::Failed(message)) => {
                    self.emit(AgentEvent::Error(self.cfg.api_key.redact(&message)));
                    // The failed turn leaves the history (its prompt and any tool rounds): a
                    // rejected request (400: too long, refused content) would otherwise be sent
                    // again with every later prompt and fail the same way. Its edits stay in the
                    // editor for review.
                    self.history.truncate(start);
                    return StopReason::EndTurn;
                }
            };
            self.report_usage(&response);
            let announced: Vec<String> = response
                .blocks
                .values()
                .filter(|b| b.value["type"] == "tool_use")
                .filter_map(|b| b.value["id"].as_str().map(str::to_owned))
                .collect();
            let stop_reason = response.stop_reason.clone().unwrap_or_default();
            if stop_reason == "refusal" {
                self.emit(AgentEvent::Notice(refusal_notice(&response.stop_details)));
                self.fail_tool_cards(&announced);
                if self.history.len() == start + 1 {
                    // The refused prompt itself goes, so the next one starts clean.
                    self.history.truncate(start);
                }
                return StopReason::Refusal;
            }
            let (echo, uses) = assistant_echo(&response.blocks);
            if !echo.is_empty() {
                self.history
                    .push(json!({"role": "assistant", "content": echo}));
            }
            let not_run: Vec<String> = announced
                .iter()
                .filter(|id| !uses.iter().any(|u| u.id == **id))
                .cloned()
                .collect();
            self.fail_tool_cards(&not_run);
            if stop_reason == "tool_use" && !uses.is_empty() {
                let (results, interrupt) = self.run_tools(&uses);
                self.history
                    .push(json!({"role": "user", "content": results}));
                if let Some(interrupt) = interrupt {
                    self.stop |= interrupt == Interrupt::Shutdown;
                    return StopReason::Cancelled;
                }
                continue;
            }
            if !uses.is_empty() {
                // Echoed tool uses always get results, or the next request would be refused.
                let why = format!("not run: the response ended with stop_reason \"{stop_reason}\"");
                let results: Vec<Value> = uses.iter().map(|u| error_result(&u.id, &why)).collect();
                self.fail_tool_cards(&uses.iter().map(|u| u.id.clone()).collect::<Vec<_>>());
                self.history
                    .push(json!({"role": "user", "content": results}));
            }
            return match stop_reason.as_str() {
                "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            };
        }
        self.emit(AgentEvent::Notice(format!(
            "Stopped after {MAX_REQUESTS_PER_TURN} requests in one turn."
        )));
        StopReason::MaxTurnRequests
    }

    fn fail_tool_cards(&self, ids: &[String]) {
        for id in ids {
            self.emit(AgentEvent::ToolUpdate {
                id: id.clone(),
                title: None,
                status: Some(ToolStatus::Failed),
            });
        }
    }

    fn report_usage(&mut self, response: &Response) {
        let info = model(&response.model)
            .or_else(|| model(&self.model))
            .unwrap_or(&MODELS[0]);
        let t = response.tokens;
        self.emit(AgentEvent::Usage {
            used: t.input + t.cache_read + t.cache_write,
            size: info.context,
        });
        self.emit(AgentEvent::Cost {
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            cache_write: t.cache_write,
            usd_estimate: t.cost(&info.prices),
        });
    }

    // ----- tools -----

    /// Runs `uses` in order; after an interrupt the rest are answered "not run".
    fn run_tools(&mut self, uses: &[ToolUse]) -> (Vec<Value>, Option<Interrupt>) {
        let mut results = Vec::with_capacity(uses.len());
        let mut interrupt = None;
        for u in uses {
            let fail = |agent: &Self, results: &mut Vec<Value>, content: String| {
                results.push(error_result(&u.id, &content));
                agent.emit(AgentEvent::ToolUpdate {
                    id: u.id.clone(),
                    title: None,
                    status: Some(ToolStatus::Failed),
                });
            };
            if interrupt.is_some() {
                fail(
                    self,
                    &mut results,
                    "not run: the user cancelled the turn".to_owned(),
                );
                continue;
            }
            let req = match &u.input {
                Err(raw) => {
                    fail(
                        self,
                        &mut results,
                        json!({ "INVALID_JSON": raw }).to_string(),
                    );
                    continue;
                }
                Ok(input) => match u.name.strip_prefix(TOOL_PREFIX) {
                    Some(name) => mcp::parse_tool_call(name, input),
                    None => Err(format!("unknown tool \"{}\"", u.name)),
                },
            };
            let req = match req {
                Ok(req) => req,
                Err(error) => {
                    let raw = u.input.as_ref().map(Value::to_string).unwrap_or_default();
                    fail(
                        self,
                        &mut results,
                        json!({ "INVALID_JSON": raw, "error": error }).to_string(),
                    );
                    continue;
                }
            };
            self.emit(AgentEvent::ToolUpdate {
                id: u.id.clone(),
                title: None,
                status: Some(ToolStatus::InProgress),
            });
            match self.call_tool(req) {
                Ok(reply) => {
                    let status = if reply.is_error {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Completed
                    };
                    results.push(tool_result(&u.id, &reply));
                    self.emit(AgentEvent::ToolUpdate {
                        id: u.id.clone(),
                        title: None,
                        status: Some(status),
                    });
                }
                Err(i) => {
                    interrupt = Some(i);
                    fail(
                        self,
                        &mut results,
                        "cancelled by the user before the editor answered; the change may \
                         still have been applied"
                            .to_owned(),
                    );
                }
            }
        }
        (results, interrupt)
    }

    /// Sends `req` to the editor and waits for its reply (commands keep being handled).
    fn call_tool(&mut self, req: crate::EditorRequest) -> Result<ToolReply, Interrupt> {
        let deadline = Instant::now() + self.cfg.timeouts.for_request(&req);
        let (reply, rx) = mpsc::sync_channel(1);
        let call = ToolCall {
            call: mcp::next_call_id(),
            req,
            reply,
        };
        if self.tools.try_send(call).is_err() {
            return Ok(text_reply(
                "the editor is not accepting tool calls (the agent is stopping)",
                true,
            ));
        }
        loop {
            match rx.try_recv() {
                Ok(reply) => return Ok(reply),
                Err(TryRecvError::Disconnected) => {
                    return Ok(text_reply(
                        "the editor dropped the request without answering",
                        true,
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(text_reply(mcp::TIMEOUT_TEXT, true));
            }
            match self.next_input(Some((now + mcp::POLL_SLICE / 5).min(deadline))) {
                Some(Input::Cmd(cmd)) => {
                    if let Some(i) = self.turn_cmd(cmd) {
                        return Err(i);
                    }
                }
                Some(Input::CmdsClosed) => return Err(Interrupt::Shutdown),
                Some(Input::Http { .. }) | None => {}
            }
        }
    }

    // ----- requests -----

    fn request(&mut self) -> Result<Response, Ended> {
        let body = request_body(
            &self.model,
            &self.effort,
            &self.cfg.system_prompt,
            &self.tool_defs,
            &self.history,
        );
        let fallbacks = model(&self.model).is_some_and(|m| m.fallbacks);
        let mut retries = 0;
        let mut new_key = false;
        loop {
            match self.attempt(&body, fallbacks) {
                Ok(response) => return Ok(response),
                Err(Attempt::Interrupted(i)) => return Err(Ended::Interrupted(i)),
                Err(Attempt::Unauthorized(message)) => {
                    // A key stored since the session started (Set API key…) goes at once.
                    if !new_key && self.refresh_key() {
                        new_key = true;
                        continue;
                    }
                    return Err(Ended::Failed(message));
                }
                Err(Attempt::Fatal(message)) => return Err(Ended::Failed(message)),
                Err(Attempt::Retry { message, after }) => {
                    if retries >= MAX_RETRIES {
                        return Err(Ended::Failed(format!(
                            "{message} (gave up after {MAX_RETRIES} retries)"
                        )));
                    }
                    retries += 1;
                    let wait = after
                        .unwrap_or_else(|| self.cfg.retry_base.saturating_mul(1 << (retries - 1)))
                        .min(MAX_RETRY_WAIT);
                    self.emit(AgentEvent::Notice(format!(
                        "{}; retrying in {:.1} s ({retries}/{MAX_RETRIES}).",
                        self.cfg.api_key.redact(&message),
                        wait.as_secs_f64()
                    )));
                    if let Some(i) = self.sleep(wait) {
                        return Err(Ended::Interrupted(i));
                    }
                }
            }
        }
    }

    /// Looks the key up again ([`MessagesConfig::key_lookup`]); true when it changed.
    fn refresh_key(&mut self) -> bool {
        let Some(lookup) = self.cfg.key_lookup else {
            return false;
        };
        match lookup() {
            Some(key) if key != self.cfg.api_key => {
                tracing::info!("api: using the API key stored since the session started");
                self.cfg.api_key = key;
                true
            }
            _ => false,
        }
    }

    fn attempt(&mut self, body: &[u8], fallbacks: bool) -> Result<Response, Attempt> {
        self.next_request += 1;
        let id = self.next_request;
        let cancel = Arc::new(AtomicBool::new(false));
        tracing::debug!(
            "api: request {id} ({} bytes, {} messages) to {}",
            body.len(),
            self.history.len(),
            self.url
        );
        if let Some(log) = &self.log {
            log.line(">", &String::from_utf8_lossy(body));
        }
        let job = HttpJob {
            http: self.http.clone(),
            url: self.url.clone(),
            key: self.cfg.api_key.clone(),
            fallbacks,
            body: body.to_vec(),
            request: id,
            cancel: Arc::clone(&cancel),
            inbox: self.inbox_tx.clone(),
            log: self.log.clone(),
        };
        if let Err(e) = thread::Builder::new()
            .name("lemma-api-http".into())
            .spawn(move || job.run())
        {
            return Err(Attempt::Fatal(format!("cannot start the HTTP thread: {e}")));
        }
        let mut response = Response::default();
        // A connection that went silent (no event, not even a `ping`) without closing, e.g.
        // after a network change, is broken: the HTTP read alone would wait for an hour.
        let idle = self.cfg.idle_timeout;
        let mut deadline = Instant::now() + idle;
        let outcome = loop {
            let Some(input) = self.next_input(Some(deadline)) else {
                break Err(broken(
                    &response,
                    &format!("no data from the API for {:.0} s", idle.as_secs_f64()),
                ));
            };
            if matches!(input, Input::Http { request, .. } if request == id) {
                deadline = Instant::now() + idle;
            }
            match input {
                Input::Cmd(cmd) => {
                    if let Some(i) = self.turn_cmd(cmd) {
                        break Err(Attempt::Interrupted(i));
                    }
                }
                Input::CmdsClosed => break Err(Attempt::Interrupted(Interrupt::Shutdown)),
                Input::Http { request, .. } if request != id => {}
                Input::Http { event, .. } => match event {
                    HttpEvent::Status {
                        code,
                        retry_after,
                        message,
                    } => break Err(classify_status(code, retry_after, &message)),
                    HttpEvent::Failed(e) => break Err(broken(&response, &e)),
                    HttpEvent::Eof => {
                        break Err(broken(&response, "the response ended early"));
                    }
                    HttpEvent::Sse { event, data } => {
                        if let Err(e) = self.on_sse(&mut response, &event, &data) {
                            break Err(e);
                        }
                        if response.done {
                            break Ok(());
                        }
                    }
                },
            }
        };
        match outcome {
            Ok(()) => Ok(response),
            Err(e) => {
                // Drops the connection (at the HTTP thread's next read).
                cancel.store(true, Ordering::SeqCst);
                // Cards of a cut-off response never run (a retry starts only before any).
                self.fail_tool_cards(
                    &response
                        .blocks
                        .values()
                        .filter(|b| b.value["type"] == "tool_use")
                        .filter_map(|b| b.value["id"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                );
                Err(e)
            }
        }
    }

    fn on_sse(&mut self, r: &mut Response, event: &str, data: &str) -> Result<(), Attempt> {
        if event == "ping" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| {
            Attempt::Fatal(format!("the API sent an unreadable \"{event}\" event: {e}"))
        })?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event)
            .to_owned();
        match kind.as_str() {
            "message_start" => {
                let message = &v["message"];
                r.model = message["model"].as_str().unwrap_or_default().to_owned();
                r.tokens.merge(&message["usage"]);
                if !r.model.is_empty() && r.model != self.model {
                    if self.served_by.as_deref() != Some(r.model.as_str()) {
                        self.emit(AgentEvent::Notice(format!(
                            "Served by {} (asked for {}).",
                            r.model, self.model
                        )));
                        self.served_by = Some(r.model.clone());
                    }
                } else {
                    self.served_by = None;
                }
            }
            "content_block_start" => {
                let index = v["index"].as_u64().unwrap_or(r.blocks.len() as u64);
                let block = v["content_block"].clone();
                match block["type"].as_str().unwrap_or_default() {
                    "tool_use" => {
                        r.shown = true;
                        self.emit(AgentEvent::ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            title: block["name"].as_str().unwrap_or_default().to_owned(),
                            kind: "other".to_owned(),
                            status: ToolStatus::Pending,
                        });
                    }
                    "text" => {
                        if let Some(t) = block["text"].as_str().filter(|t| !t.is_empty()) {
                            r.shown = true;
                            self.emit(AgentEvent::MessageChunk(t.to_owned()));
                        }
                    }
                    "thinking" => {
                        if let Some(t) = block["thinking"].as_str().filter(|t| !t.is_empty()) {
                            r.shown = true;
                            self.emit(AgentEvent::ThoughtChunk(t.to_owned()));
                        }
                    }
                    "fallback" => {
                        let from = block["from"]["model"].as_str().unwrap_or("?");
                        let to = block["to"]["model"].as_str().unwrap_or("?");
                        self.emit(AgentEvent::Notice(format!(
                            "{from} declined part of this response; {to} continues it."
                        )));
                    }
                    _ => {}
                }
                r.blocks.insert(
                    index,
                    Block {
                        value: block,
                        json: String::new(),
                        invalid: None,
                    },
                );
            }
            "content_block_delta" => {
                let index = v["index"].as_u64().unwrap_or_default();
                let delta = &v["delta"];
                let Some(block) = r.blocks.get_mut(&index) else {
                    return Ok(());
                };
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        append(&mut block.value, "text", text);
                        if !text.is_empty() {
                            r.shown = true;
                            self.emit(AgentEvent::MessageChunk(text.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        append(&mut block.value, "thinking", text);
                        if !text.is_empty() {
                            r.shown = true;
                            self.emit(AgentEvent::ThoughtChunk(text.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        append(
                            &mut block.value,
                            "signature",
                            delta["signature"].as_str().unwrap_or_default(),
                        );
                    }
                    "input_json_delta" => {
                        block
                            .json
                            .push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    "citations_delta" => {
                        if let Some(list) = block.value.get_mut("citations")
                            && let Some(list) = list.as_array_mut()
                        {
                            list.push(delta["citation"].clone());
                        } else {
                            block.value["citations"] = json!([delta["citation"].clone()]);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v["index"].as_u64().unwrap_or_default();
                if let Some(block) = r.blocks.get_mut(&index)
                    && block.value["type"] == "tool_use"
                {
                    finish_tool_input(block);
                }
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    r.stop_reason = Some(reason.to_owned());
                }
                if !v["delta"]["stop_details"].is_null() {
                    r.stop_details = v["delta"]["stop_details"].clone();
                }
                r.tokens.merge(&v["usage"]);
            }
            "message_stop" => {
                // A tool_use that never got its content_block_stop.
                for block in r.blocks.values_mut() {
                    if block.value["type"] == "tool_use" && !block.json.is_empty() {
                        finish_tool_input(block);
                    }
                }
                r.done = true;
            }
            "error" => {
                let kind = v["error"]["type"].as_str().unwrap_or("error");
                let message = v["error"]["message"].as_str().unwrap_or_default();
                let text = format!("API error ({kind}): {message}");
                let retryable =
                    matches!(kind, "overloaded_error" | "api_error" | "rate_limit_error");
                return Err(if retryable && !r.shown {
                    Attempt::Retry {
                        message: text,
                        after: None,
                    }
                } else {
                    Attempt::Fatal(text)
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// A connection that broke: retried only while nothing reached the user.
fn broken(response: &Response, error: &str) -> Attempt {
    if response.shown {
        Attempt::Fatal(format!(
            "The connection to the API broke mid-response: {error}"
        ))
    } else {
        Attempt::Retry {
            message: format!("Connection error: {error}"),
            after: None,
        }
    }
}

fn classify_status(code: u16, after: Option<Duration>, message: &str) -> Attempt {
    let detail = if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    };
    match code {
        401 => Attempt::Unauthorized(format!("API key rejected (401){detail}")),
        403 => Attempt::Fatal(format!("Permission denied (403){detail}")),
        429 => Attempt::Retry {
            message: format!("Rate limited (429){detail}"),
            after,
        },
        529 => Attempt::Retry {
            message: format!("The API is overloaded (529){detail}"),
            after,
        },
        500..=599 => Attempt::Retry {
            message: format!("API error ({code}){detail}"),
            after,
        },
        _ => Attempt::Fatal(format!("The API refused the request ({code}){detail}")),
    }
}

fn refusal_notice(details: &Value) -> String {
    let category = details["category"].as_str();
    let explanation = details["explanation"].as_str().filter(|e| !e.is_empty());
    let mut text = "The model declined this request".to_owned();
    if let Some(category) = category {
        text.push_str(&format!(" (category: {category})"));
    }
    text.push('.');
    if let Some(explanation) = explanation {
        text.push(' ');
        text.push_str(explanation);
    }
    text
}

fn append(block: &mut Value, key: &str, text: &str) {
    let mut s = block[key].as_str().unwrap_or_default().to_owned();
    s.push_str(text);
    block[key] = Value::String(s);
}

/// Parses a `tool_use`'s accumulated input strictly; no input at all is `{}`.
fn finish_tool_input(block: &mut Block) {
    if block.json.is_empty() {
        if block.value.get("input").is_none_or(Value::is_null) {
            block.value["input"] = json!({});
        }
        return;
    }
    match serde_json::from_str::<Value>(&block.json) {
        Ok(input) => {
            block.value["input"] = input;
            block.invalid = None;
        }
        Err(_) => {
            block.value["input"] = json!({});
            block.invalid = Some(block.json.clone());
        }
    }
    block.json.clear();
}

/// The response's content as it is echoed in the history, and its `tool_use`s to answer.
fn assistant_echo(blocks: &BTreeMap<u64, Block>) -> (Vec<Value>, Vec<ToolUse>) {
    let boundary = blocks
        .values()
        .rposition(|b| b.value["type"] == "fallback")
        .map_or(0, |i| i + 1);
    let mut echo = Vec::new();
    let mut uses = Vec::new();
    for (i, block) in blocks.values().enumerate() {
        let v = &block.value;
        let kind = v["type"].as_str().unwrap_or_default();
        let declined = i < boundary;
        match kind {
            "text" => {
                if v["text"].as_str().is_some_and(|t| !t.is_empty()) {
                    echo.push(v.clone());
                }
            }
            "thinking" if !declined => {
                if v["signature"].as_str().is_some_and(|s| !s.is_empty()) {
                    echo.push(v.clone());
                }
            }
            "redacted_thinking" if !declined => echo.push(v.clone()),
            "tool_use" if !declined => {
                let mut v = v.clone();
                if !v["input"].is_object() {
                    v["input"] = json!({});
                }
                uses.push(ToolUse {
                    id: v["id"].as_str().unwrap_or_default().to_owned(),
                    name: v["name"].as_str().unwrap_or_default().to_owned(),
                    input: match &block.invalid {
                        Some(raw) => Err(raw.clone()),
                        None => Ok(v["input"].clone()),
                    },
                });
                echo.push(v);
            }
            _ => {}
        }
    }
    (echo, uses)
}

/// The user message's content for a prompt.
fn user_content(blocks: Vec<PromptBlock>) -> Vec<Value> {
    let mut content: Vec<Value> = blocks
        .into_iter()
        .filter_map(|b| match b {
            PromptBlock::Text(text) => {
                (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
            }
            PromptBlock::Resource { uri, mime, text } => {
                let mime = if mime.is_empty() {
                    String::new()
                } else {
                    format!(" mime=\"{mime}\"")
                };
                Some(json!({
                    "type": "text",
                    "text": format!("<resource uri=\"{uri}\"{mime}>\n{text}\n</resource>"),
                }))
            }
            PromptBlock::Image { mime, data, .. } => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime, "data": mcp::base64(&data)},
            })),
            PromptBlock::Document { uri, mime, data }
                if mime == "application/pdf" && !data.is_empty() =>
            {
                Some(json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": mime, "data": mcp::base64(&data)},
                    "title": PromptBlock::file_name(&uri),
                }))
            }
            PromptBlock::Document { uri, mime, data } => {
                let why = if data.is_empty() {
                    "too large to include here"
                } else {
                    "a binary file that cannot be shown here"
                };
                Some(json!({
                    "type": "text",
                    "text": format!("<attachment uri=\"{uri}\" mime=\"{mime}\">({why})</attachment>"),
                }))
            }
        })
        .collect();
    if content.is_empty() {
        content.push(json!({"type": "text", "text": "(empty prompt)"}));
    }
    content
}

fn error_result(id: &str, text: &str) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "is_error": true,
        "content": [{"type": "text", "text": text}],
    })
}

fn text_reply(text: &str, is_error: bool) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(text.to_owned())],
        is_error,
    }
}

/// A `tool_result` for the editor's reply: text blocks, and PNGs as base64 `image` blocks.
fn tool_result(id: &str, reply: &ToolReply) -> Value {
    let mut content: Vec<Value> = reply
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text(text) if text.is_empty() => None,
            ToolContent::Text(text) => Some(json!({"type": "text", "text": text})),
            ToolContent::Png(png) => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": mcp::base64(png)},
            })),
        })
        .collect();
    if content.is_empty() {
        content.push(json!({"type": "text", "text": "(no output)"}));
    }
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "is_error": reply.is_error,
        "content": content,
    })
}

// ============================================================================== HTTP thread

/// Lines longer than this end the stream (no real event comes close).
const MAX_LINE: u64 = 16 << 20;
/// How much of an error answer is read.
const ERROR_BODY_LIMIT: u64 = 64 << 10;

struct HttpJob {
    http: ureq::Agent,
    url: String,
    key: ApiKey,
    fallbacks: bool,
    body: Vec<u8>,
    request: u64,
    cancel: Arc<AtomicBool>,
    inbox: mpsc::Sender<Input>,
    log: Option<RawLog>,
}

impl HttpJob {
    fn send(&self, event: HttpEvent) -> bool {
        self.inbox
            .send(Input::Http {
                request: self.request,
                event,
            })
            .is_ok()
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn run(self) {
        let Ok(mut key) = ureq::http::HeaderValue::from_str(self.key.expose()) else {
            self.send(HttpEvent::Failed(
                "the API key cannot be sent in an HTTP header".to_owned(),
            ));
            return;
        };
        key.set_sensitive(true);
        let mut request = self
            .http
            .post(&self.url)
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        if self.fallbacks {
            request = request.header("anthropic-beta", FALLBACK_BETA);
        }
        let response = match request.send(&self.body[..]) {
            Ok(response) => response,
            Err(e) => {
                let text = self.key.redact(&e.to_string());
                if let Some(log) = &self.log {
                    log.line("!", &text);
                }
                self.send(HttpEvent::Failed(text));
                return;
            }
        };
        if self.cancelled() {
            return;
        }
        let code = response.status().as_u16();
        if let Some(log) = &self.log {
            log.line("<", &format!("HTTP {code}"));
        }
        if !(200..300).contains(&code) {
            let retry_after = retry_after(response.headers());
            let mut text = String::new();
            let _ = response
                .into_body()
                .into_reader()
                .take(ERROR_BODY_LIMIT)
                .read_to_string(&mut text);
            if let Some(log) = &self.log {
                log.line("<", &self.key.redact(&text));
            }
            self.send(HttpEvent::Status {
                code,
                retry_after,
                message: self.key.redact(&error_message(&text)),
            });
            return;
        }
        let mut reader = BufReader::new(response.into_body().into_reader());
        let mut line = Vec::new();
        let (mut event, mut data) = (String::new(), String::new());
        loop {
            if self.cancelled() {
                return;
            }
            line.clear();
            match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line) {
                Ok(0) => {
                    self.send(HttpEvent::Eof);
                    return;
                }
                Ok(_) if line.len() as u64 >= MAX_LINE && line.last() != Some(&b'\n') => {
                    self.send(HttpEvent::Failed("an event line is too long".to_owned()));
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    self.send(HttpEvent::Failed(self.key.redact(&e.to_string())));
                    return;
                }
            }
            if self.cancelled() {
                return;
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                if !data.is_empty() || !event.is_empty() {
                    if let Some(log) = &self.log {
                        log.line("<", &format!("{event} {data}"));
                    }
                    let sse = HttpEvent::Sse {
                        event: std::mem::take(&mut event),
                        data: std::mem::take(&mut data),
                    };
                    if !self.send(sse) {
                        return;
                    }
                }
                continue;
            }
            if text.starts_with(':') {
                continue;
            }
            let (field, value) = text.split_once(':').unwrap_or((text, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => value.clone_into(&mut event),
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
                _ => {}
            }
        }
    }
}

/// `retry-after-ms` (milliseconds) or `retry-after` (seconds); HTTP dates are not used.
fn retry_after(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    number("retry-after-ms")
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .or_else(|| number("retry-after").map(Duration::from_secs_f64))
        .map(|d| d.min(MAX_RETRY_WAIT))
}

/// The `error.message` of an API error answer, else its first 300 characters.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().chars().take(300).collect())
}

// ============================================================================== raw log

/// Debug log of request bodies and SSE lines (0600). Never holds headers or the key.
#[derive(Clone)]
struct RawLog(Arc<Mutex<File>>);

impl RawLog {
    fn create(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    fn line(&self, direction: &str, line: &str) {
        let mut file = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{direction} {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_become_image_and_document_blocks() {
        let content = user_content(vec![
            PromptBlock::Text("What is in these?".into()),
            PromptBlock::Image {
                uri: String::new(),
                mime: "image/jpeg".into(),
                data: b"JPG".to_vec(),
            },
            PromptBlock::Document {
                uri: "file:///p/my%20paper.pdf".into(),
                mime: "application/pdf".into(),
                data: b"%PDF".to_vec(),
            },
            PromptBlock::Document {
                uri: "file:///p/a.bin".into(),
                mime: "application/octet-stream".into(),
                data: vec![0],
            },
            PromptBlock::Document {
                uri: "file:///p/huge.pdf".into(),
                mime: "application/pdf".into(),
                data: Vec::new(),
            },
        ]);
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "What is in these?"})
        );
        assert_eq!(
            content[1],
            json!({"type": "image", "source":
                {"type": "base64", "media_type": "image/jpeg", "data": "SlBH"}})
        );
        assert_eq!(
            content[2],
            json!({"type": "document", "title": "my paper.pdf", "source":
                {"type": "base64", "media_type": "application/pdf", "data": "JVBERg=="}})
        );
        assert_eq!(content[3]["type"], "text");
        assert!(
            content[3]["text"]
                .as_str()
                .unwrap()
                .contains("file:///p/a.bin")
        );
        assert_eq!(content[4]["type"], "text");
        assert!(
            content[4]["text"]
                .as_str()
                .unwrap()
                .contains("too large to include")
        );
    }

    #[test]
    fn effort_and_thinking_follow_the_model() {
        let tools = tool_schemas();
        let body = |m: &str| -> Value {
            serde_json::from_slice(&request_body(m, "high", "sys", &tools, &[])).unwrap()
        };
        let opus = body("claude-opus-5-5");
        assert_eq!(
            opus["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(opus["output_config"], json!({"effort": "high"}));
        assert_eq!(opus["fallbacks"], "default");
        assert_eq!(opus["max_tokens"], 64_000);
        assert_eq!(opus["stream"], true);
        assert_eq!(opus["tool_choice"], json!({"type": "auto"}));
        let haiku = body("claude-haiku-4-5");
        assert!(haiku.get("thinking").is_none());
        assert!(haiku.get("output_config").is_none());
        assert!(haiku.get("fallbacks").is_none());
    }

    #[test]
    fn tools_are_byte_stable_and_eager() {
        let a = serde_json::to_string(&tool_schemas()).unwrap();
        let b = serde_json::to_string(&tool_schemas()).unwrap();
        assert_eq!(a, b);
        let tools = tool_schemas();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let want: Vec<String> = mcp::TOOL_NAMES
            .iter()
            .map(|n| format!("{TOOL_PREFIX}{n}"))
            .collect();
        assert_eq!(names, want);
        for t in tools.as_array().unwrap() {
            assert_eq!(t["eager_input_streaming"], true);
            assert_eq!(t["input_schema"]["type"], "object");
            assert!(t.get("inputSchema").is_none());
        }
    }

    #[test]
    fn cost_uses_list_prices() {
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
        };
        assert_eq!(t.cost(&MODELS[0].prices), Usd(24_200_000));
        assert_eq!(
            t.cost(&model("claude-haiku-4-5").unwrap().prices),
            Usd(6_100_000)
        );
    }

    #[test]
    fn retry_after_prefers_milliseconds() {
        let mut h = ureq::http::HeaderMap::new();
        h.insert("retry-after", "2".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(2)));
        h.insert("retry-after-ms", "250".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_millis(250)));
        h.insert("retry-after-ms", "9999999".parse().unwrap());
        assert_eq!(retry_after(&h), Some(MAX_RETRY_WAIT));
    }

    #[test]
    fn fallback_echo_drops_the_declined_models_internal_blocks() {
        let mut blocks = BTreeMap::new();
        let mut put = |i: u64, value: Value| {
            blocks.insert(
                i,
                Block {
                    value,
                    json: String::new(),
                    invalid: None,
                },
            );
        };
        put(
            0,
            json!({"type": "thinking", "thinking": "a", "signature": "s"}),
        );
        put(1, json!({"type": "text", "text": "partial"}));
        put(
            2,
            json!({"type": "fallback", "from": {"model": "x"}, "to": {"model": "y"}}),
        );
        put(
            3,
            json!({"type": "thinking", "thinking": "b", "signature": "t"}),
        );
        put(4, json!({"type": "text", "text": ""}));
        put(5, json!({"type": "thinking", "thinking": "cut"}));
        let (echo, uses) = assistant_echo(&blocks);
        assert!(uses.is_empty());
        assert_eq!(
            echo,
            vec![
                json!({"type": "text", "text": "partial"}),
                json!({"type": "thinking", "thinking": "b", "signature": "t"}),
            ]
        );
    }
}
