//! Running a TeX process: in its own process group (on Windows: a job), with stdin from
//! `/dev/null` and its output discarded (TeX writes a log), killed with the whole group when it
//! outlives its timeout, or when the application ends ([`stop_all`]).

use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Why a run did not finish.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The program could not be started (not installed).
    Spawn(io::Error),
    /// It ran longer than its timeout and was killed.
    Timeout,
    /// [`stop_all`] killed it (or it was asked for afterwards).
    Stopped,
}

/// Set by [`stop_all`]: no TeX runs any more.
static STOPPED: AtomicBool = AtomicBool::new(false);

/// The process groups of the runs going on (their leaders' pids), for [`stop_all`]. A run takes
/// its own out under the lock as it reaps its process, so a group is never signalled after its
/// pid could be another process's.
#[cfg(unix)]
static RUNNING: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[cfg(unix)]
fn running() -> std::sync::MutexGuard<'static, Vec<u32>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Kills every TeX run going on, with its process group, and refuses the runs asked for after
/// (see the crate's `stop_all`).
pub(crate) fn stop_all() {
    STOPPED.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    for &pid in running().iter() {
        kill_group(pid);
    }
}

/// Whether [`stop_all`] ran.
pub(crate) fn stopped() -> bool {
    STOPPED.load(Ordering::SeqCst)
}

/// The separator of kpathsea's path lists: `;` on Windows, where `:` belongs to drive names.
pub(crate) const LIST_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// Runs `program args…` in `cwd` with `texinputs` prepended to the TeX search path, waiting at
/// most `timeout`.
pub(crate) fn run(
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
    texinputs: &OsStr,
    timeout: Duration,
) -> Result<ExitStatus, RunError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("TEXINPUTS", texinputs)
        // Unwrapped log lines, as for builds (the log parser expects them).
        .env("max_print_line", "10000")
        .env("error_line", "254")
        .env("half_error_line", "238")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    let mut child = {
        // Under the lock, so that `stop_all` either refuses the run or kills it.
        let mut running = running();
        if stopped() {
            return Err(RunError::Stopped);
        }
        let child = command.process_group(0).spawn().map_err(RunError::Spawn)?;
        running.push(child.id());
        child
    };
    #[cfg(windows)]
    let (mut child, job) = {
        if stopped() {
            return Err(RunError::Stopped);
        }
        lemma_core::process::Job::spawn(&mut command).map_err(RunError::Spawn)?
    };
    // Kills the group and reaps the process (out of `RUNNING` first: it is not reaped yet, so
    // `stop_all` cannot meet its pid reused meanwhile).
    let end = |child: &mut std::process::Child| {
        #[cfg(unix)]
        {
            kill_group(child.id());
            running().retain(|&pid| pid != child.id());
        }
        #[cfg(windows)]
        job.terminate(1);
        let _ = child.wait();
    };
    let deadline = Instant::now() + timeout;
    // TeX runs take tens of milliseconds: poll finely at first, then less often.
    let mut nap = Duration::from_micros(500);
    loop {
        #[cfg(unix)]
        let polled = {
            let mut running = running();
            let polled = child.try_wait();
            if !matches!(polled, Ok(None)) {
                running.retain(|&pid| pid != child.id());
            }
            polled
        };
        #[cfg(windows)]
        let polled = child.try_wait();
        match polled {
            Ok(Some(_)) if stopped() => return Err(RunError::Stopped),
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                end(&mut child);
                return Err(RunError::Spawn(e));
            }
        }
        if stopped() {
            end(&mut child);
            return Err(RunError::Stopped);
        }
        let now = Instant::now();
        if now >= deadline {
            end(&mut child);
            return Err(RunError::Timeout);
        }
        std::thread::sleep(nap.min(deadline - now));
        nap = (nap * 2).min(Duration::from_millis(4));
    }
}

/// SIGKILL to the process group `pid` leads (TeX starts no children here, but a package may).
#[cfg(unix)]
fn kill_group(pid: u32) {
    if let Ok(pgid) = libc::pid_t::try_from(pid) {
        // SAFETY: killpg only sends a signal; the group is the child's own (process_group(0)).
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// The TeX search path for a run whose own files are in the working directory and whose
/// project lives in `project_dir`: `.`, the project directory (unless its name would confuse
/// kpathsea), then what `TEXINPUTS` already says (or the default path). The entries are separated
/// by [`LIST_SEPARATOR`].
pub(crate) fn texinputs(project_dir: Option<&Path>, inherited: Option<&OsStr>) -> OsString {
    let mut s = OsString::from(".");
    s.push(LIST_SEPARATOR);
    if let Some(dir) = project_dir {
        let text = dir.as_os_str().as_encoded_bytes();
        // On Windows a colon belongs to the drive name, not to the list.
        let unsafe_for_kpathsea = text.iter().any(|b| {
            matches!(b, b';' | b'$' | b'{' | b'}' | b',' | b'!') || (*b == b':' && !cfg!(windows))
        });
        if !unsafe_for_kpathsea && dir.is_absolute() {
            s.push(dir.as_os_str());
            s.push(LIST_SEPARATOR);
        }
    }
    if let Some(inherited) = inherited.filter(|t| !t.is_empty()) {
        s.push(inherited);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_texinputs_are_separated_by_semicolons() {
        assert_eq!(texinputs(None, None), OsString::from(".;"));
        assert_eq!(
            texinputs(
                Some(Path::new(r"C:\Users\u\thesis")),
                Some(OsStr::new("D:/tex//;"))
            ),
            OsString::from(r".;C:\Users\u\thesis;D:/tex//;")
        );
        assert_eq!(
            texinputs(Some(Path::new(r"C:\Users\u\a;b")), None),
            OsString::from(".;"),
            "a semicolon would split the path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn texinputs_keep_the_project_and_the_default() {
        assert_eq!(texinputs(None, None), OsString::from(".:"));
        assert_eq!(
            texinputs(Some(Path::new("/home/u/thesis")), None),
            OsString::from(".:/home/u/thesis:")
        );
        assert_eq!(
            texinputs(
                Some(Path::new("/home/u/thesis")),
                Some(OsStr::new("/opt/tex//:"))
            ),
            OsString::from(".:/home/u/thesis:/opt/tex//:")
        );
        assert_eq!(
            texinputs(Some(Path::new("/home/u/a:b")), None),
            OsString::from(".:"),
            "a colon would split the path"
        );
        assert_eq!(
            texinputs(Some(Path::new("rel")), None),
            OsString::from(".:")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_process_is_killed_at_its_timeout() {
        let dir = crate::test_dir();
        let started = Instant::now();
        let r = run(
            OsStr::new("sh"),
            &["-c".into(), "sleep 5 & sleep 5".into()],
            dir.path(),
            OsStr::new(".:"),
            Duration::from_millis(200),
        );
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let dir = crate::test_dir();
        let r = run(
            OsStr::new("lemma-no-such-program"),
            &[],
            dir.path(),
            OsStr::new(".:"),
            Duration::from_secs(1),
        );
        assert!(matches!(r, Err(RunError::Spawn(_))), "{r:?}");
    }

    #[cfg(windows)]
    #[test]
    fn a_slow_windows_process_is_killed_at_its_timeout() {
        let dir = crate::test_dir();
        let started = Instant::now();
        let r = run(
            OsStr::new("cmd"),
            &[
                "/c".into(),
                "ping".into(),
                "-n".into(),
                "6".into(),
                "127.0.0.1".into(),
            ],
            dir.path(),
            OsStr::new(".;"),
            Duration::from_millis(200),
        );
        assert!(matches!(r, Err(RunError::Timeout)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn exit_statuses_come_back() {
        let dir = crate::test_dir();
        let r = run(
            OsStr::new("sh"),
            &["-c".into(), "test \"$TEXINPUTS\" = '.:' && exit 3".into()],
            dir.path(),
            OsStr::new(".:"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(r.code(), Some(3));
    }
}
