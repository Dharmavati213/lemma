//! White-box tests: the exact-model invariant of `model.rs` under random edit sequences, the
//! attribution invariants of `turns.rs` (every hunk's `turns` are the turns whose intervals touch
//! it) and the shape of the segments of `segments.rs`.

use super::FileReview;
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

fn apply(r: &mut FileReview, current: &mut String, e: &TextEdit, origin: Origin) {
    r.on_edit(e, origin);
    e.apply(current);
}

fn run(base: &str, ops: &[Op]) -> Result<(), String> {
    let mut r = FileReview::new(base.to_owned());
    let mut current = base.to_owned();
    for op in ops {
        match op {
            Op::Edit {
                origin,
                at,
                len,
                text,
            } => {
                let a = floor(&current, at % (current.len() + 1));
                let b = floor(&current, a + len);
                let origin = match origin {
                    0 => Origin::User,
                    1 => Origin::History,
                    2 => Origin::Review,
                    3 => Origin::Reload,
                    _ => Origin::Agent { call: 1 },
                };
                let e = TextEdit {
                    range: a..b,
                    text: text.clone(),
                };
                apply(&mut r, &mut current, &e, origin);
            }
            Op::Recompute => {
                r.recompute(&current);
            }
            Op::Accept(k) if !r.hunks.is_empty() => {
                let id = r.hunks[k % r.hunks.len()].id;
                r.accept(id, &current).map_err(|e| format!("accept: {e}"))?;
            }
            Op::Reject(k) if !r.hunks.is_empty() => {
                let id = r.hunks[k % r.hunks.len()].id;
                let e = r.reject(id, &current).map_err(|e| format!("reject: {e}"))?;
                apply(&mut r, &mut current, &e, Origin::Review);
            }
            Op::Undo => match r.undo_review_action(&current) {
                Some(crate::UndoAction::Edits(edits)) => {
                    for e in &edits {
                        apply(&mut r, &mut current, e, Origin::Review);
                    }
                }
                Some(crate::UndoAction::Unaccepted) | None => {}
            },
            Op::Turn => r.begin_turn(),
            Op::SetTurn(t) => r.set_turn(*t),
            Op::AcceptTurn(t) => {
                let before = current.clone();
                r.accept_turn(*t, &current);
                if current != before {
                    return Err("accept_turn changed the text".into());
                }
                // Only hunks holding more than the turn's change may stay.
                if r.check(&current, 0..r.hunks.len()).is_ok()
                    && r.clusters(*t, &current).iter().any(|c| c.keepable)
                {
                    return Err(format!(
                        "accept_turn({t}) left hunks that are the turn's alone"
                    ));
                }
            }
            Op::RejectTurn(t) => {
                let edits = r.reject_turn(*t, &current);
                for e in &edits {
                    apply(&mut r, &mut current, e, Origin::Review);
                }
            }
            Op::AcceptSome(n) => {
                let ids: Vec<_> = r.hunks.iter().skip(*n).step_by(2).map(|h| h.id).collect();
                let accepted = r.accept_hunks(&ids, &current);
                if accepted != 0 && accepted != ids.len() {
                    return Err("accept_hunks accepted only some".into());
                }
                if accepted != 0 && ids.iter().any(|id| r.hunk(*id).is_some()) {
                    return Err("accept_hunks left a hunk".into());
                }
            }
            Op::RejectSome(n) => {
                let ids: Vec<_> = r.hunks.iter().skip(*n).step_by(2).map(|h| h.id).collect();
                let edits = r.reject_hunks(&ids, &current);
                for e in &edits {
                    apply(&mut r, &mut current, e, Origin::Review);
                }
            }
            Op::Accept(_) | Op::Reject(_) => {}
        }
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

    /// Any mix of edits (every origin), recomputes, decisions and undos keeps the model exact.
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
