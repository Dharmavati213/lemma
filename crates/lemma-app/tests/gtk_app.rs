//! The `lemma` binary end to end (WP0.2 acceptance 1 and 4): `--snapshot`, `--quit-after-ms`,
//! `--mem-report`, `--build`, `--sidebar`, several FILEs with lines, and usage errors.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_app`. The binary
//! inherits the Broadway environment; `LEMMA_CONFIG=/dev/null` keeps the user's config out.

mod common;

use gtk::gdk;
use gtk::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

fn main() -> ExitCode {
    common::run(&[
        (
            "snapshot_shows_highlighted_text",
            snapshot_shows_highlighted_text,
        ),
        ("mem_report_prints_rss_and_pss", mem_report),
        ("snapshot_errors_fail_the_run", snapshot_errors),
        ("build_option_builds_the_document", build_succeeds),
        ("build_option_opens_the_first_error", build_with_errors),
        (
            "build_option_without_a_main_document_fails",
            build_without_root,
        ),
        ("usage_errors_and_help", usage),
        ("several_files_and_lines", several_files),
        (
            "quitting_stops_the_word_count",
            quitting_stops_the_word_count,
        ),
        // M5 (WP5.4)
        ("perf_mode_prints_startup_and_typing", perf_mode),
        ("perf_mode_rejects_unknown_scenarios", perf_unknown),
        ("renderer_setting_and_environment", renderer_setting),
        // Final review (M5)
        (
            "regress5_renderer_choice_is_not_inherited",
            regress5_renderer_not_inherited,
        ),
    ])
}

fn lemma(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .args(args)
        .env("LEMMA_CONFIG", "/dev/null")
        .output()
        .expect("run lemma");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    output
}

fn fixture_main() -> String {
    common::fixtures()
        .join("proj/main.tex")
        .to_string_lossy()
        .into_owned()
}

/// Acceptance 1: exits 0 and writes a PNG larger than 10 KB that shows highlighted text.
fn snapshot_shows_highlighted_text() {
    let dir = common::temp_dir("snapshot");
    let png = dir.join("m0.png");
    let out = lemma(&[
        &fixture_main(),
        "--snapshot",
        png.to_str().unwrap(),
        "--quit-after-ms",
        "1500",
    ]);
    assert!(out.status.success(), "lemma failed: {:?}", out.status);
    let size = std::fs::metadata(&png).expect("the PNG exists").len();
    assert!(size > 10_000, "PNG is only {size} bytes");
    let colored = colored_text_pixels(&png);
    println!("  {size} bytes, {colored} saturated pixels in the editor");
    assert!(
        colored > 500,
        "no syntax-highlighted text in the editor area"
    );
}

/// Counts strongly coloured pixels in the left half of the window below the header and tab bar,
/// where the editor shows the document: plain text, the gutter and the window chrome are grey,
/// so these come from syntax highlighting.
fn colored_text_pixels(png: &Path) -> usize {
    let texture = gdk::Texture::from_filename(png).expect("decode the PNG");
    let (width, height) = (texture.width() as usize, texture.height() as usize);
    let mut downloader = gdk::TextureDownloader::new(&texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    let mut colored = 0;
    for y in 110..height.saturating_sub(40) {
        for x in 60..width / 2 {
            let p = &bytes[y * stride + x * 4..][..3];
            let (max, min) = (p.iter().max().unwrap(), p.iter().min().unwrap());
            if max - min > 80 {
                colored += 1;
            }
        }
    }
    colored
}

/// Acceptance 4: the memory report (printed, not asserted under Broadway).
fn mem_report() {
    let out = lemma(&[&fixture_main(), "--mem-report", "--quit-after-ms", "1000"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value = |key: &str| {
        stdout
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no {key} line in {stdout:?}"))
    };
    let (rss, pss) = (value("Rss:"), value("Pss:"));
    println!("  Rss {rss} kB, Pss {pss} kB (budget on Wayland: 100 MB / 76 MB)");
    assert!(rss > 0 && pss > 0 && pss <= rss);
}

fn snapshot_errors() {
    let out = lemma(&[
        &fixture_main(),
        "--snapshot",
        "/nonexistent-dir/x.png",
        "--quit-after-ms",
        "200",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot write"));

    // A demo runs to completion before the run quits (`--demo completion` is implemented since
    // WP2.9a; a failing demo makes the exit status 1, see `testmode::run`).
    let out = lemma(&["--demo", "completion", "--quit-after-ms", "200"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("--demo completion"), "{stderr}");
}

/// The root shown at the start is counted (texcount on the jobs thread). Quitting ends the
/// process without destroying its window, and the count's texcount is killed then: here a
/// stand-in on `PATH` that writes its process id and sleeps.
fn quitting_stops_the_word_count() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let dir = copy_project(
        &common::fixtures().join("proj"),
        PROJECT_FILES,
        "quit-count",
    );
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pid_file = dir.join("texcount.pid");
    let script = bin.join("texcount");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
            pid_file.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let main = dir.join("main.tex");
    // Long enough for the count to start in a debug build under load.
    let out = lemma_env(
        "quit-count-settings",
        &[main.to_str().unwrap(), "--quit-after-ms", "3000"],
        &[("PATH", &path)],
        "[spell]\nenabled = false\n",
    );
    assert!(out.status.success());
    let pid = std::fs::read_to_string(&pid_file).expect("texcount started before the quit");
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    // Gone (an orphan's zombie is reaped at once).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while proc.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if proc.exists() {
        let _ = Command::new("kill").arg(pid.trim()).status();
        panic!("texcount outlived Lemma");
    }
}

/// Copies `files` (paths relative to `from`) into a fresh directory named `name`.
fn copy_project(from: &Path, files: &[&str], name: &str) -> PathBuf {
    let dir = common::temp_dir(name);
    for file in files {
        let target = dir.join(file);
        std::fs::create_dir_all(target.parent().unwrap()).expect("create directories");
        std::fs::copy(from.join(file), &target).expect("copy a fixture file");
    }
    dir
}

const PROJECT_FILES: &[&str] = &["main.tex", "refs.bib", "chapters/yoneda.tex"];

/// Builds need latexmk; like lemma-build's integration tests, skip without it.
fn have_latexmk() -> bool {
    let found = Command::new("latexmk")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        println!("  SKIP: latexmk not found");
    }
    found
}

fn build_succeeds() {
    if !have_latexmk() {
        return;
    }
    let dir = copy_project(&common::fixtures().join("proj"), PROJECT_FILES, "build-ok");
    let main = dir.join("main.tex");
    let out = lemma(&[main.to_str().unwrap(), "--build", "--quit-after-ms", "200"]);
    assert!(out.status.success(), "lemma failed: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("build: Built in "), "{stdout}");
    assert!(dir.join(".build/main.pdf").is_file());
}

fn build_with_errors() {
    if !have_latexmk() {
        return;
    }
    let from = common::fixtures().join("logs/undefcs");
    let dir = copy_project(&from, PROJECT_FILES, "build-errors");
    let main = dir.join("main.tex");
    let png = dir.join("problems.png");
    let out = lemma(&[
        main.to_str().unwrap(),
        "--build",
        "--sidebar",
        "problems",
        "--snapshot",
        png.to_str().unwrap(),
        "--quit-after-ms",
        "200",
    ]);
    // TeX errors are the build's result, not a failure of the run.
    assert!(out.status.success(), "lemma failed: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("build: Build failed: 1 error"), "{stdout}");
    let main = std::fs::canonicalize(&main).unwrap();
    let opened = format!("build: first error at {}:66", main.display());
    assert!(stdout.contains(&opened), "{stdout}");
    assert!(png.is_file());
}

fn build_without_root() {
    let dir = common::temp_dir("build-no-root");
    let file = dir.join("notes.tex");
    std::fs::write(&file, "Just a fragment, no \\documentclass.\n").unwrap();
    let out = lemma(&[file.to_str().unwrap(), "--build", "--quit-after-ms", "200"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--build: the build did not start: "),
        "{stderr}"
    );
}

fn usage() {
    let out = lemma(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option --frobnicate"));
    let out = lemma(&["main.tex", "+5"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("+5 must come before a FILE"));
    let fixtures = common::fixtures();
    let main = fixture_main();
    let out = lemma(&[&main, fixtures.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only the first FILE can be a directory")
    );
    let out = lemma(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--snapshot PNG"));
    assert!(help.contains("[+LINE] FILE[:LINE[:COLUMN]]"), "{help}");
    let out = lemma(&["--version"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("lemma "));
}

/// Several FILEs (as `lemma *.tex` gives them), with `+LINE` and `FILE:LINE`, open without
/// complaint (they used to be a usage error); `gtk_session`'s
/// `startup_opens_several_files_and_a_line` checks the tabs and cursors.
fn several_files() {
    let dir = copy_project(&common::fixtures().join("proj"), PROJECT_FILES, "several");
    let png = dir.join("several.png");
    let out = lemma(&[
        dir.join("main.tex").to_str().unwrap(),
        "+3",
        dir.join("chapters/yoneda.tex").to_str().unwrap(),
        &format!("{}:2:5", dir.join("refs.bib").display()),
        "--snapshot",
        png.to_str().unwrap(),
        "--quit-after-ms",
        "1500",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "lemma failed: {stderr}");
    assert!(!stderr.contains("lemma:"), "{stderr}");
    assert!(png.is_file());
}

/// Runs lemma with `envs` (and `GSK_RENDERER` removed unless given) and private state and data
/// directories; `config` is the settings text.
fn lemma_env(name: &str, args: &[&str], envs: &[(&str, &str)], config: &str) -> Output {
    let dir = common::temp_dir(name);
    let settings = dir.join("config.toml");
    std::fs::write(&settings, config).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lemma"));
    command
        .args(args)
        .env("LEMMA_CONFIG", &settings)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env_remove("GSK_RENDERER")
        .env_remove("LEMMA_PERF");
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("run lemma");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    output
}

/// `LEMMA_PERF` (scripts/perf-wayland.sh): one `[perf]` line per result. Broadway paints about
/// once a second, so only the shape is checked here.
fn perf_mode() {
    let out = lemma_env(
        "perf",
        &[&fixture_main(), "--quit-after-ms", "1"],
        &[
            ("LEMMA_PERF", "startup,typing"),
            ("LEMMA_PERF_KEYS", "3"),
            ("LEMMA_PERF_T0", "0"),
        ],
        "[spell]\nenabled = false\n",
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = |prefix: &str| {
        stdout
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} line in {stdout:?}"))
            .to_owned()
    };
    let first = line("[perf] startup first_frame_ms=");
    // The milestones before the first frame, and the time since exec (T0 = the epoch here).
    for key in [
        "gtk_init_ms=",
        "window_ms=",
        "presented_ms=",
        "exec_first_frame_ms=",
    ] {
        assert!(first.contains(key), "{first}");
    }
    assert!(line("[perf] startup ready_ms=").contains("exec_ready_ms="));
    let typing = line("[perf] typing ");
    for key in [
        "file=main.tex",
        "keys=3",
        "timeouts=0",
        "handler_p50=",
        "paint_p99=",
        "work_p99=",
        "to_frame_max=",
        "slowest=",
    ] {
        assert!(typing.contains(key), "{typing}");
    }
}

fn perf_unknown() {
    let out = lemma_env(
        "perf-unknown",
        &[&fixture_main(), "--quit-after-ms", "1"],
        &[("LEMMA_PERF", "startup,scroll")],
        "",
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown scenario \"scroll\""), "{stderr}");
}

/// `[editor] renderer` sets `GSK_RENDERER` unless the environment has it; `auto` (cairo) leaves
/// Broadway to its own renderer.
fn renderer_setting() {
    let log = [("LEMMA_LOG", "lemma_app::app=debug")];
    let args = [fixture_main(), "--quit-after-ms".into(), "1".into()];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stderr = |out: Output| {
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let cairo = stderr(lemma_env(
        "renderer-cairo",
        &args,
        &log,
        "[editor]\nrenderer = \"cairo\"\n",
    ));
    assert!(
        cairo.contains("GSK_RENDERER=cairo from [editor] renderer"),
        "{cairo}"
    );
    let auto = stderr(lemma_env("renderer-auto", &args, &log, ""));
    // Under Broadway (the tests' display) `auto` keeps Broadway's renderer.
    assert!(auto.contains("renderer: Broadway's own"), "{auto}");
    let env = [log[0], ("GSK_RENDERER", "cairo")];
    let set = stderr(lemma_env(
        "renderer-env",
        &args,
        &env,
        "[editor]\nrenderer = \"gl\"\n",
    ));
    assert!(set.contains("GSK_RENDERER from the environment"), "{set}");
    assert!(!set.contains("GSK_RENDERER=ngl"), "{set}");
}

/// `[editor] renderer` sets `GSK_RENDERER` for Lemma's own windows only: once GTK has read it
/// (the first window's renderer), it leaves the environment again, so the programs and apps
/// Lemma starts (latexmk, the agent, a figure opened in an image viewer) do not inherit it. A
/// `GSK_RENDERER` from the user's own environment stays.
fn regress5_renderer_not_inherited() {
    let log = [("LEMMA_LOG", "lemma_app::app=debug")];
    let args = [fixture_main(), "--quit-after-ms".into(), "1500".into()];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stderr = |out: Output| {
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let cairo = stderr(lemma_env(
        "r5-renderer-cairo",
        &args,
        &log,
        "[editor]\nrenderer = \"cairo\"\n",
    ));
    assert!(
        cairo.contains("renderer: GSK_RENDERER removed from the environment"),
        "{cairo}"
    );
    let env = [log[0], ("GSK_RENDERER", "cairo")];
    let user = stderr(lemma_env(
        "r5-renderer-user",
        &args,
        &env,
        "[editor]\nrenderer = \"cairo\"\n",
    ));
    assert!(!user.contains("GSK_RENDERER removed"), "{user}");
}
