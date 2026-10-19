//! The document's scanner states (WP2.0 item 2): `Document::line_states` stays equal to a fresh
//! `LineStates::rebuild` of the text through typing, undo/redo and agent edits; the retag ranges
//! cover every line whose tokens changed; large documents scan on the jobs thread; contexts
//! follow the text.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_line_states`.
//! `LEMMA_TEST_SEED` and `LEMMA_TEST_OPS` (default 500) change the random-operations test.

mod common;

use gtk::prelude::*;
use lemma_app::document::{self, ASYNC_SCAN_LINES, DocRef, Document};
use lemma_app::workspace::Workspace;
use lemma_core::{LineIndex, Origin, ProjectRoot, TextEdit};
use lemma_syntax::{Context, LineStates, ScanConfig, StrLines, Token};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        (
            "random_edits_undos_and_agent_edits_keep_line_states_exact",
            random_operations,
        ),
        (
            "large_documents_scan_on_the_jobs_thread",
            large_document_scans_in_the_background,
        ),
        ("context_at_follows_the_text", context_at_follows_the_text),
        ("scan_config_changes_rebuild_and_retag", scan_config_changes),
        (
            "closed_documents_have_no_states_until_opened",
            closed_and_reopened,
        ),
        ("change_and_apply_observers", change_and_apply_observers),
        (
            "keystrokes_in_a_large_document_stay_incremental",
            large_document_keystrokes,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn open_doc(text: &str) -> DocRef {
    let path = PathBuf::from("/nonexistent/lemma-test/main.tex");
    let buffer = document::new_buffer(&path);
    buffer.set_text(text);
    buffer.set_modified(false);
    Document::new_open(path, buffer, sourceview5::File::new())
}

/// Fresh states of `text` with `cfg`.
fn fresh(text: &str, cfg: &Arc<ScanConfig>) -> LineStates {
    let index = LineIndex::new(text);
    let mut states = LineStates::new(cfg.clone());
    states.rebuild(&StrLines {
        text,
        index: &index,
    });
    states
}

fn assert_exact(doc: &Document, context: &str) {
    let text = doc.text();
    let states = doc.line_states.borrow();
    let states = states
        .as_ref()
        .unwrap_or_else(|| panic!("no line states after {context}"));
    assert!(
        *states == fresh(&text, &doc.scan_config()),
        "line states differ from a fresh scan after {context}"
    );
}

/// Every line's text and tokens.
type LineTokens = Vec<(String, Vec<Token>)>;

/// Every line's text and tokens, as the document's states give them.
fn tokens_of(doc: &Document) -> LineTokens {
    let states = doc.line_states.borrow();
    let states = states.as_ref().expect("line states");
    let lines = doc.line_index().line_count();
    (0..lines)
        .map(|i| {
            let text = doc.line_text(i).expect("a line");
            let mut tokens = Vec::new();
            states.tokens(i, &text, &mut tokens);
            (text, tokens)
        })
        .collect()
}

/// Insertions that change the scanner state across lines.
const SNIPPETS: &[&str] = &[
    "x",
    "$",
    "$$",
    "\\[",
    "\\]",
    "\\(",
    "{",
    "}",
    "\n",
    "\n\n",
    "%",
    "% comment $\n",
    "\\begin{equation}\n",
    "\\end{equation}\n",
    "\\begin{align*}\n  a &= b \\\\\n",
    "\\begin{verbatim}\n",
    "\\end{verbatim}\n",
    "\\verb|$|",
    "\\text{ $y$ }",
    "\\label{eq:x}",
    "é∑",
    "\r\n",
    // GTK counts these as line breaks and `\n` lines do not.
    "\r",
    "\u{2029}",
];

/// Item 2's test: 500 random typing, deletion, undo, redo and agent operations. After **every**
/// change (each sub-edit of an undo or an agent action included) the states equal a fresh
/// rebuild, and every line outside the reported retag range kept its text and tokens.
fn random_operations() {
    let seed = std::env::var("LEMMA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5ca9_57a7e);
    let ops = std::env::var("LEMMA_TEST_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500u64);
    println!("  seed {seed}, {ops} operations");
    let fixture = std::fs::read_to_string(common::fixtures().join("proj/main.tex"))
        .expect("read the fixture");
    let doc = open_doc(&fixture);
    let buffer = doc.buffer().expect("open");
    assert_exact(&doc, "opening");

    // Checked inside the change pipeline, after every single change.
    let retagged: Rc<RefCell<Option<Range<u32>>>> = Rc::default();
    let sink = retagged.clone();
    doc.connect_retag(move |lines| {
        let previous = sink.borrow_mut().replace(lines);
        assert!(previous.is_none(), "one retag per change");
    });
    let before: Rc<RefCell<LineTokens>> = Rc::new(RefCell::new(tokens_of(&doc)));
    let delta: Rc<Cell<Option<(u32, u32, u32)>>> = Rc::default();
    let sink = delta.clone();
    doc.connect_lines_changed(move |first, removed, added| sink.set(Some((first, removed, added))));
    let changes = Rc::new(Cell::new(0usize));
    let (retags, previous, count, delta_cell) = (
        retagged.clone(),
        before.clone(),
        changes.clone(),
        delta.clone(),
    );
    doc.connect_changed(move |doc, _, _| {
        count.set(count.get() + 1);
        let n = count.get();
        assert_exact(doc, &format!("change {n}"));
        let range = retags.borrow_mut().take().expect("a retag range");
        let (first, removed, added) = delta_cell.take().expect("a line change");
        let now = tokens_of(doc);
        let old = previous.replace(now.clone());
        assert!(
            range.start <= first && range.end >= first + added,
            "change {n}: retag {range:?} misses the changed lines {first}+{added}"
        );
        for (line, current) in now.iter().enumerate() {
            let line = line as u32;
            if range.contains(&line) {
                continue;
            }
            let old_line = if line < first {
                line
            } else {
                line + removed - added
            };
            assert_eq!(
                Some(current),
                old.get(old_line as usize),
                "change {n}: line {line} changed outside the retag range {range:?}"
            );
        }
    });

    let mut rng = common::Rng::new(seed);
    let (mut undos, mut agent) = (0, 0);
    for step in 0..ops {
        let chars = buffer.char_count();
        match rng.below(10) {
            0..=3 => {
                let mut at = buffer.iter_at_offset(rng.below(chars as u64 + 1) as i32);
                let text = SNIPPETS[rng.below(SNIPPETS.len() as u64) as usize];
                buffer.begin_user_action();
                buffer.insert(&mut at, text);
                buffer.end_user_action();
            }
            4..=5 => {
                let a = rng.below(chars as u64 + 1) as i32;
                let b = (a + rng.below(60) as i32).min(chars);
                let (mut s, mut e) = (buffer.iter_at_offset(a), buffer.iter_at_offset(b));
                buffer.begin_user_action();
                buffer.delete(&mut s, &mut e);
                buffer.end_user_action();
            }
            6..=7 => {
                undos += usize::from(buffer.can_undo());
                buffer.undo();
            }
            8 => buffer.redo(),
            _ => {
                let text = doc.text();
                let mut cut = rng.below(text.len() as u64 + 1) as usize;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                let mut end = (cut + rng.below(80) as usize).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let mut edits = vec![TextEdit {
                    range: cut..end,
                    text: "\\begin{equation}\n  x = $y\n".into(),
                }];
                let mut early = 3.min(text.len());
                while !text.is_char_boundary(early) {
                    early -= 1;
                }
                if cut > early {
                    edits.push(TextEdit::insert(early, "% agent {\n"));
                }
                doc.apply_edits(&edits, Origin::Agent { call: step })
                    .expect("valid edits");
                agent += 1;
            }
        }
        assert_exact(&doc, &format!("step {step}"));
    }
    println!(
        "  {} changes checked, {undos} undos, {agent} agent edits",
        changes.get()
    );
    if ops >= 500 {
        assert!(
            undos >= 50 && agent >= 20,
            "the run exercised history and agents"
        );
    }
}

fn large_text(lines: u32) -> String {
    let source = std::fs::read_to_string(common::fixtures().join("large.tex")).expect("large.tex");
    let mut text: String = source.split_inclusive('\n').take(lines as usize).collect();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn large_document_scans_in_the_background() {
    let text = large_text(ASYNC_SCAN_LINES + 2_000);
    let doc = open_doc(&text);
    let retags: Rc<RefCell<Vec<Range<u32>>>> = Rc::default();
    let sink = retags.clone();
    doc.connect_retag(move |lines| sink.borrow_mut().push(lines));
    assert!(
        !doc.has_line_states(),
        "a document of {} lines scans on the jobs thread",
        ASYNC_SCAN_LINES + 2_000
    );
    let buffer = doc.buffer().unwrap();
    assert_eq!(
        doc.context_at(&buffer.start_iter()),
        Context::Text,
        "text context while the scan runs"
    );
    // Edits while the scan runs: an unclosed display at the top, deletions further down.
    buffer.insert(&mut buffer.start_iter(), "\\[\n x \n");
    let mut at = buffer.iter_at_line(5000).unwrap();
    buffer.insert(&mut at, "\\begin{equation}\n");
    let (mut a, mut b) = (
        buffer.iter_at_line(9000).unwrap(),
        buffer.iter_at_line(9040).unwrap(),
    );
    buffer.delete(&mut a, &mut b);
    doc.apply_edits(
        &[TextEdit::insert(0, "% agent\n")],
        Origin::Agent { call: 1 },
    )
    .unwrap();
    common::pump_until(TIMEOUT, || doc.has_line_states());
    assert_exact(&doc, "the background scan and the edits made meanwhile");
    let lines = doc.line_index().line_count();
    assert_eq!(
        retags.borrow().last(),
        Some(&(0..lines)),
        "the finished scan retags every line"
    );
    // Typing afterwards is incremental again.
    retags.borrow_mut().clear();
    buffer.insert(&mut buffer.iter_at_line(100).unwrap(), "$");
    assert_exact(&doc, "typing after the scan");
    let retagged = retags.borrow();
    assert_eq!(retagged.len(), 1);
    assert!(
        retagged[0].start == 100 && retagged[0].len() < 1000,
        "{retagged:?}"
    );

    // Closing during a scan drops it; reopening scans again.
    let doc = open_doc(&text);
    assert!(!doc.has_line_states());
    doc.close_buffer();
    common::pump_for(Duration::from_millis(300));
    assert!(!doc.has_line_states(), "a closed document has no states");
    let buffer = document::new_buffer(doc.path());
    doc.open_in_buffer(&buffer);
    common::pump_until(TIMEOUT, || doc.has_line_states());
    assert_exact(&doc, "reopening a large document");
}

fn context_at_follows_the_text() {
    let doc = open_doc("a $x$ b\n\\begin{equation}\n  z\n\\end{equation}\n% c $\nend\n");
    let buffer = doc.buffer().unwrap();
    let at = |line: i32, col: i32| {
        let iter = buffer.iter_at_line_offset(line, col).unwrap();
        doc.context_at(&iter)
    };
    assert_eq!(at(0, 1), Context::Text);
    assert!(at(0, 3).is_math(), "inside $x$: {:?}", at(0, 3));
    assert_eq!(at(0, 6), Context::Text);
    assert!(at(2, 3).is_math(), "inside the equation: {:?}", at(2, 3));
    assert_eq!(at(4, 2), Context::Comment);
    assert_eq!(at(5, 1), Context::Text);
    // `context_at_byte` agrees with the iter version.
    let iter = buffer.iter_at_line_offset(2, 3).unwrap();
    assert_eq!(doc.context_at_byte(doc.byte_offset(&iter)), at(2, 3));

    // Opening a display at the top makes the following text math (until the blank line rule).
    buffer.insert(&mut buffer.start_iter(), "\\[\n");
    assert!(at(1, 1).is_math(), "after \\[: {:?}", at(1, 1));
    assert_exact(&doc, "the display");
    assert_eq!(doc.line_text(0).as_deref(), Some("\\["));
    assert_eq!(doc.line_text(1).as_deref(), Some("a $x$ b"));
    assert_eq!(doc.line_text(99), None);

    // CRLF lines keep their `\r` in the scanner's line text, as in `StrLines`.
    let doc = open_doc("x\r\n$y$\r\n");
    assert_eq!(doc.line_text(0).as_deref(), Some("x\r"));
    assert_exact(&doc, "CRLF text");
    let buffer = doc.buffer().unwrap();
    let iter = buffer.iter_at_line_offset(1, 1).unwrap();
    assert!(doc.context_at(&iter).is_math());
}

fn scan_config_changes() {
    let doc = open_doc("\\begin{mathpar}\n  x\n\\end{mathpar}\n");
    let buffer = doc.buffer().unwrap();
    let inside = || doc.context_at(&buffer.iter_at_line_offset(1, 2).unwrap());
    assert_eq!(inside(), Context::Text, "mathpar is not math by default");
    let retags: Rc<RefCell<Vec<Range<u32>>>> = Rc::default();
    let sink = retags.clone();
    doc.connect_retag(move |lines| sink.borrow_mut().push(lines));
    let mut cfg = ScanConfig::default();
    cfg.math_envs.push("mathpar".into());
    doc.set_scan_config(Arc::new(cfg.clone()));
    assert!(inside().is_math(), "{:?}", inside());
    assert_exact(&doc, "the new rules");
    assert_eq!(retags.borrow().len(), 1);
    assert_eq!(retags.borrow()[0], 0..4);
    // The same rules again change nothing.
    doc.set_scan_config(Arc::new(cfg));
    assert_eq!(retags.borrow().len(), 1);
}

fn closed_and_reopened() {
    let dir = common::temp_dir("line-states-closed");
    std::fs::write(dir.join("ch.tex"), "$x\ny$\n").unwrap();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let mut cfg = ScanConfig::default();
    cfg.math_envs.push("mathpar".into());
    ws.set_scan_config(Arc::new(cfg));
    let doc = glib::MainContext::default()
        .block_on(ws.get_or_load(&dir.join("ch.tex")))
        .unwrap();
    assert!(!doc.is_open() && !doc.has_line_states());
    assert_eq!(doc.context_at_byte(1), Context::Text, "no states: text");
    assert_eq!(doc.line_text(1).as_deref(), Some("y$"));
    let opened = glib::MainContext::default()
        .block_on(ws.open(&dir.join("ch.tex")))
        .unwrap();
    assert!(Rc::ptr_eq(&doc, &opened));
    assert!(doc.has_line_states());
    assert!(
        doc.scan_config().math_envs.iter().any(|e| e == "mathpar"),
        "documents get the workspace's rules"
    );
    assert_exact(&doc, "opening a closed document");
    assert!(doc.context_at_byte(1).is_math());
    doc.close_buffer();
    assert!(!doc.has_line_states());

    // A document opened from disk gets the workspace's rules too.
    std::fs::write(dir.join("other.tex"), "x\n").unwrap();
    let other = glib::MainContext::default()
        .block_on(ws.open(&dir.join("other.tex")))
        .unwrap();
    assert!(other.scan_config().math_envs.iter().any(|e| e == "mathpar"));
    assert_exact(&other, "a document opened from disk");
}

fn change_and_apply_observers() {
    let doc = open_doc("alpha\nbeta\n");
    let buffer = doc.buffer().unwrap();
    let changed: Rc<RefCell<Vec<(TextEdit, Origin, String)>>> = Rc::default();
    let sink = changed.clone();
    doc.connect_changed(move |doc, edit, origin| {
        sink.borrow_mut().push((edit.clone(), origin, doc.text()));
    });
    let applied: Rc<RefCell<Vec<(usize, Origin, String)>>> = Rc::default();
    let sink = applied.clone();
    let id = doc.connect_before_apply(move |doc, edits, origin| {
        sink.borrow_mut().push((edits.len(), origin, doc.text()));
    });

    buffer.insert(&mut buffer.end_iter(), "g");
    assert_eq!(
        changed.borrow().last().cloned(),
        Some((
            TextEdit::insert(11, "g"),
            Origin::User,
            "alpha\nbeta\ng".to_owned()
        )),
        "after the change, with pre-edit offsets"
    );
    assert!(applied.borrow().is_empty(), "typing is not an apply_edits");

    doc.apply_edits(
        &[TextEdit::insert(6, "B"), TextEdit::delete(0..1)],
        Origin::Review,
    )
    .unwrap();
    assert_eq!(
        *applied.borrow(),
        [(2, Origin::Review, "alpha\nbeta\ng".to_owned())],
        "before anything is applied"
    );
    let seen = changed.borrow();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1].1, Origin::Review);
    assert_eq!(seen[2].2, "lpha\nBbeta\ng");
    drop(seen);

    // Refused edits and empty lists are not announced.
    assert!(
        doc.apply_edits(&[TextEdit::delete(0..99)], Origin::Review)
            .is_err()
    );
    doc.apply_edits(&[], Origin::Review).unwrap();
    assert_eq!(applied.borrow().len(), 1);
    doc.disconnect(id);
    doc.apply_edits(&[TextEdit::insert(0, "x")], Origin::Agent { call: 2 })
        .unwrap();
    assert_eq!(applied.borrow().len(), 1, "disconnected");
}

/// Typing in the 60k-line fixture rescans a few lines per keystroke, never the document: the
/// p99 time of a keystroke (the whole change pipeline) stays far below a full scan's.
fn large_document_keystrokes() {
    let text = std::fs::read_to_string(common::fixtures().join("large.tex")).expect("large.tex");
    let doc = open_doc(&text);
    common::pump_until(TIMEOUT, || doc.has_line_states());
    let buffer = doc.buffer().unwrap();
    let full = {
        let started = std::time::Instant::now();
        let _ = fresh(&doc.text(), &doc.scan_config());
        started.elapsed()
    };
    let mut times = Vec::new();
    let mut line = 30_000;
    for (i, c) in "word $x^2$ {a} \\[ y \\] % c\n"
        .chars()
        .cycle()
        .take(300)
        .enumerate()
    {
        if i % 60 == 0 {
            line += 7;
        }
        let mut at = buffer.iter_at_line(line).unwrap();
        let started = std::time::Instant::now();
        buffer.insert(&mut at, &c.to_string());
        times.push(started.elapsed());
    }
    assert_exact(&doc, "typing in large.tex");
    times.sort();
    let p50 = times[times.len() / 2];
    let p99 = times[times.len() * 99 / 100];
    println!("  keystroke p50 {p50:?}, p99 {p99:?}; a full scan takes {full:?}");
    assert!(
        p99 * 4 < full,
        "a keystroke ({p99:?} p99) costs about as much as a full scan ({full:?})"
    );
}
