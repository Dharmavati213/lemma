//! WP3.6: project search and replace (`search_panel.rs`, `lemma-search`).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_search`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `search-light.png` and `search-dark.png` there.

mod common;

use adw::prelude::*;
use lemma_app::editor::Editor;
use lemma_app::editor::goto;
use lemma_app::review_ui::{self, Decision, GroupKind};
use lemma_app::search_panel::{self, Panel};
use lemma_app::testmode;
use lemma_app::window::{PAGE_SEARCH, WindowState};
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::{Config, Paths};
use lemma_core::ProjectRoot;
use lemma_search::Query;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let mut tests: Vec<common::Test> = vec![
        ("results_are_grouped_in_project_order", grouped_results),
        ("results_equal_the_matcher_with_every_option", options_match),
        ("live_buffer_text_wins_over_the_disk", live_text_wins),
        ("empty_invalid_and_no_results_pages", pages),
        (
            "activating_rows_folds_files_and_goes_to_matches",
            activation,
        ),
        ("project_search_action_prefills_the_selection", show_action),
        ("replace_preview_shows_old_and_new_text", replace_preview),
        (
            "replace_all_makes_reviewable_hunks_reject_restores",
            replace_all,
        ),
        (
            "replace_turns_and_agent_turns_keep_their_numbers",
            replace_and_agent_turns,
        ),
        ("a_replace_keeps_its_name_after_a_restart", replace_persists),
        ("a_capped_search_cannot_be_replaced", capped_replace),
        ("ten_thousand_matches_stream_in_small_steps", streaming_10k),
        ("highlights_follow_the_dark_style", dark_style),
        ("demo_search_snapshot", demo_snapshot),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_search_10k_frame_budget", perf_10k));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(30);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// A project: `main.tex` includes `chapters/a.tex` and `refs.bib`; the other files are found
/// by the walk.
const PROJECT: &[(&str, &str)] = &[
    (
        "main.tex",
        "\\documentclass{amsart}\n\\begin{document}\n\\title{A Sheaf}\n\\input{chapters/a}\n\
         A sheaf is a presheaf; every Sheaf glues.\n\\bibliography{refs}\n\\end{document}\n",
    ),
    (
        "chapters/a.tex",
        "\\section{Sheaves}\nThe sheaf $\\cF$ and the sheafification of $\\cF$.\n\
         \\emph{sheaf} again\n",
    ),
    ("refs.bib", "@book{sheaf, title = {Sheaf Theory}}\n"),
    ("appendix.tex", "Appendix: no sheaf here? one sheaf.\n"),
    ("notes.md", "# Notes\n- sheaf cohomology\n"),
    ("z/deep/more.tex", "deep sheaf\n"),
    ("data.txt", "sheaf in a text file is not searched\n"),
    (".build/main.tex", "sheaf in the build directory\n"),
    (".git/sheaf.tex", "sheaf in git\n"),
];

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    let dir = common::temp_dir(name);
    fixture_in(&dir, files, None)
}

/// A window on `dir` (with `files` written there), persisting into `paths` when given (an
/// interactive window then: temporary XDG directories only).
fn fixture_in(dir: &Path, files: &[(&str, &str)], paths: Option<Paths>) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    let scripted = paths.is_none();
    let ws = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), paths, ws, scripted);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir: dir.canonicalize().unwrap(),
    }
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

    /// Opens `main.tex` and waits until the project index knows its includes.
    fn open_project(&self) -> Rc<Editor> {
        let editor = self.open("main.tex");
        lemma_app::editor::completion::refresh_project(&self.state);
        let index_has = |p: &Path| self.state.workspace.index.borrow().files.contains_key(p);
        let a = self.path("chapters/a.tex");
        common::pump_until(TIMEOUT, || {
            !lemma_app::editor::completion::project_busy(&self.state) && index_has(&a)
        });
        editor
    }

    fn panel(&self) -> Rc<Panel> {
        search_panel::ensure(&self.state)
    }

    /// Searches for `query` and waits for the end.
    fn search(&self, query: &Query) -> Rc<Panel> {
        let panel = self.panel();
        self.state.sidebar.show_page(PAGE_SEARCH);
        panel.query_entry().set_text(&query.pattern);
        panel.set_options(query.regex, query.case_sensitive, query.whole_word);
        panel.search_now();
        wait_done(&panel);
        panel
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn wait_done(panel: &Panel) {
    common::pump_until(TIMEOUT, || !panel.is_searching());
}

/// `file:line:start-end` of every match the panel lists (line 1-based, byte columns of the
/// line).
fn listed(fx: &Fixture, panel: &Panel) -> Vec<String> {
    let mut out = Vec::new();
    for file in panel.files() {
        for line in &file.lines {
            for r in &line.ranges {
                out.push(format!(
                    "{}:{}:{}-{}",
                    fx.rel(&file.path),
                    line.line + 1,
                    r.start + line.text_start,
                    r.end + line.text_start
                ));
            }
        }
    }
    out
}

/// What the matcher finds in `text` of `file`, in the same form.
fn expected(file: &str, text: &str, query: &Query) -> Vec<String> {
    let matcher = query.matcher().unwrap();
    matcher
        .find_lines(text)
        .into_iter()
        .flat_map(|h| {
            h.ranges
                .into_iter()
                .map(move |r| format!("{file}:{}:{}-{}", h.line + 1, r.start, r.end))
        })
        .collect()
}

fn grouped_results() {
    let fx = fixture("grouped", PROJECT);
    fx.open_project();
    let panel = fx.search(&Query::plain("sheaf"));
    let files: Vec<String> = panel.files().iter().map(|f| fx.rel(&f.path)).collect();
    // The project graph in include order, then the walk (the root's files by name, then
    // folders); never the build directory, `.git` or a `.txt` file.
    assert_eq!(
        files,
        [
            "main.tex",
            "chapters/a.tex",
            "refs.bib",
            "appendix.tex",
            "notes.md",
            "z/deep/more.tex"
        ]
    );
    let summary = panel.summary().unwrap();
    // As `rg -i sheaf` counts them ("Sheaves" is not one).
    assert_eq!(summary.matches, 13);
    assert_eq!(panel.status_text(), "13 results in 6 files");
    assert_eq!(panel.page(), "results");
    // A header per file, then a row per line with matches.
    let lines: usize = panel.files().iter().map(|f| f.lines.len()).sum();
    assert_eq!(panel.row_count() as usize, files.len() + lines);
    let first = panel.row(0).unwrap();
    assert!(first.line.is_none());
    assert_eq!(first.file.name, "main.tex");
    assert_eq!(panel.files()[1].dir, "chapters");
    let row = panel.row(1).unwrap();
    assert_eq!(row.line_match().unwrap().line, 2);
    assert!(
        panel
            .row_markup(1)
            .contains(r#"bgalpha="62%">Sheaf</span>"#)
            || panel
                .row_markup(1)
                .contains(r#"bgalpha="38%">Sheaf</span>"#),
        "{}",
        panel.row_markup(1)
    );
    // The list shows the rows (laid out and bound).
    assert!(block_on(testmode::wait_for_frames(&fx.state.window, 1)));
    assert!(panel.frame_stats(false).binds > 0);
    snapshot(&fx, "search-light.png");
}

fn options_match() {
    let fx = fixture("options", PROJECT);
    fx.open_project();
    let texts: Vec<(&str, String)> = ["main.tex", "chapters/a.tex", "appendix.tex"]
        .iter()
        .map(|f| (*f, std::fs::read_to_string(fx.dir.join(f)).unwrap()))
        .collect();
    let cases = [
        Query::plain("Sheaf"),
        Query {
            case_sensitive: true,
            ..Query::plain("Sheaf")
        },
        Query {
            whole_word: true,
            ..Query::plain("sheaf")
        },
        Query::regex(r"\\(emph|section)\{([^}]*)\}"),
        Query {
            case_sensitive: false,
            whole_word: true,
            ..Query::regex(r"\w+ves")
        },
    ];
    for query in cases {
        let panel = fx.search(&query);
        let got: Vec<String> = listed(&fx, &panel)
            .into_iter()
            .filter(|m| texts.iter().any(|(f, _)| m.starts_with(&format!("{f}:"))))
            .collect();
        let want: Vec<String> = texts
            .iter()
            .flat_map(|(f, t)| expected(f, t, &query))
            .collect();
        assert_eq!(got, want, "{query:?}");
        assert_eq!(panel.query(), query);
    }
}

fn live_text_wins() {
    let fx = fixture("live", PROJECT);
    let editor = fx.open_project();
    // An unsaved line in the buffer; the disk does not have it.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "live sheaf only in the buffer\n");
    assert!(editor.document().is_dirty());
    let panel = fx.search(&Query::plain("only in the buffer"));
    let files = panel.files();
    assert_eq!(files.len(), 1);
    assert_eq!(fx.rel(&files[0].path), "main.tex");
    assert!(files[0].dirty);
    assert_eq!(files[0].lines[0].line, 7);
    assert!(
        !std::fs::read_to_string(fx.dir.join("main.tex"))
            .unwrap()
            .contains("only in the buffer")
    );
    // A closed file's disk text is searched; once it is open, its buffer is.
    let a = fx.open("chapters/a.tex");
    a.buffer().set_text("nothing here any more\n");
    let panel = fx.search(&Query::plain("sheafification"));
    assert!(panel.files().is_empty(), "the buffer wins over the disk");
    assert_eq!(panel.page(), "none");
}

fn pages() {
    let fx = fixture("pages", PROJECT);
    fx.open_project();
    let panel = fx.panel();
    fx.state.sidebar.show_page(PAGE_SEARCH);
    assert_eq!(panel.page(), "empty");
    let panel = fx.search(&Query::regex("(unclosed"));
    assert_eq!(panel.page(), "error");
    assert_eq!(panel.message(), "Unclosed group");
    assert!(panel.query_entry().has_css_class("error"));
    assert!(!panel.replace_button().is_sensitive());
    let panel = fx.search(&Query::plain("no such text anywhere"));
    assert_eq!(panel.page(), "none");
    assert!(!panel.query_entry().has_css_class("error"));
    assert_eq!(
        panel.message(),
        "Nothing matches “no such text anywhere” in 6 files"
    );
    panel.query_entry().set_text("");
    panel.search_now();
    assert_eq!(panel.page(), "empty");
    assert_eq!(panel.row_count(), 0);
    // Search as you type: the entry's own delay, then the search.
    panel.query_entry().set_text("glues");
    common::pump_until(TIMEOUT, || panel.summary().is_some_and(|s| s.matches == 1));
}

fn activation() {
    let fx = fixture("activate", PROJECT);
    fx.open_project();
    let panel = fx.search(&Query::plain("sheafification"));
    assert_eq!(panel.row_count(), 2);
    // A file row folds and unfolds.
    panel.activate(0);
    assert_eq!(panel.row_count(), 1);
    assert!(panel.files()[0].is_collapsed());
    panel.activate(0);
    assert_eq!(panel.row_count(), 2);
    // A line opens its file at the match and selects it.
    let main = fx.path("main.tex");
    panel.activate(1);
    let a = fx.path("chapters/a.tex");
    common::pump_until(TIMEOUT, || {
        fx.state
            .active_editor()
            .is_some_and(|e| e.document().path() == a)
    });
    let editor = fx.state.active_editor().unwrap();
    common::pump_until(TIMEOUT, || editor.buffer().has_selection());
    let (start, end) = editor.buffer().selection_bounds().unwrap();
    assert_eq!(editor.buffer().text(&start, &end, false), "sheafification");
    let text = editor.document().text();
    assert_eq!(
        editor.document().byte_offset(&start),
        text.find("sheafification").unwrap()
    );
    // Alt+Left goes back.
    assert!(goto::back(&fx.state));
    common::pump_until(TIMEOUT, || {
        fx.state
            .active_editor()
            .is_some_and(|e| e.document().path() == main)
    });
    // After an edit above it, the row still finds its match (two lines further down).
    let editor = fx.open("chapters/a.tex");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "new first line\n\n");
    buffer.select_range(&buffer.start_iter(), &buffer.start_iter());
    panel.activate(1);
    common::pump_until(TIMEOUT, || editor.buffer().has_selection());
    let text = editor.document().text();
    assert_eq!(
        editor.cursor_offset(),
        text.find("sheafification").unwrap() + "sheafification".len()
    );
}

fn show_action() {
    let fx = fixture("show", PROJECT);
    let editor = fx.open_project();
    let doc = editor.document();
    let text = doc.text();
    let at = text.find("presheaf").unwrap();
    let (a, b) = (
        doc.iter_at_byte(at).unwrap(),
        doc.iter_at_byte(at + "presheaf".len()).unwrap(),
    );
    editor.buffer().select_range(&a, &b);
    WidgetExt::activate_action(&fx.state.window, "win.project-search", None).unwrap();
    assert_eq!(
        fx.state.sidebar.stack.visible_child_name().as_deref(),
        Some(PAGE_SEARCH)
    );
    let panel = search_panel::panel(&fx.state).expect("the page was built");
    assert_eq!(panel.query_entry().text(), "presheaf");
    common::pump_until(TIMEOUT, || {
        panel.query_entry().has_focus() || {
            panel
                .query_entry()
                .root()
                .and_then(|r| r.focus())
                .is_some_and(|f| f.is_ancestor(panel.query_entry()))
        }
    });
    common::pump_until(TIMEOUT, || panel.summary().is_some_and(|s| s.matches == 1));
    // A selection over several lines does not prefill.
    let end = doc.iter_at_byte(text.len()).unwrap();
    editor.buffer().select_range(&a, &end);
    search_panel::show(&fx.state);
    assert_eq!(panel.query_entry().text(), "presheaf");
}

fn replace_preview() {
    let fx = fixture("preview", PROJECT);
    fx.open_project();
    let panel = fx.search(&Query::regex(r"\\emph\{([^}]*)\}"));
    assert!(!panel.replace_button().is_sensitive(), "replace is hidden");
    panel.set_replacement(Some(r"\textit{$1}"));
    assert!(panel.replace_button().is_sensitive());
    let position = (0..panel.row_count())
        .find(|p| panel.row(*p).is_some_and(|r| r.line.is_some()))
        .unwrap();
    let markup = panel.row_markup(position);
    assert!(
        markup.contains(r#"strikethrough="true">\emph{sheaf}</span>"#),
        "{markup}"
    );
    assert!(
        markup.contains(r#">\textit{sheaf}</span> again"#),
        "{markup}"
    );
    // Hiding the replace entry hides the preview.
    panel.set_replacement(None);
    assert!(!panel.row_markup(position).contains("strikethrough"));
    assert!(!panel.replace_button().is_sensitive());
}

const LISTED: [&str; 6] = [
    "main.tex",
    "chapters/a.tex",
    "refs.bib",
    "appendix.tex",
    "notes.md",
    "z/deep/more.tex",
];

/// `text` with every whole-word, case-sensitive "sheaf" replaced by "faisceau".
fn faisceau(text: &str) -> String {
    let matcher = word_sheaf().matcher().unwrap();
    let template = lemma_search::Template::new(&matcher, "faisceau");
    lemma_search::apply(
        text,
        &lemma_search::plan_replace(&matcher, &template, text, |_| true),
    )
}

fn word_sheaf() -> Query {
    Query {
        whole_word: true,
        case_sensitive: true,
        ..Query::plain("sheaf")
    }
}

fn replace_all() {
    let fx = fixture("replace", PROJECT);
    let main_editor = fx.open_project();
    let originals: Vec<(&str, String)> = LISTED
        .iter()
        .map(|f| (*f, std::fs::read_to_string(fx.dir.join(f)).unwrap()))
        .collect();
    let panel = fx.search(&word_sheaf());
    let listed_files: Vec<String> = panel.files().iter().map(|f| fx.rel(&f.path)).collect();
    assert_eq!(listed_files, LISTED);
    panel.set_replacement(Some("faisceau"));
    let outcome = block_on(panel.replace_all());
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    assert_eq!(outcome.files, 6);
    let matcher = word_sheaf().matcher().unwrap();
    let total: usize = originals.iter().map(|(_, t)| matcher.count(t)).sum();
    assert_eq!(outcome.matches, total);
    assert!(outcome.turn > 0);
    // Every file holds the replaced text, in the buffer and on disk, as hunks to review.
    for (file, original) in &originals {
        let doc = fx.state.workspace.get(&fx.path(file)).expect("loaded");
        let want = faisceau(original);
        assert_ne!(want, *original);
        assert_eq!(doc.text(), want, "{file}");
        assert_eq!(
            std::fs::read_to_string(fx.dir.join(file)).unwrap(),
            want,
            "{file}"
        );
        assert!(review_ui::hunk_count(&doc) > 0, "{file} has hunks");
    }
    assert!(
        main_editor
            .document()
            .text()
            .contains("A faisceau is a presheaf")
    );
    // One named turn: the Changes panel's group and the banner name it.
    let summary = review_ui::review_summary(&fx.state);
    assert_eq!(summary.files, 6);
    assert_eq!(summary.groups.len(), 1);
    assert_eq!(summary.groups[0].kind, GroupKind::Turn(outcome.turn));
    assert_eq!(summary.groups[0].label, "Replace “sheaf” → “faisceau”");
    fx.state.review.get().unwrap().refresh();
    let banner = fx.state.banner.title().to_string();
    assert!(
        banner.starts_with("Replace “sheaf” → “faisceau” · 6 files · "),
        "{banner}"
    );
    // The search ran again: nothing is left.
    common::pump_until(TIMEOUT, || {
        !panel.is_searching() && panel.summary().is_some()
    });
    assert_eq!(panel.page(), "none");
    // Accepting a hunk keeps it; rejecting the turn restores everything else, buffer and disk.
    let main = fx.state.workspace.get(&fx.path("main.tex")).unwrap();
    let first = main.review.borrow().as_ref().unwrap().hunks()[0].id;
    review_ui::decide_hunks(&fx.state, &main, &[first], Decision::Accept);
    let rejected = review_ui::decide_turn(&fx.state, outcome.turn, Decision::Reject);
    assert!(rejected.decided > 0);
    common::pump_until(TIMEOUT, || {
        originals[1..].iter().all(|(file, original)| {
            std::fs::read_to_string(fx.dir.join(file)).is_ok_and(|t| t == *original)
        })
    });
    for (file, original) in &originals[1..] {
        let doc = fx.state.workspace.get(&fx.path(file)).unwrap();
        assert_eq!(doc.text(), *original, "{file} restored");
        assert_eq!(review_ui::hunk_count(&doc), 0);
    }
    // main.tex had one change, accepted: it stays.
    assert_eq!(main.text(), faisceau(&originals[0].1));
    assert_eq!(review_ui::hunk_count(&main), 0);
}

fn replace_and_agent_turns() {
    let fx = fixture("turns", PROJECT);
    let editor = fx.open_project();
    let doc = editor.document().clone();
    // An agent turn changes the title.
    review_ui::on_turn_started(&fx.state, 1);
    review_ui::start_review(&doc, false);
    let at = doc.text().find("A Sheaf}").unwrap();
    doc.apply_edits(
        &[lemma_core::TextEdit {
            range: at..at + 7,
            text: "The Sheaf".into(),
        }],
        lemma_core::Origin::Agent { call: 1 },
    )
    .unwrap();
    review_ui::recompute(&doc);
    review_ui::on_turn_ended(&fx.state, 1);
    let agent_turn = review_ui::review_summary(&fx.state).latest_turn.unwrap();
    // A replace elsewhere is a turn of its own, after it.
    let panel = fx.search(&Query {
        case_sensitive: true,
        ..Query::plain("glues")
    });
    panel.set_replacement(Some("is glued"));
    let outcome = block_on(panel.replace_all());
    assert!(outcome.turn > agent_turn);
    let summary = review_ui::review_summary(&fx.state);
    let kinds: Vec<GroupKind> = summary.groups.iter().map(|g| g.kind).collect();
    assert_eq!(
        kinds,
        [GroupKind::Turn(outcome.turn), GroupKind::Turn(agent_turn)]
    );
    assert_eq!(summary.groups[1].label, "Turn 1");
    // WP3.10: in the Changes panel the replace's group says Keep / Reject (it is not an agent
    // turn), the agent's says Keep Turn / Reject Turn, and Accept All / Reject All stay below.
    fx.state.sidebar.show_page(lemma_app::window::PAGE_CHANGES);
    let buttons = || {
        let mut labels = Vec::new();
        let mut stack = vec![fx.state.sidebar.changes.clone().upcast::<gtk::Widget>()];
        while let Some(w) = stack.pop() {
            let mut child = w.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
            if let Some(b) = w.downcast_ref::<gtk::Button>()
                && let Some(label) = b.label()
            {
                labels.push(label.to_string());
            }
        }
        labels.sort();
        labels
    };
    common::pump_until(TIMEOUT, || buttons().iter().any(|l| l == "Reject"));
    let labels = buttons();
    for want in [
        "Keep",
        "Reject",
        "Keep Turn",
        "Reject Turn",
        "Accept All",
        "Reject All",
    ] {
        assert_eq!(
            labels.iter().filter(|l| *l == want).count(),
            1,
            "{want}: {labels:?}"
        );
    }
    // With the agent's changes under review the banner names Claude.
    fx.state.review.get().unwrap().refresh();
    assert!(fx.state.banner.title().starts_with("Claude changed"));
    // The agent's next turn comes after the replace; `:KeepTurn 1` still means the agent's
    // first turn.
    review_ui::on_turn_started(&fx.state, 2);
    review_ui::on_turn_ended(&fx.state, 2);
    let editor_ref = fx.state.active_editor().unwrap();
    assert!(review_ui::ex_command(&fx.state, &editor_ref, "KeepTurn 1"));
    common::pump_until(TIMEOUT, || {
        review_ui::review_summary(&fx.state)
            .groups
            .iter()
            .all(|g| g.kind != GroupKind::Turn(agent_turn))
    });
    assert!(doc.text().contains("The Sheaf"));
    // The replace is still pending, and Reject Turn takes it back.
    review_ui::decide_turn(&fx.state, outcome.turn, Decision::Reject);
    common::pump_until(TIMEOUT, || review_ui::hunk_count(&doc) == 0);
    assert!(doc.text().contains("every Sheaf glues."));
}

/// Per-test XDG directories under cargo's temp dir: nothing touches the user's.
fn temp_paths(name: &str) -> Paths {
    let base = common::temp_dir(&format!("{name}-xdg"));
    Paths {
        config_dir: base.join("config/lemma"),
        state_dir: base.join("state/lemma"),
        data_dir: base.join("data/lemma"),
        runtime_dir: base.join("run/lemma"),
        cache_dir: base.join("cache/lemma"),
        home: base,
    }
}

fn replace_persists() {
    let dir = common::temp_dir("persist");
    let paths = temp_paths("persist");
    let name = "Replace “sheaf” → “faisceau”";
    {
        let fx = fixture_in(&dir, PROJECT, Some(paths.clone()));
        fx.open_project();
        let panel = fx.search(&word_sheaf());
        panel.set_replacement(Some("faisceau"));
        let outcome = block_on(panel.replace_all());
        assert_eq!(outcome.files, 6);
        lemma_app::review_persist::flush(&fx.state);
        let file = lemma_app::review_persist::state_file(&fx.state).expect("a state file");
        common::pump_until(TIMEOUT, || {
            std::fs::read(&file)
                .ok()
                .and_then(|b| lemma_app::review_persist::parse(&b).ok())
                .is_some_and(|saved| saved.turns.iter().any(|t| t.name.as_deref() == Some(name)))
        });
    }
    // A new window on the project brings the hunks back, under the replace's name.
    let fx = fixture_in(&dir, &[], Some(paths));
    fx.open("main.tex");
    common::pump_until(TIMEOUT, || {
        lemma_app::review_persist::is_restored(&fx.state)
    });
    common::pump_until(TIMEOUT, || review_ui::review_summary(&fx.state).hunks > 0);
    let summary = review_ui::review_summary(&fx.state);
    assert_eq!(summary.groups.len(), 1);
    assert_eq!(summary.groups[0].label, name);
}

fn capped_replace() {
    let line = "x x x x x x x x x x\n";
    let fx = fixture("capped", &[("main.tex", &line.repeat(2100))]);
    fx.open("main.tex");
    let panel = fx.search(&Query::plain("x"));
    let summary = panel.summary().unwrap();
    assert!(summary.capped);
    assert!(
        panel.status_text().ends_with("refine the search"),
        "{}",
        panel.status_text()
    );
    assert!(!panel.files()[0].complete);
    panel.set_replacement(Some("y"));
    assert!(!panel.replace_button().is_sensitive());
    let outcome = block_on(panel.replace_all());
    assert_eq!(outcome.turn, 0);
    assert!(
        !fx.state
            .workspace
            .get(&fx.path("main.tex"))
            .unwrap()
            .text()
            .contains('y')
    );
}

/// Streams 12 000 matches (40 files of 250 lines with one each, and one of 2 000 lines);
/// returns the panel's statistics and the main loop's longest step meanwhile.
fn stream_10k(name: &str) -> (search_panel::FrameStats, Duration) {
    let mut files: Vec<(String, String)> = (0..40)
        .map(|i| {
            (
                format!("part{i:02}.tex"),
                "a needle in a haystack\n".repeat(250),
            )
        })
        .collect();
    files.push(("big.tex".into(), "needle\n".repeat(2000)));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let fx = fixture(name, &refs);
    let panel = fx.panel();
    fx.state.sidebar.show_page(PAGE_SEARCH);
    assert!(block_on(testmode::wait_for_frames(&fx.state.window, 1)));
    panel.frame_stats(true);
    // The main loop's longest step while the results arrive.
    let mut longest = Duration::ZERO;
    let mut last = Instant::now();
    panel.query_entry().set_text("needle");
    panel.search_now();
    let context = glib::MainContext::default();
    let deadline = Instant::now() + TIMEOUT;
    while panel.is_searching() {
        assert!(Instant::now() < deadline, "the search ends");
        context.iteration(false);
        let now = Instant::now();
        longest = longest.max(now - last);
        last = now;
        std::thread::sleep(Duration::from_micros(200));
    }
    let summary = panel.summary().unwrap();
    assert_eq!(summary.matches, 12_000);
    assert_eq!(panel.row_count(), 41 + 12_000);
    let stats = panel.frame_stats(false);
    eprintln!(
        "10k: {} batches, {} rows bound, at most {} in one step; longest step {:?} (batch \
         {:?}, binds in a frame {:?}); longest main-loop step {longest:?} (Broadway's own \
         frames included)",
        stats.batches, stats.binds, stats.step_binds, stats.max, stats.ingest, stats.bind
    );
    (stats, longest)
}

fn streaming_10k() {
    let (stats, _) = stream_10k("stream");
    assert!(stats.batches > 1, "results stream in several batches");
    // GtkListView creates widgets for its first ~200 rows: a few per step, never all at once
    // (the time budget is `perf_search_10k_frame_budget`'s).
    assert!(
        stats.step_binds <= 64,
        "{} rows bound in one step",
        stats.step_binds
    );
    assert!(stats.binds <= 260, "{} rows bound", stats.binds);
}

/// The panel's work at once while 10 000 matches stream in: ≤ 4 ms (release builds; run with
/// `cargo test --release -p lemma-app --test gtk_search -- --ignored perf`).
fn perf_10k() {
    let (stats, _) = stream_10k("perf");
    if cfg!(debug_assertions) {
        eprintln!("debug build: the 4 ms budget is checked in release builds only");
    } else {
        assert!(
            stats.max <= Duration::from_millis(4),
            "panel work at once: {:?}",
            stats.max
        );
    }
}

fn dark_style() {
    let fx = fixture("dark", PROJECT);
    fx.open_project();
    let panel = fx.search(&Query::plain("sheafification"));
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(adw::ColorScheme::ForceDark);
    common::pump_until(TIMEOUT, || manager.is_dark());
    assert!(panel.row_markup(1).contains(r#"bgalpha="38%""#));
    assert!(block_on(testmode::wait_for_frames(&fx.state.window, 2)));
    snapshot(&fx, "search-dark.png");
    manager.set_color_scheme(adw::ColorScheme::ForceLight);
    common::pump_until(TIMEOUT, || !manager.is_dark());
    assert!(panel.row_markup(1).contains(r#"bgalpha="62%""#));
    manager.set_color_scheme(adw::ColorScheme::Default);
}

/// Saves the window to `$LEMMA_TEST_SNAPSHOTS/name` when that is set.
fn snapshot(fx: &Fixture, name: &str) {
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") {
        let path = PathBuf::from(dir).join(name);
        block_on(testmode::snapshot_next_frame(&fx.state.window, &path)).expect("snapshot");
    }
}

/// `lemma FILE --demo search --snapshot PNG` on a copy of the `proj` fixture.
fn demo_snapshot() {
    let dir = common::temp_dir("demo");
    copy_dir(&common::fixtures().join("proj"), &dir);
    let png = dir.join("n-search.png");
    let xdg = dir.join("xdg");
    let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(dir.join("main.tex"))
        .args(["--demo", "search", "--snapshot"])
        .arg(&png)
        .args(["--quit-after-ms", "500"])
        .env("LEMMA_CONFIG", "/dev/null")
        .env("XDG_CONFIG_HOME", xdg.join("config"))
        .env("XDG_STATE_HOME", xdg.join("state"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .env_remove("LEMMA_DEMO_SEARCH")
        .env_remove("LEMMA_DEMO_REPLACE")
        .output()
        .expect("run lemma");
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "lemma failed: {:?}", out.status);
    assert!(std::fs::metadata(&png).is_ok_and(|m| m.len() > 10_000));
    // Read-only: the demo changes no file.
    for file in ["main.tex", "chapters/yoneda.tex", "refs.bib"] {
        assert_eq!(
            std::fs::read(dir.join(file)).unwrap(),
            std::fs::read(common::fixtures().join("proj").join(file)).unwrap()
        );
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
