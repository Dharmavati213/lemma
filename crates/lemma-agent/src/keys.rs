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
//! # macOS
//!
//! On macOS the key is a generic password in the login keychain (service `lemma`, account
//! `anthropic-api-key`), through `/usr/bin/security` ([`Keyring::macos_keychain`]):
//! `find-generic-password -w` reads it and `delete-generic-password` removes it. `security` takes a
//! new password only as an argument, so [`Keyring::store`] runs `security -i` and writes the
//! `add-generic-password` command to its **stdin**, again keeping the key out of argv; it reads the
//! key back to confirm, since `security -i` does not report a failed command in its exit status.
//! Items `security` created are readable by `security` without a prompt.
//!
//! # Windows
//!
//! On Windows the key is a generic credential in the user's Credential Manager (target
//! [`WINDOWS_TARGET`], user name `anthropic-api-key`), read, written and deleted with
//! `CredReadW`, `CredWriteW` and `CredDeleteW`: no program runs, so nothing can see the key in a
//! command line. The credential is kept on this computer only (`CRED_PERSIST_LOCAL_MACHINE`).
//!
//! # Secrecy
//!
//! The key lives only in an [`ApiKey`], whose `Debug` and `Display` print `<redacted>`: it is
//! never written to `config.toml`, logs, raw logs, events or error messages. Error messages from
//! `secret-tool` are passed through [`ApiKey::redact`] as well.
//!
//! Every call here blocks until `secret-tool` (or `security`) ends (the keyring may first ask the
//! user to unlock it), at most [`SECRET_TOOL_TIMEOUT`]: call it off the GTK main thread.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The environment variable checked first.
pub const ENV_VAR: &str = "ANTHROPIC_API_KEY";
/// The keyring item's label.
pub const LABEL: &str = "Lemma Anthropic API key";
/// The keyring item's attributes, as `secret-tool` arguments.
pub const ATTRIBUTES: [&str; 4] = ["service", "lemma", "kind", "anthropic-api-key"];
/// The macOS keychain item's service and account (`security -s … -a …`).
pub const KEYCHAIN_ITEM: [&str; 4] = ["-s", "lemma", "-a", "anthropic-api-key"];
/// The Windows Credential Manager's target name for the key.
pub const WINDOWS_TARGET: &str = "lemma/anthropic-api-key";
/// `security`'s exit status when no item matches (`errSecItemNotFound`).
const SECURITY_NOT_FOUND: i32 = 44;
/// Where the key is kept, for messages: "the GNOME keyring (secret-tool)", "the macOS
/// Keychain" or "the Windows Credential Manager".
pub const KEYRING_DESCRIPTION: &str = if cfg!(target_os = "macos") {
    "the macOS Keychain"
} else if cfg!(windows) {
    "the Windows Credential Manager"
} else {
    "the GNOME keyring (secret-tool)"
};
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

    /// The key itself, for the `x-api-key` header and the keyring program's stdin only.
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
    /// The GNOME keyring (the macOS Keychain on macOS).
    Keyring,
}

impl KeySource {
    /// "from ANTHROPIC_API_KEY" / "from GNOME keyring" ("from macOS Keychain", "from Windows
    /// Credential Manager").
    pub fn describe(self) -> &'static str {
        match self {
            Self::Env => "from ANTHROPIC_API_KEY",
            Self::Keyring if cfg!(target_os = "macos") => "from macOS Keychain",
            Self::Keyring if cfg!(windows) => "from Windows Credential Manager",
            Self::Keyring => "from GNOME keyring",
        }
    }
}

/// How the keyring program is spoken to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// libsecret's `secret-tool`.
    SecretTool,
    /// macOS's `security`.
    MacSecurity,
    /// The Windows Credential Manager (no program).
    WindowsCredentials,
}

/// The keyring program to run: `secret-tool`, or `security` on macOS. For the Windows
/// Credential Manager no program runs, and `program` is the credential's target name instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyring {
    program: PathBuf,
    protocol: Protocol,
}

impl Default for Keyring {
    /// `secret-tool`, found on `PATH`; on macOS `/usr/bin/security`; on Windows the
    /// Credential Manager.
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::macos_keychain("/usr/bin/security")
        } else if cfg!(windows) {
            Self::windows_credentials(WINDOWS_TARGET)
        } else {
            Self::with_program("secret-tool")
        }
    }
}

impl Keyring {
    /// Runs this program instead of `secret-tool` (it must take `secret-tool`'s arguments).
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            protocol: Protocol::SecretTool,
        }
    }

    /// The macOS login keychain through `program`, which takes the arguments of macOS's
    /// `/usr/bin/security` (see the module docs).
    pub fn macos_keychain(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            protocol: Protocol::MacSecurity,
        }
    }

    /// The Windows Credential Manager's generic credential `target` (normally
    /// [`WINDOWS_TARGET`]; see the module docs). Only usable on Windows.
    pub fn windows_credentials(target: &str) -> Self {
        Self {
            program: PathBuf::from(target),
            protocol: Protocol::WindowsCredentials,
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

    /// Whether the program can be found (on `PATH` for the default one). The Windows
    /// Credential Manager always is.
    pub fn available(&self) -> bool {
        if self.protocol == Protocol::WindowsCredentials {
            return true;
        }
        if self.program.components().count() > 1 {
            return is_executable(&self.program);
        }
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| is_executable(&dir.join(&self.program)))
        })
    }

    /// The stored key: `Ok(None)` when there is none (or the keyring has none for us).
    pub fn lookup(&self) -> io::Result<Option<ApiKey>> {
        if self.protocol == Protocol::MacSecurity {
            return self.lookup_macos();
        }
        if self.protocol == Protocol::WindowsCredentials {
            return credentials::read(&self.target());
        }
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

    /// Stores `key` (replacing an earlier one); the key goes to the program's stdin.
    pub fn store(&self, key: &ApiKey) -> io::Result<()> {
        if self.protocol == Protocol::MacSecurity {
            return self.store_macos(key);
        }
        if self.protocol == Protocol::WindowsCredentials {
            return credentials::write(&self.target(), key);
        }
        let label = format!("--label={LABEL}");
        let mut args = vec!["store", label.as_str()];
        args.extend(ATTRIBUTES);
        let out = self.run(&args, Some(key.expose()))?;
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
        if self.protocol == Protocol::MacSecurity {
            return self.clear_macos();
        }
        if self.protocol == Protocol::WindowsCredentials {
            return credentials::delete(&self.target());
        }
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

    /// The Credential Manager target of [`Keyring::windows_credentials`].
    fn target(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// [`Keyring::lookup`] in the macOS keychain: `security find-generic-password -w` prints the
    /// password alone.
    fn lookup_macos(&self) -> io::Result<Option<ApiKey>> {
        let mut args = vec!["find-generic-password"];
        args.extend(KEYCHAIN_ITEM);
        args.push("-w");
        let out = self.run(&args, None)?;
        if out.status.success() {
            return Ok(ApiKey::new(&out.stdout));
        }
        if out.status.code() == Some(SECURITY_NOT_FOUND) {
            return Ok(None);
        }
        Err(io::Error::other(format!(
            "security find-generic-password failed: {}",
            out.stderr.trim()
        )))
    }

    /// [`Keyring::store`] in the macOS keychain: `add-generic-password -U` (replacing an earlier
    /// item) written to `security -i`'s stdin, then read back.
    fn store_macos(&self, key: &ApiKey) -> io::Result<()> {
        // The command is parsed by `security` itself: the key goes inside double quotes, which a
        // quote or a backslash in it would break. Real keys are letters, digits, `-` and `_`.
        if key.expose().contains(['"', '\\']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the key contains a quote or a backslash, which the macOS keychain tool cannot take",
            ));
        }
        let [s, service, a, account] = KEYCHAIN_ITEM;
        let command = format!(
            "add-generic-password -U {s} {service} {a} {account} -l \"{LABEL}\" -w \"{}\"\n",
            key.expose()
        );
        let out = self.run(&["-i"], Some(&command))?;
        match self.lookup_macos() {
            Ok(Some(stored)) if stored == *key => Ok(()),
            Ok(_) => Err(io::Error::other(format!(
                "security add-generic-password failed: {}",
                key.redact(out.stderr.trim())
            ))),
            Err(e) => Err(e),
        }
    }

    /// [`Keyring::clear`] in the macOS keychain.
    fn clear_macos(&self) -> io::Result<()> {
        let mut args = vec!["delete-generic-password"];
        args.extend(KEYCHAIN_ITEM);
        let out = self.run(&args, None)?;
        if out.status.success() || out.status.code() == Some(SECURITY_NOT_FOUND) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "security delete-generic-password failed: {}",
                out.stderr.trim()
            )))
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> io::Result<Output> {
        let mut child = lemma_core::process::command(&self.program)
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
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A failed write shows up as the program's own failure.
            let _ = pipe.write_all(input.as_bytes());
        }
        let stdout = child.stdout.take().map(reader_thread);
        let stderr = child.stderr.take().map(reader_thread);
        let status = wait_with_timeout(&mut child, &self.program, SECRET_TOOL_TIMEOUT)?;
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

fn wait_with_timeout(
    child: &mut Child,
    program: &Path,
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let name = program.file_name().unwrap_or(program.as_os_str());
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} did not answer in time", name.to_string_lossy()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Windows has no execute bit: any file counts.
#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Without the Windows Credential Manager (any other system), the protocol never reaches it.
#[cfg(not(windows))]
mod credentials {
    use super::ApiKey;
    use std::io;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "the Windows Credential Manager exists only on Windows",
        )
    }

    pub(super) fn read(_target: &str) -> io::Result<Option<ApiKey>> {
        Err(unsupported())
    }

    pub(super) fn write(_target: &str, _key: &ApiKey) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn delete(_target: &str) -> io::Result<()> {
        Err(unsupported())
    }
}

/// The key as a generic credential in the Windows Credential Manager (see the module docs).
#[cfg(windows)]
mod credentials {
    use super::{ApiKey, LABEL};
    use std::io;
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };

    /// The credential's user name, which the Credential Manager shows.
    const USER_NAME: &str = "anthropic-api-key";

    /// `text` as a NUL-terminated UTF-16 string.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub(super) fn read(target: &str) -> io::Result<Option<ApiKey>> {
        let target = wide(target);
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is NUL-terminated; on success the system allocates `credential`,
        // which is freed with CredFree below.
        let found =
            unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut credential) } != 0;
        if !found {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                Ok(None)
            } else {
                Err(io::Error::new(
                    e.kind(),
                    format!("the Credential Manager could not be read: {e}"),
                ))
            };
        }
        // SAFETY: CredReadW succeeded, so `credential` points at a valid CREDENTIALW whose blob
        // holds `CredentialBlobSize` bytes; both live until CredFree.
        let key = unsafe {
            let blob = std::slice::from_raw_parts(
                (*credential).CredentialBlob,
                (*credential).CredentialBlobSize as usize,
            );
            let key = String::from_utf8(blob.to_vec()).ok();
            CredFree(credential.cast());
            key
        };
        Ok(key.as_deref().and_then(ApiKey::new))
    }

    pub(super) fn write(target: &str, key: &ApiKey) -> io::Result<()> {
        let mut target = wide(target);
        let mut comment = wide(LABEL);
        let mut user = wide(USER_NAME);
        let mut blob = key.expose().as_bytes().to_vec();
        // SAFETY: an all-zero CREDENTIALW is valid (no attributes, no alias).
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target.as_mut_ptr();
        credential.Comment = comment.as_mut_ptr();
        credential.UserName = user.as_mut_ptr();
        credential.CredentialBlob = blob.as_mut_ptr();
        credential.CredentialBlobSize = u32::try_from(blob.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the key is too long"))?;
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        // SAFETY: every pointer in `credential` points into a buffer that outlives the call.
        let written = unsafe { CredWriteW(&raw const credential, 0) } != 0;
        blob.fill(0);
        if written {
            Ok(())
        } else {
            let e = io::Error::last_os_error();
            Err(io::Error::new(
                e.kind(),
                format!("the Credential Manager could not store the key: {e}"),
            ))
        }
    }

    pub(super) fn delete(target: &str) -> io::Result<()> {
        let target = wide(target);
        // SAFETY: `target` is NUL-terminated.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(io::Error::new(
                e.kind(),
                format!("the Credential Manager could not remove the key: {e}"),
            ))
        }
    }
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

/// [`lookup_with`] for this process: `ANTHROPIC_API_KEY`, then [`Keyring::default`].
pub fn lookup() -> Option<(ApiKey, KeySource)> {
    lookup_with(std::env::var_os(ENV_VAR).as_deref(), &Keyring::default())
}

/// Stores `key` in the GNOME keyring (`secret-tool` on `PATH`; the Keychain on macOS, the
/// Credential Manager on Windows).
pub fn store(key: &ApiKey) -> io::Result<()> {
    Keyring::default().store(key)
}

/// Removes the key from the GNOME keyring (`secret-tool` on `PATH`; the Keychain on macOS, the
/// Credential Manager on Windows).
pub fn clear() -> io::Result<()> {
    Keyring::default().clear()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn the_credential_manager_stores_reads_and_removes_a_key() {
        // A target of this test's own, so a key the user stored is never touched.
        let keyring = Keyring::windows_credentials(&format!("lemma-test/{}", std::process::id()));
        assert!(keyring.available());
        keyring.clear().unwrap();
        assert_eq!(keyring.lookup().unwrap(), None);
        let key = ApiKey::new("sk-ant-test-0123456789").unwrap();
        keyring.store(&key).unwrap();
        assert_eq!(keyring.lookup().unwrap(), Some(key));
        let other = ApiKey::new("sk-ant-test-replaced").unwrap();
        keyring.store(&other).unwrap();
        assert_eq!(keyring.lookup().unwrap(), Some(other));
        keyring.clear().unwrap();
        assert_eq!(keyring.lookup().unwrap(), None);
        keyring.clear().unwrap();
    }
}
