//! Hand-written MCP server (JSON-RPC 2.0, newline-delimited) behind a unix socket. Verified handshake with
//! Claude CLI 2.1.288: server/discover -> initialize(2025-11-25) -> notifications/initialized -> tools/list.
//!
//! # Shape
//!
//! The Claude CLI spawns `lemma-mcp` (std-only stdio <-> socket bridge) as the `lemma` stdio MCP
//! server. The bridge connects to [`McpServer::socket`], sends `{"lemma_token":"<token>"}` and
//! then relays JSON-RPC lines both ways. This module is the editor's end of that socket:
//!
//! - **`mcp-accept` thread**: a blocking [`UnixListener`]; one `mcp-conn-<n>` thread per
//!   connection (at most [`MAX_CONNECTIONS`]).
//! - **Connection thread**: checks the token line (constant-time compare; anything else closes
//!   the connection without a reply), then reads one JSON-RPC message per line. Everything but
//!   `tools/call` is answered by the pure [`handle_basic`]. A `tools/call` is parsed with
//!   [`parse_tool_call`]; a valid one is sent to the main thread as a [`ToolCall`] on the
//!   `tools` channel, **in arrival order**, and an `mcp-call-<id>` thread waits for its reply
//!   (so the connection keeps reading: `ping`, cancellations and parallel read-only calls are
//!   not blocked by a running `compile`).
//! - **Timeouts**: [`Timeouts`] (default [`TOOL_TIMEOUT`]; `compile` gets the build timeout plus
//!   [`COMPILE_GRACE`], and `render_page`, which may first wait for a running build, that plus
//!   [`RENDER_GRACE`]). On timeout the reply is `isError: true`, [`TIMEOUT_TEXT`].
//! - **Shapes** follow MCP 2025-11-25: unknown tools and malformed `tools/call` params are
//!   JSON-RPC errors (-32602); bad tool *arguments* are tool execution errors (`isError: true`)
//!   so the model can correct itself; a `notifications/cancelled` for a call in flight
//!   suppresses its reply. A message holding an unpaired UTF-16 surrogate escape (valid JSON
//!   that serde_json refuses) is still answered under its id: a `tools/call` gets an `isError`
//!   result, anything else is handled as if the escape were U+FFFD.
//! - **Stop**: [`McpServer::stop`] (also on drop) wakes the accept thread, closes every
//!   connection (each bridge then exits), joins all threads and removes the socket file if it is
//!   still ours. It returns within about [`POLL_SLICE`] even while tool calls are in flight.
//!
//! Every thread body is guarded with `catch_unwind`: a panic answers the message with an internal
//! error instead of dropping the connection (which would cost the CLI its lemma tools for the
//! rest of the session).

use super::{EditorRequest, ToolCall, ToolContent, ToolReply};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default per-call timeout; `compile` uses the build timeout + 20 s (`render_page` 10 s more).
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// The MCP versions this server speaks; `initialize` echoes a requested one from this list.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];
/// The version offered when the client asks for one we do not support (the newest we speak).
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// `serverInfo.name`; the CLI exposes the tools as `mcp__lemma__<tool>`.
pub const SERVER_NAME: &str = "lemma";
/// `serverInfo.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Added to the build timeout for the `compile` tool's timeout.
pub const COMPILE_GRACE: Duration = Duration::from_secs(20);
/// Added to the `compile` timeout for `render_page`, which may first wait for a running build
/// and then renders the page.
pub const RENDER_GRACE: Duration = Duration::from_secs(10);
/// The default build timeout (`build.timeout_s` = 180), used by [`Timeouts::default`].
pub const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(180);
/// The tool result text when the editor does not answer within the call's timeout.
pub const TIMEOUT_TEXT: &str = "editor did not answer in time";
/// The tool names, in the order of [`tool_definitions`].
pub const TOOL_NAMES: [&str; 10] = [
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
/// Most simultaneous connections; further ones are closed at once. The CLI opens one per agent
/// session, so this only bounds misbehaving clients.
pub const MAX_CONNECTIONS: usize = 64;
/// Most tool calls in flight on one connection; further ones get an `isError` reply.
pub const MAX_IN_FLIGHT: usize = 32;
/// Longest accepted JSON-RPC line (a `write` of a 5 MB file is about 10 MB once escaped). A longer
/// line is discarded and answered with -32600.
pub const MAX_MESSAGE_BYTES: usize = 32 << 20;
/// How long a new connection may take to send its token line.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a write to a client may block before the connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest accepted token line.
const MAX_TOKEN_LINE: usize = 4096;
/// Waiting threads re-check cancellation and stop this often; it bounds how long `stop` takes.
pub const POLL_SLICE: Duration = Duration::from_millis(50);
/// Line buffers above this size are shrunk after use, so one large `write` does not pin memory.
const KEEP_BUFFER: usize = 1 << 20;

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Per-call timeouts of an [`McpServer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Every tool but `compile` and `render_page` ([`TOOL_TIMEOUT`] by default).
    pub tool: Duration,
    /// `compile`: the build timeout plus [`COMPILE_GRACE`]. `render_page` gets this plus
    /// [`RENDER_GRACE`].
    pub compile: Duration,
}

impl Timeouts {
    /// [`TOOL_TIMEOUT`] for the tools, `build + COMPILE_GRACE` for `compile`.
    pub fn for_build_timeout(build: Duration) -> Self {
        Self {
            tool: TOOL_TIMEOUT,
            compile: build.saturating_add(COMPILE_GRACE),
        }
    }

    /// The timeout of one request. `render_page` first waits for a running build as long as
    /// `compile` may wait, then renders: it gets the `compile` timeout plus [`RENDER_GRACE`] (and
    /// never less than `tool`).
    pub fn for_request(&self, req: &EditorRequest) -> Duration {
        match req {
            EditorRequest::Compile => self.compile,
            EditorRequest::RenderPage { .. } => {
                self.compile.saturating_add(RENDER_GRACE).max(self.tool)
            }
            _ => self.tool,
        }
    }
}

impl Default for Timeouts {
    /// For the default build timeout ([`DEFAULT_BUILD_TIMEOUT`]): 30 s, 200 s for `compile` and
    /// 210 s for `render_page`.
    fn default() -> Self {
        Self::for_build_timeout(DEFAULT_BUILD_TIMEOUT)
    }
}

// ---------------------------------------------------------------------------------------------
// Tool table
// ---------------------------------------------------------------------------------------------

/// Appended to the description of every tool that changes nothing.
const READ_ONLY_NOTE: &str = "This tool changes nothing: files change only through edit and \
write, and the user reviews every change you make as red/green hunks.";

/// JSON Schemas for: read_file, edit, write, list_files, outline, compile, get_diagnostics, get_selection,
/// render_page, get_review_state. (Names become `mcp__lemma__<name>`.)
///
/// Returns the JSON array of MCP `Tool` objects (`name`, `title`, `description`, `inputSchema`,
/// `annotations`) that `tools/list` sends, in [`TOOL_NAMES`] order. The file tools mirror
/// Claude's built-in Read/Edit/Write (same argument names, similar wording), every description
/// says that the user reviews changes as red/green hunks, and the read-only tools carry
/// `readOnlyHint` so that the CLI also allows them in plan mode and may run them in parallel.
pub fn tool_definitions() -> Value {
    let path_rel = "absolute, or relative to the project root";
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    let no_args = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    json!([
        {
            "name": "read_file",
            "title": "Read file",
            "description": "Reads a file of the LaTeX project as it is in the editor right now: the \
    live buffer, including the user's unsaved typing and your pending changes. The user reviews every \
    change you make as red/green hunks and may have accepted, rejected or modified some since you last \
    looked. Use this instead of the built-in Read for project files.\n\n\
    Usage:\n\
    - file_path is an absolute path or a path relative to the project root. Paths outside the project \
    root are refused.\n\
    - By default it reads up to 2000 lines from the start of the file. For a long file you can pass \
    offset (the 1-based line to start from) and limit (the number of lines), but reading the whole \
    file is usually better.\n\
    - Lines longer than 2000 characters are truncated.\n\
    - Results use cat -n format: spaces, the line number and a tab, then the text of the line. The \
    prefix is not part of the file.\n\
    - Re-read a region before editing it again: the user may have changed it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": format!("The path of the file to read: {path_rel}")
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The line number to start reading from (1-based). Only \
    provide if the file is too large to read at once"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of lines to read (default 2000). Only provide \
    if the file is too large to read at once"
                    }
                },
                "required": ["file_path"],
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "edit",
            "title": "Edit file",
            "description": "Performs an exact string replacement in a file of the LaTeX project. \
    The change goes into the editor buffer and to disk at once, and the user reviews it as red/green \
    hunks that they accept or reject one by one.\n\n\
    Usage:\n\
    - Read the file with read_file first. When you copy text from read_file output, preserve the \
    exact indentation (tabs/spaces) as it appears AFTER the line number prefix (spaces + line number \
    + tab); never include any part of the prefix in old_string or new_string.\n\
    - The edit FAILS if old_string does not occur in the file (the user may have edited that region: \
    call read_file again) or occurs more than once. Add surrounding context to make it unique, or set \
    replace_all to change every occurrence, e.g. to rename a label or macro.\n\
    - Prefer small, focused edits over rewriting large regions: every change becomes a hunk the user \
    has to review.\n\
    - Tool results may carry a <review_feedback> block with the user's decisions on earlier hunks. Do \
    not re-apply a change the user rejected unless they ask for it.\n\
    - Paths outside the project root are refused.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": format!("The path of the file to modify: {path_rel}")
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to replace"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace it with (must be different from \
    old_string)"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace all occurrences of old_string (default false)"
                    }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": false }
        },
        {
            "name": "write",
            "title": "Write file",
            "description": "Writes a whole file of the LaTeX project, creating it and its \
    directories if needed. The new content goes into the editor buffer and to disk at once, and the \
    user reviews the difference from the previous content as red/green hunks (a new file is a single \
    hunk they can reject).\n\n\
    Usage:\n\
    - This overwrites an existing file. Read it with read_file first, and prefer edit for changes to \
    an existing file: it is cheaper and gives smaller hunks.\n\
    - Only create files the task needs.\n\
    - file_path is an absolute path or a path relative to the project root. Paths outside the project \
    root are refused.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": format!("The path of the file to write: {path_rel}")
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": false }
        },
        {
            "name": "list_files",
            "title": "List project files",
            "description": format!("Lists the files of the LaTeX project: first the files of \
    the document (the root document, then the files it includes), then the other files under the \
    project root, at most 500 in all. Build output, files TeX generates, .git and hidden directories \
    are left out. {READ_ONLY_NOTE}"),
            "inputSchema": no_args,
            "annotations": read_only
        },
        {
            "name": "outline",
            "title": "Document outline",
            "description": format!("Returns the structure of the document from the editor's \
    live text: sectioning commands, theorem-like environments, labels and included files, with \
    1-based line numbers. Without file_path it covers every file of the document, starting at the \
    root document. {READ_ONLY_NOTE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": format!("Only outline this file ({path_rel})")
                    }
                },
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "compile",
            "title": "Compile",
            "description": "Compiles the project with the editor's own latexmk build (the build \
    the user runs), waits until it finishes, and returns the status, up to 20 errors as \
    file:line: message, the number of warnings and the page count. Use this instead of running \
    latexmk or pdflatex in a shell, and get_diagnostics for the full list. The build writes only \
    into its output directory; the user reviews every change you make to project files as \
    red/green hunks.",
            "inputSchema": no_args,
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "get_diagnostics",
            "title": "Build diagnostics",
            "description": format!("Returns the errors and warnings of the last build as \
    file:line: message, for the whole project or only for file_path. Run compile first if you \
    changed files since the last build. {READ_ONLY_NOTE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": format!("Only report the diagnostics of this file \
    ({path_rel})")
                    }
                },
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "get_selection",
            "title": "Editor selection",
            "description": format!("Returns where the user is in the editor: the current file, \
    the cursor's line and column, and the selected text (at most 20000 characters). Use it when the \
    user says \"this\", \"here\" or \"the selection\". {READ_ONLY_NOTE}"),
            "inputSchema": no_args,
            "annotations": read_only
        },
        {
            "name": "render_page",
            "title": "Render PDF page",
            "description": format!("Renders one page of the compiled PDF as a PNG image (1.5x \
    scale, at most 1600 px tall) so you can check the typeset result: a formula, table, figure or \
    page layout. The reply also names a PNG copy of the image on disk. Compile first if the PDF is \
    out of date. {READ_ONLY_NOTE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The page number (1-based)"
                    }
                },
                "required": ["page"],
                "additionalProperties": false
            },
            "annotations": read_only
        },
        {
            "name": "get_review_state",
            "title": "Review state",
            "description": "Returns the changes still waiting for the user's review, as a \
    unified diff per file, plus the user's recent decisions (hunks accepted, rejected or edited). \
    The user reviews every change you make as red/green hunks; check this before redoing work, and \
    do not re-apply rejected changes unless asked. This tool changes nothing.",
            "inputSchema": no_args,
            "annotations": read_only
        }
    ])
}

/// The arguments each tool accepts, for [`parse_tool_call`]'s messages.
fn tool_arguments(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "read_file" => &["file_path", "offset", "limit"],
        "edit" => &["file_path", "old_string", "new_string", "replace_all"],
        "write" => &["file_path", "content"],
        "outline" | "get_diagnostics" => &["file_path"],
        "render_page" => &["page"],
        "list_files" | "compile" | "get_selection" | "get_review_state" => &[],
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC basics
// ---------------------------------------------------------------------------------------------

/// A JSON-RPC message, validated per JSON-RPC 2.0 and MCP (string or integer ids).
#[derive(Debug)]
enum Incoming<'a> {
    Request {
        id: &'a Value,
        method: &'a str,
        params: Option<&'a Map<String, Value>>,
    },
    Notification {
        method: &'a str,
        params: Option<&'a Map<String, Value>>,
    },
    /// A response to a request of ours. We send none, so these are ignored.
    Response,
}

fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// Classifies `msg`; `Err` is the error response to send (-32600 or -32602).
fn classify(msg: &Value) -> Result<Incoming<'_>, Value> {
    let Some(obj) = msg.as_object() else {
        let what = if msg.is_array() {
            "Invalid Request: batches are not supported"
        } else {
            "Invalid Request: not a JSON object"
        };
        return Err(error_response(&Value::Null, INVALID_REQUEST, what));
    };
    let id = obj.get("id");
    let valid_id = id.filter(|id| id.is_string() || id.is_i64() || id.is_u64());
    // Errors about a request echo its id when it is a valid one, else null.
    let invalid = |why: &str| {
        error_response(
            valid_id.unwrap_or(&Value::Null),
            INVALID_REQUEST,
            format!("Invalid Request: {why}"),
        )
    };
    let Some(method) = obj.get("method") else {
        if obj.contains_key("result") || obj.contains_key("error") {
            return Ok(Incoming::Response);
        }
        return Err(invalid("no method"));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("jsonrpc must be \"2.0\""));
    }
    let Some(method) = method.as_str() else {
        return Err(invalid("method must be a string"));
    };
    if id.is_some() && valid_id.is_none() {
        return Err(invalid("id must be a string or an integer"));
    }
    let params = match obj.get("params") {
        None => None,
        Some(Value::Object(p)) => Some(p),
        Some(_) => {
            return Err(error_response(
                valid_id.unwrap_or(&Value::Null),
                INVALID_PARAMS,
                "Invalid params: params must be an object",
            ));
        }
    };
    Ok(match valid_id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    })
}

/// Pure request handler for everything except tools/call:
/// server/discover -> {} (verified path), initialize -> echo protocolVersion if supported else "2025-11-25",
/// capabilities {"tools":{}}, serverInfo {"name":"lemma"}; tools/list; ping -> {}; notifications -> None;
/// unknown method -> error -32601.
///
/// `msg` is one parsed JSON-RPC message; the result is the whole response object (`jsonrpc`,
/// `id`, `result` or `error`). Responses to our own requests get `None` as well, and invalid
/// messages get -32600 (-32602 for non-object params). `tools/call` needs the editor, so
/// [`McpServer`] handles it before calling this; here it gets -32603.
pub fn handle_basic(msg: &Value) -> Option<Value> {
    let (id, method, params) = match classify(msg) {
        Err(response) => return Some(response),
        Ok(Incoming::Notification { .. } | Incoming::Response) => return None,
        Ok(Incoming::Request { id, method, params }) => (id, method, params),
    };
    Some(match method {
        "server/discover" | "ping" => success(id, json!({})),
        "initialize" => {
            match params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
            {
                Some(requested) => {
                    let version = PROTOCOL_VERSIONS
                        .iter()
                        .find(|v| **v == requested)
                        .copied()
                        .unwrap_or(LATEST_PROTOCOL_VERSION);
                    success(
                        id,
                        json!({
                            "protocolVersion": version,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": SERVER_NAME, "version": VERSION }
                        }),
                    )
                }
                None => error_response(
                    id,
                    INVALID_PARAMS,
                    "Invalid params: initialize needs a protocolVersion string",
                ),
            }
        }
        "tools/list" => success(id, json!({ "tools": tool_definitions() })),
        "tools/call" => error_response(
            id,
            INTERNAL_ERROR,
            "tools/call is handled by the editor's MCP server, not by handle_basic",
        ),
        other => error_response(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    })
}

// ---------------------------------------------------------------------------------------------
// tools/call arguments
// ---------------------------------------------------------------------------------------------

/// A short, single-line rendering of a JSON value for error messages.
fn preview(v: &Value) -> String {
    const MAX: usize = 60;
    let s = v.to_string();
    if s.len() <= MAX {
        return s;
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The argument object of one `tools/call`, with the tool name for messages.
struct Args<'a> {
    tool: &'a str,
    map: &'a Map<String, Value>,
}

impl Args<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        // An explicit null counts as absent: some clients send optional arguments as null.
        self.map.get(key).filter(|v| !v.is_null())
    }

    fn wrong_type(&self, key: &str, expected: &str, got: &Value) -> String {
        format!(
            "{}: argument \"{key}\" must be {expected}, got {}",
            self.tool,
            preview(got)
        )
    }

    fn string(&self, key: &str) -> Result<&str, String> {
        match self.get(key) {
            None => Err(format!(
                "{}: missing required argument \"{key}\"",
                self.tool
            )),
            Some(Value::String(s)) => Ok(s),
            Some(other) => Err(self.wrong_type(key, "a string", other)),
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf, String> {
        let s = self.string(key)?;
        self.check_path(key, s)
    }

    fn opt_path(&self, key: &str) -> Result<Option<PathBuf>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => self.check_path(key, s).map(Some),
            Some(other) => Err(self.wrong_type(key, "a string (a file path)", other)),
        }
    }

    fn check_path(&self, key: &str, s: &str) -> Result<PathBuf, String> {
        if s.trim().is_empty() {
            return Err(format!("{}: argument \"{key}\" is empty", self.tool));
        }
        if s.contains('\0') {
            return Err(format!(
                "{}: argument \"{key}\" contains a NUL character",
                self.tool
            ));
        }
        Ok(PathBuf::from(s))
    }

    /// A boolean; `"true"`/`"false"` strings are accepted too.
    fn opt_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("true") => Ok(Some(true)),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("false") => Ok(Some(false)),
            Some(other) => Err(self.wrong_type(key, "true or false", other)),
        }
    }

    /// A whole number in `min..=u32::MAX`. Integral floats (`10.0`) and decimal strings
    /// (`"10"`) are accepted, since models sometimes send those.
    fn opt_u32(&self, key: &str, min: u32, what: &str) -> Result<Option<u32>, String> {
        let Some(v) = self.get(key) else {
            return Ok(None);
        };
        let expected = if min == 0 {
            format!("a non-negative integer ({what})")
        } else {
            format!("an integer of at least {min} ({what})")
        };
        let n: Option<i128> = match v {
            Value::Number(n) => n.as_i64().map(i128::from).or_else(|| {
                n.as_u64().map(i128::from).or_else(|| {
                    n.as_f64()
                        .filter(|f| f.fract() == 0.0 && f.abs() < 1e18)
                        .map(|f| f as i128)
                })
            }),
            Value::String(s) => s.trim().parse::<i128>().ok(),
            _ => None,
        };
        match n {
            Some(n) if n >= i128::from(min) && n <= i128::from(u32::MAX) => Ok(Some(n as u32)),
            _ => Err(self.wrong_type(key, &expected, v)),
        }
    }

    fn u32(&self, key: &str, min: u32, what: &str) -> Result<u32, String> {
        self.opt_u32(key, min, what)?
            .ok_or_else(|| format!("{}: missing required argument \"{key}\"", self.tool))
    }
}

/// Parse tools/call params into an EditorRequest (paths NOT yet resolved; the main thread resolves them
/// through ProjectRoot and refuses anything outside the root).
///
/// `args` is the `arguments` object (`null` counts as `{}`). Every error names the tool and the
/// argument: an unknown tool, unexpected arguments (listing the accepted ones), a missing
/// required argument, or a value of the wrong type. Lenient where it is unambiguous: optional
/// arguments may be `null`, numbers may be integral floats or decimal strings, booleans may be
/// `"true"`/`"false"`, and `read_file`'s `offset` 0 means the first line.
pub fn parse_tool_call(name: &str, args: &Value) -> Result<EditorRequest, String> {
    let Some(accepted) = tool_arguments(name) else {
        return Err(format!(
            "unknown tool \"{name}\"; the tools are {}",
            TOOL_NAMES.join(", ")
        ));
    };
    let empty = Map::new();
    let map = match args {
        Value::Null => &empty,
        Value::Object(map) => map,
        other => {
            return Err(format!(
                "{name}: arguments must be a JSON object, got {}",
                preview(other)
            ));
        }
    };
    let mut unexpected: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !accepted.contains(k))
        .collect();
    unexpected.sort_unstable();
    if !unexpected.is_empty() {
        let list = unexpected
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let plural = if unexpected.len() == 1 { "" } else { "s" };
        return Err(if accepted.is_empty() {
            format!("{name} takes no arguments, got {list}")
        } else {
            format!(
                "{name}: unexpected argument{plural} {list}; {name} takes {}",
                accepted.join(", ")
            )
        });
    }
    let a = Args { tool: name, map };
    Ok(match name {
        "read_file" => EditorRequest::ReadFile {
            path: a.path("file_path")?,
            offset: a.opt_u32("offset", 0, "a 1-based line number")?,
            limit: a.opt_u32("limit", 1, "a number of lines")?,
        },
        "edit" => EditorRequest::Edit {
            path: a.path("file_path")?,
            old_string: a.string("old_string")?.to_owned(),
            new_string: a.string("new_string")?.to_owned(),
            replace_all: a.opt_bool("replace_all")?.unwrap_or(false),
        },
        "write" => EditorRequest::Write {
            path: a.path("file_path")?,
            content: a.string("content")?.to_owned(),
        },
        "list_files" => EditorRequest::ListFiles,
        "outline" => EditorRequest::Outline {
            path: a.opt_path("file_path")?,
        },
        "compile" => EditorRequest::Compile,
        "get_diagnostics" => EditorRequest::Diagnostics {
            path: a.opt_path("file_path")?,
        },
        "get_selection" => EditorRequest::Selection,
        "render_page" => EditorRequest::RenderPage {
            page: a.u32("page", 1, "a 1-based page number")?,
        },
        "get_review_state" => EditorRequest::ReviewState,
        _ => unreachable!("tool_arguments knows exactly the tools matched here"),
    })
}

/// {"content":[{"type":"text","text":..}|{"type":"image","data":base64,"mimeType":"image/png"}],"isError":..}
pub fn reply_to_json(r: &ToolReply) -> Value {
    let content: Vec<Value> = r
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text(text) => json!({ "type": "text", "text": text }),
            ToolContent::Png(bytes) => {
                json!({ "type": "image", "data": base64(bytes), "mimeType": "image/png" })
            }
        })
        .collect();
    json!({ "content": content, "isError": r.is_error })
}

/// Standard base64 (RFC 4648 §4: `A–Z a–z 0–9 + /`) with `=` padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |n: u32| char::from(ALPHABET[(n & 63) as usize]);
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let (triples, rest) = bytes.as_chunks::<3>();
    for &[a, b, c] in triples {
        let n = u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c);
        out.extend([sym(n >> 18), sym(n >> 12), sym(n >> 6), sym(n)]);
    }
    match *rest {
        [a] => {
            let n = u32::from(a) << 16;
            out.extend([sym(n >> 18), sym(n >> 12), '=', '=']);
        }
        [a, b] => {
            let n = u32::from(a) << 16 | u32::from(b) << 8;
            out.extend([sym(n >> 18), sym(n >> 12), sym(n >> 6), '=']);
        }
        _ => {}
    }
    out
}

fn error_reply(text: impl Into<String>) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(text.into())],
        is_error: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Socket plumbing
// ---------------------------------------------------------------------------------------------

/// `Origin::Agent { call }` ids, unique in the process. 0 is reserved for edits that bypass the
/// tools (the watcher), so this starts at 1.
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_call_id() -> u64 {
    NEXT_CALL.fetch_add(1, Ordering::Relaxed)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Compares in time that depends only on the lengths, not on where the inputs differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(std::hint::black_box(x ^ y));
    }
    std::hint::black_box(diff) == 0
}

/// Whether `line` is `{"lemma_token":"<token>"}` (any JSON spacing).
fn token_matches(line: &[u8], token: &[u8]) -> bool {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    match obj.get("lemma_token") {
        Some(Value::String(sent)) => constant_time_eq(sent.as_bytes(), token),
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    /// A line is in the buffer (without its `'\n'`; a final line without one counts too).
    Line,
    /// The line exceeded the limit; it was consumed and discarded.
    TooLong,
    Eof,
}

/// Reads one `'\n'`-terminated line into `out` (cleared first), keeping at most `max` bytes.
fn read_line_capped(r: &mut impl BufRead, out: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    out.clear();
    let mut too_long = false;
    loop {
        let buf = match r.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(if too_long {
                LineRead::TooLong
            } else if out.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        let (chunk, used, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], i + 1, true),
            None => (buf, buf.len(), false),
        };
        if !too_long {
            if out.len() + chunk.len() > max {
                too_long = true;
                out.clear();
            } else {
                out.extend_from_slice(chunk);
            }
        }
        r.consume(used);
        if done {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

/// `(st_dev, st_ino)` of a path, without following a final symlink.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Creates the socket's directory with mode 0700, or tightens an existing one of ours to 0700.
fn prepare_socket_dir(socket: &Path) -> io::Result<()> {
    let dir = match socket.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "MCP socket {} must name its (private) directory",
                    socket.display()
                ),
            ));
        }
    };
    match fs::metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            // The umask may have cleared bits but never adds any; set the mode exactly anyway.
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        }
        Err(e) => Err(e),
        Ok(meta) if !meta.is_dir() => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        )),
        Ok(meta) => {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let euid = unsafe { libc::geteuid() };
            if meta.uid() != euid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "the MCP socket directory {} belongs to another user",
                        dir.display()
                    ),
                ));
            }
            if meta.mode() & 0o777 != 0o700 {
                tracing::info!("mcp: setting {} to mode 0700", dir.display());
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
    }
}

/// Binds `socket`, replacing a stale socket file (nobody accepts on it) but never another file
/// or a live server's socket.
fn bind_socket(socket: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(socket) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            let meta = fs::symlink_metadata(socket)?;
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", socket.display()),
                ));
            }
            if UnixStream::connect(socket).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a server is already listening on {}", socket.display()),
                ));
            }
            fs::remove_file(socket)?;
            UnixListener::bind(socket)
        }
        other => other,
    }
}

/// State shared by the server handle, the accept thread and the connection threads.
struct Shared {
    token: Box<[u8]>,
    /// [`HANDSHAKE_TIMEOUT`] (shorter in unit tests).
    handshake_timeout: Duration,
    tools: async_channel::Sender<ToolCall>,
    timeouts: Mutex<Timeouts>,
    stopping: AtomicBool,
    /// A handle on each live connection, so that `stop` can shut it down.
    conns: Mutex<HashMap<u64, UnixStream>>,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn close_all_connections(&self) {
        for stream in lock(&self.conns).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

pub struct McpServer {
    pub socket: PathBuf,
    shared: Arc<Shared>,
    /// Kept open (and its fd valid) until the accept thread has been joined.
    listener: Mutex<Option<Arc<UnixListener>>>,
    accept: Mutex<Option<JoinHandle<()>>>,
    /// `(st_dev, st_ino)` of the socket file we bound, so we never remove someone else's.
    socket_id: Option<(u64, u64)>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("socket", &self.socket)
            .field("stopping", &self.shared.stopping())
            .finish_non_exhaustive()
    }
}

impl McpServer {
    /// Binds the socket (parent dir created with mode 0700), spawns the accept thread. The first line from a client
    /// must be `{"lemma_token":"<token>"}`; otherwise the connection is closed without a reply.
    ///
    /// Uses [`Timeouts::default`]; see [`McpServer::start_with`].
    pub fn start(
        socket: &Path,
        token: String,
        tools: async_channel::Sender<ToolCall>,
    ) -> std::io::Result<McpServer> {
        Self::start_with(socket, token, tools, Timeouts::default())
    }

    /// Like [`McpServer::start`], with explicit per-call timeouts (e.g.
    /// `Timeouts::for_build_timeout(build.timeout_s)`).
    ///
    /// - `token` must not be empty.
    /// - The socket's directory is created with mode 0700; an existing one that belongs to the
    ///   user is set to 0700, one that belongs to someone else is refused. The socket itself gets
    ///   mode 0600. The path must be shorter than 108 bytes (`sun_path`).
    /// - A stale socket file at `socket` (nobody listening) is replaced; a live one gives
    ///   `AddrInUse`, any other file `AlreadyExists`.
    pub fn start_with(
        socket: &Path,
        token: String,
        tools: async_channel::Sender<ToolCall>,
        timeouts: Timeouts,
    ) -> std::io::Result<McpServer> {
        if token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the MCP token must not be empty",
            ));
        }
        prepare_socket_dir(socket)?;
        let listener = Arc::new(bind_socket(socket)?);
        let socket_id = file_id(socket);
        let unbind = |e: io::Error| {
            if file_id(socket) == socket_id {
                let _ = fs::remove_file(socket);
            }
            e
        };
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).map_err(unbind)?;
        let shared = Arc::new(Shared {
            token: token.into_bytes().into_boxed_slice(),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            tools,
            timeouts: Mutex::new(timeouts),
            stopping: AtomicBool::new(false),
            conns: Mutex::new(HashMap::new()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            let listener = Arc::clone(&listener);
            thread::Builder::new()
                .name("mcp-accept".into())
                .spawn(move || accept_loop(&shared, &listener))
                .map_err(unbind)?
        };
        tracing::debug!("mcp: listening on {}", socket.display());
        Ok(McpServer {
            socket: socket.to_path_buf(),
            shared,
            listener: Mutex::new(Some(listener)),
            accept: Mutex::new(Some(accept)),
            socket_id,
        })
    }

    /// The current per-call timeouts.
    pub fn timeouts(&self) -> Timeouts {
        *lock(&self.shared.timeouts)
    }

    /// Changes the timeouts of calls that start from now on (e.g. after `build.timeout_s`
    /// changed in a reloaded config).
    pub fn set_timeouts(&self, timeouts: Timeouts) {
        *lock(&self.shared.timeouts) = timeouts;
    }

    /// Whether [`McpServer::stop`] has been called.
    pub fn is_stopped(&self) -> bool {
        self.shared.stopping()
    }

    /// Stops accepting, closes every connection (bridges exit when their socket closes), joins
    /// all server threads and removes the socket file if it is still the one we bound. Calls in
    /// flight get no reply; their late editor replies are dropped. Idempotent; also run on drop.
    /// Returns within about [`POLL_SLICE`].
    pub fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let listener = lock(&self.listener).clone();
        if let Some(listener) = &listener {
            // Wakes the blocked accept() (Linux: it fails with EINVAL) and refuses new clients.
            // SAFETY: the fd belongs to `listener`, which this Arc keeps open.
            unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) };
        }
        self.shared.close_all_connections();
        let accept = lock(&self.accept).take();
        if let Some(accept) = accept {
            let deadline = Instant::now() + Duration::from_millis(500);
            while !accept.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if !accept.is_finished() && file_id(&self.socket) == self.socket_id {
                // Fallback for kernels where shutdown() does not wake accept().
                let _ = UnixStream::connect(&self.socket);
            }
            if accept.join().is_err() {
                tracing::error!("mcp: the accept thread panicked");
            }
        }
        if self.socket_id.is_some() && file_id(&self.socket) == self.socket_id {
            let _ = fs::remove_file(&self.socket);
        }
        lock(&self.listener).take();
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &UnixListener) {
    let mut next_conn = 0u64;
    let mut threads: Vec<JoinHandle<()>> = Vec::new();
    let mut consecutive_errors = 0u32;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) if shared.stopping() => break,
            Err(e) => {
                match e.kind() {
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => continue,
                    io::ErrorKind::InvalidInput => {
                        tracing::error!("mcp: the listening socket was shut down: {e}");
                        break;
                    }
                    _ => {}
                }
                consecutive_errors += 1;
                tracing::warn!("mcp: accept failed: {e}");
                if consecutive_errors > 100 {
                    tracing::error!("mcp: giving up accepting connections");
                    break;
                }
                // E.g. out of file descriptors: back off instead of spinning.
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        consecutive_errors = 0;
        if shared.stopping() {
            break;
        }
        threads.retain(|t| !t.is_finished());
        if threads.len() >= MAX_CONNECTIONS {
            tracing::warn!("mcp: too many connections; closing a new one");
            continue;
        }
        let Ok(handle) = stream.try_clone() else {
            continue;
        };
        let id = next_conn;
        next_conn += 1;
        lock(&shared.conns).insert(id, handle);
        // `stop` sets the flag before it shuts the registered connections down, so either it
        // saw this one or we see the flag here.
        if shared.stopping() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let conn_shared = Arc::clone(shared);
        let spawned = thread::Builder::new()
            .name(format!("mcp-conn-{id}"))
            .spawn(move || {
                let result =
                    catch_unwind(AssertUnwindSafe(|| serve_connection(&conn_shared, stream)));
                if result.is_err() {
                    tracing::error!("mcp: connection {id} panicked");
                }
                lock(&conn_shared.conns).remove(&id);
            });
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => {
                tracing::warn!("mcp: cannot start a connection thread: {e}");
                if let Some(s) = lock(&shared.conns).remove(&id) {
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
        }
    }
    if shared.stopping() {
        shared.close_all_connections();
    }
    for t in threads {
        let _ = t.join();
    }
}

/// The write half of a connection; replies come from the connection thread and the call threads.
struct Conn {
    out: Mutex<UnixStream>,
    broken: AtomicBool,
    /// Calls waiting for the editor, by JSON id, with their cancellation flag.
    in_flight: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Conn {
    fn send(&self, msg: &Value) {
        if self.broken.load(Ordering::SeqCst) {
            return;
        }
        let mut line = match serde_json::to_vec(msg) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("mcp: cannot serialize a reply: {e}");
                return;
            }
        };
        line.push(b'\n');
        let mut out = lock(&self.out);
        if let Err(e) = out.write_all(&line).and_then(|()| out.flush()) {
            tracing::debug!("mcp: client gone: {e}");
            self.broken.store(true, Ordering::SeqCst);
            let _ = out.shutdown(Shutdown::Both);
        }
    }
}

/// The read half of a connection. While `deadline` is set (the handshake), every read waits at
/// most until then, so the deadline bounds the whole token line, not each read(2): a client that
/// sends it a byte at a time cannot hold the connection open.
struct ConnReader {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl io::Read for ConnReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the handshake took too long",
                ));
            }
            self.stream.set_read_timeout(Some(left))?;
        }
        io::Read::read(&mut self.stream, buf)
    }
}

fn serve_connection(shared: &Arc<Shared>, stream: UnixStream) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::with_capacity(
        64 << 10,
        ConnReader {
            stream: read_half,
            deadline: Some(Instant::now() + shared.handshake_timeout),
        },
    );
    let mut line = Vec::new();
    let first = read_line_capped(&mut reader, &mut line, MAX_TOKEN_LINE);
    reader.get_mut().deadline = None;
    let authorized = matches!(first, Ok(LineRead::Line)) && token_matches(&line, &shared.token);
    if !authorized {
        // Closed without a reply: dropping the stream is all the client learns.
        if shared.stopping() {
            return;
        }
        if matches!(first, Ok(LineRead::Eof)) {
            // A probe that connects and closes (lemma_agent::spawn checks whether the socket
            // is served already).
            tracing::debug!("mcp: a connection closed before sending anything");
        } else {
            tracing::warn!("mcp: refused a connection without the right token");
        }
        return;
    }
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    tracing::debug!("mcp: client connected");
    let conn = Arc::new(Conn {
        out: Mutex::new(stream),
        broken: AtomicBool::new(false),
        in_flight: Mutex::new(HashMap::new()),
    });
    let mut calls: Vec<JoinHandle<()>> = Vec::new();
    loop {
        if line.capacity() > KEEP_BUFFER {
            line = Vec::new();
        }
        match read_line_capped(&mut reader, &mut line, MAX_MESSAGE_BYTES) {
            Ok(LineRead::Line) => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    dispatch(shared, &conn, &mut calls, &line);
                }));
                if result.is_err() {
                    conn.send(&error_response(
                        &Value::Null,
                        INTERNAL_ERROR,
                        "internal error in the editor's MCP server",
                    ));
                }
            }
            Ok(LineRead::TooLong) => conn.send(&error_response(
                &Value::Null,
                INVALID_REQUEST,
                format!("Invalid Request: message longer than {MAX_MESSAGE_BYTES} bytes"),
            )),
            Ok(LineRead::Eof) | Err(_) => break,
        }
        if shared.stopping() || conn.broken.load(Ordering::SeqCst) {
            break;
        }
    }
    // After the client's EOF (the bridge half-closes when the CLI is done), replies still in
    // flight are delivered: the bridge forwards them for a grace period.
    for call in calls {
        let _ = call.join();
    }
    tracing::debug!("mcp: client disconnected");
}

/// A copy of `line` in which every `\u` escape of an unpaired UTF-16 surrogate (a high one not
/// followed by a low one, or a low one on its own) is `\ufffd`; `None` when there is none. Such
/// escapes are valid JSON by its grammar (RFC 8259 §8.2), and JavaScript's `JSON.stringify` writes
/// them for a string that holds half a character, but serde_json refuses them.
fn replace_lone_surrogates(line: &[u8]) -> Option<Vec<u8>> {
    // The UTF-16 code unit of a `\uXXXX` escape at `at`.
    let unit = |at: usize| -> Option<u16> {
        let escape = line.get(at..at + 6)?;
        let digits = escape.strip_prefix(br"\u")?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    };
    let mut out: Option<Vec<u8>> = None;
    // `line[..copied]` is in `out` already.
    let mut copied = 0;
    let mut i = 0;
    while i < line.len() {
        if line[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(code) = unit(i) else {
            // Another escape (`\\` included): skip both characters.
            i += 2;
            continue;
        };
        let lone = match code {
            0xD800..=0xDBFF => !matches!(unit(i + 6), Some(0xDC00..=0xDFFF)),
            0xDC00..=0xDFFF => true,
            _ => false,
        };
        if lone {
            let out = out.get_or_insert_with(|| Vec::with_capacity(line.len()));
            out.extend_from_slice(&line[copied..i]);
            out.extend_from_slice(br"\ufffd");
            copied = i + 6;
            i += 6;
        } else if (0xD800..=0xDBFF).contains(&code) {
            // A whole pair.
            i += 12;
        } else {
            i += 6;
        }
    }
    let mut out = out?;
    out.extend_from_slice(&line[copied..]);
    Some(out)
}

fn dispatch(shared: &Arc<Shared>, conn: &Arc<Conn>, calls: &mut Vec<JoinHandle<()>>, line: &[u8]) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let (msg, repaired): (Value, bool) = match serde_json::from_slice(line) {
        Ok(msg) => (msg, false),
        Err(e) => {
            match replace_lone_surrogates(line).and_then(|l| serde_json::from_slice(&l).ok()) {
                Some(msg) => (msg, true),
                None => {
                    conn.send(&error_response(
                        &Value::Null,
                        PARSE_ERROR,
                        format!("Parse error: {e}"),
                    ));
                    return;
                }
            }
        }
    };
    match classify(&msg) {
        Ok(Incoming::Request {
            id,
            method: "tools/call",
            params,
        }) if repaired => {
            // Refused rather than run with U+FFFD in place of the half character (it would end
            // up in a file, or make an `old_string` miss): the model sends the text again.
            let tool = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tools/call");
            conn.send(&success(
                id,
                reply_to_json(&error_reply(format!(
                    "{tool}: the arguments contain an unpaired UTF-16 surrogate escape (half of \
                     a character such as an emoji), which is not valid Unicode text; send them \
                     again with whole characters"
                ))),
            ));
        }
        Ok(Incoming::Request {
            id,
            method: "tools/call",
            params,
        }) => start_tool_call(shared, conn, calls, id, params),
        Ok(Incoming::Notification {
            method: "notifications/cancelled",
            params,
        }) => {
            if let Some(request) = params.and_then(|p| p.get("requestId"))
                && let Some(flag) = lock(&conn.in_flight).get(&request.to_string())
            {
                tracing::debug!("mcp: call {request} cancelled by the client");
                flag.store(true, Ordering::SeqCst);
            }
        }
        _ => {
            if let Some(reply) = handle_basic(&msg) {
                conn.send(&reply);
            }
        }
    }
}

fn start_tool_call(
    shared: &Arc<Shared>,
    conn: &Arc<Conn>,
    calls: &mut Vec<JoinHandle<()>>,
    id: &Value,
    params: Option<&Map<String, Value>>,
) {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        conn.send(&error_response(
            id,
            INVALID_PARAMS,
            "Invalid params: tools/call needs a tool name string",
        ));
        return;
    };
    if !TOOL_NAMES.contains(&name) {
        conn.send(&error_response(
            id,
            INVALID_PARAMS,
            format!("Unknown tool: {name}"),
        ));
        return;
    }
    let args = params
        .and_then(|p| p.get("arguments"))
        .unwrap_or(&Value::Null);
    let req = match parse_tool_call(name, args) {
        Ok(req) => req,
        Err(msg) => {
            conn.send(&success(id, reply_to_json(&error_reply(msg))));
            return;
        }
    };
    calls.retain(|c| !c.is_finished());
    if lock(&conn.in_flight).len() >= MAX_IN_FLIGHT {
        conn.send(&success(
            id,
            reply_to_json(&error_reply(
                "too many tool calls at once; wait for the running ones to finish",
            )),
        ));
        return;
    }
    let timeout = lock(&shared.timeouts).for_request(&req);
    let deadline = Instant::now() + timeout;
    let call = next_call_id();
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    tracing::debug!("mcp: call {call}: {name}");
    // Sent from this thread, so the editor sees calls in the order the client made them.
    let mut pending = ToolCall {
        call,
        req,
        reply: reply_tx,
    };
    loop {
        match shared.tools.try_send(pending) {
            Ok(()) => break,
            Err(async_channel::TrySendError::Closed(_)) => {
                conn.send(&success(
                    id,
                    reply_to_json(&error_reply(
                        "the editor is not accepting tool calls (the agent is stopping)",
                    )),
                ));
                return;
            }
            Err(async_channel::TrySendError::Full(back)) => {
                if shared.stopping() {
                    return;
                }
                if Instant::now() >= deadline {
                    conn.send(&success(id, reply_to_json(&error_reply(TIMEOUT_TEXT))));
                    return;
                }
                pending = back;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let key = id.to_string();
    let cancelled = Arc::new(AtomicBool::new(false));
    lock(&conn.in_flight).insert(key.clone(), Arc::clone(&cancelled));
    let waiter = {
        let shared = Arc::clone(shared);
        let conn = Arc::clone(conn);
        let id = id.clone();
        move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                wait_for_reply(&reply_rx, deadline, &cancelled, &shared.stopping)
            }));
            {
                let mut in_flight = lock(&conn.in_flight);
                if in_flight
                    .get(&key)
                    .is_some_and(|flag| Arc::ptr_eq(flag, &cancelled))
                {
                    in_flight.remove(&key);
                }
            }
            let reply = match outcome {
                Ok(Waited::Reply(reply)) => reply,
                Ok(Waited::TimedOut) => {
                    tracing::warn!("mcp: call {call} timed out");
                    error_reply(TIMEOUT_TEXT)
                }
                Ok(Waited::Dropped) => {
                    error_reply("the editor dropped the request without answering")
                }
                Ok(Waited::Abandoned) => return,
                Err(_) => error_reply("internal error in the editor's MCP server"),
            };
            conn.send(&success(&id, reply_to_json(&reply)));
        }
    };
    match thread::Builder::new()
        .name(format!("mcp-call-{call}"))
        .spawn(waiter)
    {
        Ok(handle) => calls.push(handle),
        Err(e) => {
            // The closure (and with it the receiver) is gone: the editor's reply will be dropped.
            tracing::error!("mcp: cannot start a thread for call {call}: {e}");
            lock(&conn.in_flight).remove(&id.to_string());
            conn.send(&success(
                id,
                reply_to_json(&error_reply(format!(
                    "the editor's MCP server is out of threads: {e}"
                ))),
            ));
        }
    }
}

enum Waited {
    Reply(ToolReply),
    TimedOut,
    /// The editor dropped the reply sender without answering.
    Dropped,
    /// Cancelled by the client, or the server is stopping: send nothing.
    Abandoned,
}

fn wait_for_reply(
    rx: &Receiver<ToolReply>,
    deadline: Instant,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Waited {
    loop {
        let now = Instant::now();
        if now >= deadline {
            // A reply that raced with the deadline still counts.
            return rx.try_recv().map_or(Waited::TimedOut, Waited::Reply);
        }
        match rx.recv_timeout((deadline - now).min(POLL_SLICE)) {
            Ok(reply) => return Waited::Reply(reply),
            Err(RecvTimeoutError::Disconnected) => return Waited::Dropped,
            Err(RecvTimeoutError::Timeout) => {
                if cancelled.load(Ordering::SeqCst) || stopping.load(Ordering::SeqCst) {
                    return Waited::Abandoned;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lines(input: &[u8], max: usize, cap: usize) -> Vec<(LineRead, Vec<u8>)> {
        let mut r = BufReader::with_capacity(cap, Cursor::new(input.to_vec()));
        let mut out = Vec::new();
        let mut got = Vec::new();
        loop {
            let kind = read_line_capped(&mut r, &mut out, max).unwrap();
            if kind == LineRead::Eof {
                return got;
            }
            got.push((kind, out.clone()));
        }
    }

    #[test]
    fn lines_split_on_newlines_across_buffer_refills() {
        let got = lines(b"ab\ncdefgh\n\nlast", 100, 2);
        assert_eq!(
            got,
            [
                (LineRead::Line, b"ab".to_vec()),
                (LineRead::Line, b"cdefgh".to_vec()),
                (LineRead::Line, b"".to_vec()),
                (LineRead::Line, b"last".to_vec()),
            ]
        );
    }

    #[test]
    fn overlong_lines_are_discarded_whole() {
        let got = lines(b"12345\n123456\nok\n1234567", 6, 3);
        assert_eq!(
            got,
            [
                (LineRead::Line, b"12345".to_vec()),
                (LineRead::Line, b"123456".to_vec()),
                (LineRead::Line, b"ok".to_vec()),
                (LineRead::TooLong, Vec::new()),
            ]
        );
        let got = lines(b"1234567\nok\n", 6, 64);
        assert_eq!(
            got,
            [
                (LineRead::TooLong, Vec::new()),
                (LineRead::Line, b"ok".to_vec())
            ]
        );
    }

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        // A NUL byte must not be confused with "past the end".
        assert!(!constant_time_eq(b"ab\0", b"ab"));
    }

    #[test]
    fn token_line_must_carry_the_exact_token() {
        let token = b"0123456789abcdef0123456789abcdef";
        assert!(token_matches(
            br#"{"lemma_token":"0123456789abcdef0123456789abcdef"}"#,
            token
        ));
        assert!(token_matches(
            br#" { "lemma_token" : "0123456789abcdef0123456789abcdef" } "#,
            token
        ));
        for bad in [
            &br#"{"lemma_token":"0123456789abcdef0123456789abcdeF"}"#[..],
            br#"{"lemma_token":"0123456789abcdef0123456789abcde"}"#,
            br#"{"lemma_token":""}"#,
            br#"{"lemma_token":12}"#,
            br#"{"token":"0123456789abcdef0123456789abcdef"}"#,
            br#"["0123456789abcdef0123456789abcdef"]"#,
            br#""0123456789abcdef0123456789abcdef""#,
            b"0123456789abcdef0123456789abcdef",
            b"",
            br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        ] {
            assert!(
                !token_matches(bad, token),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn lone_surrogate_escapes_are_replaced() {
        let fix =
            |s: &str| replace_lone_surrogates(s.as_bytes()).map(|v| String::from_utf8(v).unwrap());
        assert_eq!(
            fix(r#"{"a":"x \ud83d y"}"#).as_deref(),
            Some(r#"{"a":"x \ufffd y"}"#)
        );
        assert_eq!(fix(r#""\uDE00""#).as_deref(), Some(r#""\ufffd""#));
        assert_eq!(fix(r#""\ud83d""#).as_deref(), Some(r#""\ufffd""#));
        // A high surrogate followed by another high one: the first is lone, the pair stays.
        assert_eq!(
            fix(r#""\ud83d\ud83d\ude00""#).as_deref(),
            Some(r#""\ufffd\ud83d\ude00""#)
        );
        // Nothing to repair: plain text, whole pairs, other escapes, and an escaped backslash
        // followed by "ud83d" (literal text, not an escape).
        for ok in [
            r#"{"a":"plain"}"#,
            r#""\ud83d\ude00 \u00e9 \n \" \\""#,
            r#""\\ud83d""#,
            r#""\u12""#,
            "\\",
        ] {
            assert_eq!(fix(ok), None, "{ok}");
        }
        // The repaired line parses.
        let fixed = fix(r#"{"id":7,"s":"\udc00\ud800"}"#).unwrap();
        let v: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v, json!({"id": 7, "s": "\u{fffd}\u{fffd}"}));
    }

    #[test]
    fn call_ids_are_unique_and_never_zero() {
        let a = next_call_id();
        let b = next_call_id();
        assert!(a > 0 && b > a);
    }

    fn test_shared(
        handshake_timeout: Duration,
    ) -> (Arc<Shared>, async_channel::Receiver<ToolCall>) {
        let (tx, rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            token: b"secret-token".to_vec().into_boxed_slice(),
            handshake_timeout,
            tools: tx,
            timeouts: Mutex::new(Timeouts::default()),
            stopping: AtomicBool::new(false),
            conns: Mutex::new(HashMap::new()),
        });
        (shared, rx)
    }

    #[test]
    fn a_client_that_never_sends_its_token_is_dropped_after_the_handshake_timeout() {
        use std::io::Read;
        let (shared, _rx) = test_shared(Duration::from_millis(150));
        let (server_end, mut client) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let t = thread::spawn(move || serve_connection(&shared, server_end));
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        // Half a token line, then silence.
        client.write_all(b"{\"lemma_token\":\"secret").unwrap();
        let mut buf = [0u8; 8];
        let n = client.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "closed without a reply");
        t.join().unwrap();
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(150), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    #[test]
    fn a_connection_answers_after_the_token_line() {
        use std::io::BufRead;
        let (shared, _rx) = test_shared(Duration::from_secs(5));
        let (server_end, client) = UnixStream::pair().unwrap();
        let t = thread::spawn(move || serve_connection(&shared, server_end));
        let mut writer = client.try_clone().unwrap();
        // Token and first request in one write: the request must not be lost in the buffer.
        writer
            .write_all(b"{\"lemma_token\":\"secret-token\"}\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        writer.shutdown(Shutdown::Write).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn classify_recognises_message_kinds() {
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","id":"x","method":"m"})),
            Ok(Incoming::Request { method: "m", .. })
        ));
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","method":"n","params":{}})),
            Ok(Incoming::Notification {
                method: "n",
                params: Some(_)
            })
        ));
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","id":3,"result":{}})),
            Ok(Incoming::Response)
        ));
        assert!(classify(&json!({"jsonrpc":"2.0","id":1.5,"method":"m"})).is_err());
        assert!(classify(&json!({"jsonrpc":"1.0","id":1,"method":"m"})).is_err());
        assert!(classify(&json!({"jsonrpc":"2.0","id":1,"method":"m","params":[1]})).is_err());
    }
}
