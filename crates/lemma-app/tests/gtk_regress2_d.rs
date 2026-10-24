//! Regression tests for verified defects of the M2 review pass, area D (review turns,
//! persistence, Changes panel). Each test reproduces one defect and fails until it is fixed.
//!
//! Run offscreen on a private display (focus matters for one test):
//! `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_d`.

mod broadway_input;
mod common;

use adw::prelude::*;
use broadway_input::Browser;
use gtk::gdk;
use lemma_agent::{EditorRequest, ToolReply};
use lemma_app::agent::AgentState;
use lemma_app::agent::history::{self, HistoryReason};
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::Editor;
use lemma_app::review_persist::{self, StateFile};
use lemma_app::review_ui::{self, Decision};
use lemma_app::testmode::review::reply_text;
use lemma_app::window::{PAGE_CHANGES, WindowState};
use lemma_app::workspace::Workspace;
use lemma_config::{Config, Paths};
use lemma_core::{ProjectRoot, fnv1a64};
use lemma_review::HunkId;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[(FOCUS_CHILD, reject_turn_then_space)]);
    }
    // Rejecting a file the agent created moves it to the trash: keep that out of the user's.
    let data = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress2-d-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    common::run(&[
        // D-2 / D-9
        (
            "regress2_save_then_type_keeps_review",
            regress2_save_then_type_keeps_review,
        ),
        (
            "regress2_type_before_first_write_keeps_review",
            regress2_type_before_first_write_keeps_review,
        ),
        // D-3 / D-16
        (
            "regress2_alt_u_never_reverts_an_older_decision",
            regress2_alt_u_never_reverts_an_older_decision,
        ),
        // D-4 / D-14 / D-23
        (
            "regress2_persist_snapshot_does_not_stall",
            regress2_persist_snapshot_does_not_stall,
        ),
        // D-6 / D-17
        #[cfg(unix)]
        (
            "regress2_non_utf8_root_keeps_reviews",
            regress2_non_utf8_root_keeps_reviews,
        ),
        // D-7
        (
            "regress2_running_turn_rows_do_not_decide",
            regress2_running_turn_rows_do_not_decide,
        ),
        // D-12
        (
            "regress2_history_restore_is_not_the_last_turn",
            regress2_history_restore_is_not_the_last_turn,
        ),
        // D-13
        (
            "regress2_review_feedback_is_bounded",
            regress2_review_feedback_is_bounded,
        ),
        // D-15
        (
            "regress2_panel_keeps_rows_while_typing_in_a_hunk",
            regress2_panel_keeps_rows_while_typing_in_a_hunk,
        ),
        // D-18
        (
            "regress2_flush_writes_while_a_write_is_queued",
            regress2_flush_writes_while_a_write_is_queued,
        ),
        // D-20
        (
            "regress2_reject_turn_leaves_focus_off_the_toast",
            regress2_reject_turn_leaves_focus_off_the_toast,
        ),
        // D-22
        (
            "regress2_restore_notice_gives_way_to_a_new_turn",
            regress2_restore_notice_gives_way_to_a_new_turn,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix} {i}\n")).collect()
}

/// Per-test XDG directories under cargo's temp dir: the review state never touches the user's.
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

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

/// A project directory with `files`.
fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = common::temp_dir(name);
    write_files(&dir, files);
    dir
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// An interactive window (`scripted` false) on `dir` that persists into `paths`.
fn window(dir: &Path, paths: &Paths) -> Fixture {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = false;
    let ws = Workspace::new(ProjectRoot::new(dir).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        Some(paths.clone()),
        ws,
        false,
    );
    state.window.set_default_size(1000, 720);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir: dir.to_path_buf(),
    }
}

impl Fixture {
    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.path(file))).expect("open")
    }

    fn doc(&self, file: &str) -> DocRef {
        self.state
            .workspace
            .get(&self.path(file))
            .unwrap_or_else(|| panic!("no document {file}"))
    }

    fn ids(&self, file: &str) -> Vec<HunkId> {
        self.state
            .workspace
            .get(&self.path(file))
            .and_then(|d| {
                d.review
                    .borrow()
                    .as_ref()
                    .map(|r| r.hunks().iter().map(|h| h.id).collect())
            })
            .unwrap_or_default()
    }

    /// (current text, turns) of every hunk of `file`.
    fn hunks(&self, file: &str) -> Vec<(String, Vec<u32>)> {
        let doc = self.doc(file);
        let text = doc.text();
        doc.review
            .borrow()
            .as_ref()
            .map(|r| {
                r.hunks()
                    .iter()
                    .map(|h| (text[h.cur_bytes.clone()].to_owned(), h.turns.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn agent(&self, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, 1, req))
    }

    fn edit_with(&self, file: &str, old: &str, new: &str, replace_all: bool) {
        let reply = self.agent(EditorRequest::Edit {
            path: file.into(),
            old_string: old.into(),
            new_string: new.into(),
            replace_all,
        });
        assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
    }

    fn edit(&self, file: &str, old: &str, new: &str) {
        self.edit_with(file, old, new, false);
    }

    fn disk(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    fn refresh(&self) {
        self.state.review.get().unwrap().refresh();
    }

    fn state_file(&self) -> PathBuf {
        review_persist::state_file(&self.state).expect("persistence is on")
    }

    fn wait_restored(&self) {
        common::pump_until(TIMEOUT, || review_persist::is_restored(&self.state));
        common::pump_for(Duration::from_millis(30));
    }

    /// The window goes away without saving its documents (as after Discard, or a crash after
    /// the last write).
    fn discard(self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(50));
    }

    fn close(self) {
        self.state.window.close();
        common::pump_for(Duration::from_millis(50));
    }
}

/// The parsed state file at `path`, if there is one that parses.
fn read_state(path: &Path) -> Option<StateFile> {
    std::fs::read(path)
        .ok()
        .and_then(|b| review_persist::parse(&b).ok())
}

/// Inserts `text` at the start of `doc`'s buffer, as typing does.
fn type_at_start(doc: &DocRef, text: &str) {
    let buffer = doc.buffer().unwrap();
    buffer.insert(&mut buffer.start_iter(), text);
}

/// Inserts `text` at byte `at` of `doc`'s buffer, as typing does.
fn type_at(doc: &DocRef, at: usize, text: &str) {
    let buffer = doc.buffer().unwrap();
    let mut iter = doc.iter_at_byte(at).unwrap();
    buffer.insert(&mut iter, text);
}

/// Every widget below `root` (depth first), `root` included.
fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(c) = child {
        out.extend(descendants(&c));
        child = c.next_sibling();
    }
    out
}

/// The Changes panel's widgets.
fn panel_widgets(f: &Fixture) -> Vec<gtk::Widget> {
    f.refresh();
    let panel = f.state.review.get().unwrap().changes_panel().unwrap();
    descendants(panel.widget().upcast_ref())
}

/// The Changes panel's row for `title` (a file), once.
fn panel_row(f: &Fixture, title: &str) -> adw::ActionRow {
    let rows: Vec<adw::ActionRow> = panel_widgets(f)
        .into_iter()
        .filter_map(|w| w.downcast::<adw::ActionRow>().ok())
        .filter(|r| r.title() == title)
        .collect();
    assert_eq!(rows.len(), 1, "one row for {title}");
    rows[0].clone()
}

// ----- D-2 / D-9: the entry kept for a dirty document is not the one on disk -----

/// The agent edits a.tex (its edit saves the buffer) and the state is written. The user types,
/// saves (`:w`) and keeps typing at once, then the window goes away without saving (Discard, or
/// a crash). The disk still holds the agent's edit, so the next session must restore its hunk;
/// the entry kept for the dirty document is the one from before the save, whose hash no longer
/// matches the file.
fn regress2_save_then_type_keeps_review() {
    let paths = temp_paths("r2-save-type");
    let dir = project("r2-save-type", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    f.edit("a.tex", "a 5\n", "a five\n");
    let file = f.state_file();
    common::pump_until(TIMEOUT, || {
        read_state(&file).is_some_and(|s| s.reviews.len() == 1)
    });
    let doc = f.doc("a.tex");
    type_at_start(&doc, "% one\n");
    common::pump_for(Duration::from_millis(100));
    block_on(doc.save()).unwrap();
    type_at_start(&doc, "% two\n");
    common::pump_for(review_persist::DEBOUNCE + Duration::from_millis(300));
    review_persist::flush(&f.state);
    let disk = f.disk("a.tex");
    assert!(disk.contains("a five") && disk.contains("% one"));
    let saved = read_state(&file).map(|s| s.reviews[0].review.current_hash);
    eprintln!(
        "    disk hash {:x}, saved entry hash {saved:x?}",
        fnv1a64(&[disk.as_bytes()])
    );
    f.discard();
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    let restored = g.ids("a.tex").len();
    g.close();
    assert_eq!(
        restored, 1,
        "the agent's pending change on disk was not restored after Discard"
    );
}

/// The user types in a.tex right after the agent's edit, before the first write of the state:
/// no entry is ever kept for the dirty document, so the state file is removed (or never
/// written) and the agent's change on disk is not under review in the next session.
fn regress2_type_before_first_write_keeps_review() {
    let paths = temp_paths("r2-type-first");
    let dir = project("r2-type-first", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    f.edit("a.tex", "a 5\n", "a five\n");
    let doc = f.doc("a.tex");
    type_at_start(&doc, "% typing\n");
    common::pump_for(review_persist::DEBOUNCE + Duration::from_millis(300));
    review_persist::flush(&f.state);
    let file = f.state_file();
    eprintln!("    state file exists: {}", file.exists());
    f.discard();
    assert!(
        std::fs::read_to_string(dir.join("a.tex"))
            .unwrap()
            .contains("a five")
    );
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    let restored = g.ids("a.tex").len();
    g.close();
    assert_eq!(
        restored, 1,
        "the agent's pending change on disk was not restored after Discard"
    );
}

// ----- D-3 / D-16: Alt+U falls through to an older decision -----

/// Keep Turn 1, then Reject Turn 2, then Ctrl+Z (the buffer undoes the reject). The newest
/// decision can no longer be reverted, so Alt+U must leave older decisions alone (review_ui:
/// "Only the most recent decision is ever reverted"); instead it un-keeps turn 1.
fn regress2_alt_u_never_reverts_an_older_decision() {
    let paths = temp_paths("r2-altu");
    let dir = project("r2-altu", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 3\n", "a three\n");
    review_ui::on_turn_ended(&f.state, 1);
    review_ui::on_turn_started(&f.state, 2);
    f.edit("a.tex", "a 12\n", "a twelve\n");
    review_ui::on_turn_ended(&f.state, 2);
    review_ui::decide_turn(&f.state, 1, Decision::Accept);
    review_ui::decide_turn(&f.state, 2, Decision::Reject);
    common::pump_for(Duration::from_millis(100));
    assert!(f.hunks("a.tex").is_empty());
    f.doc("a.tex").buffer().unwrap().undo();
    common::pump_for(Duration::from_millis(100));
    assert_eq!(
        f.hunks("a.tex"),
        [("a twelve\n".to_owned(), vec![2])],
        "Ctrl+Z re-opened turn 2's hunk"
    );
    WidgetExt::activate_action(&f.state.window, "win.review-undo", None).unwrap();
    common::pump_for(Duration::from_millis(200));
    let after = f.hunks("a.tex");
    f.close();
    assert!(
        !after.iter().any(|(t, _)| t.contains("a three")),
        "Alt+U reverted the older Keep Turn 1: {after:?}"
    );
}

// ----- D-4 / D-14 / D-23: the debounced snapshot runs on the main thread -----

/// While the user types in a reviewed 60k-line document (dirty, so the saved entry cannot
/// change), every debounced save (500 ms after each typing pause) rebuilds and serializes the
/// whole state on the main thread and hashes it, only to find it unchanged: a main-thread stall
/// beyond the 8 ms keystroke-to-frame budget (ARCHITECTURE §7) when typing resumes.
fn regress2_persist_snapshot_does_not_stall() {
    let large = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let paths = temp_paths("r2-stall");
    let dir = project("r2-stall", &[("large.tex", &large)]);
    let f = window(&dir, &paths);
    f.open("large.tex");
    f.wait_restored();
    let line = "\\section{Section 1250}\\label{sec:1250}";
    assert_eq!(large.matches(line).count(), 1);
    f.edit("large.tex", line, &format!("{line} % agent"));
    let file = f.state_file();
    common::pump_until(TIMEOUT, || {
        read_state(&file).is_some_and(|s| s.reviews.len() == 1)
    });
    // The user types: the document is dirty from now on.
    let doc = f.doc("large.tex");
    type_at_start(&doc, "% typed\n");
    common::pump_for(review_persist::DEBOUNCE + Duration::from_secs(1));
    // The debounced save (`save(false)`) and `flush` (`save(true)`) run the same snapshot,
    // serialization and hash on the main thread before finding the state unchanged; time it.
    let mut times: Vec<Duration> = (0..3)
        .map(|_| {
            let started = Instant::now();
            review_persist::flush(&f.state);
            started.elapsed()
        })
        .collect();
    times.sort();
    let saved = std::fs::metadata(&file).map_or(0, |m| m.len());
    f.discard();
    eprintln!("    unchanged snapshot on the main thread: {times:?} ({saved} bytes of state)");
    assert!(
        times[0] < Duration::from_millis(8),
        "building, serializing and hashing an unchanged state blocked the main thread for {:?}",
        times[0]
    );
}

// ----- D-6 / D-17: a non-UTF-8 path writes an empty state file -----

/// A project in a directory whose name is not UTF-8 (a Latin-1 name from an old archive): the
/// root does not serialize, `unwrap_or_default` turns that into an empty file, and nothing is
/// restored in the next session.
#[cfg(unix)]
fn regress2_non_utf8_root_keeps_reviews() {
    use std::os::unix::ffi::OsStrExt as _;
    let paths = temp_paths("r2-latin1");
    let dir = common::temp_dir("r2-latin1").join(std::ffi::OsStr::from_bytes(b"\xdcbungen"));
    std::fs::create_dir_all(&dir).unwrap();
    write_files(&dir, &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    f.edit("a.tex", "a 5\n", "a five\n");
    let file = f.state_file();
    common::pump_until(TIMEOUT, || file.exists());
    common::pump_for(Duration::from_millis(100));
    f.close();
    let size = std::fs::metadata(&file).map_or(0, |m| m.len());
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    let restored = g.ids("a.tex").len();
    g.close();
    assert!(size > 0, "the review state was written as an empty file");
    assert_eq!(restored, 1, "the pending change was not restored");
}

// ----- D-7: rows of a running turn decide its hunks -----

/// While turn 1 runs, its group's Keep Turn / Reject Turn are insensitive and `decide_turn`
/// refuses; the group's file rows ("Reject this turn's changes in this file") still reject the
/// running turn's hunks.
fn regress2_running_turn_rows_do_not_decide() {
    let paths = temp_paths("r2-running");
    let dir = project("r2-running", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    let agent = f.state.agent_controller().unwrap().clone();
    review_ui::on_turn_started(&f.state, 1);
    agent.set_state(AgentState::Running { turn: 1 });
    f.edit("a.tex", "a 2\n", "a two\n");
    let reject: Vec<gtk::Button> = panel_widgets(&f)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter(|b| b.tooltip_text().as_deref() == Some("Reject this turn's changes in this file"))
        .collect();
    assert_eq!(reject.len(), 1, "the running turn's row");
    let sensitive = reject[0].is_sensitive();
    if sensitive {
        reject[0].emit_clicked();
        common::pump_for(Duration::from_millis(100));
    }
    let left = f.ids("a.tex").len();
    review_ui::on_turn_ended(&f.state, 1);
    agent.set_state(AgentState::Stopped);
    f.close();
    assert_eq!(
        left, 1,
        "the row button (sensitive: {sensitive}) rejected the running turn's change"
    );
}

// ----- D-12: a restore from History counts as the last agent turn -----

/// Turn 1 edits line 3 and ends. Later the user restores an older version of a.tex from History
/// (its line 15 differs): the restored lines are attributed to turn 1, so "Undo Last Turn",
/// meant to drop Claude's change, also reverts the user's own restore.
fn regress2_history_restore_is_not_the_last_turn() {
    let paths = temp_paths("r2-history");
    let start = lines("a", 20).replace("a 15\n", "old 15\n");
    let dir = project("r2-history", &[("a.tex", &start)]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 3\n", "a three\n");
    review_ui::on_turn_ended(&f.state, 1);
    let doc = f.doc("a.tex");
    let old = doc.text();
    history::record(&f.state, &f.path("a.tex"), &old, HistoryReason::Save);
    block_on(history::flush(&f.state));
    // The user rewrites line 15 and saves.
    let at = old.find("old 15").unwrap();
    let buffer = doc.buffer().unwrap();
    let (mut s, mut e) = (
        doc.iter_at_byte(at).unwrap(),
        doc.iter_at_byte(at + 3).unwrap(),
    );
    buffer.delete(&mut s, &mut e);
    let mut s = doc.iter_at_byte(at).unwrap();
    buffer.insert(&mut s, "a");
    block_on(doc.save()).unwrap();
    common::pump_for(Duration::from_millis(100));
    assert!(f.disk("a.tex").contains("a 15\n"));
    // Restore from History… the version with "old 15".
    let entries = block_on(history::entries(&f.state, &f.path("a.tex")));
    let entry = entries
        .iter()
        .find(|e| block_on(history::load(&f.state, e)).is_ok_and(|t| t == old))
        .expect("the recorded entry");
    block_on(history::restore(&f.state, &doc, entry)).unwrap();
    assert!(doc.text().contains("old 15\n"));
    let turns = f.hunks("a.tex");
    eprintln!("    hunks after the restore: {turns:?}");
    review_ui::undo_last_turn(&f.state);
    common::pump_for(Duration::from_millis(100));
    let text = doc.text();
    f.close();
    assert!(
        text.contains("old 15\n"),
        "Undo Last Turn reverted the user's restore from History (hunks were {turns:?})"
    );
}

// ----- D-13: the review feedback block has no size cap -----

/// A replace_all over 600 separate lines, then Reject All: the `<review_feedback>` block sent
/// with the next prompt has one line per hunk, each quoting both texts, with no cap on the total
/// (76 KB here with short lines; with real 100-character lines, hundreds of KB of tokens).
fn regress2_review_feedback_is_bounded() {
    let paths = temp_paths("r2-feedback");
    let text: String = (0..600)
        .map(|i| format!("Line {i} uses \\R here.\nPlain line {i}.\n"))
        .collect();
    let dir = project("r2-feedback", &[("a.tex", &text)]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.edit_with("a.tex", "\\R", "\\mathbb{R}", true);
    assert_eq!(f.ids("a.tex").len(), 600);
    let _ = review_ui::drain_feedback(&f.state.workspace);
    review_ui::decide_document(&f.state, &f.doc("a.tex"), Decision::Reject);
    common::pump_for(Duration::from_millis(50));
    let feedback = review_ui::drain_feedback(&f.state.workspace).unwrap_or_default();
    f.close();
    eprintln!(
        "    feedback: {} bytes, {} lines",
        feedback.len(),
        feedback.lines().count()
    );
    assert!(
        feedback.len() <= 32 * 1024,
        "one Reject All queued a {}-byte <review_feedback> block for the next prompt",
        feedback.len()
    );
}

// ----- D-15: the Changes panel is rebuilt whenever a hunk id changes -----

/// Typing inside a.tex's hunk changes its id (fnv of its texts); the panel model holds the ids,
/// so every group and row is destroyed and rebuilt on each recompute, b.tex's row included.
fn regress2_panel_keeps_rows_while_typing_in_a_hunk() {
    let paths = temp_paths("r2-panel");
    let dir = project(
        "r2-panel",
        &[("a.tex", &lines("a", 20)), ("b.tex", &lines("b", 20))],
    );
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 3\n", "a three\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    let before = panel_row(&f, "b.tex");
    let id = f.ids("a.tex");
    let doc = f.doc("a.tex");
    let at = doc.text().find("a three").unwrap() + 3;
    type_at(&doc, at, "e");
    review_ui::recompute(&doc);
    assert_ne!(f.ids("a.tex"), id, "typing in the hunk changed its id");
    let after = panel_row(&f, "b.tex");
    f.close();
    assert!(
        before == after,
        "the Changes panel rebuilt b.tex's row (and every other) for a keystroke in a.tex"
    );
}

// ----- D-18: flush skips a write that is still queued -----

/// The debounced write goes to gio's blocking pool; when the pool is busy it is still queued
/// when the window closes. `flush` sees the same hash and returns without writing, so the
/// process can exit with the previous state on disk.
fn regress2_flush_writes_while_a_write_is_queued() {
    let paths = temp_paths("r2-flush");
    let dir = project("r2-flush", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    f.edit("a.tex", "a 3\n", "a three\n");
    let file = f.state_file();
    common::pump_until(TIMEOUT, || {
        read_state(&file).is_some_and(|s| s.reviews.len() == 1)
    });
    let hunks_saved = |file: &Path| {
        read_state(file).map_or(0, |s| s.reviews.iter().map(|r| r.review.hunks.len()).sum())
    };
    assert_eq!(hunks_saved(&file), 1);
    // A second change; then the blocking pool gets busy (builds, history, tool walks…).
    f.edit("a.tex", "a 12\n", "a twelve\n");
    let busy: Vec<_> = (0..40)
        .map(|_| gio::spawn_blocking(|| std::thread::sleep(Duration::from_secs(3))))
        .collect();
    common::pump_for(review_persist::DEBOUNCE + Duration::from_millis(200));
    // The window closes now (Ctrl+Q): flush must leave the latest state on disk.
    review_persist::flush(&f.state);
    let on_disk = hunks_saved(&file);
    for handle in busy {
        let _ = block_on(handle);
    }
    f.discard();
    assert_eq!(
        on_disk, 2,
        "flush returned with the previous state on disk (its write was still queued)"
    );
}

// ----- D-20: Reject Turn moves the focus to the toast's Undo button -----

const FOCUS_CHILD: &str = "regress2_child_reject_turn_then_space";

/// Runs [`reject_turn_then_space`] as a child on a private display with real key events.
fn regress2_reject_turn_leaves_focus_off_the_toast() {
    broadway_input::run_child(FOCUS_CHILD, 11);
}

/// The child: Reject Turn is activated from the keyboard (Space on the focused button). The
/// rebuild of the Changes list destroys the focused button and the focus goes to the Undo button
/// of the toast the decision added, so the user's next Space (Vim leader) silently undoes the
/// rejection.
fn reject_turn_then_space() {
    let paths = temp_paths("r2-focus");
    let dir = project(
        "r2-focus",
        &[("a.tex", &lines("a", 20)), ("b.tex", &lines("b", 20))],
    );
    let f = window(&dir, &paths);
    let mut browser = Browser::connect_drawing();
    browser.settle = Duration::from_millis(20);
    common::pump_until(TIMEOUT, || f.state.window.is_active());
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 3\n", "a three\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    f.state.sidebar.show_page(PAGE_CHANGES);
    common::pump_for(Duration::from_millis(300));
    let reject: Vec<gtk::Button> = panel_widgets(&f)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter(|b| b.label().as_deref() == Some("Reject Turn"))
        .collect();
    assert_eq!(reject.len(), 1);
    common::pump_until(TIMEOUT, || {
        reject[0].grab_focus();
        common::pump_for(Duration::from_millis(20));
        reject[0].has_focus()
    });
    let focus_chain = || {
        let mut chain = Vec::new();
        let mut w = gtk::prelude::GtkWindowExt::focus(&f.state.window);
        while let Some(widget) = w {
            chain.push(widget.type_().name().to_owned());
            w = widget.parent();
        }
        chain
    };
    browser.key(gdk::Key::space, 0);
    common::pump_for(Duration::from_millis(800));
    let rejected = f.ids("a.tex").is_empty() && f.ids("b.tex").is_empty();
    let after_reject = focus_chain();
    eprintln!("    after Reject Turn: rejected {rejected}, focus {after_reject:?}");
    assert!(
        rejected,
        "Space on the focused Reject Turn button rejected the turn"
    );
    // The user's next key: Space (Vim leader).
    browser.key(gdk::Key::space, 0);
    common::pump_for(Duration::from_millis(600));
    let back = f.ids("a.tex").len() + f.ids("b.tex").len();
    let after_space = focus_chain();
    eprintln!("    after Space: {back} hunks back, focus {after_space:?}");
    f.discard();
    assert_eq!(
        back, 0,
        "the next Space undid the rejection (focus after Reject Turn: {after_reject:?})"
    );
}

// ----- D-22: the restore notice outlives a new turn -----

/// Two changes are restored from the last session; a new turn edits one of them (the count
/// stays 2). The banner still says "Restored 2 pending changes from your last session", though
/// the changes are no longer just the restored ones (review_ui: "The restore notice stays until
/// the changes are no longer the restored ones").
fn regress2_restore_notice_gives_way_to_a_new_turn() {
    let paths = temp_paths("r2-notice");
    let dir = project("r2-notice", &[("a.tex", &lines("a", 20))]);
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("a.tex", "a 12\n", "a twelve\n");
    review_ui::on_turn_ended(&f.state, 1);
    f.close();
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    g.refresh();
    assert_eq!(
        g.state.banner.title(),
        "Restored 2 pending changes from your last session"
    );
    review_ui::on_turn_started(&g.state, 1);
    g.edit("a.tex", "a twelve\n", "a twelve, revised\n");
    review_ui::on_turn_ended(&g.state, 1);
    common::pump_for(Duration::from_millis(100));
    g.refresh();
    let title = g.state.banner.title().to_string();
    let hunks = g.hunks("a.tex");
    g.close();
    eprintln!("    hunks {hunks:?}; banner {title:?}");
    assert!(
        !title.starts_with("Restored"),
        "the banner still announces only restored changes after a new turn: {title:?}"
    );
}
