//! Running latexmk: spawning, cancellation, the timeout, and collecting the results; and the
//! clean-up of a build's files (`latexmk -c`).

use crate::paths::{normalize, resolve};
use crate::runaway::runaway_text;
use crate::{
    BuildRequest, LogReport, clean_command_line, command_line, locate_runaways, parse_blg,
    parse_log,
};
use lemma_core::{DiagSource, Diagnostic, Severity};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Time between SIGTERM and SIGKILL.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(500);
/// How long [`Build::wait`] waits for a cancelled process group to disappear.
const GROUP_EXIT_LIMIT: Duration = Duration::from_millis(1500);
/// Larger logs (a runaway `\loop` printing forever) are parsed only up to this size.
const MAX_LOG_BYTES: u64 = 16 << 20;
/// Sources larger than this are not searched for runaway arguments.
const MAX_SOURCE_BYTES: u64 = 4 << 20;
/// How much of latexmk's own output to show when it fails without a TeX error.
const LATEXMK_TAIL_BYTES: u64 = 4096;
const LATEXMK_TAIL_LINES: usize = 5;

/// The process group of one build: latexmk and every TeX, biber or bibtex it starts. On Windows
/// it is a job object instead ([`lemma_core::process::Job`]).
#[derive(Debug)]
struct Group {
    /// latexmk's pid, which is also the group id.
    pgid: i32,
    #[cfg(windows)]
    job: lemma_core::process::Job,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    /// Set once [`Build::wait`] has reaped latexmk; later cancels do nothing.
    finished: AtomicBool,
}

impl Group {
    #[cfg(unix)]
    fn new(pgid: libc::pid_t) -> Self {
        Group {
            pgid,
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    #[cfg(windows)]
    fn new(pid: u32, job: lemma_core::process::Job) -> Self {
        Group {
            pgid: i32::try_from(pid).unwrap_or(i32::MAX),
            job,
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// SIGTERM to the whole group now, SIGKILL after [`KILL_GRACE`] if anything survived.
    /// Signalling latexmk alone leaves pdflatex running as an orphan. On Windows, which has no
    /// SIGTERM for these programs, the whole job ends at once.
    fn stop(self: &Arc<Self>, timed_out: bool) {
        if self.finished.load(Ordering::SeqCst) || self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        if timed_out {
            self.timed_out.store(true, Ordering::SeqCst);
        }
        #[cfg(windows)]
        self.job.terminate(1);
        #[cfg(unix)]
        self.escalate();
    }

    #[cfg(unix)]
    fn escalate(self: &Arc<Self>) {
        self.signal(libc::SIGTERM);
        let group = Arc::clone(self);
        let escalate = thread::Builder::new()
            .name("lemma-build-kill".into())
            .spawn(move || {
                thread::sleep(KILL_GRACE);
                if group.alive() {
                    group.signal(libc::SIGKILL);
                }
            });
        if escalate.is_err() {
            self.signal(libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    fn signal(&self, signal: libc::c_int) {
        // A group id of 0 or 1 would signal our own group or init; ours is always latexmk's pid.
        if self.pgid > 1 {
            // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
            unsafe {
                libc::killpg(self.pgid, signal);
            }
        }
    }

    /// Whether any process (zombies included) is still in the group.
    #[cfg(unix)]
    fn alive(&self) -> bool {
        if self.pgid <= 1 {
            return false;
        }
        // SAFETY: signal 0 only checks that the group exists; no memory is involved.
        let result = unsafe { libc::killpg(self.pgid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Whether any process is still in the job.
    #[cfg(windows)]
    fn alive(&self) -> bool {
        self.job.has_processes()
    }

    fn wait_until_gone(&self, limit: Duration) {
        let start = Instant::now();
        while self.alive() && start.elapsed() < limit {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Stops a running build from another thread. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Canceller {
    group: Arc<Group>,
}

impl Canceller {
    /// killpg(pgid, SIGTERM); SIGKILL after 500 ms if still alive (on Windows: ends the job).
    /// Idempotent; does nothing once the build has finished.
    pub fn cancel(&self) {
        self.group.stop(false);
    }

    /// The build's process-group id (latexmk's pid).
    pub fn pgid(&self) -> i32 {
        self.group.pgid
    }
}

/// A running latexmk. Dropping it without [`Build::wait`] stops the build.
#[derive(Debug)]
pub struct Build {
    /// `None` only after `Drop` handed the child to a reaper thread.
    child: Option<Child>,
    group: Arc<Group>,
    started: Instant,
    req: BuildRequest,
}

/// How a build ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    /// latexmk exited with code 0.
    Success,
    /// latexmk exited with another code (12 for TeX errors) or was killed by someone else.
    Errors,
    /// [`Canceller::cancel`] stopped it.
    Cancelled,
    /// The request's timeout stopped it.
    TimedOut,
    /// latexmk could not be started.
    SpawnFailed(String),
}

/// Everything the app needs after a build.
#[derive(Clone, Debug)]
pub struct BuildOutcome {
    pub status: BuildStatus,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    /// The parsed `.log`, with the `.blg` diagnostics appended.
    pub report: LogReport,
    /// Set when the log says "Output written" (also on exit code 12).
    pub pdf: Option<PathBuf>,
    /// The stamp of `pdf` when the build ended. A build that wrote no new PDF (latexmk's "All
    /// targets are up-to-date" leaves the last run's log, which still says "Output written")
    /// reports the stamp of the one before.
    pub pdf_stamp: Option<PdfStamp>,
    pub synctex: Option<PathBuf>,
}

impl BuildOutcome {
    /// The outcome for a build whose [`Build::start`] failed.
    pub fn spawn_failed(error: &io::Error) -> Self {
        BuildOutcome::without_run(BuildStatus::SpawnFailed(error.to_string()))
    }

    /// The outcome for a build that was stopped before latexmk built (during [`Build::clean`]).
    pub fn cancelled() -> Self {
        BuildOutcome::without_run(BuildStatus::Cancelled)
    }

    fn without_run(status: BuildStatus) -> Self {
        BuildOutcome {
            status,
            exit_code: None,
            elapsed: Duration::ZERO,
            report: LogReport::default(),
            pdf: None,
            pdf_stamp: None,
            synctex: None,
        }
    }
}

/// Identifies one version of a PDF: its size and its modification time, to the nanosecond.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdfStamp {
    pub len: u64,
    pub modified: SystemTime,
}

impl PdfStamp {
    /// The stamp of `pdf` (blocking: call it off the main thread). `None` when it cannot be
    /// read, and when its modification time is a whole second: a file system that keeps
    /// whole seconds only (FAT, HFS+) would give two builds within a second the same stamp.
    pub fn of(pdf: &Path) -> Option<PdfStamp> {
        let meta = fs::metadata(pdf).ok()?;
        let modified = meta.modified().ok()?;
        let fraction = modified.duration_since(UNIX_EPOCH).ok()?.subsec_nanos();
        (fraction != 0).then_some(PdfStamp {
            len: meta.len(),
            modified,
        })
    }
}

impl Build {
    /// Creates the output and mirror directories, then spawns latexmk ([`command_line`]) in the
    /// root's directory as the leader of a new process group (on Windows: in a new job), with
    /// stdin from /dev/null. Its output goes to `<outdir>/<stem>.latexmk.log`.
    pub fn start(req: &BuildRequest) -> io::Result<Build> {
        let (args, env) = command_line(req);
        Build::launch(req, &args, env)
    }

    /// Blocking: removes what the build of `req` generated (`latexmk -c`, see
    /// [`clean_command_line`]), so that the next build starts from the sources, and waits for it,
    /// enforcing `req.timeout`. It runs as a build does (in the root's directory and its own
    /// process group or job, its output in `<outdir>/<stem>.latexmk.log`); `started` gets its
    /// canceller once it runs. Fails when latexmk cannot run (a missing latexmk is
    /// [`io::ErrorKind::NotFound`]), is cancelled ([`io::ErrorKind::Interrupted`]) or does not
    /// exit with 0.
    pub fn clean(req: &BuildRequest, started: impl FnOnce(Canceller)) -> io::Result<()> {
        let mut clean = Build::launch(req, &clean_command_line(req), Vec::new())?;
        started(clean.canceller());
        let (status, exit_code, _) = clean.wait_for_exit();
        match status {
            BuildStatus::Success => Ok(()),
            BuildStatus::Cancelled => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "latexmk -c was stopped",
            )),
            BuildStatus::TimedOut => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "latexmk -c did not finish within {} s",
                    req.timeout.as_secs()
                ),
            )),
            _ => Err(io::Error::other(match exit_code {
                Some(code) => format!("latexmk -c failed with exit code {code}"),
                None => "latexmk -c was killed by a signal".to_owned(),
            })),
        }
    }

    /// What [`Build::start`] does, with the arguments `args` and the extra environment `env`.
    fn launch(
        req: &BuildRequest,
        args: &[OsString],
        env: Vec<(String, String)>,
    ) -> io::Result<Build> {
        let (root_dir, stem) = root_parts(&req.root)?;
        let out_dir = root_dir.join(&req.outdir);
        fs::create_dir_all(&out_dir)?;
        for dir in &req.mirror_dirs {
            if is_plain_relative(dir) {
                fs::create_dir_all(out_dir.join(dir))?;
            }
        }
        let (stdout, stderr) = match File::create(out_dir.join(file_name(stem, "latexmk.log"))) {
            Ok(file) => match file.try_clone() {
                Ok(copy) => (Stdio::from(file), Stdio::from(copy)),
                Err(_) => (Stdio::from(file), Stdio::null()),
            },
            Err(_) => (Stdio::null(), Stdio::null()),
        };
        let started = Instant::now();
        let mut command = Command::new(&req.latexmk);
        command
            .args(args)
            .envs(env)
            .current_dir(&root_dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        let cannot_run = |e: io::Error| {
            io::Error::new(
                e.kind(),
                format!("cannot run {}: {e}", req.latexmk.display()),
            )
        };
        #[cfg(unix)]
        let (child, group) = {
            let child = command.process_group(0).spawn().map_err(cannot_run)?;
            let pgid = libc::pid_t::try_from(child.id())
                .map_err(|_| io::Error::other("latexmk's pid does not fit in pid_t"))?;
            (child, Group::new(pgid))
        };
        #[cfg(windows)]
        let (child, group) = {
            let (child, job) = lemma_core::process::Job::spawn(&mut command).map_err(cannot_run)?;
            let pid = child.id();
            (child, Group::new(pid, job))
        };
        Ok(Build {
            child: Some(child),
            group: Arc::new(group),
            started,
            req: req.clone(),
        })
    }

    /// A handle that cancels this build from another thread.
    pub fn canceller(&self) -> Canceller {
        Canceller {
            group: Arc::clone(&self.group),
        }
    }

    /// Blocking: waits (enforcing `req.timeout`), then parses `<outdir>/<jobname>.log` and any
    /// `.blg`.
    ///
    /// After a cancel or timeout it also waits (up to 1.5 s) until the whole process group has
    /// exited, so no TeX process is left writing into the output directory. Runaway arguments
    /// are located in the project files listed in the `.fls`, and relative `.blg` paths are
    /// resolved against the root's directory. If latexmk fails without any error in the log or
    /// `.blg`, an error on the root file carries the end of latexmk's own output as context.
    pub fn wait(mut self) -> BuildOutcome {
        let (status, exit_code, elapsed) = self.wait_for_exit();
        self.outcome(status, exit_code, elapsed)
    }

    /// Blocking: waits for latexmk (stopping it at `req.timeout`) and, after a cancel or timeout,
    /// up to 1.5 s for the rest of its process group. Returns how the run ended, latexmk's exit
    /// code (`None` when a signal ended it) and how long it ran.
    fn wait_for_exit(&mut self) -> (BuildStatus, Option<i32>, Duration) {
        let timeout = self.req.timeout.saturating_sub(self.started.elapsed());
        let (done, finished) = mpsc::channel::<()>();
        let group = Arc::clone(&self.group);
        let watchdog = thread::Builder::new()
            .name("lemma-build-timeout".into())
            .spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(timeout) {
                    group.stop(true);
                }
            });
        let exit = self.child.as_mut().and_then(|child| child.wait().ok());
        let elapsed = self.started.elapsed();
        drop(done);
        if let Ok(watchdog) = watchdog {
            let _ = watchdog.join();
        }
        let cancelled = self.group.cancelled.load(Ordering::SeqCst);
        if cancelled {
            self.group.wait_until_gone(GROUP_EXIT_LIMIT);
        }
        self.group.finished.store(true, Ordering::SeqCst);

        let exit_code = exit.and_then(|status| status.code());
        let status = if self.group.timed_out.load(Ordering::SeqCst) {
            BuildStatus::TimedOut
        } else if cancelled {
            BuildStatus::Cancelled
        } else if exit_code == Some(0) {
            BuildStatus::Success
        } else {
            BuildStatus::Errors
        };
        (status, exit_code, elapsed)
    }

    fn outcome(
        &self,
        status: BuildStatus,
        exit_code: Option<i32>,
        elapsed: Duration,
    ) -> BuildOutcome {
        let root = std::path::absolute(&self.req.root).unwrap_or_else(|_| self.req.root.clone());
        let root = normalize(&root);
        let (root_dir, stem) = match root_parts(&root) {
            Ok((dir, stem)) => (dir, stem.to_owned()),
            Err(_) => (PathBuf::new(), root.as_os_str().to_owned()),
        };
        let out_dir = resolve(&root_dir, Path::new(&self.req.outdir));
        let output_file = |ext: &str| out_dir.join(file_name(&stem, ext));

        let mut report = read_capped(&output_file("log"), MAX_LOG_BYTES)
            .map(|log| parse_log(&log, &root_dir, &|p: &Path| root_dir.join(p).is_file()))
            .unwrap_or_default();
        if report.diagnostics.iter().any(|d| runaway_text(d).is_some()) {
            let sources = project_sources(&output_file("fls"), &root, &root_dir, &out_dir);
            locate_runaways(&mut report, &sources);
        }
        let blg = uses_bibliography(&output_file("aux"), &output_file("fls"))
            .then(|| read_capped(&output_file("blg"), MAX_LOG_BYTES))
            .flatten();
        if let Some(blg) = blg {
            let mut diagnostics = parse_blg(&blg, &[]);
            for d in &mut diagnostics {
                if d.path.as_os_str().is_empty() {
                    d.path = root.clone();
                } else if d.path.is_relative() {
                    let path = resolve(&root_dir, &d.path);
                    if path.exists() {
                        d.path = path;
                    }
                }
            }
            report.diagnostics.extend(diagnostics);
        }
        let has_error = report
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error);
        if status == BuildStatus::Errors && !has_error {
            let tail = output_tail(&output_file("latexmk.log"));
            report
                .diagnostics
                .push(latexmk_failure(root.clone(), exit_code, tail));
        }
        let pdf = report
            .output
            .as_ref()
            .map(|(path, _)| path.with_extension("pdf"))
            .filter(|path| path.is_file());
        let pdf_stamp = pdf.as_deref().and_then(PdfStamp::of);
        let synctex = pdf
            .as_ref()
            .map(|path| path.with_extension("synctex.gz"))
            .filter(|path| path.is_file());
        BuildOutcome {
            status,
            exit_code,
            elapsed,
            report,
            pdf,
            pdf_stamp,
            synctex,
        }
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        if self.group.finished.load(Ordering::SeqCst) {
            return;
        }
        self.group.stop(false);
        if let Some(mut child) = self.child.take() {
            let _ = thread::Builder::new()
                .name("lemma-build-reap".into())
                .spawn(move || child.wait());
        }
    }
}

/// The root's directory (`.` for a bare file name) and its file stem.
fn root_parts(root: &Path) -> io::Result<(PathBuf, &OsStr)> {
    let stem = root.file_stem().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file: {}", root.display()),
        )
    })?;
    let dir = match root.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, stem))
}

/// `<stem>.<ext>`; the stem may itself contain dots.
fn file_name(stem: &OsStr, ext: &str) -> PathBuf {
    let mut name = stem.to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// A mirror directory must stay inside the output directory.
fn is_plain_relative(dir: &Path) -> bool {
    dir.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Reads at most `cap` bytes of `path`, replacing invalid UTF-8 (logs may contain any bytes).
/// Only regular files are read (symlinks are followed): a FIFO would block, and a device such as
/// /dev/zero never ends.
pub(crate) fn read_capped(path: &Path, cap: u64) -> Option<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    // O_NONBLOCK: opening a FIFO for reading would otherwise wait for a writer. It does not
    // change reads from a regular file. (Windows has no FIFOs in the file system.)
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether this run used a bibliography, so that the `.blg` belongs to it: the `.aux` asks BibTeX
/// for a database (`\bibdata`), or the `.fls` shows biblatex writing its `.bcf` or the document
/// reading its `.bbl`. latexmk never deletes an old `.blg`, so after the bibliography is removed
/// the file stays behind with the last run's messages.
fn uses_bibliography(aux: &Path, fls: &Path) -> bool {
    let bibdata = read_capped(aux, MAX_LOG_BYTES).is_some_and(|aux| aux.contains("\\bibdata{"));
    bibdata
        || read_capped(fls, MAX_LOG_BYTES).is_some_and(|fls| {
            fls.lines().any(|line| {
                let ext = |prefix: &str, ext: &str| {
                    line.strip_prefix(prefix)
                        .is_some_and(|p| Path::new(p).extension().is_some_and(|e| e == ext))
                };
                ext("OUTPUT ", "bcf") || ext("INPUT ", "bbl")
            })
        })
}

/// The root plus the project files (`.tex`, `.sty`, `.cls`, `.ltx` under the root's directory,
/// outside the output directory) that the `.fls` lists as inputs, with their text. When the
/// output directory is the root's directory (`outdir = "."`) or above it, it holds the sources
/// too, and only the extension tells them apart.
fn project_sources(
    fls: &Path,
    root: &Path,
    root_dir: &Path,
    out_dir: &Path,
) -> Vec<(PathBuf, String)> {
    let out_dir_holds_sources = root_dir.starts_with(out_dir);
    let mut paths = vec![root.to_path_buf()];
    if let Some(fls) = read_capped(fls, MAX_LOG_BYTES) {
        let pwd = fls
            .lines()
            .find_map(|l| l.strip_prefix("PWD "))
            .map_or_else(|| root_dir.to_path_buf(), PathBuf::from);
        for input in fls.lines().filter_map(|l| l.strip_prefix("INPUT ")) {
            let path = resolve(&pwd, Path::new(input));
            let is_source = path
                .extension()
                .is_some_and(|e| ["tex", "sty", "cls", "ltx"].iter().any(|s| e == *s));
            if is_source
                && path.starts_with(root_dir)
                && (out_dir_holds_sources || !path.starts_with(out_dir))
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let text = read_capped(&path, MAX_SOURCE_BYTES)?;
            Some((path, text))
        })
        .collect()
}

/// The last few non-empty lines of latexmk's output.
fn output_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(LATEXMK_TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(LATEXMK_TAIL_LINES)..].join("\n");
    (!tail.is_empty()).then_some(tail)
}

fn latexmk_failure(root: PathBuf, exit_code: Option<i32>, tail: Option<String>) -> Diagnostic {
    let message = match exit_code {
        Some(code) => format!("latexmk failed with exit code {code}"),
        None => "latexmk was killed by a signal".to_owned(),
    };
    Diagnostic {
        path: root,
        line: None,
        severity: Severity::Error,
        message,
        context: tail,
        source: DiagSource::Tex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_stamps_tell_versions_apart_but_not_within_a_whole_second() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = dir.path().join("main.pdf");
        assert_eq!(PdfStamp::of(&pdf), None, "no file");
        fs::write(&pdf, "%PDF-1.5 one").unwrap();
        let at = |nanos| UNIX_EPOCH + Duration::new(1_800_000_000, nanos);
        let touch = |nanos| {
            let file = File::options().write(true).open(&pdf).unwrap();
            file.set_modified(at(nanos)).unwrap();
        };
        touch(250_000_000);
        let first = PdfStamp::of(&pdf).expect("a stamp");
        assert_eq!(
            first,
            PdfStamp {
                len: 12,
                modified: at(250_000_000)
            }
        );
        assert_eq!(PdfStamp::of(&pdf), Some(first), "an untouched file");
        // The same size a microsecond later (NTFS keeps 100 ns), or another size at the same time.
        touch(250_001_000);
        assert_ne!(PdfStamp::of(&pdf), Some(first));
        fs::write(&pdf, "%PDF-1.5 two!").unwrap();
        touch(250_000_000);
        assert_ne!(PdfStamp::of(&pdf), Some(first));
        // Whole seconds: perhaps a file system that keeps no fractions.
        touch(0);
        assert_eq!(PdfStamp::of(&pdf), None);
    }

    #[test]
    fn root_parts_of_bare_and_nested_names() {
        let (dir, stem) = root_parts(Path::new("main.tex")).expect("parts");
        assert_eq!((dir.as_path(), stem), (Path::new("."), OsStr::new("main")));
        let (dir, stem) = root_parts(Path::new("/p/my.thesis.tex")).expect("parts");
        assert_eq!(
            (dir.as_path(), stem),
            (Path::new("/p"), OsStr::new("my.thesis"))
        );
        assert!(root_parts(Path::new("/")).is_err());
        assert_eq!(
            file_name(stem, "synctex.gz"),
            Path::new("my.thesis.synctex.gz")
        );
    }

    #[test]
    fn mirror_dirs_stay_inside() {
        assert!(is_plain_relative(Path::new("chapters/part1")));
        assert!(is_plain_relative(Path::new("./chapters")));
        assert!(!is_plain_relative(Path::new("../shared")));
        assert!(!is_plain_relative(Path::new("/tmp/x")));
    }

    #[test]
    fn start_reports_a_missing_latexmk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let req = BuildRequest {
            root: dir.path().join("main.tex"),
            engine: crate::Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: vec![PathBuf::from("chapters"), PathBuf::from("../escape")],
            timeout: Duration::from_secs(5),
            latexmk: dir.path().join("no-such-latexmk"),
        };
        let err = Build::start(&req).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with("cannot run "), "{err}");
        // The directories are created before the spawn; the escaping one is not.
        assert!(dir.path().join(".build/chapters").is_dir());
        assert!(!dir.path().join("escape").exists());
        let outcome = BuildOutcome::spawn_failed(&err);
        assert!(
            matches!(outcome.status, BuildStatus::SpawnFailed(ref m) if m.starts_with("cannot run "))
        );
    }

    #[test]
    fn clean_reports_a_missing_latexmk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let req = BuildRequest {
            root: dir.path().join("main.tex"),
            engine: crate::Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(5),
            latexmk: dir.path().join("no-such-latexmk"),
        };
        let err = Build::clean(&req, |_| {}).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().starts_with("cannot run "), "{err}");
    }

    /// `Build::clean` runs latexmk with the clean-up's arguments in the root's directory, reports
    /// an exit code other than 0, and can be stopped.
    #[cfg(unix)]
    #[test]
    fn clean_runs_latexmk_c_in_the_root_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let latexmk = dir.path().join("fake-latexmk");
        // Copied into place by `cp`: had this process written it, a child forked meanwhile by a
        // test on another thread could still hold it open for writing ("Text file busy").
        let source = dir.path().join("fake-latexmk.src");
        fs::write(
            &source,
            "#!/bin/sh\npwd -P > args.txt\necho \"$*\" >> args.txt\n[ -f fail ] && exit 3\n\
             [ -f slow ] && sleep 30\nexit 0\n",
        )
        .expect("write");
        let copied = Command::new("cp").arg(&source).arg(&latexmk).status();
        assert!(copied.is_ok_and(|s| s.success()));
        fs::set_permissions(&latexmk, fs::Permissions::from_mode(0o755)).expect("chmod");
        let req = BuildRequest {
            root: dir.path().join("main.tex"),
            engine: crate::Engine::LuaLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(20),
            latexmk,
        };
        Build::clean(&req, |_| {}).expect("clean");
        let args = fs::read_to_string(dir.path().join("args.txt")).expect("args");
        let cwd = fs::canonicalize(dir.path()).expect("canonical");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                cwd.to_str().expect("utf-8 path"),
                "-lualatex -bibtex-cond1 -c -outdir=.build ./main.tex"
            ]
        );
        fs::write(dir.path().join("fail"), "").expect("write");
        let err = Build::clean(&req, |_| {}).expect_err("exit code 3");
        assert_eq!(err.to_string(), "latexmk -c failed with exit code 3");
        fs::remove_file(dir.path().join("fail")).expect("remove");
        fs::write(dir.path().join("slow"), "").expect("write");
        let started = Instant::now();
        let err = Build::clean(&req, |canceller| canceller.cancel()).expect_err("stopped");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_tail_keeps_the_last_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let text: String = (1..=20).map(|i| format!("line {i}\n\n")).collect();
        fs::write(&path, text).expect("write");
        assert_eq!(
            output_tail(&path).as_deref(),
            Some("line 16\nline 17\nline 18\nline 19\nline 20")
        );
        assert_eq!(output_tail(&dir.path().join("missing")), None);
    }
}
