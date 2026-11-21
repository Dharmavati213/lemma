//! `lemma-mcp`: the stdio <-> unix-socket bridge that the Claude CLI spawns as the `lemma` stdio MCP server.
//!
//! The editor's MCP server listens on the unix socket named by `LEMMA_MCP_SOCKET` and accepts a
//! connection only if its first line is `{"lemma_token":"<LEMMA_MCP_TOKEN>"}`. The bridge sends that
//! line, then copies bytes stdin -> socket and socket -> stdout (newline-delimited JSON-RPC passes
//! through unchanged) until either side closes:
//! - the editor closes the socket (shutdown, or a refused token): exit at once;
//! - the CLI closes stdin: half-close the socket and forward replies still in flight until the
//!   editor closes its side, for at most [`DRAIN_GRACE`].
//!
//! Until the editor has sent something back, the bridge keeps what it sent (the token line, the
//! CLI's first request). If the editor closes the connection before that, or it cannot connect,
//! it connects again after a short wait, a few times ([`RETRIES`]), and sends it all again: the
//! editor turns new connections away for a moment while a program floods its socket. Nothing is
//! sent again once the editor has answered, and it does not try again once the socket is gone
//! (the editor removes it when it stops).
//!
//! On Windows, whose standard library has no unix sockets, `LEMMA_MCP_SOCKET` names a file holding
//! the loopback address (`127.0.0.1:<port>`) of the editor's TCP listener instead, and the bridge
//! connects there; everything else is the same.
//!
//! std only, so the binary stays small and starts fast. Exit status 0 on a clean close, 1 when the
//! environment is incomplete or the socket cannot be reached.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

const SOCKET_ENV: &str = "LEMMA_MCP_SOCKET";
const TOKEN_ENV: &str = "LEMMA_MCP_TOKEN";
/// How long replies are still forwarded after stdin closed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How many times the bridge connects again before the editor has answered, and the wait before
/// the first time, doubled each time after (3.15 s in all).
const RETRIES: u32 = 6;
const FIRST_RETRY_WAIT: Duration = Duration::from_millis(50);
/// Most bytes kept to be sent again; past that, the bridge does not connect again.
const MAX_RESEND: usize = 1 << 20;

#[cfg(unix)]
type Socket = UnixStream;
#[cfg(windows)]
type Socket = std::net::TcpStream;

/// Which copy loop ended first.
enum Closed {
    /// stdin reached EOF (the CLI is done) or the socket refused writes.
    Client,
    /// The socket reached EOF (the editor closed it) or stdout refused writes.
    Editor,
}

/// The connection to the editor, which the stdin copy loop writes to.
struct Upstream {
    socket: Socket,
    /// Everything sent on the connection, the token line first, while the editor has not sent
    /// anything back: sent again on the next one. `None` once it has, or past [`MAX_RESEND`].
    unanswered: Option<Vec<u8>>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("lemma-mcp: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let socket = env_var(SOCKET_ENV)?;
    let token = env_var(TOKEN_ENV)?;
    let handshake = handshake_line(&token).into_bytes();
    let mut retries = 0;
    let first = loop {
        match connect_and_send(&socket, &handshake) {
            Ok(first) => break first,
            Err(e) if gone(&e) || !wait_to_retry(&mut retries) => {
                return Err(format!("cannot connect to {socket}: {e}"));
            }
            Err(_) => {}
        }
    };
    let upstream = Arc::new(Mutex::new(Upstream {
        socket: first,
        unanswered: Some(handshake),
    }));
    // The editor has sent something back.
    let answered = Arc::new(AtomicBool::new(false));

    let (closed_tx, closed_rx) = mpsc::channel();
    from_editor(&lock(&upstream).socket, &answered, closed_tx.clone())?;
    {
        let upstream = Arc::clone(&upstream);
        let answered = Arc::clone(&answered);
        let closed_tx = closed_tx.clone();
        thread::spawn(move || {
            let _ = to_editor(io::stdin().lock(), &upstream, &answered);
            let _ = closed_tx.send(Closed::Client);
        });
    }

    // Returning from `main` ends the process, including a copy thread still blocked in `read`.
    loop {
        match closed_rx.recv() {
            Ok(Closed::Editor) if !answered.load(Ordering::SeqCst) => {
                // Turned away (or the editor is stopping): again, with what this one was sent.
                // The lock keeps the stdin loop from writing meanwhile.
                let mut up = lock(&upstream);
                let Some(resend) = up.unanswered.clone() else {
                    return Ok(());
                };
                up.socket = loop {
                    if !wait_to_retry(&mut retries) {
                        return Ok(());
                    }
                    match connect_and_send(&socket, &resend) {
                        Ok(socket) => break socket,
                        Err(e) if gone(&e) => return Ok(()),
                        Err(_) => {}
                    }
                };
                from_editor(&up.socket, &answered, closed_tx.clone())?;
            }
            Ok(Closed::Client) => {
                let _ = lock(&upstream).socket.shutdown(Shutdown::Write);
                let _ = closed_rx.recv_timeout(DRAIN_GRACE);
                return Ok(());
            }
            Ok(Closed::Editor) | Err(_) => return Ok(()),
        }
    }
}

/// Connects to the editor and sends `bytes`.
fn connect_and_send(socket: &str, bytes: &[u8]) -> io::Result<Socket> {
    let mut stream = connect(socket)?;
    stream.write_all(bytes)?;
    Ok(stream)
}

/// The socket (on Windows the file with its address) is not there: the editor has stopped, or
/// never ran. It removes the socket when it stops, so there is nothing to wait for, and on
/// Windows a bridge that went on reading the file could keep the editor from removing it.
fn gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Waits before the next try to connect; false when there is none left.
fn wait_to_retry(retries: &mut u32) -> bool {
    if *retries >= RETRIES {
        return false;
    }
    thread::sleep(FIRST_RETRY_WAIT * 2u32.pow(*retries));
    *retries += 1;
    true
}

/// Copies what the editor sends on `socket` to stdout, in a thread of its own, and then says
/// so on `closed`.
fn from_editor(
    socket: &Socket,
    answered: &Arc<AtomicBool>,
    closed: mpsc::Sender<Closed>,
) -> Result<(), String> {
    let from = socket
        .try_clone()
        .map_err(|e| format!("cannot clone the socket: {e}"))?;
    let answered = Arc::clone(answered);
    thread::spawn(move || {
        // A failed copy means one side is gone, which ends the session like EOF does.
        let _ = pump(from, io::stdout().lock(), &answered);
        let _ = closed.send(Closed::Editor);
    });
    Ok(())
}

/// Copies `from` (stdin) to the editor's connection until EOF, keeping what the editor has not
/// answered yet (see [`Upstream::unanswered`]). A write that fails then is not an error: it is
/// sent again on the next connection.
fn to_editor(
    mut from: impl Read,
    upstream: &Mutex<Upstream>,
    answered: &AtomicBool,
) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let up = &mut *lock(upstream);
        if answered.load(Ordering::SeqCst)
            || up
                .unanswered
                .as_ref()
                .is_some_and(|kept| kept.len() + n > MAX_RESEND)
        {
            up.unanswered = None;
        }
        let written = up
            .socket
            .write_all(&buf[..n])
            .and_then(|()| up.socket.flush());
        match &mut up.unanswered {
            Some(kept) => kept.extend_from_slice(&buf[..n]),
            None => written?,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(unix)]
fn connect(socket: &str) -> io::Result<UnixStream> {
    UnixStream::connect(socket)
}

/// Connects to the loopback address in the port file `socket` (only a loopback one is used).
#[cfg(windows)]
fn connect(socket: &str) -> io::Result<std::net::TcpStream> {
    let text = std::fs::read_to_string(socket)?;
    match text.trim().parse::<std::net::SocketAddr>() {
        Ok(address) if address.ip().is_loopback() => {
            std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the file does not name the editor's MCP port",
        )),
    }
}

fn env_var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|e| format!("{name}: {e}"))
}

/// The first line sent on the socket: `{"lemma_token":"<token>"}` plus `'\n'`, JSON-escaped.
fn handshake_line(token: &str) -> String {
    let mut line = String::from("{\"lemma_token\":\"");
    for c in token.chars() {
        match c {
            '"' => line.push_str("\\\""),
            '\\' => line.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                // Writing to a String cannot fail.
                let _ = write!(line, "\\u{:04x}", u32::from(c));
            }
            c => line.push(c),
        }
    }
    line.push_str("\"}\n");
    line
}

/// Copies `from` to `to` until EOF, flushing after every read so each JSON-RPC line goes out at
/// once; `copied` is set once something was read.
fn pump(mut from: impl Read, mut to: impl Write, copied: &AtomicBool) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        copied.store(true, Ordering::SeqCst);
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_is_one_json_line() {
        assert_eq!(
            handshake_line("0123456789abcdef0123456789abcdef"),
            "{\"lemma_token\":\"0123456789abcdef0123456789abcdef\"}\n"
        );
    }

    #[test]
    fn handshake_escapes_json_specials() {
        assert_eq!(
            handshake_line("a\"b\\c\nd"),
            "{\"lemma_token\":\"a\\\"b\\\\c\\u000ad\"}\n"
        );
    }

    #[test]
    fn pump_copies_everything() {
        let input = "{\"id\":1}\n".repeat(5000);
        let mut out = Vec::new();
        let copied = AtomicBool::new(false);
        pump(input.as_bytes(), &mut out, &copied).unwrap();
        assert_eq!(out, input.as_bytes());
        assert!(copied.load(Ordering::SeqCst));
        let copied = AtomicBool::new(false);
        pump(&b""[..], &mut out, &copied).unwrap();
        assert!(!copied.load(Ordering::SeqCst));
    }
}
