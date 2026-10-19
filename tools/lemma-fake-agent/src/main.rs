//! `lemma-fake-agent`: a scripted ACP **agent** for tests (WP2.3). It plays the role of the Claude
//! ACP adapter: it speaks newline-delimited JSON-RPC on stdio, written by hand with `serde_json`
//! only, so it is an independent conformance peer for Lemma's ACP client rather than the same SDK
//! talking to itself. It never calls a model and never touches the network.
//!
//! ```text
//! lemma-fake-agent SCENARIO.json          run as an ACP agent on stdin/stdout
//! lemma-fake-agent --check FILE.json...   validate scenario files, exit 1 on the first error
//! ```
//!
//! # Protocol (shapes mirror `@agentclientprotocol/claude-agent-acp` 0.89.1)
//!
//! - `initialize` → protocol version 1, `agentInfo {name: "lemma-fake-agent", version}`,
//!   `sessionCapabilities.close`, no auth methods. A client that advertises
//!   `clientCapabilities.session.notices` gets `notice` updates; others get the adapter's
//!   transcript fallback (an `agent_message_chunk` reading `**title:** description`).
//! - `session/new` checks that `_meta.claudeCode.options.disallowedTools` contains `Edit`,
//!   `Write` and `NotebookEdit` (error -32602 otherwise), launches the stdio MCP server named
//!   `lemma` with its `env` (Lemma's `lemma-mcp` bridge) and runs the MCP handshake exactly like
//!   Claude CLI 2.1.288 (byte-identical lines): `server/discover`, `initialize` (2025-11-25),
//!   `notifications/initialized`, `tools/list`. It answers with modes `default`/`plan` and the
//!   config options `mode`, `model` (default/opus/sonnet/haiku) and `effort` (default, low…max),
//!   then sends `available_commands_update` with `compact` and `init`.
//! - `session/set_mode` sends `config_option_update` and answers `{}` (no `current_mode_update`,
//!   like the adapter); `session/set_config_option` answers `{configOptions}` (for `mode` it first
//!   sends `current_mode_update`).
//! - `session/prompt` picks the scenario turn (below) and runs it on its own thread, so
//!   `session/cancel`, permission answers and other requests keep flowing. `session/cancel` ends
//!   the running turn with `stopReason: "cancelled"` at once (between steps, during sleeps, while
//!   streaming, while waiting for a tool or a permission). A turn whose expectations fail is
//!   answered with a JSON-RPC error (-32603) that names the step.
//! - `session/close` cancels the running turn and answers `{}`. The process exits on stdin EOF,
//!   after closing the MCP server's stdin (and killing it if it lingers).
//!
//! # Scenario files
//!
//! A JSON object mapping a prompt **keyword** to a list of steps. A prompt runs the steps of the
//! longest keyword that occurs (case-insensitively) in its text blocks (except a block of Lemma's
//! instructions, which starts with `<lemma_instructions>`); `default` is used when no keyword
//! matches. Keys starting with `_` are not keywords: `_description` is free text and
//! `_options` holds start-up behaviour:
//!
//! | `_options` key | Meaning |
//! |---|---|
//! | `modes` | `"lemma"` (default: `default`, `plan`), `"adapter"` (the adapter's five modes) or `"codex"` (Codex's `read-only`, `workspace-write`, `agent`, `agent-full-access`: an agent without `default`) |
//! | `initial_mode` | the mode a new session starts in (default `default`; `agent` with the `codex` modes) |
//! | `startup_stderr` | text written to stderr at start-up |
//! | `exit_on` | `"initialize"` or `"session/new"`: exit with `exit_code` (default 1) when it arrives |
//! | `session_new_delay_ms` | delay before answering `session/new` |
//! | `mcp` | `"required"` (default: no MCP server is an error) or `"skip"` (never launch one) |
//! | `fs` | `"required"`: `initialize` must advertise `fs.readTextFile` and `fs.writeTextFile` (else error -32602); `"forbidden"`: it must advertise neither (WP5.2) |
//! | `agent_name` | the `agentInfo.name` to report (default `lemma-fake-agent`), e.g. to play Pi's adapter |
//! | `no_images` | `true`: advertise `promptCapabilities.image: false` (an agent that takes no images) |
//! | `session_new_error` | `{"code": n, "message": "…"}`: answer `session/new` with this error (e.g. -32000, Pi's "authentication required") |
//!
//! Steps (each an object with one key; strings may contain `{{i}}` inside `repeat`):
//!
//! | Step | Effect |
//! |---|---|
//! | `{"say": "text"}` / `{"think": "text"}` | `agent_message_chunk` / `agent_thought_chunk`, in 3 pieces 20 ms apart |
//! | `{"stream": {"count": n, "text": "…{{i}}…", "delay_ms": 0, "thought": false}}` | n chunks, `{{i}}` = 1…n |
//! | `{"plan": ["entry", {"content": "…", "status": "completed", "priority": "high"}]}` | `plan` |
//! | `{"tool": {"name": "edit", "args": {…}, "expect_error": …, "expect_text": "…"}}` | `tool_call`, MCP `tools/call`, `tool_call_update` with the result. `expect_error`: absent/`false` = must succeed, `true` = must fail, `"text"` = must fail with that text, `"any"` = not checked |
//! | `{"permission": {"title": "…", "kind": "execute", "options": "bash", "expect": "any", "report": false}}` | `session/request_permission`; `options` is `"bash"` (allow-once, allow-with-updates, reject), `"exit_plan"` (the adapter's ExitPlanMode set) or a list of `{id, name, kind}`; `expect` is an option id, `"cancelled"` or `"any"`; `report` says the answer as a message |
//! | `{"disk_write": {"path": "rel", "content": "…"}}` or `{"path", "old_string", "new_string", "in_place": false}` | writes a project file directly (like Bash `sed -i`: temp file + rename unless `in_place`) |
//! | `{"usage": {"used": n, "size": m}}` | `usage_update` |
//! | `{"notice": "title"}` or `{"notice": {"title", "description", "severity"}}` | `notice` (or the transcript fallback) |
//! | `{"mode": "plan"}` | agent-initiated `current_mode_update` + `config_option_update` |
//! | `{"raw_update": {…}}` | a `session/update` with this `update` object, verbatim |
//! | `{"sleep_ms": n}` | cancellable sleep |
//! | `{"stop": "max_tokens"}` | ends the turn with that stop reason |
//! | `{"stderr": "text"}` | writes to stderr |
//! | `{"exit": code}` | exits at once (a crash: no clean-up) |
//! | `{"echo_prompt": true}` | one `agent_message_chunk` with `{"prompt": [blocks as received]}` |
//! | `{"close_stdout": true}` | closes stdout but keeps running (a hung agent; the client must kill it) |
//! | `{"repeat": {"times": n, "steps": […]}}` | runs the steps n times with `{{i}}` = 1…n |
//! | `{"fs_read": {"path": "rel", "line": n, "limit": n, "expect_text": "…", "expect_content": "…", "expect_error": …}}` | a `read` tool card and `fs/read_text_file` to the client (a relative `path` is joined to the session's cwd); `expect_content` is the exact content |
//! | `{"fs_write": {"path": "rel", "content": "…", "expect_error": …}}` | an `edit` tool card and `fs/write_text_file` |
//! | `{"fs_edit": {"path": "rel", "old_string": "…", "new_string": "…", "replace_all": false, "expect_error": …}}` | like Pi's delegated `edit` tool: `fs/read_text_file`, replace, `fs/write_text_file` |
//!
//! Without a `stop` step a turn ends with `end_turn`.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const NAME: &str = "lemma-fake-agent";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Pause between the three pieces of a `say`/`think` step.
const PIECE_DELAY: Duration = Duration::from_millis(20);
/// How long a cancelled turn waits for the client to answer a pending permission request.
const CANCEL_ANSWER_GRACE: Duration = Duration::from_secs(1);
/// How long each MCP handshake request may take.
const MCP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the MCP server may take to exit after its stdin closed.
const MCP_EXIT_GRACE: Duration = Duration::from_millis(300);

/// The Claude CLI 2.1.288 MCP handshake, byte for byte (recorded by the research probe).
const MCP_DISCOVER: &str = r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"},"io.modelcontextprotocol/clientCapabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}}}}"#;
const MCP_INITIALIZE: &str = r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"}},"jsonrpc":"2.0","id":0}"#;
const MCP_INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
const MCP_TOOLS_LIST: &str = r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#;
/// MCP protocol versions the CLI accepts in the `initialize` answer.
const MCP_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];

/// How Lemma's instructions start, in the first prompt of an agent other than Claude Code.
const INSTRUCTIONS_TAG: &str = "<lemma_instructions>";

const STOP_REASONS: &[&str] = &[
    "end_turn",
    "max_tokens",
    "max_turn_requests",
    "refusal",
    "cancelled",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--check") => check_files(&args[2..]),
        Some("--help" | "-h") => {
            println!("usage: {NAME} SCENARIO.json | {NAME} --check FILE.json...");
            ExitCode::SUCCESS
        }
        Some(path) => match Scenario::load(Path::new(path)) {
            Ok(scenario) => serve(scenario),
            Err(e) => {
                eprintln!("{NAME}: {path}: {e}");
                ExitCode::from(2)
            }
        },
        None => {
            eprintln!("usage: {NAME} SCENARIO.json | {NAME} --check FILE.json...");
            ExitCode::from(2)
        }
    }
}

fn check_files(paths: &[String]) -> ExitCode {
    if paths.is_empty() {
        eprintln!("{NAME} --check: no files given");
        return ExitCode::from(2);
    }
    for path in paths {
        match Scenario::load(Path::new(path)) {
            Ok(s) => println!("ok: {path} ({} turns)", s.turns.len()),
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

// ===================================================================================== scenario

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    modes: Option<String>,
    initial_mode: Option<String>,
    startup_stderr: Option<String>,
    exit_on: Option<String>,
    exit_code: Option<i32>,
    session_new_delay_ms: u64,
    mcp: Option<String>,
    fs: Option<String>,
    agent_name: Option<String>,
    session_new_error: Option<RpcErrorSpec>,
    no_images: bool,
}

/// `_options.session_new_error`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcErrorSpec {
    code: i64,
    message: String,
}

#[derive(Debug)]
struct Scenario {
    options: Options,
    /// Lower-cased keyword → raw steps (validated at load time).
    turns: BTreeMap<String, Vec<Value>>,
}

impl Scenario {
    fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let Value::Object(map) = value else {
            return Err("the scenario must be a JSON object (keyword → steps)".into());
        };
        let mut options = Options::default();
        let mut turns = BTreeMap::new();
        for (key, value) in map {
            if key == "_options" {
                options = serde_json::from_value(value).map_err(|e| format!("_options: {e}"))?;
                continue;
            }
            if key.starts_with('_') {
                continue;
            }
            let Value::Array(steps) = value else {
                return Err(format!("\"{key}\": expected a list of steps"));
            };
            for (i, step) in steps.iter().enumerate() {
                validate_step(step).map_err(|e| format!("\"{key}\" step {}: {e}", i + 1))?;
            }
            turns.insert(key.to_lowercase(), steps);
        }
        if let Some(modes) = &options.modes
            && modes != "lemma"
            && modes != "adapter"
            && modes != "codex"
        {
            return Err(format!(
                "_options.modes: expected \"lemma\", \"adapter\" or \"codex\", got \"{modes}\""
            ));
        }
        if let Some(stage) = &options.exit_on
            && stage != "initialize"
            && stage != "session/new"
        {
            return Err(format!(
                "_options.exit_on: expected \"initialize\" or \"session/new\", got \"{stage}\""
            ));
        }
        if let Some(fs) = &options.fs
            && fs != "required"
            && fs != "forbidden"
        {
            return Err(format!(
                "_options.fs: expected \"required\" or \"forbidden\", got \"{fs}\""
            ));
        }
        if let Some(mcp) = &options.mcp
            && mcp != "required"
            && mcp != "skip"
        {
            return Err(format!(
                "_options.mcp: expected \"required\" or \"skip\", got \"{mcp}\""
            ));
        }
        if let Some(mode) = &options.initial_mode
            && !modes_for(&options).iter().any(|m| m.id == mode)
        {
            return Err(format!("_options.initial_mode: \"{mode}\" is not offered"));
        }
        if turns.is_empty() {
            return Err("no turns: add at least a \"default\" list of steps".into());
        }
        Ok(Self { options, turns })
    }

    /// The steps for a prompt: the longest keyword found in `text` (ties: alphabetical), else
    /// `default`.
    fn select(&self, text: &str) -> Option<(&str, &[Value])> {
        let lower = text.to_lowercase();
        let mut best: Option<(&str, &[Value])> = None;
        for (keyword, steps) in &self.turns {
            if keyword == "default" || !lower.contains(keyword.as_str()) {
                continue;
            }
            if best.is_none_or(|(b, _)| keyword.len() > b.len()) {
                best = Some((keyword.as_str(), steps.as_slice()));
            }
        }
        best.or_else(|| {
            self.turns
                .get("default")
                .map(|steps| ("default", steps.as_slice()))
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    Say(String),
    Think(String),
    Stream(StreamStep),
    Plan(Vec<PlanItem>),
    Tool(ToolStep),
    Permission(PermissionStep),
    DiskWrite(DiskWriteStep),
    Usage(UsageStep),
    SleepMs(u64),
    Stop(StopStep),
    Notice(NoticeStep),
    Mode(String),
    RawUpdate(Value),
    Exit(i32),
    Stderr(String),
    Repeat(RepeatStep),
    EchoPrompt(bool),
    CloseStdout(bool),
    FsRead(FsReadStep),
    FsWrite(FsWriteStep),
    FsEdit(FsEditStep),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsReadStep {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    expect_text: Option<String>,
    #[serde(default)]
    expect_content: Option<String>,
    #[serde(default)]
    expect_error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsWriteStep {
    path: String,
    content: String,
    #[serde(default)]
    expect_error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsEditStep {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    expect_error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamStep {
    count: u32,
    text: String,
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    thought: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PlanItem {
    Text(String),
    Entry(PlanEntry),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanEntry {
    content: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolStep {
    name: String,
    #[serde(default = "empty_object")]
    args: Value,
    #[serde(default)]
    expect_error: Option<Value>,
    #[serde(default)]
    expect_text: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionStep {
    title: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    options: Option<Value>,
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    report: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskWriteStep {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    in_place: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageStep {
    used: u64,
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StopStep {
    Reason(String),
    Object { reason: String },
}

impl StopStep {
    fn reason(&self) -> &str {
        match self {
            Self::Reason(r) | Self::Object { reason: r } => r,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NoticeStep {
    Title(String),
    Full(NoticeFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeFull {
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatStep {
    times: u32,
    steps: Vec<Value>,
}

/// What a tool step expects of the result.
#[derive(Debug, PartialEq)]
enum Expect {
    Success,
    Error(Option<String>),
    Any,
}

impl ToolStep {
    fn expect(&self) -> Result<Expect, String> {
        expect_of(&self.expect_error)
    }
}

/// An `expect_error` value (see the `tool` step).
fn expect_of(expect_error: &Option<Value>) -> Result<Expect, String> {
    match expect_error {
        None | Some(Value::Bool(false)) | Some(Value::Null) => Ok(Expect::Success),
        Some(Value::Bool(true)) => Ok(Expect::Error(None)),
        Some(Value::String(s)) if s == "any" => Ok(Expect::Any),
        Some(Value::String(s)) => Ok(Expect::Error(Some(s.clone()))),
        Some(other) => Err(format!(
            "expect_error: expected true, false, \"any\" or a text, got {other}"
        )),
    }
}

fn parse_step(raw: &Value) -> Result<Step, String> {
    serde_json::from_value(raw.clone()).map_err(|e| e.to_string())
}

/// Checks a raw step (and, for `repeat`, its steps with `{{i}}` = 1).
fn validate_step(raw: &Value) -> Result<(), String> {
    match parse_step(raw)? {
        Step::Plan(items) => {
            for item in &items {
                if let PlanItem::Entry(e) = item {
                    check_one_of("status", e.status.as_deref(), PLAN_STATUSES)?;
                    check_one_of("priority", e.priority.as_deref(), PLAN_PRIORITIES)?;
                }
            }
        }
        Step::Tool(t) => {
            if !t.args.is_object() {
                return Err("tool.args must be an object".into());
            }
            if t.name.is_empty() {
                return Err("tool.name is empty".into());
            }
            t.expect()?;
        }
        Step::Permission(p) => {
            let options = permission_options(p.options.as_ref())?;
            if let Some(expect) = p.expect.as_deref()
                && expect != "any"
                && expect != "cancelled"
                && !options.iter().any(|o| o.id == expect)
            {
                return Err(format!(
                    "permission.expect \"{expect}\" is not one of the offered options"
                ));
            }
            check_one_of("kind", p.kind.as_deref(), TOOL_KINDS)?;
        }
        Step::DiskWrite(d) => {
            relative_path(&d.path)?;
            match (&d.content, &d.old_string, &d.new_string) {
                (Some(_), None, None) | (None, Some(_), Some(_)) => {}
                _ => {
                    return Err(
                        "disk_write needs either content or both old_string and new_string".into(),
                    );
                }
            }
            if d.old_string.as_deref() == Some("") {
                return Err("disk_write.old_string is empty".into());
            }
        }
        Step::Stop(s) => check_one_of("stop", Some(s.reason()), STOP_REASONS)?,
        Step::FsRead(r) => {
            fs_path_valid(&r.path)?;
            expect_of(&r.expect_error)?;
        }
        Step::FsWrite(w) => {
            fs_path_valid(&w.path)?;
            expect_of(&w.expect_error)?;
        }
        Step::FsEdit(e) => {
            fs_path_valid(&e.path)?;
            expect_of(&e.expect_error)?;
            if e.old_string.is_empty() {
                return Err("fs_edit.old_string is empty".into());
            }
        }
        Step::Notice(NoticeStep::Full(n)) => {
            check_one_of("severity", n.severity.as_deref(), NOTICE_SEVERITIES)?;
        }
        Step::Repeat(r) => {
            for (i, step) in r.steps.iter().enumerate() {
                validate_step(&substitute(step, 1))
                    .map_err(|e| format!("repeat step {}: {e}", i + 1))?;
            }
        }
        _ => {}
    }
    Ok(())
}

const PLAN_STATUSES: &[&str] = &["pending", "in_progress", "completed"];
const PLAN_PRIORITIES: &[&str] = &["high", "medium", "low"];
const NOTICE_SEVERITIES: &[&str] = &["info", "warning", "error"];
const TOOL_KINDS: &[&str] = &[
    "read",
    "edit",
    "delete",
    "move",
    "search",
    "execute",
    "think",
    "fetch",
    "switch_mode",
    "other",
];
const OPTION_KINDS: &[&str] = &["allow_once", "allow_always", "reject_once", "reject_always"];

fn check_one_of(what: &str, value: Option<&str>, allowed: &[&str]) -> Result<(), String> {
    match value {
        Some(v) if !allowed.contains(&v) => Err(format!(
            "{what}: \"{v}\" is not one of {}",
            allowed.join(", ")
        )),
        _ => Ok(()),
    }
}

/// Replaces `{{i}}` in every string of `value`.
fn substitute(value: &Value, i: u32) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace("{{i}}", &i.to_string())),
        Value::Array(items) => Value::Array(items.iter().map(|v| substitute(v, i)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, i)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A project-relative path without `..`, or an error.
fn relative_path(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if path.is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "disk_write.path must be a relative path inside the project, got \"{path}\""
        ));
    }
    Ok(p)
}

/// An `fs_*` step's path: absolute, or relative inside the project.
fn fs_path_valid(path: &str) -> Result<(), String> {
    if Path::new(path).is_absolute() {
        Ok(())
    } else {
        relative_path(path).map(|_| ())
    }
}

/// Why a result does not meet `expect` (`None` when it does).
fn mismatch(expect: &Expect, is_error: bool, text: &str) -> Option<String> {
    match expect {
        Expect::Any => None,
        Expect::Success if is_error => Some(format!("expected success, got an error: {text}")),
        Expect::Error(_) if !is_error => Some(format!("expected an error, got a success: {text}")),
        Expect::Error(Some(want)) if !text.contains(want.as_str()) => Some(format!(
            "expected an error containing \"{want}\", got: {text}"
        )),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct PermOption {
    id: String,
    name: String,
    kind: String,
}

impl PermOption {
    fn new(id: &str, name: &str, kind: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"optionId": self.id, "name": self.name, "kind": self.kind})
    }
}

/// The option set of a permission step (the adapter's shapes).
fn permission_options(spec: Option<&Value>) -> Result<Vec<PermOption>, String> {
    match spec {
        None => Ok(bash_options()),
        Some(Value::String(s)) if s == "bash" => Ok(bash_options()),
        Some(Value::String(s)) if s == "exit_plan" => Ok(vec![
            PermOption::new(
                "exit-plan-clear-auto",
                "Yes, clear context (12% used) and use auto mode",
                "allow_always",
            ),
            PermOption::new("exit-plan-auto", "Yes, and use auto mode", "allow_always"),
            PermOption::new(
                "exit-plan-bypass",
                "Yes, and bypass permissions",
                "allow_always",
            ),
            PermOption::new(
                "exit-plan-default",
                "Yes, manually approve edits",
                "allow_once",
            ),
            PermOption::new("reject", "No, keep planning", "reject_once"),
        ]),
        Some(Value::Array(items)) => {
            let mut options = Vec::new();
            for item in items {
                let field = |k: &str| {
                    item.get(k)
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("permission option needs a string \"{k}\""))
                };
                let kind = field("kind")?;
                check_one_of("option kind", Some(kind), OPTION_KINDS)?;
                options.push(PermOption::new(field("id")?, field("name")?, kind));
            }
            if options.is_empty() {
                return Err("permission.options is empty".into());
            }
            Ok(options)
        }
        Some(other) => Err(format!(
            "permission.options: expected \"bash\", \"exit_plan\" or a list, got {other}"
        )),
    }
}

fn bash_options() -> Vec<PermOption> {
    vec![
        PermOption::new("allow-once", "Yes", "allow_once"),
        PermOption::new(
            "allow-with-updates",
            "Yes, and don't ask again for latexmk commands",
            "allow_always",
        ),
        PermOption::new("reject", "No", "reject_once"),
    ]
}

struct ModeInfo {
    id: &'static str,
    name: &'static str,
    description: &'static str,
}

/// The mode a new session starts in without `initial_mode`.
fn default_mode(options: &Options) -> &'static str {
    if options.modes.as_deref() == Some("codex") {
        "agent"
    } else {
        "default"
    }
}

fn modes_for(options: &Options) -> Vec<ModeInfo> {
    let mode = |id, name, description| ModeInfo {
        id,
        name,
        description,
    };
    let default = mode("default", "Manual", "Always ask before making changes");
    let plan = mode("plan", "Plan", "Create a plan before making changes");
    if options.modes.as_deref() == Some("codex") {
        return vec![
            mode("read-only", "Read-only", "Requires approval to edit files"),
            mode(
                "workspace-write",
                "Workspace access",
                "Edit workspace files",
            ),
            mode(
                "agent",
                "Auto review",
                "Only ask for potentially unsafe actions",
            ),
            mode("agent-full-access", "Full access", "Unrestricted access"),
        ];
    }
    if options.modes.as_deref() == Some("adapter") {
        vec![
            default,
            mode(
                "acceptEdits",
                "Accept edits",
                "Automatically accept all file edits",
            ),
            plan,
            mode("auto", "Auto", "Claude handles permission decisions"),
            mode(
                "bypassPermissions",
                "Bypass permissions",
                "Accepts all permissions",
            ),
        ]
    } else {
        vec![default, plan]
    }
}

const MODELS: &[(&str, &str, &str)] = &[
    ("default", "Default (recommended)", "Opus 5.5"),
    ("opus", "Opus", "Opus 5.5 for complex work"),
    ("sonnet", "Sonnet", "Sonnet 5.5 for everyday tasks"),
    ("haiku", "Haiku", "Haiku 4.5 for quick answers"),
];
const EFFORTS: &[(&str, &str)] = &[
    ("default", "Default"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Xhigh"),
    ("max", "Max"),
];

// ======================================================================================= output

/// Serialized writes to stdout, one JSON-RPC message per line.
struct Out {
    stdout: Mutex<io::Stdout>,
}

impl Out {
    fn send(&self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        let mut out = lock(&self.stdout);
        if out
            .write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            // The client is gone; nobody is left to talk to.
            drop(out);
            shutdown(0);
        }
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        );
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ======================================================================================== agent

struct Session {
    id: String,
    cwd: PathBuf,
    mode: String,
    model: String,
    effort: String,
}

/// The cancel flag of a running turn, with a condvar so sleeps wake at once.
struct Turn {
    cancelled: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Turn {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let _guard = lock(&self.lock);
        self.cv.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Sleeps for `d`; true if the turn was cancelled meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut guard = lock(&self.lock);
        loop {
            if self.is_cancelled() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = self
                .cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

struct Agent {
    out: Out,
    scenario: Scenario,
    /// The client advertised `session.notices`.
    notices: AtomicBool,
    /// The client advertised `fs.readTextFile` / `fs.writeTextFile`.
    fs_read: AtomicBool,
    fs_write: AtomicBool,
    session: Mutex<Option<Session>>,
    turn: Mutex<Option<Arc<Turn>>>,
    /// Our requests to the client (permission), by id, waiting for the whole response message.
    requests: Mutex<HashMap<u64, Sender<Value>>>,
    next_request: AtomicU64,
    next_session: AtomicU64,
    tool_seq: AtomicU64,
}

/// The MCP server process, killed on every exit path.
static MCP: Mutex<Option<Arc<Mcp>>> = Mutex::new(None);

/// Closes the MCP server and exits.
fn shutdown(code: i32) -> ! {
    if let Some(mcp) = lock(&MCP).take() {
        mcp.close();
    }
    let _ = io::stderr().flush();
    std::process::exit(code)
}

fn serve(scenario: Scenario) -> ExitCode {
    if let Some(text) = &scenario.options.startup_stderr {
        eprintln!("{text}");
    }
    let agent = Arc::new(Agent {
        out: Out {
            stdout: Mutex::new(io::stdout()),
        },
        scenario,
        notices: AtomicBool::new(false),
        fs_read: AtomicBool::new(false),
        fs_write: AtomicBool::new(false),
        session: Mutex::new(None),
        turn: Mutex::new(None),
        requests: Mutex::new(HashMap::new()),
        next_request: AtomicU64::new(0),
        next_session: AtomicU64::new(1),
        tool_seq: AtomicU64::new(1),
    });
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        handle_line(&agent, &line);
    }
    // stdin EOF: the client is done.
    if let Some(turn) = lock(&agent.turn).as_ref() {
        turn.cancel();
    }
    shutdown(0)
}

fn handle_line(agent: &Arc<Agent>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: unparsable line from the client: {e}");
            agent.out.error(&Value::Null, -32700, "Parse error");
            return;
        }
    };
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (Some(method), Some(id)) => handle_request(agent, method, id, params),
        (Some(method), None) => handle_notification(agent, method, &params),
        (None, Some(id)) => {
            let waiter = id.as_u64().and_then(|n| lock(&agent.requests).remove(&n));
            match waiter {
                Some(tx) => {
                    let _ = tx.send(msg);
                }
                None => eprintln!("{NAME}: response to an unknown request {id}"),
            }
        }
        (None, None) => eprintln!("{NAME}: ignoring a message without method or id"),
    }
}

fn handle_notification(agent: &Agent, method: &str, params: &Value) {
    match method {
        "session/cancel" => {
            let current = lock(&agent.session).as_ref().map(|s| s.id.clone());
            if params.get("sessionId").and_then(Value::as_str) != current.as_deref() {
                eprintln!("{NAME}: session/cancel for an unknown session");
                return;
            }
            if let Some(turn) = lock(&agent.turn).as_ref() {
                turn.cancel();
            }
        }
        other => eprintln!("{NAME}: ignoring notification {other}"),
    }
}

fn handle_request(agent: &Arc<Agent>, method: &str, id: Value, params: Value) {
    let result = match method {
        "initialize" => initialize(agent, &params),
        "authenticate" => Ok(json!({})),
        "session/new" => match session_new(agent, &params) {
            Ok(result) => {
                agent.out.respond(&id, result);
                send_available_commands(agent);
                return;
            }
            Err(e) => Err(e),
        },
        "session/prompt" => match prompt(agent, &params, id.clone()) {
            // Answered by the turn thread.
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => set_mode(agent, &params),
        "session/set_config_option" => set_config_option(agent, &params),
        "session/close" => close_session(agent, &params),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    match result {
        Ok(result) => agent.out.respond(&id, result),
        Err((code, message)) => {
            eprintln!("{NAME}: {method} failed: {message}");
            agent.out.error(&id, code, &message);
        }
    }
}

type RpcResult = Result<Value, (i64, String)>;

fn invalid(message: impl Into<String>) -> (i64, String) {
    (-32602, message.into())
}

fn initialize(agent: &Agent, params: &Value) -> RpcResult {
    if agent.scenario.options.exit_on.as_deref() == Some("initialize") {
        shutdown(agent.scenario.options.exit_code.unwrap_or(1));
    }
    let notices = params
        .pointer("/clientCapabilities/session/notices")
        .is_some_and(Value::is_object);
    agent.notices.store(notices, Ordering::SeqCst);
    let cap = |name: &str| {
        params
            .pointer(&format!("/clientCapabilities/fs/{name}"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    let (read, write) = (cap("readTextFile"), cap("writeTextFile"));
    agent.fs_read.store(read, Ordering::SeqCst);
    agent.fs_write.store(write, Ordering::SeqCst);
    match agent.scenario.options.fs.as_deref() {
        Some("required") if !(read && write) => {
            return Err(invalid(
                "initialize: the client must advertise fs.readTextFile and fs.writeTextFile",
            ));
        }
        Some("forbidden") if read || write => {
            return Err(invalid(
                "initialize: the client must not advertise fs.readTextFile or fs.writeTextFile",
            ));
        }
        _ => {}
    }
    if params.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        eprintln!(
            "{NAME}: the client asked for protocol {:?}; answering 1",
            params.get("protocolVersion")
        );
    }
    Ok(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {
                "image": !agent.scenario.options.no_images,
                "embeddedContext": true
            },
            "mcpCapabilities": {"http": false, "sse": false},
            "sessionCapabilities": {"close": {}}
        },
        "agentInfo": {
            "name": agent.scenario.options.agent_name.as_deref().unwrap_or(NAME),
            "title": "Lemma fake agent",
            "version": VERSION
        },
        "authMethods": []
    }))
}

fn session_new(agent: &Agent, params: &Value) -> RpcResult {
    let options = &agent.scenario.options;
    if options.exit_on.as_deref() == Some("session/new") {
        shutdown(options.exit_code.unwrap_or(1));
    }
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| invalid("session/new: cwd must be an absolute path"))?;
    check_meta(params.get("_meta"))?;
    if let Some(e) = &options.session_new_error {
        return Err((e.code, e.message.clone()));
    }
    if options.session_new_delay_ms > 0 {
        thread::sleep(Duration::from_millis(options.session_new_delay_ms));
    }
    if options.mcp.as_deref() != Some("skip") {
        let servers = params
            .get("mcpServers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let server = servers
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("lemma"))
            .ok_or_else(|| invalid("session/new: no MCP server named \"lemma\" in mcpServers"))?;
        if let Some(old) = lock(&MCP).take() {
            old.close();
        }
        let mcp =
            Mcp::launch(server, &cwd).map_err(|e| (-32603, format!("MCP server lemma: {e}")))?;
        *lock(&MCP) = Some(mcp);
    }
    let n = agent.next_session.fetch_add(1, Ordering::SeqCst);
    let session = Session {
        id: format!("fake-session-{}-{n}", std::process::id()),
        cwd,
        mode: options
            .initial_mode
            .clone()
            .unwrap_or_else(|| default_mode(options).into()),
        model: "default".into(),
        effort: "default".into(),
    };
    let result = json!({
        "sessionId": session.id,
        "modes": modes_json(options, &session.mode),
        "configOptions": config_options_json(options, &session),
    });
    if let Some(turn) = lock(&agent.turn).as_ref() {
        turn.cancel();
    }
    *lock(&agent.session) = Some(session);
    Ok(result)
}

/// `_meta.claudeCode.options.disallowedTools` must route all edits through Lemma's tools.
fn check_meta(meta: Option<&Value>) -> Result<(), (i64, String)> {
    let disallowed: Vec<&str> = meta
        .and_then(|m| m.pointer("/claudeCode/options/disallowedTools"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = ["Edit", "Write", "NotebookEdit"]
        .into_iter()
        .filter(|t| !disallowed.contains(t))
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "session/new: _meta.claudeCode.options.disallowedTools must contain Edit, Write and \
             NotebookEdit (missing: {})",
            missing.join(", ")
        )));
    }
    let allowed = meta
        .and_then(|m| m.pointer("/claudeCode/options/allowedTools"))
        .and_then(Value::as_array);
    if !allowed.is_some_and(|a| a.iter().any(|t| t == "mcp__lemma__*")) {
        eprintln!("{NAME}: warning: allowedTools does not contain mcp__lemma__*");
    }
    if meta
        .and_then(|m| m.pointer("/systemPrompt/append"))
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        eprintln!("{NAME}: warning: no _meta.systemPrompt.append");
    }
    Ok(())
}

fn modes_json(options: &Options, current: &str) -> Value {
    let available: Vec<Value> = modes_for(options)
        .iter()
        .map(|m| json!({"id": m.id, "name": m.name, "description": m.description}))
        .collect();
    json!({"currentModeId": current, "availableModes": available})
}

fn config_options_json(options: &Options, session: &Session) -> Value {
    let modes: Vec<Value> = modes_for(options)
        .iter()
        .map(|m| json!({"value": m.id, "name": m.name, "description": m.description}))
        .collect();
    let models: Vec<Value> = MODELS
        .iter()
        .map(|(v, n, d)| json!({"value": v, "name": n, "description": d}))
        .collect();
    let efforts: Vec<Value> = EFFORTS
        .iter()
        .map(|(v, n)| json!({"value": v, "name": n}))
        .collect();
    json!([
        {"id": "mode", "name": "Mode", "description": "Session permission mode", "category": "mode",
         "type": "select", "currentValue": session.mode, "options": modes},
        {"id": "model", "name": "Model", "description": "AI model to use", "category": "model",
         "type": "select", "currentValue": session.model, "options": models},
        {"id": "effort", "name": "Effort", "description": "Available effort levels for this model",
         "category": "thought_level", "type": "select", "currentValue": session.effort, "options": efforts}
    ])
}

fn send_available_commands(agent: &Agent) {
    let Some(session_id) = lock(&agent.session).as_ref().map(|s| s.id.clone()) else {
        return;
    };
    agent.out.notify(
        "session/update",
        json!({"sessionId": session_id, "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "compact",
                 "description": "Clear conversation history but keep a summary in context",
                 "input": {"hint": "<optional custom summarization instructions>"}},
                {"name": "init",
                 "description": "Initialize a new CLAUDE.md file with codebase documentation"}
            ]
        }}),
    );
}

/// The session for a request's `sessionId`, or an invalid-params error.
fn with_session<R>(
    agent: &Agent,
    params: &Value,
    f: impl FnOnce(&mut Session) -> Result<R, (i64, String)>,
) -> Result<R, (i64, String)> {
    let mut guard = lock(&agent.session);
    let session = guard
        .as_mut()
        .filter(|s| params.get("sessionId").and_then(Value::as_str) == Some(s.id.as_str()))
        .ok_or_else(|| invalid("Session not found"))?;
    f(session)
}

fn set_mode(agent: &Agent, params: &Value) -> RpcResult {
    let options = &agent.scenario.options;
    with_session(agent, params, |session| {
        let mode = params
            .get("modeId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("modeId missing"))?;
        if !modes_for(options).iter().any(|m| m.id == mode) {
            return Err(invalid(format!(
                "Mode {mode} is not available in this session"
            )));
        }
        session.mode = mode.to_owned();
        // Like the adapter: a config_option_update, then `{}` (no current_mode_update). Sent
        // under the session lock, so a concurrent `mode` step cannot interleave stale state.
        agent.out.notify(
            "session/update",
            json!({"sessionId": session.id, "update": {"sessionUpdate": "config_option_update",
                   "configOptions": config_options_json(options, session)}}),
        );
        Ok(())
    })?;
    Ok(json!({}))
}

fn set_config_option(agent: &Agent, params: &Value) -> RpcResult {
    let options = &agent.scenario.options;
    let config = with_session(agent, params, |session| {
        let config_id = params
            .get("configId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("configId missing"))?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("value must be a string for select options"))?;
        let valid = match config_id {
            "mode" => modes_for(options).iter().any(|m| m.id == value),
            "model" => MODELS.iter().any(|(v, _, _)| *v == value),
            "effort" => EFFORTS.iter().any(|(v, _)| *v == value),
            other => return Err(invalid(format!("Unknown config option: {other}"))),
        };
        if !valid {
            return Err(invalid(format!(
                "Invalid value for config option {config_id}: {value}"
            )));
        }
        match config_id {
            "mode" => {
                session.mode = value.to_owned();
                // Under the session lock, like `set_mode`.
                agent.out.notify(
                    "session/update",
                    json!({"sessionId": session.id, "update": {"sessionUpdate": "current_mode_update",
                           "currentModeId": value}}),
                );
            }
            "model" => session.model = value.to_owned(),
            _ => session.effort = value.to_owned(),
        }
        Ok(config_options_json(options, session))
    })?;
    Ok(json!({"configOptions": config}))
}

fn close_session(agent: &Agent, params: &Value) -> RpcResult {
    with_session(agent, params, |_| Ok(()))?;
    if let Some(turn) = lock(&agent.turn).as_ref() {
        turn.cancel();
    }
    Ok(json!({}))
}

fn prompt(agent: &Arc<Agent>, params: &Value, id: Value) -> Result<(), (i64, String)> {
    let (session_id, cwd) = with_session(agent, params, |s| Ok((s.id.clone(), s.cwd.clone())))?;
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("prompt must be a list of content blocks"))?;
    let text: Vec<&str> = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        // Lemma's instructions (agents other than Claude Code get them in the first prompt)
        // are not the user's words.
        .filter(|t| !t.starts_with(INSTRUCTIONS_TAG))
        .collect();
    let (keyword, steps) = agent
        .scenario
        .select(&text.join("\n"))
        .ok_or_else(|| (-32603, "no scenario turn matches the prompt".to_owned()))?;
    let steps = steps.to_vec();
    let keyword = keyword.to_owned();
    let turn = Arc::new(Turn::new());
    {
        let mut current = lock(&agent.turn);
        if current.is_some() {
            return Err((-32600, "a prompt is already running".to_owned()));
        }
        *current = Some(turn.clone());
    }
    let agent = agent.clone();
    thread::spawn(move || {
        let ctx = TurnCtx {
            agent: &agent,
            turn: &turn,
            session_id,
            cwd,
            prompt: blocks,
        };
        let result = ctx.run(&steps);
        {
            let mut current = lock(&agent.turn);
            if current.as_ref().is_some_and(|t| Arc::ptr_eq(t, &turn)) {
                *current = None;
            }
        }
        match result {
            Ok(reason) => agent.out.respond(&id, json!({"stopReason": reason})),
            Err(e) => {
                let message = format!("scenario \"{keyword}\": {e}");
                eprintln!("{NAME}: {message}");
                agent.out.error(&id, -32603, &message);
            }
        }
    });
    Ok(())
}

// ========================================================================================= turns

/// How a step ends.
enum Flow {
    Continue,
    Stop(String),
}

struct TurnCtx<'a> {
    agent: &'a Agent,
    turn: &'a Turn,
    session_id: String,
    cwd: PathBuf,
    prompt: Vec<Value>,
}

impl TurnCtx<'_> {
    fn run(&self, steps: &[Value]) -> Result<String, String> {
        match self.run_steps(steps, "")? {
            Flow::Stop(reason) => Ok(reason),
            Flow::Continue if self.turn.is_cancelled() => Ok("cancelled".into()),
            Flow::Continue => Ok("end_turn".into()),
        }
    }

    fn run_steps(&self, steps: &[Value], prefix: &str) -> Result<Flow, String> {
        for (i, raw) in steps.iter().enumerate() {
            if self.turn.is_cancelled() {
                return Ok(Flow::Stop("cancelled".into()));
            }
            let label = format!("{prefix}step {}", i + 1);
            let step = parse_step(raw).map_err(|e| format!("{label}: {e}"))?;
            match self
                .step(&step, &label)
                .map_err(|e| format!("{label}: {e}"))?
            {
                Flow::Continue => {}
                stop => return Ok(stop),
            }
        }
        Ok(Flow::Continue)
    }

    fn update(&self, update: Value) {
        self.agent.out.notify(
            "session/update",
            json!({"sessionId": self.session_id, "update": update}),
        );
    }

    fn chunk(&self, text: &str, thought: bool) {
        let kind = if thought {
            "agent_thought_chunk"
        } else {
            "agent_message_chunk"
        };
        self.update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}));
    }

    fn cancelled_flow(&self) -> Flow {
        Flow::Stop("cancelled".into())
    }

    fn step(&self, step: &Step, label: &str) -> Result<Flow, String> {
        match step {
            Step::Say(text) => Ok(self.pieces(text, false)),
            Step::Think(text) => Ok(self.pieces(text, true)),
            Step::Stream(s) => {
                for i in 1..=s.count {
                    if self.turn.is_cancelled() {
                        return Ok(self.cancelled_flow());
                    }
                    self.chunk(&s.text.replace("{{i}}", &i.to_string()), s.thought);
                    if s.delay_ms > 0 && self.turn.sleep(Duration::from_millis(s.delay_ms)) {
                        return Ok(self.cancelled_flow());
                    }
                }
                Ok(Flow::Continue)
            }
            Step::Plan(items) => {
                let entries: Vec<Value> = items
                    .iter()
                    .map(|item| match item {
                        PlanItem::Text(content) => {
                            json!({"content": content, "priority": "medium", "status": "pending"})
                        }
                        PlanItem::Entry(e) => json!({
                            "content": e.content,
                            "priority": e.priority.as_deref().unwrap_or("medium"),
                            "status": e.status.as_deref().unwrap_or("pending"),
                        }),
                    })
                    .collect();
                self.update(json!({"sessionUpdate": "plan", "entries": entries}));
                Ok(Flow::Continue)
            }
            Step::Tool(t) => self.tool(t),
            Step::Permission(p) => self.permission(p),
            Step::DiskWrite(d) => self.disk_write(d),
            Step::FsRead(r) => self.fs_read(r),
            Step::FsWrite(w) => self.fs_write(w),
            Step::FsEdit(e) => self.fs_edit(e),
            Step::Usage(u) => {
                self.update(
                    json!({"sessionUpdate": "usage_update", "used": u.used, "size": u.size}),
                );
                Ok(Flow::Continue)
            }
            Step::SleepMs(ms) => Ok(if self.turn.sleep(Duration::from_millis(*ms)) {
                self.cancelled_flow()
            } else {
                Flow::Continue
            }),
            Step::Stop(s) => Ok(Flow::Stop(s.reason().to_owned())),
            Step::Notice(n) => {
                let (title, description, severity) = match n {
                    NoticeStep::Title(t) => (t.as_str(), None, "info"),
                    NoticeStep::Full(f) => (
                        f.title.as_str(),
                        f.description.as_deref(),
                        f.severity.as_deref().unwrap_or("info"),
                    ),
                };
                if self.agent.notices.load(Ordering::SeqCst) {
                    let mut update =
                        json!({"sessionUpdate": "notice", "severity": severity, "title": title});
                    if let Some(d) = description {
                        update["description"] = json!(d);
                    }
                    self.update(update);
                } else {
                    let text = match description {
                        Some(d) => format!("**{title}:** {d}"),
                        None => format!("**{title}**"),
                    };
                    self.chunk(&text, false);
                }
                Ok(Flow::Continue)
            }
            Step::Mode(mode) => {
                let options = &self.agent.scenario.options;
                if !modes_for(options).iter().any(|m| m.id == mode) {
                    return Err(format!("mode \"{mode}\" is not offered"));
                }
                // The state change and both updates under the session lock: a concurrent
                // set_mode cannot slip between them.
                let mut guard = lock(&self.agent.session);
                let session = guard.as_mut().ok_or("no session")?;
                session.mode.clone_from(mode);
                let config = config_options_json(options, session);
                self.update(json!({"sessionUpdate": "current_mode_update", "currentModeId": mode}));
                self.update(
                    json!({"sessionUpdate": "config_option_update", "configOptions": config}),
                );
                Ok(Flow::Continue)
            }
            Step::RawUpdate(update) => {
                self.update(update.clone());
                Ok(Flow::Continue)
            }
            Step::Exit(code) => {
                // A crash: no clean-up, the MCP server is left to the client.
                let _ = io::stderr().flush();
                std::process::exit(*code)
            }
            Step::Stderr(text) => {
                eprintln!("{text}");
                Ok(Flow::Continue)
            }
            Step::CloseStdout(close) => {
                if *close {
                    // Hold the writer lock forever so nothing else writes to the closed fd, then
                    // close fd 1; the process keeps running until it is killed.
                    let guard = lock(&self.agent.out.stdout);
                    // SAFETY: fd 1 is our stdout; it is not used again (the lock is never
                    // released).
                    drop(unsafe {
                        <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(1)
                    });
                    std::mem::forget(guard);
                    loop {
                        thread::park();
                    }
                }
                Ok(Flow::Continue)
            }
            Step::Repeat(r) => {
                for i in 1..=r.times {
                    let steps: Vec<Value> = r.steps.iter().map(|s| substitute(s, i)).collect();
                    match self.run_steps(&steps, &format!("{label}, repeat {i}, "))? {
                        Flow::Continue => {}
                        stop => return Ok(stop),
                    }
                }
                Ok(Flow::Continue)
            }
            Step::EchoPrompt(echo) => {
                if *echo {
                    self.chunk(&json!({"prompt": self.prompt}).to_string(), false);
                }
                Ok(Flow::Continue)
            }
        }
    }

    /// A `say`/`think`: three pieces, 20 ms apart.
    fn pieces(&self, text: &str, thought: bool) -> Flow {
        let pieces = split_pieces(text, 3);
        for (i, piece) in pieces.iter().enumerate() {
            if self.turn.is_cancelled() {
                return self.cancelled_flow();
            }
            self.chunk(piece, thought);
            if i + 1 < pieces.len() && self.turn.sleep(PIECE_DELAY) {
                return self.cancelled_flow();
            }
        }
        Flow::Continue
    }

    fn next_tool_id(&self) -> String {
        format!(
            "toolu_fake_{:04}",
            self.agent.tool_seq.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn tool_done(&self, id: &str, ok: bool, text: &str) {
        self.update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if ok { "completed" } else { "failed" },
            "content": [{"type": "content", "content": {"type": "text", "text": text}}],
        }));
    }

    fn tool(&self, t: &ToolStep) -> Result<Flow, String> {
        let expect = t.expect()?;
        let id = self.next_tool_id();
        let title = format!("mcp__lemma__{}", t.name);
        self.update(json!({
            "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "name": title,
            "kind": "other", "status": "pending", "rawInput": t.args,
        }));
        let mcp = lock(&MCP).clone();
        let Some(mcp) = mcp else {
            self.tool_done(&id, false, "no MCP server");
            return Err(format!(
                "tool {}: no MCP server (_options.mcp is \"skip\"?)",
                t.name
            ));
        };
        if !mcp.tools.contains(&t.name) {
            self.tool_done(&id, false, "unknown tool");
            return Err(format!(
                "tool {} is not in the MCP server's tools/list ({})",
                t.name,
                mcp.tools.join(", ")
            ));
        }
        self.update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress"}),
        );
        let (is_error, text) = match mcp.call(&t.name, &t.args, self.turn) {
            Ok(result) => result,
            Err(CallError::Cancelled) => {
                self.tool_done(&id, false, "Cancelled");
                return Ok(self.cancelled_flow());
            }
            Err(CallError::Failed(e)) => {
                self.tool_done(&id, false, &e);
                return Err(format!("tool {}: {e}", t.name));
            }
        };
        self.tool_done(&id, !is_error, &text);
        let mismatch = match &expect {
            Expect::Any => None,
            Expect::Success if is_error => Some(format!("expected success, got an error: {text}")),
            Expect::Error(_) if !is_error => {
                Some(format!("expected an error, got a success: {text}"))
            }
            Expect::Error(Some(want)) if !text.contains(want.as_str()) => Some(format!(
                "expected an error containing \"{want}\", got: {text}"
            )),
            _ => None,
        };
        if let Some(m) = mismatch {
            return Err(format!("tool {}: {m}", t.name));
        }
        if let Some(want) = &t.expect_text
            && !text.contains(want.as_str())
        {
            return Err(format!(
                "tool {}: expected the result to contain \"{want}\", got: {text}",
                t.name
            ));
        }
        Ok(Flow::Continue)
    }

    fn permission(&self, p: &PermissionStep) -> Result<Flow, String> {
        let options = permission_options(p.options.as_ref())?;
        let kind = p.kind.as_deref().unwrap_or("execute");
        let id = self.next_tool_id();
        let raw_input = json!({"command": p.title});
        self.update(json!({
            "sessionUpdate": "tool_call", "toolCallId": id, "title": p.title, "kind": kind,
            "status": "pending", "rawInput": raw_input,
        }));
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": {"toolCallId": id, "title": p.title, "kind": kind, "status": "pending", "rawInput": raw_input},
            "options": options.iter().map(PermOption::to_json).collect::<Vec<_>>(),
        });
        let response = self.request_client("session/request_permission", params)?;
        if let Some(error) = response.get("error") {
            self.tool_done(&id, false, "permission request failed");
            return Err(format!("session/request_permission failed: {error}"));
        }
        let outcome = response
            .pointer("/result/outcome")
            .ok_or("permission answer without outcome")?;
        let selected = match outcome.get("outcome").and_then(Value::as_str) {
            Some("cancelled") => None,
            Some("selected") => Some(
                outcome
                    .get("optionId")
                    .and_then(Value::as_str)
                    .ok_or("selected outcome without optionId")?
                    .to_owned(),
            ),
            other => return Err(format!("unknown permission outcome {other:?}")),
        };
        let answer = selected.as_deref().unwrap_or("cancelled");
        if p.report {
            self.chunk(&format!("Permission answer: {answer}. "), false);
        }
        let Some(option_id) = selected else {
            // The client cancelled the turn (it must also send session/cancel).
            self.tool_done(&id, false, "Cancelled");
            self.turn.sleep(Duration::from_millis(500));
            return Ok(self.cancelled_flow());
        };
        let Some(option) = options.iter().find(|o| o.id == option_id) else {
            return Err(format!(
                "the client selected \"{option_id}\", which was not offered"
            ));
        };
        let allowed = option.kind.starts_with("allow");
        self.tool_done(
            &id,
            allowed,
            if allowed {
                "Done."
            } else {
                "The user doesn't want to proceed with this tool use."
            },
        );
        match p.expect.as_deref() {
            None | Some("any") => Ok(Flow::Continue),
            Some(want) if want == option_id => Ok(Flow::Continue),
            Some(want) => Err(format!(
                "permission: expected \"{want}\", the client answered \"{option_id}\""
            )),
        }
    }

    /// Sends a request to the client and waits for its response message. When the turn is
    /// cancelled meanwhile, the client must still answer (with Cancelled for permissions):
    /// it gets [`CANCEL_ANSWER_GRACE`].
    fn request_client(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.agent.next_request.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.agent.requests).insert(id, tx);
        self.agent
            .out
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut cancelled_at: Option<Instant> = None;
        loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(response) => return Ok(response),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("{method}: the client went away"));
                }
            }
            if self.turn.is_cancelled() {
                let since = *cancelled_at.get_or_insert_with(Instant::now);
                if since.elapsed() > CANCEL_ANSWER_GRACE {
                    lock(&self.agent.requests).remove(&id);
                    return Err(format!(
                        "{method} {id} was not answered within {CANCEL_ANSWER_GRACE:?} after session/cancel"
                    ));
                }
            }
        }
    }

    /// The absolute path of an `fs_*` step.
    fn fs_path(&self, path: &str) -> PathBuf {
        if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.cwd.join(path)
        }
    }

    /// Sends an `fs/*` request: `Ok(Ok(result))`, `Ok(Err(message))` for the client's error.
    fn fs_request(&self, method: &str, params: Value) -> Result<Result<Value, String>, String> {
        let (flag, capability) = if method == "fs/read_text_file" {
            (&self.agent.fs_read, "fs.readTextFile")
        } else {
            (&self.agent.fs_write, "fs.writeTextFile")
        };
        if !flag.load(Ordering::SeqCst) {
            return Err(format!(
                "{method}: the client did not advertise {capability}"
            ));
        }
        let response = self.request_client(method, params)?;
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)")
                .to_owned();
            return Ok(Err(message));
        }
        Ok(Ok(response.get("result").cloned().unwrap_or(Value::Null)))
    }

    fn fs_card(&self, kind: &str, title: &str, path: &Path) -> String {
        let id = self.next_tool_id();
        self.update(json!({
            "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind,
            "status": "in_progress", "locations": [{"path": path}],
            "rawInput": {"path": path},
        }));
        id
    }

    fn fs_read(&self, r: &FsReadStep) -> Result<Flow, String> {
        let expect = expect_of(&r.expect_error)?;
        let path = self.fs_path(&r.path);
        let id = self.fs_card("read", &format!("read {}", r.path), &path);
        let mut params = json!({"sessionId": self.session_id, "path": path});
        if let Some(line) = r.line {
            params["line"] = json!(line);
        }
        if let Some(limit) = r.limit {
            params["limit"] = json!(limit);
        }
        let (is_error, text) = match self.fs_request("fs/read_text_file", params)? {
            Ok(result) => (
                false,
                result
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("fs/read_text_file: no content in the result")?
                    .to_owned(),
            ),
            Err(message) => (true, message),
        };
        self.tool_done(&id, !is_error, &text);
        if let Some(m) = mismatch(&expect, is_error, &text) {
            return Err(format!("fs_read {}: {m}", r.path));
        }
        if let Some(want) = &r.expect_text
            && !text.contains(want.as_str())
        {
            return Err(format!(
                "fs_read {}: expected the content to contain \"{want}\", got: {text}",
                r.path
            ));
        }
        if let Some(want) = &r.expect_content
            && text != *want
        {
            return Err(format!(
                "fs_read {}: expected the content {want:?}, got {text:?}",
                r.path
            ));
        }
        Ok(Flow::Continue)
    }

    fn fs_write(&self, w: &FsWriteStep) -> Result<Flow, String> {
        let expect = expect_of(&w.expect_error)?;
        let path = self.fs_path(&w.path);
        let id = self.fs_card("edit", &format!("write {}", w.path), &path);
        let params = json!({"sessionId": self.session_id, "path": path, "content": w.content});
        let (is_error, text) = match self.fs_request("fs/write_text_file", params)? {
            Ok(_) => (false, format!("Wrote {}", w.path)),
            Err(message) => (true, message),
        };
        self.tool_done(&id, !is_error, &text);
        if let Some(m) = mismatch(&expect, is_error, &text) {
            return Err(format!("fs_write {}: {m}", w.path));
        }
        Ok(Flow::Continue)
    }

    fn fs_edit(&self, e: &FsEditStep) -> Result<Flow, String> {
        let expect = expect_of(&e.expect_error)?;
        let path = self.fs_path(&e.path);
        let id = self.fs_card("edit", &format!("edit {}", e.path), &path);
        let read = self.fs_request(
            "fs/read_text_file",
            json!({"sessionId": self.session_id, "path": path}),
        )?;
        let result = read.and_then(|result| {
            let current = result
                .get("content")
                .and_then(Value::as_str)
                .ok_or("fs/read_text_file: no content in the result")?;
            let n = current.matches(e.old_string.as_str()).count();
            match (n, e.replace_all) {
                (0, _) => Err(format!("old_string not found in {}", e.path)),
                (1, _) | (_, true) => Ok(if e.replace_all {
                    current.replace(e.old_string.as_str(), &e.new_string)
                } else {
                    current.replacen(e.old_string.as_str(), &e.new_string, 1)
                }),
                (n, false) => Err(format!("old_string occurs {n} times in {}", e.path)),
            }
        });
        let (is_error, text) = match result {
            Ok(content) => match self.fs_request(
                "fs/write_text_file",
                json!({"sessionId": self.session_id, "path": path, "content": content}),
            )? {
                Ok(_) => (false, format!("Edited {}", e.path)),
                Err(message) => (true, message),
            },
            Err(message) => (true, message),
        };
        self.tool_done(&id, !is_error, &text);
        if let Some(m) = mismatch(&expect, is_error, &text) {
            return Err(format!("fs_edit {}: {m}", e.path));
        }
        Ok(Flow::Continue)
    }

    fn disk_write(&self, d: &DiskWriteStep) -> Result<Flow, String> {
        let rel = relative_path(&d.path)?;
        let path = self.cwd.join(&rel);
        let (command, content) = match (&d.content, &d.old_string, &d.new_string) {
            (Some(content), _, _) => (format!("cat > {}", d.path), content.clone()),
            (None, Some(old), Some(new)) => {
                let current = std::fs::read_to_string(&path)
                    .map_err(|e| format!("disk_write: cannot read {}: {e}", d.path))?;
                if !current.contains(old.as_str()) {
                    return Err(format!(
                        "disk_write: \"{old}\" does not occur in {}",
                        d.path
                    ));
                }
                (
                    format!("sed -i 's/…/…/' {}", d.path),
                    current.replacen(old.as_str(), new, 1),
                )
            }
            _ => return Err("disk_write needs content or old_string + new_string".into()),
        };
        let id = self.next_tool_id();
        self.update(json!({
            "sessionUpdate": "tool_call", "toolCallId": id, "title": command, "kind": "execute",
            "status": "in_progress", "rawInput": {"command": command},
        }));
        let written = if d.in_place {
            std::fs::write(&path, &content)
        } else {
            write_by_rename(&path, &content)
        };
        if let Err(e) = written {
            self.tool_done(&id, false, &e.to_string());
            return Err(format!("disk_write {}: {e}", d.path));
        }
        self.tool_done(&id, true, "");
        Ok(Flow::Continue)
    }
}

/// Writes like `sed -i`: a temporary file next to `path`, then a rename over it.
fn write_by_rename(path: &Path, content: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.fake-agent-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Splits `text` into at most `n` non-empty pieces of about equal character counts.
fn split_pieces(text: &str, n: usize) -> Vec<&str> {
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let len = starts.len();
    if len == 0 {
        return Vec::new();
    }
    let n = n.min(len);
    (0..n)
        .map(|k| {
            let a = starts[k * len / n];
            let b = if k + 1 == n {
                text.len()
            } else {
                starts[(k + 1) * len / n]
            };
            &text[a..b]
        })
        .collect()
}

// ==================================================================================== MCP client

enum CallError {
    Cancelled,
    Failed(String),
}

/// The stdio MCP server of the session (Lemma's `lemma-mcp` bridge), driven like Claude CLI.
struct Mcp {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    /// Requests waiting for their response, by the JSON text of the id.
    pending: Arc<Mutex<HashMap<String, Sender<Value>>>>,
    next_id: AtomicU64,
    tools: Vec<String>,
}

impl Mcp {
    fn launch(server: &Value, cwd: &Path) -> Result<Arc<Mcp>, String> {
        let command = server
            .get("command")
            .and_then(Value::as_str)
            .ok_or("not a stdio server (no command)")?;
        let args: Vec<String> = server
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut cmd = Command::new(command);
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for var in server
            .get("env")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if let (Some(name), Some(value)) = (
                var.get("name").and_then(Value::as_str),
                var.get("value").and_then(Value::as_str),
            ) {
                cmd.env(name, value);
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {command}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let pending: Arc<Mutex<HashMap<String, Sender<Value>>>> = Arc::default();
        let mut mcp = Mcp {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending: pending.clone(),
            next_id: AtomicU64::new(2),
            tools: Vec::new(),
        };
        let reader_pending = pending.clone();
        let (server_requests_tx, server_requests_rx) = mpsc::channel::<Value>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                    eprintln!("{NAME}: MCP server sent a line that is not JSON");
                    continue;
                };
                match (msg.get("method"), msg.get("id")) {
                    (None, Some(id)) => {
                        if let Some(tx) = lock(&reader_pending).remove(&id.to_string()) {
                            let _ = tx.send(msg);
                        }
                    }
                    (Some(_), Some(_)) => {
                        let _ = server_requests_tx.send(msg);
                    }
                    _ => {}
                }
            }
            // EOF: fail everything still waiting.
            lock(&reader_pending).clear();
        });
        let result = mcp.handshake();
        if let Err(e) = result {
            mcp.close();
            return Err(e);
        }
        let mcp = Arc::new(mcp);
        // Requests from the server (ping, roots/list): answered from a small thread.
        let answering = Arc::downgrade(&mcp);
        thread::spawn(move || {
            for msg in server_requests_rx {
                let Some(mcp) = answering.upgrade() else {
                    break;
                };
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let reply = if msg.get("method").and_then(Value::as_str) == Some("ping") {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                };
                let _ = mcp.write_line(&reply.to_string());
            }
        });
        Ok(mcp)
    }

    fn write_line(&self, line: &str) -> Result<(), String> {
        let mut guard = lock(&self.stdin);
        let stdin = guard.as_mut().ok_or("the MCP server is closed")?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("cannot write to the MCP server: {e}"))
    }

    fn expect_response(&self, id: &str) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id.to_owned(), tx);
        rx
    }

    fn handshake_request(&self, line: &str, id: &str, what: &str) -> Result<Value, String> {
        let rx = self.expect_response(id);
        self.write_line(line)?;
        rx.recv_timeout(MCP_HANDSHAKE_TIMEOUT).map_err(|e| match e {
            RecvTimeoutError::Timeout => {
                format!("no answer to {what} within {MCP_HANDSHAKE_TIMEOUT:?}")
            }
            RecvTimeoutError::Disconnected => {
                format!("the server closed the connection during {what}")
            }
        })
    }

    /// The Claude CLI 2.1.288 sequence; fills `tools`.
    fn handshake(&mut self) -> Result<(), String> {
        // Any answer (result or error) is fine: the CLI falls back to `initialize`.
        self.handshake_request(
            MCP_DISCOVER,
            "\"server-discover-probe-1\"",
            "server/discover",
        )?;
        let init = self.handshake_request(MCP_INITIALIZE, "0", "initialize")?;
        if let Some(error) = init.get("error") {
            return Err(format!("initialize failed: {error}"));
        }
        let version = init
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !MCP_VERSIONS.contains(&version) {
            return Err(format!(
                "initialize answered protocol version \"{version}\", expected one of {}",
                MCP_VERSIONS.join(", ")
            ));
        }
        if !init
            .pointer("/result/capabilities/tools")
            .is_some_and(Value::is_object)
        {
            return Err("initialize answer has no capabilities.tools".into());
        }
        let name = init
            .pointer("/result/serverInfo/name")
            .and_then(Value::as_str);
        if name != Some("lemma") {
            return Err(format!(
                "initialize answer has serverInfo.name {name:?}, expected \"lemma\""
            ));
        }
        self.write_line(MCP_INITIALIZED)?;
        let list = self.handshake_request(MCP_TOOLS_LIST, "1", "tools/list")?;
        let tools = list
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .ok_or("tools/list answer has no tools")?;
        self.tools = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        Ok(())
    }

    /// `tools/call`; `(is_error, text)` of the result. Cancelling the turn stops waiting (the
    /// late answer is dropped).
    fn call(&self, name: &str, args: &Value, turn: &Turn) -> Result<(bool, String), CallError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let line = format!(
            r#"{{"method":"tools/call","params":{{"name":{},"arguments":{}}},"jsonrpc":"2.0","id":{id}}}"#,
            Value::String(name.to_owned()),
            args
        );
        let key = id.to_string();
        let rx = self.expect_response(&key);
        self.write_line(&line).map_err(CallError::Failed)?;
        let response = loop {
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(response) => break response,
                Err(RecvTimeoutError::Timeout) => {
                    if turn.is_cancelled() {
                        lock(&self.pending).remove(&key);
                        return Err(CallError::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CallError::Failed(
                        "the MCP server closed the connection".into(),
                    ));
                }
            }
        };
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            return Ok((true, message));
        }
        let result = response.get("result").cloned().unwrap_or(Value::Null);
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut parts = Vec::new();
        for item in result
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    parts.push(
                        item.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
                Some("image") => parts.push(format!(
                    "[image {}, {} base64 bytes]",
                    item.get("mimeType").and_then(Value::as_str).unwrap_or("?"),
                    item.get("data").and_then(Value::as_str).map_or(0, str::len)
                )),
                other => parts.push(format!("[{} content]", other.unwrap_or("unknown"))),
            }
        }
        Ok((is_error, parts.join("\n")))
    }

    /// Closes stdin, gives the server [`MCP_EXIT_GRACE`] to exit, then kills it.
    fn close(&self) {
        drop(lock(&self.stdin).take());
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        let deadline = Instant::now() + MCP_EXIT_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_split_by_chars() {
        assert_eq!(split_pieces("abcdef", 3), ["ab", "cd", "ef"]);
        assert_eq!(split_pieces("ab", 3), ["a", "b"]);
        assert!(split_pieces("", 3).is_empty());
        assert_eq!(split_pieces("äöü€x", 3).concat(), "äöü€x");
        assert_eq!(split_pieces("abcdefg", 3).len(), 3);
    }

    #[test]
    fn keyword_selection_prefers_the_longest_match() {
        let s = Scenario::parse(
            r#"{"default": [{"say": "d"}], "fix": [{"say": "f"}], "fix all": [{"say": "a"}],
                "_description": "ignored", "_options": {"mcp": "skip"}}"#,
        )
        .unwrap();
        assert_eq!(s.select("Please FIX this").unwrap().0, "fix");
        assert_eq!(s.select("fix all of it").unwrap().0, "fix all");
        assert_eq!(s.select("hello").unwrap().0, "default");
        assert_eq!(s.options.mcp.as_deref(), Some("skip"));
    }

    #[test]
    fn invalid_scenarios_are_refused_with_the_step() {
        let bad = [
            (r#"{"default": [{"sya": "x"}]}"#, "unknown variant"),
            (r#"{"default": [{"stop": "done"}]}"#, "stop"),
            (
                r#"{"default": [{"tool": {"name": "edit", "args": 3}}]}"#,
                "object",
            ),
            (
                r#"{"default": [{"disk_write": {"path": "../x", "content": ""}}]}"#,
                "relative",
            ),
            (r#"{"default": [{"disk_write": {"path": "x"}}]}"#, "content"),
            (
                r#"{"default": [{"permission": {"title": "t", "expect": "yes"}}]}"#,
                "offered",
            ),
            (
                r#"{"default": [{"repeat": {"times": 2, "steps": [{"plan": [{"content": "c", "status": "done"}]}]}}]}"#,
                "status",
            ),
            (r#"{"_options": {"modes": "all"}, "default": []}"#, "modes"),
            (
                r#"{"_options": {"initial_mode": "auto"}, "default": []}"#,
                "initial_mode",
            ),
            (r#"{"_description": "x"}"#, "no turns"),
            (r#"[1]"#, "object"),
        ];
        for (text, want) in bad {
            let err = Scenario::parse(text).unwrap_err();
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn substitution_reaches_nested_strings() {
        let v = json!({"tool": {"args": {"old_string": "Line {{i}} of", "n": 3}}});
        assert_eq!(
            substitute(&v, 12),
            json!({"tool": {"args": {"old_string": "Line 12 of", "n": 3}}})
        );
    }

    #[test]
    fn fixture_scenarios_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/agent");
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                Scenario::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                count += 1;
            }
        }
        assert!(count >= 6, "expected the six M2 scenarios, found {count}");
    }

    /// The fake agent's messages must parse as the ACP schema types the real client uses.
    #[test]
    fn messages_match_the_acp_schema() {
        use agent_client_protocol_schema::v1 as s;
        let options = Options::default();
        let session = Session {
            id: "fake".into(),
            cwd: PathBuf::from("/"),
            mode: "default".into(),
            model: "default".into(),
            effort: "default".into(),
        };
        let new = json!({"sessionId": "fake", "modes": modes_json(&options, "default"),
                          "configOptions": config_options_json(&options, &session)});
        let parsed: s::NewSessionResponse = serde_json::from_value(new).unwrap();
        assert_eq!(parsed.config_options.unwrap().len(), 3);
        assert_eq!(parsed.modes.unwrap().available_modes.len(), 2);
        let perm = json!({"sessionId": "fake", "toolCall": {"toolCallId": "t", "title": "x", "kind": "execute", "status": "pending"},
                          "options": bash_options().iter().map(PermOption::to_json).collect::<Vec<_>>()});
        let parsed: s::RequestPermissionRequest = serde_json::from_value(perm).unwrap();
        assert_eq!(parsed.options.len(), 3);
        for update in [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "c", "priority": "medium", "status": "pending"}]}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "mcp__lemma__edit", "kind": "other", "status": "pending", "rawInput": {}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed",
                   "content": [{"type": "content", "content": {"type": "text", "text": "ok"}}]}),
            json!({"sessionUpdate": "usage_update", "used": 1, "size": 2}),
            json!({"sessionUpdate": "notice", "severity": "info", "title": "t"}),
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
            json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "d", "input": {"hint": "h"}}]}),
        ] {
            let n = json!({"sessionId": "fake", "update": update});
            serde_json::from_value::<s::SessionNotification>(n.clone())
                .unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }
}
