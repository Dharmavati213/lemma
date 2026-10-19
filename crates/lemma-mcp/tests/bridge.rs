//! End-to-end tests of the `lemma-mcp` binary against an in-test unix-socket server.
//!
//! Unix socket paths must be shorter than 108 bytes, so sockets live in a fresh directory under
//! `$XDG_RUNTIME_DIR` (or the temp dir), and one test hands the bridge a relative path.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const SOCKET_NAME: &str = "mcp.sock";
const TIMEOUT: Duration = Duration::from_secs(10);
/// `sun_path` is 108 bytes including the terminating NUL; keep a margin.
const MAX_SOCKET_PATH: usize = 100;

/// A private directory holding one test socket, removed on drop.
struct SocketDir(PathBuf);

impl SocketDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "lemma-mcp-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let candidates = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([std::env::temp_dir(), PathBuf::from("/tmp")]);
        for base in candidates {
            let dir = base.join(&name);
            if dir.join(SOCKET_NAME).as_os_str().len() >= MAX_SOCKET_PATH {
                continue;
            }
            if std::fs::DirBuilder::new().mode(0o700).create(&dir).is_ok() {
                return Self(dir);
            }
        }
        panic!("no writable directory gives a socket path shorter than {MAX_SOCKET_PATH} bytes");
    }

    fn socket(&self) -> PathBuf {
        self.0.join(SOCKET_NAME)
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the fake editor saw on its single connection.
#[derive(Debug, Default)]
struct ServerLog {
    handshake: String,
    requests: Vec<String>,
    /// The bridge half-closed the socket (EOF) rather than the server hanging up first.
    saw_eof: bool,
}

/// How the fake editor treats the connection after a valid handshake.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Answer every request line until EOF.
    EchoUntilEof,
    /// Answer one request, then close the connection.
    HangUpAfterOne,
}

/// Accepts one connection. A bad token closes it without a reply, like the real MCP server.
fn spawn_server(listener: UnixListener, behaviour: Behaviour) -> JoinHandle<ServerLog> {
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone");
        let mut lines = BufReader::new(stream).lines();
        let mut log = ServerLog {
            handshake: lines.next().and_then(Result::ok).unwrap_or_default(),
            ..ServerLog::default()
        };
        if log.handshake != format!("{{\"lemma_token\":\"{TOKEN}\"}}") {
            return log;
        }
        loop {
            match lines.next() {
                Some(Ok(request)) => {
                    writeln!(writer, "{{\"reply_to\":{request}}}").expect("reply");
                    log.requests.push(request);
                    if matches!(behaviour, Behaviour::HangUpAfterOne) {
                        return log;
                    }
                }
                Some(Err(_)) => return log,
                None => {
                    log.saw_eof = true;
                    return log;
                }
            }
        }
    })
}

/// A running bridge with its stdout delivered line by line.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
}

impl Bridge {
    fn spawn(socket: &Path, token: &str, cwd: Option<&Path>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lemma-mcp"));
        cmd.env("LEMMA_MCP_SOCKET", socket)
            .env("LEMMA_MCP_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().expect("spawn lemma-mcp");
        let stdin = child.stdin.take();
        let out = child.stdout.take().expect("stdout");
        let (tx, stdout) = mpsc::channel();
        thread::spawn(move || {
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
        }
    }

    fn send(&mut self, line: &str) {
        self.try_send(line).expect("write to bridge");
    }

    /// Like `send`, but the bridge may already have exited.
    fn try_send(&mut self, line: &str) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}")?;
        stdin.flush()
    }

    fn recv(&self) -> String {
        self.stdout
            .recv_timeout(TIMEOUT)
            .expect("bridge relayed a reply")
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for the bridge to exit; panics (after killing it) if it does not.
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("lemma-mcp did not exit within {TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the bridge printed on stdout after the replies already received, up to EOF.
    /// Call it after the bridge exited.
    fn remaining_output(&self) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            match self.stdout.recv_timeout(TIMEOUT) {
                Ok(line) => lines.push(line),
                Err(RecvTimeoutError::Disconnected) => return lines,
                Err(RecvTimeoutError::Timeout) => panic!("stdout of lemma-mcp never closed"),
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn relays_two_requests_and_exits_when_stdin_closes() {
    let dir = SocketDir::new();
    let server = spawn_server(
        UnixListener::bind(dir.socket()).unwrap(),
        Behaviour::EchoUntilEof,
    );
    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN, None);

    let first = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let second = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    bridge.send(first);
    assert_eq!(bridge.recv(), format!("{{\"reply_to\":{first}}}"));
    bridge.send(second);
    assert_eq!(bridge.recv(), format!("{{\"reply_to\":{second}}}"));

    bridge.close_stdin();
    assert!(bridge.wait().success());
    let log = server.join().unwrap();
    assert_eq!(log.handshake, format!("{{\"lemma_token\":\"{TOKEN}\"}}"));
    assert_eq!(log.requests, [first, second]);
    assert!(
        log.saw_eof,
        "the bridge half-closes the socket on stdin EOF"
    );
    assert!(bridge.remaining_output().is_empty());
}

#[test]
fn accepts_a_relative_socket_path() {
    let dir = SocketDir::new();
    let server = spawn_server(
        UnixListener::bind(dir.socket()).unwrap(),
        Behaviour::EchoUntilEof,
    );
    let mut bridge = Bridge::spawn(Path::new(SOCKET_NAME), TOKEN, Some(&dir.0));

    bridge.send(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(
        bridge.recv(),
        r#"{"reply_to":{"jsonrpc":"2.0","id":1,"method":"ping"}}"#
    );
    bridge.close_stdin();
    assert!(bridge.wait().success());
    assert_eq!(server.join().unwrap().requests.len(), 1);
}

#[test]
fn stops_relaying_after_a_bad_token() {
    let dir = SocketDir::new();
    let server = spawn_server(
        UnixListener::bind(dir.socket()).unwrap(),
        Behaviour::EchoUntilEof,
    );
    let mut bridge = Bridge::spawn(&dir.socket(), "not-the-token", None);
    // Racing with the refusal, so the write itself may fail once the bridge is gone.
    let _ = bridge.try_send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);

    // The server closes the connection after reading the handshake; the bridge must exit on its
    // own, with stdin still open, and never print a reply.
    let log = server.join().unwrap();
    assert_eq!(log.handshake, r#"{"lemma_token":"not-the-token"}"#);
    assert!(bridge.wait().success());
    assert!(bridge.remaining_output().is_empty());
    assert!(log.requests.is_empty());
}

#[test]
fn exits_when_the_editor_hangs_up() {
    let dir = SocketDir::new();
    let server = spawn_server(
        UnixListener::bind(dir.socket()).unwrap(),
        Behaviour::HangUpAfterOne,
    );
    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN, None);

    bridge.send(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(
        bridge.recv(),
        r#"{"reply_to":{"jsonrpc":"2.0","id":1,"method":"ping"}}"#
    );
    // stdin stays open: the socket closing alone must end the bridge.
    assert!(bridge.wait().success());
    assert_eq!(server.join().unwrap().requests.len(), 1);
}

#[test]
fn fails_without_environment_or_server() {
    let dir = SocketDir::new();
    let missing_env = Command::new(env!("CARGO_BIN_EXE_lemma-mcp"))
        .env_remove("LEMMA_MCP_SOCKET")
        .env_remove("LEMMA_MCP_TOKEN")
        .stdin(Stdio::null())
        .output()
        .expect("run lemma-mcp");
    assert_eq!(missing_env.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing_env.stderr).contains("LEMMA_MCP_SOCKET"));

    // Nobody listens on this path.
    let no_server = Command::new(env!("CARGO_BIN_EXE_lemma-mcp"))
        .env("LEMMA_MCP_SOCKET", dir.socket())
        .env("LEMMA_MCP_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .output()
        .expect("run lemma-mcp");
    assert_eq!(no_server.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_server.stderr).contains("cannot connect"));
}

#[test]
fn socket_dir_paths_fit_in_sun_path() {
    let dir = SocketDir::new();
    assert!(dir.socket().as_os_str().len() < MAX_SOCKET_PATH);
    // Binding proves the path is accepted by the kernel.
    let _listener = UnixListener::bind(dir.socket()).unwrap();
    UnixStream::connect(dir.socket()).unwrap();
}
