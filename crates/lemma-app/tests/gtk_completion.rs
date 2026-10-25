//! The completion provider, its triggers, activation, and Tab precedence (WP2.9a acceptance;
//! ARCHITECTURE §5 "Completion", "Snippets").
//!
//! The project is a copy of `tests/fixtures/proj` (two files, a bibliography and an `.aux`), with
//! `\Z` defined, a figure and a word list added.
//!
//! - **In process:** the proposals in every context, the trigger policy on the scanner's real
//!   contexts, accepting each kind of proposal, Tab precedence 1–4 through the key layer, the
//!   project feed (included files, `.bib`, `.aux`, packages, files other programs change),
//!   `--demo completion`, and the per-keystroke cost (`perf_completion_keystroke`, with
//!   `--ignored`).
//! - **Real input** (`popup_on_real_key_events`, see `broadway_input`): the popup opens while
//!   typing, Enter accepts, Tab moves between the stops GtkSourceView manages, steps out, and
//!   indents, Escape closes, in plain editing and in Vim insert mode.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_completion`.

mod broadway_input;
mod common;

use adw::prelude::*;
use broadway_input::{Browser, CONTROL, SHIFT};
use gtk::gdk;
use lemma_app::editor::completion::{self, Trigger};
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, VimMode, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_complete::{Insert, Item, ItemKind};
use lemma_config::Config;
use lemma_core::{Origin, ProjectRoot, TextEdit};
use std::cell::Cell;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

const REAL_CHILD: &str = "real_input_completion";
const DEMO_CHILD: &str = "real_display_demo";

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[(REAL_CHILD, real_child), (DEMO_CHILD, demo_child)]);
    }
    let mut tests: Vec<common::Test> = vec![
        ("popup_on_real_key_events", popup_on_real_key_events),
        ("proposals_in_every_context", proposals_in_every_context),
        ("the_project_feed_reads_the_project", project_feed),
        (
            "the_project_feed_follows_files_on_disk",
            project_feed_follows_the_disk,
        ),
        (
            "the_project_feed_follows_files_on_disk_at_its_cap",
            project_feed_at_its_cap,
        ),
        (
            "the_project_feed_finds_the_root_of_a_chapter",
            project_feed_from_a_chapter,
        ),
        (
            "the_project_feed_reads_local_packages",
            project_feed_reads_local_packages,
        ),
        ("triggers_on_the_scanners_contexts", triggers),
        ("accepting_each_kind_of_proposal", accepting),
        ("accepted_proposals_rank_higher", recency),
        ("tab_precedence_through_the_key_layer", tab_precedence),
        ("detach_disconnects_everything", detach),
        (
            "end_offers_the_environment_that_is_open",
            end_offers_the_open_environment,
        ),
        (
            "one_reindex_job_per_document_at_a_time",
            reindex_single_flight,
        ),
        (
            "no_completion_or_snippets_outside_latex",
            no_completion_outside_latex,
        ),
        ("demo_completion_shows_the_popup", demo),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_completion_keystroke", perf_keystroke));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(10);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    /// The text main.tex was opened with.
    original: String,
}

/// The project: `tests/fixtures/proj` plus `\Z`, a probe line before `\printbibliography`, a
/// figure and a word list.
fn project(name: &str) -> PathBuf {
    let dir = common::temp_dir(name);
    let src = common::fixtures().join("proj");
    std::fs::create_dir_all(dir.join("chapters")).unwrap();
    std::fs::create_dir_all(dir.join("figures")).unwrap();
    let main = std::fs::read_to_string(src.join("main.tex"))
        .unwrap()
        .replace(
            "\\newcommand{\\Hom}",
            "\\newcommand{\\Z}{\\mathbb{Z}}\n\\newcommand{\\Hom}",
        )
        .replace("\\printbibliography", "PROBE\n\\printbibliography");
    std::fs::write(dir.join("main.tex"), main).unwrap();
    for file in ["refs.bib", "main.aux", "chapters/yoneda.tex"] {
        std::fs::copy(src.join(file), dir.join(file)).unwrap();
    }
    std::fs::write(dir.join("figures/plot.pdf"), b"%PDF-1.4\n").unwrap();
    std::fs::write(dir.join("words.txt"), "quasicoherent\nMac Lane\n").unwrap();
    // An unrelated document next to the project: never part of its index.
    std::fs::write(
        dir.join("other.tex"),
        "\\documentclass{article}\n\\newcommand{\\Unrelated}{u}\n\\begin{document}\n\
         \\section{Other}\\label{other:label}\n\\end{document}\n",
    )
    .unwrap();
    dir
}

fn fixture(name: &str, vim: bool) -> Fixture {
    fixture_at(&project(name), "main.tex", vim)
}

/// A window on the project in `dir`, showing `file`.
fn fixture_at(dir: &std::path::Path, file: &str, vim: bool) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = dir.to_path_buf();
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = vim;
    config.spell.word_lists = vec![dir.join("words.txt")];
    // Spell checking (WP2.9b) is not under test here.
    config.spell.enabled = false;
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    lemma_app::app::set_accels(&app, &state.config.keys);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let editor = block_on(state.open_file(&dir.join(file))).expect("open");
    // The view is the window's focus widget. (Not `has_focus`: on the shared Broadway display
    // another test process's window may be the active one.)
    common::pump_until(TIMEOUT, || editor.view().is_focus());
    let original = editor.document().text();
    let f = Fixture {
        state,
        editor,
        original,
    };
    f.settle();
    f
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    fn buffer(&self) -> &sourceview5::Buffer {
        self.editor.buffer()
    }

    fn text(&self) -> String {
        self.editor.document().text()
    }

    /// Back to the text main.tex was opened with (ending snippet sessions first).
    fn reset(&self) {
        lemma_app::editor::snippets::end_snippet_sessions(self.editor.view());
        // Not `set_text`: Vim's insert mode keeps a user action open, and `set_text` is an
        // irreversible action.
        let buffer = self.buffer();
        let (mut start, mut end) = buffer.bounds();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &self.original);
    }

    /// Waits until the project feed has nothing left to do.
    fn settle(&self) {
        completion::refresh_project(&self.state);
        common::pump_until(TIMEOUT, || !completion::project_busy(&self.state));
    }

    /// The line that holds the probe (`PROBE` at first).
    fn probe_line(&self) -> i32 {
        let text = self.text();
        let at = text.find("\n\\printbibliography").expect("the probe line");
        i32::try_from(text[..at].matches('\n').count()).unwrap()
    }

    /// The probe line's text.
    fn probe_text(&self) -> String {
        let line = usize::try_from(self.probe_line()).unwrap();
        self.text().split('\n').nth(line).unwrap().to_owned()
    }

    /// Makes the probe line `before` + `after` with the cursor between them. Like every edit
    /// that is not typing, it ends the snippet sessions first.
    fn probe(&self, before: &str, after: &str) {
        lemma_app::editor::snippets::end_snippet_sessions(self.editor.view());
        let line = self.probe_line();
        let buffer = self.buffer();
        let mut start = buffer.iter_at_line(line).unwrap();
        let mut end = start;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &format!("{before}{after}"));
        let cursor = buffer
            .iter_at_line_index(line, i32::try_from(before.len()).unwrap())
            .unwrap();
        buffer.place_cursor(&cursor);
    }

    /// The proposals with the cursor after `before` on the probe line.
    fn items(&self, before: &str, after: &str) -> Vec<Item> {
        self.probe(before, after);
        completion::items_at_cursor(&self.state, &self.editor)
    }

    fn labels(&self, before: &str) -> Vec<String> {
        self.items(before, "")
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    fn cursor_offset(&self) -> i32 {
        let buffer = self.buffer();
        let line = self.probe_line();
        buffer.iter_at_mark(&buffer.get_insert()).offset()
            - buffer.iter_at_line(line).unwrap().offset()
    }

    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; probe line {:?}, popup visible {}, shown {:?}, \
                 focus {}",
                self.probe_text(),
                completion::popup_visible(&self.editor),
                labels_of(&completion::shown_items(&self.state, &self.editor)),
                self.editor.view().has_focus()
            );
            common::pump_for(Duration::from_millis(5));
        }
    }

    fn accept(&self, label: &str) {
        let item = completion::items_at_cursor(&self.state, &self.editor)
            .into_iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no proposal {label:?} at {:?}", self.probe_text()));
        completion::accept(&self.state, &self.editor, &item);
        common::pump_for(Duration::from_millis(5));
    }

    /// One key press through the key layer, Vim and the text view's insertion (as M1 tests do).
    fn press(&self, keyval: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        let event = KeyEvent {
            keyval,
            keycode: 0,
            state,
            vim: self.editor.vim().is_some(),
        };
        let result = keys::dispatch(self.editor.view(), &event);
        if result == glib::Propagation::Proceed
            && !state.intersects(gdk::ModifierType::CONTROL_MASK)
        {
            let consumed = self
                .editor
                .vim()
                .is_some_and(|im| vim::feed_key(&im, self.editor.view(), keyval, state));
            if !consumed && let Some(c) = keyval.to_unicode().filter(|c| !c.is_control()) {
                self.buffer()
                    .insert_interactive_at_cursor(&c.to_string(), true);
            }
        }
        common::pump_for(Duration::from_millis(2));
        result
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(broadway_input::keyval(c), gdk::ModifierType::empty());
        }
    }
}

// ───────────── Proposals in every context ─────────────

fn proposals_in_every_context() {
    let f = fixture("contexts", false);
    // Commands in math: `\fr` → `\frac` first, a snippet with two stops.
    let items = f.items("$x = \\fr", "$");
    assert_eq!(items[0].label, "\\frac", "{:?}", labels_of(&items));
    assert!(matches!(items[0].insert, Insert::Snippet(_)));
    assert_eq!(items[0].kind, ItemKind::Command);
    assert!(items.len() <= 50);
    // Text-only commands are hidden in math, shown in text.
    assert!(!f.labels("$\\sectio").contains(&"\\section".to_owned()));
    assert!(f.labels("\\sectio").contains(&"\\section".to_owned()));
    // Commands that belong in text come after the math ones in math (`\frame` after `\frac`,
    // `\cfrac`, `\dfrac`, `\tfrac`, `\frown`, `\mathfrak`), and math-only ones are not offered
    // in text.
    let math = f.labels("$x = \\fr");
    let text_ones = ["\\frame", "\\framebox", "\\footref", "\\figurename"];
    let first_text = math
        .iter()
        .position(|l| text_ones.contains(&l.as_str()))
        .unwrap_or(math.len());
    for name in [
        "\\frac",
        "\\cfrac",
        "\\dfrac",
        "\\tfrac",
        "\\frown",
        "\\mathfrak",
    ] {
        assert!(
            math.iter()
                .position(|l| l == name)
                .is_some_and(|p| p < first_text),
            "{name} before the text commands: {math:?}"
        );
    }
    let text = f.labels("see \\fr");
    assert!(!text.contains(&"\\frac".to_owned()), "{text:?}");
    assert!(text.contains(&"\\frame".to_owned()), "{text:?}");
    // Glyphs: symbols, and the user's `\Z` (ℤ) from the preamble.
    let alpha = f.items("$\\alph", "$");
    assert_eq!(alpha[0].label, "\\alpha");
    assert_eq!(alpha[0].glyph, Some('α'));
    let z = f.items("$\\Z", "$");
    let user_z = z.iter().find(|i| i.label == "\\Z").expect("the user's \\Z");
    assert_eq!(user_z.kind, ItemKind::UserMacro);
    assert_eq!(user_z.glyph, Some('ℤ'));
    // Environments after `\begin{`.
    let envs = f.labels("\\begin{al");
    assert_eq!(&envs[..3], ["align", "align*", "alignat"], "{envs:?}");
    // The project's theorems are environments too.
    assert!(f.labels("\\begin{lem").contains(&"lemma".to_owned()));
    // References: labels of both files with their `.aux` numbers.
    let refs = f.items("see \\ref{", "}");
    let yoneda = refs
        .iter()
        .find(|i| i.label == "lem:yoneda")
        .expect("a label of chapters/yoneda.tex");
    assert_eq!(yoneda.kind, ItemKind::Label);
    let detail = yoneda.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("2.1") && detail.contains("lemma"),
        "{detail}"
    );
    let eq = f.items("see \\eqref{eq:co", "}");
    assert_eq!(eq[0].label, "eq:cocycle");
    assert!(eq[0].detail.as_deref().unwrap_or("").contains("(1)"));
    // Citations with author and year, also after a comma.
    let cites = f.items("\\cite{", "}");
    let mac_lane = cites
        .iter()
        .find(|i| i.label == "MacLane")
        .expect("MacLane");
    let detail = mac_lane.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("Mac Lane") && detail.contains("1998"),
        "{detail}"
    );
    assert!(
        f.labels("\\cite{SGA1, Vis")
            .first()
            .is_some_and(|l| l == "Vistoli")
    );
    // Files and packages.
    assert!(
        f.labels("\\includegraphics{")
            .contains(&"figures/plot.pdf".to_owned())
    );
    assert!(
        f.labels("\\input{chap")
            .contains(&"chapters/yoneda".to_owned())
    );
    assert!(
        f.labels("\\usepackage{amsm")
            .contains(&"amsmath".to_owned())
    );
    // An exact line-begin snippet trigger hides everything else.
    let thm = f.items("  thm", "");
    assert_eq!(labels_of(&thm), ["thm"]);
    assert_eq!(thm[0].kind, ItemKind::Snippet);
    // Words in text: the word list and the project's own words; never in math.
    assert!(f.labels("a quasi").contains(&"quasicoherent".to_owned()));
    assert!(f.labels("The Lor").contains(&"Lorem".to_owned()));
    assert!(!f.labels("$quasi").contains(&"quasicoherent".to_owned()));
    // Nothing in comments, and nothing for label definitions.
    assert!(f.items("% \\fr", "").is_empty());
    assert!(f.items("\\label{se", "}").is_empty());
    // A typed text that is exactly a complete proposal is proposed, but not preselected: Enter
    // makes a new line after `\item`.
    let item = f.items("\\item", "");
    assert_eq!(item[0].label, "\\item");
    assert!(completion::show(&f.state, &f.editor));
    assert!(!completion::popup_selects_first(&f.state, &f.editor));
    f.probe("\\ite", "");
    assert!(completion::show(&f.state, &f.editor));
    assert!(completion::popup_selects_first(&f.state, &f.editor));
    completion::hide(&f.state, &f.editor);
}

fn labels_of(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

/// The feed indexes the included chapter and the bibliography, reads the `.aux`, and loads
/// the packages' command tables; an edit is indexed after a short delay. The chapter's words
/// follow what Lemma saves in it, in its tab or closed, from the next build on.
fn project_feed() {
    let f = fixture("feed", false);
    let index = f.state.workspace.index.borrow();
    assert!(
        index
            .files
            .keys()
            .any(|p| p.ends_with("chapters/yoneda.tex")),
        "{:?}",
        index.files.keys().collect::<Vec<_>>()
    );
    assert_eq!(index.all_bib().len(), 4);
    assert_eq!(
        index.aux.labels.get("thm:fpqc").map(|(n, _)| n.as_str()),
        Some("3.2")
    );
    drop(index);
    let packages = completion::loaded_packages(&f.state);
    for package in ["mathtools", "hyperref", "class-amsart"] {
        assert!(packages.contains(&package), "{package} in {packages:?}");
    }
    // `\coloneqq` comes from mathtools.
    assert!(f.labels("$\\colone").contains(&"\\coloneqq".to_owned()));
    // A new label is proposed once the edit is indexed.
    f.probe("\\label{sec:new}", "");
    f.wait("the edit to be indexed", || {
        !completion::project_busy(&f.state)
            && f.state
                .workspace
                .index
                .borrow()
                .find_label("sec:new")
                .is_some()
    });
    assert!(f.labels("\\ref{sec:n").contains(&"sec:new".to_owned()));
    // A new macro decides `if_undefined` bodies.
    assert!(!completion::macro_defined(&f.state, "Ker"));
    f.probe("\\DeclareMathOperator{\\Ker}{Ker}", "");
    f.wait("the macro to be indexed", || {
        completion::macro_defined(&f.state, "Ker")
    });
    // After a build: the new `.aux` (beside the PDF), the changed bibliography, new files.
    let dir = f.state.workspace.root().path().to_path_buf();
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    let aux =
        std::fs::read_to_string(dir.join("main.aux")).unwrap() + "\\newlabel{sec:new}{{9.9}{7}}\n";
    std::fs::write(dir.join(".build/main.aux"), aux).unwrap();
    let bib = std::fs::read_to_string(dir.join("refs.bib")).unwrap()
        + "\n@book{Lurie, author = {Lurie, Jacob}, title = {Higher Topos Theory}, year = {2009}}\n";
    std::fs::write(dir.join("refs.bib"), bib).unwrap();
    std::fs::write(dir.join("figures/new.png"), b"").unwrap();
    let builds = completion::base_word_builds(&f.state);
    completion::refresh_after_build(&f.state, Some(&dir.join(".build/main.pdf")));
    f.wait("the build's files to be read", || {
        !completion::project_busy(&f.state)
    });
    // No `.tex` file read from disk changed: their words are not collected again.
    assert_eq!(completion::base_word_builds(&f.state), builds);
    let refs = f.items("\\ref{sec:n", "}");
    let new = refs.iter().find(|i| i.label == "sec:new").expect("sec:new");
    assert!(
        new.detail.as_deref().unwrap_or("").contains("9.9"),
        "{new:?}"
    );
    let cites = f.labels("\\cite{Lur");
    assert_eq!(cites.first().map(String::as_str), Some("Lurie"));
    assert!(
        f.labels("\\includegraphics{")
            .contains(&"figures/new.png".to_owned())
    );
    // The chapter read from disk, saved with a new word while open and closed, then saved
    // again while closed (as an agent's edit is): no discovery job reads these texts, yet the
    // next build's pass collects the words of the file as saved.
    let idle =
        || lemma_app::agent::watcher::is_idle(&f.state) && !completion::project_busy(&f.state);
    let chapter_path = dir.join("chapters/yoneda.tex");
    let chapter = block_on(f.state.open_file(&chapter_path)).expect("open the chapter");
    let buffer = chapter.buffer();
    buffer.insert(&mut buffer.end_iter(), "\nA quasitriangular case.\n");
    block_on(chapter.document().save()).unwrap();
    // The file monitor sees the save while the tab is open (Lemma's own write).
    common::pump_for(Duration::from_millis(500));
    common::pump_until(TIMEOUT, idle);
    f.state.close_editor(&chapter, false);
    drop(chapter);
    common::pump_until(TIMEOUT, idle);
    completion::refresh_after_build(&f.state, None);
    f.wait("the word saved in the chapter's tab", || {
        !completion::project_busy(&f.state)
            && f.labels("A quasitri")
                .contains(&"quasitriangular".to_owned())
    });
    let closed = block_on(f.state.workspace.get_or_load(&chapter_path)).unwrap();
    assert!(!closed.is_open());
    let end = closed.text().len();
    closed
        .apply_edits(
            &[TextEdit::insert(end, "A pseudomonotone case.\n")],
            Origin::User,
        )
        .unwrap();
    block_on(closed.save()).unwrap();
    common::pump_for(Duration::from_millis(500));
    common::pump_until(TIMEOUT, idle);
    completion::refresh_after_build(&f.state, None);
    f.wait("the word saved in the closed chapter", || {
        !completion::project_busy(&f.state)
            && f.labels("A pseudomon")
                .contains(&"pseudomonotone".to_owned())
    });
}

/// Files another program writes reach the feed without a build: a citation key added to the
/// bibliography, a label added to the chapter (not open), an image created and one deleted
/// (file arguments). Saving a document (written through a temporary file renamed onto it)
/// does not list the project's files again.
fn project_feed_follows_the_disk() {
    let f = fixture("feed-disk", false);
    let dir = f.state.workspace.root().path().to_path_buf();
    let idle =
        || lemma_app::agent::watcher::is_idle(&f.state) && !completion::project_busy(&f.state);
    common::pump_for(Duration::from_millis(300));
    common::pump_until(TIMEOUT, idle);
    let walks = completion::file_walks(&f.state);
    f.probe("Saved here.", "");
    block_on(f.editor.document().save()).unwrap();
    common::pump_for(Duration::from_millis(500));
    common::pump_until(TIMEOUT, idle);
    assert_eq!(
        completion::file_walks(&f.state),
        walks,
        "a save lists the files"
    );
    let bib = std::fs::read_to_string(dir.join("refs.bib")).unwrap()
        + "\n@book{Lurie, author = {Lurie, Jacob}, title = {Higher Topos Theory}, year = {2009}}\n";
    std::fs::write(dir.join("refs.bib"), bib).unwrap();
    let builds = completion::base_word_builds(&f.state);
    let chapter = std::fs::read_to_string(dir.join("chapters/yoneda.tex")).unwrap()
        + "\\section{Elsewhere}\\label{sec:elsewhere}\nThe zygomorphic case.\n";
    std::fs::write(dir.join("chapters/yoneda.tex"), &chapter).unwrap();
    std::fs::write(dir.join("figures/drawn.png"), b"").unwrap();
    std::fs::remove_file(dir.join("figures/plot.pdf")).unwrap();
    f.wait("the files written elsewhere", || {
        let index = f.state.workspace.index.borrow();
        index.find_label("sec:elsewhere").is_some()
            && index.all_bib().iter().any(|e| e.key == "Lurie")
    });
    assert!(f.labels("\\cite{Lur").contains(&"Lurie".to_owned()));
    assert!(
        f.labels("\\ref{sec:else")
            .contains(&"sec:elsewhere".to_owned())
    );
    // The chapter's words follow its text; the same text read again collects nothing.
    f.wait("the chapter's new word", || {
        !completion::project_busy(&f.state)
            && f.labels("The zygom").contains(&"zygomorphic".to_owned())
    });
    let builds = completion::base_word_builds(&f.state) - builds;
    assert!(builds >= 1, "{builds} word builds");
    let builds = completion::base_word_builds(&f.state);
    std::fs::write(dir.join("chapters/yoneda.tex"), &chapter).unwrap();
    let chapter_path = lemma_app::workspace::normalize(&dir.join("chapters/yoneda.tex"));
    assert!(completion::disk_changed(&f.state, &chapter_path, false));
    common::pump_until(TIMEOUT, idle);
    assert_eq!(completion::base_word_builds(&f.state), builds);
    f.wait("the new image", || {
        let files = f.labels("\\includegraphics{");
        files.contains(&"figures/drawn.png".to_owned())
            && !files.contains(&"figures/plot.pdf".to_owned())
    });
    assert!(completion::file_walks(&f.state) > walks);
    // The chapters' directory moved out of the project (`mv`): its files get no events of their
    // own, yet their labels and their place in the file list go.
    let outside = common::temp_dir("feed-disk-moved-out");
    std::fs::remove_dir(&outside).unwrap();
    std::fs::rename(dir.join("chapters"), &outside).unwrap();
    f.wait("the chapters moved out", || {
        f.state
            .workspace
            .index
            .borrow()
            .find_label("sec:elsewhere")
            .is_none()
            && !f
                .labels("\\input{")
                .iter()
                .any(|file| file.starts_with("chapters/"))
    });
}

/// At the cap of files read from disk (300, `MAX_DISK_FILES`), a file of the index that another
/// program changes is still read again, and the files past the cap stay out: the file read
/// again does not give its place to them. The 40 packages of the preamble, looked for next to
/// the root, take no place from the chapters either.
fn project_feed_at_its_cap() {
    let dir = common::temp_dir("feed-cap");
    // 300 includes: `aaa.tex`, whose own includes are past the cap (and listed first, as the
    // index orders its files by path), then 299 chapters.
    let mut main = String::from("\\documentclass{article}\n");
    for package in DISTRIBUTION_PACKAGES {
        main += &format!("\\usepackage{{{package}}}\n");
    }
    main += "\\begin{document}\n\\input{aaa}\n";
    let mut aaa = String::new();
    std::fs::create_dir_all(dir.join("c")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    for i in 0..299 {
        main += &format!("\\input{{c/c{i:03}}}\n");
        let chapter = format!("\\section{{Chapter {i}}}\\label{{c:{i}}}\n");
        std::fs::write(dir.join(format!("c/c{i:03}.tex")), chapter).unwrap();
    }
    for i in 0..5 {
        aaa += &format!("\\input{{sub/s{i}}}\n");
        let sub = format!("\\section{{Sub {i}}}\\label{{s:{i}}}\n");
        std::fs::write(dir.join(format!("sub/s{i}.tex")), sub).unwrap();
    }
    main += "\\end{document}\n";
    std::fs::write(dir.join("main.tex"), main).unwrap();
    std::fs::write(dir.join("aaa.tex"), aaa).unwrap();
    let f = fixture_at(&dir, "main.tex", false);
    let idle =
        || lemma_app::agent::watcher::is_idle(&f.state) && !completion::project_busy(&f.state);
    let has = |label: &str| f.state.workspace.index.borrow().find_label(label).is_some();
    common::pump_until(TIMEOUT, idle);
    assert!(has("c:298") && !has("s:0"), "the cap");
    let packages = completion::loaded_packages(&f.state);
    assert!(packages.contains(&"tikz"), "{packages:?}");
    std::fs::write(
        dir.join("c/c150.tex"),
        "\\section{Changed}\\label{c:changed}\n",
    )
    .unwrap();
    f.wait("the chapter changed on disk", || has("c:changed"));
    common::pump_until(TIMEOUT, idle);
    assert!(!has("c:150"));
    assert!((0..5).all(|i| !has(&format!("s:{i}"))), "past the cap");
}

/// Distribution packages loaded by the local-package tests' root: none of them is in its
/// directory.
const DISTRIBUTION_PACKAGES: &[&str] = &[
    "amsmath",
    "amssymb",
    "amsthm",
    "mathtools",
    "hyperref",
    "geometry",
    "graphicx",
    "xcolor",
    "tikz",
    "booktabs",
    "enumitem",
    "caption",
    "subcaption",
    "float",
    "fancyhdr",
    "microtype",
    "babel",
    "fontenc",
    "inputenc",
    "lmodern",
    "csquotes",
    "cleveref",
    "siunitx",
    "listings",
    "xspace",
    "url",
    "setspace",
    "titlesec",
    "array",
    "multirow",
    "longtable",
    "tabularx",
    "makecell",
    "placeins",
    "appendix",
    "bm",
    "cancel",
    "todonotes",
    "mdframed",
    "thmtools",
];

/// Local packages and classes reach the index (`\usepackage{mymacros}` with `mymacros.sty`
/// next to the root, what it loads in turn, the root's `\documentclass` with its `.cls`), so
/// their macros are offered, without their internal `@` names; the 40 packages of the
/// distribution beside them are looked for once, still load their command tables and take
/// nothing from the index. Unloading the package takes its files out of the index again.
fn project_feed_reads_local_packages() {
    let dir = common::temp_dir("feed-packages");
    let mut main = String::from("\\documentclass{mythesis}\n");
    for package in DISTRIBUTION_PACKAGES {
        main += &format!("\\usepackage{{{package}}}\n");
    }
    main += "\\usepackage{mymacros}\n\\begin{document}\nText.\nPROBE\n\\printbibliography\n\
             \\end{document}\n";
    std::fs::write(dir.join("main.tex"), main).unwrap();
    std::fs::write(
        dir.join("mymacros.sty"),
        "\\ProvidesPackage{mymacros}\n\\RequirePackage{myextra}\n\
         \\newcommand{\\my@helper}{x}\n\\newcommand{\\mydef}{\\my@helper}\n",
    )
    .unwrap();
    std::fs::write(dir.join("myextra.sty"), "\\newcommand{\\myextra}{y}\n").unwrap();
    std::fs::write(
        dir.join("mythesis.cls"),
        "\\ProvidesClass{mythesis}\n\\LoadClass{article}\n\\newcommand{\\thesistitle}{t}\n",
    )
    .unwrap();
    let f = fixture_at(&dir, "main.tex", false);
    let indexed = || -> Vec<String> {
        let index = f.state.workspace.index.borrow();
        let mut names: Vec<String> = index
            .files
            .keys()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        indexed(),
        ["main.tex", "myextra.sty", "mymacros.sty", "mythesis.cls"]
    );
    for (typed, macro_name) in [
        ("\\myd", "\\mydef"),
        ("\\myex", "\\myextra"),
        ("\\thesist", "\\thesistitle"),
    ] {
        let labels = f.labels(typed);
        assert!(
            labels.contains(&macro_name.to_owned()),
            "{typed}: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.contains('@')),
            "{typed}: {labels:?}"
        );
    }
    let packages = completion::loaded_packages(&f.state);
    for package in ["mathtools", "hyperref", "tikz", "siunitx", "cleveref"] {
        assert!(packages.contains(&package), "{package} in {packages:?}");
    }
    // Without the package, its files leave the index and its macros the proposals.
    let buffer = f.buffer();
    let text = f.text();
    let at = i32::try_from(text.find("\\usepackage{mymacros}\n").unwrap()).unwrap();
    let (mut start, mut end) = (buffer.iter_at_offset(at), buffer.iter_at_offset(at + 22));
    buffer.delete(&mut start, &mut end);
    f.wait("the package to leave the index", || {
        !completion::project_busy(&f.state) && indexed() == ["main.tex", "mythesis.cls"]
    });
    assert!(!f.labels("\\myd").contains(&"\\mydef".to_owned()));
}

/// Opening only a chapter: the root next to it is found (it includes the chapter), with its
/// macros, labels, bibliography and `.aux`; the unrelated document beside it stays out.
fn project_feed_from_a_chapter() {
    let f = fixture_at(&project("chapter"), "chapters/yoneda.tex", false);
    let index = f.state.workspace.index.borrow();
    assert!(
        index.files.keys().any(|p| p.ends_with("main.tex")),
        "the root: {:?}",
        index.files.keys().collect::<Vec<_>>()
    );
    assert!(
        !index.files.keys().any(|p| p.ends_with("other.tex")),
        "an unrelated document"
    );
    assert_eq!(index.all_bib().len(), 4);
    assert_eq!(
        index.aux.labels.get("lem:yoneda").map(|(n, _)| n.as_str()),
        Some("2.1")
    );
    drop(index);
    assert!(completion::macro_defined(&f.state, "Hom"));
    assert!(!completion::macro_defined(&f.state, "Unrelated"));
    let buffer = f.buffer();
    buffer.place_cursor(&buffer.end_iter());
    buffer.insert_at_cursor("\\ref{sec:");
    let labels: Vec<String> = completion::items_at_cursor(&f.state, &f.editor)
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.contains(&"sec:intro".to_owned()), "{labels:?}");
    assert!(
        !labels.iter().any(|l| l.starts_with("other:")),
        "{labels:?}"
    );
    // File arguments are relative to the root's directory, not the chapter's.
    for (command, file) in [
        ("\\includegraphics{", "figures/plot.pdf"),
        ("\\input{", "chapters/yoneda"),
    ] {
        let buffer = f.buffer();
        buffer.place_cursor(&buffer.end_iter());
        // (The brace closes the argument typed before.)
        buffer.insert_at_cursor(&format!("}}\n{command}"));
        // (The scanner's contexts follow the edit.)
        common::pump_for(Duration::from_millis(5));
        let labels: Vec<String> = completion::items_at_cursor(&f.state, &f.editor)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert!(labels.iter().any(|l| l == file), "{command}: {labels:?}");
    }
}

// ───────────── Triggers ─────────────

fn triggers() {
    let f = fixture("triggers", false);
    let t = |before: &str, after: &str| {
        f.probe(before, after);
        let c = before.chars().next_back().unwrap();
        completion::trigger_at_cursor(&f.state, &f.editor, c)
    };
    assert_eq!(t("$x + \\", "$"), Some(Trigger::Command));
    assert_eq!(t("$x + \\fr", "$"), Some(Trigger::Command));
    assert_eq!(t("text \\emp", ""), Some(Trigger::Command));
    assert_eq!(t("see \\ref{", "}"), Some(Trigger::Argument));
    assert_eq!(t("see \\ref{eq:", "}"), Some(Trigger::Argument));
    assert_eq!(t("\\cite{SGA1,", "}"), Some(Trigger::Argument));
    assert_eq!(t("\\cite[p.~3]{SGA1, ", "}"), Some(Trigger::Argument));
    assert_eq!(t("\\begin{", "}"), Some(Trigger::Argument));
    assert_eq!(t("\\usepackage{", "}"), Some(Trigger::Argument));
    assert_eq!(
        t("\\includegraphics[width=3cm]{", "}"),
        Some(Trigger::Argument)
    );
    assert_eq!(t("\\input{", "}"), Some(Trigger::Argument));
    // Commands the scanner does not classify, through the word lists' classifiers.
    assert_eq!(t("see \\autoref{", "}"), Some(Trigger::Argument));
    // Words: in text from `completion.min_word` (2) characters on; never in math.
    assert_eq!(t("a t", ""), None);
    assert_eq!(t("a th", ""), Some(Trigger::Word));
    assert_eq!(t("$ab", "$"), None);
    // Never: comments, label definitions, `\frac{`, a line break `\\`, punctuation.
    assert_eq!(t("% \\fr", ""), None);
    assert_eq!(t("\\label{", "}"), None);
    assert_eq!(t("$\\frac{", "}$"), None);
    assert_eq!(t("a\\\\", ""), None);
    assert_eq!(t("word.", ""), None);
    // `completion.auto = false`: no words, but commands and arguments still open the popup.
    let mut config = (*f.state.config.get()).clone();
    config.completion.auto = false;
    f.state.set_config(Rc::new(config));
    assert_eq!(t("a th", ""), None);
    assert_eq!(t("$\\fr", "$"), Some(Trigger::Command));
    assert_eq!(t("\\ref{", "}"), Some(Trigger::Argument));
}

// ───────────── Accepting ─────────────

fn accepting() {
    let f = fixture("accept", false);
    // A command snippet: one undo step, the cursor in the first stop.
    f.probe("$x = \\fr", "$");
    f.accept("\\frac");
    assert_eq!(f.probe_text(), "$x = \\frac{}{}$");
    assert!(completion::snippet_active(&f.state, &f.editor));
    f.type_text("a");
    assert_eq!(f.probe_text(), "$x = \\frac{a}{}$");
    f.buffer().undo();
    f.buffer().undo();
    assert_eq!(
        f.probe_text(),
        "$x = \\fr$",
        "undo gives back the typed text"
    );
    // A command right before its arguments keeps them.
    f.probe("$\\fr", "{a}{b}$");
    f.accept("\\frac");
    assert_eq!(f.probe_text(), "$\\frac{a}{b}$");
    // A plain command.
    f.probe("$\\alph", "$");
    f.accept("\\alpha");
    assert_eq!(f.probe_text(), "$\\alpha$");
    // An environment after `\begin{`: the whole environment when it is not closed further on.
    f.probe("\\begin{al", "");
    f.accept("align");
    let text = f.text();
    assert!(
        text.contains("\\begin{align}\n  \n\\end{align}\n\\printbibliography"),
        "{text}"
    );
    // The cursor waits in the body.
    let cursor = f.buffer().iter_at_mark(&f.buffer().get_insert());
    assert_eq!(cursor.line(), f.probe_line() - 1);
    assert_eq!(cursor.line_offset(), 2);
    // … also when the typing helpers paired the brace.
    f.reset();
    f.probe("\\begin{gat", "}");
    f.accept("gather");
    assert!(f.text().contains("\\begin{gather}\n  \n\\end{gather}\n"));
    // … and just the name (with its brace) when an `\end{align}` follows.
    f.reset();
    f.probe("\\begin{al", "}\n  x\n\\end{align}");
    f.accept("align");
    assert!(f.text().contains("\\begin{align}\n  x\n\\end{align}\n"));
    assert!(!f.text().contains("\\end{align}\n\\end{align}"));
    // … the whole environment inside one of the same name (the outer `\end` is the outer's),
    // and when the only `\end` further on is in a comment.
    f.reset();
    f.probe(
        "\\begin{itemize}\\item a \\begin{it",
        "\\item b\\end{itemize}",
    );
    f.accept("itemize");
    let text = f.text();
    assert_eq!(
        text.matches("\\begin{itemize}").count(),
        text.matches("\\end{itemize}").count(),
        "{text}"
    );
    f.reset();
    let buffer = f.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "% old: \\end{center}\n");
    f.probe("\\begin{cen", "");
    f.accept("center");
    assert!(
        f.text().contains("\\begin{center}\n  \n\\end{center}\n"),
        "{}",
        f.text()
    );
    // `\end{`: the name and the closing brace.
    f.reset();
    f.probe("\\end{equa", "");
    f.accept("equation");
    assert_eq!(f.probe_text(), "\\end{equation}");
    // Labels and citations replace just the item.
    f.probe("see \\ref{sec:in", "}");
    f.accept("sec:intro");
    assert_eq!(f.probe_text(), "see \\ref{sec:intro}");
    f.probe("\\cite{SGA1, Vis", "}");
    f.accept("Vistoli");
    assert_eq!(f.probe_text(), "\\cite{SGA1, Vistoli}");
    // A manual snippet from the popup.
    f.reset();
    f.probe("  th", "");
    f.accept("thm");
    assert!(
        f.text()
            .contains("  \\begin{theorem}\n    \n  \\end{theorem}")
    );
    // A word.
    f.reset();
    f.probe("a quasi", "");
    f.accept("quasicoherent");
    assert_eq!(f.probe_text(), "a quasicoherent");
}

fn recency() {
    let f = fixture("recency", false);
    let score = |f: &Fixture| {
        f.items("$\\fr", "$")
            .iter()
            .find(|i| i.label == "\\frown")
            .map(|i| i.score)
            .expect("\\frown")
    };
    let before = score(&f);
    for _ in 0..2 {
        f.probe("$\\fr", "$");
        f.accept("\\frown");
    }
    assert!(score(&f) > before, "accepting raises the rank");
}

// ───────────── Tab precedence ─────────────

/// Tab in the key layer: 1. an exact manual trigger expands (even with the popup open); 2. in an
/// active snippet the key goes on to GtkSourceView (its next stop); 3. else the cursor steps
/// out past the next closing `}`, `)`, `]` or `$`; 4. else the key goes on (indentation).
fn tab_precedence() {
    for vim in [false, true] {
        let f = fixture(&format!("tab-{vim}"), vim);
        if vim {
            // Insert mode at the end of the probe line.
            f.probe("", "");
            f.press(gdk::Key::A, gdk::ModifierType::SHIFT_MASK);
            assert_eq!(f.editor.vim_status().unwrap().mode, VimMode::Insert);
        }
        let tab = |f: &Fixture| f.press(gdk::Key::Tab, gdk::ModifierType::empty());
        // 1. `thm` at the line start, popup open or not.
        f.probe("  thm", "");
        completion::show(&f.state, &f.editor);
        assert_eq!(tab(&f), glib::Propagation::Stop);
        assert!(
            f.text()
                .contains("  \\begin{theorem}\n    \n  \\end{theorem}")
        );
        assert!(!completion::popup_visible(&f.editor));
        // 2. Inside an active snippet the key goes on to GtkSourceView.
        f.reset();
        f.probe("$\\fr", "$");
        f.accept("\\frac");
        assert!(completion::snippet_active(&f.state, &f.editor));
        assert_eq!(tab(&f), glib::Propagation::Proceed);
        assert_eq!(
            f.press(gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Proceed
        );
        // 3. No snippet: out past the next closing delimiter on the line.
        f.probe("$\\frac{a", "}{b}$");
        assert!(!completion::snippet_active(&f.state, &f.editor));
        assert_eq!(tab(&f), glib::Propagation::Stop);
        assert_eq!(f.cursor_offset(), "$\\frac{a}".len() as i32);
        assert_eq!(tab(&f), glib::Propagation::Stop);
        assert_eq!(f.cursor_offset(), "$\\frac{a}{b}".len() as i32);
        assert_eq!(tab(&f), glib::Propagation::Stop);
        assert_eq!(f.cursor_offset(), "$\\frac{a}{b}$".len() as i32);
        // 4. Nothing applies: the key goes on (the view indents).
        assert_eq!(tab(&f), glib::Propagation::Proceed);
        f.probe("word", "");
        assert_eq!(tab(&f), glib::Propagation::Proceed);
        // Shift+Tab outside a snippet goes on too (the view unindents).
        assert_eq!(
            f.press(gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Proceed
        );
        // Ctrl+Tab is not ours.
        assert_eq!(
            f.press(gdk::Key::Tab, gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Proceed
        );
        if vim {
            // In normal mode Tab is Vim's.
            f.press(gdk::Key::Escape, gdk::ModifierType::empty());
            f.probe("  thm", "");
            assert_eq!(tab(&f), glib::Propagation::Proceed);
            assert!(f.probe_text().ends_with("thm"));
        }
    }
}

/// `detach` removes the provider, the key handler and the document observers.
fn detach() {
    let f = fixture("detach", false);
    let layer = f.editor.view().key_layer().expect("a key layer");
    let handlers = layer.len();
    completion::detach(&f.state, &f.editor);
    assert_eq!(layer.len(), handlers - 1);
    // Typing no longer expands anything.
    f.probe("$x", "$");
    f.type_text(" ff");
    assert_eq!(f.probe_text(), "$x ff$");
    assert!(completion::items_at_cursor(&f.state, &f.editor).is_empty());
    // Detaching twice is harmless.
    completion::detach(&f.state, &f.editor);
}

/// `--demo completion` resolves with the popup on screen. It runs on a private Broadway display
/// (see `broadway_input`) without a browser, so frames are drawn (the real-input tests' browser
/// never acknowledges frames) and no other test process shares the screen.
fn demo() {
    broadway_input::run_child(DEMO_CHILD, 3);
}

fn demo_child() {
    let f = fixture("demo", true);
    // The demo places `\fr` at the first math position of the open document.
    block_on(lemma_app::testmode::completion::run_demo(f.state.clone())).expect("the demo");
    assert!(completion::popup_visible(&f.editor));
    let shown = completion::shown_items(&f.state, &f.editor);
    assert_eq!(shown.first().map(|i| i.label.as_str()), Some("\\frac"));
}

// ───────────── Real input ─────────────

fn popup_on_real_key_events() {
    broadway_input::run_child(REAL_CHILD, 2);
}

/// Real key events, synchronized with a probe in the key layer.
struct Real {
    browser: Browser,
    seen: Rc<Cell<u64>>,
}

impl Real {
    fn new(f: &Fixture) -> Self {
        let seen: Rc<Cell<u64>> = Rc::default();
        let count = seen.clone();
        keys::register(f.editor.view(), i32::MAX, move |_, _| {
            count.set(count.get() + 1);
            glib::Propagation::Proceed
        })
        .expect("a key layer");
        Self {
            browser: Browser::connect(),
            seen,
        }
    }

    fn key(&mut self, f: &Fixture, key: gdk::Key, mods: i32) {
        let before = self.seen.get();
        self.browser.key(key, mods);
        f.wait("the key", || self.seen.get() > before);
        common::pump_for(Duration::from_millis(5));
    }

    fn text(&mut self, f: &Fixture, text: &str) {
        for c in text.chars() {
            let (key, mods) = match c {
                '\t' => (gdk::Key::Tab, 0),
                '\n' => (gdk::Key::Return, 0),
                c if c.is_uppercase() => (broadway_input::keyval(c), SHIFT),
                c => (broadway_input::keyval(c), 0),
            };
            self.key(f, key, mods);
        }
    }

    /// Starts typing on the probe line, empty (in Vim: from normal mode with `A`).
    fn start(&mut self, f: &Fixture, before: &str, after: &str) {
        self.key(f, gdk::Key::Escape, 0);
        if f.editor.vim().is_some() {
            f.wait("normal mode", || {
                f.editor.vim_status().unwrap().mode == VimMode::Normal
            });
            f.probe("", "");
            self.key(f, gdk::Key::A, SHIFT);
            f.wait("insert mode", || {
                f.editor.vim_status().unwrap().mode == VimMode::Insert
            });
        }
        f.probe(before, after);
        f.wait("no popup", || !completion::popup_visible(&f.editor));
    }

    fn wait_popup(&self, f: &Fixture, first: &str) {
        f.wait(&format!("the popup with {first:?} first"), || {
            completion::popup_visible(&f.editor)
                && completion::shown_items(&f.state, &f.editor)
                    .first()
                    .is_some_and(|i| i.label == first)
        });
    }
}

fn real_child() {
    let f = fixture("real", false);
    let mut real = Real::new(&f);
    for vim in [false, true] {
        f.state.set_vim_enabled(vim);
        common::pump_for(Duration::from_millis(30));
        real_popup(&f, &mut real, vim);
        real_tab(&f, &mut real);
    }
}

/// The popup opens on its triggers and Enter accepts the first command; Escape closes it.
fn real_popup(f: &Fixture, real: &mut Real, vim: bool) {
    // `\` in math opens it; typing refilters; Enter accepts `\frac`; Tab walks its stops.
    real.start(f, "$x = ", "$");
    real.text(f, "\\fr");
    real.wait_popup(f, "\\frac");
    real.text(f, "\n");
    f.wait("\\frac", || f.probe_text() == "$x = \\frac{}{}$");
    assert!(!completion::popup_visible(&f.editor));
    real.text(f, "a\tb\tc");
    assert_eq!(f.probe_text(), "$x = \\frac{a}{b}c$");
    // Shift+Tab goes back to the previous stop.
    real.start(f, "$", "$");
    real.text(f, "\\fr");
    real.wait_popup(f, "\\frac");
    real.text(f, "\na\tb");
    real.key(f, gdk::Key::ISO_Left_Tab, SHIFT);
    real.text(f, "x");
    assert_eq!(f.probe_text(), "$\\frac{x}{b}$");
    // References open after `{`, citations again after `,`.
    real.start(f, "see ", "");
    real.text(f, "\\ref{sec:i");
    real.wait_popup(f, "sec:intro");
    real.start(f, "", "");
    real.text(f, "\\cite{SGA1,");
    f.wait("the citation popup", || {
        completion::popup_visible(&f.editor)
            && completion::shown_items(&f.state, &f.editor)
                .iter()
                .any(|i| i.label == "Vistoli")
    });
    // Escape closes it (and in Vim leaves insert mode).
    real.key(f, gdk::Key::Escape, 0);
    f.wait("the popup to close", || {
        !completion::popup_visible(&f.editor)
    });
    if vim {
        assert_eq!(f.editor.vim_status().unwrap().mode, VimMode::Normal);
    }
    // A word popup selects nothing: Enter makes a new line.
    real.start(f, "The Lor", "");
    real.text(f, "e");
    f.wait("the word popup", || {
        completion::popup_visible(&f.editor)
            && completion::shown_items(&f.state, &f.editor)
                .iter()
                .any(|i| i.label == "Lorem")
    });
    let line = f.probe_line();
    real.text(f, "\n");
    f.wait("a new line", || f.probe_line() == line + 1);
    assert!(f.text().contains("The Lore\n\n\\printbibliography"));
    // Undo the new line so the probe stays one line.
    real.key(f, gdk::Key::Escape, 0);
    if vim {
        real.text(f, "u");
    } else {
        real.key(f, broadway_input::keyval('z'), CONTROL);
    }
    f.wait("the undo", || f.probe_line() == line);
}

/// Tab precedence on real keys: an exact trigger expands with the popup open, a snippet's stops
/// are walked by GtkSourceView, the cursor steps out of `{…}`, else the view indents.
fn real_tab(f: &Fixture, real: &mut Real) {
    // 1. `thm` at the line start (the popup shows just the snippet).
    real.start(f, "", "");
    real.text(f, "  th");
    real.text(f, "m");
    f.wait("the snippet popup", || completion::popup_visible(&f.editor));
    real.text(f, "\t[Main]");
    assert!(
        f.text()
            .contains("  \\begin{theorem}[Main]\n    \n  \\end{theorem}"),
        "{}",
        f.text()
    );
    // 2. covered in `real_popup` (Tab between `\frac`'s stops).
    // 3. Out of a group without a snippet.
    real.start(f, "$\\frac{a", "}{b}$");
    real.text(f, "\tx");
    assert_eq!(f.probe_text(), "$\\frac{a}x{b}$");
    // 4. Nothing applies: the view indents.
    real.start(f, "word", "");
    real.text(f, "\t");
    f.wait("the indentation", || f.probe_text() != "word");
    assert!(f.probe_text().starts_with("word") && f.probe_text().trim_end() == "word");
    real.key(f, gdk::Key::Escape, 0);
}

// ───────────── Performance ─────────────

/// Main-thread time per keystroke with completion and autosnippets on, up to the next frame:
/// the key layer, the insertion (document, scanner, observers), the popup policy, recomputing
/// the proposals as a refilter does, and the view's snapshot. Budget (ARCHITECTURE §7):
/// keystroke to frame ≤ 8 ms p99, checked in release builds.
fn perf_keystroke() {
    let f = fixture("perf", false);
    // A larger document: the fixture's body repeated (about 5k lines).
    let text = f.text();
    let start = text.find("\\section{Introduction}").unwrap();
    let end = text.find("PROBE").unwrap();
    let body = &text[start..end];
    let mut big = text[..end].to_owned();
    while big.lines().count() < 5000 {
        big.push_str(body);
    }
    big.push_str(&text[end..]);
    f.buffer().set_text(&big);
    f.settle();
    // GtkTextView lays out a newly set text in the background, in short idle steps; the first
    // jump to the end of it (the probe line) waits for that. It happens once after a document is
    // loaded, not on a keystroke, so it is done before the measurement.
    f.probe("", "");
    f.editor
        .view()
        .scroll_mark_onscreen(&f.buffer().get_insert());
    common::pump_for(Duration::from_millis(300));
    let script = "We have $x_n \\to \\alpha$ by \\cite{Vistoli} and \\ref{eq:cocycle}, so every \
                  map $f\\colon X \\to Y$ with $a ff b$ is flat. Hence $H^i(X, \\cF) = 0$ for \
                  $i > 0$, as in \\eqref{eq:amitsur}. Lorem ipsum dolor sit amet.";
    let view = f.editor.view().clone();
    // The window's own frames: Broadway renders a whole frame in software (about 50 ms) about
    // once a second, whatever changed. They are reported, not counted: the keystroke's frame is
    // the snapshot of the view below.
    let frame_start: Rc<Cell<Option<Instant>>> = Rc::default();
    let frames: Rc<std::cell::RefCell<Vec<Duration>>> = Rc::default();
    if let Some(clock) = view.frame_clock() {
        let start = frame_start.clone();
        clock.connect_before_paint(move |_| start.set(Some(Instant::now())));
        let (start, frames) = (frame_start.clone(), frames.clone());
        clock.connect_after_paint(move |_| {
            if let Some(t) = start.take() {
                frames.borrow_mut().push(t.elapsed());
            }
        });
    }
    let mut times = Vec::new();
    let mut window_frames = Vec::new();
    // (time, round * 1000 + index, key, [key handling, other main-loop work, proposals, frame])
    let mut slowest: Vec<(Duration, usize, char, [Duration; 4])> = Vec::new();
    let context = glib::MainContext::default();
    for round in 0..3 {
        f.probe(&format!("Round {round}: "), "");
        for (i, c) in script.chars().enumerate() {
            frames.borrow_mut().clear();
            let started = Instant::now();
            f.press(broadway_input::keyval(c), gdk::ModifierType::empty());
            let pressed = Instant::now();
            while context.pending() {
                context.iteration(false);
            }
            let pending = Instant::now();
            let _ = completion::items_at_cursor(&f.state, &f.editor);
            let proposed = Instant::now();
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&view)).snapshot(
                &snapshot,
                f64::from(view.width()),
                f64::from(view.height()),
            );
            drop(snapshot.to_node());
            let painted: Duration = frames.borrow().iter().sum();
            window_frames.extend(frames.borrow().iter().copied());
            // `press` pumps for 2 ms after the key: not part of the keystroke's work.
            let took = started
                .elapsed()
                .saturating_sub(Duration::from_millis(2))
                .saturating_sub(painted);
            times.push(took);
            let phases = [
                pressed - started,
                (pending - pressed).saturating_sub(painted),
                proposed - pending,
                proposed.elapsed(),
            ];
            slowest.push((took, round * 1000 + i, c, phases));
        }
    }
    times.sort();
    let p = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    let (p50, p99, max) = (p(0.5), p(0.99), times[times.len() - 1]);
    println!(
        "    keystroke with completion on a {}-line document ({} keys): p50 {p50:?}, p99 \
         {p99:?}, max {max:?}",
        big.lines().count(),
        times.len()
    );
    slowest.sort_by_key(|s| std::cmp::Reverse(s.0));
    println!(
        "    slowest keys (time, round * 1000 + index, key, [key, main loop, proposals, \
         frame]): {:?}",
        &slowest[..3]
    );
    println!("    window frames rendered meanwhile (not counted): {window_frames:?}");
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(p99 <= Duration::from_millis(8), "p99 {p99:?} over 8 ms");
    }
}

/// `\end{` closes whatever is open, also an environment that starts math, offered first; a name
/// typed in full is not preselected (Enter makes a new line).
fn end_offers_the_open_environment() {
    let f = fixture("end-open", false);
    for (body, first) in [
        ("\\begin{align}\n  a &= b\n\\end{al", "align"),
        ("\\begin{align*}\n  a &= b\n\\end{", "align*"),
        ("\\begin{equation}\n  a = b\n\\end{eq", "equation"),
        ("\\begin{gather*}\n  a = b\n\\end{ga", "gather*"),
        (
            "\\begin{itemize}\n\\item a\n\\begin{enumerate}\n\\item b\n\\end{",
            "enumerate",
        ),
        (
            "\\begin{itemize}\n\\item a % \\end{itemize}\n\\end{it",
            "itemize",
        ),
    ] {
        f.reset();
        f.probe("", "");
        f.buffer().insert_at_cursor(body);
        common::pump_for(Duration::from_millis(5));
        let labels: Vec<String> = completion::items_at_cursor(&f.state, &f.editor)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(
            labels.first().map(String::as_str),
            Some(first),
            "{body:?}: {labels:?}"
        );
    }
    // Typed in full: proposed, not preselected.
    f.reset();
    f.probe("", "");
    f.buffer()
        .insert_at_cursor("\\begin{equation}\n  a = b\n\\end{equation");
    common::pump_for(Duration::from_millis(5));
    assert!(completion::show(&f.state, &f.editor));
    assert!(!completion::popup_selects_first(&f.state, &f.editor));
    completion::hide(&f.state, &f.editor);
}

/// Re-indexing runs one job per document at a time: edits made while it runs are indexed once
/// more when it ends, so an older text is never merged after a newer one.
fn reindex_single_flight() {
    let f = fixture("single-flight", false);
    let jobs = |f: &Fixture| -> u32 {
        let state = completion::project_feed_state(&f.state);
        state
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no job count in {state:?}"))
    };
    assert_eq!(jobs(&f), 0, "settled");
    for k in 0..5 {
        f.probe(&format!("\\label{{flight:{k}}}"), "");
        completion::refresh_project(&f.state);
        assert!(
            jobs(&f) <= 1,
            "{}",
            completion::project_feed_state(&f.state)
        );
    }
    f.wait("the feed to settle", || !completion::project_busy(&f.state));
    let index = f.state.workspace.index.borrow();
    assert!(index.find_label("flight:4").is_some());
    for k in 0..4 {
        assert!(
            index.find_label(&format!("flight:{k}")).is_none(),
            "flight:{k}"
        );
    }
}

/// A file that is not LaTeX (Markdown) gets no completion and no autosnippets, and stays out of
/// the project index.
fn no_completion_outside_latex() {
    let f = fixture("not-latex", false);
    let dir = f.state.workspace.root().path().to_path_buf();
    std::fs::write(dir.join("NOTES.md"), "# Notes\n").unwrap();
    let notes = block_on(f.state.open_file(&dir.join("NOTES.md"))).expect("open");
    common::pump_until(TIMEOUT, || notes.view().is_focus());
    let buffer = notes.buffer();
    buffer.place_cursor(&buffer.end_iter());
    buffer.insert_at_cursor("see \\fr");
    assert!(completion::items_at_cursor(&f.state, &notes).is_empty());
    completion::refresh_project(&f.state);
    f.wait("the feed to settle", || !completion::project_busy(&f.state));
    assert!(
        !f.state
            .workspace
            .index
            .borrow()
            .files
            .keys()
            .any(|p| p.ends_with("NOTES.md"))
    );
    assert!(!completion::snippet_active(&f.state, &notes));
}
