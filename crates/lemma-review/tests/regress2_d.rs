//! Regression tests for verified review defects of the M2 review pass (area D: review turns,
//! persistence). Each test reproduces one defect and fails until it is fixed.
//!
//! `cargo test -p lemma-review --test regress2_d`; the timing tests are ignored by default and
//! run with `cargo test --release -p lemma-review --test regress2_d -- --ignored`.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit, apply_all};
use lemma_review::{FileReview, UndoAction, str_replace};
use std::time::{Duration, Instant};

/// The turns of every hunk, in order.
fn turns(s: &Session) -> Vec<Vec<u32>> {
    s.hunks().iter().map(|h| h.turns.clone()).collect()
}

/// The text `reject_turn(turn)` leaves (applied to a copy).
fn rejected_turn(s: &Session, turn: u32) -> String {
    let mut review = s.review.clone();
    let mut text = s.text.clone();
    apply_all(&mut text, &review.reject_turn(turn, &s.text));
    text
}

// ---- D-1: Reject Turn reverts another turn's work across blank lines --------------------------

/// Turn 1 removes a doubled blank line; turn 2 adds a paragraph two lines above it. The two edits
/// never touch, yet the line diff pairs them through the shared blank lines into hunks labelled
/// with one turn each, and rejecting turn 2 duplicates a paragraph instead of giving back the
/// text after turn 1 (the contract: "`reject_turn(2)` restores exactly the text after turn
/// 1 wherever turn 2 did not touch turn-1 hunks").
#[test]
fn regress2_reject_turn_duplicates_paragraph() {
    let mut s = Session::new("\\section{Intro}\n\nWe prove the theorem.\n\n\n\\section{Proof}\n");
    s.review.set_turn(1);
    s.agent_replace(
        "theorem.\n\n\n\\section{Proof}",
        "theorem.\n\n\\section{Proof}",
    );
    let t1 = s.text.clone();
    s.review.begin_turn();
    s.review.set_turn(2);
    s.agent_replace(
        "\\section{Intro}\n\n",
        "\\section{Intro}\n\nMotivation first.\n\n",
    );
    s.check();
    eprintln!("hunks {:?} turns {:?}", s.pairs(), turns(&s));
    let rejected = rejected_turn(&s, 2);
    assert_eq!(
        rejected, t1,
        "Reject Turn 2 did not give back the text after turn 1"
    );
}

/// Turn 1 deletes the last paragraph; turn 2 appends a new one after the paragraph before it.
/// The line diff makes one hunk "old remark → future work" labelled turn 2 alone, so rejecting
/// turn 2 brings back the remark turn 1 deleted.
#[test]
fn regress2_reject_turn_restores_other_turns_deletion() {
    let mut s =
        Session::new("\\section{Conclusion}\n\nWe showed X.\n\nThis old remark is wrong.\n");
    s.review.set_turn(1);
    s.agent_replace(
        "We showed X.\n\nThis old remark is wrong.\n",
        "We showed X.\n",
    );
    let t1 = s.text.clone();
    s.review.begin_turn();
    s.review.set_turn(2);
    s.agent_replace("We showed X.\n", "We showed X.\n\nFuture work: Y.\n");
    eprintln!("hunks {:?} turns {:?}", s.pairs(), turns(&s));
    let rejected = rejected_turn(&s, 2);
    assert!(
        !rejected.contains("This old remark is wrong."),
        "Reject Turn 2 brought back the paragraph turn 1 deleted: {rejected:?}"
    );
    assert_eq!(rejected, t1);
}

// ---- D-10: a decision larger than the record cap cannot be undone ---------------------------

/// `n` separate lines, each changed by one replace_all of the agent: `n` hunks.
fn many_changed(n: usize) -> Session {
    let base: String = (0..n)
        .map(|i| format!("Line {i} uses \\R here.\nPlain line {i}.\n"))
        .collect();
    let mut s = Session::new(&base);
    s.review.set_turn(1);
    let edits = str_replace(&s.text, "\\R", "\\mathbb{R}", true).unwrap();
    assert_eq!(edits.len(), n);
    s.edits(&edits, Origin::Agent { call: 1 });
    s.recompute();
    assert_eq!(s.hunks().len(), n);
    s
}

/// Reject all, applied as one `Origin::Review` action and recomputed: the reject edits and the
/// texts they replaced.
fn reject_everything(s: &mut Session) -> (Vec<TextEdit>, Vec<String>) {
    let rejects = s.review.reject_all(&s.text);
    let replaced: Vec<String> = rejects
        .iter()
        .map(|e| s.text[e.range.clone()].to_owned())
        .collect();
    s.edits(&rejects, Origin::Review);
    s.recompute();
    (rejects, replaced)
}

/// `n` separate lines, each changed by one replace_all of the agent, then Reject all.
fn many_rejected(n: usize) -> (Session, String, Vec<TextEdit>, Vec<String>) {
    let mut s = many_changed(n);
    let (base, agent_text) = (s.review.baseline().to_owned(), s.text.clone());
    let (rejects, replaced) = reject_everything(&mut s);
    assert_eq!((s.text.as_str(), s.review.baseline()), (&*base, &*base));
    assert!(s.hunks().is_empty());
    (s, agent_text, rejects, replaced)
}

/// One Reject all of 600 hunks is one decision: Alt+U must bring every hunk back. The reject
/// records beyond the cap of 512 are dropped while the decision is made, so Alt+U only re-applies
/// some of them.
#[test]
fn regress2_alt_u_of_large_reject_restores_every_hunk() {
    let (mut s, agent_text, _, _) = many_rejected(600);
    let action = s.undo_review();
    let restored = match &action {
        Some(UndoAction::Edits(edits)) => edits.len(),
        _ => 0,
    };
    assert_eq!(
        (restored, s.hunks().len()),
        (600, 600),
        "Alt+U of one 600-hunk Reject all restored {restored} edits, {} hunks",
        s.hunks().len()
    );
    assert_eq!(s.text, agent_text);
}

/// Ctrl+Z of a 600-hunk Reject all puts the agent's text back; every change must show as a hunk
/// again (§4.4: undoing a reject re-opens the hunk), none may be folded into the baseline.
#[test]
fn regress2_ctrl_z_of_large_reject_reopens_every_hunk() {
    let (mut s, agent_text, rejects, replaced) = many_rejected(600);
    s.undo_action(&rejects, &replaced);
    s.recompute();
    assert_eq!(s.text, agent_text);
    let folded = s.review.baseline().matches("\\mathbb{R}").count();
    assert_eq!(
        (s.hunks().len(), folded),
        (600, 0),
        "after Ctrl+Z: {} hunks of 600; {folded} agent changes folded into the baseline",
        s.hunks().len()
    );
}

// ---- D-11: an external (Reload) change merged into a turn's hunk is rejected with it ----------

/// Turn 1 changes line 3; outside the turn another program changes line 4 and the watcher reloads
/// it (Origin::Reload). The line diff merges both into one hunk labelled turn 1 alone, so Reject
/// Turn 1 silently reverts the other program's change too.
#[test]
fn regress2_reject_turn_keeps_reloaded_change() {
    let base: String = (0..10).map(|i| format!("l{i}\n")).collect();
    let mut s = Session::new(&base);
    s.review.set_turn(1);
    s.agent_replace("l3\n", "L3 by the agent\n");
    s.review.set_turn(0);
    let at = s.text.find("l4\n").unwrap();
    s.edit(&TextEdit::insert(at + 2, " edited in nvim"), Origin::Reload);
    s.recompute();
    eprintln!("hunks {:?} turns {:?}", s.pairs(), turns(&s));
    let rejected = rejected_turn(&s, 1);
    assert!(
        rejected.contains("l4 edited in nvim\n"),
        "Reject Turn 1 reverted the reloaded change: {rejected:?}"
    );
}

// ---- D-5: replace_all with many matches is superquadratic in on_edit ------------------------

/// Feeds a replace_all of `$` → `\(` (four matches per line, descending as `str_replace` returns
/// them) over `lines` lines to a review at once (`on_edits`, as `Document::apply_edits` does).
fn dollar_replace_all(lines: usize) -> (usize, Duration) {
    let text: String = (0..lines)
        .map(|i| format!("Line {i}: let $x_{i}$ and $y$ be given.\n"))
        .collect();
    let edits = str_replace(&text, "$", "\\(", true).unwrap();
    let mut review = FileReview::new(text);
    review.set_turn(1);
    let started = Instant::now();
    review.on_edits(&edits, Origin::Agent { call: 1 });
    (edits.len(), started.elapsed())
}

/// A replace_all with 20k matches (every `$` of a 5000-line chapter, e.g. `$` → `\(`) must not
/// freeze the main thread for seconds: ARCHITECTURE §7 accepts 0.5–0.7 s only for a pathological
/// whole-file rewrite of 60k lines, but fed to `on_edit` one by one the matches were
/// superquadratic (10k: 0.4 s, 20k: 1.7–2.2 s, 40k: 6.3 s in release). `on_edits` takes them in
/// about 0.1 s. Release build: `-- --ignored regress2_replace_all` (about 4 s in a debug build,
/// too slow for the default run).
#[test]
#[ignore = "release timing test (D-5, fixed); run in release with --ignored regress2_replace_all"]
fn regress2_replace_all_20k_matches_is_fast() {
    dollar_replace_all(200);
    let (n, took) = dollar_replace_all(5000);
    eprintln!("replace_all of $: {n} edits, on_edits {took:?}");
    assert_eq!(n, 20_000);
    assert!(
        took < Duration::from_secs(1),
        "on_edits for {n} replace_all matches took {took:?}"
    );
}

/// A copy of the session, to take several ways from one state.
fn fork(s: &Session) -> Session {
    Session {
        review: s.review.clone(),
        text: s.text.clone(),
    }
}

/// Reject all of 5000 hunks (the D-10 case at scale), and its Ctrl+Z and Alt+U, each with its
/// recompute, must not freeze the window: every reject (and every step of their undo) re-checked
/// every record of the decision against the text, and Ctrl+Z, which puts the hunks back from the
/// top down, found each record past the hunks already back (O(rejects³)): 2.3 s, 100 s and 1.1 s
/// in release, now about 0.05 s, 0.07 s and 0.18 s. Release build: `-- --ignored perf_reject`.
#[test]
#[ignore = "release timing test; run in release with --ignored perf_reject"]
fn perf_reject_all_of_5000_hunks_and_its_undo() {
    let n = 5000;
    let mut s = many_changed(n);
    let agent_text = s.text.clone();
    let t = Instant::now();
    let (rejects, replaced) = reject_everything(&mut s);
    let reject = t.elapsed();
    assert!(s.hunks().is_empty());

    let mut undone = fork(&s);
    let t = Instant::now();
    undone.undo_action(&rejects, &replaced);
    undone.recompute();
    let ctrl_z = t.elapsed();
    assert_eq!(undone.text, agent_text);
    assert_eq!(undone.hunks().len(), n);
    assert_eq!(undone.review.baseline(), s.review.baseline());

    let mut reverted = fork(&s);
    let t = Instant::now();
    let action = reverted.undo_review();
    let alt_u = t.elapsed();
    assert!(matches!(action, Some(UndoAction::Edits(e)) if e.len() == n));
    assert_eq!(reverted.text, agent_text);
    assert_eq!(reverted.hunks().len(), n);

    eprintln!("{n} hunks: Reject all {reject:?}, its Ctrl+Z {ctrl_z:?}, its Alt+U {alt_u:?}");
    for (what, took) in [("Reject all", reject), ("Ctrl+Z", ctrl_z), ("Alt+U", alt_u)] {
        assert!(
            took < Duration::from_millis(500),
            "{what} of {n} hunks took {took:?}"
        );
    }
}
