//! White-box tests: the exact-model invariant of `model.rs` under random edit sequences, the
//! attribution invariants of `turns.rs` (every hunk's `turns` are the turns whose intervals touch
//! it) and the shape of the segments of `segments.rs`.

use super::FileReview;
use super::journal::Entry;
use super::turns::touch;
use crate::text::{count_newlines, line_span, to_u32};
use lemma_core::{Origin, TextEdit};
use proptest::prelude::*;

/// Checks that the hunk list describes `current` exactly (see `model.rs`).
fn check_model(r: &FileReview, current: &str) -> Result<(), String> {
    if r.hunks.len() != r.meta.len() {
        return Err("hunks and meta differ in length".into());
    }
    let (mut cur, mut base) = (0, 0);
    for (k, (h, m)) in r.hunks.iter().zip(&r.meta).enumerate() {
        if h.cur_bytes.start < cur || h.base_bytes.start < base {
            return Err(format!("hunk {k} overlaps its predecessor: {h:?}"));
        }
        if current.get(cur..h.cur_bytes.start) != r.baseline.get(base..h.base_bytes.start) {
            return Err(format!("gap before hunk {k} differs"));
        }
        if current.get(h.cur_bytes.clone()) != Some(m.new_text.as_str()) {
            return Err(format!(
                "hunk {k} new_text is not current[cur_bytes]: {h:?} {m:?}"
            ));
        }
        if r.baseline.get(h.base_bytes.clone()) != Some(h.old_text.as_str()) {
            return Err(format!("hunk {k} old_text is not baseline[base_bytes]"));
        }
        let cur_line = to_u32(count_newlines(&current.as_bytes()[..h.cur_bytes.start]));
        let base_line = to_u32(count_newlines(&r.baseline.as_bytes()[..h.base_bytes.start]));
        if h.cur_lines != line_span(cur_line, &m.new_text)
            || h.base_lines != line_span(base_line, &h.old_text)
        {
            return Err(format!("hunk {k} has wrong lines: {h:?}"));
        }
        if r.hunks.iter().filter(|o| o.id == h.id).count() != 1 {
            return Err(format!("hunk id {:?} is not unique", h.id));
        }
        cur = h.cur_bytes.end;
        base = h.base_bytes.end;
    }
    if current.get(cur..) != r.baseline.get(base..) {
        return Err("text after the last hunk differs".into());
    }
    if r.expected_len() != current.len() {
        return Err("expected_len differs".into());
    }
    let growth: isize = r
        .hunks
        .iter()
        .map(|h| h.cur_bytes.len() as isize - h.base_bytes.len() as isize)
        .sum();
    if r.baseline.len() as isize + growth != current.len() as isize {
        return Err("the hunks' growth does not add up to the current length".into());
    }
    check_attribution(r, current)
}

/// The attribution lists are sorted, disjoint and inside the text, and every hunk lists exactly
/// the turns whose intervals touch it.
fn check_attribution(r: &FileReview, current: &str) -> Result<(), String> {
    let mut last: Option<(u32, std::ops::Range<usize>)> = None;
    for (turn, x) in r.attribution.intervals() {
        if x.start > x.end || x.end > current.len() {
            return Err(format!("turn {turn} interval {x:?} is outside the text"));
        }
        if let Some((t, prev)) = &last
            && (*t > turn || (*t == turn && prev.end >= x.start))
        {
            return Err(format!(
                "turn {turn}: {prev:?} and {x:?} are not sorted and apart"
            ));
        }
        last = Some((turn, x.clone()));
    }
    for h in &r.hunks {
        let mut expected: Vec<u32> = r
            .attribution
            .intervals()
            .filter(|(_, x)| touch(x, &h.cur_bytes))
            .map(|(t, _)| t)
            .collect();
        expected.dedup();
        if h.turns != expected {
            return Err(format!("hunk {h:?} should have turns {expected:?}"));
        }
    }
    check_segments(r, current)
}

/// The segments lie in the text on char boundaries, sorted, with disjoint interiors (an empty one
/// never strictly inside another).
fn check_segments(r: &FileReview, current: &str) -> Result<(), String> {
    let mut prev: Option<std::ops::Range<usize>> = None;
    for s in r.segments.iter() {
        if current.get(s.range.clone()).is_none() {
            return Err(format!("segment {s:?} does not fit the text"));
        }
        if let Some(p) = &prev {
            let (x, y) = (&p, &s.range);
            let sorted = (x.start, x.end) <= (y.start, y.end);
            let apart = if x.is_empty() || y.is_empty() {
                !(x.start < y.start && y.start < x.end)
            } else {
                x.end <= y.start
            };
            if !sorted || !apart {
                return Err(format!(
                    "segments {x:?} and {y:?} overlap or are not sorted"
                ));
            }
        }
        prev = Some(s.range.clone());
    }
    for (_, x) in r.unknown.intervals() {
        if x.start > x.end || x.end > current.len() {
            return Err(format!("unknown interval {x:?} is outside the text"));
        }
    }
    Ok(())
}

/// One random step of a review session.
#[derive(Clone, Debug)]
enum Op {
    Edit {
        origin: u8,
        at: usize,
        len: usize,
        text: String,
    },
    Recompute,
    Accept(usize),
    Reject(usize),
    Undo,
    Turn,
    SetTurn(u32),
    AcceptTurn(u32),
    RejectTurn(u32),
    /// Decide every other hunk (from the n-th) as one decision.
    AcceptSome(usize),
    RejectSome(usize),
    /// The text goes into a new buffer, or out of every buffer (`set_in_buffer`).
    InBuffer(bool),
    /// Every occurrence of `from` replaced by `to` as one action (a replace_all), edit by edit.
    All {
        origin: u8,
        from: &'static str,
        to: &'static str,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let text = prop::sample::select(vec![
        "", "x", "yz", "\n", "a\nb", "é", "\n\n", "∑\n", "\r\n",
    ])
    .prop_map(str::to_owned);
    prop_oneof![
        6 => (0u8..5, any::<usize>(), 0usize..6, text).prop_map(|(origin, at, len, text)| Op::Edit {
            origin,
            at,
            len,
            text
        }),
        2 => Just(Op::Recompute),
        1 => any::<usize>().prop_map(Op::Accept),
        1 => any::<usize>().prop_map(Op::Reject),
        1 => Just(Op::Undo),
        1 => Just(Op::Turn),
        1 => (0u32..3).prop_map(Op::SetTurn),
        1 => (0u32..3).prop_map(Op::AcceptTurn),
        1 => (0u32..3).prop_map(Op::RejectTurn),
        1 => (0usize..2).prop_map(Op::AcceptSome),
        1 => (0usize..2).prop_map(Op::RejectSome),
        1 => any::<bool>().prop_map(Op::InBuffer),
    ]
}

/// Floors `i` to a char boundary of `s`.
fn floor(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn origin_of(n: u8) -> Origin {
    match n {
        0 => Origin::User,
        1 => Origin::History,
        2 => Origin::Review,
        3 => Origin::Reload,
        _ => Origin::Agent { call: 1 },
    }
}

fn apply(r: &mut FileReview, current: &mut String, e: &TextEdit, origin: Origin) {
    r.on_edit(e, origin);
    e.apply(current);
}

/// Plays one step of a random session on `r` and its text `current`.
fn play(r: &mut FileReview, current: &mut String, op: &Op) -> Result<(), String> {
    match op {
        Op::Edit {
            origin,
            at,
            len,
            text,
        } => {
            let a = floor(current, at % (current.len() + 1));
            let b = floor(current, a + len);
            let e = TextEdit {
                range: a..b,
                text: text.clone(),
            };
            apply(r, current, &e, origin_of(*origin));
        }
        Op::All { origin, from, to } => {
            for e in &crate::str_replace(current, from, to, true).unwrap_or_default() {
                apply(r, current, e, origin_of(*origin));
            }
        }
        Op::Recompute => {
            r.recompute(current);
        }
        Op::Accept(k) if !r.hunks.is_empty() => {
            let id = r.hunks[k % r.hunks.len()].id;
            r.accept(id, current).map_err(|e| format!("accept: {e}"))?;
        }
        Op::Reject(k) if !r.hunks.is_empty() => {
            let id = r.hunks[k % r.hunks.len()].id;
            let e = r.reject(id, current).map_err(|e| format!("reject: {e}"))?;
            apply(r, current, &e, Origin::Review);
        }
        Op::Undo => match r.undo_review_action(current) {
            Some(crate::UndoAction::Edits(edits)) => {
                for e in &edits {
                    apply(r, current, e, Origin::Review);
                }
            }
            Some(crate::UndoAction::Unaccepted) | None => {}
        },
        Op::Turn => r.begin_turn(),
        Op::SetTurn(t) => r.set_turn(*t),
        Op::AcceptTurn(t) => {
            let before = current.clone();
            r.accept_turn(*t, current);
            if *current != before {
                return Err("accept_turn changed the text".into());
            }
            // Only hunks holding more than the turn's change may stay.
            if r.check(current, 0..r.hunks.len()).is_ok()
                && r.clusters(*t, current).iter().any(|c| c.keepable)
            {
                return Err(format!(
                    "accept_turn({t}) left hunks that are the turn's alone"
                ));
            }
        }
        Op::RejectTurn(t) => {
            let edits = r.reject_turn(*t, current);
            for e in &edits {
                apply(r, current, e, Origin::Review);
            }
        }
        Op::AcceptSome(n) => {
            let ids: Vec<_> = r.hunks.iter().skip(*n).step_by(2).map(|h| h.id).collect();
            let accepted = r.accept_hunks(&ids, current);
            if accepted != 0 && accepted != ids.len() {
                return Err("accept_hunks accepted only some".into());
            }
            if accepted != 0 && ids.iter().any(|id| r.hunk(*id).is_some()) {
                return Err("accept_hunks left a hunk".into());
            }
        }
        Op::RejectSome(n) => {
            let ids: Vec<_> = r.hunks.iter().skip(*n).step_by(2).map(|h| h.id).collect();
            let edits = r.reject_hunks(&ids, current);
            for e in &edits {
                apply(r, current, e, Origin::Review);
            }
        }
        Op::InBuffer(in_buffer) => r.set_in_buffer(*in_buffer),
        Op::Accept(_) | Op::Reject(_) => {}
    }
    Ok(())
}

fn run(base: &str, ops: &[Op]) -> Result<(), String> {
    let mut r = FileReview::new(base.to_owned());
    let mut current = base.to_owned();
    for op in ops {
        play(&mut r, &mut current, op)?;
        check_model(&r, &current).map_err(|e| format!("after {op:?}: {e}"))?;
    }
    // Recompute agrees with the model: the hunks still describe the text exactly.
    r.recompute(&current);
    check_model(&r, &current).map_err(|e| format!("after the final recompute: {e}"))?;
    let mut rejected = current.clone();
    let mut copy = r.clone();
    let edits = copy.reject_all(&current);
    lemma_core::apply_all(&mut rejected, &edits);
    if rejected != r.baseline {
        return Err("reject_all does not restore the baseline".into());
    }
    r.accept_all(&current);
    if r.baseline != current || !r.is_clean() {
        return Err("accept_all does not make the baseline the current text".into());
    }
    Ok(())
}

/// Starting texts: empty, with and without a final newline, multibyte and `\r`.
const BASES: &[&str] = &[
    "",
    "a\n",
    "a\nb\nc\n",
    "one\ntwo\nthree\nfour\n",
    "x\ny",
    "é\n∑\r\nz",
];

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    ProptestConfig::with_cases(cases)
}

proptest! {
    #![proptest_config(config())]

    /// Any mix of edits (every origin), recomputes, decisions and undos, with the text in a
    /// buffer or not, keeps the model exact.
    #[test]
    fn model_stays_exact(
        base in prop::sample::select(BASES),
        ops in prop::collection::vec(op(), 1..40),
    ) {
        if let Err(e) = run(base, &ops) {
            return Err(TestCaseError::fail(e));
        }
    }
}

#[test]
fn edits_between_recomputes_keep_lines() {
    let mut r = FileReview::new("a\nb\nc\nd\n".into());
    let mut cur = String::from("a\nb\nc\nd\n");
    apply(
        &mut r,
        &mut cur,
        &TextEdit::insert(4, "X\n"),
        Origin::Agent { call: 1 },
    );
    apply(&mut r, &mut cur, &TextEdit::insert(0, "0\n"), Origin::User);
    check_model(&r, &cur).unwrap();
    assert_eq!(r.hunks[0].cur_lines, 3..4);
    assert_eq!(r.hunks[0].base_lines, 3..3);
    apply(&mut r, &mut cur, &TextEdit::insert(7, "Y"), Origin::User);
    check_model(&r, &cur).unwrap();
    assert!(r.hunks[0].user_touched);
}

/// An agent insertion or deletion is widened over the text up to `MAX_SLIDE` bytes before it,
/// which may start inside a multibyte character: the widening starts at the next character.
#[test]
fn widening_starts_on_a_char_boundary() {
    for pad in 0..4 {
        let base = format!("{}{}abc\n", "é".repeat(200), "a".repeat(pad));
        let mut r = FileReview::new(base.clone());
        let mut cur = base.clone();
        let end = base.len() - 1;
        let agent = Origin::Agent { call: 1 };
        apply(&mut r, &mut cur, &TextEdit::insert(end, "X"), agent);
        apply(&mut r, &mut cur, &TextEdit::delete(end - 3..end - 2), agent);
        check_model(&r, &cur).unwrap();
        r.recompute(&cur);
        check_model(&r, &cur).unwrap();
        assert_eq!(r.hunks.len(), 1, "{pad}");
    }
}

/// Deciding a hunk takes the unknown places around it along (they describe its change), but not
/// one at the start of the next hunk, which may be about that hunk: Keep Turn kept the first of
/// two hunks of a turn and so made the second, whose change is not known exactly, look like the
/// turn's alone (random sessions found it: `play`'s check after `AcceptTurn`). The same for a
/// reject, and for its redo after an undo.
#[test]
fn a_decision_leaves_the_next_hunks_unknown_places() {
    let base = "a\nb\nc\nd\n";
    let agent = Origin::Agent { call: 1 };
    let review = || {
        let mut r = FileReview::new(base.to_owned());
        let mut cur = base.to_owned();
        // Two edits in the first hunk, so that redoing its reject is not undoing one of them.
        apply(&mut r, &mut cur, &TextEdit::insert(0, "A"), agent);
        apply(&mut r, &mut cur, &TextEdit::insert(2, "Z"), agent);
        apply(&mut r, &mut cur, &TextEdit::insert(6, "C"), agent);
        r.recompute(&cur);
        assert_eq!(r.hunks.len(), 2);
        // What the turn changed at the start of the second hunk is not known exactly.
        let at = r.hunks[1].cur_bytes.start;
        r.unknown.add(0, at..at);
        let clusters = r.clusters(0, &cur);
        assert_eq!(clusters.iter().filter(|c| c.keepable).count(), 1);
        (r, cur)
    };
    let undecidable = |r: &FileReview, cur: &str| {
        let [h] = &r.hunks[..] else {
            panic!("{:?}", r.hunks);
        };
        assert_eq!(cur.get(h.cur_bytes.clone()), Some("Cc\n"));
        assert!(r.clusters(0, cur).iter().all(|c| !c.keepable));
    };

    let (mut r, cur) = review();
    r.accept_turn(0, &cur);
    undecidable(&r, &cur);

    let (mut r, mut cur) = review();
    let id = r.hunks[0].id;
    let e = r.reject(id, &cur).unwrap();
    let proposal = cur[e.range.clone()].to_owned();
    apply(&mut r, &mut cur, &e, Origin::Review);
    r.recompute(&cur);
    undecidable(&r, &cur);
    // Ctrl+Z, then Ctrl+Shift+Z.
    let start = e.range.start;
    let undo = TextEdit::delete(start..start + e.text.len());
    apply(&mut r, &mut cur, &undo, Origin::History);
    apply(
        &mut r,
        &mut cur,
        &TextEdit::insert(start, &proposal),
        Origin::History,
    );
    r.recompute(&cur);
    assert_eq!(r.hunks.len(), 2);
    apply(&mut r, &mut cur, &e, Origin::History);
    r.recompute(&cur);
    undecidable(&r, &cur);
}

/// A recompute gives a hunk whose texts a hunk had at the recompute before that hunk's word
/// spans, if they were refined in time, and refines only the others (new texts, or spans
/// refined past the deadline).
#[test]
fn recompute_keeps_the_word_spans_of_unchanged_texts() {
    let base = "a\nlet x = alpha + 1;\nb\nlet y = gamma + 2;\nc\nlet z = delta + 3;\nd\n";
    let mut r = FileReview::new(base.into());
    let mut cur = base.to_owned();
    let agent = Origin::Agent { call: 1 };
    for (from, to) in [("delta", "omega"), ("gamma", "theta"), ("alpha", "beta")] {
        let at = cur.find(from).unwrap();
        let e = TextEdit {
            range: at..at + from.len(),
            text: to.into(),
        };
        apply(&mut r, &mut cur, &e, agent);
    }
    r.recompute(&cur);
    assert_eq!(r.hunks.len(), 3);
    assert!(r.meta.iter().all(|m| m.exact_words));
    assert!(r.hunks.iter().all(|h| h.words.len() == 2), "{:?}", r.hunks);
    let refined: Vec<_> = r.hunks.iter().map(|h| h.words.clone()).collect();
    // Spans the word diff would not give, to tell kept ones from refined ones.
    let marker = vec![crate::WordSpan {
        side: crate::Side::New,
        line: 0,
        bytes: 0..1,
    }];
    for h in &mut r.hunks {
        h.words = marker.clone();
    }
    r.meta[1].exact_words = false;
    // Typing outside the hunks, and the agent changing the last hunk's text.
    apply(
        &mut r,
        &mut cur,
        &TextEdit::insert(0, "typed "),
        Origin::User,
    );
    let at = cur.find("omega").unwrap();
    apply(&mut r, &mut cur, &TextEdit::insert(at, "big "), agent);
    r.recompute(&cur);
    assert_eq!(r.hunks[0].words, marker, "kept");
    assert_eq!(
        r.hunks[1].words, refined[1],
        "refined past the deadline before"
    );
    assert_ne!(r.hunks[2].words, marker, "new texts");
    assert!(r.meta.iter().all(|m| m.exact_words));
}

// ----- on_edits ---------------------------------------------------------------------------------

const WORDS: &[&str] = &["alpha", "$x$", "\\beta", "é∑", "", "a\rb", "x x", "$"];

/// A document of unique lines, long enough (up to ~2.5 KiB) that most of it lies beyond the reach
/// of any one edit (a few hundred bytes, see `batch.rs`).
fn long_document() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (prop::sample::select(WORDS), prop::sample::select(WORDS)),
        8..120,
    )
    .prop_map(|lines| {
        lines
            .iter()
            .enumerate()
            .map(|(i, (a, b))| format!("l{i} {a} {b}\n"))
            .collect()
    })
}

/// Texts for edits: short ones, line breaks, multibyte characters, and long ones that make hunks,
/// segments and attribution intervals reach past an edit's neighbourhood.
fn edit_text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::sample::select(vec!["", "x", "yz", "\n", "a\nb", "é", "\\(", "$"])
            .prop_map(str::to_owned),
        1 => (1usize..80, 0usize..3).prop_map(|(n, kind)| match kind {
            0 => "word ".repeat(n),
            1 => "new line\n".repeat(n / 4 + 1),
            _ => "∑é".repeat(n),
        }),
    ]
}

/// A replace_all of a common token, of any origin.
fn all_op() -> impl Strategy<Value = Op> {
    (
        0u8..5,
        prop::sample::select(vec!["$", "x", "\n", "é∑", " "]),
        prop::sample::select(vec!["\\(", "", "yy", "\n\n", "$"]),
    )
        .prop_map(|(origin, from, to)| Op::All { origin, from, to })
}

/// A random session step on a long document: most often one of [`op`]'s, else a long edit or a
/// replace_all of a common token.
fn wide_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => op(),
        2 => (0u8..5, any::<usize>(), 0usize..600, edit_text())
            .prop_map(|(origin, at, len, text)| Op::Edit { origin, at, len, text }),
        1 => all_op(),
    ]
}

/// The changes of one action, as `apply_edits` gets them (descending, disjoint).
#[derive(Clone, Debug)]
enum Action {
    /// Laid out from the end of the text down: each edit replaces `len` bytes ending `gap` bytes
    /// before the start of the one before it (adjacent edits, insertions at one point, edits
    /// within and beyond each other's reach).
    Spread(Vec<(usize, usize, String)>),
    /// Every occurrence of `from` replaced by `to`.
    All(&'static str, &'static str),
}

fn action() -> impl Strategy<Value = Action> {
    let gap = prop_oneof![0usize..4, 0usize..300, 0usize..1500];
    let len = prop_oneof![0usize..4, 0usize..40, 0usize..700];
    prop_oneof![
        3 => prop::collection::vec((gap, len, edit_text()), 2..24).prop_map(Action::Spread),
        1 => (
            prop::sample::select(vec!["$", "x", "\n", "l1", " ", "é∑"]),
            prop::sample::select(vec!["\\(", "", "zz", "\n", "$$"]),
        )
            .prop_map(|(from, to)| Action::All(from, to)),
    ]
}

fn action_edits(current: &str, action: &Action) -> Vec<TextEdit> {
    match action {
        Action::Spread(spread) => {
            let mut pos = current.len();
            let mut edits = Vec::new();
            for (gap, len, text) in spread {
                let b = floor(current, pos.saturating_sub(*gap));
                let a = floor(current, b.saturating_sub(*len));
                edits.push(TextEdit {
                    range: a..b,
                    text: text.clone(),
                });
                pos = a;
            }
            edits
        }
        Action::All(from, to) => crate::str_replace(current, from, to, true).unwrap_or_default(),
    }
}

/// Everything a review holds: its `Debug` form (every field), and the occurrence hints sorted
/// (a `HashMap` prints in no fixed order). Word spans are left out, with whether they were refined
/// in time: a recompute has 8 ms for them, and on a loaded machine one of two recomputes of the
/// same texts may run out.
fn state(r: &FileReview) -> (String, Vec<(u64, u64)>) {
    let mut r = r.clone();
    for h in &mut r.hunks {
        h.words.clear();
    }
    for m in &mut r.meta {
        m.exact_words = false;
    }
    let mut hints: Vec<(u64, u64)> = r.free_hints.drain().collect();
    hints.sort_unstable();
    (format!("{r:?}"), hints)
}

proptest! {
    #![proptest_config(config())]

    /// `on_edits` leaves a review exactly as `on_edit` for each edit in turn does (every field:
    /// hunks and their bookkeeping, attribution, segments, records, journal, events), whatever
    /// the review holds (a random session of every kind of step) and wherever the edits land,
    /// for every origin.
    #[test]
    fn on_edits_is_on_edit_for_each_edit(
        base in long_document(),
        history in prop::collection::vec(wide_op(), 0..16),
        action in action(),
        origin in 0u8..5,
    ) {
        let mut r = FileReview::new(base.clone());
        let mut current = base;
        for op in &history {
            play(&mut r, &mut current, op).map_err(TestCaseError::fail)?;
        }
        let edits = action_edits(&current, &action);
        let origin = origin_of(origin);
        let mut one_by_one = r.clone();
        for e in &edits {
            one_by_one.on_edit(e, origin);
        }
        r.on_edits(&edits, origin);
        prop_assert!(r.batch.is_none());
        prop_assert_eq!(state(&r), state(&one_by_one));
        lemma_core::apply_all(&mut current, &edits);
        check_model(&r, &current).map_err(TestCaseError::fail)?;
    }
}

/// A step after a decision that leaves it the newest one: an edit of any origin, a replace_all
/// or a recompute.
fn later_op() -> impl Strategy<Value = Op> {
    let len = prop_oneof![0usize..6, 0usize..600];
    prop_oneof![
        4 => (0u8..5, any::<usize>(), len, edit_text())
            .prop_map(|(origin, at, len, text)| Op::Edit { origin, at, len, text }),
        1 => all_op(),
        1 => Just(Op::Recompute),
    ]
}

proptest! {
    #![proptest_config(config())]

    /// Alt+U of an Accept all, or of an accept of several hunks, puts the regions back at once
    /// (`unaccept_batch`) exactly as one by one (every field of the review, then after a
    /// recompute), also when edits since moved some of them or cut into them.
    #[test]
    fn unaccept_batch_is_unaccept_each(
        base in long_document(),
        history in prop::collection::vec(wide_op(), 0..16),
        some in prop::option::of(0usize..2),
        later in prop::collection::vec(later_op(), 0..4),
    ) {
        let mut r = FileReview::new(base.clone());
        let mut current = base;
        for op in &history {
            let _ = play(&mut r, &mut current, op);
        }
        r.recompute(&current);
        match some {
            Some(n) => {
                let _ = play(&mut r, &mut current, &Op::AcceptSome(n));
            }
            None => r.accept_all(&current),
        }
        for op in &later {
            let _ = play(&mut r, &mut current, op);
        }
        let Some(Entry::Accept(regions)) = r.journal.pop() else {
            return Ok(());
        };
        let mut sorted = regions.clone();
        sorted.sort_by_key(|x| std::cmp::Reverse(x.cur.start));
        let mut one_by_one = r.clone();
        let any = r.unaccept(regions, &current);
        prop_assert!(r.batch.is_none());
        prop_assert_eq!(any, one_by_one.unaccept_each(sorted, &current));
        prop_assert_eq!(state(&r), state(&one_by_one));
        check_model(&r, &current).map_err(TestCaseError::fail)?;
        r.recompute(&current);
        one_by_one.recompute(&current);
        prop_assert_eq!(state(&r), state(&one_by_one));
    }
}

/// Accepts the hunks at `picked` (ascending) one by one, from the last one down, each moving the
/// baseline after it (`accept_at`), as the decisions did before `accept_many`.
fn accept_each(r: &mut FileReview, picked: &[usize]) {
    let mut regions = Vec::new();
    let mut events = Vec::new();
    for &k in picked.iter().rev() {
        let (region, event) = r.accept_at(k);
        regions.push(region);
        events.push(event);
    }
    events.reverse();
    r.events.extend(events);
    r.journal.push(Entry::Accept(regions));
}

proptest! {
    #![proptest_config(config())]

    /// Accept all, an accept of several hunks and Keep Turn rebuild the baseline once
    /// (`accept_many`) with exactly the result of accepting the hunks one by one (every field of
    /// the review, then after a recompute, and after Alt+U), with line hunks or provisional ones.
    #[test]
    fn accept_many_is_accept_each(
        base in long_document(),
        history in prop::collection::vec(wide_op(), 0..16),
        recompute in any::<bool>(),
        decision in 0usize..4,
        turn in any::<prop::sample::Index>(),
    ) {
        let mut r = FileReview::new(base.clone());
        let mut current = base;
        for op in &history {
            let _ = play(&mut r, &mut current, op);
        }
        if recompute {
            r.recompute(&current);
        }
        if r.check(&current, 0..r.hunks.len()).is_err() {
            return Ok(());
        }
        let picked: Vec<usize> = match decision {
            0 => (0..r.hunks.len()).collect(),
            1 | 2 => (decision - 1..r.hunks.len()).step_by(2).collect(),
            _ => {
                // A turn some hunk is of alone.
                let mut turns: Vec<u32> = r
                    .hunks
                    .iter()
                    .filter(|h| h.turns.len() == 1)
                    .map(|h| h.turns[0])
                    .collect();
                turns.sort_unstable();
                turns.dedup();
                let Some(&turn) = turns.get(turn.index(turns.len().max(1))) else {
                    return Ok(());
                };
                let clusters = r.clusters(turn, &current);
                let mut picked: Vec<usize> = clusters
                    .into_iter()
                    .filter(|c| c.keepable)
                    .flat_map(|c| c.hunks)
                    .collect();
                picked.sort_unstable();
                picked
            }
        };
        if picked.is_empty() {
            return Ok(());
        }
        let mut one_by_one = r.clone();
        r.accept_many(&picked, &current);
        accept_each(&mut one_by_one, &picked);
        prop_assert_eq!(state(&r), state(&one_by_one));
        check_model(&r, &current).map_err(TestCaseError::fail)?;
        r.recompute(&current);
        one_by_one.recompute(&current);
        prop_assert_eq!(state(&r), state(&one_by_one));
        let undone = r.undo_review_action(&current);
        prop_assert_eq!(undone, one_by_one.undo_review_action(&current));
        prop_assert_eq!(state(&r), state(&one_by_one));
        check_model(&r, &current).map_err(TestCaseError::fail)?;
    }
}
