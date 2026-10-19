//! Running a TeX process: in its own process group, with stdin from `/dev/null` and its output
//! discarded (TeX writes a log), killed with the whole group when it outlives its timeout.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Why a run did not finish.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The program could not be started (not installed).
    Spawn(io::Error),
    /// It ran longer than its timeout and was killed.
    Timeout,
}

/// Runs `program args…` in `cwd` with `texinputs` prepended to the TeX search path, waiting at
/// most `timeout`.
pub(crate) fn run(
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
    texinputs: &OsStr,
    timeout: Duration,
) -> Result<ExitStatus, RunError> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("TEXINPUTS", texinputs)
        // Unwrapped log lines, as for builds (the log parser expects them).
        .env("max_print_line", "10000")
        .env("error_line", "254")
        .env("half_error_line", "238")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(RunError::Spawn)?;
    let deadline = Instant::now() + timeout;
    // TeX runs take tens of milliseconds: poll finely at first, then less often.
    let mut nap = Duration::from_micros(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                kill_group(child.id());
                let _ = child.wait();
                return Err(RunError::Spawn(e));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            kill_group(child.id());
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        std::thread::sleep(nap.min(deadline - now));
        nap = (nap * 2).min(Duration::from_millis(4));
    }
}

/// SIGKILL to the process group `pid` leads (TeX starts no children here, but a package may).
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
/// kpathsea), then what `TEXINPUTS` already says (or the default path).
pub(crate) fn texinputs(project_dir: Option<&Path>, inherited: Option<&OsStr>) -> OsString {
    let mut s = OsString::from(".:");
    if let Some(dir) = project_dir {
        let text = dir.as_os_str().as_encoded_bytes();
        let unsafe_for_kpathsea = text
            .iter()
            .any(|b| matches!(b, b':' | b';' | b'$' | b'{' | b'}' | b',' | b'!'));
        if !unsafe_for_kpathsea && dir.is_absolute() {
            s.push(dir.as_os_str());
            s.push(":");
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
