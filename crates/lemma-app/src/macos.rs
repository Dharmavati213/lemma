//! macOS: the process environment, set by `main` before any other thread exists.
//!
//! An app started from Finder, the Dock or `open` gets launchd's environment, which is missing
//! two things:
//!
//! - **The bundle's libraries and data.** `Lemma.app` carries GTK and its dependencies in
//!   `Contents/Resources/lib` (linked through `@executable_path`) and their data in
//!   `Contents/Resources/share`: GSettings schemas, icon themes, GtkSourceView's language files
//!   (see `packaging/macos/`). [`bundle_environment`] points GLib, GTK and gdk-pixbuf at them and
//!   keeps them from loading the modules of a Homebrew installation, which are linked against a
//!   second copy of GLib. Enchant finds its providers by itself, next to its own library
//!   (`Resources/lib/enchant-2`).
//! - **PATH** is `/usr/bin:/bin:/usr/sbin:/sbin`: no `latexmk` (MacTeX puts it in
//!   `/Library/TeX/texbin`), nothing from Homebrew, no `node` and no agent CLIs.
//!   [`complete_path`] adds the login shell's PATH and the usual directories.
//!
//! Started from a terminal, Lemma keeps the terminal's environment and only adds what is missing.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long the login shell may take to print its PATH.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Directories added to PATH when they exist: Homebrew (Apple silicon, then Intel), MacTeX's
/// `texbin` and MacPorts.
const USUAL_DIRS: [&str; 5] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/Library/TeX/texbin",
    "/opt/homebrew/sbin",
    "/opt/local/bin",
];

/// In `Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/`: gdk-pixbuf's loader list with
/// [`RESOURCES_MARK`] where the absolute path of `Contents/Resources` belongs.
const PIXBUF_CACHE_TEMPLATE: &str = "loaders.cache.in";
/// The placeholder in [`PIXBUF_CACHE_TEMPLATE`].
const RESOURCES_MARK: &str = "@RESOURCES@";

/// Sets up the environment (see the module docs). Call it first thing in `main`: it changes
/// environment variables, which is only sound while the process has a single thread.
pub fn prepare_environment() {
    if let Some(resources) = bundle_resources() {
        bundle_environment(&resources);
    }
    complete_path();
    // Without any locale variable the C library's character set is ASCII, for GLib and for the
    // programs Lemma runs (latexmk, git, the agent).
    if ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .all(|name| env::var_os(name).is_none_or(|v| v.is_empty()))
    {
        set_var("LC_CTYPE", "UTF-8");
    }
}

/// `Lemma.app/Contents/Resources` when this executable is `Lemma.app/Contents/MacOS/lemma` and
/// the bundle carries its data (`Resources/share`).
fn bundle_resources() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    if macos_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let resources = contents.join("Resources");
    resources.join("share").is_dir().then_some(resources)
}

/// Points GLib, GTK and gdk-pixbuf at the bundle's data and modules (see the module docs).
fn bundle_environment(resources: &Path) {
    let share = resources.join("share");
    let lib = resources.join("lib");

    // Icon themes, GtkSourceView's languages and styles, GLib's MIME data.
    let mut data_dirs = vec![share.clone()];
    if let Some(old) = env::var_os("XDG_DATA_DIRS") {
        data_dirs.extend(env::split_paths(&old).filter(|d| *d != share));
    }
    if let Ok(joined) = env::join_paths(data_dirs) {
        set_var("XDG_DATA_DIRS", joined);
    }
    set_var("GSETTINGS_SCHEMA_DIR", share.join("glib-2.0/schemas"));
    // GIO and GTK modules (TLS, print backends, media) come only from the bundle, which may have
    // none: those of a Homebrew installation would load Homebrew's GLib next to the bundle's.
    set_var("GIO_MODULE_DIR", lib.join("gio/modules"));
    set_var("GTK_EXE_PREFIX", resources);
    set_var("GTK_DATA_PREFIX", resources);
    // gdk-pixbuf's loader list holds absolute paths, so it is written out for where the bundle
    // is now. Without one, gdk-pixbuf would read Homebrew's: pointing at a missing file instead
    // leaves it without the optional loaders (GTK reads PNG, JPEG and TIFF itself).
    let loaders = pixbuf_loader_cache(resources)
        .unwrap_or_else(|| resources.join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"));
    set_var("GDK_PIXBUF_MODULE_FILE", loaders);
}

/// Writes the bundle's gdk-pixbuf loader list for its present location into Lemma's cache
/// directory (unless it is there already) and returns its path; `None` when there is no
/// template or the file cannot be written.
fn pixbuf_loader_cache(resources: &Path) -> Option<PathBuf> {
    let template_path = resources
        .join("lib/gdk-pixbuf-2.0/2.10.0")
        .join(PIXBUF_CACHE_TEMPLATE);
    let template = fs::read_to_string(&template_path).ok()?;
    let text = template.replace(RESOURCES_MARK, resources.to_str()?);
    let dir = lemma_config::Paths::from_env()?.cache_dir;
    let path = dir.join("gdk-pixbuf-loaders.cache");
    if fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Some(path);
    }
    fs::create_dir_all(&dir).ok()?;
    // Another instance may be writing it too: write a private file and rename it into place.
    let temp = dir.join(format!(".gdk-pixbuf-loaders.cache.{}", std::process::id()));
    fs::write(&temp, text).ok()?;
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        return None;
    }
    Some(path)
}

/// Adds the login shell's PATH (only when not started from a terminal) and the
/// [`USUAL_DIRS`] that exist to PATH. The shell's order comes first, then the inherited entries,
/// then the usual directories; duplicates are dropped.
fn complete_path() {
    let inherited = env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut add = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if env::var_os("TERM").is_none()
        && let Some(shell_path) = login_shell_path()
    {
        env::split_paths(&shell_path).for_each(&mut add);
    }
    env::split_paths(&inherited).for_each(&mut add);
    let home = env::var_os("HOME").map(PathBuf::from);
    let personal = home
        .iter()
        .flat_map(|h| [h.join(".local/bin"), h.join(".cargo/bin")]);
    USUAL_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(personal)
        .filter(|d| d.is_dir())
        .for_each(&mut add);
    if let Ok(joined) = env::join_paths(&dirs)
        && joined != inherited
    {
        set_var("PATH", joined);
    }
}

/// PATH as the user's login shell sets it (`$SHELL -l -i`, which reads `.zprofile` and `.zshrc`,
/// where Homebrew, MacTeX's `path_helper`, nvm and mise add theirs); `None` when the shell does
/// not answer within [`SHELL_TIMEOUT`].
///
/// The shell writes PATH to a file rather than to a pipe: a program started by the shell's
/// start-up files could keep a pipe open and block the read. No thread is started, because the
/// environment is changed afterwards.
fn login_shell_path() -> Option<OsString> {
    let shell = env::var_os("SHELL")
        .filter(|s| Path::new(s).is_absolute())
        .unwrap_or_else(|| OsString::from("/bin/zsh"));
    let out = tempfile::NamedTempFile::new().ok()?;
    let mut child = Command::new(&shell)
        // `printenv` and this redirection work the same in sh, bash, zsh and fish.
        .args([
            "-l",
            "-i",
            "-c",
            "/usr/bin/printenv PATH > \"$LEMMA_PATH_OUT\"",
        ])
        .env("LEMMA_PATH_OUT", out.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own group, so that a shell which hangs is stopped with everything it started.
        .process_group(0)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
                    // SAFETY: killpg only sends a signal; the group is the shell's own.
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                }
                let _ = child.wait();
                return None;
            }
        }
    }
    let text = fs::read(out.path()).ok()?;
    let path = text.strip_suffix(b"\n").unwrap_or(&text);
    use std::os::unix::ffi::OsStrExt as _;
    (!path.is_empty()).then(|| OsStr::from_bytes(path).to_owned())
}

fn set_var(name: &str, value: impl AsRef<OsStr>) {
    // SAFETY: only `prepare_environment` calls this, and `main` calls that before any other
    // thread exists, so nothing reads the environment concurrently.
    unsafe { env::set_var(name, value) };
}
