//! End-to-end builds with the real latexmk, each in its own temporary directory. Every test
//! prints `SKIP` and passes when latexmk is not installed.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use lemma_build::{Build, BuildOutcome, BuildRequest, BuildStatus, Engine, PdfStamp};
use lemma_core::Severity;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

fn request(root: PathBuf, timeout: Duration) -> BuildRequest {
    BuildRequest {
        root,
        engine: Engine::PdfLatex,
        outdir: ".build".into(),
        mirror_dirs: Vec::new(),
        timeout,
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

fn errors(outcome: &BuildOutcome) -> Vec<String> {
    outcome
        .report
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| format!("{}:{:?} {}", d.path.display(), d.line, d.message))
        .collect()
}

/// The command names of the processes in group `pgid`, from /proc.
#[cfg(unix)]
fn group_members(pgid: i32) -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp …"; comm may contain spaces and parentheses.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let pgrp = stat[close + 1..].split_whitespace().nth(2);
        if pgrp.and_then(|g| g.parse::<i32>().ok()) == Some(pgid) {
            members.push(stat[open + 1..close].to_owned());
        }
    }
    members
}

/// Whether the process group no longer exists: kill(-pgid, 0) fails with ESRCH.
#[cfg(unix)]
fn group_gone(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    let result = unsafe { libc::kill(-pgid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(unix)]
fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[cfg(unix)]
const INFINITE_LOOP: &str =
    "\\documentclass{article}\n\\begin{document}\nx\\loop\\iftrue\\repeat\n\\end{document}\n";

/// Waits until pdflatex runs inside the build's group, so cancelling really has an orphan to
/// leave behind if only latexmk were signalled.
#[cfg(unix)]
fn wait_for_tex(pgid: i32) {
    let running = wait_for(Duration::from_secs(60), || {
        group_members(pgid)
            .iter()
            .any(|c| c == "pdflatex" || c == "pdftex")
    });
    assert!(
        running,
        "pdflatex never started in group {pgid}: {:?}",
        group_members(pgid)
    );
}

#[test]
fn proj_builds_with_pdf_and_synctex() {
    if !have_latexmk() {
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proj");
    let files: Vec<(&str, String)> = ["main.tex", "refs.bib", "chapters/yoneda.tex"]
        .iter()
        .map(|f| (*f, fs::read_to_string(fixture.join(f)).expect("fixture")))
        .collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(f, t)| (*f, t.as_str())).collect();
    let dir = project(&files);
    let root = lemma_core::canonicalize(dir.path())
        .expect("canonical")
        .join("main.tex");
    let outcome = build(&request(root.clone(), Duration::from_secs(180)));

    assert_eq!(outcome.status, BuildStatus::Success, "{:?}", outcome.report);
    assert_eq!(outcome.exit_code, Some(0));
    let out = root.with_file_name(".build");
    assert_eq!(outcome.pdf.as_deref(), Some(out.join("main.pdf").as_path()));
    assert_eq!(
        outcome.synctex.as_deref(),
        Some(out.join("main.synctex.gz").as_path())
    );
    assert_eq!(outcome.report.output, Some((out.join("main.pdf"), 5)));
    assert!(errors(&outcome).is_empty(), "{:?}", errors(&outcome));
    // The real file-system check attributes the class warning to the root file.
    assert!(
        outcome
            .report
            .diagnostics
            .iter()
            .any(|d| d.path == root && d.line == Some(32))
    );
    assert!(out.join("main.latexmk.log").is_file());
    assert!(!outcome.report.fatal);
}

#[test]
fn an_up_to_date_build_reports_the_stamp_of_the_pdf_before() {
    if !have_latexmk() {
        return;
    }
    let text = |body: &str| {
        format!("\\documentclass{{article}}\n\\begin{{document}}\n{body}\n\\end{{document}}\n")
    };
    let dir = project(&[("main.tex", &text("Hello."))]);
    let req = request(dir.path().join("main.tex"), Duration::from_secs(120));
    let first = build(&req);
    assert_eq!(first.status, BuildStatus::Success, "{:?}", errors(&first));
    let stamp = first.pdf_stamp.expect("a stamp of the PDF");
    assert_eq!(first.pdf.as_deref().and_then(PdfStamp::of), Some(stamp));

    // Nothing changed: latexmk runs no TeX, and the log it leaves still says "Output written".
    let second = build(&req);
    assert_eq!(second.status, BuildStatus::Success);
    assert_eq!(second.pdf, first.pdf);
    assert_eq!(second.pdf_stamp, Some(stamp));

    // A changed source writes a new PDF.
    fs::write(dir.path().join("main.tex"), text("Hello again.")).unwrap();
    let third = build(&req);
    assert_eq!(third.status, BuildStatus::Success);
    assert!(third.pdf_stamp.is_some_and(|s| s != stamp), "{third:?}");
}

#[test]
fn include_needs_and_gets_mirror_dirs() {
    if !have_latexmk() {
        return;
    }
    let files = [
        (
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\include{chapters/intro}\n\\end{document}\n",
        ),
        ("chapters/intro.tex", "Hello from an included chapter.\n"),
    ];
    let dir = project(&files);
    let mut req = request(dir.path().join("main.tex"), Duration::from_secs(120));
    req.mirror_dirs = vec![PathBuf::from("chapters")];
    let outcome = build(&req);
    assert_eq!(
        outcome.status,
        BuildStatus::Success,
        "{:?}",
        errors(&outcome)
    );
    assert!(dir.path().join(".build/chapters/intro.aux").is_file());
    assert!(outcome.pdf.is_some());

    // Without the mirror directory TeX cannot write chapters/intro.aux into .build (§6).
    let bare = project(&files);
    let outcome = build(&request(
        bare.path().join("main.tex"),
        Duration::from_secs(120),
    ));
    assert_eq!(outcome.status, BuildStatus::Errors);
    let errors = errors(&outcome);
    assert!(
        errors.iter().any(|e| e.contains("can't write on file")),
        "{errors:?}"
    );
}

#[test]
fn errors_are_parsed_and_the_pdf_is_still_reported() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[(
        "main.tex",
        "\\documentclass{article}\n\\begin{document}\nThis is \\undefinedmacro here.\n\\end{document}\n",
    )]);
    let root = dir.path().join("main.tex");
    let outcome = build(&request(root, Duration::from_secs(120)));
    assert_eq!(outcome.status, BuildStatus::Errors);
    assert_eq!(outcome.exit_code, Some(12));
    let error = outcome
        .report
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("an error");
    assert!(error.path.ends_with("main.tex"), "{}", error.path.display());
    assert_eq!(
        (error.line, error.message.as_str()),
        (Some(3), "Undefined control sequence.")
    );
    assert_eq!(
        error.context.as_deref(),
        Some("This is \\undefinedmacro <HERE> here.")
    );
    // nonstopmode still writes the PDF, and the app reloads it.
    assert!(outcome.pdf.is_some());
}

#[test]
fn runaway_argument_is_located_in_an_input_file() {
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
    let outcome = build(&request(
        dir.path().join("main.tex"),
        Duration::from_secs(120),
    ));
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
        runaway.path.ends_with("sec.tex"),
        "{}",
        runaway.path.display()
    );
    assert_eq!(runaway.line, Some(2));
}

#[test]
#[cfg(unix)]
fn cancel_kills_the_whole_process_group() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[("main.tex", INFINITE_LOOP)]);
    let build = Build::start(&request(
        dir.path().join("main.tex"),
        Duration::from_secs(120),
    ))
    .expect("start latexmk");
    let canceller = build.canceller();
    let pgid = canceller.pgid();
    let waiter = thread::spawn(move || build.wait());
    wait_for_tex(pgid);

    let cancelled_at = Instant::now();
    canceller.cancel();
    canceller.cancel(); // idempotent
    let outcome = waiter.join().expect("wait thread");
    assert_eq!(outcome.status, BuildStatus::Cancelled);
    let gone = wait_for(
        Duration::from_secs(1).saturating_sub(cancelled_at.elapsed()),
        || group_gone(pgid),
    );
    assert!(gone, "group {pgid} still alive: {:?}", group_members(pgid));
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    assert!(
        group_members(pgid).is_empty(),
        "orphans: {:?}",
        group_members(pgid)
    );
    // Cancelling a finished build does nothing.
    canceller.cancel();
}

#[test]
#[cfg(unix)]
fn timeout_stops_the_build() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[("main.tex", INFINITE_LOOP)]);
    let build = Build::start(&request(
        dir.path().join("main.tex"),
        Duration::from_secs(2),
    ))
    .expect("start latexmk");
    let pgid = build.canceller().pgid();
    let outcome = build.wait();
    assert_eq!(outcome.status, BuildStatus::TimedOut);
    assert!(
        outcome.elapsed >= Duration::from_secs(2) && outcome.elapsed < Duration::from_secs(4),
        "{:?}",
        outcome.elapsed
    );
    assert!(wait_for(Duration::from_secs(1), || group_gone(pgid)));
}

#[test]
#[cfg(unix)]
fn dropping_a_build_stops_it() {
    if !have_latexmk() {
        return;
    }
    let dir = project(&[("main.tex", INFINITE_LOOP)]);
    let build = Build::start(&request(
        dir.path().join("main.tex"),
        Duration::from_secs(120),
    ))
    .expect("start latexmk");
    let pgid = build.canceller().pgid();
    wait_for_tex(pgid);
    drop(build);
    assert!(
        wait_for(Duration::from_secs(2), || group_gone(pgid)),
        "{:?}",
        group_members(pgid)
    );
}
