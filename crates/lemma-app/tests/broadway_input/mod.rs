//! Real key events for GTK tests (WP2.9a; the technique of `tests/gtk_keys.rs`, WP2.10).
//!
//! GTK cannot inject events, so a test starts a private `gtk4-broadwayd` (its own display and
//! loopback port, so no other test window can receive the keys), runs its own binary again as a
//! child on that display, and the child sends key events through the daemon's browser protocol
//! (a WebSocket on 127.0.0.1). They take GDK's real path: application accelerators, the capture
//! phase from the window down (Lemma's key layer, Vim's controller), the text view, its input
//! method and its commit. Nothing leaves the machine.

#![allow(dead_code)]

use glib::translate::IntoGlib;
use gtk::gdk;
use gtk::prelude::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Set (to the daemon's port) in the child process.
pub const CHILD_PORT_ENV: &str = "LEMMA_TEST_BROADWAY_PORT";

/// True in the child process (which then runs only the real-input tests).
pub fn is_child() -> bool {
    std::env::var_os(CHILD_PORT_ENV).is_some()
}

/// A private `gtk4-broadwayd`, stopped (and its socket removed) on drop.
struct Daemon {
    child: Child,
    display: u32,
    port: u16,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: kill(2) on our own child process, which has not been reaped yet.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Starts a daemon on a free display and loopback port; `None` when `gtk4-broadwayd` is not
/// installed.
fn start_daemon(salt: u32) -> Option<Daemon> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let pid = std::process::id();
    for attempt in 0..8 {
        let display = 300 + (pid.wrapping_mul(13) + salt * 101 + attempt * 37) % 600;
        // broadwayd listens on `broadway<display + 1>.socket`.
        let socket = runtime.join(format!("broadway{}.socket", display + 1));
        if socket.exists() {
            continue;
        }
        let port = TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("a free loopback port")
            .port();
        let spawned = Command::new("gtk4-broadwayd")
            .args([
                format!(":{display}"),
                "--address".into(),
                "127.0.0.1".into(),
                "--port".into(),
                port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(child) = spawned else {
            println!("    (gtk4-broadwayd is not installed: the real-input tests are skipped)");
            return None;
        };
        let mut daemon = Daemon {
            child,
            display,
            port,
            socket,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = daemon.child.try_wait() {
                break;
            }
            if daemon.socket.exists() && TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(daemon);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    panic!("could not start a private gtk4-broadwayd");
}

/// Runs the test `child_test` of this binary as a child process on a private daemon and fails
/// when it fails (printing its output). The child selects it by name (the usual filter).
pub fn run_child(child_test: &str, salt: u32) {
    let Some(daemon) = start_daemon(salt) else {
        return;
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(child_test)
        .env(CHILD_PORT_ENV, daemon.port.to_string())
        .env("GDK_BACKEND", "broadway")
        .env("BROADWAY_DISPLAY", format!(":{}", daemon.display))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the child");
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let readers = (
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        }),
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (readers.0.join().unwrap(), readers.1.join().unwrap());
    drop(daemon);
    let passed =
        status.is_some_and(|s| s.success()) && out.contains(&format!("test {child_test} ... ok"));
    if !passed || std::env::var_os("LEMMA_TEST_TRACE").is_some() {
        println!("---- child stdout ----\n{out}\n---- child stderr ----\n{err}");
    }
    assert!(
        passed,
        "the real-input child {child_test} failed ({status:?})"
    );
}

/// A WebSocket client of the Broadway daemon speaking its browser protocol: each input message
/// is a binary frame of big-endian `i32`s `[type, serial, time, args…]`; a key is
/// `[keysym, modifier state]` and goes to the focused window.
///
/// **Frames.** GDK's Broadway backend waits for the browser to answer a roundtrip request
/// after each frame (up to a long timeout). [`Browser::connect`] never answers, like the harness
/// of `gtk_keys.rs`: frames nearly stop (one every few seconds), which keeps the tests
/// deterministic (nothing is laid out or drawn behind their back, spell checking never starts
/// after a first frame). [`Browser::connect_drawing`] answers them as the browser page does, so
/// frames run at full speed, for tests that need layout.
pub struct Browser {
    writer: Arc<Mutex<TcpStream>>,
    time: i32,
    /// How long the main loop runs after each key.
    pub settle: Duration,
}

const KEY_PRESS: i32 = 7;
const KEY_RELEASE: i32 = 8;
const ROUNDTRIP_NOTIFY: i32 = 14;
// GDK modifier bits, as the browser client sends them.
pub const SHIFT: i32 = 1;
pub const CONTROL: i32 = 4;
pub const ALT: i32 = 8;

impl Browser {
    /// Connects to the daemon of this child process (frames nearly stop; see [`Browser`]).
    pub fn connect() -> Self {
        Self::connect_with(false)
    }

    /// Connects and answers the daemon's roundtrips, so frames are drawn (see [`Browser`]).
    pub fn connect_drawing() -> Self {
        Self::connect_with(true)
    }

    fn connect_with(draw: bool) -> Self {
        let port: u16 = std::env::var(CHILD_PORT_ENV)
            .expect("in the child")
            .parse()
            .unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            stream,
            "GET /socket HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: broadway\r\n\
             Origin: http://127.0.0.1:{port}\r\n\r\n"
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("handshake");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
        // The daemon streams the display to its browser: drop it (answering its roundtrips
        // when `draw`).
        let answers = writer.clone();
        std::thread::spawn(move || {
            let mut reader = stream;
            while let Ok(message) = read_message(&mut reader) {
                if !draw {
                    continue;
                }
                for (id, tag) in roundtrips(&message) {
                    send(&answers, &[ROUNDTRIP_NOTIFY, 0, 0, id, tag]);
                }
            }
        });
        Self {
            writer,
            time: 1000,
            settle: Duration::from_millis(1),
        }
    }

    /// Presses and releases `key` with the modifier bits `mods`, then runs the main loop.
    pub fn key(&mut self, key: gdk::Key, mods: i32) {
        let keysym = i32::try_from(key.into_glib()).unwrap();
        self.time += 20;
        send(&self.writer, &[KEY_PRESS, 0, self.time, keysym, mods]);
        send(&self.writer, &[KEY_RELEASE, 0, self.time + 5, keysym, mods]);
        crate::common::pump_for(self.settle);
    }

    /// Types `text` character by character (`\n` is Return, `\t` Tab).
    pub fn text(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.key(gdk::Key::Return, 0),
                '\t' => self.key(gdk::Key::Tab, 0),
                c => {
                    let mods = if c.is_uppercase() { SHIFT } else { 0 };
                    self.key(keyval(c), mods);
                }
            }
        }
    }

    /// Ctrl + the key of `c`.
    pub fn ctrl(&mut self, c: char) {
        self.key(keyval(c), CONTROL);
    }
}

/// Sends one input message: a masked binary WebSocket frame of big-endian `i32`s.
fn send(writer: &Mutex<TcpStream>, words: &[i32]) {
    let payload: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    let mask = [0x5a, 0x17, 0xc3, 0x08];
    let mut frame = vec![0x82, 0x80 | u8::try_from(payload.len()).unwrap()];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    if let Ok(mut stream) = writer.lock() {
        let _ = stream.write_all(&frame);
    }
}

/// Reads one WebSocket message (joining continuation frames; control frames are skipped).
fn read_message(reader: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut message = Vec::new();
    loop {
        let mut head = [0u8; 2];
        reader.read_exact(&mut head)?;
        let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
        let mut len = u64::from(head[1] & 0x7f);
        if len == 126 {
            let mut ext = [0u8; 2];
            reader.read_exact(&mut ext)?;
            len = u64::from(u16::from_be_bytes(ext));
        } else if len == 127 {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            len = u64::from_be_bytes(ext);
        }
        let mut mask = [0u8; 4];
        if head[1] & 0x80 != 0 {
            reader.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; usize::try_from(len).unwrap_or(0)];
        reader.read_exact(&mut payload)?;
        if head[1] & 0x80 != 0 {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        if opcode == 0x8 {
            return Err(std::io::ErrorKind::ConnectionAborted.into());
        }
        if opcode >= 0x8 {
            continue; // ping/pong
        }
        message.extend_from_slice(&payload);
        if fin {
            return Ok(message);
        }
    }
}

/// The `(surface id, tag)` of every roundtrip request in a message of the daemon's display
/// stream (the commands of the browser page's `handleCommands`: `u8` op, `u32` serial, then the
/// op's arguments, little-endian).
fn roundtrips(message: &[u8]) -> Vec<(i32, i32)> {
    let mut found = Vec::new();
    let mut pos = 0usize;
    let u16_at = |p: usize| {
        message
            .get(p..p + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |p: usize| {
        message
            .get(p..p + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    while pos + 5 <= message.len() {
        let op = message[pos];
        pos += 5;
        let size = match op {
            0 => 3,          // grab pointer: id, owner events
            1 | 10 => 0,     // ungrab pointer, disconnected
            2 => 10,         // new surface: id, x, y, w, h
            3..=7 | 12 => 2, // show, hide, raise, lower, destroy, show keyboard
            9 => 4,          // transient for: id, parent
            8 => {
                // move/resize: id, flags, [x, y], [w, h]
                let Some(&flags) = message.get(pos + 2) else {
                    break;
                };
                3 + if flags & 1 != 0 { 4 } else { 0 } + if flags & 2 != 0 { 4 } else { 0 }
            }
            13 => match u32_at(pos + 4) {
                Some(n) => 8 + n as usize, // upload texture: id, size, data
                None => break,
            },
            14 => 4, // release texture: id
            15 => match u32_at(pos + 2) {
                Some(n) => 6 + 4 * n as usize, // set nodes: id, length, nodes
                None => break,
            },
            16 => {
                // roundtrip: id, tag
                if let (Some(id), Some(tag)) = (u16_at(pos), u32_at(pos + 2)) {
                    found.push((i32::from(id), tag as i32));
                }
                6
            }
            _ => break,
        };
        pos += size;
    }
    found
}

/// The keyval of character `c`.
pub fn keyval(c: char) -> gdk::Key {
    // `gdk::Key` has no safe constructor from a keyval; it is a `u32` GValue.
    gdk::unicode_to_keyval(u32::from(c))
        .to_value()
        .get::<gdk::Key>()
        .expect("a keyval")
}
