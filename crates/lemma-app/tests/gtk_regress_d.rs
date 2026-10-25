//! Regression tests for verified area-D defects (app shell: documents, saving, Vim, window).
//! Each test reproduces one defect and fails until it is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress_d`.
//!
//! A few defects hang or abort the process; their tests run the scenario in a child process (this
//! binary again, with `LEMMA_REGRESS_D_CHILD=<probe>`) and fail on a hang, a crash or a GLib
//! critical in the child.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_agent::{EditorRequest, ToolReply};
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::saver::SaveError;
use lemma_app::testmode::review::reply_text;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{Origin, ProjectRoot, TextEdit};
use sourceview5::prelude::*;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHILD_ENV: &str = "LEMMA_REGRESS_D_CHILD";

fn main() -> ExitCode {
    if let Ok(probe) = std::env::var(CHILD_ENV) {
        return run_child(&probe);
    }
    // Rejecting a file the agent created moves it to the trash: keep that out of the user's.
    let data = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress-d-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    common::run(&[
        // D1 / D6 / D20 / D33
        (
            "regress_qbang_discard_under_review",
            regress_qbang_discard_under_review,
        ),
        (
            "regress_dialog_discard_under_review",
            regress_dialog_discard_under_review,
        ),
        // D2 / D17
        (
            "regress_reopened_closed_doc_saves",
            regress_reopened_closed_doc_saves,
        ),
        // D3 / D9 / D22 / D38
        (
            "regress_latin1_closed_save_keeps_encoding",
            regress_latin1_closed_save_keeps_encoding,
        ),
        // D4 / D10 / D36
        (
            "regress_crlf_closed_then_opened_keeps_crlf",
            regress_crlf_closed_then_opened_keeps_crlf,
        ),
        // D5
        (
            "regress_u2029_save_writes_buffer_text",
            regress_u2029_save_writes_buffer_text,
        ),
        // D7 / D23 / D34
        (
            "regress_closed_doc_external_change_refused",
            regress_closed_doc_external_change_refused,
        ),
        (
            "regress_reopened_closed_doc_external_change_refused",
            regress_reopened_closed_doc_external_change_refused,
        ),
        // D8 / D25
        (
            "regress_mixed_line_endings_round_trip",
            regress_mixed_line_endings_round_trip,
        ),
        // D11 / D24
        (
            "regress_external_change_same_second_refused",
            regress_external_change_same_second_refused,
        ),
        // D12 / D29
        (
            "regress_typing_during_save_writes_a_real_state",
            regress_typing_during_save_writes_a_real_state,
        ),
        // D13
        (
            "regress_review_edit_in_insert_mode_is_own_undo_step",
            regress_review_edit_in_insert_mode_is_own_undo_step,
        ),
        // D14
        ("regress_colon_w_bang_saves", regress_colon_w_bang_saves),
        // D15 / D26
        (
            "regress_scripted_flag_without_deadline_keeps_prompts",
            regress_scripted_flag_without_deadline_keeps_prompts,
        ),
        // D16
        (
            "regress_vim_join_whitespace_last_line_returns",
            regress_vim_join_whitespace_last_line_returns,
        ),
        // D18
        (
            "regress_latin1_trailing_accent_opens",
            regress_latin1_trailing_accent_opens,
        ),
        // D19
        (
            "regress_wq_then_discard_does_not_abort",
            regress_wq_then_discard_does_not_abort,
        ),
        // D21
        (
            "regress_nul_closed_doc_open_keeps_text",
            regress_nul_closed_doc_open_keeps_text,
        ),
        // D27
        ("regress_colon_e_bang_reverts", regress_colon_e_bang_reverts),
        // D31 / D32
        (
            "regress_missing_dir_root_is_not_filesystem_root",
            regress_missing_dir_root_is_not_filesystem_root,
        ),
        // Saving after the fixes: what must keep working.
        ("touched_file_still_saves", touched_file_still_saves),
        ("utf8_bom_round_trips", utf8_bom_round_trips),
        (
            "latin1_save_refuses_unencodable_text",
            latin1_save_refuses_unencodable_text,
        ),
        (
            "colon_w_bang_overwrites_external_change",
            colon_w_bang_overwrites_external_change,
        ),
        (
            "colon_e_reloads_only_when_clean",
            colon_e_reloads_only_when_clean,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ----- fixture -----

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// A window on a fresh directory holding `files`. `vim` turns Vim mode on; `scripted` suppresses
/// the unsaved-changes dialogs (as the review tests do).
fn fixture(name: &str, files: &[(&str, &[u8])], vim: bool, scripted: bool) -> Fixture {
    let dir = common::temp_dir(name);
    for (file, bytes) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = vim;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, scripted);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        common::pump_until(TIMEOUT, || editor.view().has_focus());
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn disk(&self, file: &str) -> Vec<u8> {
        std::fs::read(self.path(file)).unwrap()
    }

    fn disk_str(&self, file: &str) -> String {
        String::from_utf8_lossy(&self.disk(file)).into_owned()
    }

    fn doc(&self, file: &str) -> DocRef {
        self.state
            .workspace
            .get(&self.path(file))
            .expect("document in the workspace")
    }

    fn agent_edit(&self, file: &str, old: &str, new: &str) -> ToolReply {
        block_on(agent_edits::apply(
            &self.state.workspace,
            1,
            EditorRequest::Edit {
                path: file.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        ))
    }

    fn edit_ok(&self, file: &str, old: &str, new: &str) {
        let reply = self.agent_edit(file, old, new);
        assert!(!reply.is_error, "agent edit failed: {}", reply_text(&reply));
    }

    /// Answers the alert dialog that is open on the window.
    fn answer(&self, response: &str) {
        common::pump_until(TIMEOUT, || self.state.window.visible_dialog().is_some());
        let alert = self
            .state
            .window
            .visible_dialog()
            .and_then(|d| d.downcast::<adw::AlertDialog>().ok())
            .expect("an alert dialog");
        alert.emit_by_name::<()>("response", &[&response]);
        alert.force_close();
        common::pump_for(Duration::from_millis(50));
    }

    /// Types `keys` into `editor`'s Vim context (`\n` = Return, `\x1b` = Escape), as
    /// `tests/gtk_vim.rs` does, after Lemma's key guard (see `type_keys`).
    fn type_keys(&self, editor: &Editor, keys: &str) {
        type_keys(&self.state, editor, keys);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

/// Types `keys` as a keyboard does: each key press first passes Lemma's key guard (the capture
/// controller on the view's parent, which stops the keys of `Editor::vim_blocks_key`), then goes
/// to the Vim context with `gtk_im_context_filter_key`.
fn type_keys(state: &WindowState, editor: &Editor, keys: &str) {
    let im = editor.vim().expect("Vim is on");
    let surface = state.window.surface().expect("realized window");
    let display = surface.display();
    let keyboard = display
        .default_seat()
        .and_then(|s| s.keyboard())
        .expect("a keyboard device");
    for c in keys.chars() {
        let keyval = match c {
            '\n' => gdk::Key::Return,
            '\x1b' => gdk::Key::Escape,
            c => gdk::unicode_to_keyval(u32::from(c))
                .to_value()
                .get::<gdk::Key>()
                .expect("a keyval"),
        };
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(|k| k.level()))
            .unwrap_or_else(|| panic!("no keycode for {c:?}"));
        let mods = if key.level() > 0 {
            gdk::ModifierType::SHIFT_MASK
        } else {
            gdk::ModifierType::empty()
        };
        if editor.vim_blocks_key(keyval, mods) {
            common::pump_for(Duration::from_millis(2));
            continue;
        }
        let consumed = im.filter_key(
            true,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            mods,
            key.group(),
        );
        im.filter_key(
            false,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            mods,
            key.group(),
        );
        if !consumed && c != '\x1b' {
            editor
                .buffer()
                .insert_interactive_at_cursor(&c.to_string(), true);
        }
        common::pump_for(Duration::from_millis(2));
    }
}

/// Waits until the file at `path` differs from `old`, or `timeout` passes.
fn wait_disk_change(path: &Path, old: &[u8], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::fs::read(path).ok().as_deref() != Some(old) {
            common::pump_for(Duration::from_millis(50));
            return;
        }
        common::pump_for(Duration::from_millis(20));
    }
}

/// Sets `path`'s modification time `secs` seconds into the future, so the change cannot fall
/// into the same second as an earlier load or save.
fn bump_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

// ----- D1 / D6 / D20 / D33: discarding a document under review -----

/// `:q!` on a tab whose document has a pending hunk must throw the typing away. Today the typing
/// stays in the closed document's shadow (even marked clean), and the next Reject writes it.
fn regress_qbang_discard_under_review() {
    let f = fixture("qbang", &[("a.tex", b"one\ntwo\nthree\n")], true, false);
    let ed = f.open("a.tex");
    f.edit_ok("a.tex", "three", "THREE");
    assert_eq!(f.disk_str("a.tex"), "one\ntwo\nTHREE\n");
    // The user types at the top, changes their mind and quits without saving.
    f.type_keys(&ed, "ggIJUNK \x1b");
    assert_eq!(ed.document().text(), "JUNK one\ntwo\nTHREE\n");
    f.type_keys(&ed, ":q!\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    drop(ed);
    let doc = f.doc("a.tex");
    println!(
        "  after :q!: open={} dirty={} text={:?}",
        doc.is_open(),
        doc.is_dirty(),
        doc.text()
    );
    // Reject the agent's hunk from the Changes panel: the file is saved.
    let before = f.disk("a.tex");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    wait_disk_change(&f.path("a.tex"), &before, Duration::from_secs(5));
    assert_eq!(
        f.disk_str("a.tex"),
        "one\ntwo\nthree\n",
        "the typing discarded with :q! must not reach the disk"
    );
}

/// Ctrl+W → Discard on a tab under review: the next "save all" (what every build runs first)
/// must not write the discarded typing.
fn regress_dialog_discard_under_review() {
    let f = fixture("discard", &[("a.tex", b"one\ntwo\nthree\n")], false, false);
    let ed = f.open("a.tex");
    f.edit_ok("a.tex", "three", "THREE");
    ed.buffer().insert(&mut ed.buffer().start_iter(), "JUNK ");
    f.state
        .tab_view
        .close_page(&f.state.tab_view.page(ed.widget()));
    f.answer("discard");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    drop(ed);
    let dirty: Vec<String> = f
        .state
        .workspace
        .dirty_documents()
        .iter()
        .map(|d| d.display_name())
        .collect();
    println!("  dirty documents after Discard: {dirty:?}");
    let _ = block_on(f.state.workspace.save_all_dirty());
    assert_eq!(
        f.disk_str("a.tex"),
        "one\ntwo\nTHREE\n",
        "the typing discarded in the dialog must not reach the disk"
    );
}

// ----- D2 / D17: Lemma's own closed-document write breaks the next save -----

fn regress_reopened_closed_doc_saves() {
    let f = fixture(
        "reopen-save",
        &[("a.tex", b"l1\nl2\nl3\nl4\n")],
        false,
        true,
    );
    let ed = f.open("a.tex");
    f.edit_ok("a.tex", "l2", "L2");
    // Close the tab while the hunk is pending: the document stays as a closed document.
    f.state.close_editor(&ed, false);
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    drop(ed);
    // A later agent edit while it is closed (in another second than the last FileSaver save).
    std::thread::sleep(Duration::from_millis(1100));
    f.edit_ok("a.tex", "l4", "L4");
    assert_eq!(f.disk_str("a.tex"), "l1\nL2\nl3\nL4\n");
    // The user opens the file again (Alt+], the Changes panel) and saves.
    let ed = f.open("a.tex");
    ed.buffer().insert(&mut ed.buffer().start_iter(), "% x\n");
    let result = block_on(ed.document().save());
    assert_eq!(
        result,
        Ok(()),
        "Lemma's own write is not an external modification"
    );
    assert_eq!(f.disk_str("a.tex"), "% x\nl1\nL2\nl3\nL4\n");
}

// ----- D3 / D9 / D22 / D38: closed saves re-encode as UTF-8 -----

fn regress_latin1_closed_save_keeps_encoding() {
    let f = fixture(
        "latin1-closed",
        &[("latin.tex", b"caf\xe9 au lait\nline two\nline three\n")],
        false,
        true,
    );
    let ed = f.open("latin.tex");
    println!(
        "  loaded encoding: {:?}",
        ed.document().file().encoding().charset()
    );
    f.edit_ok("latin.tex", "line three", "LINE THREE");
    assert_eq!(
        f.disk("latin.tex"),
        b"caf\xe9 au lait\nline two\nLINE THREE\n",
        "an open save keeps Latin-1"
    );
    f.state.close_editor(&ed, false);
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    drop(ed);
    f.edit_ok("latin.tex", "line two", "LINE TWO");
    assert_eq!(
        f.disk("latin.tex"),
        b"caf\xe9 au lait\nLINE TWO\nLINE THREE\n",
        "a closed save must keep the file's Latin-1 encoding"
    );
}

// ----- D4 / D10 / D36: closed CRLF document opened and saved becomes LF -----

fn regress_crlf_closed_then_opened_keeps_crlf() {
    let f = fixture(
        "crlf-closed",
        &[("a.tex", b"l1\r\nl2\r\nl3\r\n")],
        false,
        true,
    );
    // The agent edits the file while it is not open.
    f.edit_ok("a.tex", "l2", "L2");
    assert_eq!(f.disk("a.tex"), b"l1\r\nL2\r\nl3\r\n");
    // The user opens it and rejects the hunk.
    let ed = f.open("a.tex");
    let doc = ed.document().clone();
    let before = f.disk("a.tex");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    wait_disk_change(&f.path("a.tex"), &before, Duration::from_secs(5));
    assert_eq!(
        f.disk("a.tex"),
        b"l1\r\nl2\r\nl3\r\n",
        "the file's CRLF line endings must be kept"
    );
}

// ----- D5: U+2029 is saved as garbage -----

fn regress_u2029_save_writes_buffer_text() {
    let f = fixture(
        "u2029",
        &[("a.tex", b"\\section{Intro}\nText here.\n")],
        false,
        true,
    );
    let ed = f.open("a.tex");
    // Text pasted from a PDF or a word processor can carry a PARAGRAPH SEPARATOR.
    ed.buffer()
        .insert(&mut ed.buffer().end_iter(), "see\u{2029}below\n");
    let text = ed.document().text();
    block_on(ed.document().save()).expect("save");
    assert_eq!(
        f.disk_str("a.tex"),
        text,
        "the file must hold the buffer's text (bytes {:x?})",
        f.disk("a.tex")
    );
}

// ----- D7 / D23 / D34: closed documents overwrite external changes -----

fn regress_closed_doc_external_change_refused() {
    let f = fixture("closed-external", &[("a.tex", b"mine\n")], false, true);
    // The agent edits the file while it is not open: a closed document under review.
    f.edit_ok("a.tex", "mine", "% mine");
    let doc = f.doc("a.tex");
    assert!(!doc.is_open());
    // Another program (git checkout, another editor) changes the file.
    std::fs::write(f.path("a.tex"), "theirs\n").unwrap();
    bump_mtime(&f.path("a.tex"), 5);
    // Rejecting the hunk saves the closed document.
    doc.apply_edits(
        &[TextEdit {
            range: 0..2,
            text: String::new(),
        }],
        Origin::Review,
    )
    .unwrap();
    let result = block_on(doc.save());
    println!("  closed save -> {result:?}");
    assert_eq!(
        f.disk_str("a.tex"),
        "theirs\n",
        "a file changed on disk must not be overwritten without asking"
    );
    assert_eq!(result, Err(SaveError::ExternallyModified));
}

fn regress_reopened_closed_doc_external_change_refused() {
    let f = fixture("reopen-external", &[("a.tex", b"mine\n")], false, true);
    f.edit_ok("a.tex", "mine", "% mine");
    // The user opens the closed document, then another program changes the file.
    let ed = f.open("a.tex");
    std::fs::write(f.path("a.tex"), "theirs\n").unwrap();
    bump_mtime(&f.path("a.tex"), 5);
    ed.buffer().insert(&mut ed.buffer().end_iter(), "typed\n");
    let result = block_on(ed.document().save());
    println!("  save after reopen -> {result:?}");
    assert_eq!(
        f.disk_str("a.tex"),
        "theirs\n",
        "a file changed on disk must not be overwritten without asking"
    );
    assert_eq!(result, Err(SaveError::ExternallyModified));
}

// ----- D8 / D25: mixed line endings are normalised on save -----

fn regress_mixed_line_endings_round_trip() {
    let cases: &[&[u8]] = &[b"a\r\nb\nc\r\n", b"a\nb\r\nc\n", b"a\rb\nc\n"];
    let mut failures = Vec::new();
    for (i, bytes) in cases.iter().enumerate() {
        let name = format!("m{i}.tex");
        let f = fixture(&format!("mixed-{i}"), &[(&name, bytes)], false, true);
        let ed = f.open(&name);
        let text = ed.document().text();
        assert_eq!(text.as_bytes(), *bytes, "the loader keeps the bytes");
        // Ctrl+S / :w without an edit.
        block_on(ed.document().save()).expect("save");
        let disk = f.disk(&name);
        println!(
            "  {:?} -> {:?}",
            String::from_utf8_lossy(bytes),
            String::from_utf8_lossy(&disk)
        );
        if disk != *bytes {
            failures.push(String::from_utf8_lossy(bytes).into_owned());
        }
    }
    assert!(
        failures.is_empty(),
        "saving without an edit changed these files: {failures:?}"
    );
}

// ----- D11 / D24: external changes within the same second go unnoticed -----

fn regress_external_change_same_second_refused() {
    let f = fixture("same-second", &[], false, true);
    let path = f.path("a.tex");
    // Pin both modification times inside one whole second (100 ms and 600 ms past it), so the
    // case does not depend on how long the load takes on a busy machine.
    let second = UNIX_EPOCH
        + Duration::from_secs(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(10),
        );
    let set_mtime = |at: SystemTime| {
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(at))
            .unwrap();
    };
    std::fs::write(&path, "mine\n").unwrap();
    set_mtime(second + Duration::from_millis(100));
    let loaded_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let ed = f.open("a.tex");
    // Another program writes the file a moment later (a formatter, git, an agent's sed -i).
    std::fs::write(&path, "theirs, longer\n").unwrap();
    set_mtime(second + Duration::from_millis(600));
    let external_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_secs();
    println!(
        "  load mtime {:?}, external mtime {:?}",
        loaded_mtime.duration_since(UNIX_EPOCH).unwrap(),
        external_mtime.duration_since(UNIX_EPOCH).unwrap()
    );
    assert_eq!(
        secs(loaded_mtime),
        secs(external_mtime),
        "test setup: both writes in one second"
    );
    assert_ne!(
        loaded_mtime, external_mtime,
        "test setup: the mtime changed"
    );
    ed.buffer().insert(&mut ed.buffer().end_iter(), "typed\n");
    let result = block_on(ed.document().save());
    println!("  save -> {result:?}");
    assert_eq!(
        f.disk_str("a.tex"),
        "theirs, longer\n",
        "the external change must not be overwritten without asking"
    );
    assert_eq!(result, Err(SaveError::ExternallyModified));
}

// ----- D12 / D29: typing during a save -----

fn regress_typing_during_save_writes_a_real_state() {
    let large = std::fs::read(common::fixtures().join("large.tex")).unwrap();
    let mut failures = Vec::new();
    for iterations in [2usize, 5, 10, 20] {
        let f = fixture(
            &format!("save-typing-{iterations}"),
            &[("large.tex", &large)],
            false,
            true,
        );
        let ed = f.open("large.tex");
        let doc = ed.document().clone();
        let buffer = ed.buffer().clone();
        buffer.insert(&mut buffer.start_iter(), "% first\n");
        let at_request = doc.text();
        let save = doc.save();
        let ctx = glib::MainContext::default();
        for _ in 0..iterations {
            ctx.iteration(true);
        }
        // The user keeps typing at both ends of the document while the save runs.
        buffer.insert(&mut buffer.start_iter(), "TOP\n");
        buffer.insert(&mut buffer.end_iter(), "BOTTOM\n");
        let after = doc.text();
        block_on(save).expect("save");
        let disk = std::fs::read_to_string(f.path("large.tex")).unwrap();
        let (is_before, is_after) = (disk == at_request, disk == after);
        println!(
            "  {iterations} iterations: disk==at_request {is_before}, disk==after {is_after}, \
             TOP {}, BOTTOM {}",
            disk.starts_with("TOP\n"),
            disk.ends_with("BOTTOM\n")
        );
        if !is_before && !is_after {
            failures.push(iterations);
        }
    }
    assert!(
        failures.is_empty(),
        "the saved file matched no state of the buffer (after {failures:?} iterations)"
    );
}

// ----- D13: a review edit in Vim insert mode joins the user's undo step -----

fn regress_review_edit_in_insert_mode_is_own_undo_step() {
    let f = fixture(
        "insert-undo",
        &[("a.tex", b"alpha\nbeta\ngamma\n")],
        true,
        true,
    );
    let ed = f.open("a.tex");
    let doc = ed.document().clone();
    // Insert mode, typing at the end of line 1.
    f.type_keys(&ed, "Axyz");
    assert_eq!(doc.text(), "alphaxyz\nbeta\ngamma\n");
    // A reject (Alt+N or the Changes panel) applies its edit while insert mode is still on.
    doc.apply_edits(
        &[TextEdit {
            range: 14..19,
            text: "GAMMA".into(),
        }],
        Origin::Review,
    )
    .unwrap();
    assert_eq!(doc.text(), "alphaxyz\nbeta\nGAMMA\n");
    f.type_keys(&ed, "\x1bu");
    let text = doc.text();
    println!("  after one u: {text:?}");
    assert!(
        text == "alphaxyz\nbeta\ngamma\n" || text == "alpha\nbeta\nGAMMA\n",
        "one undo step must revert either the review edit or the typing, not both: {text:?}"
    );
}

// ----- D14: :w! does nothing -----

fn regress_colon_w_bang_saves() {
    let f = fixture("w-bang", &[("a.tex", b"one\n")], true, true);
    let ed = f.open("a.tex");
    f.type_keys(&ed, "Otwo\x1b");
    assert!(ed.document().is_dirty());
    f.type_keys(&ed, ":w!\n");
    common::pump_for(Duration::from_millis(1500));
    assert_eq!(f.disk_str("a.tex"), "two\none\n", ":w! must write the file");
    assert!(!ed.document().is_dirty());
}

// ----- D15 / D26: a scripted flag without --quit-after-ms keeps the window but drops prompts --

fn regress_scripted_flag_without_deadline_keeps_prompts() {
    let status = child("scripted_prompts", Duration::from_secs(60));
    assert_eq!(
        status,
        Ok(Some(0)),
        "an interactive window opened with --sidebar must still ask about unsaved changes"
    );
}

// ----- D16: Vim J on a whitespace-only last line -----

fn regress_vim_join_whitespace_last_line_returns() {
    let status = child("vim_join", Duration::from_secs(30));
    assert_eq!(
        status,
        Ok(Some(0)),
        "GJ on a whitespace-only last line must return"
    );
}

// ----- D18: Latin-1 file with an accent near its end -----

fn regress_latin1_trailing_accent_opens() {
    let f = fixture("latin1-tail", &[("tail.tex", b"% caf\xe9\n")], false, true);
    let result = block_on(f.state.workspace.open(&f.path("tail.tex")));
    match result {
        Ok(doc) => assert_eq!(doc.text(), "% café\n"),
        Err(e) => panic!("a valid Latin-1 file must open: {e}"),
    }
}

// ----- D19: an editor that outlives its tab -----

fn regress_wq_then_discard_does_not_abort() {
    let status = child("wq_discard", Duration::from_secs(60));
    assert_eq!(
        status,
        Ok(Some(0)),
        "closing a tab while its :wq save runs must not crash or log criticals"
    );
}

// ----- D21: NUL in a closed document -----

fn regress_nul_closed_doc_open_keeps_text() {
    let original = b"a\0b\nsecond line\n";
    let f = fixture("nul", &[("a.tex", original)], false, true);
    let reply = f.agent_edit("a.tex", "second line", "SECOND line");
    if reply.is_error {
        // Refusing the file is fine; it must be left alone.
        assert_eq!(f.disk("a.tex"), original);
        return;
    }
    let doc = f.doc("a.tex");
    let text = doc.text();
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(f.state.open_file(&f.path("a.tex")))
    }));
    println!(
        "  open panicked: {}; text afterwards {:?}",
        opened.is_err(),
        doc.text()
    );
    assert_eq!(
        doc.text(),
        text,
        "opening must not lose the document's text"
    );
    let _ = block_on(doc.save());
    assert_eq!(
        f.disk("a.tex"),
        text.as_bytes(),
        "saving afterwards must not truncate the file"
    );
    assert!(opened.is_ok(), "opening the document panicked");
}

// ----- D27: :e! does not revert -----

fn regress_colon_e_bang_reverts() {
    let f = fixture("e-bang", &[("a.tex", b"line one\nline two\n")], true, true);
    let ed = f.open("a.tex");
    f.type_keys(&ed, "dd");
    assert_eq!(ed.document().text(), "line two\n");
    f.type_keys(&ed, ":e!\n");
    common::pump_for(Duration::from_millis(500));
    let editor = f.state.active_editor().expect("an editor");
    assert_eq!(
        editor.document().text(),
        "line one\nline two\n",
        ":e! must revert the buffer to the file on disk"
    );
    assert!(!editor.document().is_dirty());
}

// ----- D31 / D32: the project root falls back to "/" -----

fn regress_missing_dir_root_is_not_filesystem_root() {
    let dir = common::temp_dir("root-slash");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let victim = outside.join("victim.tex");
    std::fs::write(&victim, "original\n").unwrap();
    let edits = dir.join("edits.json");
    std::fs::write(
        &edits,
        serde_json::json!([{
            "tool": "edit",
            "file_path": victim,
            "old_string": "original",
            "new_string": "CHANGED BY AGENT",
        }])
        .to_string(),
    )
    .unwrap();
    // The user starts a new paper before creating its directory.
    let main = dir.join("newpaper").join("main.tex");
    let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(&main)
        .arg("--apply-as-agent")
        .arg(&edits)
        .args(["--quit-after-ms", "1500"])
        .env("LEMMA_CONFIG", "/dev/null")
        .output()
        .expect("run lemma");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "original\n",
        "an agent edit outside the intended project directory must be refused"
    );
}

// ----- saving after the fixes -----

/// A file whose modification time changed but whose contents did not (`touch`, a checkout of the
/// same version) is not "changed by another program".
fn touched_file_still_saves() {
    let f = fixture("touched", &[("a.tex", b"same\n")], false, true);
    let ed = f.open("a.tex");
    bump_mtime(&f.path("a.tex"), 5);
    ed.buffer().insert(&mut ed.buffer().end_iter(), "more\n");
    assert_eq!(block_on(ed.document().save()), Ok(()));
    assert_eq!(f.disk_str("a.tex"), "same\nmore\n");
    // And the next save checks against this write.
    ed.buffer().insert(&mut ed.buffer().end_iter(), "again\n");
    assert_eq!(block_on(ed.document().save()), Ok(()));
    assert_eq!(f.disk_str("a.tex"), "same\nmore\nagain\n");
}

fn utf8_bom_round_trips() {
    let f = fixture(
        "bom",
        &[("a.tex", "\u{feff}\\section{A}\r\n".as_bytes())],
        false,
        true,
    );
    let ed = f.open("a.tex");
    ed.buffer().insert(&mut ed.buffer().end_iter(), "x\r\n");
    block_on(ed.document().save()).expect("save");
    assert_eq!(f.disk_str("a.tex"), "\u{feff}\\section{A}\r\nx\r\n");
}

fn latin1_save_refuses_unencodable_text() {
    let f = fixture("latin1-arrow", &[("a.tex", b"caf\xe9\n")], false, true);
    let ed = f.open("a.tex");
    assert_eq!(ed.document().charset().as_deref(), Some("ISO-8859-15"));
    ed.buffer()
        .insert(&mut ed.buffer().end_iter(), "\u{2192}\n");
    let result = block_on(ed.document().save());
    assert!(matches!(result, Err(SaveError::Io(_))), "{result:?}");
    assert_eq!(f.disk("a.tex"), b"caf\xe9\n", "nothing written");
    assert!(ed.document().is_dirty());
}

fn colon_w_bang_overwrites_external_change() {
    let f = fixture("w-bang-external", &[("a.tex", b"one\n")], true, true);
    let ed = f.open("a.tex");
    std::fs::write(f.path("a.tex"), "theirs\n").unwrap();
    bump_mtime(&f.path("a.tex"), 5);
    f.type_keys(&ed, "Otwo\x1b:w\n");
    common::pump_for(Duration::from_millis(500));
    assert_eq!(f.disk_str("a.tex"), "theirs\n", ":w asks first");
    f.type_keys(&ed, ":w!\n");
    common::pump_until(TIMEOUT, || !ed.document().is_dirty());
    assert_eq!(f.disk_str("a.tex"), "two\none\n", ":w! overwrites");
}

fn colon_e_reloads_only_when_clean() {
    let f = fixture("colon-e", &[("a.tex", b"one\n")], true, true);
    let ed = f.open("a.tex");
    // Unsaved changes: `:e` keeps them.
    f.type_keys(&ed, "Otwo\x1b:e\n");
    common::pump_for(Duration::from_millis(300));
    assert_eq!(ed.document().text(), "two\none\n");
    assert!(ed.document().is_dirty());
    // Saved, then changed by another program: `:e` shows the new text.
    f.type_keys(&ed, ":w\n");
    common::pump_until(TIMEOUT, || !ed.document().is_dirty());
    std::fs::write(f.path("a.tex"), "three\n").unwrap();
    f.type_keys(&ed, ":e\n");
    common::pump_until(TIMEOUT, || ed.document().text() == "three\n");
    assert!(!ed.document().is_dirty());
    // The reload is one undo step.
    f.type_keys(&ed, "u");
    assert_eq!(ed.document().text(), "two\none\n");
}

// ----- child processes -----

/// Runs this binary with `LEMMA_REGRESS_D_CHILD=probe`. `Ok(code)` when it exited (`None` for a
/// signal), `Err` when it was still running after `timeout` (it is killed).
fn child(probe: &str, timeout: Duration) -> Result<Option<i32>, String> {
    // The real app's interactive window (`scripted_prompts`) saves its session and keeps copies
    // of unsaved text: in a temporary state directory, never the user's.
    let state = common::temp_dir(&format!("child-{probe}-state"));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env(CHILD_ENV, probe)
        .env("LEMMA_CONFIG", "/dev/null")
        .env("XDG_STATE_HOME", &state)
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

static CHILD_CRITICALS: AtomicUsize = AtomicUsize::new(0);

fn run_child(probe: &str) -> ExitCode {
    if probe != "scripted_prompts" {
        if gtk::init().is_err() {
            println!("SKIP: no display");
            return ExitCode::SUCCESS;
        }
        sourceview5::init();
    }
    glib::log_set_writer_func(|level, fields| {
        if matches!(
            level,
            glib::LogLevel::Error | glib::LogLevel::Critical | glib::LogLevel::Warning
        ) && !common::is_environment_noise(fields)
        {
            CHILD_CRITICALS.fetch_add(1, Ordering::SeqCst);
        }
        glib::log_writer_default(level, fields)
    });
    match probe {
        "vim_join" => child_vim_join(),
        // Manual probe (not a test): the same keys on a plain GtkSourceView view, no Lemma code.
        "vim_join_plain" => child_vim_join_plain(),
        // Manual probe (not a test): typing and apply_edits cost with a lone CR in the text.
        "perf_lone_cr" => child_perf_lone_cr(),
        "wq_discard" => child_wq_discard(),
        "scripted_prompts" => child_scripted_prompts(),
        other => panic!("unknown probe {other}"),
    }
}

fn child_vim_join() -> ExitCode {
    let f = fixture("child-vim-join", &[("a.tex", b"a\n    ")], true, true);
    let ed = f.open("a.tex");
    type_keys(&f.state, &ed, "G");
    println!("  pressing J on the last line {:?}", ed.document().text());
    type_keys(&f.state, &ed, "J");
    println!("  J returned: {:?}", ed.document().text());
    drop(ed);
    drop(f);
    // More joins that reach a whitespace-only end of the buffer (each would hang), and joins
    // that must still work.
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("a\nb   ", "GJ", Some("a\nb   ")),
        ("a\nb\n  ", "GkJ", Some("a\nb\n  ")),
        ("a\n  ", "GVJ", Some("a\n  ")),
        ("a\n\t", "G3J", Some("a\n\t")),
        ("a\nb\n", "ggJ", Some("a b\n")),
        ("a\n  ", "GfJ\x1b", None),
    ];
    for (text, keys, expected) in cases {
        let f = fixture(
            "child-vim-join-more",
            &[("a.tex", text.as_bytes())],
            true,
            true,
        );
        let ed = f.open("a.tex");
        println!("  {text:?}: typing {keys:?}");
        type_keys(&f.state, &ed, keys);
        let result = ed.document().text();
        println!("    -> {result:?}");
        if let Some(expected) = expected
            && result != *expected
        {
            println!("    expected {expected:?}");
            return ExitCode::from(6);
        }
    }
    ExitCode::SUCCESS
}

fn child_vim_join_plain() -> ExitCode {
    let window = gtk::Window::new();
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_implicit_trailing_newline(false);
    buffer.set_text("a\n    ");
    let view = sourceview5::View::with_buffer(&buffer);
    let im = sourceview5::VimIMContext::new();
    let keys = gtk::EventControllerKey::new();
    keys.set_im_context(Some(&im));
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    view.add_controller(keys);
    im.set_client_widget(Some(&view));
    window.set_child(Some(&view));
    window.present();
    common::pump_until(TIMEOUT, || window.is_mapped());
    let surface = window.surface().unwrap();
    let display = surface.display();
    let keyboard = display.default_seat().and_then(|s| s.keyboard()).unwrap();
    for keyval in [gdk::Key::G, gdk::Key::J] {
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(|k| k.level()))
            .unwrap();
        let mods = if key.level() > 0 {
            gdk::ModifierType::SHIFT_MASK
        } else {
            gdk::ModifierType::empty()
        };
        println!("  plain view: pressing {keyval:?}");
        im.filter_key(
            true,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            mods,
            key.group(),
        );
        im.filter_key(
            false,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            mods,
            key.group(),
        );
        common::pump_for(Duration::from_millis(5));
    }
    println!("  plain view: returned");
    ExitCode::SUCCESS
}

fn child_perf_lone_cr() -> ExitCode {
    let large = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    for lines in [10_000usize, 60_000] {
        let text: String = large.split_inclusive('\n').take(lines).collect();
        for lone_cr in [false, true] {
            let path = PathBuf::from("/nonexistent/lemma-perf/main.tex");
            let buffer = lemma_app::document::new_buffer(&path);
            buffer.set_text(&text);
            if lone_cr {
                buffer.insert(&mut buffer.end_iter(), "x\r");
            }
            let doc = lemma_app::document::Document::new_open(
                path,
                buffer.clone(),
                sourceview5::File::new(),
            );
            let start = Instant::now();
            for _ in 0..200 {
                let mut iter = buffer.iter_at_line(lines as i32 / 2).unwrap();
                buffer.insert(&mut iter, "a");
            }
            let per_key = start.elapsed() / 200;
            let current = doc.text();
            let edits: Vec<TextEdit> = (0..200)
                .map(|i| {
                    let mut at = current.len() - 1000 * (i + 1);
                    while !current.is_char_boundary(at) {
                        at -= 1;
                    }
                    TextEdit {
                        range: at..at,
                        text: "z".into(),
                    }
                })
                .collect();
            let start = Instant::now();
            doc.apply_edits(&edits, Origin::Review).unwrap();
            println!(
                "  {lines} lines, lone CR {lone_cr}: {per_key:?} per insert, apply_edits(200) {:?}",
                start.elapsed()
            );
        }
    }
    ExitCode::SUCCESS
}

fn child_wq_discard() -> ExitCode {
    let large = std::fs::read(common::fixtures().join("large.tex")).unwrap();
    // Three copies: a save that takes long enough to close the tab meanwhile.
    let big: Vec<u8> = [&large[..], &large[..], &large[..]].concat();
    let f = fixture("child-wq-discard", &[("big.tex", &big)], true, false);
    let ed = f.open("big.tex");
    type_keys(&f.state, &ed, "ix\x1b");
    type_keys(&f.state, &ed, ":wq\n");
    // The save runs; the user closes the tab (Ctrl+W) and picks Discard meanwhile.
    f.state
        .tab_view
        .close_page(&f.state.tab_view.page(ed.widget()));
    f.answer("discard");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    drop(ed);
    let path = f.path("big.tex");
    common::pump_until(TIMEOUT, || {
        std::fs::read(&path).is_ok_and(|b| b.first() == Some(&b'x'))
    });
    common::pump_for(Duration::from_millis(500));
    let criticals = CHILD_CRITICALS.load(Ordering::SeqCst);
    println!("  survived; {criticals} GLib warnings/criticals");
    if criticals == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
    }
}

/// Runs the real app with `--sidebar changes` (a "scripted" flag) but no `--quit-after-ms`: the
/// window stays open for normal editing. Types into it and closes the window; exits 0 when the
/// unsaved-changes dialog appears, 3 when the window closed without asking.
fn child_scripted_prompts() -> ExitCode {
    use lemma_app::cli::{Options, Parsed};
    let dir = common::temp_dir("child-scripted");
    let main = dir.join("main.tex");
    std::fs::write(&main, "hello\n").unwrap();
    // `LEMMA_REGRESS_D_CONTROL=1` runs the control case without the flag (the dialog appears).
    let mut args = vec![main.clone().into_os_string()];
    if std::env::var_os("LEMMA_REGRESS_D_CONTROL").is_none() {
        args.extend(["--sidebar".into(), "changes".into()]);
    }
    let Ok(Parsed::Run(options)) = Options::parse(args) else {
        panic!("options");
    };
    let started = Instant::now();
    let typed = std::cell::Cell::new(false);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let window = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<adw::ApplicationWindow>().ok())
            .find(|w| w.is_visible());
        let Some(window) = window else {
            return glib::ControlFlow::Continue;
        };
        if started.elapsed() < Duration::from_secs(4) {
            return glib::ControlFlow::Continue;
        }
        if !typed.get() {
            let Some(view) = find_view(window.upcast_ref()) else {
                return glib::ControlFlow::Continue;
            };
            let buffer = view.buffer();
            buffer.insert(&mut buffer.start_iter(), "UNSAVED WORK ");
            typed.set(true);
            println!("  typed into the window; closing it");
            window.close();
            return glib::ControlFlow::Continue;
        }
        if started.elapsed() > Duration::from_secs(6) {
            if window.visible_dialog().is_some() {
                println!("  the unsaved-changes dialog appeared");
                std::process::exit(0);
            }
            println!("  the window is open but no dialog appeared");
            std::process::exit(5);
        }
        glib::ControlFlow::Continue
    });
    let _ = lemma_app::app::run(options, started);
    println!("  the window closed without asking about the unsaved changes");
    ExitCode::from(3)
}

/// The first `sourceview5::View` under `widget`.
fn find_view(widget: &gtk::Widget) -> Option<sourceview5::View> {
    if let Ok(view) = widget.clone().downcast::<sourceview5::View>() {
        return Some(view);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(view) = find_view(&c) {
            return Some(view);
        }
        child = c.next_sibling();
    }
    None
}
