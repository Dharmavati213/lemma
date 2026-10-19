//! Regression tests for verified area-E2 defects of the second review pass (completion,
//! snippets, the key layer, Vim, the project index). Each test reproduces one defect and fails
//! until it is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_e2`.
//!
//! - Defects that hang the app run their scenario in a child process (this binary again, with
//!   `LEMMA_REGRESS2_E2_CHILD=<probe>`); the test fails when the child is still running after a
//!   timeout.
//! - Defects that need GtkSourceView's own Tab handling (snippet stops) run on real key events
//!   in a child on a private Broadway daemon (`broadway_input`).
//!
//! No test can start a real agent: the configuration points at no adapter and the agent
//! scenario does not exist.

mod broadway_input;
mod common;

use adw::prelude::*;
use broadway_input::Browser;
use gtk::gdk;
use lemma_app::agent::AgentState;
use lemma_app::agent::panel::{self, PanelCommand};
use lemma_app::editor::completion;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, VimMode, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

const HANG_CHILD_ENV: &str = "LEMMA_REGRESS2_E2_CHILD";
const REAL_CHILD: &str = "real_input_nested_snippets";

fn main() -> ExitCode {
    if let Ok(probe) = std::env::var(HANG_CHILD_ENV) {
        return run_hang_child(&probe);
    }
    if broadway_input::is_child() {
        return common::run(&[(REAL_CHILD, real_nested_snippets_child)]);
    }
    common::run(&[
        // E2-1
        (
            "regress2_agentstop_bang_keeps_the_agent_stopped",
            regress2_agentstop_bang_keeps_the_agent_stopped,
        ),
        // E2-2 / E2-11
        (
            "regress2_end_offers_the_open_math_environment",
            regress2_end_offers_the_open_math_environment,
        ),
        // E2-3
        (
            "regress2_no_latex_autosnippets_in_other_files",
            regress2_no_latex_autosnippets_in_other_files,
        ),
        // E2-4
        (
            "regress2_begin_accept_closes_nested_and_commented",
            regress2_begin_accept_closes_nested_and_commented,
        ),
        // E2-8
        (
            "regress2_crlf_environment_expansion_keeps_crlf",
            regress2_crlf_environment_expansion_keeps_crlf,
        ),
        // E2-9
        (
            "regress2_tab_step_out_skips_opening_and_escaped_dollars",
            regress2_tab_step_out_skips_opening_and_escaped_dollars,
        ),
        // E2-10
        (
            "regress2_vim_counted_join_at_the_end_returns",
            regress2_vim_counted_join_at_the_end_returns,
        ),
        (
            "regress2_vim_dot_repeated_join_at_the_end_returns",
            regress2_vim_dot_repeated_join_at_the_end_returns,
        ),
        // E2-12
        (
            "regress2_sibling_sharing_macros_is_not_promoted",
            regress2_sibling_sharing_macros_is_not_promoted,
        ),
        // E2-13
        (
            "regress2_closed_tab_leaves_the_index",
            regress2_closed_tab_leaves_the_index,
        ),
        // E2-14
        (
            "regress2_reindex_never_merges_an_older_result_last",
            regress2_reindex_never_merges_an_older_result_last,
        ),
        // E2-15
        (
            "regress2_chapter_first_lists_files_from_the_root",
            regress2_chapter_first_lists_files_from_the_root,
        ),
        // E2-16 / E2-17 / E2-18
        (
            "regress2_nested_and_replacement_snippets_on_real_keys",
            regress2_nested_and_replacement_snippets_on_real_keys,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(10);
/// Marks the cursor in the texts given to [`Fixture::set`].
const CURSOR: char = '‸';

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    dir: PathBuf,
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

/// A window on `dir` (the workspace root) showing `open`.
fn fixture_in(dir: &Path, open: &str, vim: bool) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = vim;
    config.spell.word_lists = Vec::new();
    config.spell.enabled = false;
    // Never a real agent: no adapter can be found here.
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    lemma_app::app::set_accels(&app, &state.config.keys);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    if let Some(agent) = state.agent_controller() {
        agent.set_scenario(Some(dir.join("no-such-scenario.json")));
    }
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let editor = block_on(state.open_file(&dir.join(open))).expect("open");
    common::pump_until(TIMEOUT, || editor.view().is_focus());
    let f = Fixture {
        state,
        editor,
        dir: dir.to_path_buf(),
    };
    f.settle();
    f
}

fn fixture(name: &str, files: &[(&str, &str)], open: &str, vim: bool) -> Fixture {
    let dir = common::temp_dir(name);
    write_files(&dir, files);
    fixture_in(&dir, open, vim)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    fn open(&self, rel: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(rel))).expect("open");
        common::pump_until(TIMEOUT, || editor.view().is_focus());
        self.settle();
        editor
    }

    /// Waits until the project feed has nothing left to do.
    fn settle(&self) {
        completion::refresh_project(&self.state);
        common::pump_until(TIMEOUT, || !completion::project_busy(&self.state));
    }

    /// Replaces `editor`'s text with `marked` (without the [`CURSOR`] mark) and puts the cursor
    /// at the mark. Not `set_text`: Vim's insert mode keeps a user action open.
    fn set(&self, editor: &Editor, marked: &str) {
        lemma_app::editor::snippets::end_snippet_sessions(editor.view());
        let at = marked.find(CURSOR).expect("a cursor mark");
        let cursor = i32::try_from(marked[..at].chars().count()).unwrap();
        let text = marked.replace(CURSOR, "");
        let buffer = editor.buffer();
        let (mut start, mut end) = buffer.bounds();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &text);
        buffer.place_cursor(&buffer.iter_at_offset(cursor));
        common::pump_for(Duration::from_millis(5));
    }

    /// One key press as GTK delivers it: the key layer, then Vim, then the text view's
    /// insertion (a Tab nobody consumed indents by two spaces, as the view does).
    fn press(&self, editor: &Editor, keyval: gdk::Key, state: gdk::ModifierType) {
        let display = WidgetExt::display(&self.state.window);
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(gdk::KeymapKey::level));
        let state = match &key {
            Some(k) if k.level() > 0 => state | gdk::ModifierType::SHIFT_MASK,
            _ => state,
        };
        let event = KeyEvent {
            keyval,
            keycode: key.map_or(0, |k| k.keycode()),
            state,
            vim: editor.vim().is_some(),
        };
        let layer = keys::dispatch(editor.view(), &event);
        if layer == glib::Propagation::Proceed && !editor.vim_blocks_key(keyval, state) {
            let consumed = editor
                .vim()
                .is_some_and(|im| vim::feed_key(&im, editor.view(), keyval, state));
            let modified =
                state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            if !consumed && !modified {
                let text = match keyval {
                    gdk::Key::Return => Some("\n".to_owned()),
                    gdk::Key::Tab => Some("  ".to_owned()),
                    k => k.to_unicode().filter(|c| !c.is_control()).map(String::from),
                };
                if let Some(text) = text {
                    editor.buffer().insert_interactive_at_cursor(&text, true);
                }
            }
        }
        common::pump_for(Duration::from_millis(2));
    }

    fn type_text(&self, editor: &Editor, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\t' => gdk::Key::Tab,
                '\n' => gdk::Key::Return,
                '\x1b' => gdk::Key::Escape,
                c => broadway_input::keyval(c),
            };
            self.press(editor, key, gdk::ModifierType::empty());
        }
    }

    fn labels(&self, editor: &Editor) -> Vec<String> {
        completion::items_at_cursor(&self.state, editor)
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    fn accept(&self, editor: &Editor, label: &str) {
        let item = completion::items_at_cursor(&self.state, editor)
            .into_iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| {
                panic!("no proposal {label:?}; proposals {:?}", self.labels(editor))
            });
        completion::accept(&self.state, editor, &item);
        common::pump_for(Duration::from_millis(5));
    }

    fn mode(&self, editor: &Editor) -> VimMode {
        editor.vim_status().expect("Vim is on").mode
    }

    fn cursor_offset(&self, editor: &Editor) -> i32 {
        let buffer = editor.buffer();
        buffer.iter_at_mark(&buffer.get_insert()).offset()
    }
}

// ───────────── E2-1: `:AgentStop!` behind the panel's back ─────────────

/// `:AgentStop!` must stop the agent as the panel's "Stop Agent (Free Memory)" does: showing the
/// pane again must not start it.
fn regress2_agentstop_bang_keeps_the_agent_stopped() {
    let f = fixture(
        "agentstop",
        &[("main.tex", "line 0\nline 1\n")],
        "main.tex",
        true,
    );
    let agent = f.state.agent_controller().expect("an agent").clone();
    let commands: Rc<RefCell<Vec<PanelCommand>>> = Rc::default();
    let log = commands.clone();
    // Every command is handled here, so no agent process is ever started.
    panel::set_command_hook(
        &f.state,
        Some(Box::new(move |command: &PanelCommand| {
            log.borrow_mut().push(command.clone());
            true
        })),
    );
    f.state.agent.split.set_show_sidebar(true);
    common::pump_for(Duration::from_millis(30));
    agent.set_state(AgentState::Idle);
    common::pump_for(Duration::from_millis(30));
    // Back to the editor, in normal mode, and stop the agent process.
    f.state.agent.split.set_show_sidebar(false);
    f.editor.view().grab_focus();
    common::pump_for(Duration::from_millis(30));
    for key in [":", "A", "g", "e", "n", "t", "S", "t", "o", "p", "!"] {
        let c = key.chars().next().unwrap();
        f.press(
            &f.editor,
            broadway_input::keyval(c),
            gdk::ModifierType::empty(),
        );
    }
    f.press(&f.editor, gdk::Key::Return, gdk::ModifierType::empty());
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    commands.borrow_mut().clear();
    // The user looks at the transcript: hide and show the pane.
    f.state.agent.split.set_show_sidebar(true);
    common::pump_for(Duration::from_millis(50));
    let seen = commands.borrow().clone();
    let card = panel::get(&f.state).and_then(|p| p.setup());
    panel::set_command_hook(&f.state, None);
    assert!(
        !seen.contains(&PanelCommand::Start),
        "showing the pane after :AgentStop! started the agent again: commands {seen:?}, card \
         {card:?}"
    );
}

// ───────────── E2-2 / E2-11: `\end{` inside display math ─────────────

fn regress2_end_offers_the_open_math_environment() {
    let doc = |body: &str| {
        format!(
            "\\documentclass{{article}}\n\\usepackage{{amsmath}}\n\\begin{{document}}\n{body}\n\
             \\end{{document}}\n"
        )
    };
    let f = fixture(
        "end-math-env",
        &[("main.tex", &doc("Text."))],
        "main.tex",
        false,
    );
    let cases = [
        ("align", "\\begin{align}\n  a &= b\n\\end{al‸"),
        ("equation", "\\begin{equation}\n  a = b\n\\end{equation‸"),
        ("gather*", "\\begin{gather*}\n  a = b\n\\end{ga‸"),
    ];
    let mut failures = Vec::new();
    for (env, body) in cases {
        f.set(&f.editor, &doc(body));
        let labels = f.labels(&f.editor);
        if labels.first().map(String::as_str) != Some(env) {
            failures.push(format!(
                "\\end{{ inside {env}: proposals {labels:?} (the first row is what Enter accepts)"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// ───────────── E2-3: LaTeX autosnippets in other files ─────────────

fn regress2_no_latex_autosnippets_in_other_files() {
    let f = fixture(
        "non-latex",
        &[
            (
                "main.tex",
                "\\documentclass{article}\n\\begin{document}\nText.\n\\end{document}\n",
            ),
            ("NOTES.md", "# Notes\n\n"),
            ("latexmkrc", "# rc\n\n"),
        ],
        "main.tex",
        false,
    );
    let notes = f.open("NOTES.md");
    f.set(&notes, "# Notes\n\n‸");
    f.type_text(&notes, "Run mkdir build first.");
    let rc = f.open("latexmkrc");
    f.set(&rc, "# rc\n‸\n");
    f.type_text(&rc, "$a ff");
    let (notes_text, rc_text) = (notes.document().text(), rc.document().text());
    assert!(
        notes_text.contains("Run mkdir build first.") && rc_text.contains("$a ff"),
        "LaTeX autosnippets changed typed text in non-LaTeX files: NOTES.md {notes_text:?}, \
         latexmkrc {rc_text:?}"
    );
}

// ───────────── E2-4: `\begin{` accepted inside a same-name environment ─────────────

fn regress2_begin_accept_closes_nested_and_commented() {
    let f = fixture(
        "begin-nested",
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nText.\n\\end{document}\n",
        )],
        "main.tex",
        false,
    );
    // Inside an itemize: a new nested itemize.
    f.set(
        &f.editor,
        "\\documentclass{article}\n\\begin{document}\n\\begin{itemize}\n  \\item a\n  \
         \\begin{it‸\n  \\item b\n\\end{itemize}\n\\end{document}\n",
    );
    f.accept(&f.editor, "itemize");
    let nested = f.editor.document().text();
    // A commented-out `\end{center}` further down.
    f.set(
        &f.editor,
        "\\documentclass{article}\n\\begin{document}\n\\begin{cen‸\n\nText.\n% old: \
         \\end{center}\n\\end{document}\n",
    );
    f.accept(&f.editor, "center");
    let commented = f.editor.document().text();
    let uncommented_ends = |text: &str, env: &str| {
        text.lines()
            .map(|l| l.split('%').next().unwrap_or(""))
            .map(|l| l.matches(&format!("\\end{{{env}}}")).count())
            .sum::<usize>()
    };
    let begins = |text: &str, env: &str| text.matches(&format!("\\begin{{{env}}}")).count();
    let mut failures = Vec::new();
    if begins(&nested, "itemize") != uncommented_ends(&nested, "itemize") {
        failures.push(format!("nested itemize left unbalanced: {nested:?}"));
    }
    if begins(&commented, "center") != uncommented_ends(&commented, "center") {
        failures.push(format!(
            "a commented \\end{{center}} counted as closing: {commented:?}"
        ));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// ───────────── E2-8: CRLF documents ─────────────

fn regress2_crlf_environment_expansion_keeps_crlf() {
    let text = "\\documentclass{article}\r\n\\begin{document}\r\nText.\r\n\\end{document}\r\n";
    let f = fixture("crlf", &[("main.tex", text)], "main.tex", false);
    f.set(
        &f.editor,
        "\\documentclass{article}\r\n\\begin{document}\r\n\\begin{al‸\r\n\\end{document}\r\n",
    );
    f.accept(&f.editor, "align");
    let result = f.editor.document().text();
    assert!(result.contains("\\begin{align}"), "{result:?}");
    let bare_lf = result
        .char_indices()
        .filter(|&(i, c)| c == '\n' && (i == 0 || result.as_bytes()[i - 1] != b'\r'))
        .count();
    assert_eq!(
        bare_lf, 0,
        "LF-only line breaks in a CRLF document: {result:?}"
    );
}

// ───────────── E2-9: Tab step-out ─────────────

fn regress2_tab_step_out_skips_opening_and_escaped_dollars() {
    use lemma_app::editor::snippets::step_out_chars;
    let mut failures = Vec::new();
    // An escaped `\$` or `\}` is no closing delimiter: the `}` at the end is.
    if step_out_chars(r"50\$ off}") != Some(9) {
        failures.push(format!(
            r"step_out_chars('50\$ off}}') = {:?}, expected Some(9)",
            step_out_chars(r"50\$ off}")
        ));
    }
    if step_out_chars(r"a\} b}") != Some(6) {
        failures.push(format!(
            r"step_out_chars('a\}} b}}') = {:?}, expected Some(6)",
            step_out_chars(r"a\} b}")
        ));
    }
    // In text before an inline formula, Tab must not jump past its opening `$` into it.
    let doc = "\\documentclass{article}\n\\begin{document}\n‸$f$ is injective\n\\end{document}\n";
    let f = fixture(
        "step-out",
        &[("main.tex", &doc.replace('‸', ""))],
        "main.tex",
        false,
    );
    f.set(&f.editor, doc);
    let line_start = f.cursor_offset(&f.editor);
    f.press(&f.editor, gdk::Key::Tab, gdk::ModifierType::empty());
    let line = f
        .editor
        .document()
        .text()
        .lines()
        .nth(2)
        .unwrap()
        .to_owned();
    let col = f.cursor_offset(&f.editor) - line_start;
    if line == "$f$ is injective" && col == 1 {
        failures.push(format!(
            "Tab before `$f$` in text moved the cursor into the formula (line {line:?}, column \
             {col})"
        ));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// ───────────── E2-10: counted and repeated joins at the end of a file ─────────────

fn regress2_vim_counted_join_at_the_end_returns() {
    let status = hang_child("counted_join", Duration::from_secs(30));
    assert_eq!(
        status,
        Ok(Some(0)),
        "2J/3J that reach the empty last line must return"
    );
}

fn regress2_vim_dot_repeated_join_at_the_end_returns() {
    let status = hang_child("dot_join", Duration::from_secs(30));
    assert_eq!(
        status,
        Ok(Some(0)),
        "`.` repeating J onto the empty last line must return"
    );
}

/// Runs this binary with `LEMMA_REGRESS2_E2_CHILD=probe`: `Ok(code)` when it exited, `Err` when
/// it was still running after `timeout` (it is killed).
fn hang_child(probe: &str, timeout: Duration) -> Result<Option<i32>, String> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env(HANG_CHILD_ENV, probe)
        .env("LEMMA_CONFIG", "/dev/null")
        .spawn()
        .expect("spawn the child");
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            println!("  child {probe}: {status}");
            return Ok(status.code());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "child {probe} still running after {timeout:?} (hang)"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_hang_child(probe: &str) -> ExitCode {
    if gtk::init().is_err() {
        println!("SKIP: no display");
        return ExitCode::SUCCESS;
    }
    sourceview5::init();
    match probe {
        "counted_join" => child_counted_join(),
        "dot_join" => child_dot_join(),
        other => panic!("unknown probe {other}"),
    }
}

/// Puts the cursor on `line` (0-based, column 0) in normal mode and types `keys`.
fn join_case(name: &str, text: &str, line: i32, keys: &str) -> String {
    let f = fixture(name, &[("a.tex", text)], "a.tex", true);
    let ed = f.editor.clone();
    let buffer = ed.buffer();
    buffer.place_cursor(&buffer.iter_at_line(line).unwrap());
    assert_eq!(f.mode(&ed), VimMode::Normal);
    for c in keys.chars() {
        println!(
            "  {name}: pressing {c:?} (Vim pending {:?})",
            ed.vim_status().map(|s| s.command)
        );
        f.type_text(&ed, &c.to_string());
    }
    let result = ed.document().text();
    println!("  {name}: returned {result:?}");
    result
}

fn child_counted_join() -> ExitCode {
    join_case(
        "child-2j",
        "\\documentclass{article}\n\\begin{document}\nLast words.\n\\end{document}\n",
        2,
        "2J",
    );
    join_case("child-3j", "a\nb\nc\nd\ne\nf\n", 3, "3J");
    ExitCode::SUCCESS
}

fn child_dot_join() -> ExitCode {
    join_case(
        "child-jdot",
        "\\section{End}\nLast\nparagraph\nhere.\n",
        1,
        "J..",
    );
    ExitCode::SUCCESS
}

// ───────────── E2-12: siblings that share an included file ─────────────

fn regress2_sibling_sharing_macros_is_not_promoted() {
    let f = fixture(
        "sibling",
        &[
            (
                "paper.tex",
                "\\documentclass{article}\n\\input{macros}\n\\begin{document}\n\
                 \\section{Intro}\\label{p:intro}\nText.\n\\end{document}\n",
            ),
            ("macros.tex", "\\newcommand{\\R}{\\mathbb{R}}\n"),
            (
                "talk.tex",
                "% Slides for the paper.\n\\documentclass{beamer}\n\\input{macros}\n\
                 \\newcommand{\\N}{N}\n\\begin{document}\n\\begin{frame}\\label{talk:only}\
                 \\end{frame}\n\\end{document}\n",
            ),
        ],
        "paper.tex",
        false,
    );
    f.settle();
    let files: Vec<String> = f
        .state
        .workspace
        .index
        .borrow()
        .files
        .keys()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    f.set(
        &f.editor,
        "\\documentclass{article}\n\\input{macros}\n\\begin{document}\n\
         \\section{Intro}\\label{p:intro}\nSee \\ref{‸}.\n\\end{document}\n",
    );
    let refs = f.labels(&f.editor);
    let n_defined = completion::macro_defined(&f.state, "N");
    assert!(
        !files.iter().any(|f| f == "talk.tex")
            && !refs.iter().any(|l| l == "talk:only")
            && !n_defined,
        "an unrelated sibling that inputs the same macros file joined the project: index files \
         {files:?}, \\ref{{ proposals {refs:?}, \\N defined {n_defined}"
    );
}

// ───────────── E2-13: closed tabs stay in the index ─────────────

fn regress2_closed_tab_leaves_the_index() {
    let dir = common::temp_dir("stale-index");
    write_files(
        &dir,
        &[
            (
                "a/paperA.tex",
                "\\documentclass{article}\n\\newcommand{\\N}{\\mathbb{N}}\n\\begin{document}\n\
                 \\section{A}\\label{a:secret}\n\\end{document}\n",
            ),
            (
                "b/paperB.tex",
                "\\documentclass{article}\n\\begin{document}\n\\section{B}\\label{b:own}\n\
                 Text.\n\\end{document}\n",
            ),
        ],
    );
    let f = fixture_in(&dir, "a/paperA.tex", false);
    let a = f.editor.clone();
    let b = f.open("b/paperB.tex");
    f.state.close_editor(&a, true);
    common::pump_until(TIMEOUT, || f.state.editors().len() == 1);
    drop(a);
    f.settle();
    f.set(
        &b,
        "\\documentclass{article}\n\\begin{document}\n\\section{B}\\label{b:own}\n\
         See \\ref{‸}.\n\\end{document}\n",
    );
    let refs = f.labels(&b);
    let n_defined = completion::macro_defined(&f.state, "N");
    let files: Vec<String> = f
        .state
        .workspace
        .index
        .borrow()
        .files
        .keys()
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        !refs.iter().any(|l| l == "a:secret") && !n_defined,
        "a closed document of another project still feeds completion: \\ref{{ in paperB \
         {refs:?}, \\N defined {n_defined}, index files {files:?}"
    );
}

// ───────────── E2-14: re-index jobs out of order ─────────────

fn regress2_reindex_never_merges_an_older_result_last() {
    // A block that takes the structure pass a long time (unclosed `\label{` arguments).
    let slow: String = "\\label{x \n".repeat(6000);
    let text = format!(
        "\\documentclass{{article}}\n\\begin{{document}}\n\\section{{S}}\\label{{stale:one}}\n\
         {slow}\\end{{document}}\n"
    );
    // How long one pass takes here.
    let started = Instant::now();
    let _ = lemma_syntax::index_file(&text);
    let pass = started.elapsed();
    println!("  one structure pass over the slow block: {pass:?}");
    let f = fixture("reindex-race", &[("main.tex", &text)], "main.tex", false);
    let buffer = f.editor.buffer();
    let fast = "\\documentclass{article}\n\\begin{document}\n\\section{S}\nText.\n\
                \\end{document}\n";
    let replace_all = |text: &str| {
        let (mut start, mut end) = buffer.bounds();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, text);
    };
    let wait_idle = || {
        let deadline = Instant::now() + Duration::from_secs(120);
        common::pump_until(Duration::from_secs(121), || {
            !completion::project_busy(&f.state) || Instant::now() > deadline
        });
        common::pump_for(Duration::from_millis(100));
    };
    let main = f.dir.join("main.tex");
    let indexed_stale = || {
        f.state
            .workspace
            .index
            .borrow()
            .files
            .iter()
            .filter(|(p, _)| **p == main)
            .any(|(_, file)| file.labels.iter().any(|l| l.name == "stale:one"))
    };
    assert!(indexed_stale(), "control: the label is indexed at first");
    // Control: the same final edit once the feed is idle drops the label.
    replace_all(fast);
    wait_idle();
    assert!(
        !indexed_stale(),
        "control: an edit after an idle feed is indexed"
    );
    replace_all(&text);
    wait_idle();
    assert!(indexed_stale(), "control: the slow text is indexed again");
    // A few small edits, each after a pause longer than the re-index delay (300 ms): their jobs
    // run at the same time when one pass takes longer than the pause.
    for _ in 0..3 {
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, "%");
        common::pump_for(Duration::from_millis(350));
    }
    // Then the slow block and the label go away.
    replace_all(fast);
    wait_idle();
    assert!(
        !f.editor.document().text().contains("stale:one"),
        "the text no longer has the label"
    );
    assert!(
        !indexed_stale(),
        "an older re-index result was merged after the newest: the index still has stale:one"
    );
}

// ───────────── E2-15: file list from the chapter's directory ─────────────

fn regress2_chapter_first_lists_files_from_the_root() {
    let dir = common::temp_dir("chapter-first");
    let src = common::fixtures().join("proj");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    std::fs::create_dir_all(dir.join("figures")).unwrap();
    for file in ["main.tex", "refs.bib", "main.aux", "chapters/yoneda.tex"] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    std::fs::write(dir.join("figures/plot.pdf"), b"%PDF-1.4\n").unwrap();
    let f = fixture_in(&dir, "chapters/yoneda.tex", false);
    let roots: Vec<String> = f
        .state
        .workspace
        .index
        .borrow()
        .files
        .keys()
        .map(|p| p.display().to_string())
        .collect();
    let yoneda = f.editor.document().text();
    f.set(&f.editor, &format!("{yoneda}\\includegraphics{{‸}}\n"));
    let graphics = f.labels(&f.editor);
    f.set(&f.editor, &format!("{yoneda}\\input{{‸}}\n"));
    let inputs = f.labels(&f.editor);
    assert!(
        graphics.iter().any(|l| l == "figures/plot.pdf")
            && inputs.iter().any(|l| l == "chapters/yoneda"),
        "file arguments are relative to the chapter's directory, not the root's: index {roots:?}, \
         \\includegraphics{{ {graphics:?}, \\input{{ {inputs:?}"
    );
}

// ───────────── E2-16 / E2-17 / E2-18: snippet sessions on real keys ─────────────

fn regress2_nested_and_replacement_snippets_on_real_keys() {
    broadway_input::run_child(REAL_CHILD, 7);
}

/// Line 2 is text.
const DOC: &str = "\\documentclass{article}\n\\begin{document}\nText: \n\n\\end{document}\n";
const TEXT_LINE: i32 = 2;

/// Real key events; each key waits until the key layer has seen it.
struct Real {
    browser: Browser,
    seen: Rc<std::cell::Cell<u64>>,
}

impl Real {
    fn new(f: &Fixture) -> Self {
        let seen: Rc<std::cell::Cell<u64>> = Rc::default();
        let count = seen.clone();
        keys::register(f.editor.view(), i32::MAX, move |_, _| {
            count.set(count.get() + 1);
            glib::Propagation::Proceed
        })
        .expect("the view has a key layer");
        Self {
            browser: Browser::connect(),
            seen,
        }
    }

    fn key(&mut self, f: &Fixture, key: gdk::Key, mods: i32) {
        let before = self.seen.get();
        self.browser.key(key, mods);
        let deadline = Instant::now() + TIMEOUT;
        while self.seen.get() <= before {
            assert!(Instant::now() < deadline, "the key never arrived");
            common::pump_for(Duration::from_millis(2));
        }
        common::pump_for(Duration::from_millis(5));
        let _ = f;
    }

    fn text(&mut self, f: &Fixture, text: &str) {
        for c in text.chars() {
            match c {
                '\t' => self.key(f, gdk::Key::Tab, 0),
                '\n' => self.key(f, gdk::Key::Return, 0),
                c => {
                    let mods = if c.is_uppercase() {
                        broadway_input::SHIFT
                    } else {
                        0
                    };
                    self.key(f, broadway_input::keyval(c), mods);
                }
            }
        }
    }

    /// `text` in the buffer, typing from the end of `line` (plain editing).
    fn start(&mut self, f: &Fixture, text: &str, line: i32) {
        self.key(f, gdk::Key::Escape, 0);
        let buffer = f.editor.buffer();
        lemma_app::editor::snippets::end_snippet_sessions(f.editor.view());
        buffer.set_text(text);
        let mut iter = buffer.iter_at_line(line).unwrap();
        if !iter.ends_line() {
            iter.forward_to_line_end();
        }
        buffer.place_cursor(&iter);
        common::pump_for(Duration::from_millis(5));
    }
}

fn line_of(f: &Fixture, line: i32) -> String {
    f.editor
        .document()
        .text()
        .split('\n')
        .nth(usize::try_from(line).unwrap())
        .unwrap_or_default()
        .to_owned()
}

fn real_nested_snippets_child() {
    let f = fixture("real-nested", &[("main.tex", DOC)], "main.tex", false);
    let mut typist = Real::new(&f);
    let mut failures = Vec::new();
    let mut case = |typist: &mut Real, name: &str, keys: &[&str], check: &dyn Fn(&str) -> bool| {
        typist.start(&f, DOC, TEXT_LINE);
        for part in keys {
            typist.text(&f, part);
            println!(
                "  {name}: after {part:?}: {:?} (snippet active {})",
                line_of(&f, TEXT_LINE),
                completion::snippet_active(&f.state, &f.editor)
            );
        }
        let line = line_of(&f, TEXT_LINE);
        if !check(&line) {
            failures.push(format!("{name}: {line:?}"));
        }
    };
    // E2-16: an inner \frac in the numerator, then the outer denominator.
    case(
        &mut typist,
        "E2-16 ff in ff, then the outer denominator",
        &["$ff", "ff", "1", "\t", "2", "\t", "3"],
        &|line| line.contains("\\frac{\\frac{1}{2}}{3}"),
    );
    // E2-17: one more Tab after the nested snippet is done must not select the formula.
    case(
        &mut typist,
        "E2-17 extra Tab after ff in mk",
        &["mk", "ff", "1", "\t", "2", "\t", "\t", " after"],
        &|line| line.contains("$\\frac{1}{2}$") && line.contains("after"),
    );
    // E2-18: a replacement autosnippet inside a stop keeps the session.
    case(
        &mut typist,
        "E2-18 -> inside the numerator, Tab to the denominator",
        &["$ff", "a -> b", "\t", "c"],
        &|line| line.contains("\\frac{a \\to b}{c}"),
    );
    assert!(failures.is_empty(), "{failures:#?}");
}
