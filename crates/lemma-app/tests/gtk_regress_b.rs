//! Regression tests for verified M1 defects in builds and diagnostics (review area B). Each test
//! fails until its defect is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress_b`. Tests that
//! build need latexmk and print `SKIP` without it.

mod common;

use adw::prelude::*;
use lemma_app::build_ui::Reason;
use lemma_app::diagnostics_panel::{ERROR_CATEGORY, Problem};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_build::{BuildOutcome, BuildStatus};
use lemma_config::Config;
use lemma_core::{DiagSource, ProjectRoot, Severity};
use sourceview5::prelude::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    common::run(&[
        (
            "regress_invalid_key_override_keeps_default_accels",
            regress_invalid_key_override_keeps_default_accels,
        ),
        (
            "regress_diag_marks_follow_edits_made_during_the_build",
            regress_diag_marks_follow_edits_made_during_the_build,
        ),
        (
            "regress_problem_click_goes_to_the_marked_line",
            regress_problem_click_goes_to_the_marked_line,
        ),
        (
            "regress_include_with_dot_slash_builds",
            regress_include_with_dot_slash_builds,
        ),
        (
            "regress_latin1_root_builds_when_not_open",
            regress_latin1_root_builds_when_not_open,
        ),
        (
            "regress_remembered_root_stays_in_its_project",
            regress_remembered_root_stays_in_its_project,
        ),
        (
            "regress_stop_cancels_a_request_being_prepared",
            regress_stop_cancels_a_request_being_prepared,
        ),
        (
            "regress_fifo_input_does_not_wedge_root_detection",
            regress_fifo_input_does_not_wedge_root_detection,
        ),
        (
            "regress_saving_the_bib_builds_its_document",
            regress_saving_the_bib_builds_its_document,
        ),
        (
            "regress_refilling_the_problems_list_is_not_quadratic",
            regress_refilling_the_problems_list_is_not_quadratic,
        ),
        (
            "regress_status_bar_names_the_earlier_pdf",
            regress_status_bar_names_the_earlier_pdf,
        ),
        (
            "regress_marks_follow_edits_in_a_reopened_file",
            regress_marks_follow_edits_in_a_reopened_file,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_TIMEOUT: Duration = Duration::from_secs(180);

fn have_latexmk() -> bool {
    let found = Command::new("latexmk")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        println!("  SKIP: latexmk is not installed");
    }
    found
}

/// A copy of the sources of `tests/fixtures/proj`.
fn copy_proj(name: &str) -> PathBuf {
    let dir = common::temp_dir(name);
    let src = common::fixtures().join("proj");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    for file in ["main.tex", "refs.bib", "chapters/yoneda.tex"] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    dir.canonicalize().unwrap()
}

/// Writes `files` (relative path, bytes) into a fresh test directory.
fn write_project(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = common::temp_dir(name);
    for (file, bytes) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    dir.canonicalize().unwrap()
}

fn config(on_save: bool) -> Config {
    let mut config = Config::default();
    config.build.on_save = on_save;
    config
}

/// A scripted window (never blocks on a dialog) whose project root is `dir`.
fn window(dir: &Path, config: Config) -> Rc<WindowState> {
    let workspace = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        None,
        workspace,
        true,
    );
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    state
}

fn open(state: &Rc<WindowState>, path: &Path) -> Rc<lemma_app::editor::Editor> {
    glib::MainContext::default()
        .block_on(state.open_file(path))
        .expect("open the document")
}

/// Records every finished build of `state`.
fn outcomes(state: &WindowState) -> Rc<RefCell<Vec<BuildOutcome>>> {
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let seen = outcomes.clone();
    state
        .build
        .get()
        .unwrap()
        .connect_finished(move |o| seen.borrow_mut().push(o.clone()));
    outcomes
}

/// Pumps the main loop until `done` or `limit`; whether `done` became true.
fn pump_while_not(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + limit;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// 0-based lines of `buffer` that carry an error mark.
fn error_mark_lines(buffer: &sourceview5::Buffer) -> Vec<i32> {
    (0..buffer.line_count())
        .filter(|&l| {
            !buffer
                .source_marks_at_line(l, Some(ERROR_CATEGORY))
                .is_empty()
        })
        .collect()
}

/// 0-based line of the first occurrence of `needle`.
fn line_of(buffer: &sourceview5::Buffer, needle: &str) -> Option<i32> {
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string();
    text.lines()
        .position(|l| l.contains(needle))
        .map(|l| l as i32)
}

const UNDEFINED: &str = "\\undefinedcommand ";

/// B14: an unparsable `[keys]` override of `win.stop-build` (likewise `win.sync-forward`,
/// `win.pdf-invert`) leaves the action with no accelerator at all; app and review actions keep
/// their default in the same case.
fn regress_invalid_key_override_keeps_default_accels() {
    let app = common::application();
    let mut config = config(false);
    for action in ["win.stop-build", "win.sync-forward", "win.pdf-invert"] {
        app.set_accels_for_action(action, &[]);
        config
            .keys
            .bindings
            .insert(action.into(), "Ctrl+Alt+J".into());
    }
    assert!(
        gtk::accelerator_parse("Ctrl+Alt+J").is_none(),
        "precondition: the override is invalid"
    );
    lemma_app::app::set_accels(&app, &config.keys);
    let dir = common::temp_dir("accels");
    let state = window(&dir, config);
    // Compared as parsed chords: GTK reports `<Control><Shift>i` as `<Shift><Control>i`.
    let parse = |accel: &str| gtk::accelerator_parse(accel).expect("a valid accelerator");
    let got: Vec<(&str, Vec<_>)> = ["win.stop-build", "win.sync-forward", "win.pdf-invert"]
        .into_iter()
        .map(|a| {
            (
                a,
                app.accels_for_action(a).iter().map(|s| parse(s)).collect(),
            )
        })
        .collect();
    state.window.destroy();
    lemma_app::app::set_accels(&app, &lemma_config::KeysCfg::default());
    assert_eq!(
        got,
        vec![
            ("win.stop-build", vec![parse("<Shift>F5")]),
            ("win.sync-forward", vec![parse("<Control><Alt>j")]),
            ("win.pdf-invert", vec![parse("<Control><Shift>i")]),
        ],
        "an invalid override must be ignored, keeping the default"
    );
}

/// B2/B4: lines added above an error while latexmk runs are not accounted for: the error's
/// gutter mark (and underline) lands on the line number the saved snapshot had.
fn regress_diag_marks_follow_edits_made_during_the_build() {
    if !have_latexmk() {
        return;
    }
    let dir = copy_proj("drift-during");
    let state = window(&dir, config(true));
    let editor = open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.iter_at_line(65).unwrap(), UNDEFINED);
    state.save_document(editor.document());
    common::pump_until(TIMEOUT, || build.is_running());
    // The user keeps typing while the build runs: three new lines at the top.
    buffer.insert(&mut buffer.start_iter(), "%\n%\n%\n");
    common::pump_until(BUILD_TIMEOUT, || !outcomes.borrow().is_empty());
    let problems = build.diagnostics().problems();
    assert!(
        problems
            .iter()
            .any(|p| p.line == Some(66) && p.severity == Severity::Error),
        "precondition: the build reports the error at the saved line 66: {problems:#?}"
    );
    let text_line = line_of(buffer, UNDEFINED.trim()).unwrap();
    let marks = error_mark_lines(buffer);
    state.window.destroy();
    assert_eq!(
        text_line, 68,
        "precondition: the error text moved down 3 lines"
    );
    assert_eq!(
        marks,
        vec![text_line],
        "the error mark must sit on the line holding the error text (0-based {text_line})"
    );
}

/// B2/B16: after an edit above an error the gutter mark follows the text, but activating the
/// problem in the Problems list jumps to the build's stale line number.
fn regress_problem_click_goes_to_the_marked_line() {
    if !have_latexmk() {
        return;
    }
    let dir = copy_proj("drift-after");
    let state = window(&dir, config(true));
    let editor = open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.iter_at_line(65).unwrap(), UNDEFINED);
    state.save_document(editor.document());
    common::pump_until(BUILD_TIMEOUT, || !outcomes.borrow().is_empty());
    let problems = build.diagnostics().problems();
    let index = problems
        .iter()
        .position(|p| p.line == Some(66) && p.severity == Severity::Error)
        .unwrap_or_else(|| panic!("precondition: an error at line 66: {problems:#?}"));
    // One new line at the top, after the build.
    buffer.insert(&mut buffer.start_iter(), "%\n");
    let marks = error_mark_lines(buffer);
    assert_eq!(marks, vec![66], "precondition: the mark followed the text");
    buffer.place_cursor(&buffer.start_iter());
    build.diagnostics().activate(index);
    let moved = pump_while_not(TIMEOUT, || {
        buffer.iter_at_mark(&buffer.get_insert()).line() != 0
    });
    let cursor = buffer.iter_at_mark(&buffer.get_insert()).line();
    state.window.destroy();
    assert!(moved, "activating the problem did not move the cursor");
    assert_eq!(
        cursor, 66,
        "activating the problem must go to the marked line (0-based 66), not the build's line"
    );
}

/// B3: `\include{./chapters/intro}` (a leading `./`) yields no mirror directory, so TeX cannot
/// write `.build/chapters/intro.aux` and the build fails.
fn regress_include_with_dot_slash_builds() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "include-dot",
        &[
            (
                "main.tex",
                b"\\documentclass{article}\n\\begin{document}\n\\include{./chapters/intro}\n\\end{document}\n",
            ),
            ("chapters/intro.tex", b"Hello from an included chapter.\n"),
        ],
    );
    let state = window(&dir, config(false));
    open(&state, &dir.join("main.tex"));
    let outcomes = outcomes(&state);
    state.build.get().unwrap().request(Reason::Manual);
    common::pump_until(BUILD_TIMEOUT, || !outcomes.borrow().is_empty());
    let outcome = outcomes.borrow()[0].clone();
    let problems = state.build.get().unwrap().diagnostics().problems();
    state.window.destroy();
    assert_eq!(
        outcome.status,
        BuildStatus::Success,
        "problems: {:?}",
        problems.iter().map(|p| &p.message).collect::<Vec<_>>()
    );
}

/// B6: a Latin-1 main document that is not open is read as strict UTF-8, so detection says it
/// "does not exist" and nothing is built.
fn regress_latin1_root_builds_when_not_open() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "latin1",
        &[
            (
                "main.tex",
                b"\\documentclass{article}\n\\usepackage[latin1]{inputenc}\n\\begin{document}\nCaf\xe9.\n\\input{chapters/a}\n\\end{document}\n",
            ),
            (
                "chapters/a.tex",
                b"% !TEX root = ../main.tex\nSome text.\n",
            ),
        ],
    );
    let state = window(&dir, config(false));
    open(&state, &dir.join("chapters/a.tex"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    build.request(Reason::Manual);
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    let status = state.status.build.text().to_string();
    let built = outcomes.borrow().len();
    state.window.destroy();
    assert_eq!(built, 1, "no build ran; status bar: {status:?}");
}

/// B7: the window's remembered root applies to any file without a detectable root, even in an
/// unrelated directory: saving `B/notes.tex` builds project A.
fn regress_remembered_root_stays_in_its_project() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "remembered",
        &[
            (
                "A/main.tex",
                b"\\documentclass{article}\n\\begin{document}\nProject A.\n\\end{document}\n",
            ),
            ("B/notes.tex", b"Some unrelated notes.\n"),
        ],
    );
    let state = window(&dir.join("A"), config(false));
    open(&state, &dir.join("A/main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    build.request(Reason::Manual);
    common::pump_until(BUILD_TIMEOUT, || !outcomes.borrow().is_empty());
    assert_eq!(build.root(), Some(dir.join("A/main.tex")));

    open(&state, &dir.join("B/notes.tex"));
    build.request(Reason::Save);
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    let status = state.status.build.text().to_string();
    let builds = outcomes.borrow().len();
    state.window.destroy();
    assert_eq!(
        builds,
        1,
        "saving B/notes.tex built {:?}; status bar: {status:?}",
        dir.join("A/main.tex")
    );
}

/// B13: Stop while a request is still saving or detecting does not cancel it; its build starts
/// afterwards.
fn regress_stop_cancels_a_request_being_prepared() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "stop-preparing",
        &[(
            "main.tex",
            b"\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n",
        )],
    );
    let state = window(&dir, config(false));
    open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    build.request(Reason::Manual);
    assert!(
        build.is_busy() && !build.is_running(),
        "precondition: preparing"
    );
    WidgetExt::activate_action(&state.window, "win.stop-build", None).unwrap();
    let mut started = false;
    common::pump_until(BUILD_TIMEOUT, || {
        started |= build.is_running();
        !build.is_busy()
    });
    let builds = outcomes.borrow().len();
    state.window.destroy();
    assert!(
        !started && builds == 0,
        "Stop was ignored: a build started (finished builds: {builds})"
    );
}

/// B15: root detection reads `\input` targets with an unbounded `read_to_string` and no
/// regular-file check (`metadata().len()` is 0 for a FIFO, /dev/zero, …). A FIFO blocks the
/// jobs thread forever: the request stays "preparing" and no build ever starts.
fn regress_fifo_input_does_not_wedge_root_detection() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "fifo",
        &[(
            "main.tex",
            b"\\documentclass{article}\n\\begin{document}\n\\input{ch}\n\\end{document}\n",
        )],
    );
    let fifo = dir.join("ch.tex");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success(), "mkfifo");
    let state = window(&dir, config(false));
    open(&state, &dir.join("main.tex"));
    let build = state.build.get().unwrap();
    build.request(Reason::Manual);
    // Detection is done once a build runs (TeX may then block on the FIFO; that is latexmk's
    // timeout's business) or the request ended.
    let detected = pump_while_not(Duration::from_secs(15), || {
        build.is_running() || !build.is_busy()
    });
    // Clean up either way: release a reader blocked in open(), replace the FIFO by a file and
    // stop any build.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo);
    }
    std::fs::remove_file(&fifo).unwrap();
    std::fs::write(&fifo, "Chapter.\n").unwrap();
    let _ = pump_while_not(Duration::from_secs(5), || build.is_running());
    build.stop();
    let _ = pump_while_not(Duration::from_secs(30), || !build.is_busy());
    state.window.destroy();
    assert!(
        detected,
        "root detection was still blocked on the FIFO ch.tex after 15 s"
    );
}

/// B18: with refs.bib as the active file, root detection ignores `\bibliography{refs}`, so a
/// save of the .bib file builds nothing.
fn regress_saving_the_bib_builds_its_document() {
    if !have_latexmk() {
        return;
    }
    let dir = write_project(
        "bib",
        &[
            (
                "main.tex",
                b"\\documentclass{article}\n\\begin{document}\nSee \\cite{knuth}.\n\\bibliographystyle{plain}\n\\bibliography{refs}\n\\end{document}\n",
            ),
            (
                "refs.bib",
                b"@book{knuth, author = {D. Knuth}, title = {TAOCP}, publisher = {AW}, year = {1968}}\n",
            ),
        ],
    );
    let state = window(&dir, config(true));
    open(&state, &dir.join("refs.bib"));
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    build.request(Reason::Save);
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    let status = state.status.build.text().to_string();
    let builds = outcomes.borrow().len();
    let root = build.root();
    state.window.destroy();
    assert_eq!(
        builds, 1,
        "saving refs.bib built nothing; status bar: {status:?}"
    );
    assert_eq!(root, Some(dir.join("main.tex")));
}

/// B10: `rebuild_list` removes the old rows one at a time from the top; each removal reruns the
/// header function on the new first row, which counts all (new) problems of its file. Refilling
/// the list is O(old × new) on the main thread.
///
/// The cost of replacing n rows by n rows, minus the cost of filling an empty list, is the
/// removal cost. It must grow about linearly (×6 from 1500 to 9000 rows) or stay below half the
/// fill cost; quadratic removal grows about ×20.
fn regress_refilling_the_problems_list_is_not_quadratic() {
    let dir = common::temp_dir("list-perf");
    let root = dir.join("main.tex");
    std::fs::write(&root, "x\n").unwrap();
    let state = window(&dir, config(false));
    let panel = state.build.get().unwrap().diagnostics();
    let problems = |n: usize| -> Vec<Problem> {
        (0..n)
            .map(|i| Problem {
                path: root.clone(),
                line: Some(u32::try_from(i).unwrap() + 1),
                severity: Severity::BadBox,
                message: format!("Overfull \\hbox ({i}.0pt too wide) in paragraph"),
                context: None,
                source: DiagSource::Tex,
            })
            .collect()
    };
    let timed = |p: Vec<Problem>| {
        let started = Instant::now();
        panel.set(p, &root);
        started.elapsed()
    };
    let mut removal = Vec::new();
    for n in [1500, 9000] {
        let fill = timed(problems(n));
        let refill = timed(problems(n));
        println!("  {n} problems: fill {fill:.2?}, refill {refill:.2?}");
        removal.push((fill, refill.saturating_sub(fill)));
        timed(Vec::new());
    }
    state.window.destroy();
    let (_, small) = removal[0];
    let (fill, large) = removal[1];
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-3);
    assert!(
        large < fill / 2 || ratio < 10.0,
        "removing 9000 rows took {large:.2?} ({ratio:.1}× the time for 1500 rows; filling 9000 \
         took {fill:.2?})"
    );
}

/// Known item: while the preview shows the PDF an earlier session built and this session has not
/// built yet, the status bar said "Not built".
fn regress_status_bar_names_the_earlier_pdf() {
    let dir = write_project(
        "earlier-pdf",
        &[(
            "main.tex",
            b"\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n",
        )],
    );
    let pdf = dir.join(".build/main.pdf");
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf).unwrap();
    let state = window(&dir, config(false));
    open(&state, &dir.join("main.tex"));
    let controller = state.pdf.get().unwrap();
    let shown = pump_while_not(TIMEOUT, || {
        controller.pdf().is_some() && state.status.build.text() != "Not built"
    });
    let status = state.status.build.text().to_string();
    let requested = controller.requested_pdf();
    state.window.destroy();
    assert_eq!(
        requested,
        Some(pdf),
        "precondition: the earlier PDF is shown"
    );
    assert!(
        shown && status.starts_with("PDF from "),
        "status bar: {status:?}"
    );
}

/// B2: a file edited (and saved) after the build, closed and opened again gets its marks on the
/// moved lines: the reopened document is followed from where the old one left off.
fn regress_marks_follow_edits_in_a_reopened_file() {
    if !have_latexmk() {
        return;
    }
    let dir = copy_proj("drift-reopen");
    let state = window(&dir, config(false));
    let main = dir.join("main.tex");
    let editor = open(&state, &main);
    let build = state.build.get().unwrap();
    let outcomes = outcomes(&state);
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.iter_at_line(65).unwrap(), UNDEFINED);
    build.request(Reason::Manual);
    common::pump_until(BUILD_TIMEOUT, || !outcomes.borrow().is_empty());
    assert!(
        build
            .diagnostics()
            .problems()
            .iter()
            .any(|p| p.line == Some(66) && p.severity == Severity::Error),
        "precondition: an error at line 66"
    );
    // Two lines more at the top, saved (no build), then the tab is closed and reopened.
    buffer.insert(&mut buffer.start_iter(), "%\n%\n");
    let doc = editor.document().clone();
    glib::MainContext::default()
        .block_on(doc.save())
        .expect("save main.tex");
    state.close_editor(&editor, false);
    drop((editor, doc));
    common::pump_until(TIMEOUT, || state.editors().is_empty());
    let reopened = open(&state, &main);
    let buffer = reopened.buffer();
    let text_line = line_of(buffer, UNDEFINED.trim()).unwrap();
    let marks = error_mark_lines(buffer);
    state.window.destroy();
    assert_eq!(
        text_line, 67,
        "precondition: the error text moved down 2 lines"
    );
    assert_eq!(
        marks,
        vec![text_line],
        "the error mark must follow the saved edit"
    );
}
