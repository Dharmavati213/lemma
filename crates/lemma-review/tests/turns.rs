//! Per-turn attribution (WP2.6): `set_turn`, `Hunk::turns`, `hunks_of_turn`, `accept_turn`,
//! `reject_turn`, and attribution through undo, decisions and persistence.
//!
//! The property tests run 2000 cases each by default; `PROPTEST_CASES` overrides it.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit, apply_all};
use lemma_review::{FileReview, HunkId, PersistedReview, UndoAction, minimal_edits, str_replace};
use proptest::prelude::*;
use proptest::sample::Index;
use std::ops::Range;

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    ProptestConfig::with_cases(cases)
}

/// The turns of every hunk, in order.
fn turns(s: &Session) -> Vec<Vec<u32>> {
    s.hunks().iter().map(|h| h.turns.clone()).collect()
}

/// An agent `edit` call in the current turn (str_replace, applied, recomputed).
fn agent(s: &mut Session, old: &str, new: &str) -> Vec<TextEdit> {
    s.agent_replace(old, new)
}

// ---- attribution -------------------------------------------------------------------------------

#[test]
fn edits_belong_to_the_current_turn() {
    let mut s = Session::new("a\nb\nc\nd\ne\nf\n");
    assert_eq!(s.review.turn(), 0);
    agent(&mut s, "a\n", "A\n");
    s.review.set_turn(1);
    agent(&mut s, "c\n", "C\n");
    s.review.begin_turn();
    s.review.set_turn(2);
    assert_eq!(s.review.turn(), 2);
    agent(&mut s, "e\n", "E\n");
    assert_eq!(turns(&s), [vec![0], vec![1], vec![2]]);
    assert_eq!(s.review.hunks_of_turn(1).len(), 1);
    assert_eq!(s.review.hunks_of_turn(1)[0].cur_lines, 2..3);
    assert_eq!(s.review.mixed_hunks_of_turn(1), 0);
    // A second edit of turn 2 on the line of turn 1 makes that hunk mixed.
    agent(&mut s, "C\n", "Cc\n");
    assert_eq!(turns(&s), [vec![0], vec![1, 2], vec![2]]);
    assert!(s.review.hunks_of_turn(1).is_empty());
    assert_eq!(s.review.mixed_hunks_of_turn(1), 1);
    assert_eq!(s.review.mixed_hunks_of_turn(2), 1);
    assert_eq!(s.review.hunks_of_turn(2).len(), 1);
}

#[test]
fn turns_are_known_before_the_recompute() {
    let mut s = Session::new("a\nb\nc\n");
    s.review.set_turn(4);
    s.edit(&TextEdit::insert(2, "B"), Origin::Agent { call: 1 });
    assert_eq!(turns(&s), [vec![4]]);
    s.recompute();
    assert_eq!(turns(&s), [vec![4]]);
}

#[test]
fn adjacent_edits_of_two_turns_make_one_mixed_hunk() {
    let mut s = Session::new("a\nb\nc\nd\n");
    s.review.set_turn(1);
    agent(&mut s, "b\n", "B\n");
    s.review.set_turn(2);
    agent(&mut s, "c\n", "C\n");
    assert_eq!(turns(&s), [vec![1, 2]]);
}

#[test]
fn deletions_and_insertions_placed_off_the_line_get_their_turn() {
    // str_replace places these mid-line, the line diff at the next line start.
    let mut s = Session::new("one\nfoo\ntwo\nthree\nfour\n");
    s.review.set_turn(3);
    agent(&mut s, "one\nfoo", "one");
    assert_eq!(s.hunks().len(), 1);
    assert_eq!(s.hunks()[0].cur_bytes, 4..4);
    assert_eq!(turns(&s), [vec![3]]);
    s.review.set_turn(5);
    agent(&mut s, "three", "three\nX");
    assert_eq!(turns(&s), [vec![3], vec![5]]);
    // Equal lines make the insertion ambiguous: whichever copy the diff marks, it is turn 6's.
    let mut s = Session::new("a\nb\nc\n");
    s.review.set_turn(6);
    agent(&mut s, "a\n", "a\nb\n");
    assert_eq!(turns(&s), [vec![6]]);
}

#[test]
fn user_typing_keeps_the_turns() {
    let mut s = Session::new("a\nb\nc\nd\ne\n");
    s.review.set_turn(1);
    agent(&mut s, "b\n", "B\n");
    // Outside the hunk (folded into the baseline) and inside it.
    s.user(TextEdit::insert(0, "zz"));
    s.user(TextEdit::insert(s.text.find('B').unwrap() + 1, "u"));
    s.recompute();
    s.check();
    assert_eq!(turns(&s), [vec![1]]);
    assert!(s.hunks()[0].user_touched);
}

#[test]
fn reload_changes_have_no_turn() {
    let mut s = Session::new("a\nb\nc\n");
    s.edit(&TextEdit::insert(2, "x"), Origin::Reload);
    s.recompute();
    assert_eq!(turns(&s), [Vec::<u32>::new()]);
    assert!(s.review.hunks_of_turn(0).is_empty());
}

// ---- deciding a turn ---------------------------------------------------------------------------

/// A review with turn 1 on lines 1 and 5, turn 2 on lines 3 and 5 (line 5 mixed).
fn two_turns() -> Session {
    let mut s = Session::new("l0\nl1\nl2\nl3\nl4\nl5 a b\nl6\nl7\n");
    s.review.set_turn(1);
    agent(&mut s, "l1\n", "L1\n");
    agent(&mut s, "l5 a", "l5 A");
    s.review.begin_turn();
    s.review.set_turn(2);
    agent(&mut s, "l3\n", "L3\n");
    agent(&mut s, "A b", "A B");
    assert_eq!(turns(&s), [vec![1], vec![2], vec![1, 2]]);
    s
}

#[test]
fn reject_turn_leaves_other_turns_and_mixed_hunks() {
    let mut s = two_turns();
    let ids = s.ids();
    assert_eq!(s.review.mixed_hunks_of_turn(2), 1);
    let edits = s.review.reject_turn(2, &s.text);
    assert_eq!(edits.len(), 1);
    s.edits(&edits, Origin::Review);
    s.recompute();
    s.check();
    assert_eq!(s.text, "l0\nL1\nl2\nl3\nl4\nl5 A B\nl6\nl7\n");
    assert_eq!(s.ids(), [ids[0], ids[2]]);
    assert_eq!(turns(&s), [vec![1], vec![1, 2]]);
    // Rejecting a turn with nothing of its own changes nothing.
    assert!(s.review.reject_turn(2, &s.text).is_empty());
    // One Alt+U brings the turn back.
    assert!(matches!(s.undo_review(), Some(UndoAction::Edits(_))));
    assert_eq!(s.ids(), ids);
    assert_eq!(turns(&s), [vec![1], vec![2], vec![1, 2]]);
}

#[test]
fn ctrl_z_of_a_rejected_turn_brings_its_turn_back() {
    let mut s = two_turns();
    let ids = s.ids();
    let edits = s.review.reject_turn(2, &s.text);
    let replaced: Vec<String> = edits
        .iter()
        .map(|e| s.text[e.range.clone()].to_owned())
        .collect();
    s.edits(&edits, Origin::Review);
    s.recompute();
    s.undo_action(&edits, &replaced);
    s.recompute();
    assert_eq!(s.ids(), ids);
    assert_eq!(turns(&s), [vec![1], vec![2], vec![1, 2]]);
}

#[test]
fn accept_turn_keeps_the_text_and_reports_mixed_hunks() {
    let mut s = two_turns();
    let ids = s.ids();
    let text = s.text.clone();
    assert_eq!(s.review.accept_turn(1, &s.text), 1);
    assert_eq!(s.text, text);
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), "l0\nL1\nl2\nl3\nl4\nl5 a b\nl6\nl7\n");
    assert_eq!(s.ids(), [ids[1], ids[2]]);
    assert_eq!(turns(&s), [vec![2], vec![1, 2]]);
    // A later turn editing the accepted line is that turn's alone.
    s.review.set_turn(3);
    agent(&mut s, "L1\n", "L1!\n");
    assert_eq!(turns(&s), [vec![3], vec![2], vec![1, 2]]);
    // Alt+U of the accept: the hunk comes back with turn 1 (merged with turn 3's change).
    assert_eq!(s.review.undo_review_action(&s.text), None);
    let mut s = two_turns();
    s.review.accept_turn(1, &s.text);
    s.recompute();
    assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
    assert_eq!(s.ids(), ids);
    assert_eq!(turns(&s), [vec![1], vec![2], vec![1, 2]]);
}

#[test]
fn a_rejected_hunk_no_longer_counts_for_its_turn() {
    let mut s = two_turns();
    let id = s.ids()[0];
    s.reject(id);
    // Turn 3 edits the line turn 1's rejected change was on: it is turn 3's alone.
    s.review.set_turn(3);
    agent(&mut s, "l1\n", "l1!\n");
    assert_eq!(turns(&s), [vec![3], vec![2], vec![1, 2]]);
}

#[test]
fn buffer_undo_of_an_agent_edit_takes_its_turn_out_of_a_mixed_hunk() {
    let mut s = Session::new("x\nl5 a b\ny\n");
    s.review.set_turn(1);
    agent(&mut s, "l5 a", "l5 A");
    s.review.set_turn(2);
    let edits = str_replace(&s.text, "A b", "A B", false).unwrap();
    let replaced: Vec<String> = edits
        .iter()
        .map(|e| s.text[e.range.clone()].to_owned())
        .collect();
    s.edits(&edits, Origin::Agent { call: 9 });
    s.recompute();
    assert_eq!(turns(&s), [vec![1, 2]]);
    s.undo_action(&edits, &replaced);
    s.recompute();
    assert_eq!(turns(&s), [vec![1]]);
    // Redo puts it back.
    s.edits(&edits, Origin::History);
    s.recompute();
    assert_eq!(turns(&s), [vec![1, 2]]);
}

#[test]
fn a_set_of_hunks_is_one_decision() {
    let mut s = two_turns();
    let ids = s.ids();
    let unknown = HunkId(42);
    let edits = s.review.reject_hunks(&[ids[2], unknown, ids[0]], &s.text);
    assert_eq!(edits.len(), 2);
    assert!(edits[0].range.start > edits[1].range.start, "descending");
    s.edits(&edits, Origin::Review);
    s.recompute();
    assert_eq!(s.ids(), [ids[1]]);
    assert!(matches!(s.undo_review(), Some(UndoAction::Edits(_))));
    assert_eq!(s.ids(), ids);
    assert_eq!(s.review.accept_hunks(&[ids[1], ids[2]], &s.text), 2);
    s.recompute();
    assert_eq!(s.ids(), [ids[0]]);
    assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
    assert_eq!(s.ids(), ids);
    assert_eq!(turns(&s), [vec![1], vec![2], vec![1, 2]]);
    let stale = format!("{}!", s.text);
    assert_eq!(s.review.accept_hunks(&ids, &stale), 0);
    assert!(s.review.reject_hunks(&ids, &stale).is_empty());
}

#[test]
fn decisions_on_stale_text_do_nothing_per_turn() {
    let mut s = two_turns();
    let stale = format!("{}!", s.text);
    assert!(s.review.reject_turn(2, &stale).is_empty());
    assert_eq!(s.review.accept_turn(2, &stale), 1);
    assert_eq!(s.hunks().len(), 3);
}

#[test]
fn remap_turns_renumbers() {
    let mut s = two_turns();
    s.review.remap_turns(|t| t + 10);
    assert_eq!(turns(&s), [vec![11], vec![12], vec![11, 12]]);
    s.review.remap_turns(|_| 0);
    assert_eq!(turns(&s), [vec![0], vec![0], vec![0]]);
}

// ---- persistence -------------------------------------------------------------------------------

#[test]
fn persistence_keeps_turns_ids_and_touched_flags() {
    let mut s = Session::new("a\nb\na\nb\nc\n");
    s.review.set_turn(1);
    // Two hunks with equal texts: occurrences 0 and 1.
    agent(&mut s, "b\na", "B\na");
    agent(&mut s, "b\nc", "B\nc");
    s.review.set_turn(2);
    agent(&mut s, "c\n", "c\nnew\n");
    // Accepting the first renumbers nothing: the second keeps occurrence 1.
    let first = s.ids()[0];
    s.accept(first);
    let touched = s.hunks()[1].cur_bytes.start + 1;
    s.user(TextEdit::insert(touched, "u"));
    s.recompute();
    let p = s.review.to_persisted("x.tex".into(), &s.text);
    let json = serde_json::to_string(&p).unwrap();
    let back: PersistedReview = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
    let restored = FileReview::from_persisted(&back, &s.text).unwrap();
    assert_eq!(restored.hunks(), s.hunks());
    assert!(restored.hunks().iter().any(|h| h.user_touched));
    assert_eq!(
        restored
            .hunks()
            .iter()
            .map(|h| &h.turns)
            .collect::<Vec<_>>(),
        [&vec![1], &vec![2]]
    );
    // The restored review attributes new edits to turn 0 until told otherwise.
    assert_eq!(restored.turn(), 0);
}

#[test]
fn files_without_the_new_fields_still_load() {
    let mut s = Session::new("a\nb\n");
    agent(&mut s, "b", "B");
    let json = format!(
        r#"{{"path":"x.tex","baseline":"a\nb\n","current_hash":{},"created_unix":7}}"#,
        lemma_core::fnv1a64(&[s.text.as_bytes()])
    );
    let p: PersistedReview = serde_json::from_str(&json).unwrap();
    assert!(p.attribution.is_empty() && p.hunks.is_empty());
    let restored = FileReview::from_persisted(&p, &s.text).unwrap();
    assert_eq!(restored.hunks().len(), 1);
    assert_eq!(restored.hunks()[0].id, s.ids()[0]);
    assert!(restored.hunks()[0].turns.is_empty());
    // Empty new fields are not written.
    let p = FileReview::new("a\n".into()).to_persisted("y.tex".into(), "a\n");
    let json = serde_json::to_string(&p).unwrap();
    assert!(
        !json.contains("attribution") && !json.contains("hunks"),
        "{json}"
    );
}

#[test]
fn damaged_attribution_is_ignored() {
    let mut s = Session::new("a\nb\n");
    s.review.set_turn(1);
    agent(&mut s, "b", "B");
    let mut p = s.review.to_persisted("x.tex".into(), &s.text);
    p.attribution.push(lemma_review::TurnRange {
        turn: 9,
        bytes: 3..1000,
    });
    // A saved hunk listed twice.
    p.hunks.push(p.hunks[0].clone());
    let restored = FileReview::from_persisted(&p, &s.text).unwrap();
    assert_eq!(restored.hunks(), s.hunks());

    // Two hunks with equal texts whose saved occurrences collide: the ids stay unique.
    let mut s = Session::new(
        "a
x
a
x
",
    );
    agent(
        &mut s, "a
x
a", "A
x
a",
    );
    agent(
        &mut s, "x
a
x", "x
A
x",
    );
    let mut p = s.review.to_persisted("x.tex".into(), &s.text);
    assert_eq!(p.hunks.len(), 2);
    for h in &mut p.hunks {
        h.occurrence = 1;
    }
    let restored = FileReview::from_persisted(&p, &s.text).unwrap();
    let ids: Vec<HunkId> = restored.hunks().iter().map(|h| h.id).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert!(ids.contains(&s.ids()[1]), "one of them keeps occurrence 1");
}

// ---- properties --------------------------------------------------------------------------------

const WORDS: &[&str] = &["alpha", "x", "\\beta", "é∑", "$f$", "\\\\", "a\rb", ""];

/// A document of unique lines (`l<i> …`), so line alignment is never ambiguous.
fn document() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(
            (prop::sample::select(WORDS), prop::sample::select(WORDS)),
            3..18,
        ),
        any::<bool>(),
    )
        .prop_map(|(lines, trailing_newline)| {
            let mut s: String = lines
                .iter()
                .enumerate()
                .map(|(i, (a, b))| format!("l{i} {a} {b}\n"))
                .collect();
            if !trailing_newline {
                s.pop();
            }
            s
        })
}

/// An agent change, applied either as the `write` tool does (`minimal_edits`) or as the `edit`
/// tool does (`str_replace` with some context around the change).
#[derive(Clone, Debug)]
struct AgentOp {
    kind: OpKind,
    /// Bytes of context on each side for the `edit` tool; `None` for `write`.
    context: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
enum OpKind {
    /// Replace `len` lines at `at` by `new` fresh lines.
    Lines { at: Index, len: usize, new: usize },
    /// Change the last word of a line.
    Word { line: Index, word: &'static str },
}

fn agent_op() -> impl Strategy<Value = AgentOp> {
    let kind = prop_oneof![
        (any::<Index>(), 0usize..3, 0usize..3).prop_map(|(at, len, new)| OpKind::Lines {
            at,
            len,
            new
        }),
        (any::<Index>(), prop::sample::select(WORDS))
            .prop_map(|(line, word)| OpKind::Word { line, word }),
    ];
    let context = prop::option::of((0usize..12, 0usize..12));
    (kind, context).prop_map(|(kind, context)| AgentOp { kind, context })
}

/// The text after `op`; fresh lines are numbered from `serial` so they stay unique.
fn changed(text: &str, op: &OpKind, serial: &mut usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    match op {
        OpKind::Lines { at, len, new } => {
            let start = at.index(lines.len() + 1);
            let end = (start + len).min(lines.len());
            let fresh: String = (0..*new)
                .map(|j| {
                    *serial += 1;
                    format!("n{serial}_{j} {}\n", WORDS[j % WORDS.len()])
                })
                .collect();
            [lines[..start].concat(), fresh, lines[end..].concat()].concat()
        }
        OpKind::Word { line, word } => {
            if lines.is_empty() {
                return text.to_owned();
            }
            let k = line.index(lines.len());
            let l = lines[k];
            let body = l.strip_suffix('\n').unwrap_or(l);
            let cut = body.rfind(' ').map_or(body.len(), |i| i + 1);
            let newline = if l.ends_with('\n') { "\n" } else { "" };
            let replaced = format!("{}{word}{newline}", &body[..cut]);
            [lines[..k].concat(), replaced, lines[k + 1..].concat()].concat()
        }
    }
}

/// The edits turning `text` into `target` the way `op` says: `minimal_edits`, or the
/// `str_replace` of the changed bytes with some context (when that context is unique).
fn tool_edits(text: &str, target: &str, context: Option<(usize, usize)>) -> Vec<TextEdit> {
    let minimal = minimal_edits(text, target);
    let (Some((left, right)), [e]) = (context, minimal.as_slice()) else {
        return minimal;
    };
    let mut s = e.range.start.saturating_sub(left);
    while !text.is_char_boundary(s) {
        s -= 1;
    }
    let mut t = (e.range.end + right).min(text.len());
    while !text.is_char_boundary(t) {
        t += 1;
    }
    let old = &text[s..t];
    let mut new = old.to_owned();
    TextEdit {
        range: e.range.start - s..e.range.end - s,
        text: e.text.clone(),
    }
    .apply(&mut new);
    match str_replace(text, old, &new, false) {
        Ok(edits) if !old.is_empty() => edits,
        _ => minimal,
    }
}

/// Runs `ops` as agent edits of the current turn, recomputing after the ops whose bit is set in
/// `mask`. Returns the text before and after each op.
fn run_turn(
    s: &mut Session,
    ops: &[AgentOp],
    mask: u32,
    serial: &mut usize,
) -> Vec<(String, String)> {
    let mut steps = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let before = s.text.clone();
        let target = changed(&s.text, &op.kind, serial);
        let edits = tool_edits(&s.text, &target, op.context);
        s.edits(&edits, Origin::Agent { call: i as u64 });
        assert_eq!(s.text, target);
        if mask & (1 << i) != 0 {
            s.recompute();
        }
        steps.push((before, target));
    }
    steps
}

/// Line ranges of the line diff of `old` and `new`: (old lines, new lines) of each hunk.
fn line_hunks(old: &str, new: &str) -> Vec<(Range<u32>, Range<u32>)> {
    let mut r = FileReview::new(old.to_owned());
    r.on_edit(
        &TextEdit {
            range: 0..old.len(),
            text: new.to_owned(),
        },
        Origin::Reload,
    );
    r.recompute(new);
    r.hunks()
        .iter()
        .map(|h| (h.base_lines.clone(), h.cur_lines.clone()))
        .collect()
}

/// Whether a step of turn 2 changed lines of turn 1's changes (`base` → `t1`) or lines next to
/// them (line ranges compared as closed intervals, as adjacent changes make one line hunk).
fn second_turn_touches_first(base: &str, t1: &str, steps: &[(String, String)]) -> bool {
    let touch = |x: &Range<u32>, y: &Range<u32>| x.start <= y.end && y.start <= x.end;
    // Turn 1's changed lines, in the line numbers of the text before each step.
    let mut first: Vec<Range<u32>> = line_hunks(base, t1).into_iter().map(|(_, n)| n).collect();
    for (before, after) in steps {
        let hunks = line_hunks(before, after);
        if first
            .iter()
            .any(|x| hunks.iter().any(|(old, _)| touch(x, old)))
        {
            return true;
        }
        for x in &mut first {
            let delta: i64 = hunks
                .iter()
                .filter(|(old, _)| old.end < x.start)
                .map(|(old, new)| i64::from(new.len() as u32) - i64::from(old.len() as u32))
                .sum();
            let shift = |l: u32| u32::try_from(i64::from(l) + delta).unwrap();
            *x = shift(x.start)..shift(x.end);
        }
    }
    false
}

/// Every text `edits` (descending, applied to `text`) give with some of them left out.
fn undo_subsets(text: &str, edits: &[TextEdit]) -> Vec<String> {
    let n = edits.len().min(10);
    (0u32..1 << n)
        .map(|undone| {
            let kept: Vec<TextEdit> = edits
                .iter()
                .enumerate()
                .filter(|(i, _)| *i >= n || undone & (1 << i) == 0)
                .map(|(_, e)| e.clone())
                .collect();
            let mut out = text.to_owned();
            apply_all(&mut out, &kept);
            out
        })
        .collect()
}

/// Two turns of agent edits on `base`: returns the session, the text after turn 1, and whether
/// turn 2 edited turn 1's changes (see [`second_turn_touches_first`]).
fn two_turn_session(
    base: &str,
    first: &[AgentOp],
    second: &[AgentOp],
    mask: u32,
) -> (Session, String, bool) {
    let mut s = Session::new(base);
    let mut serial = 0;
    s.review.set_turn(1);
    run_turn(&mut s, first, mask, &mut serial);
    let t1 = s.text.clone();
    s.review.begin_turn();
    s.review.set_turn(2);
    let steps = run_turn(&mut s, second, mask >> 8, &mut serial);
    s.recompute();
    s.check();
    let touched = second_turn_touches_first(base, &t1, &steps);
    (s, t1, touched)
}

proptest! {
    #![proptest_config(config())]

    /// After two turns, `reject_turn(2)` restores exactly the text after turn 1 wherever turn 2
    /// did not edit turn 1's hunks (then no hunk is mixed, all of turn 2's hunks go and the
    /// others keep their ids). In every case it only takes back edits of turn 2 (checked when
    /// turn 2 is one tool call), and buffer undo brings turn 2 back with its ids and turns.
    /// (Where turn 2 rewrote, deleted or took back turn 1's change, taking turn 2 back gives turn
    /// 1's text back there, which may join a hunk next to it; and a hunk of turn 2 that holds
    /// such a change it cannot take apart is left to be decided on its own.)
    #[test]
    fn rejecting_the_second_turn_restores_the_first(
        base in document(),
        first in prop::collection::vec(agent_op(), 1..5),
        second in prop::collection::vec(agent_op(), 1..5),
        mask in any::<u32>(),
    ) {
        let (mut s, t1, touched) = two_turn_session(&base, &first, &second, mask);
        let t2 = s.text.clone();
        let untouched = !touched;
        let ids = s.ids();
        let all_turns = turns(&s);
        let own: Vec<HunkId> = s.review.hunks_of_turn(2).iter().map(|h| h.id).collect();
        let mixed = s.review.mixed_hunks_of_turn(2);
        if untouched {
            prop_assert_eq!(mixed, 0, "turns {:?}", all_turns);
            prop_assert!(all_turns.iter().all(|t| t == &[1] || t == &[2]), "{:?}", all_turns);
        }
        let edits = s.review.reject_turn(2, &s.text);
        let replaced: Vec<String> =
            edits.iter().map(|e| s.text[e.range.clone()].to_owned()).collect();
        s.edits(&edits, Origin::Review);
        s.recompute();
        s.check();
        if untouched {
            prop_assert_eq!(&s.text, &t1);
            prop_assert!(s.review.hunks_of_turn(2).is_empty());
            prop_assert_eq!(s.review.mixed_hunks_of_turn(2), mixed);
            let expected: Vec<HunkId> =
                ids.iter().copied().filter(|id| !own.contains(id)).collect();
            prop_assert_eq!(s.ids(), expected);
        }
        if let [op] = second.as_slice() {
            // The text is turn 2's with some of its edits undone, and nothing else.
            let edits = tool_edits(&t1, &t2, op.context);
            prop_assert!(
                undo_subsets(&t1, &edits).contains(&s.text),
                "rejecting turn 2 gave {:?}; turn 1 left {:?}, turn 2 {:?}", s.text, t1, t2
            );
        }
        // Ctrl+Z of the reject: turn 2's hunks come back with their ids and turns.
        s.undo_action(&edits, &replaced);
        s.recompute();
        prop_assert_eq!(&s.text, &t2);
        prop_assert_eq!(s.ids(), ids);
        prop_assert_eq!(turns(&s), all_turns);
    }

    /// `accept_turn` never changes the text: it moves turn 2's own hunks into the baseline (all of
    /// them when the turns did not touch each other's changes; a hunk that also holds a change
    /// of turn 1, which turn 2 rewrote, stays), keeps the other ids, and Alt+U reverts it.
    #[test]
    fn accepting_a_turn_moves_only_its_hunks_into_the_baseline(
        base in document(),
        first in prop::collection::vec(agent_op(), 1..5),
        second in prop::collection::vec(agent_op(), 1..5),
        mask in any::<u32>(),
        which in 1u32..3,
    ) {
        let (mut s, _, touched) = two_turn_session(&base, &first, &second, mask);
        let (text, ids, all_turns) = (s.text.clone(), s.ids(), turns(&s));
        let own: Vec<_> = s
            .review
            .hunks_of_turn(which)
            .iter()
            .map(|h| (h.id, h.base_bytes.clone(), s.text[h.cur_bytes.clone()].to_owned()))
            .collect();
        let baseline_before = s.review.baseline().to_owned();
        let mixed = s.review.mixed_hunks_of_turn(which);
        let left_alone = s.review.accept_turn(which, &s.text);
        prop_assert_eq!(&s.text, &text);
        // The hunks still there are those left alone: the mixed ones and own ones it kept out.
        let still: Vec<HunkId> = s.review.hunks().iter().map(|h| h.id).collect();
        let kept: Vec<_> = own.iter().filter(|(id, _, _)| !still.contains(id)).collect();
        prop_assert_eq!(left_alone, mixed + own.len() - kept.len());
        if !touched {
            prop_assert_eq!(kept.len(), own.len());
        }
        let mut baseline = baseline_before;
        let edits: Vec<TextEdit> = kept
            .iter()
            .rev()
            .map(|(_, base, new)| TextEdit { range: base.clone(), text: new.clone() })
            .collect();
        apply_all(&mut baseline, &edits);
        prop_assert_eq!(s.review.baseline(), baseline.as_str());
        s.recompute();
        s.check();
        let expected: Vec<HunkId> =
            ids.iter().copied().filter(|id| !kept.iter().any(|(o, _, _)| o == id)).collect();
        prop_assert_eq!(s.ids(), expected);
        prop_assert_eq!(s.review.hunks_of_turn(which).len(), own.len() - kept.len());
        if !kept.is_empty() {
            prop_assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
            prop_assert_eq!(s.ids(), ids);
            prop_assert_eq!(turns(&s), all_turns);
        }
    }

    /// Persisting and restoring reproduces the hunks with their ids and turns.
    #[test]
    fn persistence_keeps_the_turns(
        base in document(),
        first in prop::collection::vec(agent_op(), 1..5),
        second in prop::collection::vec(agent_op(), 1..5),
        mask in any::<u32>(),
    ) {
        let (s, _, _) = two_turn_session(&base, &first, &second, mask);
        let p = s.review.to_persisted("x.tex".into(), &s.text);
        let json = serde_json::to_string(&p).unwrap();
        let back: PersistedReview = serde_json::from_str(&json).unwrap();
        let restored = FileReview::from_persisted(&back, &s.text).expect("same text");
        // Word spans get a time budget (they may differ under load); everything else is exact.
        let key = |r: &FileReview| -> Vec<_> {
            r.hunks()
                .iter()
                .map(|h| {
                    (h.id, h.cur_bytes.clone(), h.base_bytes.clone(), h.user_touched, h.turns.clone())
                })
                .collect()
        };
        prop_assert_eq!(key(&restored), key(&s.review));
    }
}

// ---- documents with repeated blank lines -------------------------------------------------------

/// A document of paragraphs (unique lines) separated by runs of blank lines: the line diff can
/// pair one turn's change with a paragraph that only moved, or with another turn's change.
fn paragraph_document() -> impl Strategy<Value = String> {
    prop::collection::vec((1usize..3, 1usize..4), 2..8).prop_map(|paragraphs| {
        let mut s = String::new();
        for (i, (lines, blanks)) in paragraphs.iter().enumerate() {
            for j in 0..*lines {
                s.push_str(&format!("p{i}.{j} text\n"));
            }
            s.push_str(&"\n".repeat(*blanks));
        }
        s
    })
}

/// An agent change on a paragraph document: `del` lines at `at` are replaced by new lines, each
/// a fresh paragraph line (`true`) or a blank line; applied as `minimal_edits` or as a
/// `str_replace` with some context (see [`tool_edits`]).
#[derive(Clone, Debug)]
struct ParagraphOp {
    at: Index,
    del: usize,
    new: Vec<bool>,
    context: Option<(usize, usize)>,
}

fn paragraph_op() -> impl Strategy<Value = ParagraphOp> {
    (
        any::<Index>(),
        0usize..3,
        prop::collection::vec(any::<bool>(), 0..4),
        prop::option::of((0usize..12, 0usize..12)),
    )
        .prop_map(|(at, del, new, context)| ParagraphOp {
            at,
            del,
            new,
            context,
        })
}

/// The text after `op`.
fn paragraph_changed(text: &str, op: &ParagraphOp, serial: &mut usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = op.at.index(lines.len() + 1);
    let end = (start + op.del).min(lines.len());
    let fresh: String = op
        .new
        .iter()
        .map(|&paragraph| {
            if paragraph {
                *serial += 1;
                format!("n{serial} new\n")
            } else {
                "\n".to_owned()
            }
        })
        .collect();
    [lines[..start].concat(), fresh, lines[end..].concat()].concat()
}

/// Lines `[lo, hi]` of `text` (closed), widened over the blank lines next to them.
fn widened_over_blanks(text: &str, lo: usize, hi: usize) -> (usize, usize) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let blank = |i: usize| lines.get(i).is_some_and(|l| l.trim().is_empty());
    let (mut lo, mut hi) = (lo, hi);
    while lo > 0 && blank(lo - 1) {
        lo -= 1;
    }
    while hi + 1 < lines.len() && blank(hi + 1) {
        hi += 1;
    }
    (lo, hi)
}

/// Whether every change of turn 1 (`base` → `t1`) and every edit of turn 2 (`second`, on `t1`)
/// are apart by at least one paragraph line neither changed, however the line diff places them
/// in their runs of blank lines.
fn separated_by_a_paragraph(base: &str, t1: &str, second: &[TextEdit]) -> bool {
    let lines: Vec<&str> = t1.split_inclusive('\n').collect();
    let line_of = |x: usize| t1[..x].matches('\n').count();
    let first: Vec<(usize, usize)> = line_hunks(base, t1)
        .into_iter()
        .map(|(_, new)| {
            let (s, e) = (new.start as usize, new.end as usize);
            widened_over_blanks(t1, s.saturating_sub(1), e)
        })
        .collect();
    let second: Vec<(usize, usize)> = second
        .iter()
        .map(|e| {
            widened_over_blanks(
                t1,
                line_of(e.range.start.saturating_sub(1)),
                line_of(e.range.end),
            )
        })
        .collect();
    let paragraph_between = |a: (usize, usize), b: (usize, usize)| {
        let (x, y) = if a.1 < b.0 { (a, b) } else { (b, a) };
        x.1 < y.0 && (x.1 + 1..y.0).any(|i| lines.get(i).is_some_and(|l| !l.trim().is_empty()))
    };
    first
        .iter()
        .all(|&a| second.iter().all(|&b| paragraph_between(a, b)))
}

proptest! {
    #![proptest_config(config())]

    /// With blank lines between paragraphs the line diff may pair the two turns' changes in any
    /// way. `reject_turn(2)` still only ever takes back edits of turn 2 (the text is the second
    /// turn's with some of its edits undone: never a paragraph turn 1 deleted, never a duplicate);
    /// it takes all of them back when the turns are apart by a paragraph (unless the line diff
    /// moved that paragraph too); and Alt+U or buffer undo brings turn 2's text and hunks back.
    #[test]
    fn rejecting_the_second_turn_with_blank_lines(
        base in paragraph_document(),
        first in prop::collection::vec(paragraph_op(), 1..4),
        second in paragraph_op(),
        mask in any::<u32>(),
    ) {
        let mut s = Session::new(&base);
        let mut serial = 0;
        s.review.set_turn(1);
        // (With runs of blank lines a `str_replace` context may match another place than the
        // one meant: the agent's edit is whatever the tool did.)
        for (i, op) in first.iter().enumerate() {
            let target = paragraph_changed(&s.text, op, &mut serial);
            let edits = tool_edits(&s.text, &target, op.context);
            s.edits(&edits, Origin::Agent { call: i as u64 });
            if mask & (1 << i) != 0 {
                s.recompute();
            }
        }
        let t1 = s.text.clone();
        s.review.begin_turn();
        s.review.set_turn(2);
        let target = paragraph_changed(&t1, &second, &mut serial);
        let second_edits = tool_edits(&t1, &target, second.context);
        s.edits(&second_edits, Origin::Agent { call: 9 });
        s.recompute();
        s.check();
        let t2 = s.text.clone();
        let (ids, all_turns, pairs) = (s.ids(), turns(&s), s.pairs());
        let own: Vec<HunkId> = s.review.hunks_of_turn(2).iter().map(|h| h.id).collect();
        let edits = s.review.reject_turn(2, &s.text);
        let replaced: Vec<String> =
            edits.iter().map(|e| s.text[e.range.clone()].to_owned()).collect();
        s.edits(&edits, Origin::Review);
        s.recompute();
        s.check();
        let rejected = s.text.clone();
        // Some of turn 2's edits are undone, nothing else changed.
        if second_edits.len() <= 8 {
            let n = second_edits.len();
            let possible: Vec<String> = (0u32..1 << n)
                .map(|undone| {
                    let kept: Vec<TextEdit> = second_edits
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| undone & (1 << i) == 0)
                        .map(|(_, e)| e.clone())
                        .collect();
                    let mut text = t1.clone();
                    apply_all(&mut text, &kept);
                    text
                })
                .collect();
            prop_assert!(
                possible.contains(&rejected),
                "rejecting turn 2 gave {:?}; turn 1 left {:?}, turn 2 {:?} (turns {:?})",
                rejected, t1, t2, all_turns
            );
        }
        // All of turn 2 is taken back when the turns are apart. (Equally long alignments may
        // still let the line diff move the paragraph between them, and so put both turns'
        // changes into one mixed hunk, which is left alone, or move turn 2's change away from
        // its attribution, so that its hunk has no turn: then turn 2 is still mentioned, or a
        // hunk had no turn.)
        let unlabelled = all_turns.iter().any(Vec::is_empty);
        let left = s.hunks().iter().any(|h| h.turns.contains(&2));
        if !unlabelled && !left && separated_by_a_paragraph(&base, &t1, &second_edits) {
            prop_assert_eq!(&rejected, &t1, "turns {:?}", all_turns);
        }
        // Alt+U reverts it as one decision.
        let mut alt = Session {
            review: s.review.clone(),
            text: s.text.clone(),
        };
        if !edits.is_empty() {
            prop_assert!(matches!(alt.undo_review(), Some(UndoAction::Edits(_))));
        }
        prop_assert_eq!(&alt.text, &t2);
        prop_assert_eq!(alt.pairs(), pairs.clone());
        // Ctrl+Z of the reject: turn 2 is back, with its hunks, its own with their ids. (The line
        // diff may move other hunks with equal texts, such as deleted blank lines, along a run of
        // blank lines in between, and so renumber them; and not every label may come back: a
        // reject can move another turn's deletion away from its attribution.)
        s.undo_action(&edits, &replaced);
        s.recompute();
        prop_assert_eq!(&s.text, &t2);
        prop_assert_eq!(s.pairs(), pairs);
        let now = s.ids();
        prop_assert!(own.iter().all(|id| now.contains(id)), "{:?} lost some of {:?} ({:?})", now, own, ids);
    }
}
