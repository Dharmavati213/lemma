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
//! std only, so the binary stays small and starts fast. Exit status 0 on a clean close, 1 when the
//! environment is incomplete or the socket cannot be reached.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const SOCKET_ENV: &str = "LEMMA_MCP_SOCKET";
const TOKEN_ENV: &str = "LEMMA_MCP_TOKEN";
/// How long replies are still forwarded after stdin closed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Which copy loop ended first.
enum Closed {
    /// stdin reached EOF (the CLI is done) or the socket refused writes.
    Client,
    /// The socket reached EOF (the editor closed it) or stdout refused writes.
    Editor,
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
    let mut upstream =
        UnixStream::connect(&socket).map_err(|e| format!("cannot connect to {socket}: {e}"))?;
    upstream
        .write_all(handshake_line(&token).as_bytes())
        .map_err(|e| format!("cannot send the token to {socket}: {e}"))?;
    let from_editor = upstream
        .try_clone()
        .map_err(|e| format!("cannot clone the socket: {e}"))?;
    let to_editor = upstream
        .try_clone()
        .map_err(|e| format!("cannot clone the socket: {e}"))?;

    let (closed_tx, closed_rx) = mpsc::channel();
    let editor_tx = closed_tx.clone();
    thread::spawn(move || {
        // A failed copy means one side is gone, which ends the session like EOF does.
        let _ = pump(from_editor, io::stdout().lock());
        let _ = editor_tx.send(Closed::Editor);
    });
    thread::spawn(move || {
        let _ = pump(io::stdin().lock(), &to_editor);
        let _ = closed_tx.send(Closed::Client);
    });

    // Returning from `main` ends the process, including a copy thread still blocked in `read`.
    if let Ok(Closed::Client) = closed_rx.recv() {
        let _ = upstream.shutdown(Shutdown::Write);
        let _ = closed_rx.recv_timeout(DRAIN_GRACE);
    }
    Ok(())
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

/// Copies `from` to `to` until EOF, flushing after every read so each JSON-RPC line goes out at once.
fn pump(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
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
        pump(input.as_bytes(), &mut out).unwrap();
        assert_eq!(out, input.as_bytes());
    }
}
