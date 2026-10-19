//! The Anthropic API key of the API-key backend (WP5.1, [`crate::messages`]).
//!
//! # Lookup order
//!
//! 1. the environment variable [`ENV_VAR`] (`ANTHROPIC_API_KEY`);
//! 2. the GNOME keyring through libsecret's CLI:
//!    `secret-tool lookup service lemma kind anthropic-api-key`.
//!
//! [`Keyring::store`] runs `secret-tool store --label="Lemma Anthropic API key" service lemma kind
//! anthropic-api-key` with the key on **stdin**, never in argv; [`Keyring::clear`] removes it.
//!
//! # Secrecy
//!
//! The key lives only in an [`ApiKey`], whose `Debug` and `Display` print `<redacted>`: it is
//! never written to `config.toml`, logs, raw logs, events or error messages. Error messages from
//! `secret-tool` are passed through [`ApiKey::redact`] as well.
//!
//! Every call here blocks until `secret-tool` ends (the keyring may first ask the user to unlock
//! it), at most [`SECRET_TOOL_TIMEOUT`]: call it off the GTK main thread.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The environment variable checked first.
pub const ENV_VAR: &str = "ANTHROPIC_API_KEY";
/// The keyring item's label.
pub const LABEL: &str = "Lemma Anthropic API key";
/// The keyring item's attributes, as `secret-tool` arguments.
pub const ATTRIBUTES: [&str; 4] = ["service", "lemma", "kind", "anthropic-api-key"];
/// How long one `secret-tool` run may take (an unlock prompt included) before it is killed.
pub const SECRET_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Keys longer than this are refused (a real key is about 100 bytes).
const MAX_KEY_BYTES: usize = 4096;

/// An API key. `Debug`/`Display` never show it; [`ApiKey::expose`] is the only way to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The key without surrounding whitespace; `None` when that is empty, longer than 4 KiB or
    /// holds a control character or a space (it could not be sent as an HTTP header).
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim();
        let valid = !key.is_empty()
            && key.len() <= MAX_KEY_BYTES
            && !key.chars().any(|c| c.is_control() || c.is_whitespace());
        valid.then(|| Self(key.to_owned()))
    }

    /// The key itself, for the `x-api-key` header and `secret-tool`'s stdin only.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the key replaced by `<redacted>`.
    pub fn redact(&self, text: &str) -> String {
        text.replace(self.0.as_str(), "<redacted>")
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where a key came from (for the panel's key-status row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// [`ENV_VAR`].
    Env,
    /// The GNOME keyring.
    Keyring,
}

impl KeySource {
    /// "from ANTHROPIC_API_KEY" / "from GNOME keyring".
    pub fn describe(self) -> &'static str {
        match self {
            Self::Env => "from ANTHROPIC_API_KEY",
            Self::Keyring => "from GNOME keyring",
        }
    }
}

/// The `secret-tool` program to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyring {
    program: PathBuf,
}

impl Default for Keyring {
    /// `secret-tool`, found on `PATH`.
    fn default() -> Self {
        Self {
            program: PathBuf::from("secret-tool"),
        }
    }
}

impl Keyring {
    /// Runs this program instead of `secret-tool`.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// `secret-tool` from the first directory of `path` (a `PATH`-style list) that holds an
    /// executable one; `None` when none does.
    pub fn on_path(path: &OsStr) -> Option<Self> {
        std::env::split_paths(path)
            .map(|dir| dir.join("secret-tool"))
            .find(|p| is_executable(p))
            .map(Self::with_program)
    }

    /// Whether the program can be found (on `PATH` for the default one).
    pub fn available(&self) -> bool {
        if self.program.components().count() > 1 {
            return is_executable(&self.program);
        }
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| is_executable(&dir.join(&self.program)))
        })
    }

    /// The stored key: `Ok(None)` when there is none (or the keyring has none for us).
    pub fn lookup(&self) -> io::Result<Option<ApiKey>> {
        let mut args = vec!["lookup"];
        args.extend(ATTRIBUTES);
        let out = self.run(&args, None)?;
        if !out.status.success() {
            // secret-tool exits 1 without output when nothing matches.
            if out.stderr.trim().is_empty() {
                return Ok(None);
            }
            return Err(io::Error::other(format!(
                "secret-tool lookup failed: {}",
                out.stderr.trim()
            )));
        }
        Ok(ApiKey::new(&out.stdout))
    }

    /// Stores `key` (replacing an earlier one); the key goes to `secret-tool`'s stdin.
    pub fn store(&self, key: &ApiKey) -> io::Result<()> {
        let label = format!("--label={LABEL}");
        let mut args = vec!["store", label.as_str()];
        args.extend(ATTRIBUTES);
        let out = self.run(&args, Some(key))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "secret-tool store failed: {}",
                key.redact(out.stderr.trim())
            )))
        }
    }

    /// Removes the stored key (nothing stored is not an error).
    pub fn clear(&self) -> io::Result<()> {
        let mut args = vec!["clear"];
        args.extend(ATTRIBUTES);
        let out = self.run(&args, None)?;
        if out.status.success() || out.stderr.trim().is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "secret-tool clear failed: {}",
                out.stderr.trim()
            )))
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&ApiKey>) -> io::Result<Output> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot run {}: {e}", self.program.display()),
                )
            })?;
        if let (Some(key), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A failed write shows up as secret-tool's own failure.
            let _ = pipe.write_all(key.expose().as_bytes());
        }
        let stdout = child.stdout.take().map(reader_thread);
        let stderr = child.stderr.take().map(reader_thread);
        let status = wait_with_timeout(&mut child, SECRET_TOOL_TIMEOUT)?;
        let collect = |h: Option<thread::JoinHandle<Vec<u8>>>| {
            h.and_then(|h| h.join().ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        };
        Ok(Output {
            status,
            stdout: collect(stdout),
            stderr: collect(stderr),
        })
    }
}

struct Output {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn reader_thread(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.by_ref().take(64 * 1024).read_to_end(&mut buf);
        buf
    })
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "secret-tool did not answer in time",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The key and where it came from: `env_value` (pass `std::env::var_os(ENV_VAR)`) first, then
/// the keyring. A keyring that cannot be asked counts as holding no key (logged).
pub fn lookup_with(env_value: Option<&OsStr>, keyring: &Keyring) -> Option<(ApiKey, KeySource)> {
    if let Some(key) = env_value.and_then(OsStr::to_str).and_then(ApiKey::new) {
        return Some((key, KeySource::Env));
    }
    match keyring.lookup() {
        Ok(key) => key.map(|k| (k, KeySource::Keyring)),
        Err(e) => {
            tracing::warn!("keys: the keyring could not be asked: {e}");
            None
        }
    }
}

/// [`lookup_with`] for this process: `ANTHROPIC_API_KEY`, then `secret-tool` on `PATH`.
pub fn lookup() -> Option<(ApiKey, KeySource)> {
    lookup_with(std::env::var_os(ENV_VAR).as_deref(), &Keyring::default())
}

/// Stores `key` in the GNOME keyring (`secret-tool` on `PATH`).
pub fn store(key: &ApiKey) -> io::Result<()> {
    Keyring::default().store(key)
}

/// Removes the key from the GNOME keyring (`secret-tool` on `PATH`).
pub fn clear() -> io::Result<()> {
    Keyring::default().clear()
}
