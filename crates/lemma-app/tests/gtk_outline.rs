//! WP3.1: the Outline page (`outline_panel.rs`): the project tree with `.aux` numbers, click
//! and Enter navigation across files, following the cursor, the fuzzy filter, incremental
//! updates that keep the collapse state, figures and included files, theorem environments
//! declared in a preamble file, chapters changed on disk, and the keystroke cost.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_outline`.
//! The performance test: `scripts/broadway.sh cargo test --release -p lemma-app --test
//! gtk_outline -- --ignored perf`.

mod broadway_input;
mod common;

use adw::prelude::*;
use lemma_app::editor::Editor;
use lemma_app::editor::{completion, goto};
use lemma_app::outline_panel::{self as outline, OutlineStats};
use lemma_app::window::{PAGE_CHANGES, PAGE_OUTLINE, WindowState};
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[("outline_real_clicks_child", real_clicks::child)]);
    }
    let ignored = std::env::args().any(|a| a == "--ignored" || a == "--include-ignored");
    let mut tests: Vec<common::Test> = vec![
        ("outline_of_proj_matches_the_golden_tree", golden_proj),
        ("opening_the_chapter_shows_the_whole_project", chapter_first),
        (
            "click_goes_to_the_line_across_files_and_back",
            click_navigation,
        ),
        ("enter_on_a_row_goes_to_its_line", enter_navigation),
        (
            "following_the_cursor_selects_the_innermost_row",
            follow_cursor,
        ),
        ("following_the_cursor_can_be_turned_off", follow_off),
        ("the_menu_toggles_follow_cursor_in_config_toml", follow_menu),
        ("filter_is_fuzzy_ranked_and_enter_goes", filter),
        ("edits_patch_the_tree_and_keep_collapse_state", incremental),
        ("rows_follow_line_shifts_before_the_index", line_shifts),
        ("figures_tables_and_included_files", figures_and_files),
        ("nothing_happens_while_the_page_is_hidden", hidden_is_free),
        ("a_build_brings_numbers", build_numbers),
        ("expand_and_collapse_all_and_keys", expand_collapse_all),
        (
            "a_file_read_later_moves_and_stays_expanded",
            file_read_later,
        ),
        (
            "theorems_declared_in_a_preamble_file",
            theorems_from_a_preamble_file,
        ),
        (
            "a_chapter_changed_on_disk_updates_the_tree",
            a_chapter_changed_on_disk,
        ),
        (
            "clicks_on_rows_and_arrows_with_a_real_pointer",
            real_clicks::parent,
        ),
    ];
    if ignored {
        tests.push(("perf_outline_keystroke", perf_keystroke));
        tests.push(("perf_outline_large_tex", perf_large));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(30);

/// The outline of `tests/fixtures/proj` with the numbers of its committed `main.aux`.
const GOLDEN_PROJ: &str = "\
1 Introduction
[chapters/yoneda.tex]
  2 The Yoneda lemma
    Lemma 2.1 (Yoneda)
    Corollary 2.2
3 Descent
  Definition 3.1
  Theorem 3.2 (fpqc descent)
  Remark
4 Sheaves on sites
  Proposition 4.1
  Example
  4.1 Long computation
  4.2 Filler subsection 1
  4.3 Filler subsection 2
  4.4 Filler subsection 3
  4.5 Filler subsection 4
  4.6 Filler subsection 5
  4.7 Filler subsection 6
";

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// A window on a copy of `tests/fixtures/<fixture>` (or an empty directory) plus `files`.
fn fixture(name: &str, from: Option<&str>, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, from, files, |_| {})
}

fn fixture_with(
    name: &str,
    from: Option<&str>,
    files: &[(&str, &str)],
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    if let Some(from) = from {
        copy_dir(&common::fixtures().join(from), &dir);
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
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir: normalize(&dir),
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// Shows the Outline page and waits until it is settled.
    fn show_outline(&self) {
        self.state.sidebar.show_page(PAGE_OUTLINE);
        self.settle();
    }

    fn settle(&self) {
        completion::refresh_project(&self.state);
        let state = self.state.clone();
        common::pump_until(TIMEOUT, || outline::is_settled(&state));
    }

    /// Waits until the outline is settled and its tree satisfies `ok`.
    fn wait_tree(&self, what: &str, ok: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            common::pump_for(Duration::from_millis(20));
            let tree = outline::tree_text(&self.state);
            if outline::is_settled(&self.state) && ok(&tree) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}; the tree is:\n{tree}");
        }
    }

    fn active_path(&self) -> Option<PathBuf> {
        self.state
            .active_editor()
            .map(|e| e.document().path().to_path_buf())
    }

    /// The (0-based) line and column of the active editor's cursor.
    fn cursor(&self) -> (u32, usize) {
        let editor = self.state.active_editor().expect("an editor");
        let doc = editor.document();
        let offset = editor.cursor_offset();
        let line = doc.line_index().line_of(offset);
        (line, offset - doc.line_index().line_start(line))
    }

    fn put_cursor(&self, editor: &Editor, line: u32, column: usize) {
        let start = editor.document().line_index().line_start(line);
        assert!(editor.place_cursor_at(start + column));
        self.settle_follow();
    }

    fn settle_follow(&self) {
        let state = self.state.clone();
        common::pump_for(Duration::from_millis(5));
        common::pump_until(TIMEOUT, || outline::is_settled(&state));
    }

    /// Waits until the cursor is in `file` at 0-based `line`.
    fn wait_cursor(&self, file: &str, line: u32) {
        let path = self.path(file);
        let state = self.state.clone();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            common::pump_for(Duration::from_millis(10));
            if self.active_path().as_deref() == Some(path.as_path()) && self.cursor().0 == line {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the cursor is at {:?}:{:?}, not {file}:{line}",
                self.active_path(),
                state.active_editor().map(|_| self.cursor())
            );
        }
    }

    fn line_of(&self, file: &str, needle: &str) -> u32 {
        let text = std::fs::read_to_string(self.path(file)).unwrap();
        let at = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {file}"));
        u32::try_from(text[..at].matches('\n').count()).unwrap()
    }

    fn stats(&self) -> OutlineStats {
        outline::stats(&self.state)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// Inserts `text` at the start of 0-based `line` of `editor`'s buffer, as typing does.
fn insert_at_line(editor: &Editor, line: u32, text: &str) {
    let buffer = editor.buffer();
    let offset = editor.document().line_index().line_start(line);
    let mut iter = editor.iter_near(offset).unwrap();
    buffer.begin_user_action();
    buffer.insert(&mut iter, text);
    buffer.end_user_action();
}

/// Replaces the first `old` in `editor`'s text by `new`.
fn replace(editor: &Editor, old: &str, new: &str) {
    let text = editor.document().text();
    let at = text
        .find(old)
        .unwrap_or_else(|| panic!("{old:?} is in the text"));
    let buffer = editor.buffer();
    let mut start = editor.iter_near(at).unwrap();
    let mut end = editor.iter_near(at + old.len()).unwrap();
    buffer.begin_user_action();
    buffer.delete(&mut start, &mut end);
    let mut at_iter = editor.iter_near(at).unwrap();
    buffer.insert(&mut at_iter, new);
    buffer.end_user_action();
}

// ───────────────────────── tests ─────────────────────────

fn golden_proj() {
    let f = fixture("golden", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the whole project", |tree| tree == GOLDEN_PROJ);
    assert_eq!(outline::root(&f.state), Some(f.path("main.tex")));
    // Everything is expanded at first: the rows shown are the whole tree.
    let shown: Vec<String> = outline::rows(&f.state);
    let golden: Vec<&str> = GOLDEN_PROJ.lines().collect();
    assert_eq!(shown, golden);
    assert_eq!(outline::page_state(&f.state).as_deref(), Some("tree"));
    assert!(f.stats().aux_reads >= 1);
    // A tab of another kind (the bibliography) keeps the project's outline.
    f.open("refs.bib");
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    assert_eq!(outline::root(&f.state), Some(f.path("main.tex")));
    assert_eq!(outline::tree_text(&f.state), GOLDEN_PROJ);
}

fn chapter_first() {
    let f = fixture("chapter", Some("proj"), &[]);
    f.open("chapters/yoneda.tex");
    f.show_outline();
    // The chapter's root is found next to it (the project feed reads `main.tex`).
    f.wait_tree("the root's project", |tree| tree == GOLDEN_PROJ);
    assert_eq!(outline::root(&f.state), Some(f.path("main.tex")));
}

fn click_navigation() {
    let f = fixture("click", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    // Into the chapter file: it opens in a tab, the cursor on the `\begin{lemma}`.
    assert!(outline::activate_row(&f.state, "Lemma 2.1 (Yoneda)"));
    let lemma = f.line_of("chapters/yoneda.tex", "\\begin{lemma}");
    f.wait_cursor("chapters/yoneda.tex", lemma);
    assert_eq!(f.cursor().1, 0);
    // A narrow window shows the sidebar as an overlay: it gets out of the way.
    if f.state.sidebar.split.is_collapsed() {
        assert!(!f.state.sidebar.split.shows_sidebar());
    }
    // A heading in the root: the cursor goes onto the command.
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    assert!(outline::activate_row(&f.state, "4.1 Long computation"));
    let sub = f.line_of("main.tex", "\\subsection{Long computation}");
    f.wait_cursor("main.tex", sub);
    // Back (Alt+Left / Ctrl+O) returns to the lemma, as every navigation records the jump.
    assert!(goto::back(&f.state));
    f.wait_cursor("chapters/yoneda.tex", lemma);
    // A file row goes to the file's start.
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    assert!(outline::activate_row(&f.state, "1 Introduction"));
    f.wait_cursor("main.tex", f.line_of("main.tex", "\\section{Introduction}"));
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    assert!(outline::activate_row(&f.state, "[chapters/yoneda.tex]"));
    f.wait_cursor("chapters/yoneda.tex", 0);
    assert!(!outline::activate_row(&f.state, "No such row"));
}

fn enter_navigation() {
    let f = fixture("enter", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let position = outline::rows(&f.state)
        .iter()
        .position(|r| r.trim() == "Theorem 3.2 (fpqc descent)")
        .unwrap();
    assert!(outline::activate_position(
        &f.state,
        u32::try_from(position).unwrap()
    ));
    f.wait_cursor("main.tex", f.line_of("main.tex", "\\begin{theorem}[fpqc"));
    // The view has the focus afterwards (typing goes on in the text).
    let editor = f.state.active_editor().unwrap();
    common::pump_until(TIMEOUT, || editor.view().has_focus());
}

fn follow_cursor() {
    let f = fixture("follow", Some("proj"), &[]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let theorem = f.line_of("main.tex", "\\begin{theorem}[fpqc");
    let adjustment = main.view().vadjustment().unwrap();
    // Inside the theorem.
    f.put_cursor(&main, theorem + 2, 3);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("Theorem 3.2 (fpqc descent)")
    );
    // The editor was not scrolled by the outline.
    let scrolled = adjustment.value();
    f.put_cursor(&main, theorem + 1, 0);
    assert_eq!(adjustment.value(), scrolled);
    // After the theorem, in its section's proof.
    let proof = f.line_of("main.tex", "By~\\cite[Tag 023N]");
    f.put_cursor(&main, proof, 0);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("3 Descent")
    );
    // On a subsection.
    let filler = f.line_of("main.tex", "Filler subsection 3");
    f.put_cursor(&main, filler + 3, 0);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("4.4 Filler subsection 3")
    );
    // In the preamble: nothing is selected.
    f.put_cursor(&main, 2, 0);
    assert_eq!(outline::selected_row(&f.state), None);
    // A collapsed parent: its row (the deepest one shown) is selected instead.
    assert!(outline::set_expanded(&f.state, "3 Descent", false));
    f.put_cursor(&main, theorem + 2, 0);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("3 Descent")
    );
    assert!(outline::set_expanded(&f.state, "3 Descent", true));
    f.settle_follow();
    // In the chapter file, before and in its section.
    let chapter = f.open("chapters/yoneda.tex");
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.put_cursor(&chapter, 1, 0);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("2 The Yoneda lemma")
    );
    let lemma = f.line_of("chapters/yoneda.tex", "\\begin{lemma}");
    f.put_cursor(&chapter, lemma + 1, 0);
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("Lemma 2.1 (Yoneda)")
    );
    // Switching tabs follows the other tab's cursor (still in the theorem).
    f.state.select_editor(&main);
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle_follow();
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("Theorem 3.2 (fpqc descent)")
    );
}

fn follow_off() {
    let f = fixture_with("follow-off", Some("proj"), &[], |c| {
        c.outline.follow_cursor = false;
    });
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let theorem = f.line_of("main.tex", "\\begin{theorem}[fpqc");
    f.put_cursor(&main, theorem + 1, 0);
    assert_eq!(outline::selected_row(&f.state), None);
    // Turned on (the page's menu writes the setting; here it is set directly).
    let mut config = (*f.state.config.get()).clone();
    config.outline.follow_cursor = true;
    f.state.set_config(Rc::new(config));
    f.settle_follow();
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("Theorem 3.2 (fpqc descent)")
    );
}

fn follow_menu() {
    let f = fixture("follow-menu", Some("proj"), &[]);
    let config = f.dir.join("settings/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    // The settings of the fixture, which the written file replaces when it is applied.
    std::fs::write(
        &config,
        "# my settings\n[editor]\nvim = false\n[spell]\nenabled = false\n[build]\n\
         on_save = false\n[outline]\n# keep this comment\n",
    )
    .unwrap();
    f.state
        .set_settings_files(Some(lemma_app::settings::SettingsFiles {
            config: config.clone(),
            snippets: f.dir.join("settings/snippets.toml"),
            prompts: f.dir.join("settings/prompts.toml"),
        }));
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let page = outline::page(&f.state).unwrap();
    let state_of = |page: &gtk::Widget| {
        page.activate_action("outline.follow-cursor", None).unwrap();
        f.state.config.outline.follow_cursor
    };
    // Off: written to config.toml (comments kept) and applied at once.
    assert!(!state_of(&page));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# keep this comment"), "{text}");
    assert!(text.contains("follow_cursor = false"), "{text}");
    let theorem = f.line_of("main.tex", "\\begin{theorem}[fpqc");
    f.put_cursor(&main, theorem + 1, 0);
    assert_eq!(outline::selected_row(&f.state), None);
    // On again.
    assert!(state_of(&page));
    f.settle_follow();
    assert_eq!(
        outline::selected_row(&f.state).as_deref(),
        Some("Theorem 3.2 (fpqc descent)")
    );
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("follow_cursor = true")
    );
    // Without a writable config.toml the change holds for the session.
    f.state
        .set_settings_files(Some(lemma_app::settings::SettingsFiles {
            config: PathBuf::from("/dev/null"),
            snippets: f.dir.join("settings/snippets.toml"),
            prompts: f.dir.join("settings/prompts.toml"),
        }));
    assert!(!state_of(&page));
}

fn filter() {
    let f = fixture("filter", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    outline::set_filter(&f.state, "fpqc");
    let (results, selected) = outline::filter_results(&f.state);
    assert_eq!(
        results.first().map(String::as_str),
        Some("Theorem 3.2 (fpqc descent)")
    );
    assert_eq!(selected, Some(0));
    assert_eq!(outline::page_state(&f.state).as_deref(), Some("results"));
    // Fuzzy: letters in order, best first.
    outline::set_filter(&f.state, "yoneda");
    let (results, _) = outline::filter_results(&f.state);
    for row in [
        "Lemma 2.1 (Yoneda)",
        "2 The Yoneda lemma",
        "[chapters/yoneda.tex]",
    ] {
        assert!(results.iter().any(|r| r == row), "{row} in {results:?}");
    }
    outline::set_filter(&f.state, "lng cmp");
    let (results, _) = outline::filter_results(&f.state);
    assert_eq!(
        results.first().map(String::as_str),
        Some("4.1 Long computation")
    );
    // Numbers and theorem names match too.
    outline::set_filter(&f.state, "Prop");
    let (results, _) = outline::filter_results(&f.state);
    assert_eq!(results.first().map(String::as_str), Some("Proposition 4.1"));
    // Enter goes to the selected result.
    outline::set_filter(&f.state, "fpqc");
    outline::activate_filter(&f.state);
    f.wait_cursor("main.tex", f.line_of("main.tex", "\\begin{theorem}[fpqc"));
    // No match; cleared: the tree is back, as it was.
    outline::set_filter(&f.state, "zzzzqqq");
    assert_eq!(outline::page_state(&f.state).as_deref(), Some("none"));
    assert!(outline::filter_results(&f.state).0.is_empty());
    outline::set_filter(&f.state, "");
    assert_eq!(outline::page_state(&f.state).as_deref(), Some("tree"));
    assert_eq!(outline::rows(&f.state).len(), GOLDEN_PROJ.lines().count());
}

fn incremental() {
    let f = fixture("incremental", Some("proj"), &[]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    assert!(outline::set_expanded(&f.state, "4 Sheaves on sites", false));
    assert_eq!(
        outline::is_expanded(&f.state, "4 Sheaves on sites"),
        Some(false)
    );
    let before = f.stats();
    // A new section: one row is created, nothing else.
    let sites = f.line_of("main.tex", "\\section{Sheaves on sites}");
    insert_at_line(&main, sites, "\\section{Fresh}\n");
    f.wait_tree("the new section", |tree| {
        tree.contains("\nFresh\n4 Sheaves")
    });
    let after = f.stats();
    assert_eq!(after.nodes_created - before.nodes_created, 1, "{after:?}");
    assert_eq!(
        outline::is_expanded(&f.state, "4 Sheaves on sites"),
        Some(false)
    );
    // An edited title updates its row in place: same number (from its label), same children,
    // still collapsed.
    replace(
        &main,
        "\\section{Sheaves on sites}",
        "\\section{Sheaves on big sites}",
    );
    f.wait_tree("the edited title", |tree| {
        tree.contains("4 Sheaves on big sites")
    });
    let edited = f.stats();
    assert_eq!(edited.nodes_created, after.nodes_created, "{edited:?}");
    assert!(edited.nodes_updated > after.nodes_updated);
    assert_eq!(
        outline::is_expanded(&f.state, "4 Sheaves on big sites"),
        Some(false)
    );
    // A theorem added inside a collapsed section.
    let filler = f.line_of("main.tex", "\\subsection{Filler subsection 1}");
    insert_at_line(&main, filler, "\\begin{lemma}[New]\nx\n\\end{lemma}\n");
    f.wait_tree("the new lemma", |tree| {
        tree.contains("  Lemma (New)\n  4.2 Filler")
    });
    assert_eq!(f.stats().nodes_created, edited.nodes_created + 1);
    // Removing the new section takes its row away; the rest is as before.
    replace(&main, "\\section{Fresh}\n", "");
    f.wait_tree("the section gone", |tree| !tree.contains("Fresh"));
    // Undo everything: the golden tree again, with the collapse state kept.
    replace(&main, "\\begin{lemma}[New]\nx\n\\end{lemma}\n", "");
    replace(&main, "Sheaves on big sites", "Sheaves on sites");
    f.wait_tree("the original", |tree| tree == GOLDEN_PROJ);
    assert_eq!(
        outline::is_expanded(&f.state, "4 Sheaves on sites"),
        Some(false)
    );
    // Re-expanding shows the children again.
    assert!(outline::set_expanded(&f.state, "4 Sheaves on sites", true));
    assert_eq!(outline::rows(&f.state).len(), GOLDEN_PROJ.lines().count());
}

fn line_shifts() {
    let f = fixture("shifts", Some("proj"), &[]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let descent = f.line_of("main.tex", "\\section{Descent}");
    assert_eq!(outline::row_line(&f.state, "3 Descent"), Some(descent));
    // Three lines above it: the row moves at once, before the project index is updated.
    insert_at_line(&main, 1, "% one\n% two\n% three\n");
    assert_eq!(outline::row_line(&f.state, "3 Descent"), Some(descent + 3));
    let intro = f.line_of("main.tex", "\\section{Introduction}");
    assert_eq!(
        outline::row_line(&f.state, "1 Introduction"),
        Some(intro + 3)
    );
    // A click right away lands on the heading.
    f.state.sidebar.show_page(PAGE_OUTLINE);
    assert!(outline::activate_row(&f.state, "3 Descent"));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        common::pump_for(Duration::from_millis(10));
        if f.cursor() == (descent + 3, 0) {
            break;
        }
        assert!(Instant::now() < deadline, "cursor at {:?}", f.cursor());
    }
    // Deleting the lines moves it back; the index agrees afterwards.
    let start = main.document().line_index().line_start(1);
    let end = main.document().line_index().line_start(4);
    let buffer = main.buffer();
    let mut a = main.iter_near(start).unwrap();
    let mut b = main.iter_near(end).unwrap();
    buffer.delete(&mut a, &mut b);
    assert_eq!(outline::row_line(&f.state, "3 Descent"), Some(descent));
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    assert_eq!(outline::row_line(&f.state, "3 Descent"), Some(descent));
}

const FIGURES_MAIN: &str = r"\documentclass{scrartcl}
\input{preamble}
\begin{document}
\addsec{Preface}
\section{Plots}
\begin{figure}[ht]
  \centering
  \input{plot.tikz}
  \caption[Short caption]{A long caption with $\alpha$}\label{fig:plot}
\end{figure}
\begin{table}
  \caption{Numbers \emph{and} words}
\end{table}
\input{empty}
\input{parts/later}
\end{document}
";

fn figures_and_files() {
    let f = fixture(
        "figures",
        None,
        &[
            ("main.tex", FIGURES_MAIN),
            ("preamble.tex", "\\newtheorem{conj}{Conjecture}\n"),
            ("plot.tikz", "\\draw (0,0) -- (1,1);\n"),
            ("empty.tex", "Some text without structure.\n"),
            (
                "parts/later.tex",
                "\\subsection{Later}\n\\begin{conj}[Big]\nx\n\\end{conj}\n",
            ),
            (
                "main.aux",
                "\\newlabel{fig:plot}{{1}{1}{Short caption}{figure.1}{}}\n\
                 \\@writefile{toc}{\\contentsline {section}{\\nonumberline Preface}{1}{}}\n\
                 \\@writefile{toc}{\\contentsline {section}{\\numberline {1}Plots}{1}{}}\n\
                 \\@writefile{lot}{\\contentsline {table}{\\numberline {1}{\\ignorespaces Numbers \\emph {and} words}}{1}{}}\n\
                 \\@writefile{toc}{\\contentsline {subsection}{\\numberline {1.1}Later}{1}{}}\n",
            ),
        ],
    );
    f.open("main.tex");
    f.show_outline();
    // The preamble file and the TikZ file inside the figure have no rows of their own; the
    // empty body file is listed; captions come from the open text.
    let expected = "\
∗ Preface
1 Plots
  Figure 1 Short caption
  Table 1 Numbers and words
  [empty.tex]
  [parts/later.tex]
    1.1 Later
      Conjecture (Big)
";
    f.wait_tree("figures and files", |tree| tree == expected);
}

fn hidden_is_free() {
    let f = fixture("hidden", Some("proj"), &[]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    // Another page: edits and cursor moves cost the outline nothing.
    f.state.sidebar.show_page(PAGE_CHANGES);
    common::pump_for(Duration::from_millis(50));
    let before = f.stats();
    let sites = f.line_of("main.tex", "\\section{Sheaves on sites}");
    insert_at_line(&main, sites, "\\section{Hidden}\n");
    for line in 0..20 {
        assert!(main.place_cursor_at(main.document().line_index().line_start(line)));
        common::pump_for(Duration::from_millis(2));
    }
    completion::refresh_project(&f.state);
    let state = f.state.clone();
    common::pump_until(TIMEOUT, || !completion::project_busy(&state));
    common::pump_for(Duration::from_millis(300));
    let hidden = f.stats();
    assert_eq!(hidden.syncs, before.syncs);
    assert!(!outline::tree_text(&f.state).contains("Hidden"));
    // Shown again: brought up to date.
    f.show_outline();
    f.wait_tree("the edit", |tree| tree.contains("\nHidden\n"));
}

fn build_numbers() {
    if which("latexmk").is_none() || which("pdflatex").is_none() {
        println!("    (latexmk or pdflatex is missing: skipped)");
        return;
    }
    let f = fixture(
        "build",
        None,
        &[(
            "main.tex",
            "\\documentclass{article}\n\\newtheorem{thm}{Theorem}[section]\n\
             \\begin{document}\n\\section{Alpha}\n\\subsection{Beta}\n\
             \\begin{thm}\\label{t}x\\end{thm}\n\\section{Gamma}\n\
             \\begin{figure}\\caption{Cap}\\end{figure}\n\\end{document}\n",
        )],
    );
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("no numbers before a build", |tree| {
        tree == "Alpha\n  Beta\n    Theorem\nGamma\n  Figure Cap\n"
    });
    let state = f.state.clone();
    state.request_build(lemma_app::build_ui::Reason::Manual);
    let build = state.build.get().unwrap();
    common::pump_until(Duration::from_secs(120), || {
        build.last_outcome().is_some() && !build.is_busy()
    });
    f.settle();
    f.wait_tree("the numbers of the build", |tree| {
        tree == "1 Alpha\n  1.1 Beta\n    Theorem 1.1\n2 Gamma\n  Figure 1 Cap\n"
    });
}

fn expand_collapse_all() {
    let f = fixture("all", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let page = outline::page(&f.state).unwrap();
    assert!(page.activate_action("outline.collapse-all", None).is_ok());
    assert_eq!(
        outline::rows(&f.state),
        [
            "1 Introduction",
            "[chapters/yoneda.tex]",
            "3 Descent",
            "4 Sheaves on sites"
        ]
    );
    // Expanding a collapsed-all row shows its children collapsed too.
    assert!(outline::set_expanded(
        &f.state,
        "[chapters/yoneda.tex]",
        true
    ));
    common::pump_for(Duration::from_millis(20));
    assert_eq!(
        outline::is_expanded(&f.state, "2 The Yoneda lemma"),
        Some(false)
    );
    assert!(page.activate_action("outline.expand-all", None).is_ok());
    assert_eq!(outline::rows(&f.state).len(), GOLDEN_PROJ.lines().count());
}

/// A file row first shown before its file was read (no headings: under the open section) moves
/// to the top level once its `\section` is known, and stays expanded: the rows destroyed by the
/// move notify `expanded = false`, which must not be taken for the user collapsing them.
fn file_read_later() {
    let f = fixture(
        "read-later",
        None,
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\n\\section{Intro}\n\
             \\input{chapters/a}\n\\section{After}\n\\end{document}\n",
        )],
    );
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the unread file", |t| {
        t == "Intro\n  [chapters/a.tex]\nAfter\n"
    });
    // The project feed found no file; it appears (as the feed would put it in after a build).
    f.state.workspace.index.borrow_mut().update_file(
        f.path("chapters/a.tex"),
        std::sync::Arc::new(lemma_syntax::index_file("\\section{A}\n\\subsection{B}\n")),
    );
    outline::refresh(&f.state);
    f.wait_tree("the file's sections", |t| {
        t == "Intro\n[chapters/a.tex]\n  A\n    B\nAfter\n"
    });
    assert_eq!(
        outline::rows(&f.state),
        ["Intro", "[chapters/a.tex]", "  A", "    B", "After"]
    );
    assert_eq!(
        outline::is_expanded(&f.state, "[chapters/a.tex]"),
        Some(true)
    );
}

const THEOREMS_MAIN: &str = "\\documentclass{article}\n\\input{preamble}\n\\begin{document}\n\
\\section{Results}\n\\input{parts/later}\n\\end{document}\n";
const THEOREMS_PREAMBLE: &str = "\\newtheorem{mainthm}{Main Theorem}\n";
const THEOREMS_LATER: &str = "\\subsection{Later}\n\\begin{mainthm}[Big]\nx\n\\end{mainthm}\n";
const THEOREMS_TREE: &str = "Results\n  [parts/later.tex]\n    Later\n      Main Theorem (Big)\n";

/// A theorem environment declared in a preamble file (`mainthm` is none of the common names)
/// gives the rows of the files that use it, whichever of them the project feed reads first:
/// the preamble and the chapter read together, the chapter open alone (as a session restores
/// it; its root and the preamble are found after it was indexed), and the preamble `\input`
/// typed after the chapter was indexed. Nothing but the feed itself indexes the files again
/// (the page is shown without `completion::refresh_project`).
fn theorems_from_a_preamble_file() {
    let files = [
        ("main.tex", THEOREMS_MAIN),
        ("preamble.tex", THEOREMS_PREAMBLE),
        ("parts/later.tex", THEOREMS_LATER),
    ];
    for (name, open) in [
        ("theorems-root", "main.tex"),
        ("theorems-chapter", "parts/later.tex"),
    ] {
        let f = fixture(name, None, &files);
        f.open(open);
        f.state.sidebar.show_page(PAGE_OUTLINE);
        f.wait_tree(&format!("the preamble's theorem with {open} open"), |t| {
            t == THEOREMS_TREE
        });
    }
    let f = fixture(
        "theorems-typed",
        None,
        &[
            (
                "main.tex",
                &THEOREMS_MAIN.replace("\\input{preamble}\n", ""),
            ),
            ("preamble.tex", THEOREMS_PREAMBLE),
            ("parts/later.tex", THEOREMS_LATER),
        ],
    );
    let main = f.open("main.tex");
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.wait_tree("no theorem without the preamble", |t| {
        t == "Results\n  [parts/later.tex]\n    Later\n"
    });
    insert_at_line(&main, 1, "\\input{preamble}\n");
    f.wait_tree("the theorem once the preamble is read", |t| {
        t == THEOREMS_TREE
    });
    // The preamble's declaration goes: the row goes too.
    replace(&main, "\\input{preamble}\n", "");
    f.wait_tree("no theorem once the preamble is gone", |t| {
        t == "Results\n  [parts/later.tex]\n    Later\n"
    });
}

/// Another program changes a chapter that is not open (a `git checkout`, another editor): the
/// page follows without an edit, a build or `outline::is_settled` (which looks at the index
/// again by itself): only the file monitor and the project feed bring the change.
fn a_chapter_changed_on_disk() {
    let f = fixture("disk", Some("proj"), &[]);
    f.open("main.tex");
    f.show_outline();
    f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
    let chapter = f.path("chapters/yoneda.tex");
    let text = std::fs::read_to_string(&chapter).unwrap() + "\\section{Written elsewhere}\n";
    std::fs::write(&chapter, text).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !outline::tree_text(&f.state).contains("  Written elsewhere\n") {
        assert!(
            Instant::now() < deadline,
            "the chapter's new section; the tree is:\n{}",
            outline::tree_text(&f.state)
        );
        common::pump_for(Duration::from_millis(20));
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(program))
            .find(|p| p.is_file())
    })
}

/// The cost per keystroke on the 7k-line corpus (the first 7000 lines of `large.tex`, about
/// 880 rows): bursts of typing (letters, and Enter, which moves the rows below) at random places.
/// Everything the outline does for a key — the document observers, the cursor observer and the
/// `follow` it queues — must stay ≤ 1 ms per key (p99 and mean). Jumping to another place and
/// the patch after the project index caught up are printed.
fn perf_keystroke() {
    let large = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let mut text: String = large.split_inclusive('\n').take(7000).collect();
    text.push_str("\\end{document}\n");
    let f = fixture("perf", None, &[("main.tex", &text)]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the large outline", |tree| tree.lines().count() > 800);
    let rows = outline::tree_text(&f.state).lines().count();
    let context = glib::MainContext::default();
    let mut rng = common::Rng::new(7);
    let mut per_key = Vec::new();
    let mut jumps = Vec::new();
    let before = f.stats();
    for _burst in 0..20 {
        // Jump somewhere (navigation, not typing) and let the outline settle there.
        let line = u32::try_from(rng.below(6800)).unwrap() + 100;
        let start = main.document().line_index().line_start(line);
        let jump_before = f.stats().key_time;
        assert!(main.place_cursor_at(start));
        while context.iteration(false) {}
        jumps.push(f.stats().key_time - jump_before);
        common::pump_for(Duration::from_millis(150));
        // Type 40 keys: letters, and Enter every 12th key.
        for k in 0..40 {
            let key_before = f.stats().key_time;
            let buffer = main.buffer();
            let mut iter = buffer.iter_at_mark(&buffer.get_insert());
            buffer.begin_user_action();
            buffer.insert(&mut iter, if k % 12 == 11 { "\n" } else { "x" });
            buffer.end_user_action();
            while context.iteration(false) {}
            per_key.push(f.stats().key_time - key_before);
        }
    }
    let after = f.stats();
    per_key.sort();
    jumps.sort();
    let keys = u32::try_from(per_key.len()).unwrap();
    let mean = (after.key_time - before.key_time - jumps.iter().sum::<Duration>()) / keys;
    let p99 = per_key[per_key.len() * 99 / 100];
    println!(
        "perf_outline_keystroke: {rows} rows, {keys} keys: mean {mean:?}, p99 {p99:?}, max {:?}; \
         jumps: median {:?}, max {:?}; {} scrolls, slowest {:?}",
        per_key.last().unwrap(),
        jumps[jumps.len() / 2],
        jumps.last().unwrap(),
        after.scrolls - before.scrolls,
        after.max_scroll,
    );
    // The incremental patch once the project index has the edits.
    let synced = f.stats().syncs;
    f.settle();
    f.state.sidebar.show_page(PAGE_OUTLINE);
    f.settle();
    let patched = f.stats();
    println!(
        "perf_outline_keystroke: {} patches after the index updates, the last {:?} (outline \
         {:?}; {} rows created, {} updated in all; the first build took {:?})",
        patched.syncs - synced,
        patched.last_sync,
        patched.last_build,
        patched.nodes_created - after.nodes_created,
        patched.nodes_updated - after.nodes_updated,
        patched.max_sync,
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: budgets not checked)");
        return;
    }
    assert!(mean <= Duration::from_millis(1), "{mean:?} per key");
    assert!(p99 <= Duration::from_millis(1), "p99 {p99:?}");
    assert!(
        patched.last_sync <= Duration::from_millis(5),
        "patch {:?}",
        patched.last_sync
    );
}

/// `large.tex` (60k lines, about 7500 rows): the first outline and the patch after an edit
/// (printed; generous budgets only, the 7k-line corpus is the reference).
fn perf_large() {
    let large = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let f = fixture("perf-large", None, &[("main.tex", &large)]);
    let main = f.open("main.tex");
    f.show_outline();
    f.wait_tree("the large outline", |tree| tree.lines().count() > 7000);
    let first = f.stats();
    let rows = outline::tree_text(&f.state).lines().count();
    let line = f.line_of("main.tex", "\\section{Section 1500}");
    insert_at_line(&main, line, "\\section{Inserted}\n");
    f.wait_tree("the inserted section", |tree| tree.contains("\nInserted\n"));
    let patched = f.stats();
    println!(
        "perf_outline_large_tex: {rows} rows: first outline {:?}; patch after an edit {:?} \
         (outline {:?}, {} rows created)",
        first.max_sync,
        patched.last_sync,
        patched.last_build,
        patched.nodes_created - first.nodes_created,
    );
    if !cfg!(debug_assertions) {
        assert!(
            patched.last_sync <= Duration::from_millis(60),
            "{patched:?}"
        );
    }
}

mod real_clicks {
    //! Real pointer clicks (a private Broadway daemon, see `tests/broadway_input`): a click on a
    //! row's text goes to its line, a click on its arrow only collapses it, and a collapse made
    //! with the arrow is remembered when the row is made again.

    use super::*;
    use gtk::gdk;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    pub fn parent() {
        broadway_input::run_child("outline_real_clicks_child", 17);
    }

    pub fn child() {
        let f = fixture("real-clicks", Some("proj"), &[]);
        f.open("main.tex");
        f.show_outline();
        f.wait_tree("the project", |tree| tree == GOLDEN_PROJ);
        let mut mouse = Mouse::connect();
        common::pump_until(TIMEOUT, || mouse.window_surface().is_some());
        let window = f.state.window.clone();
        // A click on the title of "3 Descent" goes to its line.
        let editor = f.state.active_editor().unwrap();
        assert!(editor.place_cursor_at(0));
        f.settle_follow();
        let title = label_in_outline(&f, "Descent").expect("the row's title");
        mouse.click_widget(&window, &title);
        f.wait_cursor("main.tex", f.line_of("main.tex", "\\section{Descent}"));
        common::pump_until(TIMEOUT, || editor.view().has_focus());
        // A click on the arrow of "2 The Yoneda lemma" collapses it and goes nowhere.
        f.state.sidebar.show_page(PAGE_OUTLINE);
        f.settle();
        assert!(editor.place_cursor_at(0));
        f.settle_follow();
        let arrow =
            arrow_of(&label_in_outline(&f, "The Yoneda lemma").unwrap()).expect("the row's arrow");
        mouse.click_widget(&window, &arrow);
        common::pump_until(TIMEOUT, || {
            outline::is_expanded(&f.state, "2 The Yoneda lemma") == Some(false)
        });
        f.settle_follow();
        assert_eq!(f.cursor(), (0, 0));
        assert_eq!(f.active_path(), Some(f.path("main.tex")));
        assert!(
            !outline::rows(&f.state)
                .iter()
                .any(|r| r.contains("Lemma 2.1"))
        );
        // Remembered: the file row collapsed and expanded makes the row again, collapsed.
        assert!(outline::set_expanded(
            &f.state,
            "[chapters/yoneda.tex]",
            false
        ));
        assert!(outline::set_expanded(
            &f.state,
            "[chapters/yoneda.tex]",
            true
        ));
        f.settle_follow();
        assert_eq!(
            outline::is_expanded(&f.state, "2 The Yoneda lemma"),
            Some(false)
        );
        // Its arrow again: expanded, the lemma is back.
        let arrow = arrow_of(&label_in_outline(&f, "The Yoneda lemma").unwrap()).unwrap();
        mouse.click_widget(&window, &arrow);
        common::pump_until(TIMEOUT, || {
            outline::rows(&f.state)
                .iter()
                .any(|r| r.trim() == "Lemma 2.1 (Yoneda)")
        });
        // Keys on the tree: Left collapses, Right expands and then enters the row, Enter goes.
        assert!(label_in_outline(&f, "Descent").is_some());
        common::pump_until(TIMEOUT, || window.is_active());
        assert!(outline::focus_row(&f.state, "3 Descent"));
        common::pump_for(Duration::from_millis(50));
        mouse.key(gdk::Key::Left);
        common::pump_until(TIMEOUT, || {
            outline::is_expanded(&f.state, "3 Descent") == Some(false)
        });
        mouse.key(gdk::Key::Right);
        common::pump_until(TIMEOUT, || {
            outline::is_expanded(&f.state, "3 Descent") == Some(true)
        });
        mouse.key(gdk::Key::Right);
        common::pump_until(TIMEOUT, || {
            outline::selected_row(&f.state).as_deref() == Some("Definition 3.1")
        });
        mouse.key(gdk::Key::Left);
        common::pump_until(TIMEOUT, || {
            outline::selected_row(&f.state).as_deref() == Some("3 Descent")
        });
        mouse.key(gdk::Key::Down);
        mouse.key(gdk::Key::Return);
        f.wait_cursor("main.tex", f.line_of("main.tex", "\\begin{definition}"));
        // Typing on the list filters it; Enter goes to the best match.
        assert!(label_in_outline(&f, "Introduction").is_some());
        f.settle();
        assert!(outline::focus_row(&f.state, "1 Introduction"));
        common::pump_for(Duration::from_millis(50));
        for key in [gdk::Key::f, gdk::Key::p, gdk::Key::q] {
            mouse.key(key);
        }
        common::pump_until(TIMEOUT, || {
            outline::filter_results(&f.state)
                .0
                .first()
                .map(String::as_str)
                == Some("Theorem 3.2 (fpqc descent)")
        });
        mouse.key(gdk::Key::Return);
        f.wait_cursor("main.tex", f.line_of("main.tex", "\\begin{theorem}[fpqc"));
    }

    /// The label showing `text` in the outline's tree, with the sidebar shown (a narrow window
    /// slides it over the editor; navigating dismisses it).
    fn label_in_outline(f: &Fixture, text: &str) -> Option<gtk::Widget> {
        f.state.sidebar.show_page(PAGE_OUTLINE);
        let page = outline::page(&f.state)?;
        let window = f.state.window.clone();
        let find = || {
            descendants(&page).into_iter().find(|w| {
                w.is_drawable()
                    && w.downcast_ref::<gtk::Label>()
                        .is_some_and(|l| l.text() == text)
            })
        };
        let on_screen = |w: &gtk::Widget| {
            w.compute_point(&window, &gtk::graphene::Point::new(0.0, 0.0))
                .is_some_and(|p| p.x() >= 0.0 && p.y() >= 0.0)
        };
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            common::pump_for(Duration::from_millis(20));
            if let Some(label) = find().filter(|l| on_screen(l)) {
                return Some(label);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
        }
    }

    /// The expander arrow of the row holding `label`.
    fn arrow_of(label: &gtk::Widget) -> Option<gtk::Widget> {
        let mut w = label.parent();
        while let Some(widget) = w {
            if let Ok(expander) = widget.clone().downcast::<gtk::TreeExpander>() {
                return descendants(expander.upcast_ref())
                    .into_iter()
                    .find(|c| c.css_name() == "expander");
            }
            w = widget.parent();
        }
        None
    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = Vec::new();
        let mut stack = vec![widget.clone()];
        while let Some(w) = stack.pop() {
            let mut child = w.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
            out.push(w);
        }
        out
    }

    const ENTER: i32 = 0;
    const POINTER_MOVE: i32 = 2;
    const BUTTON_PRESS: i32 = 3;
    const BUTTON_RELEASE: i32 = 4;
    const KEY_PRESS: i32 = 7;
    const KEY_RELEASE: i32 = 8;
    const ROUNDTRIP_NOTIFY: i32 = 14;
    const BUTTON1_MASK: i32 = 1 << 8;

    /// A browser client of the daemon that sends pointer events (the key client of
    /// `broadway_input` sends keys only) and answers the roundtrips, so frames are drawn.
    /// The surfaces of the display: (id, x, y, width, height).
    type Surfaces = Arc<Mutex<Vec<(i32, i32, i32, i32, i32)>>>;

    struct Mouse {
        writer: Arc<Mutex<TcpStream>>,
        /// The surfaces, from the display stream.
        surfaces: Surfaces,
        time: i32,
    }

    impl Mouse {
        fn connect() -> Self {
            let port: u16 = std::env::var(broadway_input::CHILD_PORT_ENV)
                .expect("in the child")
                .parse()
                .unwrap();
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            write!(
                stream,
                "GET /socket HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: broadway\r\n\
                 Origin: http://127.0.0.1:{port}\r\n\r\n"
            )
            .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("handshake");
                head.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
            let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
            let surfaces: Surfaces = Arc::default();
            let (answers, known) = (writer.clone(), surfaces.clone());
            std::thread::spawn(move || {
                let mut reader = stream;
                while let Ok(message) = read_message(&mut reader) {
                    for command in commands(&message) {
                        match command {
                            Command::Roundtrip(id, tag) => {
                                send(&answers, &[ROUNDTRIP_NOTIFY, 0, 0, id, tag]);
                            }
                            Command::Surface(id, x, y, w, h) => {
                                let mut known = known.lock().unwrap();
                                known.retain(|s| s.0 != id);
                                known.push((id, x, y, w, h));
                            }
                            Command::Move(id, pos, size) => {
                                let mut known = known.lock().unwrap();
                                if let Some(s) = known.iter_mut().find(|s| s.0 == id) {
                                    if let Some((x, y)) = pos {
                                        (s.1, s.2) = (x, y);
                                    }
                                    if let Some((w, h)) = size {
                                        (s.3, s.4) = (w, h);
                                    }
                                }
                            }
                        }
                    }
                }
            });
            Self {
                writer,
                surfaces,
                time: 1000,
            }
        }

        /// The largest surface (the window): id and position.
        fn window_surface(&self) -> Option<(i32, i32, i32)> {
            let surfaces = self.surfaces.lock().unwrap();
            surfaces
                .iter()
                .max_by_key(|s| s.3 * s.4)
                .map(|s| (s.0, s.1, s.2))
        }

        /// Clicks button 1 on the middle of `widget` (enter, move, press, release).
        fn click_widget(&mut self, window: &adw::ApplicationWindow, widget: &gtk::Widget) {
            let middle = gtk::graphene::Point::new(
                widget.width() as f32 / 2.0,
                widget.height() as f32 / 2.0,
            );
            let point = widget
                .compute_point(window, &middle)
                .expect("the widget is in the window");
            let (dx, dy) = window.native().unwrap().surface_transform();
            let (x, y) = (
                (f64::from(point.x()) + dx).round() as i32,
                (f64::from(point.y()) + dy).round() as i32,
            );
            let (id, sx, sy) = self.window_surface().expect("the window's surface");

            let (rx, ry) = (sx + x, sy + y);
            let mut event = |kind: i32, state: i32, extra: &[i32]| {
                self.time += 20;
                let mut words = vec![kind, 0, self.time, id, id, rx, ry, x, y, state];
                words.extend_from_slice(extra);
                send(&self.writer, &words);
                common::pump_for(Duration::from_millis(30));
            };
            event(ENTER, 0, &[0]);
            event(POINTER_MOVE, 0, &[]);
            event(BUTTON_PRESS, BUTTON1_MASK, &[1]);
            event(BUTTON_RELEASE, 0, &[1]);
            common::pump_for(Duration::from_millis(100));
        }

        /// Presses and releases `key` (no modifiers) in the focused window.
        fn key(&mut self, key: gdk::Key) {
            use glib::translate::IntoGlib;
            let keysym = i32::try_from(key.into_glib()).unwrap();
            self.time += 20;
            send(&self.writer, &[KEY_PRESS, 0, self.time, keysym, 0]);
            send(&self.writer, &[KEY_RELEASE, 0, self.time + 5, keysym, 0]);
            common::pump_for(Duration::from_millis(40));
        }
    }

    enum Command {
        Roundtrip(i32, i32),
        Surface(i32, i32, i32, i32, i32),
        Move(i32, Option<(i32, i32)>, Option<(i32, i32)>),
    }

    /// The commands of a display-stream message this client cares about (the op layout of
    /// `broadway_input`'s parser).
    fn commands(message: &[u8]) -> Vec<Command> {
        let mut found = Vec::new();
        let mut pos = 0usize;
        let u16_at = |p: usize| {
            message
                .get(p..p + 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
        };
        let i16_at = |p: usize| u16_at(p).map(|v| i32::from(v as i16));
        let u32_at = |p: usize| {
            message
                .get(p..p + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        while pos + 5 <= message.len() {
            let op = message[pos];
            pos += 5;
            let size = match op {
                0 => 3,
                1 | 10 => 0,
                2 => {
                    if let (Some(id), Some(x), Some(y), Some(w), Some(h)) = (
                        u16_at(pos),
                        i16_at(pos + 2),
                        i16_at(pos + 4),
                        u16_at(pos + 6),
                        u16_at(pos + 8),
                    ) {
                        found.push(Command::Surface(
                            i32::from(id),
                            x,
                            y,
                            i32::from(w),
                            i32::from(h),
                        ));
                    }
                    10
                }
                3..=7 | 12 => 2,
                9 => 4,
                8 => {
                    let (Some(id), Some(&flags)) = (u16_at(pos), message.get(pos + 2)) else {
                        break;
                    };
                    let mut at = pos + 3;
                    let mut position = None;
                    let mut size = None;
                    if flags & 1 != 0 {
                        position = i16_at(at).zip(i16_at(at + 2));
                        at += 4;
                    }
                    if flags & 2 != 0 {
                        size = u16_at(at)
                            .zip(u16_at(at + 2))
                            .map(|(w, h)| (i32::from(w), i32::from(h)));
                    }
                    found.push(Command::Move(i32::from(id), position, size));
                    3 + if flags & 1 != 0 { 4 } else { 0 } + if flags & 2 != 0 { 4 } else { 0 }
                }
                13 => match u32_at(pos + 4) {
                    Some(n) => 8 + n as usize,
                    None => break,
                },
                14 => 4,
                15 => match u32_at(pos + 2) {
                    Some(n) => 6 + 4 * n as usize,
                    None => break,
                },
                16 => {
                    if let (Some(id), Some(tag)) = (u16_at(pos), u32_at(pos + 2)) {
                        found.push(Command::Roundtrip(i32::from(id), tag as i32));
                    }
                    6
                }
                _ => break,
            };
            pos += size;
        }
        found
    }

    fn send(writer: &Mutex<TcpStream>, words: &[i32]) {
        let payload: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mask = [0x5a, 0x17, 0xc3, 0x08];
        let mut frame = vec![0x82, 0x80 | u8::try_from(payload.len()).unwrap()];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        if let Ok(mut stream) = writer.lock() {
            let _ = stream.write_all(&frame);
        }
    }

    fn read_message(reader: &mut TcpStream) -> std::io::Result<Vec<u8>> {
        let mut message = Vec::new();
        loop {
            let mut head = [0u8; 2];
            reader.read_exact(&mut head)?;
            let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
            let mut len = u64::from(head[1] & 0x7f);
            if len == 126 {
                let mut ext = [0u8; 2];
                reader.read_exact(&mut ext)?;
                len = u64::from(u16::from_be_bytes(ext));
            } else if len == 127 {
                let mut ext = [0u8; 8];
                reader.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext);
            }
            let mut mask = [0u8; 4];
            if head[1] & 0x80 != 0 {
                reader.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; usize::try_from(len).unwrap_or(0)];
            reader.read_exact(&mut payload)?;
            if head[1] & 0x80 != 0 {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            if opcode == 0x8 {
                return Err(std::io::ErrorKind::ConnectionAborted.into());
            }
            if opcode >= 0x8 {
                continue;
            }
            message.extend_from_slice(&payload);
            if fin {
                return Ok(message);
            }
        }
    }
}
