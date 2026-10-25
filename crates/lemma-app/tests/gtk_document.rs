//! Document, workspace and saver tests on real GtkSourceView buffers (WP0.2 acceptance 2 and 3).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_document`.
//! `LEMMA_TEST_SEED` and `LEMMA_TEST_OPS` (default 500) change the seed and length of the
//! random-operations test.

mod common;

use gtk::prelude::*;
use lemma_app::document::{self, DocRef, Document, EditError};
use lemma_app::saver::SaveError;
use lemma_app::workspace::Workspace;
use lemma_core::{LineIndex, Origin, ProjectRoot, TextEdit, fnv1a64};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    let mut tests: Vec<common::Test> = vec![
        (
            "random_operations_keep_line_index_and_origins",
            random_operations,
        ),
        ("apply_edits_is_one_undo_step", apply_edits_is_one_undo_step),
        (
            "apply_edits_rejects_bad_edits",
            apply_edits_rejects_bad_edits,
        ),
        ("byte_offsets_and_iters_round_trip", byte_offsets_round_trip),
        ("closed_documents", closed_documents),
        (
            "replace_all_under_review_reports_every_change",
            replace_all_under_review,
        ),
        (
            "nested_apply_edits_reach_the_review_with_their_origin",
            nested_apply_edits_keep_their_origin,
        ),
        ("observers_and_disconnect", observers_and_disconnect),
        ("save_round_trip_is_byte_identical", save_round_trip),
        ("saves_are_coalesced", saves_are_coalesced),
        (
            "external_modification_is_refused",
            external_modification_is_refused,
        ),
        (
            "closed_document_save_and_reopen",
            closed_document_save_and_reopen,
        ),
        (
            "workspace_open_missing_file_and_dedup",
            workspace_open_missing_and_dedup,
        ),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_apply_edits_replace_all", perf_replace_all));
    }
    common::run(&tests)
}

fn open_doc(text: &str) -> DocRef {
    let path = PathBuf::from("/nonexistent/lemma-test/main.tex");
    let buffer = document::new_buffer(&path);
    buffer.set_text(text);
    buffer.set_modified(false);
    Document::new_open(path, buffer, sourceview5::File::new())
}

fn buffer_of(doc: &DocRef) -> sourceview5::Buffer {
    doc.buffer().expect("an open document")
}

fn assert_index_exact(doc: &DocRef, context: &str) {
    let text = doc.text();
    assert_eq!(
        *doc.line_index(),
        LineIndex::new(&text),
        "line index out of sync after {context}"
    );
}

/// Records the origin of every change the document reports.
fn record_origins(doc: &DocRef) -> Rc<RefCell<Vec<Origin>>> {
    let log = Rc::new(RefCell::new(Vec::new()));
    let sink = log.clone();
    doc.connect_edit(move |_, _, origin| sink.borrow_mut().push(origin));
    log
}

const SNIPPETS: &[&str] = &[
    "x",
    "\n",
    "\n\n",
    "é",
    "∑_{i}",
    "𝔸",
    "\\begin{theorem}\n  $x$\n\\end{theorem}\n",
    "word ",
    "% comment\n",
    // CRLF, and with random deletes lone CRs, which GTK counts as line breaks but `\n` lines
    // do not.
    "\r\n",
    "\r",
    "\u{2029}",
];

/// Acceptance 2: 500 random inserts, deletes, undos, redos and agent edits keep
/// `LineIndex == LineIndex::new(buffer text)`; undo/redo sub-edits carry `History`, typing
/// `User`, and `apply_edits` its own origin.
fn random_operations() {
    let seed = std::env::var("LEMMA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_1e44a);
    let ops = std::env::var("LEMMA_TEST_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500u64);
    println!("  seed {seed}, {ops} operations");
    let fixture = std::fs::read_to_string(common::fixtures().join("proj/main.tex"))
        .expect("read the fixture");
    let doc = open_doc(&fixture);
    let buffer = buffer_of(&doc);
    let origins = record_origins(&doc);
    let line_count = Rc::new(Cell::new(doc.line_index().line_count()));
    let count = line_count.clone();
    doc.connect_lines_changed(move |_, removed, added| {
        count.set(count.get() - removed + added);
    });
    let mut rng = common::Rng::new(seed);
    let (mut undos, mut redos, mut history_edits) = (0, 0, 0);

    for step in 0..ops {
        origins.borrow_mut().clear();
        let chars = buffer.char_count();
        let expected = match rng.below(10) {
            0..=3 => {
                let mut at = buffer.iter_at_offset(rng.below(chars as u64 + 1) as i32);
                let text = SNIPPETS[rng.below(SNIPPETS.len() as u64) as usize];
                buffer.begin_user_action();
                buffer.insert(&mut at, text);
                buffer.end_user_action();
                Some(Origin::User)
            }
            4..=5 => {
                let a = rng.below(chars as u64 + 1) as i32;
                let b = (a + rng.below(40) as i32).min(chars);
                let (mut s, mut e) = (buffer.iter_at_offset(a), buffer.iter_at_offset(b));
                buffer.begin_user_action();
                buffer.delete(&mut s, &mut e);
                buffer.end_user_action();
                Some(Origin::User)
            }
            6..=7 => {
                let can = buffer.can_undo();
                buffer.undo();
                undos += usize::from(can);
                Some(Origin::History)
            }
            8 => {
                let can = buffer.can_redo();
                buffer.redo();
                redos += usize::from(can);
                Some(Origin::History)
            }
            _ => {
                let text = doc.text();
                let len = text.len();
                let mut cut = rng.below(len as u64 + 1) as usize;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let edits = [
                    TextEdit::insert(len, "% agent\n"),
                    TextEdit {
                        range: cut..cut,
                        text: "∂".into(),
                    },
                ];
                let call = step;
                doc.apply_edits(&edits, Origin::Agent { call })
                    .expect("valid edits");
                Some(Origin::Agent { call })
            }
        };
        assert_index_exact(&doc, &format!("step {step}"));
        assert_eq!(
            line_count.get(),
            doc.line_index().line_count(),
            "lines_changed, step {step}"
        );
        if let Some(expected) = expected {
            let seen = origins.borrow();
            assert!(
                seen.iter().all(|o| *o == expected),
                "step {step}: expected only {expected:?}, saw {seen:?}"
            );
            if expected == Origin::History {
                history_edits += seen.len();
            }
        }
        assert_eq!(
            doc.origin(),
            Origin::User,
            "origin restored after step {step}"
        );
    }
    println!("  {undos} undos, {redos} redos, {history_edits} history sub-edits");
    if ops >= 500 {
        assert!(
            undos >= 20 && redos >= 1 && history_edits >= 20,
            "the run exercised history"
        );
    }
}

fn apply_edits_is_one_undo_step() {
    let doc = open_doc("alpha\nbeta\ngamma\n");
    let buffer = buffer_of(&doc);
    let origins = record_origins(&doc);
    // Cursor at the start of "gamma" (byte 11).
    buffer.place_cursor(&buffer.iter_at_offset(11));
    let version = doc.version();
    doc.apply_edits(
        &[
            TextEdit::insert(17, "delta\n"),
            TextEdit {
                range: 6..10,
                text: "BETA".into(),
            },
            TextEdit::delete(0..1),
        ],
        Origin::Agent { call: 7 },
    )
    .unwrap();
    assert_eq!(doc.text(), "lpha\nBETA\ngamma\ndelta\n");
    assert!(doc.version() > version);
    assert!(
        origins
            .borrow()
            .iter()
            .all(|o| *o == Origin::Agent { call: 7 })
    );
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    assert_eq!(doc.byte_offset(&cursor), 10, "the cursor kept its place");
    assert_index_exact(&doc, "agent edits");

    origins.borrow_mut().clear();
    buffer.undo();
    assert_eq!(
        doc.text(),
        "alpha\nbeta\ngamma\n",
        "one undo reverts all edits"
    );
    assert!(!origins.borrow().is_empty());
    assert!(origins.borrow().iter().all(|o| *o == Origin::History));
    assert_index_exact(&doc, "undo");
    buffer.redo();
    assert_eq!(doc.text(), "lpha\nBETA\ngamma\ndelta\n");
    assert_index_exact(&doc, "redo");
    assert_eq!(doc.origin(), Origin::User);

    // with_origin restores the previous origin, also when nested.
    doc.with_origin(Origin::Review, || {
        assert_eq!(doc.origin(), Origin::Review);
        doc.with_origin(Origin::Reload, || assert_eq!(doc.origin(), Origin::Reload));
        assert_eq!(doc.origin(), Origin::Review);
    });
    assert_eq!(doc.origin(), Origin::User);
}

fn apply_edits_rejects_bad_edits() {
    let doc = open_doc("aé\nb");
    let len = doc.text().len();
    let bad = [
        TextEdit::insert(len + 1, "x"),
        TextEdit::delete(2..3), // inside 'é'
        // An inverted range (built at run time; a literal one is a lint error).
        TextEdit {
            range: std::ops::Range { start: 3, end: 1 },
            text: String::new(),
        },
    ];
    for edit in bad {
        assert_eq!(
            doc.apply_edits(std::slice::from_ref(&edit), Origin::User),
            Err(EditError::InvalidRange(edit.clone()))
        );
    }
    assert_eq!(
        doc.apply_edits(
            &[TextEdit::insert(0, "x"), TextEdit::insert(1, "y")],
            Origin::User
        ),
        Err(EditError::NotDescending)
    );
    assert_eq!(doc.text(), "aé\nb", "nothing was applied");
    assert_eq!(doc.apply_edits(&[], Origin::User), Ok(()));
}

fn byte_offsets_round_trip() {
    // With and without line breaks that only GTK counts (lone CR, U+2029).
    for text in ["a∑b\n\n𝔸x\r\nlast", "a∑b\rc\u{2029}d\n𝔸\r\nend"] {
        let doc = open_doc(text);
        for (offset, _) in text.char_indices().chain([(text.len(), ' ')]) {
            let iter = doc
                .iter_at_byte(offset)
                .unwrap_or_else(|| panic!("{text:?}: no iter at char boundary {offset}"));
            assert_eq!(doc.byte_offset(&iter), offset, "{text:?} at {offset}");
        }
        assert!(doc.iter_at_byte(2).is_none(), "inside '∑'");
        assert!(doc.iter_at_byte(text.len() + 1).is_none());
        doc.apply_edits(
            &[TextEdit::insert(text.len(), "\n∂"), TextEdit::delete(1..4)],
            Origin::User,
        )
        .unwrap();
        assert_index_exact(&doc, text);
    }
}

fn closed_documents() {
    let doc = Document::new_closed(PathBuf::from("/tmp/x/closed.tex"), "one\ntwo\n".into());
    assert!(!doc.is_open());
    assert!(!doc.is_dirty());
    assert!(doc.iter_at_byte(0).is_none());
    let origins = record_origins(&doc);
    doc.apply_edits(
        &[
            TextEdit::insert(8, "three\n"),
            TextEdit {
                range: 0..3,
                text: "ONE".into(),
            },
        ],
        Origin::Agent { call: 1 },
    )
    .unwrap();
    assert_eq!(doc.text(), "ONE\ntwo\nthree\n");
    assert_eq!(origins.borrow().len(), 2);
    assert!(doc.is_dirty());
    assert_index_exact(&doc, "closed edits");
    doc.with_text(|t| assert!(t.ends_with("three\n")));

    // Opening it in a buffer keeps the text and the dirty state, and is not undoable.
    let buffer = document::new_buffer(doc.path());
    doc.open_in_buffer(&buffer);
    assert!(doc.is_open());
    assert_eq!(doc.text(), "ONE\ntwo\nthree\n");
    assert!(doc.is_dirty());
    assert!(!buffer.can_undo());
    buffer.insert_at_cursor("% ");
    assert_index_exact(&doc, "typing after open_in_buffer");

    // Closing the buffer keeps the text as the shadow.
    doc.close_buffer();
    assert!(!doc.is_open());
    assert_eq!(doc.text(), "% ONE\ntwo\nthree\n");
    assert!(doc.is_dirty());
    buffer.insert_at_cursor("ignored");
    assert_eq!(
        doc.text(),
        "% ONE\ntwo\nthree\n",
        "the old buffer is disconnected"
    );
}

/// What a document reported while it changed: the edits (pre-edit offsets) and line deltas.
type Reported = Rc<RefCell<(Vec<TextEdit>, Vec<(u32, u32, u32)>)>>;

fn record_changes(doc: &DocRef) -> Reported {
    let log: Reported = Rc::default();
    let sink = log.clone();
    doc.connect_edit(move |_, edit, _| sink.borrow_mut().0.push(edit.clone()));
    let sink = log.clone();
    doc.connect_lines_changed(move |first, removed, added| {
        sink.borrow_mut().1.push((first, removed, added));
    });
    log
}

/// A replace_all with hundreds of matches in a file under review (agent edits, as the `edit`
/// tool and Replace in Project apply them): the review gets the changes the document reports
/// all at once (`FileReview::on_edits`) and ends as it would change by change; a closed document
/// rebuilds its text and line index once and still reports every change (observers, line
/// deltas, version) as it would one by one; an open one reports a delete and an insert per edit.
fn replace_all_under_review() {
    let text: String = (0..200)
        .map(|i| format!("Line {i}: let $x_{i}$ and $é$ be given.\nPlain {i}.\n"))
        .collect();
    let agent = Origin::Agent { call: 1 };
    for open in [false, true] {
        let doc = if open {
            open_doc(&text)
        } else {
            Document::new_closed(PathBuf::from("/tmp/x/closed.tex"), text.clone())
        };
        // An earlier agent change, already a hunk.
        let mut review = lemma_review::FileReview::new(text.clone());
        review.set_turn(1);
        doc.review.replace(Some(review));
        let at = text.find("Plain 100.").unwrap();
        doc.apply_edits(&[TextEdit::delete(at..at + 6)], agent)
            .unwrap();
        doc.with_text(|t| doc.review.borrow_mut().as_mut().unwrap().recompute(t));
        let before = doc.review.borrow().clone().unwrap();
        let start = doc.text();
        let edits = lemma_review::str_replace(&start, "$", "\\(", true).unwrap();
        assert_eq!(edits.len(), 800);
        let reported = record_changes(&doc);
        let version = doc.version();
        doc.apply_edits(&edits, agent).unwrap();

        let mut want = start.clone();
        lemma_core::apply_all(&mut want, &edits);
        assert_eq!(doc.text(), want);
        assert_index_exact(&doc, "a replace_all");
        let (changes, lines) = reported.borrow().clone();
        let expected: Vec<TextEdit> = if open {
            edits
                .iter()
                .flat_map(|e| {
                    [
                        TextEdit::delete(e.range.clone()),
                        TextEdit::insert(e.range.start, e.text.clone()),
                    ]
                })
                .collect()
        } else {
            edits.clone()
        };
        assert_eq!(changes, expected, "open: {open}");
        assert_eq!(doc.version(), version + changes.len() as u64);
        let mut index = LineIndex::new(&start);
        let deltas: Vec<(u32, u32, u32)> = changes
            .iter()
            .map(|e| {
                let delta = index.line_delta(e);
                index.apply(e);
                delta
            })
            .collect();
        assert_eq!(lines, deltas, "open: {open}");
        // The review: as fed change by change.
        let mut one_by_one = before;
        for e in &changes {
            one_by_one.on_edit(e, agent);
        }
        let mut review = doc.review.borrow_mut();
        let review = review.as_mut().unwrap();
        assert_eq!(review.hunks(), one_by_one.hunks());
        assert_eq!(
            review.to_persisted("x.tex".into(), &want),
            one_by_one.to_persisted("x.tex".into(), &want)
        );
        // Word spans get a time budget (they may differ under load); the rest is exact.
        let key = |r: &lemma_review::FileReview| -> Vec<_> {
            r.hunks()
                .iter()
                .map(|h| {
                    (
                        h.id,
                        h.cur_bytes.clone(),
                        h.base_bytes.clone(),
                        h.turns.clone(),
                    )
                })
                .collect()
        };
        review.recompute(&want);
        one_by_one.recompute(&want);
        assert_eq!(key(review), key(&one_by_one));
        // A hunk per changed line; the earlier change joins the lines around it into one.
        assert_eq!(review.hunks().len(), 199, "open: {open}");
    }
}

/// An `apply_edits` made while another one runs (an `end-user-action` handler's, as the
/// environment rename's when an agent edit closes the user action Vim's insert mode keeps open)
/// reaches the review with its own origin, in order: the user's change goes into the baseline,
/// the agent's becomes a hunk.
fn nested_apply_edits_keep_their_origin() {
    let text = "alpha\nbeta\ngamma\n";
    let doc = open_doc(text);
    doc.review
        .replace(Some(lemma_review::FileReview::new(text.to_owned())));
    let buffer = buffer_of(&doc);
    buffer.begin_user_action();
    let weak = Rc::downgrade(&doc);
    let once = Cell::new(true);
    let handler = buffer.connect_end_user_action(move |_| {
        if let Some(doc) = weak.upgrade()
            && once.replace(false)
        {
            let end = doc.text().len();
            doc.apply_edits(&[TextEdit::insert(end, "user\n")], Origin::User)
                .unwrap();
        }
    });
    doc.apply_edits(
        &[TextEdit {
            range: 0..5,
            text: "ALPHA".into(),
        }],
        Origin::Agent { call: 1 },
    )
    .unwrap();
    buffer.disconnect(handler);
    buffer.end_user_action();
    let current = doc.text();
    assert_eq!(current, "ALPHA\nbeta\ngamma\nuser\n");
    let mut review = doc.review.borrow_mut();
    let review = review.as_mut().unwrap();
    review.recompute(&current);
    assert_eq!(review.baseline(), "alpha\nbeta\ngamma\nuser\n");
    let hunks: Vec<(usize, usize)> = review
        .hunks()
        .iter()
        .map(|h| (h.cur_bytes.start, h.cur_bytes.end))
        .collect();
    assert_eq!(hunks, [(0, 6)]);
}

/// `apply_edits` of a replace_all of `$` with 20 000 matches (5000 lines) in a file under
/// review, closed and open (release builds; run with `cargo test --release -p lemma-app --test
/// gtk_document -- --ignored perf`).
fn perf_replace_all() {
    let text: String = (0..5000)
        .map(|i| format!("Line {i}: let $x_{i}$ and $y$ be given.\nPlain {i}.\n"))
        .collect();
    let edits = lemma_review::str_replace(&text, "$", "\\(", true).unwrap();
    for open in [false, true] {
        let doc = if open {
            open_doc(&text)
        } else {
            Document::new_closed(PathBuf::from("/tmp/x/closed.tex"), text.clone())
        };
        let mut review = lemma_review::FileReview::new(text.clone());
        review.set_turn(1);
        doc.review.replace(Some(review));
        let started = std::time::Instant::now();
        doc.apply_edits(&edits, Origin::Agent { call: 1 }).unwrap();
        let took = started.elapsed();
        eprintln!(
            "    apply_edits of {} matches under review, {}: {took:?}",
            edits.len(),
            if open { "open" } else { "closed" }
        );
        if cfg!(debug_assertions) {
            eprintln!("    debug build: the budget is checked in release builds only");
        } else {
            let budget = Duration::from_millis(if open { 1500 } else { 500 });
            assert!(took < budget, "{took:?}");
        }
    }
}

fn observers_and_disconnect() {
    let doc = open_doc("x\n");
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let id = doc.connect_hunks_changed(move || c.set(c.get() + 1));
    doc.notify_hunks_changed();
    doc.notify_hunks_changed();
    assert_eq!(calls.get(), 2);
    doc.disconnect(id);
    doc.notify_hunks_changed();
    assert_eq!(calls.get(), 2);

    let lines = Rc::new(RefCell::new(Vec::new()));
    let l = lines.clone();
    doc.connect_lines_changed(move |first, removed, added| {
        l.borrow_mut().push((first, removed, added))
    });
    buffer_of(&doc).insert(&mut buffer_of(&doc).end_iter(), "a\nb\n");
    assert_eq!(*lines.borrow(), [(1, 1, 3)]);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    glib::MainContext::default().block_on(future)
}

fn workspace(dir: &Path) -> Rc<Workspace> {
    Workspace::new(ProjectRoot::new(dir).expect("project root"))
}

/// Acceptance 3: loading with FileLoader and saving with FileSaver writes the file back byte for
/// byte, including a file without a final newline, CRLF line ends and Latin-1.
fn save_round_trip() {
    let dir = common::temp_dir("roundtrip");
    let fixture = std::fs::read(common::fixtures().join("proj/main.tex")).expect("fixture");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("main.tex", fixture),
        ("no-final-newline.tex", "x = ∑_{i} a_i\nlast line".into()),
        ("crlf.tex", b"\\section{A}\r\nline\r\n".to_vec()),
        ("latin1.tex", b"caf\xe9 au lait\n".to_vec()),
        ("empty.tex", Vec::new()),
    ];
    let ws = workspace(&dir);
    for (name, bytes) in cases {
        let path = dir.join(name);
        std::fs::write(&path, &bytes).unwrap();
        let doc = block_on(ws.open(&path)).expect("open");
        assert!(!doc.is_dirty(), "{name}: clean after loading");
        if std::str::from_utf8(&bytes).is_ok_and(|s| !s.contains('\r')) {
            assert_eq!(
                doc.text().as_bytes(),
                bytes,
                "{name}: the buffer holds the file"
            );
        }
        block_on(doc.save()).expect("save");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{name}: unchanged round trip"
        );

        // An edit is written exactly, and the write is recognised as our own.
        doc.apply_edits(&[TextEdit::insert(0, "% edited\n")], Origin::User)
            .unwrap();
        assert!(doc.is_dirty());
        block_on(doc.save()).expect("save after edit");
        assert!(!doc.is_dirty(), "{name}: clean after saving");
        let disk = std::fs::read(&path).unwrap();
        if name != "crlf.tex" && name != "latin1.tex" {
            assert_eq!(
                disk,
                doc.text().as_bytes(),
                "{name}: disk equals the buffer"
            );
            assert!(doc.is_own_write(fnv1a64(&[&disk])), "{name}: own write");
        } else {
            assert!(disk.starts_with(b"% edited"), "{name}");
            assert!(
                disk.ends_with(&bytes),
                "{name}: original encoding and line ends kept"
            );
        }
        assert!(!doc.is_own_write(fnv1a64(&[b"something else"])));
    }
}

fn saves_are_coalesced() {
    let dir = common::temp_dir("coalesce");
    let path = dir.join("doc.tex");
    std::fs::write(&path, "start\n").unwrap();
    let ws = workspace(&dir);
    let doc = block_on(ws.open(&path)).unwrap();
    let mut saves = Vec::new();
    for i in 0..5 {
        let end = doc.text().len();
        doc.apply_edits(
            &[TextEdit::insert(end, format!("line {i}\n"))],
            Origin::User,
        )
        .unwrap();
        saves.push(doc.save());
    }
    let results = block_on(async {
        let mut results = Vec::new();
        for save in saves {
            results.push(save.await);
        }
        results
    });
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), doc.text());
    assert!(!doc.is_dirty());
}

fn external_modification_is_refused() {
    let dir = common::temp_dir("external");
    let path = dir.join("doc.tex");
    std::fs::write(&path, "mine\n").unwrap();
    let ws = workspace(&dir);
    let doc = block_on(ws.open(&path)).unwrap();
    // Another program writes the file later.
    std::fs::write(&path, "theirs\n").unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(later))
        .unwrap();
    doc.apply_edits(&[TextEdit::insert(0, "% ")], Origin::User)
        .unwrap();
    assert_eq!(block_on(doc.save()), Err(SaveError::ExternallyModified));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "theirs\n",
        "nothing written"
    );
    assert!(doc.is_dirty());
    block_on(doc.save_overwriting()).expect("overwrite");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "% mine\n");
    // Later saves work normally again.
    doc.apply_edits(&[TextEdit::insert(0, "%")], Origin::User)
        .unwrap();
    block_on(doc.save()).expect("save after overwrite");
}

fn closed_document_save_and_reopen() {
    let dir = common::temp_dir("closed");
    let path = dir.join("chapter.tex");
    std::fs::write(&path, "\\section{One}\n").unwrap();
    let ws = workspace(&dir);
    let doc = block_on(ws.get_or_load(&path)).expect("load closed");
    assert!(!doc.is_open());
    assert!(Rc::ptr_eq(&doc, &block_on(ws.get_or_load(&path)).unwrap()));
    doc.apply_edits(
        &[TextEdit::insert(14, "Text.\n")],
        Origin::Agent { call: 3 },
    )
    .unwrap();
    assert_eq!(ws.dirty_documents().len(), 1);
    block_on(ws.save_all_dirty()).expect("save all");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\\section{One}\nText.\n"
    );
    assert!(doc.is_own_write(fnv1a64(&[b"\\section{One}\nText.\n"])));
    assert!(!doc.is_dirty());

    // Opening it in an editor reuses the document.
    let opened = block_on(ws.open(&path)).unwrap();
    assert!(Rc::ptr_eq(&doc, &opened));
    assert!(opened.is_open());
    assert_eq!(opened.text(), "\\section{One}\nText.\n");
    // Without a review, releasing the editor drops the document.
    ws.release(&opened);
    assert!(ws.get(&path).is_none());
}

fn workspace_open_missing_and_dedup() {
    let dir = common::temp_dir("missing");
    let ws = workspace(&dir);
    let path = dir.join("new.tex");
    let doc = block_on(ws.open(&path)).expect("a missing file opens empty");
    assert_eq!(doc.text(), "");
    buffer_of(&doc).insert_at_cursor("\\documentclass{article}\n");
    block_on(doc.save()).expect("the first save creates the file");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\\documentclass{article}\n"
    );

    let again = block_on(ws.open(&dir.join("./new.tex"))).unwrap();
    assert!(Rc::ptr_eq(&doc, &again), "paths are normalized");
    assert_eq!(ws.documents().len(), 1);
    let err = block_on(ws.open(&dir)).expect_err("a directory is not a document");
    assert!(err.contains("cannot open"), "{err}");
    let err = block_on(ws.get_or_load(&dir.join("absent.tex"))).expect_err("missing");
    assert!(err.contains("cannot read"), "{err}");
}
