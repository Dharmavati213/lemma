//! Shared helpers of the ACP client tests (`tests/acp_*.rs`): the `lemma-fake-agent` and
//! `lemma-mcp` binaries, sandbox directories, event helpers and a tool-call answerer.
#![allow(dead_code)]

use lemma_agent::{
    AgentCmd, AgentConfig, AgentEvent, AgentHandle, EditorRequest, ToolCall, ToolContent, ToolReply,
};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The MCP socket's stream: a unix socket, or on Windows a loopback TCP connection to the port
/// that the socket file names (see `lemma_agent`'s `transport.rs`).
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Stream = std::net::TcpStream;

/// Connects to the MCP socket `socket`.
pub fn connect(socket: &Path) -> std::io::Result<Stream> {
    #[cfg(unix)]
    return Stream::connect(socket);
    #[cfg(windows)]
    {
        let address: std::net::SocketAddr = std::fs::read_to_string(socket)?
            .trim()
            .parse()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        Stream::connect(address)
    }
}

/// Binds a server at the MCP socket `socket` (on Windows: a loopback port named by the file).
#[cfg(unix)]
fn bind(socket: &Path) -> std::os::unix::net::UnixListener {
    std::os::unix::net::UnixListener::bind(socket).unwrap()
}

#[cfg(windows)]
fn bind(socket: &Path) -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(socket, listener.local_addr().unwrap().to_string()).unwrap();
    listener
}

/// Generous timeout for start-up and turns on a loaded machine.
pub const SLOW: Duration = Duration::from_secs(20);

pub struct Bins {
    pub fake_agent: PathBuf,
    pub mcp_bridge: PathBuf,
}

/// `lemma-fake-agent` and `lemma-mcp`: `$LEMMA_FAKE_AGENT` / `$LEMMA_MCP_BRIDGE` when set, else
/// built (once per test binary) into this test's own target directory and profile, so they are
/// never stale. Cargo does not build other packages' binaries for `cargo test -p lemma-agent`.
pub fn bins() -> &'static Bins {
    static BINS: OnceLock<Bins> = OnceLock::new();
    BINS.get_or_init(|| {
        let exe = std::env::current_exe().expect("current_exe");
        // <target>/<profile>/deps/<test>
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("test binary under <target>/<profile>/deps")
            .to_path_buf();
        let fake = std::env::var_os("LEMMA_FAKE_AGENT").map(PathBuf::from);
        let bridge = std::env::var_os("LEMMA_MCP_BRIDGE").map(PathBuf::from);
        if fake.is_none() || bridge.is_none() {
            let target_dir = profile_dir.parent().expect("target dir");
            let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            cmd.args([
                "build",
                "--quiet",
                "--offline",
                "-p",
                "lemma-fake-agent",
                "-p",
                "lemma-mcp",
            ])
            .arg("--target-dir")
            .arg(target_dir)
            .current_dir(env!("CARGO_MANIFEST_DIR"));
            match profile_dir.file_name().and_then(|n| n.to_str()) {
                Some("debug") => {}
                Some("release") => {
                    cmd.arg("--release");
                }
                Some(other) => {
                    cmd.args(["--profile", other]);
                }
                None => {}
            }
            let status = cmd.status().expect("run cargo build");
            assert!(
                status.success(),
                "building lemma-fake-agent and lemma-mcp failed"
            );
        }
        let bins = Bins {
            fake_agent: fake.unwrap_or_else(|| {
                profile_dir.join(format!("lemma-fake-agent{}", std::env::consts::EXE_SUFFIX))
            }),
            mcp_bridge: bridge.unwrap_or_else(|| {
                profile_dir.join(format!("lemma-mcp{}", std::env::consts::EXE_SUFFIX))
            }),
        };
        assert!(
            bins.fake_agent.is_file(),
            "{} missing",
            bins.fake_agent.display()
        );
        assert!(
            bins.mcp_bridge.is_file(),
            "{} missing",
            bins.mcp_bridge.display()
        );
        bins
    })
}

/// The repository's `tests/fixtures`.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// A private directory for one test, removed on drop. Its socket path stays under 108 bytes.
pub struct Sandbox {
    pub dir: PathBuf,
}

impl Sandbox {
    pub fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let leaf = format!("acp-{}-{n}-{name}", std::process::id());
        let mut base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        if base.join(&leaf).join("run/mcp.sock").as_os_str().len() >= 100 {
            base = std::env::temp_dir();
        }
        let dir = base.join(leaf);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).expect("create sandbox");
        let dir = lemma_core::canonicalize(&dir).expect("canonical sandbox");
        Self { dir }
    }

    /// The project root (`<sandbox>/proj`), a copy of `tests/fixtures/proj`'s sources.
    pub fn project(&self) -> PathBuf {
        self.dir.join("proj")
    }

    pub fn copy_fixture_project(&self) {
        let src = fixtures().join("proj");
        let dst = self.project();
        std::fs::create_dir_all(dst.join("chapters")).unwrap();
        for rel in ["main.tex", "refs.bib", "chapters/yoneda.tex"] {
            std::fs::copy(src.join(rel), dst.join(rel)).unwrap();
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("run/mcp.sock")
    }

    /// Writes a scenario file; returns its path.
    pub fn scenario(&self, json: &serde_json::Value) -> PathBuf {
        let path = self.dir.join("scenario.json");
        std::fs::write(&path, serde_json::to_vec_pretty(json).unwrap()).unwrap();
        path
    }

    pub fn config(&self, scenario: &Path) -> AgentConfig {
        let bins = bins();
        AgentConfig {
            program: bins.fake_agent.clone(),
            args: vec![scenario.as_os_str().to_owned()],
            env: vec![(
                "CLAUDE_CODE_EXECUTABLE".into(),
                "/nonexistent/claude".into(),
            )],
            cwd: self.project(),
            mcp_bridge: bins.mcp_bridge.clone(),
            mcp_socket: self.socket(),
            mcp_token: "0123456789abcdef0123456789abcdef".into(),
            system_prompt_append: "You are working inside Lemma (test).".into(),
            raw_log: None,
            flavor: lemma_agent::AcpFlavor::Claude,
            load_session: None,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The next event within `timeout`.
pub fn next_event(handle: &AgentHandle, timeout: Duration) -> Option<AgentEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        match handle.events.try_recv() {
            Ok(event) => return Some(event),
            Err(async_channel::TryRecvError::Closed) => return None,
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_micros(500));
    }
}

/// Events up to and including the first one matching `stop`; panics after `timeout`.
pub fn events_until(
    handle: &AgentHandle,
    timeout: Duration,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match next_event(handle, left) {
            Some(event) => {
                let done = stop(&event);
                seen.push(event);
                if done {
                    return seen;
                }
            }
            None => panic!(
                "no matching event within {timeout:?} (channel closed: {}); got {seen:#?}",
                handle.events.is_closed()
            ),
        }
    }
}

/// Every event until the channel closes (after `Exited`).
pub fn drain(handle: &AgentHandle, timeout: Duration) -> Vec<AgentEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match next_event(handle, left) {
            Some(event) => seen.push(event),
            None if handle.events.is_closed() && handle.events.is_empty() => return seen,
            None => panic!("the events channel did not close within {timeout:?}; got {seen:#?}"),
        }
    }
}

/// Waits for `Ready` (the first event) and returns its fields.
pub fn expect_ready(handle: &AgentHandle) -> AgentEvent {
    match next_event(handle, SLOW) {
        Some(ready @ AgentEvent::Ready { .. }) => ready,
        other => panic!("expected Ready first, got {other:?}"),
    }
}

pub fn send(handle: &AgentHandle, cmd: AgentCmd) {
    handle
        .cmd
        .send_blocking(cmd)
        .expect("agent command channel open");
}

pub fn prompt(handle: &AgentHandle, text: &str) {
    send(
        handle,
        AgentCmd::Prompt(vec![lemma_agent::PromptBlock::Text(text.into())]),
    );
}

pub fn is_turn_end(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::TurnEnded(_))
}

/// Shuts the agent down and returns the remaining events (the channel must close).
pub fn shutdown(handle: &AgentHandle) -> Vec<AgentEvent> {
    let _ = handle.cmd.send_blocking(AgentCmd::Shutdown);
    drain(handle, SLOW)
}

/// The text of the message chunks, concatenated.
pub fn message_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageChunk(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

pub fn errors(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// Answers tool calls on a thread like the editor would, recording each request.
pub struct ToolAnswerer {
    pub calls: Arc<Mutex<Vec<(u64, EditorRequest)>>>,
    thread: Option<JoinHandle<()>>,
}

impl ToolAnswerer {
    pub fn start(
        tools: async_channel::Receiver<ToolCall>,
        answer: impl Fn(&EditorRequest) -> ToolReply + Send + 'static,
    ) -> Self {
        let calls: Arc<Mutex<Vec<(u64, EditorRequest)>>> = Arc::default();
        let recorded = calls.clone();
        let thread = thread::spawn(move || {
            while let Ok(call) = tools.recv_blocking() {
                let reply = answer(&call.req);
                recorded.lock().unwrap().push((call.call, call.req));
                let _ = call.reply.send(reply);
            }
        });
        Self {
            calls,
            thread: Some(thread),
        }
    }

    /// Waits for the tools channel to close (the agent's MCP server stopped).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

pub fn text_reply(text: &str, is_error: bool) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(text.into())],
        is_error,
    }
}

/// A minimal stand-in for the editor's MCP socket server that records every line it receives
/// (the token line included) and answers the handshake, `tools/list` and `tools/call`.
pub struct RecordingMcpServer {
    pub lines: Arc<Mutex<Vec<String>>>,
    socket: PathBuf,
    accept: Option<JoinHandle<()>>,
}

pub const TOOL_NAMES: &[&str] = &[
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

impl RecordingMcpServer {
    pub fn start(socket: &Path) -> Self {
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = bind(socket);
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = lines.clone();
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let recorded = recorded.clone();
                // One connection at a time is all the bridge makes; probes close at once.
                if !serve_connection(stream, &recorded) {
                    break;
                }
            }
        });
        Self {
            lines,
            socket: socket.to_path_buf(),
            accept: Some(accept),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

impl Drop for RecordingMcpServer {
    fn drop(&mut self) {
        // A `{"stop":true}` line ends the accept loop.
        if let Ok(mut s) = connect(&self.socket) {
            let _ = s.write_all(b"{\"stop\":true}\n");
        }
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Serves one connection; false when asked to stop.
fn serve_connection(stream: Stream, recorded: &Mutex<Vec<String>>) -> bool {
    let mut out = stream.try_clone().unwrap();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line == "{\"stop\":true}" {
            return false;
        }
        recorded.lock().unwrap().push(line.clone());
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let result = match msg.get("method").and_then(|m| m.as_str()) {
            Some("server/discover") => serde_json::json!({}),
            Some("initialize") => serde_json::json!({
                "protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                "serverInfo": {"name": "lemma", "version": "test"}}),
            Some("tools/list") => serde_json::json!({"tools": TOOL_NAMES.iter().map(|n|
                serde_json::json!({"name": n, "description": n, "inputSchema": {"type": "object"}})).collect::<Vec<_>>()}),
            Some("tools/call") => serde_json::json!({
                "content": [{"type": "text", "text": "recorded"}], "isError": false}),
            _ => serde_json::json!({}),
        };
        let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
        if writeln!(out, "{reply}").is_err() {
            break;
        }
    }
    true
}

/// Number of threads of this process.
pub fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

/// `/proc/<pid>/stat` fields after the command name: state, ppid, pgrp.
pub fn proc_stat(pid: i32) -> Option<(char, i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgrp = f.next()?.parse().ok()?;
    Some((state, ppid, pgrp))
}

/// Pids of the processes whose parent is `parent`.
pub fn children_of(parent: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if proc_stat(pid).is_some_and(|(_, ppid, _)| ppid == parent) {
            out.push(pid);
        }
    }
    out
}

/// The command line of `pid`, NUL-separated arguments joined by spaces.
pub fn cmdline(pid: i32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// The fake agent started for `scenario` (a child of this process).
pub fn agent_pid(scenario: &Path) -> i32 {
    let me = std::process::id() as i32;
    let needle = scenario.to_string_lossy().into_owned();
    let deadline = Instant::now() + SLOW;
    loop {
        if let Some(pid) = children_of(me)
            .into_iter()
            .find(|p| cmdline(*p).contains(&needle))
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the fake agent process was not found"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `cond` until it holds or `timeout` passes.
pub fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}
