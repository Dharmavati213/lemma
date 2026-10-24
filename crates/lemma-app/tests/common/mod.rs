//! A tiny test harness for GTK integration tests (`harness = false`): GTK must be initialised and
//! used on one thread, so the tests run sequentially on the main thread.
//!
//! Arguments that do not start with `-` filter tests by substring, like libtest.
//!
//! A test fails if GLib or GTK logs a warning or critical while it runs (they are still printed),
//! unless it announced them with [`expect_logged_problems`].

#![allow(dead_code)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A named test.
pub type Test = (&'static str, fn());

/// Initialises GTK (printing `SKIP: no display` and succeeding when there is no display), runs
/// the tests that match the command-line filters, and reports like libtest.
pub fn run(tests: &[Test]) -> ExitCode {
    if gtk::init().is_err() {
        println!("SKIP: no display");
        return ExitCode::SUCCESS;
    }
    sourceview5::init();
    count_logged_problems();
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let selected: Vec<&Test> = tests
        .iter()
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str())))
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in selected.iter().copied() {
        let started = Instant::now();
        let logged_before = logged_problems();
        EXPECTED.store(0, Ordering::SeqCst);
        let result = catch_unwind(AssertUnwindSafe(test));
        let unexpected =
            (logged_problems() - logged_before).saturating_sub(EXPECTED.load(Ordering::SeqCst));
        let ok = result.is_ok() && unexpected == 0;
        let status = if ok { "ok" } else { "FAILED" };
        println!("test {name} ... {status} ({:.2?})", started.elapsed());
        if result.is_ok() && unexpected > 0 {
            println!(
                "    GLib logged {unexpected} unexpected warning(s) or critical(s), printed above"
            );
        }
        if !ok {
            failed.push(*name);
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failures: {failed:?}");
        ExitCode::FAILURE
    }
}

static LOGGED: AtomicUsize = AtomicUsize::new(0);
static EXPECTED: AtomicUsize = AtomicUsize::new(0);

/// Counts GLib/GTK warnings and criticals while still printing them. GLib accepts one writer
/// per process; [`run`] installs it.
fn count_logged_problems() {
    glib::log_set_writer_func(|level, fields| {
        if matches!(
            level,
            glib::LogLevel::Error | glib::LogLevel::Critical | glib::LogLevel::Warning
        ) && !is_environment_noise(fields)
        {
            LOGGED.fetch_add(1, Ordering::SeqCst);
        }
        glib::log_writer_default(level, fields)
    });
}

/// Warnings about the machine rather than about Lemma, printed but not counted:
/// - enchant (spell checking, WP2.9b) warns once per process about every optional provider
///   plugin whose library is not installed (`Error loading plugin: libvoikko.so.1: …`), through
///   structured logging that no log handler can intercept;
/// - GTK 4.24's Broadway backend warns about its own frame clock now and then (`Duplicate call
///   to gdk_frame_timings_submitted() with different values`, `… called on submitted frame`),
///   about once a second while frames are drawn. The function is internal to GDK, so nothing
///   Lemma does can cause it.
pub fn is_environment_noise(fields: &[glib::LogField<'_>]) -> bool {
    let field = |key: &str| {
        fields
            .iter()
            .find(|f| f.key() == key)
            .and_then(glib::LogField::value_str)
    };
    let message = field("MESSAGE");
    match field("GLIB_DOMAIN") {
        Some("libenchant") => message.is_some_and(|m| m.contains("Error loading plugin")),
        Some("Gdk") => message.is_some_and(|m| m.contains("gdk_frame_timings_submitted()")),
        _ => false,
    }
}

/// How many warnings and criticals GLib has logged in this process so far.
pub fn logged_problems() -> usize {
    LOGGED.load(Ordering::SeqCst)
}

/// Declares that the running test logs `n` warnings or criticals on purpose.
pub fn expect_logged_problems(n: usize) {
    EXPECTED.fetch_add(n, Ordering::SeqCst);
}

/// Panics if GLib logged a warning or critical since `logged_problems()` returned `before`.
pub fn assert_no_new_problems(before: usize) {
    assert_eq!(
        logged_problems(),
        before,
        "GLib logged warnings or criticals (printed above)"
    );
}

/// Runs the default main context until `done` returns true; panics after `timeout`.
pub fn pump_until(timeout: Duration, mut done: impl FnMut() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out after {timeout:?}");
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs the default main context for `duration`.
pub fn pump_for(duration: Duration) {
    let deadline = Instant::now() + duration;
    pump_until(duration + Duration::from_secs(1), || {
        Instant::now() >= deadline
    });
}

/// A fresh, empty directory for one test under cargo's per-test temp dir.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-app-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// The repository's `tests/fixtures` directory.
pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// One registered application per test binary (without an id, so nothing is exported on
/// D-Bus), for tests that build windows.
pub fn application() -> adw::Application {
    thread_local! {
        static APP: adw::Application = {
            use gio::prelude::*;
            let app = adw::Application::builder()
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(None::<&gio::Cancellable>).expect("register the application");
            app
        };
    }
    APP.with(Clone::clone)
}

/// A small deterministic PRNG (xorshift64*), so failures are reproducible from the seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Builds `lemma-fake-agent` and `lemma-mcp` into this test's target directory and profile
/// (once per run; cargo does not build other packages' binaries for `cargo test -p lemma-app`),
/// where the bridge finds them next to `lemma`.
pub fn require_agent_binaries() {
    thread_local! {
        static BUILT: Cell<bool> = const { Cell::new(false) };
    }
    if BUILT.with(Cell::get) {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    // <target>/<profile>/deps/<test>
    let profile_dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
    let target_dir = profile_dir.parent().unwrap();
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo
        .args([
            "build",
            "--quiet",
            "--offline",
            "-p",
            "lemma-fake-agent",
            "-p",
            "lemma-mcp",
        ])
        .arg("--target-dir")
        .arg(target_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") | None => {}
        Some("release") => {
            cargo.arg("--release");
        }
        Some(other) => {
            cargo.args(["--profile", other]);
        }
    }
    assert!(
        cargo.status().is_ok_and(|s| s.success()),
        "building lemma-fake-agent and lemma-mcp failed"
    );
    assert!(
        lemma_app::testmode::sibling_binary("lemma-mcp").is_some()
            && lemma_app::testmode::fake_agent_binary().is_some(),
        "lemma-mcp and lemma-fake-agent are not next to the test's directory"
    );
    BUILT.with(|b| b.set(true));
}
