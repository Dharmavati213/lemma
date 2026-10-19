//! WP3.7: chktex in the Problems page (lint on open and save, line following, the soft
//! underline and its hover, silencing by comment and by configuration, the defaults, a missing
//! chktex), plain-English explanations and quick fixes of build errors (each fix's hunk, and
//! Reject restoring the text), and the word count (equal to a direct texcount run; selection;
//! after a build; a missing texcount).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_lint`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `o-problems-lint-light.png` / `-dark.png` there.
//! chktex and texcount tests print `SKIP` and pass when the program is missing.

mod common;

use adw::prelude::*;
use lemma_app::build_ui::Reason;
use lemma_app::diagnostics_panel::{Finding, Problem, ProblemFilter};
use lemma_app::editor::hover;
use lemma_app::editor::{Editor, Trigger};
use lemma_app::review_ui::{self, Decision};
use lemma_app::settings::SettingsFiles;
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_app::{lint, wordcount};
use lemma_build::chktex;
use lemma_config::Config;
use lemma_core::{DiagSource, ProjectRoot, Severity};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        ("lint_on_open_and_save_follows_edits", lint_open_and_save),
        (
            "lint_underline_is_soft_and_hovers",
            lint_underline_and_hover,
        ),
        ("lint_disable_on_line_with_a_comment", silence_by_comment),
        ("lint_disable_everywhere_in_config", silence_by_config),
        ("lint_defaults_enabled_and_off", lint_defaults_and_off),
        ("lint_missing_chktex_is_quiet", lint_missing_program),
        ("lint_filter_menu_and_navigation", lint_filter_and_menu),
        ("explanations_on_build_rows", explanations),
        ("quick_fix_wraps_math", fix_missing_dollar),
        ("quick_fix_loads_the_package", fix_package),
        ("quick_fix_closest_macro", fix_closest_macro),
        ("quick_fix_environments", fix_environments),
        ("quick_fix_mismatched_end", fix_mismatched_end),
        ("quick_fix_file_name", fix_file_name),
        ("quick_fix_begin_document", fix_begin_document),
        ("quick_fix_none_for_defined_macros", fix_none),
        ("word_count_equals_texcount", word_count),
        ("word_count_selection", word_count_selection),
        ("word_count_after_a_build", word_count_after_build),
        ("word_count_missing_texcount_and_config", word_count_hidden),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn have(program: &str, arg: &str) -> bool {
    let found = Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        println!("SKIP: {program} not found");
    }
    found
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, files, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    lint::set_debounce(&state, Duration::from_millis(30));
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        assert!(block_on(testmode::wait_for_frames(&self.state.window, 1)));
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        normalize(&self.dir.join(file))
    }

    fn panel(&self) -> &lemma_app::diagnostics_panel::DiagnosticsPanel {
        self.state.build.get().unwrap().diagnostics()
    }

    fn lint(&self) -> Vec<Finding> {
        self.panel().findings(DiagSource::Lint)
    }

    /// Waits until `runs` lint runs finished and nothing else is pending.
    fn wait_lint(&self, runs: u64) {
        common::pump_until(TIMEOUT, || {
            lint::runs(&self.state) >= runs && lint::is_idle(&self.state)
        });
    }

    fn wait_count(&self, runs: u64) {
        common::pump_until(TIMEOUT, || {
            wordcount::runs(&self.state) >= runs && wordcount::is_idle(&self.state)
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// chktex run directly on `text` (as the app runs it): (line, column, number) of the warnings
/// Lemma shows by default.
fn direct_chktex(text: &str, dir: &Path) -> Vec<(u32, u32, u32)> {
    let mut v: Vec<(u32, u32, u32)> = chktex::run(Path::new("chktex"), text, dir, "main.tex")
        .expect("chktex runs")
        .into_iter()
        .filter(|w| chktex::is_shown(w.number, &[], &[]))
        .map(|w| (w.line, w.column, w.number))
        .collect();
    v.sort_unstable();
    v
}

fn shown(findings: &[Finding]) -> Vec<(u32, u32, u32)> {
    let mut v: Vec<(u32, u32, u32)> = findings
        .iter()
        .map(|f| {
            (
                f.problem.line.unwrap(),
                f.columns.as_ref().map_or(1, |c| c.start + 1),
                f.code.unwrap(),
            )
        })
        .collect();
    v.sort_unstable();
    v
}

const LINT_TEX: &str = "\\documentclass{article}\n\
\\begin{document}\n\
See \\cite{x} for the proof.\n\
So on and on...\n\
A \"quote\" here.\n\
\\end{document}\n";

fn lint_open_and_save() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-open", &[("main.tex", LINT_TEX)]);
    let editor = f.open("main.tex");
    f.wait_lint(1);
    let findings = f.lint();
    assert_eq!(shown(&findings), direct_chktex(LINT_TEX, &f.dir));
    let numbers: Vec<u32> = findings.iter().map(|f| f.code.unwrap()).collect();
    assert!(numbers.contains(&2) && numbers.contains(&11), "{numbers:?}");
    assert!(!numbers.contains(&18), "18 is silenced by default");
    let w2 = findings.iter().find(|f| f.code == Some(2)).unwrap();
    assert_eq!(w2.problem.line, Some(3));
    assert_eq!(w2.problem.source, DiagSource::Lint);
    assert_eq!(w2.problem.path, f.path("main.tex"));
    // WP3.10: the rows (several widgets per finding) are built when the page shows them; until
    // then the findings cost no widgets (lint runs on open: about 1.5 MB at idle for the
    // warnings of a 1k-line document).
    let rows = |f: &Fixture| {
        let mut n = 0;
        let mut stack = vec![f.state.sidebar.problems.clone().upcast::<gtk::Widget>()];
        while let Some(w) = stack.pop() {
            let mut child = w.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
            n += usize::from(w.is::<gtk::ListBoxRow>());
        }
        n
    };
    f.state.sidebar.split.set_show_sidebar(false);
    common::pump_for(Duration::from_millis(50));
    assert_eq!(rows(&f), 0, "rows were built for a hidden page");
    f.state.sidebar.split.set_show_sidebar(true);
    f.state.sidebar.show_page(lemma_app::window::PAGE_PROBLEMS);
    common::pump_until(TIMEOUT, || rows(&f) == findings.len());
    // The build's problems are not touched: lint is a section of its own.
    assert!(f.panel().problems().is_empty());
    assert!(f.panel().shows_filter_bar());
    assert!(
        f.panel().summary_text().contains("lint warning"),
        "{}",
        f.panel().summary_text()
    );

    // Lines follow edits made without saving.
    editor
        .buffer()
        .insert(&mut editor.buffer().start_iter(), "% one\n% two\n");
    let moved = f.lint();
    let w2 = moved.iter().find(|f| f.code == Some(2)).unwrap();
    assert_eq!(w2.problem.line, Some(5));
    // A save lints again: the same warnings at their new lines.
    let runs = lint::runs(&f.state);
    block_on(editor.document().save()).unwrap();
    f.wait_lint(runs + 1);
    let text = editor.document().text();
    assert_eq!(shown(&f.lint()), direct_chktex(&text, &f.dir));
    assert_eq!(
        f.lint()
            .iter()
            .find(|f| f.code == Some(2))
            .unwrap()
            .problem
            .line,
        Some(5)
    );
}

fn lint_underline_and_hover() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-hover", &[("main.tex", LINT_TEX)]);
    let editor = f.open("main.tex");
    f.wait_lint(1);
    let buffer = editor.buffer();
    let tag = buffer
        .tag_table()
        .lookup(lint::STYLE.tag)
        .expect("the lint tag");
    // Softer than TeX errors: a blue wavy underline under exactly the characters chktex marks.
    assert_eq!(tag.underline(), pango::Underline::Error);
    let rgba = tag.underline_rgba().unwrap();
    assert!(rgba.blue() > rgba.red(), "blue, not red or amber");
    let doc = editor.document();
    let ellipsis = doc.text().find("...").unwrap();
    let iter = |offset| doc.iter_at_byte(offset).unwrap();
    assert!(iter(ellipsis).has_tag(&tag) && iter(ellipsis + 2).has_tag(&tag));
    assert!(
        !iter(ellipsis - 2).has_tag(&tag),
        "only the offending characters"
    );
    let line_three = doc.line_index().line_start(2);
    assert!(!iter(line_three).has_tag(&tag));

    // The hover names the warning.
    let (finding, range) = f
        .panel()
        .finding_at(&editor, ellipsis + 1)
        .expect("a finding");
    assert_eq!(finding.code, Some(11));
    assert_eq!(range, ellipsis..ellipsis + 3);
    assert!(hover::show_at(&editor, ellipsis + 1, Trigger::Keyboard));
    common::pump_until(TIMEOUT, || hover::is_shown(editor.view()));
    assert_eq!(
        hover::shown_range(editor.view()).map(|(r, _)| r),
        Some(ellipsis..ellipsis + 3)
    );
    let label = hover::shown_widget(editor.view())
        .and_then(|w| {
            let mut stack = vec![w];
            while let Some(w) = stack.pop() {
                if let Ok(l) = w.clone().downcast::<gtk::Label>() {
                    return Some(l.text().to_string());
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    c = x.next_sibling();
                    stack.push(x);
                }
            }
            None
        })
        .unwrap_or_default();
    assert!(label.contains("chktex warning 11"), "{label:?}");
    // Elsewhere, no lint hover.
    assert!(f.panel().finding_at(&editor, 3).is_none());
    snapshots(&f, &editor, ellipsis + 1);
    hover::hide(editor.view());
}

/// `LEMMA_TEST_SNAPSHOTS=dir`: the lint hover over its underline, and the Problems page with
/// lint findings, light and dark.
fn snapshots(f: &Fixture, editor: &Editor, at: usize) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let style = adw::StyleManager::default();
    for (scheme, name) in [
        (adw::ColorScheme::ForceLight, "light"),
        (adw::ColorScheme::ForceDark, "dark"),
    ] {
        style.set_color_scheme(scheme);
        f.state.sidebar.split.set_show_sidebar(false);
        common::pump_for(Duration::from_millis(100));
        assert!(hover::show_at(editor, at, Trigger::Keyboard));
        common::pump_until(TIMEOUT, || hover::is_shown(editor.view()));
        let path = dir.join(format!("o-lint-hover-{name}.png"));
        block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
        hover::hide(editor.view());
        f.state.sidebar.show_page(lemma_app::window::PAGE_PROBLEMS);
        common::pump_for(Duration::from_millis(100));
        let path = dir.join(format!("o-problems-lint-{name}.png"));
        block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
    }
    style.set_color_scheme(adw::ColorScheme::Default);
}

fn silence_by_comment() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-comment", &[("main.tex", LINT_TEX)]);
    let editor = f.open("main.tex");
    f.wait_lint(1);
    let index = f
        .lint()
        .iter()
        .position(|x| x.code == Some(2))
        .expect("warning 2");
    let actions = f.panel().finding_actions(DiagSource::Lint, index);
    assert_eq!(
        actions.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(),
        ["Disable on This Line", "Disable Everywhere"]
    );
    let runs = lint::runs(&f.state);
    f.panel().run_finding_action(DiagSource::Lint, index, 0);
    let doc = editor.document();
    common::pump_until(TIMEOUT, || doc.text().contains("% chktex 2"));
    assert_eq!(
        doc.line_text(2).as_deref(),
        Some("See \\cite{x} for the proof. % chktex 2")
    );
    // The document was clean: it is saved (and linted again), so the warning is gone.
    common::pump_until(TIMEOUT, || !doc.is_dirty());
    assert_eq!(
        std::fs::read_to_string(f.path("main.tex")).unwrap(),
        doc.text()
    );
    f.wait_lint(runs + 1);
    assert!(f.lint().iter().all(|x| x.code != Some(2)), "{:?}", f.lint());
    assert!(f.lint().iter().any(|x| x.code == Some(11)));
    // One undo step takes it back.
    editor.buffer().undo();
    assert_eq!(
        doc.line_text(2).as_deref(),
        Some("See \\cite{x} for the proof.")
    );
}

fn silence_by_config() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-config", &[("main.tex", LINT_TEX)]);
    let config = f.dir.join("config/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = "# my settings\n[lint]\nchktex = true # keep it on\n";
    std::fs::write(&config, original).unwrap();
    f.state
        .set_settings_files(Some(SettingsFiles::next_to(config.clone())));
    f.open("main.tex");
    f.wait_lint(1);
    let index = f
        .lint()
        .iter()
        .position(|x| x.code == Some(11))
        .expect("warning 11");
    let runs = lint::runs(&f.state);
    f.panel().run_finding_action(DiagSource::Lint, index, 1);
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.starts_with(original), "comments kept: {written}");
    assert!(written.contains("disabled = [11]"), "{written}");
    assert_eq!(f.state.config.lint.disabled, [11]);
    // Applied at once: every file is linted again without it.
    f.wait_lint(runs + 1);
    assert!(
        f.lint().iter().all(|x| x.code != Some(11)),
        "{:?}",
        f.lint()
    );
    assert!(f.lint().iter().any(|x| x.code == Some(2)));
    // A second one joins the list.
    let index = f.lint().iter().position(|x| x.code == Some(2)).unwrap();
    f.panel().run_finding_action(DiagSource::Lint, index, 1);
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("disabled = [2, 11]")
    );
}

fn lint_defaults_and_off() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-defaults", &[("main.tex", LINT_TEX)]);
    f.open("main.tex");
    f.wait_lint(1);
    assert!(f.lint().iter().all(|x| x.code != Some(18)));
    // `[lint] enabled` shows a warning Lemma silences by default.
    let runs = lint::runs(&f.state);
    let mut config = (*f.state.config.get()).clone();
    config.lint.enabled = vec![18];
    f.state.set_config(Rc::new(config.clone()));
    f.wait_lint(runs + 1);
    assert_eq!(
        f.lint().iter().filter(|x| x.code == Some(18)).count(),
        2,
        "both quotes"
    );
    // Off: everything goes, and the page is as before.
    config.lint.chktex = false;
    f.state.set_config(Rc::new(config.clone()));
    assert!(f.lint().is_empty());
    assert!(!f.panel().shows_filter_bar());
    // On again: linted again.
    let runs = lint::runs(&f.state);
    config.lint.chktex = true;
    f.state.set_config(Rc::new(config));
    f.wait_lint(runs + 1);
    assert!(!f.lint().is_empty());
}

fn lint_missing_program() {
    let f = fixture("lint-missing", &[("main.tex", LINT_TEX)]);
    lint::set_program(&f.state, Path::new("/nonexistent/lemma-test-chktex"));
    let before = common::logged_problems();
    f.open("main.tex");
    f.wait_lint(1);
    assert!(!lint::is_available(&f.state));
    assert!(f.lint().is_empty());
    assert!(!f.panel().shows_filter_bar());
    // Nothing more runs: saves do not try again.
    let runs = lint::runs(&f.state);
    let editor = f.state.active_editor().unwrap();
    block_on(editor.document().save()).unwrap();
    common::pump_for(Duration::from_millis(150));
    assert_eq!(lint::runs(&f.state), runs);
    common::assert_no_new_problems(before);
}

fn lint_filter_and_menu() {
    if !have("chktex", "--version") {
        return;
    }
    let f = fixture("lint-filter", &[("main.tex", LINT_TEX)]);
    f.open("main.tex");
    f.wait_lint(1);
    let root = f.path("main.tex");
    f.panel().set(
        vec![Problem {
            path: root.clone(),
            line: Some(4),
            severity: Severity::Error,
            message: "Undefined control sequence.".into(),
            context: Some("So on \\x <HERE> and".into()),
            source: DiagSource::Tex,
        }],
        &root,
    );
    let panel = f.panel();
    assert_eq!(panel.filter(), ProblemFilter::All);
    let summary = panel.summary_text();
    assert!(
        summary.starts_with("1 error · ") && summary.contains("lint warning"),
        "{summary}"
    );
    panel.set_filter(ProblemFilter::Build);
    assert_eq!(panel.summary_text(), "1 error");
    panel.set_filter(ProblemFilter::Source(DiagSource::Lint));
    assert!(
        panel.summary_text().ends_with("lint warnings"),
        "{}",
        panel.summary_text()
    );
    panel.set_filter(ProblemFilter::All);

    // The context menu of a lint row: its actions, then the agent.
    let menu = panel.finding_context_menu(DiagSource::Lint, 0).unwrap();
    let labels: Vec<String> = (0..menu.n_items())
        .filter_map(|i| {
            menu.item_attribute_value(i, gio::MENU_ATTRIBUTE_LABEL, None)
                .and_then(|v| v.get::<String>())
        })
        .collect();
    assert_eq!(
        labels,
        [
            "Disable on This Line",
            "Disable Everywhere",
            "Ask Agent to _Fix"
        ]
    );
    assert!(panel.finding_context_menu(DiagSource::Lint, 999).is_none());

    // A click on a lint row goes to its line and column.
    let ellipsis = f.lint().iter().position(|x| x.code == Some(11)).unwrap();
    panel.activate_finding(DiagSource::Lint, ellipsis);
    let editor = f.state.active_editor().unwrap();
    let doc = editor.document();
    let want = doc.text().find("...").unwrap();
    common::pump_until(TIMEOUT, || editor.cursor_offset() == want);
}

/// A window with `files`, the root `main.tex` open and indexed.
fn project(name: &str, files: &[(&str, &str)]) -> Fixture {
    let f = fixture_with(name, files, |c| c.lint.chktex = false);
    f.open("main.tex");
    let root = f.path("main.tex");
    common::pump_until(TIMEOUT, || {
        f.state.workspace.index.borrow().files.contains_key(&root)
    });
    f
}

fn error(f: &Fixture, file: &str, line: u32, message: &str, context: Option<&str>) -> Problem {
    Problem {
        path: f.path(file),
        line: Some(line),
        severity: Severity::Error,
        message: message.into(),
        context: context.map(str::to_owned),
        source: DiagSource::Tex,
    }
}

fn explanations() {
    let f = project(
        "explain-rows",
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nHello \\lamda.\n\\end{document}\n",
        )],
    );
    let root = f.path("main.tex");
    let mut warning = error(
        &f,
        "main.tex",
        3,
        "Reference `x' on page 1 undefined on input line 3.",
        None,
    );
    warning.severity = Severity::Warning;
    f.panel().set(
        vec![
            error(
                &f,
                "main.tex",
                3,
                "Undefined control sequence.",
                Some("Hello \\lamda <HERE> ."),
            ),
            warning,
            error(&f, "main.tex", 3, "Something nobody explains.", None),
        ],
        &root,
    );
    let panel = f.panel();
    let text = panel.explanation(0).expect("explained");
    assert!(
        text.contains("\\lamda") && text.contains("package"),
        "{text}"
    );
    assert!(panel.is_expanded(0), "errors show their explanation");
    assert!(panel.explanation(1).unwrap().contains("\\label{x}"));
    assert!(!panel.is_expanded(1), "warnings start collapsed");
    assert_eq!(panel.explanation(2), None);
    panel.set_expanded(1, true);
    assert!(panel.is_expanded(1));
    // The fix appears once computed, in the row and in its context menu (after the agent).
    common::pump_until(TIMEOUT, || panel.fix_label(0).is_some());
    assert_eq!(panel.fix_label(0).as_deref(), Some("Use \\lambda"));
    let menu = panel.context_menu(0).unwrap();
    let action = |i| {
        menu.item_attribute_value(i, gio::MENU_ATTRIBUTE_ACTION, None)
            .and_then(|v| v.get::<String>())
    };
    assert_eq!(action(0).as_deref(), Some("problems.ask-agent-fix"));
    assert_eq!(action(1).as_deref(), Some("problems.apply-fix"));
    assert_eq!(panel.context_menu(1).unwrap().n_items(), 1);
    // Gutter tooltips carry the explanation too (checked through the explanation table).
    assert!(panel.fix_label(2).is_none());
}

/// Applies the fix of `problem` (the only problem of a build of main.tex): its label is
/// `label`, `file` then reads `fixed`, under review; Reject restores `original` in the buffer
/// and on disk.
fn check_fix(f: &Fixture, problem: Problem, label: &str, file: &str, fixed: &str) {
    let root = f.path("main.tex");
    let path = f.path(file);
    let original = std::fs::read_to_string(&path).unwrap();
    f.panel().set(vec![problem], &root);
    common::pump_until(TIMEOUT, || f.panel().fix_label(0).is_some());
    assert_eq!(f.panel().fix_label(0).as_deref(), Some(label));
    f.panel().apply_fix(0);
    let doc = || f.state.workspace.get(&path).expect("loaded");
    common::pump_until(TIMEOUT, || {
        f.state
            .workspace
            .get(&path)
            .is_some_and(|d| d.text() == fixed)
    });
    let doc = doc();
    review_ui::recompute_if_stale(&doc);
    assert!(review_ui::hunk_count(&doc) >= 1, "reviewable");
    common::pump_until(TIMEOUT, || f.state.banner.is_revealed());
    let banner = f.state.banner.title().to_string();
    assert!(banner.starts_with("Fix: "), "{banner}");
    // The file was clean: saved like an agent edit.
    common::pump_until(TIMEOUT, || !doc.is_dirty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fixed);
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    assert_eq!(doc.text(), original);
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).unwrap() == original
    });
    assert_eq!(review_ui::hunk_count(&doc), 0);
}

fn fix_missing_dollar() {
    let tex = "\\documentclass{article}\n\\begin{document}\nThe value x_1 is positive and \\alpha_n too.\n\\end{document}\n";
    let f = project("fix-dollar", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            3,
            "Missing $ inserted.",
            Some("The value x_ <HERE> 1 is positive and \\alpha_n too."),
        ),
        "Put x_1 in $…$",
        "main.tex",
        &tex.replace("x_1", "$x_1$"),
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            3,
            "Missing $ inserted.",
            Some("The value x_1 is positive and \\alpha <HERE> _n too."),
        ),
        "Put \\alpha_n in $…$",
        "main.tex",
        &tex.replace("\\alpha_n", "$\\alpha_n$"),
    );
}

fn fix_package() {
    let tex = "\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\nThe ring $\\mathbb{Z}$ and \\SI{3}{m}.\n\\end{document}\n";
    let f = project("fix-package", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "Undefined control sequence.",
            Some("The ring $\\mathbb <HERE> {Z}$ and \\SI{3}{m}."),
        ),
        "Add \\usepackage{amssymb}",
        "main.tex",
        &tex.replace(
            "\\usepackage{amsmath}\n",
            "\\usepackage{amsmath}\n\\usepackage{amssymb}\n",
        ),
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "Undefined control sequence.",
            Some("The ring $\\mathbb{Z}$ and \\SI <HERE> {3}{m}."),
        ),
        "Add \\usepackage{siunitx}",
        "main.tex",
        &tex.replace(
            "\\usepackage{amsmath}\n",
            "\\usepackage{amsmath}\n\\usepackage{siunitx}\n",
        ),
    );
}

fn fix_closest_macro() {
    let tex = "\\documentclass{article}\n\\newcommand{\\cF}{\\mathcal{F}}\n\\begin{document}\nTake $\\lamda$ and $\\cG$ and \\cF.\n\\end{document}\n";
    let f = project("fix-macro", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "Undefined control sequence.",
            Some("Take $\\lamda <HERE> $ and"),
        ),
        "Use \\lambda",
        "main.tex",
        &tex.replace("\\lamda", "\\lambda"),
    );
    // The project's own macros are candidates too.
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "Undefined control sequence.",
            Some("Take $\\lamda$ and $\\cG <HERE> $ and"),
        ),
        "Use \\cF",
        "main.tex",
        &tex.replace("$\\cG$", "$\\cF$"),
    );
}

fn fix_environments() {
    let tex = "\\documentclass{article}\n\\newtheorem{theorem}{Theorem}[section]\n\\begin{document}\n\\begin{lemma}\nTrue.\n\\end{lemma}\n\\begin{equaiton}\nx\n\\end{equaiton}\n\\begin{tikzcd}\nA\n\\end{tikzcd}\n\\end{document}\n";
    let f = project("fix-env", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "Environment lemma undefined.",
            Some("\\begin{lemma} <HERE>"),
        ),
        "Declare \\newtheorem{lemma}",
        "main.tex",
        &tex.replace(
            "[section]\n",
            "[section]\n\\newtheorem{lemma}[theorem]{Lemma}\n",
        ),
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            7,
            "Environment equaiton undefined.",
            Some("\\begin{equaiton} <HERE>"),
        ),
        "Use “equation”",
        "main.tex",
        &tex.replace("equaiton", "equation"),
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            10,
            "Environment tikzcd undefined.",
            Some("\\begin{tikzcd} <HERE>"),
        ),
        "Add \\usepackage{tikz-cd}",
        "main.tex",
        &tex.replace(
            "\\documentclass{article}\n",
            "\\documentclass{article}\n\\usepackage{tikz-cd}\n",
        ),
    );
}

fn fix_mismatched_end() {
    let tex = "\\documentclass{article}\n\\begin{document}\n\\begin{itemize}\n\\item a\n\\end{enumerate}\n\\end{document}\n";
    let f = project("fix-end", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            5,
            "\\begin{itemize} on input line 3 ended by \\end{enumerate}.",
            Some("\\end{enumerate} <HERE>"),
        ),
        "Change to \\end{itemize}",
        "main.tex",
        &tex.replace("\\end{enumerate}", "\\end{itemize}"),
    );
}

fn fix_file_name() {
    let tex = "\\documentclass{article}\n\\usepackage{graphicx}\n\\begin{document}\n\\input{chapters/intor}\n\\includegraphics{figs/plott}\n\\end{document}\n";
    let f = project(
        "fix-file",
        &[
            ("main.tex", tex),
            ("chapters/intro.tex", "Intro.\n"),
            ("chapters/other.tex", "Other.\n"),
            ("figs/plot.png", "png"),
            ("figs/plot.txt", "not an image"),
        ],
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            4,
            "File `chapters/intor.tex' not found.",
            Some("\\input{chapters/intor} <HERE>"),
        ),
        "Use chapters/intro",
        "main.tex",
        &tex.replace("chapters/intor", "chapters/intro"),
    );
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            5,
            "File `figs/plott' not found.",
            Some("\\includegraphics{figs/plott} <HERE>"),
        ),
        "Use figs/plot",
        "main.tex",
        &tex.replace("figs/plott", "figs/plot"),
    );
}

fn fix_begin_document() {
    let tex = "\\documentclass{article}\n\\usepackage{amsmath}\nSome text in the preamble.\n\\end{document}\n";
    let f = project("fix-begin", &[("main.tex", tex)]);
    check_fix(
        &f,
        error(
            &f,
            "main.tex",
            3,
            "Missing \\begin{document}.",
            Some("S <HERE> ome text"),
        ),
        "Insert \\begin{document}",
        "main.tex",
        &tex.replace("Some text", "\\begin{document}\nSome text"),
    );
}

fn fix_none() {
    // An undefined command inside a macro's definition: TeX's context names the macro the
    // user typed, which is defined; no fix, but the explanation and the agent stay.
    let tex = "\\documentclass{article}\n\\newcommand{\\foo}{\\undefinedthing}\n\\begin{document}\nUse \\foo here and $x\\le y$.\n\\end{document}\n";
    let f = project("fix-none", &[("main.tex", tex)]);
    let root = f.path("main.tex");
    f.panel().set(
        vec![
            error(
                &f,
                "main.tex",
                4,
                "Undefined control sequence.",
                Some("Use \\foo <HERE> here"),
            ),
            error(
                &f,
                "main.tex",
                5,
                "Missing $ inserted.",
                Some("\\end{document} <HERE>"),
            ),
            error(
                &f,
                "main.tex",
                4,
                "Extra alignment tab has been changed to \\cr.",
                Some("Use <HERE> \\foo"),
            ),
        ],
        &root,
    );
    common::pump_for(Duration::from_millis(300));
    for i in 0..3 {
        assert!(f.panel().explanation(i).is_some(), "row {i} explained");
        assert_eq!(f.panel().fix_label(i), None, "row {i} has no fix");
    }
    // Applying anyway changes nothing and says so.
    let before = common::logged_problems();
    f.panel().apply_fix(0);
    common::pump_for(Duration::from_millis(200));
    assert_eq!(f.state.workspace.get(&root).unwrap().text(), tex);
    common::assert_no_new_problems(before);
}

/// A copy of the `proj` fixture.
fn proj(name: &str) -> Fixture {
    let src = common::fixtures().join("proj");
    let main = std::fs::read_to_string(src.join("main.tex")).unwrap();
    let chapter = std::fs::read_to_string(src.join("chapters/yoneda.tex")).unwrap();
    let bib = std::fs::read_to_string(src.join("refs.bib")).unwrap();
    fixture_with(
        name,
        &[
            ("main.tex", &main),
            ("chapters/yoneda.tex", &chapter),
            ("refs.bib", &bib),
        ],
        |c| c.lint.chktex = false,
    )
}

/// `texcount ARGS` in `dir` with `stdin`: its first number.
fn direct_texcount(dir: &Path, args: &[&str], stdin: Option<&str>) -> u64 {
    let mut child = Command::new("texcount")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    wordcount::parse_single(&String::from_utf8_lossy(&out.stdout)).unwrap()
}

fn word_count() {
    if !have("texcount", "-v") {
        return;
    }
    let f = proj("wordcount");
    let root = f.path("main.tex");
    assert!(
        !f.state.status.word_count.is_visible(),
        "hidden before a count"
    );
    wordcount::count_now(&f.state, &root);
    f.wait_count(1);
    let direct = direct_texcount(&f.dir, &["-1", "-sum", "-merge", "main.tex"], None);
    let (counted_root, count) = wordcount::last_count(&f.state).unwrap();
    assert_eq!(counted_root, root);
    assert_eq!(count.total, direct);
    assert!(!count.errors);
    assert_eq!(
        wordcount::status_text(&f.state),
        Some(wordcount::words_label(direct))
    );
    assert!(f.state.status.word_count.is_visible());
    // The breakdown: one row per section, with clean titles, adding up to the total.
    let titles: Vec<&str> = count.sections.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"Introduction"), "{titles:?}");
    assert!(titles.contains(&"The Yoneda lemma"), "{titles:?}");
    assert_eq!(count.sections.iter().map(|s| s.words).sum::<u64>(), direct);
    let popover = wordcount::popover(&f.state).unwrap();
    popover.popup();
    common::pump_until(TIMEOUT, || popover.is_visible());
    let labels: Vec<String> = all_labels(&popover);
    assert!(labels.iter().any(|l| l == "Introduction"), "{labels:?}");
    assert!(
        labels.iter().any(|l| *l == wordcount::words_label(direct)),
        "{labels:?}"
    );
    popover.popdown();
}

fn all_labels(widget: &impl IsA<gtk::Widget>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone().upcast::<gtk::Widget>()];
    while let Some(w) = stack.pop() {
        if let Ok(l) = w.clone().downcast::<gtk::Label>() {
            out.push(l.text().to_string());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            c = x.next_sibling();
            stack.push(x);
        }
    }
    out
}

fn word_count_selection() {
    if !have("texcount", "-v") {
        return;
    }
    let f = proj("wordcount-selection");
    let editor = f.open("main.tex");
    // The shown root is counted before any build.
    f.wait_count(1);
    assert!(wordcount::last_count(&f.state).is_some());
    let buffer = editor.buffer();
    let start = buffer.iter_at_line(38).unwrap();
    let end = buffer.iter_at_line(47).unwrap();
    buffer.select_range(&start, &end);
    let selected = buffer.text(&start, &end, false).to_string();
    wordcount::show(&f.state);
    common::pump_until(TIMEOUT, || wordcount::selection_count(&f.state).is_some());
    let direct = direct_texcount(&f.dir, &["-1", "-sum", "-utf8", "-"], Some(&selected));
    assert_eq!(wordcount::selection_count(&f.state), Some(Ok(direct)));
    assert!(direct > 10, "{direct}");
    let popover = wordcount::popover(&f.state).unwrap();
    assert!(popover.is_visible());
    common::pump_until(TIMEOUT, || {
        all_labels(&popover)
            .iter()
            .any(|l| *l == format!("Selection: {}", wordcount::words_label(direct)))
    });
    popover.popdown();
}

fn word_count_after_build() {
    if !have("texcount", "-v") || !have("latexmk", "-v") {
        return;
    }
    let f = proj("wordcount-build");
    // Not a root: nothing is counted when it is shown.
    let editor = f.open("chapters/yoneda.tex");
    common::pump_for(Duration::from_millis(200));
    assert_eq!(wordcount::runs(&f.state), 0);
    let _ = editor;
    let build = f.state.build.get().unwrap();
    let finished = Rc::new(std::cell::Cell::new(false));
    let done = finished.clone();
    build.connect_finished(move |_| done.set(true));
    f.state.request_build(Reason::Manual);
    common::pump_until(Duration::from_secs(120), || finished.get());
    f.wait_count(1);
    let direct = direct_texcount(&f.dir, &["-1", "-sum", "-merge", "main.tex"], None);
    assert_eq!(wordcount::last_count(&f.state).unwrap().1.total, direct);
    assert_eq!(
        wordcount::status_text(&f.state),
        Some(wordcount::words_label(direct))
    );
}

fn word_count_hidden() {
    let f = proj("wordcount-hidden");
    let root = f.path("main.tex");
    wordcount::set_program(&f.state, Path::new("/nonexistent/lemma-test-texcount"));
    wordcount::count_now(&f.state, &root);
    f.wait_count(1);
    assert!(!f.state.status.word_count.is_visible());
    assert_eq!(wordcount::status_text(&f.state), None);
    if !have("texcount", "-v") {
        return;
    }
    // `[status] word_count = false` hides a count that exists.
    wordcount::set_program(&f.state, Path::new("texcount"));
    wordcount::count_now(&f.state, &root);
    f.wait_count(2);
    assert!(f.state.status.word_count.is_visible());
    let mut config = (*f.state.config.get()).clone();
    config.status.word_count = false;
    f.state.set_config(Rc::new(config.clone()));
    assert!(!f.state.status.word_count.is_visible());
    config.status.word_count = true;
    f.state.set_config(Rc::new(config));
    assert!(f.state.status.word_count.is_visible());
}
