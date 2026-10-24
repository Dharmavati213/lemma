//! Helpers shared by the `mcp_*` integration tests: private socket directories, the real
//! `lemma-mcp` bridge binary, a raw socket client, and a fake editor thread that answers
//! `ToolCall`s.
#![allow(dead_code)]

use lemma_agent::mcp::{McpServer, Timeouts};
use lemma_agent::{ToolCall, ToolContent, ToolReply};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
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

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";
pub const SOCKET_NAME: &str = "mcp.sock";
/// Generous upper bound for anything that should happen "at once".
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// `sun_path` is 108 bytes including the terminating NUL; keep a margin.
const MAX_SOCKET_PATH: usize = 100;

/// A fresh directory for one test socket, removed on drop. Not created yet: `McpServer::start`
/// creates it with mode 0700 (call [`SocketDir::create`] to make it beforehand).
pub struct SocketDir(pub PathBuf);

impl SocketDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "lemma-mcp-srv-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let candidates = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([std::env::temp_dir(), PathBuf::from("/tmp")]);
        for base in candidates {
            let dir = base.join(&name);
            // Room for one nested level ("sub/mcp.sock") in the permission tests.
            if dir.join("sub").join(SOCKET_NAME).as_os_str().len() >= MAX_SOCKET_PATH {
                continue;
            }
            if base.is_dir() {
                return Self(dir);
            }
        }
        panic!("no directory gives a socket path shorter than {MAX_SOCKET_PATH} bytes");
    }

    pub fn create(&self, mode: u32) {
        #[allow(unused_mut)] // only Unix sets a mode
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(mode);
        builder
            .create(&self.0)
            .expect("create the socket directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    pub fn socket(&self) -> PathBuf {
        self.0.join(SOCKET_NAME)
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Starts a server in a fresh directory; the receiver gets its tool calls.
pub fn start_server(
    timeouts: Timeouts,
) -> (SocketDir, McpServer, async_channel::Receiver<ToolCall>) {
    let dir = SocketDir::new();
    let (tx, rx) = async_channel::unbounded();
    let server =
        McpServer::start_with(&dir.socket(), TOKEN.to_owned(), tx, timeouts).expect("start");
    (dir, server, rx)
}

pub fn text_reply(text: impl Into<String>) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(text.into())],
        is_error: false,
    }
}

/// A fake editor: answers every tool call with `answer` on its own thread until the channel
/// closes. Returns the calls it saw (their `call` ids and requests), in order.
pub fn spawn_editor(
    rx: async_channel::Receiver<ToolCall>,
    answer: impl Fn(&ToolCall) -> Option<ToolReply> + Send + 'static,
) -> JoinHandle<Vec<ToolCall>> {
    thread::spawn(move || {
        let mut seen = Vec::new();
        while let Ok(call) = rx.recv_blocking() {
            if let Some(reply) = answer(&call) {
                let _ = call.reply.send(reply);
            }
            seen.push(call);
        }
        seen
    })
}

/// Receives the next tool call, failing the test after [`TIMEOUT`].
pub fn next_call(rx: &async_channel::Receiver<ToolCall>) -> ToolCall {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(call) => return call,
            Err(async_channel::TryRecvError::Closed) => panic!("tool channel closed"),
            Err(async_channel::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "no tool call arrived");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

pub fn request(id: impl Into<Value>, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id.into(), "method": method, "params": params }).to_string()
}

pub fn tool_call(id: impl Into<Value>, name: &str, arguments: Value) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// The text of a successful `tools/call` response (asserting its shape).
pub fn result_text(response: &Value) -> (String, bool) {
    let result = &response["result"];
    let is_error = result["isError"].as_bool().expect("isError");
    let text = result["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter(|c| c["type"] == "text")
        .map(|c| c["text"].as_str().expect("text").to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    (text, is_error)
}

/// A client that talks to the server's socket directly (no bridge).
pub struct RawClient {
    stream: Stream,
    lines: Receiver<String>,
}

impl RawClient {
    /// Connects and sends `first_line` (normally the token line).
    pub fn connect_with(socket: &Path, first_line: &str) -> Self {
        let mut stream = connect(socket).expect("connect");
        stream
            .write_all(format!("{first_line}\n").as_bytes())
            .expect("send the first line");
        let reader = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { stream, lines }
    }

    pub fn connect(socket: &Path) -> Self {
        Self::connect_with(socket, &format!("{{\"lemma_token\":\"{TOKEN}\"}}"))
    }

    pub fn send(&mut self, line: &str) {
        self.stream
            .write_all(format!("{line}\n").as_bytes())
            .expect("send");
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("send");
    }

    pub fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(TIMEOUT)
            .expect("the server replied");
        serde_json::from_str(&line).expect("a JSON reply")
    }

    /// The next reply, if one arrives within `wait`.
    pub fn try_recv(&self, wait: Duration) -> Option<Value> {
        match self.lines.recv_timeout(wait) {
            Ok(line) => Some(serde_json::from_str(&line).expect("a JSON reply")),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Waits until the server closes the connection; panics if it sends anything first.
    pub fn expect_closed_silently(&mut self) {
        match self.lines.recv_timeout(TIMEOUT) {
            Err(RecvTimeoutError::Disconnected) => {}
            Ok(line) => panic!("expected no reply, got {line}"),
            Err(RecvTimeoutError::Timeout) => panic!("the server kept the connection open"),
        }
    }

    pub fn call(&mut self, line: &str) -> Value {
        self.send(line);
        self.recv()
    }

    pub fn shutdown_write(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }
}

/// Sends `payload` on a fresh connection and returns everything the server sends until it closes
/// the connection (a reset, which Linux reports when the server closes with unread input,
/// counts as closing).
pub fn raw_exchange(socket: &Path, payload: &[u8]) -> Vec<u8> {
    let mut stream = connect(socket).expect("connect");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    // The server may close before it has read everything.
    let _ = stream.write_all(payload);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut out = Vec::new();
    match stream.read_to_end(&mut out) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("the server did not close the connection: {e}"),
    }
    out
}

/// The real `lemma-mcp` bridge. `cargo test -p lemma-agent` does not build other packages'
/// binaries, so this builds it once per test process with `cargo build -p lemma-mcp` into the
/// target directory of the running test (cargo releases its build lock while tests run), with
/// the same profile. `LEMMA_MCP_BRIDGE` overrides it.
pub fn bridge_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(path) = std::env::var_os("LEMMA_MCP_BRIDGE") {
            return PathBuf::from(path);
        }
        let exe = std::env::current_exe().expect("test executable");
        // <target>/<profile>/deps/<test>
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("test executable in <target>/<profile>/deps");
        let target_dir = profile_dir.parent().expect("target directory");
        let profile = profile_dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("profile directory name");
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args(["build", "--quiet", "-p", "lemma-mcp", "--target-dir"])
            .arg(target_dir)
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        match profile {
            "debug" => {}
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.args(["--profile", other]);
            }
        }
        let out = cmd.output().expect("run cargo build -p lemma-mcp");
        let bin = profile_dir.join(format!("lemma-mcp{}", std::env::consts::EXE_SUFFIX));
        assert!(
            out.status.success() && bin.is_file(),
            "building lemma-mcp failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        bin
    })
}

/// A running `lemma-mcp` bridge with its stdout delivered line by line.
pub struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    reader: Option<JoinHandle<()>>,
}

impl Bridge {
    pub fn spawn(socket: &Path, token: &str) -> Self {
        let mut child = Command::new(bridge_binary())
            .env("LEMMA_MCP_SOCKET", socket)
            .env("LEMMA_MCP_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lemma-mcp");
        let stdin = child.stdin.take();
        let out = child.stdout.take().expect("stdout");
        let (tx, stdout) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            stdout,
            reader: Some(reader),
        }
    }

    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| stdin.flush())
            .expect("write to the bridge");
    }

    /// Like `send`, but the bridge may already have exited.
    pub fn try_send(&mut self, line: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin
                .write_all(format!("{line}\n").as_bytes())
                .and_then(|()| stdin.flush());
        }
    }

    pub fn recv(&self) -> Value {
        let line = self
            .stdout
            .recv_timeout(TIMEOUT)
            .expect("the bridge relayed a reply");
        serde_json::from_str(&line).expect("a JSON reply")
    }

    pub fn try_recv(&self, wait: Duration) -> Option<Value> {
        self.stdout
            .recv_timeout(wait)
            .ok()
            .map(|line| serde_json::from_str(&line).expect("a JSON reply"))
    }

    pub fn call(&mut self, line: &str) -> Value {
        self.send(line);
        self.recv()
    }

    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for the bridge to exit; panics (after killing it) if it does not.
    pub fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("lemma-mcp did not exit within {TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lines printed after the replies already received, up to EOF. Call after `wait`.
    pub fn remaining_output(&self) -> Vec<String> {
        self.stdout.try_iter().collect()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The handshake Claude CLI 2.1.288 sent to a stdio MCP server (recorded by the research probe):
/// `server/discover`, `initialize`, `notifications/initialized`, `tools/list`.
pub fn cli_handshake() -> [&'static str; 4] {
    [
        r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"},"io.modelcontextprotocol/clientCapabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}}}}"#,
        r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"}},"jsonrpc":"2.0","id":0}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#,
    ]
}

/// The names of this process's threads (`/proc/self/task/*/comm`).
pub fn thread_names() -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").expect("/proc/self/task") {
        let Ok(entry) = entry else { continue };
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            names.push(comm.trim_end().to_owned());
        }
    }
    names
}

/// Polls `done` every 5 ms for up to [`TIMEOUT`].
pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}
