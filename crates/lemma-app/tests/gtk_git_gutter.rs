//! WP3.8: the git gutter (`git_gutter.rs`) on throwaway repositories: lines added, changed and
//! deleted relative to `HEAD`, a file git does not track, a file added to the index, a file
//! outside any repository, edits and commits, no git per keystroke, the renderer's place, the
//! `[git] gutter` switch and closing tabs.
//!
//! Every repository is created in this test's temporary directory with `GIT_CONFIG_GLOBAL=/dev/null`
//! and `GIT_CONFIG_NOSYSTEM=1`; `GIT_CEILING_DIRECTORIES` keeps git from looking above it (so
//! "outside any repository" is true, and the project's own repository is never read).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_git_gutter`; the perf
//! test: `… --release … -- --ignored perf`. `LEMMA_TEST_SNAPSHOTS=dir` saves
//! `p-git-gutter-test.png` there.

mod common;

use adw::prelude::*;
use lemma_app::editor::Editor;
use lemma_app::editor::review_gutter::ReviewGutter;
use lemma_app::git_gutter::{self, GitGutterRenderer, MarkKind, Marks, Status};
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&tmp).expect("create the temporary directory");
    // SAFETY: no other thread exists yet.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CEILING_DIRECTORIES", &tmp);
        std::env::set_var("GIT_AUTHOR_NAME", "Lemma Test");
        std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.invalid");
        std::env::set_var("GIT_COMMITTER_NAME", "Lemma Test");
        std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.invalid");
        std::env::remove_var("GIT_DIR");
        std::env::remove_var("GIT_WORK_TREE");
    }
    let mut tests: Vec<common::Test> = vec![
        ("marks_follow_edits_against_head", edits),
        ("untracked_staged_and_committed_files", index_and_commits),
        ("outside_any_repository", outside),
        ("marks_clear_after_a_commit", commit_clears),
        ("no_git_per_keystroke", no_git_per_keystroke),
        ("marks_equal_a_full_diff_after_random_edits", random_edits),
        ("head_text_is_read_once_per_commit", head_cached),
        ("line_ends_and_subdirectories", crlf_and_subdirectories),
        ("renderer_sits_left_of_the_review_gutter", renderer_place),
        ("config_switch_and_closing_tabs", config_and_close),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_git_gutter_keystrokes_large_tex", perf_large));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(20);

const TEXT: &str = "\\documentclass{article}\n\
\\begin{document}\n\
one\n\
two\n\
three\n\
four\n\
five\n\
six\n\
\\end{document}\n";

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// Runs git in `dir`; panics when it fails.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository in a fresh directory with `files` committed.
fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = common::temp_dir(name);
    write(&dir, files);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    dir.canonicalize().unwrap()
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(dir: &Path) -> Fixture {
    fixture_with(dir, |_| {})
}

fn fixture_with(dir: &Path, configure: impl FnOnce(&mut Config)) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.history.enabled = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir: dir.to_path_buf(),
    }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        assert!(block_on(testmode::wait_for_frames(&self.state.window, 1)));
        self.settle();
        editor
    }

    /// Waits until the gutter has nothing to do.
    fn settle(&self) {
        common::pump_until(TIMEOUT, || git_gutter::is_idle(&self.state));
    }

    /// Waits until `editor`'s marks satisfy `done`.
    fn wait_marks(&self, editor: &Editor, what: &str, done: impl Fn(&Marks) -> bool) -> Marks {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let marks = git_gutter::marks(editor).expect("the gutter follows the editor");
            if done(&marks) && git_gutter::is_idle(&self.state) {
                return marks;
            }
            assert!(Instant::now() < deadline, "{what}: marks are {marks:?}");
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_status(&self, editor: &Editor, status: Status) {
        let deadline = Instant::now() + TIMEOUT;
        while git_gutter::status(editor) != Some(status.clone())
            || !git_gutter::is_idle(&self.state)
        {
            assert!(
                Instant::now() < deadline,
                "status {:?}, wanted {status:?}",
                git_gutter::status(editor)
            );
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// The marks as one character per line: `.` same, `A` added, `C` changed, preceded by `^` when
/// lines were deleted just before it (the last entry is the line after the last).
fn picture(marks: &Marks, lines: u32) -> String {
    (0..=lines)
        .map(|l| {
            let mut s = String::new();
            if marks.deleted_before(l) {
                s.push('^');
            }
            s.push(match marks.kind_of(l) {
                Some(MarkKind::Added) => 'A',
                Some(MarkKind::Changed) => 'C',
                None => '.',
            });
            s
        })
        .collect()
}

fn set_text(editor: &Editor, text: &str) {
    editor.buffer().set_text(text);
}

fn line_count(editor: &Editor) -> u32 {
    editor.document().line_index().line_count()
}

// ----- the tests -----

fn edits() {
    let dir = repository("edits", &[("main.tex", TEXT)]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    assert!(git_gutter::marks(&editor).unwrap().is_empty(), "clean");
    assert!(git_gutter::has_renderer(&editor));

    // Change "two", add two lines after "four", delete "six".
    let edited = TEXT
        .replace("two\n", "TWO\n")
        .replace("four\n", "four\nnew 1\nnew 2\n")
        .replace("six\n", "");
    set_text(&editor, &edited);
    let marks = f.wait_marks(&editor, "after the edits", |m| !m.is_empty());
    assert_eq!(
        picture(&marks, line_count(&editor)),
        "...C..AA.^...",
        "{marks:?}"
    );
    // Typing inside an added line keeps it added; the edit back to HEAD clears everything.
    set_text(&editor, TEXT);
    f.wait_marks(&editor, "back to HEAD", Marks::is_empty);
    // Everything deleted: one wedge, at the top.
    set_text(&editor, "");
    let marks = f.wait_marks(&editor, "emptied", |m| !m.is_empty());
    assert!(marks.deleted_before(0), "{marks:?}");
    assert!(marks.lines.is_empty() || marks.kind_of(0) == Some(MarkKind::Changed));
}

fn index_and_commits() {
    let dir = repository("index", &[("main.tex", TEXT)]);
    write(&dir, &[("new.tex", "a\nb\nc\n")]);
    let f = fixture(&dir);
    let editor = f.open("new.tex");
    f.wait_status(&editor, Status::Untracked);
    assert!(git_gutter::marks(&editor).unwrap().is_empty(), "untracked");
    assert!(
        !git_gutter::has_renderer(&editor),
        "no strip: the gutter keeps its width"
    );
    // Added to the index: the file monitor notices; every line is new relative to HEAD.
    git(&dir, &["add", "new.tex"]);
    f.wait_status(&editor, Status::Tracked { in_head: false });
    assert!(git_gutter::has_renderer(&editor));
    let marks = f.wait_marks(&editor, "staged", |m| !m.is_empty());
    assert_eq!(picture(&marks, line_count(&editor)), "AAA..");
    // Committed: HEAD moved; nothing differs any more.
    git(&dir, &["commit", "-q", "-m", "add new"]);
    f.wait_status(&editor, Status::Tracked { in_head: true });
    f.wait_marks(&editor, "committed", Marks::is_empty);
    // Removed from the index again (still in HEAD): still compared with HEAD.
    git(&dir, &["rm", "-q", "--cached", "new.tex"]);
    git_gutter::refresh(&f.state);
    f.settle();
    assert_eq!(
        git_gutter::status(&editor),
        Some(Status::Tracked { in_head: true })
    );

    // A repository without commits: tracked files are all added, untracked ones unmarked.
    let empty = common::temp_dir("index-unborn");
    write(&empty, &[("a.tex", "x\ny\n"), ("b.tex", "z\n")]);
    git(&empty, &["init", "-q"]);
    git(&empty, &["add", "a.tex"]);
    let g = fixture(&empty.canonicalize().unwrap());
    let a = g.open("a.tex");
    let b = g.open("b.tex");
    g.wait_status(&a, Status::Tracked { in_head: false });
    g.wait_status(&b, Status::Untracked);
    assert_eq!(
        picture(&g.wait_marks(&a, "unborn", |m| !m.is_empty()), 3),
        "AA.."
    );
    // The first commit.
    git(&empty, &["commit", "-q", "-m", "first"]);
    g.wait_status(&a, Status::Tracked { in_head: true });
    g.wait_marks(&a, "first commit", Marks::is_empty);
}

fn outside() {
    let dir = common::temp_dir("outside");
    write(&dir, &[("main.tex", TEXT)]);
    let runs = git_gutter::git_runs();
    let f = fixture(&dir.canonicalize().unwrap());
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::OutsideRepository);
    assert!(!git_gutter::has_renderer(&editor));
    set_text(&editor, "changed\n");
    common::pump_for(git_gutter::DIFF_DELAY * 3);
    f.settle();
    assert!(git_gutter::marks(&editor).unwrap().is_empty());
    // One look for the repository; another tab in the same directory asks nothing more.
    assert_eq!(git_gutter::git_runs() - runs, 1, "one discovery");
    write(&dir, &[("other.tex", "x\n")]);
    let other = f.open("other.tex");
    f.wait_status(&other, Status::OutsideRepository);
    git_gutter::refresh(&f.state);
    f.settle();
    assert_eq!(git_gutter::git_runs() - runs, 1, "cached per directory");
}

fn commit_clears() {
    let dir = repository("commit", &[("main.tex", TEXT)]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    let edited = TEXT.replace("three\n", "three\nmore\n");
    set_text(&editor, &edited);
    assert_eq!(block_on(editor.document().save()), Ok(()));
    let marks = f.wait_marks(&editor, "edited", |m| !m.is_empty());
    assert_eq!(marks.kind_of(5), Some(MarkKind::Added));
    // Committed from a terminal: the monitor sees HEAD move, the marks go.
    git(&dir, &["commit", "-q", "-am", "more"]);
    f.wait_marks(&editor, "after the commit", Marks::is_empty);
    // New edits are compared with the new HEAD.
    set_text(&editor, &edited.replace("more\n", "MORE\n"));
    let marks = f.wait_marks(&editor, "edited again", |m| !m.is_empty());
    assert_eq!(picture(&marks, line_count(&editor)), ".....C......");
    // `git reset --hard HEAD~1` from a terminal: HEAD moves back (the buffer keeps its text).
    git(&dir, &["reset", "-q", "--soft", "HEAD~1"]);
    let marks = f.wait_marks(&editor, "after a reset", |m| {
        m.kind_of(5) == Some(MarkKind::Added)
    });
    assert_eq!(marks.kind_of(5), Some(MarkKind::Added));
}

fn no_git_per_keystroke() {
    let dir = repository("keys", &[("main.tex", TEXT)]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    let runs = git_gutter::git_runs();
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(4).unwrap());
    for c in "typed on the line ".chars().cycle().take(120) {
        buffer.insert_at_cursor(&c.to_string());
        common::pump_for(Duration::from_millis(3));
    }
    let marks = f.wait_marks(&editor, "typed", |m| !m.is_empty());
    assert_eq!(marks.kind_of(4), Some(MarkKind::Changed), "{marks:?}");
    // A line break typed at the end of it: a new line after it.
    let mut end = buffer.iter_at_line(4).unwrap();
    end.forward_to_line_end();
    buffer.place_cursor(&end);
    buffer.insert_at_cursor("\n");
    let marks = f.wait_marks(&editor, "a line break", |m| m.kind_of(5).is_some());
    assert_eq!(picture(&marks, 5), "....CA", "{marks:?}");
    assert_eq!(git_gutter::git_runs(), runs, "typing never runs git");
    // Marks move with inserted lines before the next diff.
    let line_before = marks.lines[0].0.start;
    buffer.insert(&mut buffer.start_iter(), "x\ny\n");
    let moved = git_gutter::marks(&editor).unwrap();
    assert_eq!(moved.lines[0].0.start, line_before + 2, "{moved:?}");
    f.wait_marks(&editor, "rediffed", |m| {
        m.kind_of(0) == Some(MarkKind::Added)
    });
    assert_eq!(git_gutter::git_runs(), runs);
}

/// The line hashes the gutter keeps up to date edit by edit give the marks a diff of the whole
/// texts gives, through random inserts, deletions, line joins and splits, undo and redo.
fn random_edits() {
    let dir = repository("random", &[("main.tex", TEXT)]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    let buffer = editor.buffer();
    let mut rng = common::Rng::new(0x38);
    for round in 0..12 {
        for _ in 0..25 {
            let len = buffer.char_count();
            let at = rng.below(len as u64 + 1) as i32;
            match rng.below(5) {
                0 | 1 => {
                    let piece = ["x", "\n", "ab\ncd", "\\[\n", "é\n\n"][rng.below(5) as usize];
                    buffer.insert(&mut buffer.iter_at_offset(at), piece);
                }
                2 | 3 if len > 0 => {
                    let end = (at + 1 + rng.below(12) as i32).min(len);
                    let (mut a, mut b) = (buffer.iter_at_offset(at), buffer.iter_at_offset(end));
                    buffer.delete(&mut a, &mut b);
                }
                _ => {
                    if buffer.can_undo() && rng.below(2) == 0 {
                        buffer.undo();
                    } else if buffer.can_redo() {
                        buffer.redo();
                    }
                }
            }
        }
        let expected = git_gutter::compute_marks(TEXT, &editor.document().text());
        let marks = f.wait_marks(&editor, &format!("round {round}"), |m| *m == expected);
        assert_eq!(marks, expected, "round {round}");
    }
}

fn head_cached() {
    let dir = repository("cached", &[("main.tex", TEXT), ("b.tex", "b\n")]);
    let f = fixture(&dir);
    let main = f.open("main.tex");
    // The directory is known to be in a work tree: the strip is there before git answers, so
    // the text does not move.
    let b = block_on(f.state.open_file(&dir.join("b.tex"))).unwrap();
    assert!(git_gutter::has_renderer(&b));
    assert_eq!(git_gutter::status(&b), Some(Status::Pending));
    f.wait_status(&main, Status::Tracked { in_head: true });
    f.wait_status(&b, Status::Tracked { in_head: true });
    // A refresh (focus, a save, the index) resolves HEAD once; the texts stay cached.
    let runs = git_gutter::git_runs();
    git_gutter::refresh(&f.state);
    f.settle();
    assert_eq!(git_gutter::git_runs() - runs, 1, "one rev-parse");
    // A save refreshes too (debounced).
    assert_eq!(block_on(main.document().save()), Ok(()));
    common::pump_for(git_gutter::REFRESH_DELAY * 2);
    f.settle();
    assert_eq!(git_gutter::git_runs() - runs, 2);
    // A new commit reads both texts again.
    write(&dir, &[("c.tex", "c\n")]);
    git(&dir, &["add", "c.tex"]);
    git(&dir, &["commit", "-q", "-m", "c"]);
    common::pump_for(git_gutter::REFRESH_DELAY * 2);
    f.settle();
    let after = git_gutter::git_runs() - runs;
    assert!((5..=8).contains(&after), "rev-parse and two reads: {after}");
}

fn crlf_and_subdirectories() {
    let crlf = TEXT.replace('\n', "\r\n");
    let dir = repository("crlf", &[("main.tex", &crlf), ("ch/one.tex", "a\nb\n")]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    assert!(git_gutter::marks(&editor).unwrap().is_empty());
    // The same text with LF ends: no line changed.
    set_text(&editor, TEXT);
    common::pump_for(git_gutter::DIFF_DELAY * 3);
    f.settle();
    assert!(git_gutter::marks(&editor).unwrap().is_empty());
    let sub = f.open("ch/one.tex");
    f.wait_status(&sub, Status::Tracked { in_head: true });
    set_text(&sub, "a\nB\n");
    let marks = f.wait_marks(&sub, "subdirectory", |m| !m.is_empty());
    assert_eq!(picture(&marks, line_count(&sub)), ".C..");
}

fn renderer_place() {
    let dir = repository("place", &[("main.tex", TEXT)]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    let view = editor.view();
    let gutter = sourceview5::prelude::ViewExt::gutter(
        view.upcast_ref::<sourceview5::View>(),
        gtk::TextWindowType::Left,
    );
    let mut ours = None;
    let mut review = None;
    let mut child = gutter.first_child();
    while let Some(widget) = child {
        if widget.is::<GitGutterRenderer>() {
            ours = widget.compute_bounds(&gutter).map(|b| (b.x(), b.width()));
        }
        if widget.is::<ReviewGutter>() {
            review = widget.compute_bounds(&gutter).map(|b| b.x());
        }
        child = widget.next_sibling();
    }
    let (x, width) = ours.expect("the git renderer is in the gutter");
    let review = review.expect("the review gutter is there");
    println!("  git renderer at x={x} ({width} px), review gutter at x={review}");
    assert!(x + width <= review, "left of the line numbers");
    assert!(width <= 8.0, "a thin bar");
    // Something to draw: the frame draws the bar without warnings.
    set_text(&editor, &TEXT.replace("one", "ONE"));
    f.wait_marks(&editor, "edited", |m| !m.is_empty());
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") {
        let png = PathBuf::from(dir).join("p-git-gutter-test.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &png)).unwrap();
    }
}

fn config_and_close() {
    let dir = repository("config", &[("main.tex", TEXT), ("b.tex", "b\n")]);
    let f = fixture(&dir);
    let editor = f.open("main.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    set_text(&editor, &TEXT.replace("one", "ONE"));
    f.wait_marks(&editor, "edited", |m| !m.is_empty());
    // Off: the renderer goes and git rests.
    let mut config = (*f.state.config.get()).clone();
    config.git.gutter = false;
    f.state.set_config(Rc::new(config.clone()));
    assert!(!git_gutter::has_renderer(&editor));
    assert_eq!(git_gutter::status(&editor), Some(Status::Off));
    let runs = git_gutter::git_runs();
    git_gutter::refresh(&f.state);
    let b = f.open("b.tex");
    assert!(!git_gutter::has_renderer(&b));
    assert_eq!(git_gutter::git_runs(), runs, "no git while off");
    // On again: both tabs get their marks.
    config.git.gutter = true;
    f.state.set_config(Rc::new(config));
    assert!(git_gutter::has_renderer(&editor), "known to be tracked");
    f.wait_status(&b, Status::Tracked { in_head: true });
    assert!(git_gutter::has_renderer(&b), "tracked once git answered");
    f.wait_marks(&editor, "on again", |m| {
        m.kind_of(2) == Some(MarkKind::Changed)
    });
    // Closing a tab removes the renderer and forgets the file.
    let weak = editor.view().downgrade();
    f.state.close_editor(&editor, true);
    common::pump_for(Duration::from_millis(50));
    assert!(git_gutter::marks(&editor).is_none());
    drop(editor);
    common::pump_for(Duration::from_millis(50));
    assert!(weak.upgrade().is_none(), "the closed view was freed");
    f.settle();
}

/// The git gutter's cost on `large.tex` (60k lines; release builds check the budgets): per
/// keystroke on the main thread (typing with the gutter on against typing with it off), and
/// per diff (the main thread only copies the text; hashing and diffing run on the jobs thread).
fn perf_large() {
    let text = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let dir = repository("perf", &[("large.tex", &text)]);
    let f = fixture(&dir);
    let editor = f.open("large.tex");
    f.wait_status(&editor, Status::Tracked { in_head: true });
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(30_000).unwrap());
    let type_keys = |n: usize| -> Vec<Duration> {
        let mut times = Vec::new();
        for i in 0..n {
            let started = Instant::now();
            buffer.insert_at_cursor(if i % 20 == 19 { "\n" } else { "x" });
            times.push(started.elapsed());
            common::pump_for(Duration::from_millis(2));
        }
        times.sort();
        times
    };
    let on = type_keys(300);
    let marks = f.wait_marks(&editor, "typed", |m| !m.is_empty());
    assert!(marks.kind_of(30_000).is_some(), "{marks:?}");
    let (main, jobs) = git_gutter::last_diff_cost(&editor).expect("a diff ran");
    let mut config = (*f.state.config.get()).clone();
    config.git.gutter = false;
    f.state.set_config(Rc::new(config));
    let off = type_keys(300);
    let median = |t: &[Duration]| t[t.len() / 2];
    let p99 = |t: &[Duration]| t[t.len() * 99 / 100];
    println!(
        "  keystroke p50 {:?} on / {:?} off, p99 {:?} on / {:?} off; diff: {main:?} main \
         thread, {jobs:?} jobs thread",
        median(&on),
        median(&off),
        p99(&on),
        p99(&off)
    );
    if !cfg!(debug_assertions) {
        assert!(
            median(&on).saturating_sub(median(&off)) < Duration::from_micros(200),
            "the gutter's share of a keystroke"
        );
        assert!(
            main < Duration::from_millis(2),
            "main thread per diff: {main:?}"
        );
        assert!(
            jobs < Duration::from_millis(60),
            "jobs thread per diff: {jobs:?}"
        );
    }
}
