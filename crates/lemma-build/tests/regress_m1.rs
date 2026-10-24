//! Regression tests for verified M1 defects in the build pipeline (review area B). Each test
//! fails until its defect is fixed. Tests that run latexmk print `SKIP` and pass when it is not
//! installed.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

#[cfg(unix)]
use lemma_build::RootSource;
use lemma_build::{
    Build, BuildOutcome, BuildRequest, BuildStatus, Engine, command_line, detect_root,
};
use lemma_core::DiagSource;
use lemma_syntax::{ProjectIndex, index_file};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

fn have_latexmk() -> bool {
    let found = Command::new("latexmk")
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        eprintln!("SKIP: latexmk not found");
    }
    found
}

fn request(root: PathBuf) -> BuildRequest {
    BuildRequest {
        root,
        engine: Engine::PdfLatex,
        outdir: ".build".into(),
        mirror_dirs: Vec::new(),
        timeout: Duration::from_secs(120),
        latexmk: PathBuf::from("latexmk"),
    }
}

/// Writes `files` (relative path, contents) into a new temporary directory.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, text).expect("write source");
    }
    dir
}

fn build(req: &BuildRequest) -> BuildOutcome {
    Build::start(req).expect("start latexmk").wait()
}

const SIMPLE_DOC: &str = "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n";

// ----- B1: the root's file name reaches latexmk as an option -----

/// `command_line` passes the root's bare file name last. latexmk parses every argument that
/// starts with `-` as an option, so a root named `-notes.tex` is never compiled, and a name such
/// as `-pdflatex=<cmd>.tex` sets the command latexmk runs.
#[test]
fn regress_root_file_name_is_never_a_latexmk_option() {
    for name in ["-notes.tex", "-pdflatex=touch INJECTED;true.tex"] {
        let (args, _) = command_line(&request(PathBuf::from("/p").join(name)));
        let last = args.last().expect("an argument").to_string_lossy();
        assert!(
            !last.starts_with('-'),
            "the root {name:?} is passed as {last:?}, which latexmk parses as an option: {args:?}"
        );
    }
}

/// End to end: a project whose main document is `-notes.tex` builds.
#[test]
fn regress_root_with_leading_dash_builds() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[("-notes.tex", SIMPLE_DOC)]);
    let outcome = build(&request(dir.path().join("-notes.tex")));
    let tail = fs::read_to_string(dir.path().join(".build/-notes.latexmk.log")).unwrap_or_default();
    assert_eq!(
        outcome.status,
        BuildStatus::Success,
        "latexmk output:\n{tail}"
    );
    assert!(outcome.pdf.is_some(), "no PDF; latexmk output:\n{tail}");
}

/// End to end: a root file name in the form of a value-taking latexmk option runs a command.
#[test]
fn regress_root_file_name_cannot_inject_a_command() {
    if !have_latexmk() {
        return;
    }
    let name = "-pdflatex=touch INJECTED;true.tex";
    let dir = project(&[(name, SIMPLE_DOC)]);
    let _ = build(&request(dir.path().join(name)));
    assert!(
        !dir.path().join("INJECTED").exists(),
        "latexmk ran `touch INJECTED` taken from the root's file name"
    );
}

// ----- B5: outdir = "." makes every source count as generated -----

/// With `outdir = "."` the output directory is the root's directory, so `project_sources` drops
/// every source but the root, and a runaway argument in an `\input` file is no longer located.
#[test]
fn regress_outdir_dot_still_locates_runaways_in_input_files() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nIntro.\n\\input{sec}\n\\end{document}\n",
        ),
        (
            "sec.tex",
            "Some text.\nMore \\emph{unclosed text that runs on\nuntil the file ends.\n",
        ),
    ]);
    let mut req = request(dir.path().join("main.tex"));
    req.outdir = ".".into();
    let outcome = build(&req);
    assert_eq!(outcome.status, BuildStatus::Errors);
    let runaway = outcome
        .report
        .diagnostics
        .iter()
        .find(|d| {
            d.context
                .as_deref()
                .is_some_and(|c| c.starts_with("Runaway argument?"))
        })
        .unwrap_or_else(|| panic!("no runaway error: {:?}", outcome.report.diagnostics));
    assert!(
        runaway.path.ends_with("sec.tex") && runaway.line == Some(2),
        "the runaway in sec.tex:2 was reported at {}:{:?}",
        runaway.path.display(),
        runaway.line
    );
}

// ----- B8: a stale .blg is parsed after every later build -----

#[test]
fn regress_stale_blg_is_not_reported_after_the_bibliography_is_removed() {
    if !have_latexmk() {
        return;
    }
    let with_bib = "\\documentclass{article}\n\\begin{document}\nSee \\cite{missing}.\n\
                    \\bibliographystyle{plain}\n\\bibliography{refs}\n\\end{document}\n";
    let dir = project(&[
        ("main.tex", with_bib),
        (
            "refs.bib",
            "@book{other, author = {A. Author}, title = {T}, publisher = {P}, year = {2000}}\n",
        ),
    ]);
    let root = dir.path().join("main.tex");
    let first = build(&request(root.clone()));
    let bibtex = |o: &BuildOutcome| -> Vec<String> {
        o.report
            .diagnostics
            .iter()
            .filter(|d| d.source == DiagSource::Bibtex)
            .map(|d| format!("{:?} {}", d.severity, d.message))
            .collect()
    };
    assert!(
        !bibtex(&first).is_empty(),
        "precondition: the first build has BibTeX warnings: {:?}",
        first.report.diagnostics
    );

    // The citation and the bibliography are gone: BibTeX no longer runs.
    fs::write(&root, SIMPLE_DOC).expect("rewrite main.tex");
    let second = build(&request(root));
    assert_eq!(second.status, BuildStatus::Success, "{:?}", second.report);
    assert!(
        bibtex(&second).is_empty(),
        "the second build (no bibliography) still reports the old .blg: {:?}",
        bibtex(&second)
    );
}

// ----- B15: unbounded reads during root detection -----

/// Runs `f` on a thread; `None` if it has not returned after `limit`.
#[cfg(unix)]
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok()
}

/// Opens `fifo` for writing without blocking and closes it, so a reader blocked in `open`
/// sees end of file.
#[cfg(unix)]
fn release_fifo(fifo: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo);
}

#[cfg(unix)]
fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo {}", path.display());
}

/// The `.fls` rule reads every `*.fls` in the file's directory and three ancestors with an
/// unbounded `read_to_string` and no regular-file check. A FIFO named `*.fls` blocks root
/// detection forever (a symlink to /dev/zero exhausts memory instead, see the next test).
#[test]
#[cfg(unix)]
fn regress_fifo_fls_does_not_block_root_detection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = lemma_core::canonicalize(dir.path()).expect("canonical");
    let file = d.join("chapter.tex");
    fs::write(&file, "\\section{A}\n").expect("write");
    let fifo = d.join("old.fls");
    mkfifo(&fifo);
    let target = file.clone();
    let result = within(Duration::from_secs(10), move || {
        detect_root(&target, "\\section{A}\n", &ProjectIndex::default(), None)
    });
    // Unblock the detection thread whatever happened.
    release_fifo(&fifo);
    let guess = result.expect("detect_root blocked for 10 s reading the FIFO old.fls");
    assert_eq!(guess.source, RootSource::Unknown);
}

/// A symlink named `*.fls` pointing at /dev/zero: `read_to_string` reads NUL bytes (valid UTF-8)
/// without any bound. The detection runs in a child process whose address space is capped at
/// 2 GiB (so the test cannot exhaust the machine's memory); std then reports the failed
/// allocation as an error, which `fls_root` skips, so the child survives. Its peak RSS shows
/// how much was read: a bounded read stays far below 256 MiB.
#[test]
#[cfg(unix)]
fn regress_dev_zero_fls_does_not_exhaust_memory() {
    if let Some(dir) = std::env::var_os("LEMMA_REGRESS_DEV_ZERO_CHILD") {
        let file = PathBuf::from(dir).join("chapter.tex");
        let guess = detect_root(&file, "\\section{A}\n", &ProjectIndex::default(), None);
        assert_eq!(guess.source, RootSource::Unknown);
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let d = lemma_core::canonicalize(dir.path()).expect("canonical");
    fs::write(d.join("chapter.tex"), "\\section{A}\n").expect("write");
    std::os::unix::fs::symlink("/dev/zero", d.join("zero.fls")).expect("symlink");
    let exe = std::env::current_exe().expect("test binary");
    let child = {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "regress_dev_zero_fls_does_not_exhaust_memory",
            "--test-threads=1",
        ])
        .env("LEMMA_REGRESS_DEV_ZERO_CHILD", &d)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        // SAFETY: setrlimit is async-signal-safe and touches no memory of the parent.
        unsafe {
            cmd.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 2 << 30,
                    rlim_max: 2 << 30,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn the child test")
    };
    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    let start = Instant::now();
    // SAFETY: plain C structs, zero-initialised, filled in by wait4.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let mut status = 0;
    loop {
        // SAFETY: `status` and `usage` are valid for writes; `pid` is our child.
        let reaped = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
        if reaped == pid {
            break;
        }
        if start.elapsed() > Duration::from_secs(60) {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("root detection ran for 60 s reading /dev/zero");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let peak_mib = usage.ru_maxrss / 1024;
    assert!(
        peak_mib < 256,
        "root detection with zero.fls -> /dev/zero peaked at {peak_mib} MiB RSS (address space \
         capped at 2 GiB; exit status {status:#x})"
    );
}

// ----- B18: a .bib file never finds the document that uses it -----

#[test]
fn regress_bib_file_finds_the_document_that_uses_it() {
    let mut idx = ProjectIndex::default();
    let bibtex_doc = "\\documentclass{article}\n\\begin{document}\nSee \\cite{knuth}.\n\
                      \\bibliographystyle{plain}\n\\bibliography{refs}\n\\end{document}\n";
    let biblatex_doc = "\\documentclass{article}\n\\usepackage{biblatex}\n\
                        \\addbibresource{lit.bib}\n\\begin{document}\nSee \\cite{knuth}.\n\
                        \\printbibliography\n\\end{document}\n";
    idx.update_file(
        PathBuf::from("/p/main.tex"),
        Arc::new(index_file(bibtex_doc)),
    );
    idx.update_file(
        PathBuf::from("/q/thesis.tex"),
        Arc::new(index_file(biblatex_doc)),
    );
    let bib = "@book{knuth, author = {D. Knuth}, title = {TAOCP}, year = {1968}}\n";
    for (file, root) in [
        ("/p/refs.bib", "/p/main.tex"),
        ("/q/lit.bib", "/q/thesis.tex"),
    ] {
        let guess = detect_root(Path::new(file), bib, &idx, None);
        assert_eq!(
            guess.root.as_deref(),
            Some(Path::new(root)),
            "{file} is the bibliography of {root}: {guess:?}"
        );
    }
}
