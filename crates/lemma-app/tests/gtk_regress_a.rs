//! Regression tests for review defects confirmed in the M1 verification (area A: agent-edit
//! review). Each test reproduces one finding and fails until it is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress_a`.

mod common;

use adw::prelude::*;
use lemma_agent::{EditorRequest, ToolReply};
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::testmode::{self, review::reply_text};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use lemma_review::Hunk;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    // Rejecting a file the agent created moves it to the trash: keep that out of the user's.
    let data = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress-a-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    common::run(&[
        // A3 (A19)
        ("regress_alt_u_reverts_all_files_decision", alt_u_all_files),
        // A4 (A20)
        (
            "regress_alt_u_skips_to_older_decision",
            alt_u_skips_gone_document,
        ),
        // A2 (A11)
        (
            "regress_closed_doc_stale_shadow_agent",
            closed_doc_stale_agent,
        ),
        (
            "regress_closed_doc_stale_shadow_open_save",
            closed_doc_stale_open,
        ),
        (
            "regress_closed_doc_pending_review_external_change_refused",
            closed_doc_pending_external,
        ),
        // A10
        (
            "regress_discarded_typing_written_by_reject",
            discard_then_reject,
        ),
        (
            "regress_forced_close_typing_written_by_reject",
            forced_close_then_reject,
        ),
        // A12
        (
            "regress_crlf_closed_doc_opened_keeps_crlf",
            crlf_closed_then_opened,
        ),
        (
            "regress_latin1_closed_doc_save_keeps_encoding",
            latin1_closed_save,
        ),
        // A5
        ("regress_header_height_stable", header_height_stable),
        // A6
        ("regress_crlf_phantom_extra_line", crlf_phantom_height),
        // A7
        ("regress_phantom_layout_cost", phantom_layout_cost),
        // A13
        ("regress_gutter_lone_cr_cost", gutter_lone_cr_cost),
        // A14
        ("regress_retag_cost_after_many_hunks", retag_cost),
        // A15
        (
            "regress_hidden_changes_panel_rebuilds_plain_diff",
            hidden_panel_plain_diff,
        ),
        // A17
        (
            "regress_concurrent_new_file_writes",
            concurrent_new_file_writes,
        ),
        // A22
        ("regress_bar_labels_follow_rebound_keys", bar_labels_rebound),
        ("regress_bar_labels_name_alt_not_option", bar_labels_default),
        // A23
        (
            "regress_reject_save_conflict_offers_overwrite",
            reject_conflict_overwrite,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
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
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
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
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(900, 720);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.dir.join(file))).expect("open")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn disk(&self, file: &str) -> String {
        String::from_utf8_lossy(&std::fs::read(self.path(file)).unwrap()).into_owned()
    }

    fn doc(&self, file: &str) -> DocRef {
        self.state
            .workspace
            .get(&self.path(file))
            .expect("document")
    }

    fn hunks(&self, file: &str) -> Vec<Hunk> {
        self.state
            .workspace
            .get(&self.path(file))
            .and_then(|d| d.review.borrow().as_ref().map(|r| r.hunks().to_vec()))
            .unwrap_or_default()
    }

    fn agent(&self, call: u64, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, call, req))
    }

    fn edit(&self, file: &str, old: &str, new: &str) -> ToolReply {
        let reply = self.agent(
            1,
            EditorRequest::Edit {
                path: file.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        );
        assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
        reply
    }

    fn write(&self, file: &str, content: &str) -> ToolReply {
        let reply = self.agent(
            1,
            EditorRequest::Write {
                path: file.into(),
                content: content.into(),
            },
        );
        assert!(!reply.is_error, "write failed: {}", reply_text(&reply));
        reply
    }

    fn action(&self, name: &str) {
        WidgetExt::activate_action(&self.state.window, &format!("win.{name}"), None)
            .unwrap_or_else(|e| panic!("win.{name}: {e}"));
        common::pump_for(Duration::from_millis(30));
    }

    fn put_cursor(&self, editor: &Editor, line: u32) {
        let buffer = editor.buffer();
        buffer.place_cursor(&buffer.iter_at_line(line as i32).unwrap());
    }

    fn frames(&self, n: u32) {
        assert!(block_on(testmode::wait_for_frames(&self.state.window, n)));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut child = widget.first_child();
    while let Some(c) = child {
        out.extend(descendants(&c));
        child = c.next_sibling();
        out.push(c);
    }
    out
}

// ----- A3: Alt+U after a decision on every file reverts only one file -----

fn alt_u_all_files() {
    let f = fixture("a3", &[("a.tex", &lines(20)), ("b.tex", &lines(20))]);
    f.open("a.tex");
    f.edit("a.tex", "line 5\n", "line five\n");
    f.edit("b.tex", "line 7\n", "line seven\n");
    let counts = |f: &Fixture| (f.hunks("a.tex").len(), f.hunks("b.tex").len());
    assert_eq!(counts(&f), (1, 1));

    // Ctrl+Alt+N, then one Alt+U.
    f.action("review-reject-all");
    common::pump_for(Duration::from_millis(100));
    assert_eq!(counts(&f), (0, 0));
    f.action("review-undo");
    common::pump_for(Duration::from_millis(100));
    assert_eq!(
        counts(&f),
        (1, 1),
        "one Alt+U after Reject All must bring back the hunks of every file"
    );

    // Ctrl+Alt+Y, then one Alt+U.
    f.action("review-accept-all");
    assert_eq!(counts(&f), (0, 0));
    f.action("review-undo");
    assert_eq!(
        counts(&f),
        (1, 1),
        "one Alt+U after Accept All must bring back the hunks of every file"
    );
}

// ----- A4: Alt+U silently reverts an older decision when the last one's document is gone -----

fn alt_u_skips_gone_document() {
    // A rejected new file goes to the trash; Alt+U must not re-open an older accept elsewhere.
    let f = fixture("a4", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    f.put_cursor(&editor, 4);
    f.action("review-accept");
    assert!(f.hunks("main.tex").is_empty());
    f.write("new.tex", "fresh\ntext\n");
    let path = f.path("new.tex");
    review_ui::decide_document(&f.state, &f.doc("new.tex"), Decision::Reject);
    common::pump_until(TIMEOUT, || !path.exists());
    f.action("review-undo");
    common::pump_for(Duration::from_millis(100));
    assert!(
        f.hunks("main.tex").is_empty(),
        "Alt+U (meant for the rejected new.tex) re-opened the older accept in main.tex"
    );

    // The same after the last decision's tab was closed.
    let f = fixture("a20", &[("a.tex", &lines(20)), ("b.tex", &lines(20))]);
    let a = f.open("a.tex");
    f.edit("a.tex", "line 5\n", "line five\n");
    f.put_cursor(&a, 4);
    f.action("review-accept");
    let b = f.open("b.tex");
    f.edit("b.tex", "line 7\n", "line seven\n");
    f.put_cursor(&b, 6);
    f.action("review-accept");
    assert!(f.hunks("a.tex").is_empty() && f.hunks("b.tex").is_empty());
    f.state.close_editor(&b, false);
    common::pump_for(Duration::from_millis(100));
    f.action("review-undo");
    common::pump_for(Duration::from_millis(100));
    assert!(
        f.hunks("a.tex").is_empty(),
        "Alt+U after closing b.tex re-opened the older accept in a.tex"
    );
}

// ----- A2 / A11: a decided closed document keeps a stale shadow -----

fn closed_doc_stale_agent() {
    let f = fixture("a2", &[("main.tex", "x\n"), ("b.tex", "b1\nb2\nb3\n")]);
    f.edit("b.tex", "b1\n", "B1\n");
    assert!(!f.doc("b.tex").is_open());
    review_ui::decide_document(&f.state, &f.doc("b.tex"), Decision::Accept);
    assert!(f.hunks("b.tex").is_empty());
    // Later another program (git pull, another editor) changes the file.
    std::fs::write(f.path("b.tex"), "B1\nb2\nb3\nexternal line\n").unwrap();

    let reply = f.agent(
        2,
        EditorRequest::ReadFile {
            path: "b.tex".into(),
            offset: None,
            limit: None,
        },
    );
    let read = reply_text(&reply);
    f.edit("b.tex", "b2\n", "B2\n");
    let disk = f.disk("b.tex");
    assert!(
        disk.contains("external line"),
        "the agent's edit overwrote the external change; disk is now {disk:?} \
         (read_file had returned {read:?})"
    );
    assert!(
        read.contains("external line"),
        "read_file returned the stale shadow: {read:?}"
    );
}

fn closed_doc_stale_open() {
    let f = fixture("a11", &[("main.tex", "x\n"), ("ch.tex", "c1\nc2\nc3\n")]);
    f.edit("ch.tex", "c1\n", "C1\n");
    review_ui::decide_document(&f.state, &f.doc("ch.tex"), Decision::Accept);
    std::fs::write(f.path("ch.tex"), "C1\nc2\nc3\nexternal line\n").unwrap();
    // The user opens the file, types a line and saves.
    let editor = f.open("ch.tex");
    let shown = f.doc("ch.tex").text();
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "typed\n");
    let result = block_on(f.doc("ch.tex").save());
    let disk = f.disk("ch.tex");
    assert!(
        disk.contains("external line"),
        "saving silently overwrote the external change (save result {result:?}, the editor \
         showed {shown:?}, disk now {disk:?})"
    );
}

/// A closed document whose hunks still await review is not edited over an external change:
/// the agent gets an error and the file keeps the other program's text.
fn closed_doc_pending_external() {
    let f = fixture(
        "a2-pending",
        &[("main.tex", "x\n"), ("b.tex", "b1\nb2\nb3\n")],
    );
    f.edit("b.tex", "b1\n", "B1\n");
    assert_eq!(f.hunks("b.tex").len(), 1);
    std::fs::write(f.path("b.tex"), "b1\nb2\nb3\nexternal line\n").unwrap();
    let reply = f.agent(
        2,
        EditorRequest::Edit {
            path: "b.tex".into(),
            old_string: "b2\n".into(),
            new_string: "B2\n".into(),
            replace_all: false,
        },
    );
    assert!(reply.is_error, "{}", reply_text(&reply));
    assert!(
        reply_text(&reply).contains("changed by another program"),
        "{}",
        reply_text(&reply)
    );
    assert_eq!(f.disk("b.tex"), "b1\nb2\nb3\nexternal line\n");
}

// ----- A10: "Discard" on tab close keeps the typing, a later Reject writes it -----

fn discard_then_reject() {
    let original = lines(10);
    let f = fixture("a10", &[("main.tex", &original)]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% DISCARDED TYPING\n");
    common::pump_for(Duration::from_millis(100));
    // Close the tab without saving: in a scripted window no dialog is shown, which is the path
    // the "Save Changes?" dialog's Discard takes (close_page_finish(page, true)).
    f.state.close_editor(&editor, false);
    common::pump_for(Duration::from_millis(100));
    assert!(f.state.editors().is_empty());
    let doc = f.doc("main.tex");
    let dirty_after_discard = doc.is_dirty();
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !f.disk("main.tex").contains("line five"));
    let disk = f.disk("main.tex");
    assert!(
        !disk.contains("DISCARDED"),
        "Reject wrote the typing the user discarded: {disk:?} (closed doc dirty after discard: \
         {dirty_after_discard})"
    );
    assert_eq!(disk, original);
}

/// The same through a forced close (`:q!`, `close_editor(.., true)`), which clears the modified
/// flag before the tab closes.
fn forced_close_then_reject() {
    let original = lines(10);
    let f = fixture("a10-forced", &[("main.tex", &original)]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% DISCARDED TYPING\n");
    common::pump_for(Duration::from_millis(100));
    f.state.close_editor(&editor, true);
    common::pump_for(Duration::from_millis(100));
    let doc = f.doc("main.tex");
    assert!(!doc.text().contains("DISCARDED"), "{:?}", doc.text());
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !f.disk("main.tex").contains("line five"));
    assert_eq!(f.disk("main.tex"), original);
}

// ----- A12: closed documents lose CRLF / Latin-1 -----

fn crlf_closed_then_opened() {
    let crlf = "\\documentclass{article}\r\n\\begin{document}\r\nOne.\r\nTwo.\r\nThree.\r\n\
                \\end{document}\r\n";
    let f = fixture("a12-crlf", &[("main.tex", "x\n"), ("ch.tex", crlf)]);
    // The agent edits ch.tex while it is not open.
    f.edit("ch.tex", "Two.", "Deux.");
    assert_eq!(f.disk("ch.tex").matches("\r\n").count(), 6);
    // The user opens it and rejects the change there.
    f.open("ch.tex");
    review_ui::decide_document(&f.state, &f.doc("ch.tex"), Decision::Reject);
    common::pump_until(TIMEOUT, || f.disk("ch.tex").contains("Two."));
    common::pump_for(Duration::from_millis(100));
    let disk = f.disk("ch.tex");
    assert_eq!(
        disk,
        crlf,
        "the CRLF line ends were rewritten ({} CRLF left)",
        disk.matches("\r\n").count()
    );
}

fn latin1_closed_save() {
    let f = fixture("a12-latin1", &[("main.tex", "x\n")]);
    let original: &[u8] = b"\\documentclass{article}\n\\usepackage[latin1]{inputenc}\n\
                            \\begin{document}\nCaf\xe9 one.\nTwo.\n\\end{document}\n";
    std::fs::write(f.path("l1.tex"), original).unwrap();
    let editor = f.open("l1.tex");
    f.edit("l1.tex", "Two.", "Deux.");
    let bytes = std::fs::read(f.path("l1.tex")).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"Caf\xe9"),
        "precondition: the open document saves in Latin-1"
    );
    // Close the tab with the change pending, then reject it from the Changes panel.
    f.state.close_editor(&editor, false);
    common::pump_for(Duration::from_millis(100));
    review_ui::decide_document(&f.state, &f.doc("l1.tex"), Decision::Reject);
    common::pump_until(TIMEOUT, || f.disk("l1.tex").contains("Two."));
    let bytes = std::fs::read(f.path("l1.tex")).unwrap();
    assert_eq!(
        bytes,
        original,
        "the closed save re-encoded the Latin-1 file as UTF-8: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

// ----- A5: the header band height depends on whether bar 0 was visible -----

fn header_height_stable() {
    let f = fixture("a5", &[("main.tex", &lines(30))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    let review = editor.view().review_state().unwrap();
    let first = review.header_height();
    f.frames(2);
    // Any later recompute (the next keystroke, the next agent edit) re-plans the bands.
    review_ui::recompute(&f.doc("main.tex"));
    let later = review.header_height();
    let bars = review.visible_bars();
    assert_eq!(
        first, later,
        "the header band changed height between recomputes (bars {bars:?}): every hunk's text \
         jumps"
    );
}

// ----- A6: CRLF deleted text gets an extra line -----

fn crlf_phantom_height() {
    let lf = lines(10);
    let crlf = lf.replace('\n', "\r\n");
    let f = fixture("a6", &[("lf.tex", &lf), ("crlf.tex", &crlf)]);
    let deleted = |file: &str, old: &str| {
        let editor = f.open(file);
        f.edit(file, old, "");
        f.frames(1);
        let review = editor.view().review_state().unwrap();
        let blocks = review.blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].old_lines, 1);
        blocks[0].deleted
    };
    let lf_height = deleted("lf.tex", "line 5\n");
    let crlf_height = deleted("crlf.tex", "line 5\r\n");
    assert_eq!(
        crlf_height, lf_height,
        "one deleted CRLF line reserves {crlf_height}px, one LF line {lf_height}px"
    );
}

// ----- A7: the phantom of a big deletion is one Pango layout, rebuilt per keystroke -----

fn phantom_layout_cost() {
    // Cost of one keystroke inside a hunk whose deleted text has `n` lines.
    let keystroke_cost = |n: usize, reading: bool| -> (Duration, Duration) {
        let f = fixture(
            &format!("a7-{n}-{reading}"),
            &[("main.tex", &lines(n + 2000))],
        );
        let editor = f.open("main.tex");
        if reading {
            review_ui::set_reading_mode(&f.state, true);
        }
        // The agent replaces an n-line block by one line.
        let block: String = (1001..=1000 + n).map(|i| format!("line {i}\n")).collect();
        let started = Instant::now();
        f.edit("main.tex", &block, "a new summary line\n");
        let edit_time = started.elapsed();
        let hunks = f.hunks("main.tex");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].base_lines.len(), n);
        f.frames(1);
        // One keystroke inside the hunk: the next recompute gives it a new HunkId.
        let buffer = editor.buffer();
        let mut at = buffer.iter_at_line(1000).unwrap();
        at.forward_to_line_end();
        buffer.insert(&mut at, "!");
        let started = Instant::now();
        review_ui::recompute(&f.doc("main.tex"));
        (edit_time, started.elapsed())
    };
    let (_, reading) = keystroke_cost(4000, true);
    let (_, k2000) = keystroke_cost(2000, false);
    let (edit_time, keystroke) = keystroke_cost(4000, false);
    eprintln!(
        "keystroke in a hunk with deleted lines: 2000 -> {k2000:?}, 4000 -> {keystroke:?} \
         (4000 in reading mode {reading:?}); the 4000-line agent edit took {edit_time:?}"
    );
    assert!(
        keystroke < Duration::from_millis(100),
        "one keystroke in a hunk with 4000 deleted lines blocked the main thread for \
         {keystroke:?} (reading mode, which skips the phantom: {reading:?})"
    );
}

// ----- A13: the review gutter's per-line byte_offset is O(n) after a lone CR -----

/// Duration of the next frame (layout, snapshot and render) after everything in the
/// view (text, gutter, renderers) was invalidated.
fn paint_time(f: &Fixture, editor: &Editor) -> Duration {
    use std::cell::Cell;
    let clock = f.state.window.frame_clock().expect("a frame clock");
    let start = Rc::new(Cell::new(None::<Instant>));
    let took = Rc::new(Cell::new(None::<Duration>));
    let s = start.clone();
    let layout = clock.connect_before_paint(move |_| s.set(Some(Instant::now())));
    let (s, t) = (start.clone(), took.clone());
    let after = clock.connect_after_paint(move |_| {
        if let Some(t0) = s.get() {
            t.set(Some(t0.elapsed()));
        }
    });
    editor.view().queue_draw();
    for w in descendants(editor.view()) {
        w.queue_draw();
    }
    common::pump_until(TIMEOUT, || took.get().is_some());
    clock.disconnect(layout);
    clock.disconnect(after);
    took.get().unwrap()
}

fn gutter_lone_cr_cost() {
    let text: String = (0..60_000)
        .map(|i| format!("Line {i} with math $x^{i}$ and text.\n"))
        .collect();
    let f = fixture("a13", &[("main.tex", &text)]);
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(59_950).unwrap());
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
    f.frames(2);
    let plain: Duration = (0..3).map(|_| paint_time(&f, &editor)).min().unwrap();
    // One lone CR near the top, inside a line (an agent edit or a paste can bring one in).
    let mut at = buffer.iter_at_line_offset(3, 4).unwrap();
    buffer.insert(&mut at, "\r");
    f.frames(2);
    let with_cr: Duration = (0..3).map(|_| paint_time(&f, &editor)).min().unwrap();
    eprintln!("paint near the end: plain {plain:?}, after one lone CR {with_cr:?}");
    assert!(
        with_cr < plain * 3 + Duration::from_millis(10),
        "one lone CR made painting the editor take {with_cr:?} instead of {plain:?}"
    );
}

// ----- A14: every plan change re-tags the whole buffer, for every anchor tag ever made -----

fn retag_cost() {
    // 200 hunks over 2400 lines (small enough for the diff to finish within its deadline in a
    // debug build).
    let text: String = (0..2400)
        .map(|i| {
            if i % 12 == 5 {
                "\\lipsum[1-12]\n".to_owned()
            } else {
                format!("Line {i} with math and text.\n")
            }
        })
        .collect();
    let f = fixture("a14", &[("main.tex", &text)]);
    let editor = f.open("main.tex");
    let doc = f.doc("main.tex");
    let buffer = editor.buffer();
    let enter_at_top = || {
        buffer.insert(&mut buffer.iter_at_line(1).unwrap(), "\n");
        let started = Instant::now();
        review_ui::recompute(&doc);
        started.elapsed()
    };
    f.edit("main.tex", "Line 2399 with", "Last line with");
    let before: Duration = (0..3).map(|_| enter_at_top()).min().unwrap();
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    let reply = f.agent(
        1,
        EditorRequest::Edit {
            path: "main.tex".into(),
            old_string: "\\lipsum[1-12]".into(),
            new_string: "\\lipsum[1-3]".into(),
            replace_all: true,
        },
    );
    assert!(!reply.is_error, "{}", reply_text(&reply));
    assert_eq!(f.hunks("main.tex").len(), 200);
    f.frames(1);
    let many: Duration = (0..3).map(|_| enter_at_top()).min().unwrap();
    // Down to one hunk again: the 200 anchor tags are still cleared over the whole buffer.
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    f.edit("main.tex", "Last line with", "Final line with");
    assert_eq!(f.hunks("main.tex").len(), 1);
    let after: Duration = (0..3).map(|_| enter_at_top()).min().unwrap();
    eprintln!(
        "Enter at the top, recompute + sync: 1 hunk (fresh) {before:?}; 200 hunks {many:?}; \
         1 hunk after the 200 were accepted {after:?}"
    );
    assert!(
        after < before * 3 + Duration::from_millis(2),
        "with one hunk left, Enter at the top costs {after:?}, against {before:?} before the \
         200-hunk review created its anchor tags"
    );
}

// ----- A15: the hidden Changes panel rebuilds the plain diff after every recompute -----

fn hidden_panel_plain_diff() {
    let text: String = (0..60_000)
        .map(|i| format!("Line {i} with math $x^{i}$ and text.\n"))
        .collect();
    let f = fixture("a15", &[("main.tex", &text)]);
    f.open("main.tex");
    f.edit("main.tex", "Line 30000 with", "Line thirty thousand with");
    let controller = f.state.review.get().unwrap();
    let panel = controller.changes_panel().unwrap();
    // The refresh that follows every recompute (each debounced keystroke).
    let refresh = || {
        let started = Instant::now();
        controller.refresh();
        started.elapsed()
    };
    let off: Duration = (0..5).map(|_| refresh()).min().unwrap();
    // The user once looked at the plain diff, then hid the sidebar.
    panel.diff_toggle().set_active(true);
    f.state.sidebar.split.set_show_sidebar(false);
    common::pump_for(Duration::from_millis(50));
    assert!(!f.state.sidebar.changes.is_mapped());
    let hidden: Duration = (0..5).map(|_| refresh()).min().unwrap();
    eprintln!("panel refresh: plain diff off {off:?}; plain diff on, sidebar hidden {hidden:?}");
    assert!(
        hidden < off * 5 + Duration::from_millis(1),
        "with the sidebar hidden, every recompute still rebuilds the plain diff: {hidden:?} \
         against {off:?}"
    );
}

// ----- A17: concurrent writes of a new file concatenate -----

fn concurrent_new_file_writes() {
    let f = fixture("a17", &[("main.tex", "x\n")]);
    let ws = f.state.workspace.clone();
    let write = |call: u64, content: &str| {
        glib::spawn_future_local(agent_edits::apply(
            &ws,
            call,
            EditorRequest::Write {
                path: "new.tex".into(),
                content: content.into(),
            },
        ))
    };
    let (a, b) = (write(1, "X\n"), write(2, "Y\n"));
    let ra = block_on(a).unwrap();
    let rb = block_on(b).unwrap();
    let disk = f.disk("new.tex");
    assert!(
        disk == "X\n" || disk == "Y\n",
        "two writes of a new file gave {disk:?} (replies {:?} / {:?})",
        reply_text(&ra),
        reply_text(&rb)
    );
}

// ----- A22: bar labels are hard-coded -----

fn bar_labels_rebound() {
    let f = fixture_with("a22", &[("main.tex", &lines(20))], |c| {
        c.keys
            .bindings
            .insert("win.review-accept".into(), "<Alt>a".into());
    });
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    f.frames(2);
    let review = editor.view().review_state().unwrap();
    let labels: Vec<String> = review
        .bar_widgets()
        .iter()
        .flat_map(descendants)
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .map(|l| l.label().to_string())
        .collect();
    assert!(
        !labels.iter().any(|l| l == "⌥Y"),
        "Accept is bound to Alt+A, but the bars still say ⌥Y: {labels:?}"
    );
}

/// With the default keys the bars name the chords as the platform does: "Alt+Y", not "⌥Y".
fn bar_labels_default() {
    let f = fixture("a22-default", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    f.frames(1);
    let review = editor.view().review_state().unwrap();
    let bar = review.bar_widgets().remove(0);
    let labels: Vec<String> = descendants(&bar)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .map(|l| l.label().to_string())
        .collect();
    assert!(
        labels.iter().any(|l| l == "Alt+Y") && labels.iter().any(|l| l == "Alt+N"),
        "{labels:?}"
    );
    let tooltips: Vec<String> = descendants(&bar)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter_map(|b| b.tooltip_text().map(|t| t.to_string()))
        .collect();
    assert!(
        tooltips.iter().any(|t| t.contains("(Alt+Y)")),
        "{tooltips:?}"
    );
}

// ----- A23: a Reject that cannot save offers no Overwrite -----

fn reject_conflict_overwrite() {
    let f = fixture("a23", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    // Another program changes the file (git stash pop, another editor).
    let external = f.disk("main.tex") + "EXTERNAL\n";
    std::fs::write(f.path("main.tex"), &external).unwrap();
    // GtkSourceView compares modification times in whole seconds.
    std::fs::File::options()
        .write(true)
        .open(f.path("main.tex"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    f.put_cursor(&editor, 4);
    f.action("review-reject");
    common::pump_for(Duration::from_millis(300));
    assert_eq!(f.disk("main.tex"), external, "nothing was overwritten");
    let buttons: Vec<String> = descendants(&f.state.toasts)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter_map(|b| b.label().map(|l| l.to_string()))
        .collect();
    assert!(
        buttons.iter().any(|l| l.contains("Overwrite")),
        "the failed save after Reject offers no Overwrite (toast buttons: {buttons:?})"
    );
}
