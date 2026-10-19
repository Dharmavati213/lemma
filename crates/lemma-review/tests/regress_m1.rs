//! Regression tests for review-core defects found in the M1 verification (area A).
//!
//! Each test reproduces one confirmed finding and fails until it is fixed.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit};
use lemma_review::{FileReview, ReviewEvent};
use std::path::Path;
use std::time::{Duration, Instant};

fn large() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/large.tex");
    std::fs::read_to_string(path).expect("tests/fixtures/large.tex")
}

// ---- A1: the user's own undo next to a hunk end is taken for an agent edit -------------------

/// `dd` on the blank line right after a hunk, then `u`: both are the user's own edits outside the
/// hunk (§10.3 Inside rule), so the baseline must get the blank line back, no `UndoneByUser` may
/// be queued, and rejecting the hunk must keep the restored line.
#[test]
fn regress_user_undo_after_hunk_end_blank_line() {
    let base = "Intro.\nOld claim.\n\nNext paragraph.\n";
    let mut s = Session::new(base);
    s.agent_replace("Old claim.", "New claim.");
    let end = s.hunks()[0].cur_bytes.end;
    assert_eq!(
        &s.text[end..end + 1],
        "\n",
        "the blank line follows the hunk"
    );
    // dd on the blank line (outside the hunk), then the debounced recompute.
    s.user(TextEdit::delete(end..end + 1));
    s.recompute();
    // u: GTK's undo re-inserts the blank line (History).
    s.edit(&TextEdit::insert(end, "\n"), Origin::History);
    s.recompute();
    s.check();
    let events = s.review.drain_events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ReviewEvent::UndoneByUser { .. })),
        "the agent is told the user undid its change: {events:?}"
    );
    assert_eq!(
        s.review.baseline(),
        base,
        "the user's undo of their own edit must reach the baseline"
    );
    let [h] = s.hunks() else {
        panic!("{:?}", s.hunks())
    };
    assert_eq!(
        h.old_text, "Old claim.\n",
        "the hunk grew over the user's line"
    );
    let id = h.id;
    s.reject(id);
    assert_eq!(
        s.text, base,
        "Reject deleted the blank line the user had restored"
    );
}

/// `x` on the first character of the line after a hunk, then `u`: the restored character must not
/// become part of the agent's proposal.
#[test]
fn regress_user_undo_after_hunk_end_one_char() {
    let base = "\\section{A}\nold text\nthe end\n";
    let mut s = Session::new(base);
    s.agent_replace("old text", "new text");
    let end = s.hunks()[0].cur_bytes.end;
    assert_eq!(&s.text[end..end + 1], "t");
    s.user(TextEdit::delete(end..end + 1));
    s.recompute();
    s.edit(&TextEdit::insert(end, "t"), Origin::History);
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), base);
    let id = s.hunks()[0].id;
    s.reject(id);
    assert_eq!(s.text, base, "Reject removed the user's restored character");
}

/// `dd` on the line just above a hunk, then `u`: GTK puts the line back at the hunk's start,
/// which the Inside rule alone would make part of the agent's proposal (and Reject would then
/// delete it). The user's own undo goes back into the baseline.
#[test]
fn regress_user_undo_before_hunk_start() {
    let base = "Intro.\nKeep me.\nOld claim.\nEnd.\n";
    let mut s = Session::new(base);
    s.agent_replace("Old claim.", "New claim.");
    let start = s.hunks()[0].cur_bytes.start;
    let line = start - "Keep me.\n".len()..start;
    s.user(TextEdit::delete(line.clone()));
    s.recompute();
    s.edit(&TextEdit::insert(line.start, "Keep me.\n"), Origin::History);
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), base);
    let [h] = s.hunks() else {
        panic!("{:?}", s.hunks())
    };
    assert!(
        !h.user_touched,
        "the user's undo counted as editing the hunk"
    );
    assert_eq!(h.old_text, "Old claim.\n");
    let id = h.id;
    s.reject(id);
    assert_eq!(
        s.text, base,
        "Reject deleted the line the user had restored"
    );
}

/// Ctrl+Z of an agent deletion too large to be recorded (over 256 KiB) still removes its hunk:
/// GTK re-inserts exactly the deleted text at the deletion point.
#[test]
fn regress_undo_of_unrecorded_agent_deletion_resolves_the_hunk() {
    let block: String = (0..14_000)
        .map(|i| format!("line {i} of the block\n"))
        .collect();
    assert!(block.len() > 256 * 1024);
    let base = format!("intro\n{block}outro\n");
    let mut s = Session::new(&base);
    s.edit(
        &TextEdit::delete(6..6 + block.len()),
        Origin::Agent { call: 1 },
    );
    s.recompute();
    assert_eq!(s.hunks().len(), 1);
    s.edit(&TextEdit::insert(6, block.clone()), Origin::History);
    s.recompute();
    s.check();
    assert_eq!(s.hunks(), [], "the undone deletion left a hunk");
    assert_eq!(
        s.review.baseline(),
        base,
        "the baseline took the text twice"
    );
    let events = s.review.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReviewEvent::UndoneByUser { .. })),
        "{events:?}"
    );
}

/// `dd` + `u` right at an agent's pure deletion, of a line equal to the deleted one: the user
/// undoes their own deletion, which must reach the baseline (not count as undoing the agent's).
#[test]
fn regress_user_undo_at_a_pure_deletion_of_the_same_text() {
    let base = "A\nX\nX\nD\n";
    let mut s = Session::new(base);
    // The agent deletes one "X" line: a pure deletion right after the other one.
    s.edit(&TextEdit::delete(2..4), Origin::Agent { call: 1 });
    s.recompute();
    let point = s.hunks()[0].cur_bytes.clone();
    assert!(point.is_empty());
    let line = point.start - 2..point.start;
    assert_eq!(&s.text[line.clone()], "X\n");
    // dd on the remaining "X" line, which ends at the deletion point; then u.
    s.user(TextEdit::delete(line.clone()));
    s.recompute();
    s.edit(&TextEdit::insert(line.start, "X\n"), Origin::History);
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), base);
    let events = s.review.drain_events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ReviewEvent::UndoneByUser { .. })),
        "{events:?}"
    );
    let id = s.hunks()[0].id;
    s.reject(id);
    assert_eq!(s.text, base, "Reject lost the line the user restored");
}

// ---- A8: the 8 ms Myers deadline collapses distant hunks ---------------------------------------

/// Two agent edits far apart in `large.tex` (60k lines) are two hunks of one line each, not one
/// hunk spanning the 48k lines between them.
#[test]
fn regress_distant_edits_in_large_file_stay_separate_hunks() {
    let text = large();
    let mut s = Session::new(&text);
    s.agent_replace("\\label{sec:250}", "\\label{sec:250-agent}");
    s.agent_replace("\\label{sec:2250}", "\\label{sec:2250-agent}");
    s.check();
    let shape: Vec<(std::ops::Range<u32>, usize)> = s
        .hunks()
        .iter()
        .map(|h| (h.cur_lines.clone(), h.old_text.len()))
        .collect();
    assert_eq!(
        shape.len(),
        2,
        "the diff deadline merged both edits into one hunk: {shape:?}"
    );
    assert!(shape.iter().all(|(lines, _)| lines.len() == 1), "{shape:?}");
}

/// The same texts must give the same hunks whatever the machine load: a user edit far from both
/// agent edits stays outside every hunk, so rejecting the agent's changes keeps it.
#[test]
fn regress_user_text_between_distant_hunks_survives_reject() {
    let text = large();
    let mut s = Session::new(&text);
    s.agent_replace("\\label{sec:250}", "\\label{sec:250-agent}");
    s.agent_replace("\\label{sec:2250}", "\\label{sec:2250-agent}");
    // The user types a sentence at line 30000, far from both changes.
    let at = s.text.match_indices('\n').nth(29_999).unwrap().0 + 1;
    let class = s
        .review
        .on_edit(&TextEdit::insert(at, "My own sentence.\n"), Origin::User);
    TextEdit::insert(at, "My own sentence.\n").apply(&mut s.text);
    s.recompute();
    assert_eq!(
        class,
        lemma_review::EditClass::Outside,
        "typing 24k lines away from any agent change counts as inside a hunk"
    );
    s.reject_all();
    assert!(
        s.text.contains("My own sentence.\n"),
        "rejecting the agent's changes discarded the user's sentence"
    );
}

// ---- A9: replace_all is cubic in the number of matches ----------------------------------------

/// Feeds `n` identical one-byte insertions (a replace_all, descending as `str_replace` returns
/// them) through `on_edit` and returns the time it took.
fn replace_all_cost(n: usize) -> Duration {
    let text: String = (0..n)
        .map(|i| format!("Line {i} with math $x$ and text.\n"))
        .collect();
    let edits = lemma_review::str_replace(&text, "with math", "with maths", true).unwrap();
    assert_eq!(edits.len(), n);
    let mut review = FileReview::new(text);
    let started = Instant::now();
    for e in &edits {
        review.on_edit(e, Origin::Agent { call: 1 });
    }
    started.elapsed()
}

/// A replace_all with 2000 matches (a macro rename across a thesis) must not block the main
/// thread: `on_edit` for all of them stays far below a second even in a debug build.
#[test]
fn regress_replace_all_on_edit_is_not_cubic() {
    // Warm up (allocator, page faults).
    replace_all_cost(100);
    let small = replace_all_cost(1000);
    let big = replace_all_cost(2000);
    eprintln!("replace_all on_edit: 1000 matches {small:?}, 2000 matches {big:?}");
    assert!(
        big < Duration::from_millis(500),
        "on_edit for 2000 replace_all matches took {big:?} (1000 took {small:?})"
    );
}
