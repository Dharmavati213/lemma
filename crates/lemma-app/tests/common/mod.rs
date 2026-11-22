//! A tiny test harness for GTK integration tests (`harness = false`): GTK must be initialised and
//! used on one thread, so the tests run sequentially on the main thread.
//!
//! Arguments that do not start with `-` filter tests by substring, like libtest.
//!
//! A test fails if GLib or GTK logs a warning or critical while it runs (they are still printed),
//! unless it announced them with [`expect_logged_problems`].

#![allow(dead_code)]

pub mod big_project;

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
///
/// With `LEMMA_TESTS_NEED_DISPLAY` set (`scripts/ci.sh` sets it: it gives the tests a display),
/// no display is a failure: a whole test program skipped would pass for a run of its tests.
pub fn run(tests: &[Test]) -> ExitCode {
    if gtk::init().is_err() {
        if std::env::var_os("LEMMA_TESTS_NEED_DISPLAY").is_some() {
            eprintln!("no display, and LEMMA_TESTS_NEED_DISPLAY is set: the GTK tests did not run");
            return ExitCode::FAILURE;
        }
        println!("SKIP: no display");
        return ExitCode::SUCCESS;
    }
    sourceview5::init();
    // As every window of the app has it (the first `WindowState` asks for it): a test's own
    // windows too.
    lemma_app::editor::guard_input_methods();
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
        IN_TEST.set(true);
        let result = catch_unwind(AssertUnwindSafe(test));
        IN_TEST.set(false);
        // A test that found out part-way that it cannot run here (see [`skip`]) passes, like
        // the tests that print `SKIP` and return; what its unwinding logged is not counted.
        if let Err(payload) = &result
            && let Some(Skip(reason)) = payload.downcast_ref()
        {
            println!("  SKIP: {reason}");
            println!("test {name} ... ok ({:.2?})", started.elapsed());
            continue;
        }
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

thread_local! {
    /// Whether [`run`] is running a test (a helper that skips a test is also used by a
    /// program's own set-up, before any test).
    static IN_TEST: Cell<bool> = const { Cell::new(false) };
}

/// What [`skip`] unwinds with.
struct Skip(String);

/// Ends the running test as skipped: [`run`] prints `SKIP: reason` and counts it as passed.
/// For a test that learns deep inside a helper that it cannot run on this system; a test that
/// knows at its start prints `SKIP` and returns. (It unwinds without a panic message.)
pub fn skip(reason: impl Into<String>) -> ! {
    std::panic::resume_unwind(Box::new(Skip(reason.into())))
}

/// Whether the display can make key events: its keymap has a key for a letter. Under Wine
/// (`packaging/windows/build.sh --test`) GDK finds no keyboard layout, so no keyval has a
/// keycode and a key event made from one (`gtk_im_context_filter_key`) reaches nobody.
pub fn has_keymap() -> bool {
    use gtk::prelude::*;
    gdk::Display::default().is_some_and(|d| d.map_keyval(gdk::Key::a).is_some())
}

/// For a test that synthesizes key events, before it does: on Windows without a keymap (Wine)
/// the test is skipped. Everywhere else it goes on, and a missing keycode fails it.
pub fn require_keymap() {
    if cfg!(windows) && !has_keymap() {
        skip("the display has no keymap (Wine has no keyboard layout for GDK): no key events");
    }
}

/// For a test that makes Lemma send a file to the trash, before it does: skipped on Windows.
/// The tests keep the trash in a temporary `XDG_DATA_HOME`, which Windows' Recycle Bin does not
/// follow (the files would go to the user's own), and Wine asks in a dialog before it takes a
/// file, which nobody answers.
pub fn require_private_trash() {
    if cfg!(windows) {
        skip("on Windows the trash is the user's Recycle Bin (and Wine asks before each file)");
    }
}

/// For a test whose stand-in for a program (latexmk, npm, node, latexindent, …) is a shell
/// script made executable, before it writes one: skipped on Windows, which runs neither a `#!`
/// script nor `cp` and `chmod`. Outside a test (a program's set-up before its tests) it does
/// nothing.
pub fn require_shell_scripts() {
    if cfg!(windows) && IN_TEST.get() {
        skip("the stand-in program is a shell script, which Windows does not run");
    }
}

/// For a test that makes symbolic links, before it does: skipped on Windows, where making one
/// needs a privilege most users do not have (the tests make them with Unix's call).
pub fn require_symlinks() {
    if cfg!(windows) {
        skip("the test makes symbolic links, which need a privilege on Windows");
    }
}

/// For a test whose files have a name Windows does not allow (with `<`, `>`, `:`, `"`, `|`, `?`
/// or `*`), before it makes one: skipped there.
pub fn require_any_file_name() {
    if cfg!(windows) {
        skip("Windows does not allow the file name the test uses");
    }
}

/// For a test that shows or stores the API key, before it does: skipped on Windows. The tests'
/// keyring is a stand-in `secret-tool` on their `PATH`; on Windows Lemma keeps the key in the
/// user's Credential Manager, where a test must neither look nor write (one that stores its
/// test key there would replace the user's own).
pub fn require_stand_in_keyring() {
    if cfg!(windows) {
        skip("on Windows the API key is in the user's Credential Manager, not for tests");
    }
}

/// `vim::feed_key` (a key press and release for Vim, made from the display's keymap) after
/// [`require_keymap`].
pub fn feed_vim_key(
    im: &sourceview5::VimIMContext,
    view: &lemma_app::editor::TexView,
    keyval: gdk::Key,
    state: gdk::ModifierType,
) -> bool {
    require_keymap();
    lemma_app::editor::vim::feed_key(im, view, keyval, state)
}

static LOGGED: AtomicUsize = AtomicUsize::new(0);
static EXPECTED: AtomicUsize = AtomicUsize::new(0);

/// Counts GLib/GTK warnings and criticals while still printing them. GLib accepts one writer
/// per process; [`run`] installs it. With `LEMMA_TEST_BACKTRACE` set, a backtrace is printed
/// before each one that counts.
fn count_logged_problems() {
    glib::log_set_writer_func(|level, fields| {
        if matches!(
            level,
            glib::LogLevel::Error | glib::LogLevel::Critical | glib::LogLevel::Warning
        ) && !is_environment_noise(fields)
        {
            LOGGED.fetch_add(1, Ordering::SeqCst);
            // Who made GLib log it: the Rust frames below the C library's.
            if std::env::var_os("LEMMA_TEST_BACKTRACE").is_some() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
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
///   Lemma does can cause it;
/// - on Windows under Wine (`packaging/windows/build.sh --test`), GDK's Win32 backend warns
///   about what Wine lacks: no presentation time for a frame, no blur behind a window, no
///   properties of the input devices, no keyboard layout DLL, no list of the clipboard's
///   formats; GIO finds no settings schemas beside a test program, and enchant's Windows
///   spelling provider does not load.
///
/// GTK 4.24.1's `gtk_im_context_ime_message_filter: assertion 'GTK_IS_IM_CONTEXT_IME (data)'
/// failed` is not among them: it is not Wine's (Windows' input method is called after it was
/// freed), and `lemma_app::editor::guard_input_methods` prevents it.
pub fn is_environment_noise(fields: &[glib::LogField<'_>]) -> bool {
    let field = |key: &str| {
        fields
            .iter()
            .find(|f| f.key() == key)
            .and_then(glib::LogField::value_str)
    };
    let message = field("MESSAGE");
    let has = |parts: &[&str]| message.is_some_and(|m| parts.iter().any(|p| m.contains(p)));
    let domain = field("GLIB_DOMAIN");
    if cfg!(windows) {
        let wine: &[&str] = match domain {
            Some("Gdk") => &[
                "gdk_frame_clock_presented: assertion 'presentation_time != 0'",
                "DwmEnableBlurBehindWindow",
                "Failed to get device node property",
                "Failed to load keyboard layout DLL",
                "Could not open registry key 'Keyboard Layout",
                "GetUpdatedClipboardFormats() failed",
            ],
            Some("GLib-GIO") => &["g_settings_schema_source_lookup: assertion 'source != NULL'"],
            Some("libenchant") => &["EnchantProvider cannot be NULL"],
            _ => &[],
        };
        if has(wine) {
            return true;
        }
    }
    match domain {
        Some("libenchant") => has(&["Error loading plugin"]),
        Some("Gdk") => has(&["gdk_frame_timings_submitted()"]),
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

/// Runs one iteration of the default main context (without blocking) and returns how long it
/// kept the main thread: its wall time less the time the thread was ready to run but waited for
/// a CPU, from Linux's scheduler statistics. On a loaded machine the scheduler can leave a ready
/// thread waiting for tens of milliseconds, which nothing in Lemma causes; work and blocking
/// (reading files, waiting for a process or another thread) count in full. Where the system
/// keeps no such statistics, this is the wall time.
pub fn timed_iteration() -> Duration {
    let started = Instant::now();
    let waited_before = run_queue_wait();
    glib::MainContext::default().iteration(false);
    // Read before the clock stops, so that every wait it counts lies within the wall time.
    let waited = run_queue_wait()
        .zip(waited_before)
        .map_or(Duration::ZERO, |(after, before)| {
            after.saturating_sub(before)
        });
    started.elapsed().saturating_sub(waited)
}

/// How long the calling thread has waited for a CPU while ready to run, in all (the second
/// field of `/proc/thread-self/schedstat`, in nanoseconds); `None` where the system does not
/// say.
fn run_queue_wait() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let nanos = stat.split_whitespace().nth(1)?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

/// A path compiled into the test program, as a path of the system the program runs on. A
/// Windows program built on Linux (`packaging/windows/build.sh --test`, run under Wine) has
/// Linux's paths in it: `/home/…`, which Windows reads as relative to the current drive, so it
/// is not absolute and is spelled unlike every path Lemma gets from the system. There it is made
/// absolute, `Z:\home\…` (with `..` resolved and `\` between the names); elsewhere it is
/// returned as it is.
pub fn native(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if cfg!(windows) {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Cargo's directory for the temporary files of this package's tests
/// (`CARGO_TARGET_TMPDIR`), see [`native`].
pub fn target_tmpdir() -> PathBuf {
    native(env!("CARGO_TARGET_TMPDIR"))
}

/// The directory of this package's `Cargo.toml` (`CARGO_MANIFEST_DIR`), see [`native`].
pub fn manifest_dir() -> PathBuf {
    native(env!("CARGO_MANIFEST_DIR"))
}

/// `Path::canonicalize` as Lemma spells canonical paths (`lemma_core::canonicalize`): on Windows
/// without the `\\?\` prefix that `std` adds, which no path of Lemma's has. The same elsewhere.
pub trait Canonical {
    fn canonical(&self) -> std::io::Result<PathBuf>;
}

impl<P: AsRef<Path> + ?Sized> Canonical for P {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        lemma_core::canonicalize(self)
    }
}

/// `rel` (a path with `/` between its names, as the tests write them) with the system's
/// separator, as Lemma shows a path below the project root to the user: `chapters\a.tex` on
/// Windows. Not for what the agent is told (tool replies, review feedback, prompts, the names
/// `@` proposes): that has `/` on every system (`lemma_core::agent_path`), so a test compares
/// it with `rel` as it is.
pub fn shown(rel: &str) -> String {
    rel.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// A fresh, empty directory for one test under cargo's per-test temp dir.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = target_tmpdir().join(format!("lemma-app-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// The programs Lemma looks for on `PATH` to run an agent (and the keyring tool the API key
/// backend asks), which a test's `PATH` must never find on the host: see
/// [`system_programs_without`].
pub const AGENT_PROGRAMS: &[&str] = &[
    "node",
    "nodejs",
    "npm",
    "npx",
    "claude",
    "pi",
    "codex",
    "grok",
    "gemini",
    "opencode",
    "copilot",
    "secret-tool",
];

/// A stand-in for the system's program directories (`/usr/bin`, `/bin`) on a test's `PATH`: a
/// directory under `root` with a link to each of their programs except `hidden`. A fake
/// program's script still finds `cat` or `sleep` there, but Lemma never finds the host's own
/// node, npm or claude: with the real directories on `PATH`, a test that expects them missing
/// passed or failed with what the machine had installed. (Empty outside Unix.)
pub fn system_programs_without(root: &Path, hidden: &[&str]) -> PathBuf {
    let dir = root.join("system-bin");
    std::fs::create_dir_all(&dir).expect("create the system programs' stand-in");
    #[cfg(unix)]
    for system in ["/usr/bin", "/bin"] {
        let Ok(entries) = std::fs::read_dir(system) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| hidden.contains(&n)) {
                continue;
            }
            // As on `PATH`, the first directory's program wins (`/bin` is often `/usr/bin`).
            let _ = std::os::unix::fs::symlink(entry.path(), dir.join(&name));
        }
    }
    #[cfg(not(unix))]
    let _ = hidden;
    dir
}

/// The `PATH` of a test program that must not find the host's agent programs: `first` (the
/// test's own directory of stand-ins), then [`system_programs_without`]`(root, hidden)`. On
/// Windows, where that stand-in is empty, the directories of the `PATH` the program was started
/// with follow, without those that hold one of `hidden`: Windows finds a program's libraries
/// through `PATH` (the test program's own plugins, and the libraries of the programs it starts).
pub fn path_without(first: &Path, root: &Path, hidden: &[&str]) -> std::ffi::OsString {
    let mut dirs = vec![first.to_path_buf(), system_programs_without(root, hidden)];
    if cfg!(windows) {
        let holds_hidden = |dir: &Path| {
            hidden.iter().any(|name| {
                ["", ".exe", ".cmd", ".bat", ".com", ".ps1"]
                    .iter()
                    .any(|suffix| dir.join(format!("{name}{suffix}")).exists())
            })
        };
        let started_with = std::env::var_os("PATH").unwrap_or_default();
        dirs.extend(std::env::split_paths(&started_with).filter(|dir| !holds_hidden(dir)));
    }
    std::env::join_paths(dirs).expect("directories that PATH can hold")
}

/// Puts `home` back into `HOME`, for a test program that set it and reads it again later
/// (`Paths::from_env`). On Windows the spell checker's library (enchant) replaces `HOME` with
/// the user's profile directory when its providers load. Nothing happens elsewhere.
pub fn restore_home(home: &Path) {
    if cfg!(windows) {
        // SAFETY: Windows' own call to set a variable is safe with other threads running.
        unsafe { std::env::set_var("HOME", home) };
    }
}

/// Whether enchant has an `en_US` spelling dictionary (hunspell's, aspell's, …). The tests that
/// wait for spell checking need one: without it nothing is checked, so they print `SKIP` and
/// pass, as the tests for programs that are not installed do. Call it after `gtk::init`.
pub fn english_dictionary() -> bool {
    libspelling::Provider::default().supports_language("en_US")
}

/// The repository's `tests/fixtures` directory.
pub fn fixtures() -> PathBuf {
    native(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures"))
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

/// One entry of the outline [`write_pdf`] writes: its depth (0 at the top), its title, and its
/// destination: a 0-based page and a y in bp from the top of that page.
pub type OutlineEntry = (u32, String, u32, f64);

/// Writes a PDF of blank pages of `sizes` (bp) with `outline` (bookmarks) in document order, as
/// a build would: an entry's parent is the last entry before it one level up, and the entries
/// with children are closed.
pub fn write_pdf(path: &Path, sizes: &[(f64, f64)], outline: &[OutlineEntry]) {
    use std::fmt::Write;
    // Objects: 1 the catalog, 2 the page tree, then the pages, the outline and its entries.
    let page_object = |page: usize| page + 3;
    let root = sizes.len() + 3;
    let entry_object = |i: usize| root + 1 + i;
    let mut parents = Vec::with_capacity(outline.len());
    let mut ancestors: Vec<usize> = Vec::new();
    for (i, (depth, ..)) in outline.iter().enumerate() {
        ancestors.truncate(*depth as usize);
        parents.push(ancestors.last().copied());
        ancestors.push(i);
    }
    let children = |parent: Option<usize>| -> Vec<usize> {
        (0..outline.len())
            .filter(|&i| parents[i] == parent)
            .collect()
    };
    let mut objects = Vec::new();
    objects.push(if outline.is_empty() {
        "<< /Type /Catalog /Pages 2 0 R >>".to_string()
    } else {
        format!("<< /Type /Catalog /Pages 2 0 R /Outlines {root} 0 R /PageMode /UseOutlines >>")
    });
    let kids: Vec<String> = (0..sizes.len())
        .map(|p| format!("{} 0 R", page_object(p)))
        .collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        sizes.len()
    ));
    for (w, h) in sizes {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Resources << >> >>"
        ));
    }
    let top = children(None);
    objects.push(match (top.first(), top.last()) {
        (Some(first), Some(last)) => format!(
            "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {} >>",
            entry_object(*first),
            entry_object(*last),
            top.len()
        ),
        _ => "<< /Type /Outlines /Count 0 >>".to_string(),
    });
    for (i, (_, title, page, y)) in outline.iter().enumerate() {
        let siblings = children(parents[i]);
        let at = siblings.iter().position(|&s| s == i).unwrap();
        let mut entry = format!(
            "<< /Title ({}) /Parent {} 0 R /Dest [{} 0 R /XYZ 0 {} null]",
            title
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)"),
            parents[i].map_or(root, entry_object),
            page_object(*page as usize),
            sizes[*page as usize].1 - y
        );
        if at > 0 {
            write!(entry, " /Prev {} 0 R", entry_object(siblings[at - 1])).unwrap();
        }
        if let Some(next) = siblings.get(at + 1) {
            write!(entry, " /Next {} 0 R", entry_object(*next)).unwrap();
        }
        let own = children(Some(i));
        if let (Some(first), Some(last)) = (own.first(), own.last()) {
            write!(
                entry,
                " /First {} 0 R /Last {} 0 R /Count -{}",
                entry_object(*first),
                entry_object(*last),
                own.len()
            )
            .unwrap();
        }
        entry.push_str(" >>");
        objects.push(entry);
    }
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    std::fs::write(path, pdf).expect("write the PDF");
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
    // Whoever runs the tests built them already and says where they are (as for lemma-agent's
    // tests): `packaging/windows/build.sh --test`, whose Windows test programs under Wine
    // cannot run Linux's cargo (given this program's `Z:\…` path as the target directory, it
    // made a directory of that name here).
    if std::env::var_os(lemma_app::testmode::FAKE_AGENT_ENV).is_some()
        && std::env::var_os("LEMMA_MCP_BRIDGE").is_some()
    {
        assert!(
            lemma_app::testmode::sibling_binary("lemma-mcp").is_some()
                && lemma_app::testmode::fake_agent_binary().is_some(),
            "LEMMA_FAKE_AGENT is not a program, or lemma-mcp is not next to the test's directory"
        );
        BUILT.with(|b| b.set(true));
        return;
    }
    let exe = std::env::current_exe().unwrap();
    // <target>/<profile>, wherever below it cargo put this test program.
    let profile_dir = lemma_app::testmode::cargo_profile_dir(&exe)
        .expect("the test program is below cargo's <target>/<profile>");
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
