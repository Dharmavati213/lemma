//! GTK tests of WP2.6: per-turn grouping in the Changes panel (Keep Turn / Reject Turn / Undo
//! Last Turn, counts that agree everywhere) and persistence of reviews across sessions.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_review_persist`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `b-review-turns.png` (two turns in the Changes panel) there.

mod common;

use common::Canonical as _;

use adw::prelude::*;
use lemma_agent::{EditorRequest, ToolReply};
use lemma_app::agent::AgentState;
use lemma_app::agent::history::{self, HistoryReason};
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::Editor;
use lemma_app::review_persist::{self, StateFile};
use lemma_app::review_ui::{self, Decision, GroupKind};
use lemma_app::testmode::{self, review::reply_text};
use lemma_app::window::{PAGE_CHANGES, WindowState};
use lemma_app::workspace::Workspace;
use lemma_config::{Config, Paths};
use lemma_core::{Origin, ProjectRoot, TextEdit, fnv1a64};
use lemma_review::HunkId;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    // Rejecting a file the agent created moves it to the trash: keep that out of the user's.
    let data = common::target_tmpdir().join(format!("lemma-persist-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    common::run(&[
        ("turn_groups_and_counts_agree", groups_and_counts),
        ("keep_turn_and_reject_turn", keep_and_reject_turn),
        (
            "reject_turn_takes_back_part_of_a_hunk",
            reject_turn_takes_back_part_of_a_hunk,
        ),
        ("rejected_new_file_comes_back", rejected_new_file_comes_back),
        (
            "a_rejected_new_file_without_a_trash_is_deleted_and_comes_back",
            rejected_new_file_without_a_trash,
        ),
        (
            "a_trashed_file_comes_back_only_for_its_own_decision",
            trashed_file_of_its_own_decision,
        ),
        ("banner_names_the_source", banner_names_the_source),
        ("undo_last_turn_and_alt_u", undo_last_turn),
        ("running_turn_is_not_decided", running_turn),
        ("turn_ex_commands", turn_ex_commands),
        ("close_and_reopen_restores_the_hunks", close_and_reopen),
        ("mismatched_disk_hash_restores_nothing", mismatched_hash),
        (
            "changed_outside_between_sessions",
            changed_outside_between_sessions,
        ),
        (
            "changed_outside_then_edited_first",
            changed_outside_then_edited_first,
        ),
        (
            "changed_outside_then_edited_during_the_history_lookup",
            changed_outside_then_edited_during_the_lookup,
        ),
        ("unsaved_typing_keeps_the_saved_entry", unsaved_typing),
        (
            "restored_turns_come_before_this_session",
            turns_after_restore,
        ),
        ("writes_are_debounced_and_cleared", debounced_writes),
        ("writes_while_typing_goes_on", writes_while_typing_goes_on),
        ("bare_list_files_are_read", bare_list_file),
        ("persistence_is_off_for_scripted_runs", scripted_runs),
        ("snapshot_two_turns", snapshot_two_turns),
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

/// A project directory with `files`.
fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = common::temp_dir(name);
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// A window on `dir` that persists into `paths` (an interactive window: `scripted` false).
fn window(dir: &Path, paths: &Paths) -> Fixture {
    window_with(dir, Some(paths.clone()), false)
}

fn window_with(dir: &Path, paths: Option<Paths>, scripted: bool) -> Fixture {
    window_then(dir, paths, scripted, |_| {})
}

/// [`window_with`], running `first` on the new window before the main loop runs (before its
/// saved reviews have been read).
fn window_then(
    dir: &Path,
    paths: Option<Paths>,
    scripted: bool,
    first: impl FnOnce(&Rc<WindowState>),
) -> Fixture {
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
    let state = WindowState::new(&common::application(), Rc::new(config), paths, ws, scripted);
    first(&state);
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

    fn has_review(&self, file: &str) -> bool {
        self.state
            .workspace
            .get(&self.path(file))
            .is_some_and(|d| d.review.borrow().is_some() && review_ui::hunk_count(&d) > 0)
    }

    fn ids(&self, file: &str) -> Vec<HunkId> {
        self.doc(file)
            .review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.id).collect())
            .unwrap_or_default()
    }

    fn turns(&self, file: &str) -> Vec<Vec<u32>> {
        self.doc(file)
            .review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.turns.clone()).collect())
            .unwrap_or_default()
    }

    fn agent(&self, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, 1, req))
    }

    fn edit(&self, file: &str, old: &str, new: &str) {
        let reply = self.agent(EditorRequest::Edit {
            path: file.into(),
            old_string: old.into(),
            new_string: new.into(),
            replace_all: false,
        });
        assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
    }

    fn write(&self, file: &str, content: &str) {
        let reply = self.agent(EditorRequest::Write {
            path: file.into(),
            content: content.into(),
        });
        assert!(!reply.is_error, "write failed: {}", reply_text(&reply));
    }

    fn disk(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    /// Activates `win.<name>` (with a `u32` parameter) and lets idle work run.
    fn action(&self, name: &str, turn: Option<u32>) {
        let param = turn.map(|t| t.to_variant());
        WidgetExt::activate_action(&self.state.window, &format!("win.{name}"), param.as_ref())
            .unwrap_or_else(|e| panic!("win.{name}: {e}"));
        common::pump_for(Duration::from_millis(30));
    }

    fn refresh(&self) {
        self.state.review.get().unwrap().refresh();
    }

    fn panel_groups(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.refresh();
        let controller = self.state.review.get().unwrap();
        controller.changes_panel().unwrap().groups()
    }

    /// The window's state file, once written.
    fn state_file(&self) -> PathBuf {
        review_persist::state_file(&self.state).expect("persistence is on")
    }

    fn wait_restored(&self) {
        common::pump_until(TIMEOUT, || review_persist::is_restored(&self.state));
        common::pump_for(Duration::from_millis(30));
    }

    fn close(self) {
        self.state.window.close();
        common::pump_for(Duration::from_millis(30));
    }
}

/// The parsed state file at `path`.
fn read_state(path: &Path) -> StateFile {
    review_persist::parse(&std::fs::read(path).expect("the state file")).expect("parses")
}

/// Waits until the state file at `path` satisfies `ok`.
fn wait_state(path: &Path, ok: impl Fn(Option<&StateFile>) -> bool) {
    common::pump_until(TIMEOUT, || {
        let state = std::fs::read(path)
            .ok()
            .and_then(|b| review_persist::parse(&b).ok());
        ok(state.as_ref())
    });
}

/// Two turns: turn 1 changes a.tex line 2 and b.tex line 7 (b.tex is not open); turn 2 changes
/// a.tex line 12 and a.tex line 2 again (that hunk is mixed).
fn two_turns(f: &Fixture) {
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    review_ui::on_turn_started(&f.state, 2);
    f.edit("a.tex", "a 12\n", "a twelve\n");
    f.edit("a.tex", "a two\n", "a two!\n");
    review_ui::on_turn_ended(&f.state, 2);
}

fn two_file_project(name: &str) -> PathBuf {
    project(
        name,
        &[("a.tex", &lines("a", 20)), ("b.tex", &lines("b", 20))],
    )
}

// ----- grouping and counts -----

fn groups_and_counts() {
    let paths = temp_paths("groups");
    let f = window(&two_file_project("groups"), &paths);
    f.open("a.tex");
    two_turns(&f);
    assert_eq!(f.turns("a.tex"), [vec![1, 2], vec![2]]);
    assert_eq!(f.turns("b.tex"), [vec![1]]);
    let groups = f.panel_groups();
    assert_eq!(
        groups,
        vec![
            (
                "Turn 2 · 1 change in 1 file".to_owned(),
                vec![("a.tex".to_owned(), "1 change · +1 −1".to_owned())]
            ),
            (
                "Turn 1 · 1 change in 1 file".to_owned(),
                vec![("b.tex".to_owned(), "1 change · +1 −1".to_owned())]
            ),
            (
                "Several turns · 1 change in 1 file".to_owned(),
                vec![("a.tex".to_owned(), "1 change · +1 −1".to_owned())]
            ),
        ]
    );
    // One source of truth: banner, header button, summary and panel agree.
    let summary = review_ui::review_summary(&f.state);
    assert_eq!((summary.files, summary.hunks), (2, 3));
    assert_eq!(review_ui::summary(&f.state.workspace), (2, 3));
    assert_eq!(f.state.banner.title(), "Claude changed 2 files · 3 hunks");
    let controller = f.state.review.get().unwrap();
    let button = controller.summary_button();
    assert!(button.is_visible());
    assert!(
        button
            .tooltip_text()
            .is_some_and(|t| t.starts_with("Claude changed 2 files · 3 hunks")),
        "{:?}",
        button.tooltip_text()
    );
    let in_groups: usize = summary.groups.iter().map(|g| g.hunks).sum();
    assert_eq!(in_groups, summary.hunks);
    let panel = controller.changes_panel().unwrap();
    assert_eq!(panel.rows().len(), 3);
    assert!(panel.undo_turn_button().is_visible());
    assert!(
        panel
            .undo_turn_button()
            .tooltip_text()
            .is_some_and(|t| t.contains("Turn 2")),
        "{:?}",
        panel.undo_turn_button().tooltip_text()
    );
    assert_eq!(summary.latest_turn, Some(2));
    assert_eq!(
        summary.groups.iter().map(|g| g.kind).collect::<Vec<_>>(),
        [GroupKind::Turn(2), GroupKind::Turn(1), GroupKind::Mixed]
    );
    // Without turns the panel is the plain file list (M1).
    let plain = window_with(&two_file_project("groups-plain"), None, true);
    plain.open("a.tex");
    plain.edit("a.tex", "a 3\n", "a three\n");
    plain.refresh();
    let plain_panel = plain.state.review.get().unwrap().changes_panel().unwrap();
    assert!(plain_panel.groups().is_empty());
    assert_eq!(
        plain_panel.rows(),
        [("a.tex".to_owned(), "1 change · +1 −1".to_owned())]
    );
    assert!(!plain_panel.undo_turn_button().is_visible());
    plain.state.window.destroy();
    f.close();
}

fn keep_and_reject_turn() {
    let paths = temp_paths("keep-reject");
    let f = window(&two_file_project("keep-reject"), &paths);
    f.open("a.tex");
    two_turns(&f);
    let b_disk = f.disk("b.tex");
    // Keep turn 1: b.tex's change is accepted, nothing is written; Alt+U brings it back.
    let b_ids = f.ids("b.tex");
    f.action("review-keep-turn", Some(1));
    assert!(!f.has_review("b.tex"));
    assert_eq!(f.disk("b.tex"), b_disk);
    assert_eq!(f.turns("a.tex"), [vec![1, 2], vec![2]]);
    f.action("review-undo", None);
    assert_eq!(f.ids("b.tex"), b_ids);
    assert_eq!(f.turns("b.tex"), [vec![1]]);
    f.action("review-keep-turn", Some(1));
    assert!(!f.has_review("b.tex"));
    // Reject turn 2: line 12 comes back in the buffer and on disk; the mixed hunk stays.
    let mixed = f.ids("a.tex")[0];
    f.action("review-reject-turn", Some(2));
    common::pump_until(TIMEOUT, || f.disk("a.tex").contains("\na 12\n"));
    assert!(f.doc("a.tex").text().contains("\na 12\n"));
    assert_eq!(f.ids("a.tex"), [mixed]);
    assert!(
        f.disk("a.tex").contains("\na two!\n"),
        "the mixed hunk is kept"
    );
    assert_eq!(
        f.panel_groups(),
        vec![(
            "Several turns · 1 change in 1 file".to_owned(),
            vec![("a.tex".to_owned(), "1 change · +1 −1".to_owned())]
        )]
    );
    // Rejecting a turn with only mixed hunks changes nothing.
    let outcome = review_ui::decide_turn(&f.state, 2, Decision::Reject);
    assert_eq!((outcome.decided, outcome.mixed), (0, 1));
    assert_eq!(f.ids("a.tex"), [mixed]);
    // A group row decides only its own hunks: reject the mixed one from its row.
    let doc = f.doc("a.tex");
    review_ui::decide_hunks(&f.state, &doc, &[mixed], Decision::Reject);
    common::pump_until(TIMEOUT, || f.disk("a.tex") == lines("a", 20));
    f.close();
}

/// Reject Turn on a hunk that also holds a change of no turn (another program changed the next
/// line, and the open document reloaded it): only the turn's edit is taken back. The toast said
/// the turn had no pending changes, without an Undo, and Alt+U did not bring the edit back (the
/// decision was not recorded).
fn reject_turn_takes_back_part_of_a_hunk() {
    let paths = temp_paths("partial");
    let f = window(&two_file_project("partial"), &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 5\n", "a five\n");
    review_ui::on_turn_ended(&f.state, 1);
    let doc = f.doc("a.tex");
    let at = doc.text().find("a 6\n").unwrap();
    doc.apply_edits(
        &[TextEdit {
            range: at..at + 3,
            text: "a SIX".into(),
        }],
        Origin::Reload,
    )
    .unwrap();
    review_ui::recompute(&doc);
    assert_eq!(f.turns("a.tex"), [vec![1]], "one hunk, lines 5–6");
    let outcome = review_ui::decide_turn(&f.state, 1, Decision::Reject);
    let reloaded = lines("a", 20).replace("a 6\n", "a SIX\n");
    common::pump_until(TIMEOUT, || f.disk("a.tex") == reloaded);
    assert_eq!(doc.text(), reloaded);
    assert_eq!(f.turns("a.tex"), [Vec::<u32>::new()], "the reload is left");
    assert_eq!((outcome.decided, outcome.partly, outcome.files), (0, 1, 1));
    let toasts = toast_titles(&f.state);
    assert!(
        toasts.iter().any(|t| t
            == "Rejected turn 1's part of 1 change in 1 file (the other edits there are left \
                to review)"),
        "{toasts:?}"
    );
    assert!(
        toasts.iter().any(|t| t == "_Undo" || t == "Undo"),
        "{toasts:?}"
    );
    // One Alt+U brings the turn's edit back.
    f.action("review-undo", None);
    assert_eq!(
        doc.text(),
        lines("a", 20).replace("a 5\na 6\n", "a five\na SIX\n")
    );
    assert_eq!(f.turns("a.tex"), [vec![1]]);
    f.close();
}

/// A file the agent created and the user typed in, rejected whole: it goes to the trash, and the
/// toast's Undo (Alt+U) brings it back with its tab, the agent's text and the user's, under
/// review as it was. That Undo did nothing ("no longer under review"): the file's document and
/// review were gone. The text before the reject is in the local history too (the typing was
/// never saved, and the trash has the file as it was last saved).
fn rejected_new_file_comes_back() {
    common::require_private_trash();
    let paths = temp_paths("untrash");
    let f = window(&two_file_project("untrash"), &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.write("notes/new.tex", "fresh\ntext\n");
    review_ui::on_turn_ended(&f.state, 1);
    let editor = f.open("notes/new.tex");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.iter_at_offset(5), " mine");
    let typed = "fresh mine\ntext\n";
    let doc = f.doc("notes/new.tex");
    review_ui::recompute_if_stale(&doc);
    let ids = f.ids("notes/new.tex");
    let touched = |doc: &DocRef| {
        doc.review
            .borrow()
            .as_ref()
            .is_some_and(|r| r.hunks().iter().all(|h| h.user_touched))
    };
    assert_eq!(ids.len(), 1);
    assert!(touched(&doc));
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    let path = f.path("notes/new.tex");
    common::pump_until(TIMEOUT, || !path.exists());
    assert!(f.state.workspace.get(&path).is_none());
    wait_toast(
        &f.state,
        "Rejected new.tex: the new file went to the trash with your edits in it",
    );
    block_on(history::flush(&f.state));
    let entries = block_on(history::entries(&f.state, &path));
    assert_eq!(
        entries.first().map(|e| e.reason),
        Some(HistoryReason::Reject),
        "{entries:?}"
    );
    assert_eq!(
        block_on(history::load(&f.state, &entries[0])).as_deref(),
        Ok(typed)
    );
    // The toast's Undo.
    f.action("review-undo", None);
    common::pump_until(TIMEOUT, || {
        f.state
            .workspace
            .get(&path)
            .is_some_and(|d| review_ui::hunk_count(&d) > 0)
    });
    let doc = f.doc("notes/new.tex");
    assert_eq!(doc.text(), typed);
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).is_ok_and(|t| t == typed)
    });
    assert_eq!(f.ids("notes/new.tex"), ids, "the same change");
    assert!(touched(&doc), "with the user's edits in it");
    assert_eq!(f.turns("notes/new.tex"), [vec![1]]);
    assert!(f.state.editor_for(&doc).is_some(), "its tab is back");
    // Still a file the agent created: rejecting it again trashes it again.
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !path.exists());
    f.close();
}

/// A project on a file system without a trash (a container's volume, some network mounts;
/// here the memory file system of `/dev/shm`, which GIO never trashes on). Rejecting a file the
/// agent created deletes it there: it stayed on disk with the agent's text in it, no longer
/// under review, and a toast said that it could not be moved to the trash. The toast says what
/// happened, the text is in the local history, and Alt+U brings the file back for review.
fn rejected_new_file_without_a_trash() {
    let shm = Path::new("/dev/shm");
    let dir = shm.join(format!("lemma-app-{}-no-trash", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if !cfg!(target_os = "linux") || std::fs::create_dir_all(&dir).is_err() {
        println!("    SKIP: no /dev/shm to make a project in");
        return;
    }
    let probe = dir.join("probe");
    std::fs::write(&probe, "x").unwrap();
    if gio::File::for_path(&probe)
        .trash(None::<&gio::Cancellable>)
        .is_ok()
    {
        println!("    SKIP: /dev/shm has a trash here");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    std::fs::remove_file(&probe).unwrap();
    std::fs::write(dir.join("a.tex"), lines("a", 20)).unwrap();
    let paths = temp_paths("no-trash");
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.write("notes/new.tex", "fresh\ntext\n");
    review_ui::on_turn_ended(&f.state, 1);
    let path = f.path("notes/new.tex");
    let doc = f.doc("notes/new.tex");
    let ids = f.ids("notes/new.tex");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !path.exists());
    assert!(f.state.workspace.get(&path).is_none());
    wait_toast(&f.state, "new.tex was deleted: this folder has no trash");
    assert!(
        !toast_titles(&f.state)
            .iter()
            .any(|t| t.contains("Could not move")),
        "{:?}",
        toast_titles(&f.state)
    );
    block_on(history::flush(&f.state));
    let entries = block_on(history::entries(&f.state, &path));
    assert_eq!(
        block_on(history::load(&f.state, &entries[0])).as_deref(),
        Ok("fresh\ntext\n"),
        "the rejected text is in the local history"
    );
    f.action("review-undo", None);
    common::pump_until(TIMEOUT, || {
        f.state
            .workspace
            .get(&path)
            .is_some_and(|d| review_ui::hunk_count(&d) > 0)
    });
    assert_eq!(f.doc("notes/new.tex").text(), "fresh\ntext\n");
    assert_eq!(f.ids("notes/new.tex"), ids, "the same change");
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).is_ok_and(|t| t == "fresh\ntext\n")
    });
    f.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The file the agent created, rejected (to the trash), created again and kept, its tab closed:
/// Alt+U of the keep says the file left the review. It took the trashed first version for the
/// keep's, and said "exists again" (with the file deleted meanwhile, it brought the rejected
/// text back). Alt+U of the reject then comes to the trashed file; while a file of that name
/// exists it cannot come back, and once that file is gone, the next Alt+U brings it back (the
/// failed attempt used it up).
fn trashed_file_of_its_own_decision() {
    common::require_private_trash();
    let paths = temp_paths("untrash-own");
    let f = window(&two_file_project("untrash-own"), &paths);
    f.open("a.tex");
    let path = f.path("notes/new.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.write("notes/new.tex", "first\n");
    review_ui::on_turn_ended(&f.state, 1);
    review_ui::decide_document(&f.state, &f.doc("notes/new.tex"), Decision::Reject);
    common::pump_until(TIMEOUT, || {
        !path.exists() && f.state.workspace.get(&path).is_none()
    });
    review_ui::on_turn_started(&f.state, 2);
    f.write("notes/new.tex", "second\n");
    review_ui::on_turn_ended(&f.state, 2);
    let editor = f.open("notes/new.tex");
    review_ui::decide_document(&f.state, editor.document(), Decision::Accept);
    f.state.close_editor(&editor, false);
    drop(editor);
    common::pump_until(TIMEOUT, || f.state.workspace.get(&path).is_none());
    // Alt+U of the keep.
    f.action("review-undo", None);
    wait_toast(
        &f.state,
        "Cannot undo the last decision: notes/new.tex is no longer under review",
    );
    common::pump_for(Duration::from_millis(200));
    assert_eq!(f.disk("notes/new.tex"), "second\n");
    assert!(f.state.workspace.get(&path).is_none());
    // Alt+U of the reject: the file exists.
    f.action("review-undo", None);
    wait_toast(
        &f.state,
        "Cannot undo the last decision: notes/new.tex exists again",
    );
    assert_eq!(f.disk("notes/new.tex"), "second\n");
    // Once it is gone, Alt+U again.
    std::fs::remove_file(&path).unwrap();
    f.action("review-undo", None);
    common::pump_until(TIMEOUT, || {
        f.state
            .workspace
            .get(&path)
            .is_some_and(|d| review_ui::hunk_count(&d) > 0)
    });
    assert_eq!(f.doc("notes/new.tex").text(), "first\n");
    f.close();
}

/// The banner names who made what is under review: a restore from History (the banner said
/// "Claude changed …" of it), the agent, or several sources. Undo Last Turn takes back the
/// agent's turn and leaves the restore alone.
fn banner_names_the_source() {
    let paths = temp_paths("source");
    let f = window(&two_file_project("source"), &paths);
    let editor = f.open("a.tex");
    let doc = f.doc("a.tex");
    let old = doc.text();
    history::record(&f.state, &f.path("a.tex"), &old, HistoryReason::Save);
    block_on(history::flush(&f.state));
    // The user rewrites line 3 and saves; then restores the old version.
    let buffer = editor.buffer();
    let at = old.find("a 3").unwrap();
    let (mut s, mut e) = (
        doc.iter_at_byte(at).unwrap(),
        doc.iter_at_byte(at + 3).unwrap(),
    );
    buffer.delete(&mut s, &mut e);
    buffer.insert(&mut doc.iter_at_byte(at).unwrap(), "a three");
    block_on(doc.save()).unwrap();
    let entries = block_on(history::entries(&f.state, &f.path("a.tex")));
    let entry = entries
        .iter()
        .find(|e| block_on(history::load(&f.state, e)).is_ok_and(|t| t == old))
        .expect("the recorded entry");
    block_on(history::restore(&f.state, &doc, entry)).unwrap();
    assert_eq!(doc.text(), old);
    let titles =
        |f: &Fixture| -> Vec<String> { f.panel_groups().into_iter().map(|(t, _)| t).collect() };
    assert_eq!(titles(&f), ["Restore from History · 1 change in 1 file"]);
    assert_eq!(
        f.state.banner.title(),
        "Restore from History · 1 file · 1 hunk"
    );
    // An agent turn too: two sources.
    review_ui::on_turn_started(&f.state, 1);
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    f.refresh();
    assert_eq!(
        f.state.banner.title(),
        "Changes to review · 2 files · 2 hunks"
    );
    // Undo Last Turn takes back the agent's turn, not the restore.
    review_ui::undo_last_turn(&f.state);
    common::pump_until(TIMEOUT, || f.disk("b.tex") == lines("b", 20));
    assert_eq!(doc.text(), old);
    f.refresh();
    assert_eq!(
        f.state.banner.title(),
        "Restore from History · 1 file · 1 hunk"
    );
    // The restore kept, the agent's change alone is the agent's.
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    review_ui::on_turn_started(&f.state, 2);
    f.edit("b.tex", "b 9\n", "b nine\n");
    review_ui::on_turn_ended(&f.state, 2);
    f.refresh();
    assert_eq!(f.state.banner.title(), "Claude changed 1 file · 1 hunk");
    f.close();
}

fn undo_last_turn() {
    let paths = temp_paths("undo-turn");
    let f = window(&two_file_project("undo-turn"), &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    review_ui::on_turn_started(&f.state, 2);
    f.edit("a.tex", "a 12\n", "a twelve\n");
    f.edit("b.tex", "b 17\n", "b seventeen\n");
    review_ui::on_turn_ended(&f.state, 2);
    let (a_ids, b_ids) = (f.ids("a.tex"), f.ids("b.tex"));
    // The header button rejects the latest turn in every file.
    f.refresh();
    let panel = f.state.review.get().unwrap().changes_panel().unwrap();
    panel.undo_turn_button().emit_clicked();
    common::pump_until(TIMEOUT, || {
        f.disk("a.tex").contains("\na 12\n") && f.disk("b.tex").contains("\nb 17\n")
    });
    assert_eq!(f.ids("a.tex"), [a_ids[0]]);
    assert_eq!(f.ids("b.tex"), [b_ids[0]]);
    assert_eq!(f.turns("a.tex"), [vec![1]]);
    // One Alt+U brings turn 2 back in both files, with the same ids and turns.
    f.action("review-undo", None);
    common::pump_until(TIMEOUT, || f.disk("b.tex").contains("\nb seventeen\n"));
    assert_eq!(f.ids("a.tex"), a_ids);
    assert_eq!(f.ids("b.tex"), b_ids);
    assert_eq!(f.turns("b.tex"), [vec![1], vec![2]]);
    // Undo Last Turn twice: turn 2, then turn 1.
    f.action("review-undo-turn", None);
    f.action("review-undo-turn", None);
    common::pump_until(TIMEOUT, || {
        f.disk("a.tex") == lines("a", 20) && f.disk("b.tex") == lines("b", 20)
    });
    assert!(!f.has_review("a.tex") && !f.has_review("b.tex"));
    f.refresh();
    assert!(!panel.undo_turn_button().is_visible());
    f.close();
}

fn running_turn() {
    let paths = temp_paths("running");
    let f = window(&two_file_project("running"), &paths);
    f.open("a.tex");
    let agent = f.state.agent_controller().unwrap().clone();
    review_ui::on_turn_started(&f.state, 1);
    agent.set_state(AgentState::Running { turn: 1 });
    f.edit("a.tex", "a 2\n", "a two\n");
    let groups = f.panel_groups();
    assert_eq!(groups[0].0, "Turn 1 · 1 change in 1 file · running");
    let panel = f.state.review.get().unwrap().changes_panel().unwrap();
    assert!(!panel.undo_turn_button().is_sensitive());
    let outcome = review_ui::decide_turn(&f.state, 1, Decision::Reject);
    assert_eq!(outcome.decided, 0);
    assert!(f.has_review("a.tex"));
    // Once it ends it can be rejected.
    review_ui::on_turn_ended(&f.state, 1);
    agent.set_state(AgentState::Idle);
    assert_eq!(f.panel_groups()[0].0, "Turn 1 · 1 change in 1 file");
    assert!(panel.undo_turn_button().is_sensitive());
    let outcome = review_ui::decide_turn(&f.state, 1, Decision::Reject);
    assert_eq!((outcome.decided, outcome.files), (1, 1));
    agent.set_state(AgentState::Stopped);
    f.close();
}

fn turn_ex_commands() {
    let paths = temp_paths("ex-turns");
    let f = window(&two_file_project("ex-turns"), &paths);
    let editor = f.open("a.tex");
    two_turns(&f);
    assert!(review_ui::ex_command(&f.state, &editor, ":KeepTurn 1"));
    common::pump_for(Duration::from_millis(50));
    assert!(!f.has_review("b.tex"));
    assert!(review_ui::ex_command(&f.state, &editor, "RejectTurn"));
    common::pump_until(TIMEOUT, || f.disk("a.tex").contains("\na 12\n"));
    assert!(review_ui::ex_command(&f.state, &editor, ":RejectTurn x"));
    common::pump_for(Duration::from_millis(30));
    assert!(review_ui::ex_command(&f.state, &editor, ":UndoTurn"));
    common::pump_for(Duration::from_millis(50));
    // The latest turn with pending changes is the mixed hunk's turn 2: it has nothing of its own.
    assert_eq!(f.turns("a.tex"), [vec![1, 2]]);
    assert!(!review_ui::ex_command(
        &f.state,
        &editor,
        ":NotATurnCommand"
    ));
    f.close();
}

// ----- persistence -----

fn close_and_reopen() {
    let paths = temp_paths("reopen");
    let dir = two_file_project("reopen");
    let f = window(&dir, &paths);
    f.open("a.tex");
    two_turns(&f);
    f.write("notes/new.tex", "fresh\ntext\n");
    // The user edits inside turn 2's hunk on line 12, then saves.
    let doc = f.doc("a.tex");
    let at = doc.text().find("a twelve").unwrap() + 1;
    let buffer = doc.buffer().unwrap();
    let mut iter = doc.iter_at_byte(at).unwrap();
    buffer.insert(&mut iter, "!");
    block_on(doc.save()).unwrap();
    common::pump_for(Duration::from_millis(80));
    type Snapshot<'a> = (&'a str, Vec<HunkId>, Vec<Vec<u32>>);
    let before: Vec<Snapshot> = ["a.tex", "b.tex", "notes/new.tex"]
        .into_iter()
        .map(|file| (file, f.ids(file), f.turns(file)))
        .collect();
    let touched: Vec<bool> = doc
        .review
        .borrow()
        .as_ref()
        .unwrap()
        .hunks()
        .iter()
        .map(|h| h.user_touched)
        .collect();
    assert_eq!(touched, [false, true]);
    let file = f.state_file();
    f.close();
    // Closing wrote everything.
    let saved = read_state(&file);
    assert_eq!(saved.reviews.len(), 3);
    assert_eq!(saved.root, dir.canonical().unwrap());
    let saved_turns: Vec<u32> = saved.turns.iter().map(|t| t.turn).collect();
    assert_eq!(saved_turns, [1, 2]);
    assert!(
        saved
            .reviews
            .iter()
            .any(|r| r.new_file && r.review.path == Path::new("notes/new.tex"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the review state is private");
    }

    // A new session restores the hunks with the same ids, turns and flags.
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    for (file, ids, turns) in &before {
        assert_eq!(&g.ids(file), ids, "{file}");
        assert_eq!(&g.turns(file), turns, "{file}");
    }
    let touched: Vec<bool> = g
        .doc("a.tex")
        .review
        .borrow()
        .as_ref()
        .unwrap()
        .hunks()
        .iter()
        .map(|h| h.user_touched)
        .collect();
    assert_eq!(touched, [false, true]);
    assert!(review_ui::is_new_file(&g.doc("notes/new.tex")));
    g.refresh();
    assert!(g.state.banner.is_revealed());
    assert_eq!(
        g.state.banner.title(),
        "Restored 4 pending changes from your last session"
    );
    // Earlier turns are labelled with their date.
    let groups = g.panel_groups();
    let titles: Vec<&str> = groups.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(titles.len(), 3, "{titles:?}");
    assert!(titles[0].starts_with("Turn 2 · ") && titles[0].ends_with("2 changes in 2 files"));
    assert!(titles[1].starts_with("Turn 1 · ") && titles[1].ends_with("1 change in 1 file"));
    assert_eq!(titles[2], "Several turns · 1 change in 1 file");
    // The restored new file is still trashed when it is rejected. (The rest is left out where
    // the tests have no trash of their own.)
    common::require_private_trash();
    let new = g.doc("notes/new.tex");
    review_ui::decide_document(&g.state, &new, Decision::Reject);
    common::pump_until(TIMEOUT, || !g.path("notes/new.tex").exists());
    // Once everything is decided, the state file goes away.
    review_ui::decide_everything(&g.state, Decision::Accept);
    common::pump_until(TIMEOUT, || !file.exists());
    g.close();
}

fn mismatched_hash() {
    let paths = temp_paths("mismatch");
    let dir = two_file_project("mismatch");
    let f = window(&dir, &paths);
    f.open("a.tex");
    two_turns(&f);
    let a_ids = f.ids("a.tex");
    let file = f.state_file();
    f.close();
    assert_eq!(read_state(&file).reviews.len(), 2);
    // Another program changes b.tex: its review no longer applies, and the local history that
    // has the text it was saved with is gone too.
    std::fs::write(dir.join("b.tex"), lines("B", 20)).unwrap();
    let _ = std::fs::remove_dir_all(paths.history_dir());
    let g = window(&dir, &paths);
    // Nothing is restored before a document is shown.
    common::pump_for(Duration::from_millis(200));
    assert!(!review_persist::is_restored(&g.state));
    assert!(g.state.workspace.is_empty());
    g.open("a.tex");
    g.wait_restored();
    assert_eq!(g.ids("a.tex"), a_ids);
    assert!(
        g.state.workspace.get(&g.path("b.tex")).is_none(),
        "b.tex is not under review"
    );
    assert_eq!(
        g.state.banner.title(),
        "Restored 2 pending changes from your last session"
    );
    // The user is told which file lost its pending changes, and why: what the agent changed
    // there now passes for the user's own text (it was dropped without a word).
    wait_toast(&g.state, "b.tex (it was changed outside Lemma)");
    assert!(
        toast_titles(&g.state)
            .iter()
            .any(|t| t.contains("no longer marked for review")),
        "{:?}",
        toast_titles(&g.state)
    );
    // The file is rewritten without b.tex.
    wait_state(&file, |s| s.is_some_and(|s| s.reviews.len() == 1));
    assert_eq!(read_state(&file).reviews[0].review.path, Path::new("a.tex"));
    g.close();
}

/// Another program changed one reviewed file and deleted another between two sessions. The
/// text the first review was saved with is in the local history: its review comes back on it
/// and the other program's change is a change to review too (as it is when Lemma sees it
/// happen), and a toast says so; the deleted file's loss is told.
fn changed_outside_between_sessions() {
    let paths = temp_paths("outside");
    let dir = project(
        "outside",
        &[
            ("a.tex", &lines("a", 20)),
            ("b.tex", &lines("b", 20)),
            ("c.tex", &lines("c", 20)),
        ],
    );
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    f.edit("c.tex", "c 3\n", "c three\n");
    review_ui::on_turn_ended(&f.state, 1);
    let b_ids = f.ids("b.tex");
    block_on(history::flush(&f.state));
    let file = f.state_file();
    f.close();
    assert_eq!(read_state(&file).reviews.len(), 3);
    // Another program changes b.tex further down, and deletes c.tex.
    let agent_b = std::fs::read_to_string(dir.join("b.tex")).unwrap();
    std::fs::write(
        dir.join("b.tex"),
        agent_b.replace("b 15\n", "b fifteen (outside)\n"),
    )
    .unwrap();
    std::fs::remove_file(dir.join("c.tex")).unwrap();

    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    let b = g.doc("b.tex");
    assert_eq!(
        b.text(),
        std::fs::read_to_string(dir.join("b.tex")).unwrap()
    );
    // The agent's change keeps its id and turn; the other program's is under review apart.
    assert_eq!(g.ids("b.tex").len(), 2, "{:?}", g.turns("b.tex"));
    assert_eq!(g.ids("b.tex")[0], b_ids[0]);
    assert_eq!(g.turns("b.tex"), [vec![1], vec![]]);
    // Toasts say both (one at a time: the first stays until it is dismissed).
    common::pump_until(TIMEOUT, || !toast_titles(&g.state).is_empty());
    let toasts: Vec<String> = toast_titles(&g.state)
        .into_iter()
        .chain(queued_toast_titles(&g.state))
        .collect();
    assert!(
        toasts
            .iter()
            .any(|t| t.contains("b.tex was changed outside Lemma")),
        "{toasts:?}"
    );
    assert!(
        toasts.iter().any(|t| t.contains("c.tex (it was deleted)")),
        "nothing says that c.tex lost its pending changes: {toasts:?}"
    );
    // Rejecting the other program's change takes it back, in the buffer and on disk.
    let outside = g.ids("b.tex")[1];
    review_ui::decide_hunks(&g.state, &b, &[outside], Decision::Reject);
    common::pump_until(TIMEOUT, || g.disk("b.tex").contains("\nb 15\n"));
    assert!(g.disk("b.tex").contains("\nb seven\n"));
    // The state written back has a.tex and b.tex.
    wait_state(&file, |s| {
        s.is_some_and(|s| {
            let mut names: Vec<_> = s.reviews.iter().map(|r| r.review.path.clone()).collect();
            names.sort();
            names == [PathBuf::from("a.tex"), PathBuf::from("b.tex")]
        })
    });
    g.close();
}

/// An agent edit to a file that another program changed between the sessions, before the
/// restore reached it: the new review starts from the file as it is, and a toast says the saved
/// changes are lost (they went without a word).
fn changed_outside_then_edited_first() {
    let paths = temp_paths("outside-edited");
    let dir = two_file_project("outside-edited");
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    f.close();
    std::fs::write(dir.join("b.tex"), lines("B", 20)).unwrap();
    let g = window_then(&dir, Some(paths.clone()), false, |state| {
        review_ui::on_turn_started(state, 1);
    });
    assert!(!review_persist::is_restored(&g.state));
    g.edit("b.tex", "B 15\n", "B fifteen\n");
    review_ui::on_turn_ended(&g.state, 1);
    g.wait_restored();
    assert_eq!(g.ids("b.tex").len(), 1, "only the new edit");
    wait_toast(&g.state, "b.tex (it was changed outside Lemma)");
    g.close();
}

/// The same edit, made while the restore looks the text b.tex's review was saved with up in the
/// local history (that review is then in neither of the lists an agent edit looks in): the
/// outcome is the one above, a new review from the file as it is and the same toast, as it
/// would be with that review found by the edit (its text no longer matches the file).
fn changed_outside_then_edited_during_the_lookup() {
    let paths = temp_paths("outside-lookup");
    let dir = two_file_project("outside-lookup");
    let f = window(&dir, &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit("a.tex", "a 2\n", "a two\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    review_ui::on_turn_ended(&f.state, 1);
    block_on(history::flush(&f.state));
    f.close();
    let agent_b = std::fs::read_to_string(dir.join("b.tex")).unwrap();
    std::fs::write(
        dir.join("b.tex"),
        agent_b.replace("b 15\n", "b fifteen (outside)\n"),
    )
    .unwrap();
    let g = window_then(&dir, Some(paths.clone()), false, |state| {
        review_ui::on_turn_started(state, 1);
    });
    assert!(g.state.workspace.is_empty());
    // The restore starts; once it has loaded b.tex it waits for the history's answer.
    review_persist::editor_attached(&g.state);
    let b = g.path("b.tex");
    let context = glib::MainContext::default();
    let deadline = Instant::now() + TIMEOUT;
    while g.state.workspace.get(&b).is_none() {
        assert!(Instant::now() < deadline, "b.tex was never loaded");
        assert!(!review_persist::is_restored(&g.state));
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!review_persist::is_restored(&g.state));
    let doc = g.doc("b.tex");
    assert!(doc.review.borrow().is_none());
    // The agent's edit, as `agent_edits` makes it (without the awaits of the tool).
    review_ui::start_review(&doc, false);
    doc.apply_edits(&[TextEdit::insert(0, "new\n")], Origin::Agent { call: 1 })
        .unwrap();
    review_ui::recompute(&doc);
    review_ui::on_turn_ended(&g.state, 1);
    g.wait_restored();
    assert_eq!(g.ids("b.tex").len(), 1, "only the new edit");
    wait_toast(&g.state, "b.tex (it was changed outside Lemma)");
    g.close();
}

/// The texts of the toast `state` shows.
fn toast_titles(state: &WindowState) -> Vec<String> {
    fn labels(widget: &gtk::Widget, out: &mut Vec<String>) {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            out.push(label.label().to_string());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            labels(&c, out);
            child = c.next_sibling();
        }
    }
    let mut out = Vec::new();
    let mut child = state.toasts.first_child();
    while let Some(c) = child {
        if c.type_().name() == "AdwToastWidget" {
            labels(&c, &mut out);
        }
        child = c.next_sibling();
    }
    out
}

/// The texts of the toasts waiting behind the one shown: each is shown in turn as the one before
/// is dismissed (by its close button).
fn queued_toast_titles(state: &WindowState) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..8 {
        let close = descendants(&state.toasts)
            .into_iter()
            .filter(|w| {
                std::iter::successors(w.parent(), |p| p.parent())
                    .any(|p| p.type_().name() == "AdwToastWidget")
            })
            .find_map(|w| w.downcast::<gtk::Button>().ok());
        let Some(close) = close else {
            break;
        };
        close.emit_clicked();
        common::pump_for(Duration::from_millis(400));
        out.extend(toast_titles(state));
    }
    out
}

fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![widget.as_ref().clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            out.push(c.clone());
            stack.push(c);
        }
    }
    out
}

/// Waits until the toast shown says `needle`.
fn wait_toast(state: &WindowState, needle: &str) {
    common::pump_until(TIMEOUT, || {
        toast_titles(state).iter().any(|t| t.contains(needle))
    });
    assert!(
        toast_titles(state).iter().any(|t| t.contains(needle)),
        "no toast says {needle:?}: {:?}",
        toast_titles(state)
    );
}

fn unsaved_typing() {
    let paths = temp_paths("unsaved");
    let dir = two_file_project("unsaved");
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.edit("a.tex", "a 5\n", "a five\n");
    let file = f.state_file();
    let disk_hash = fnv1a64(&[f.disk("a.tex").as_bytes()]);
    wait_state(&file, |s| {
        s.is_some_and(|s| s.reviews.len() == 1 && s.reviews[0].review.current_hash == disk_hash)
    });
    // Unsaved typing (outside and inside the hunk) does not replace the entry that matches the
    // file on disk.
    let doc = f.doc("a.tex");
    let buffer = doc.buffer().unwrap();
    buffer.insert(&mut buffer.start_iter(), "% typed\n");
    common::pump_for(review_persist::DEBOUNCE + Duration::from_millis(200));
    review_persist::flush(&f.state);
    let saved = read_state(&file);
    assert_eq!(saved.reviews[0].review.current_hash, disk_hash);
    // The window goes away without saving (as after Discard).
    f.state.window.destroy();
    common::pump_for(Duration::from_millis(30));
    let g = window(&dir, &paths);
    g.open("a.tex");
    g.wait_restored();
    assert_eq!(g.ids("a.tex").len(), 1);
    assert!(!g.doc("a.tex").text().contains("% typed"));
    g.close();
}

fn turns_after_restore() {
    let paths = temp_paths("renumber");
    let dir = two_file_project("renumber");
    let f = window(&dir, &paths);
    f.open("a.tex");
    two_turns(&f);
    f.close();
    // The next session starts a turn before its saved state was even read, and edits b.tex
    // before any document is shown (the saved reviews wait to be restored).
    let g = window_then(&dir, Some(paths.clone()), false, |state| {
        review_ui::on_turn_started(state, 1);
    });
    assert!(!review_persist::is_restored(&g.state));
    g.edit("b.tex", "b 15\n", "b fifteen\n");
    review_ui::on_turn_ended(&g.state, 1);
    g.wait_restored();
    // b.tex's saved hunk stays (the edit started from the saved review), and this session's
    // turn 1 is numbered after the restored turns 1 and 2.
    assert_eq!(g.turns("b.tex"), [vec![1], vec![3]]);
    assert_eq!(g.turns("a.tex"), [vec![1, 2], vec![2]]);
    assert_eq!(
        g.state.banner.title(),
        "Claude changed 2 files · 4 hunks",
        "the restore notice gives way to the new change"
    );
    let titles: Vec<String> = g.panel_groups().into_iter().map(|(t, _)| t).collect();
    assert_eq!(titles[0], "Turn 1 · 1 change in 1 file");
    assert!(titles[1].starts_with("Turn 2 · "), "{titles:?}");
    // Undo Last Turn rejects this session's turn.
    review_ui::undo_last_turn(&g.state);
    common::pump_until(TIMEOUT, || g.disk("b.tex").contains("\nb 15\n"));
    assert_eq!(g.turns("b.tex"), [vec![1]]);
    // The next turn of this session is review turn 4.
    review_ui::on_turn_started(&g.state, 2);
    g.edit("b.tex", "b 18\n", "b eighteen\n");
    assert_eq!(g.turns("b.tex"), [vec![1], vec![4]]);
    assert_eq!(review_ui::turn_label(&g.state, 4), "Turn 2");
    review_ui::on_turn_ended(&g.state, 2);
    g.close();
}

fn debounced_writes() {
    let paths = temp_paths("debounce");
    let dir = two_file_project("debounce");
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    let file = f.state_file();
    assert!(!file.exists());
    let started = Instant::now();
    f.edit("a.tex", "a 5\n", "a five\n");
    common::pump_for(Duration::from_millis(150));
    if started.elapsed() < review_persist::DEBOUNCE {
        assert!(!file.exists(), "written before the debounce");
    }
    wait_state(&file, |s| s.is_some_and(|s| s.reviews.len() == 1));
    assert!(started.elapsed() >= review_persist::DEBOUNCE);
    // Nothing left to review: the file is removed.
    review_ui::decide_everything(&f.state, Decision::Reject);
    common::pump_until(TIMEOUT, || !file.exists());
    f.close();
    assert!(!file.exists());
}

/// Typing in a reviewed document refreshes the review every keystroke or so: the agent's edit
/// meanwhile is written within `MAX_DELAY` all the same (each refresh put it off by another
/// debounce, so nothing was written until the typing paused, and a crash lost the hunks).
fn writes_while_typing_goes_on() {
    let paths = temp_paths("typing");
    let dir = two_file_project("typing");
    let f = window(&dir, &paths);
    let editor = f.open("a.tex");
    f.wait_restored();
    let file = f.state_file();
    f.edit("a.tex", "a 5\n", "a five\n");
    f.edit("b.tex", "b 7\n", "b seven\n");
    let started = Instant::now();
    let buffer = editor.buffer();
    let deadline = review_persist::MAX_DELAY + Duration::from_millis(1500);
    while started.elapsed() < deadline && !file.exists() {
        // A keystroke after the last line, outside the hunk (its recompute refreshes the review).
        buffer.insert(&mut buffer.end_iter(), "x");
        common::pump_for(Duration::from_millis(120));
    }
    assert!(
        file.exists(),
        "nothing written after {:?} of typing",
        started.elapsed()
    );
    let saved = read_state(&file);
    let mut files: Vec<String> = saved
        .reviews
        .iter()
        .map(|r| r.review.path.display().to_string())
        .collect();
    files.sort();
    assert_eq!(files, ["a.tex", "b.tex"]);
    f.close();
}

fn bare_list_file() {
    let paths = temp_paths("bare");
    let dir = two_file_project("bare");
    // A file as ARCHITECTURE §4.5 describes it: a list of PersistedReview.
    let disk = lines("a", 20).replace("a 3\n", "a THREE\n");
    std::fs::write(dir.join("a.tex"), &disk).unwrap();
    let root = dir.canonical().unwrap();
    let file = review_persist::file_for_root(&paths.review_dir(), &root);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let entry = serde_json::json!([{
        "path": "a.tex",
        "baseline": lines("a", 20),
        "current_hash": fnv1a64(&[disk.as_bytes()]),
        "created_unix": 0,
    }]);
    std::fs::write(&file, entry.to_string()).unwrap();
    let f = window(&dir, &paths);
    f.open("a.tex");
    f.wait_restored();
    assert_eq!(f.ids("a.tex").len(), 1);
    assert_eq!(f.turns("a.tex"), [Vec::<u32>::new()]);
    // Rewritten in the current form.
    wait_state(&file, |s| {
        s.is_some_and(|s| s.version == 1 && s.root == root)
    });
    f.close();
}

fn scripted_runs() {
    let paths = temp_paths("scripted");
    let dir = two_file_project("scripted");
    let f = window_with(&dir, Some(paths.clone()), true);
    f.open("a.tex");
    f.edit("a.tex", "a 5\n", "a five\n");
    assert!(review_persist::state_file(&f.state).is_none());
    common::pump_for(review_persist::DEBOUNCE + Duration::from_millis(200));
    f.state.window.close();
    common::pump_for(Duration::from_millis(30));
    assert!(
        !paths.review_dir().exists(),
        "a scripted run wrote the review state"
    );
}

/// `b-review-turns.png`: the Changes panel with two turns next to the hunks. The lines are
/// long enough to show beside the panel, which overlays the editor in a narrow window.
fn snapshot_two_turns() {
    const A: &str = "\\section{Sheaves}\\label{sec:sheaves}\n\
        Let $X$ be a topological space; write $\\mathrm{Open}(X)$ for its poset of open sets.\n\
        A presheaf of abelian groups on $X$ is a functor $\\mathrm{Open}(X)^{op}\\to\\mathrm{Ab}$.\n\
        \n\
        It is a sheaf when sections that agree on overlaps glue uniquely to a section.\n\
        Every presheaf has a sheafification, and the two have the same stalks at points.\n\
        \n\
        \\begin{lemma}\\label{lem:stalks}\n\
        A morphism of sheaves is an isomorphism when it is one on all stalks.\n\
        \\end{lemma}\n";
    const B: &str = "\\section{Descent}\\label{sec:descent}\n\
        Descent data for a cover $\\{U_i\\to U\\}$ are compatible objects on the pieces.\n\
        The cocycle condition holds on the triple overlaps $U_{ijk}$ of the cover.\n";
    let paths = temp_paths("shot");
    let f = window(&project("shot", &[("a.tex", A), ("b.tex", B)]), &paths);
    f.open("a.tex");
    review_ui::on_turn_started(&f.state, 1);
    f.edit(
        "a.tex",
        "for its poset of open sets.",
        "for the category of open subsets of $X$.",
    );
    f.edit(
        "b.tex",
        "are compatible objects",
        "are objects with gluing isomorphisms",
    );
    review_ui::on_turn_ended(&f.state, 1);
    review_ui::on_turn_started(&f.state, 2);
    f.edit(
        "a.tex",
        "It is a sheaf when sections that agree on overlaps glue uniquely to a section.\n",
        "It is a sheaf when compatible local sections glue uniquely to a global section.\n\
         Equivalently, the equalizer diagram of every open cover is exact.\n",
    );
    review_ui::on_turn_ended(&f.state, 2);
    f.state.sidebar.show_page(PAGE_CHANGES);
    f.refresh();
    let groups = f.panel_groups();
    assert_eq!(groups.len(), 2);
    assert!(f.state.sidebar.changes.is_mapped());
    if let Ok(dir) = std::env::var("LEMMA_TEST_SNAPSHOTS") {
        assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
        let shot = Path::new(&dir).join("b-review-turns.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &shot)).unwrap();
        println!("  saved {}", shot.display());
    }
    f.close();
}
