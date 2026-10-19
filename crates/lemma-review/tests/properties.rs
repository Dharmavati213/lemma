//! The WP1.1 acceptance properties (ARCHITECTURE §10.7). 2000 cases each by default;
//! `PROPTEST_CASES` overrides it.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit};
use lemma_review::{EditClass, FileReview, Hunk, minimal_edits};
use proptest::prelude::*;
use proptest::sample::Index;

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    ProptestConfig {
        // Property 5 skips sessions whose edits merged into a single hunk.
        max_global_rejects: cases * 4,
        ..ProptestConfig::with_cases(cases)
    }
}

const WORDS: &[&str] = &["alpha", "x", "\\beta", "é∑", "$f$", "\\\\", "a\rb", ""];

/// A document of unique lines (`l<i> …`), so line alignment is never ambiguous.
fn document() -> impl Strategy<Value = String> {
    document_of(1..16)
}

fn document_of(lines: std::ops::Range<usize>) -> impl Strategy<Value = String> {
    (
        prop::collection::vec(
            (prop::sample::select(WORDS), prop::sample::select(WORDS)),
            lines,
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

/// An agent change, applied with the `write` tool's `minimal_edits`.
#[derive(Clone, Debug)]
enum AgentOp {
    /// Replace `len` lines at `at` by `new` fresh lines (insertion if `len == 0`, deletion if
    /// `new == 0`).
    Lines { at: Index, len: usize, new: usize },
    /// Change the last word of a line.
    Word { line: Index, word: &'static str },
}

/// A `Lines` op that changes the line count.
fn resizing_op() -> impl Strategy<Value = AgentOp> {
    (any::<Index>(), 0usize..3, 0usize..3)
        .prop_filter("resizes", |(_, len, new)| len != new)
        .prop_map(|(at, len, new)| AgentOp::Lines { at, len, new })
}

fn agent_op() -> impl Strategy<Value = AgentOp> {
    prop_oneof![
        (any::<Index>(), 0usize..3, 0usize..3).prop_map(|(at, len, new)| AgentOp::Lines {
            at,
            len,
            new
        }),
        (any::<Index>(), prop::sample::select(WORDS))
            .prop_map(|(line, word)| AgentOp::Word { line, word }),
    ]
}

/// The text after `op`; fresh lines are numbered from `serial` so they stay unique.
fn changed(text: &str, op: &AgentOp, serial: &mut usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    match op {
        AgentOp::Lines { at, len, new } => {
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
        AgentOp::Word { line, word } => {
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

/// Applies agent ops to a fresh review; recomputes after each op whose bit is set in `mask`,
/// and always at the end.
fn agent_session(base: &str, ops: &[AgentOp], mask: u32) -> Session {
    let mut s = Session::new(base);
    let mut serial = 0;
    for (i, op) in ops.iter().enumerate() {
        let target = changed(&s.text, op, &mut serial);
        let edits = minimal_edits(&s.text, &target);
        s.edits(&edits, Origin::Agent { call: i as u64 });
        assert_eq!(s.text, target);
        if mask & (1 << i) != 0 {
            s.recompute();
        }
    }
    s.recompute();
    s.check();
    s
}

/// A user edit between hunks; positions are chosen inside the gaps of the current hunks.
#[derive(Clone, Debug)]
enum UserOp {
    /// Insert ` u<k>` after a word of a line.
    Token {
        gap: Index,
        line: Index,
        word: Index,
    },
    /// Insert the line `u<k>` before a line.
    NewLine { gap: Index, line: Index },
    /// Delete a whole line.
    DeleteLine { gap: Index, line: Index },
    /// Delete one word (not the line's id).
    DeleteWord {
        gap: Index,
        line: Index,
        word: Index,
    },
}

fn user_op() -> impl Strategy<Value = UserOp> {
    prop_oneof![
        (any::<Index>(), any::<Index>(), any::<Index>())
            .prop_map(|(gap, line, word)| UserOp::Token { gap, line, word }),
        (any::<Index>(), any::<Index>()).prop_map(|(gap, line)| UserOp::NewLine { gap, line }),
        (any::<Index>(), any::<Index>()).prop_map(|(gap, line)| UserOp::DeleteLine { gap, line }),
        (any::<Index>(), any::<Index>(), any::<Index>())
            .prop_map(|(gap, line, word)| UserOp::DeleteWord { gap, line, word }),
    ]
}

/// Byte ranges of the whole lines of `text` lying in the unchanged gap number `gap`, and whether
/// that gap separates two hunks.
fn gap_lines(text: &str, hunks: &[Hunk], gap: &Index) -> (Vec<std::ops::Range<usize>>, bool) {
    let g = gap.index(hunks.len() + 1);
    let lo = if g == 0 {
        0
    } else {
        hunks[g - 1].cur_bytes.end
    };
    let hi = hunks.get(g).map_or(text.len(), |h| h.cur_bytes.start);
    let mut out = Vec::new();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let r = pos..pos + line.len();
        pos = r.end;
        if r.start >= lo && r.end <= hi {
            out.push(r);
        }
    }
    (out, g > 0 && g < hunks.len())
}

/// The edit for `op`, if the chosen gap has a suitable line.
fn user_edit(text: &str, hunks: &[Hunk], op: &UserOp, serial: usize) -> Option<TextEdit> {
    let (gap, line) = match op {
        UserOp::Token { gap, line, .. }
        | UserOp::NewLine { gap, line }
        | UserOp::DeleteLine { gap, line }
        | UserOp::DeleteWord { gap, line, .. } => (gap, line),
    };
    let (lines, inner) = gap_lines(text, hunks, gap);
    if lines.is_empty() {
        return None;
    }
    // Deleting the last unchanged line between two hunks joins them into one line hunk, which is
    // how line diffs work rather than a change made by the review.
    if inner && lines.len() == 1 && matches!(op, UserOp::DeleteLine { .. }) {
        return None;
    }
    let r = lines[line.index(lines.len())].clone();
    let body = text[r.clone()].trim_end_matches('\n');
    // Word boundaries after the line id: offsets of ' ' in the body, plus its end.
    let spaces: Vec<usize> = body.match_indices(' ').map(|(i, _)| i).collect();
    Some(match op {
        UserOp::Token { word, .. } => {
            let mut ends: Vec<usize> = spaces.get(1..).unwrap_or_default().to_vec();
            ends.push(body.len());
            TextEdit::insert(
                r.start + ends[word.index(ends.len())],
                format!(" u{serial}"),
            )
        }
        UserOp::NewLine { .. } => TextEdit::insert(r.start, format!("u{serial}\n")),
        UserOp::DeleteLine { .. } => TextEdit::delete(r),
        UserOp::DeleteWord { word, .. } => {
            if spaces.len() < 2 {
                return None;
            }
            // Delete " word" for one of the words after the id.
            let k = 1 + word.index(spaces.len() - 1);
            let end = spaces.get(k + 1).copied().unwrap_or(body.len());
            TextEdit::delete(r.start + spaces[k]..r.start + end)
        }
    })
}

proptest! {
    #![proptest_config(config())]

    /// 1. After random agent edits, accept_all ⇒ baseline == current and is_clean.
    #[test]
    fn accept_all_makes_the_baseline_current(
        base in document(), ops in prop::collection::vec(agent_op(), 1..8), mask in any::<u32>(),
    ) {
        let mut s = agent_session(&base, &ops, mask);
        s.review.accept_all(&s.text);
        prop_assert_eq!(s.review.baseline(), s.text.as_str());
        prop_assert!(s.review.is_clean());
        s.recompute();
        prop_assert!(s.hunks().is_empty());
    }

    /// 2. Applying reject_all edits ⇒ current == baseline.
    #[test]
    fn reject_all_restores_the_baseline(
        base in document(), ops in prop::collection::vec(agent_op(), 1..8), mask in any::<u32>(),
    ) {
        let mut s = agent_session(&base, &ops, mask);
        prop_assert_eq!(s.review.baseline(), base.as_str());
        s.reject_all();
        prop_assert_eq!(s.text.as_str(), base.as_str());
        prop_assert!(s.review.is_clean());
        prop_assert!(s.hunks().is_empty());
    }

    /// 3. Random User edits outside hunks never create or alter hunks.
    #[test]
    fn user_edits_outside_hunks_leave_hunks_alone(
        base in document(),
        ops in prop::collection::vec(agent_op(), 1..6),
        user in prop::collection::vec(user_op(), 1..12),
        mask in any::<u32>(),
    ) {
        let mut s = agent_session(&base, &ops, u32::MAX);
        let (ids, pairs) = (s.ids(), s.pairs());
        for (i, op) in user.iter().enumerate() {
            let Some(e) = user_edit(&s.text, s.hunks(), op, i) else { continue };
            prop_assert_eq!(s.review.on_edit(&e, Origin::User), EditClass::Outside, "{:?}", e);
            e.apply(&mut s.text);
            if mask & (1 << i) != 0 {
                let delta = s.recompute();
                prop_assert!(delta.added.is_empty() && delta.removed.is_empty(), "{:?}", delta);
            }
            s.check();
        }
        let delta = s.recompute();
        prop_assert!(delta.added.is_empty() && delta.removed.is_empty(), "{:?}", delta);
        prop_assert_eq!(s.ids(), ids);
        prop_assert_eq!(s.pairs(), pairs);
        prop_assert!(s.review.drain_events().is_empty());
        s.check();
    }

    /// 3b. A User edit outside hunks followed by its GTK undo (History) changes nothing: the
    /// baseline, the text and the hunk ids are as before, even when the edit touched a hunk's
    /// boundary (`dd` on the line above a hunk, then `u`).
    #[test]
    fn undoing_a_user_edit_outside_hunks_changes_nothing(
        base in document(),
        ops in prop::collection::vec(agent_op(), 1..6),
        op in user_op(),
        recompute_between in any::<bool>(),
    ) {
        let mut s = agent_session(&base, &ops, u32::MAX);
        let (ids, baseline, text) = (s.ids(), s.review.baseline().to_owned(), s.text.clone());
        let Some(e) = user_edit(&s.text, s.hunks(), &op, 0) else { return Ok(()) };
        let removed = s.text[e.range.clone()].to_owned();
        s.user(e.clone());
        if recompute_between {
            s.recompute();
        }
        // GTK's undo of the edit.
        if !e.text.is_empty() {
            let at = e.range.start;
            s.edit(&TextEdit::delete(at..at + e.text.len()), Origin::History);
        }
        if !removed.is_empty() {
            s.edit(&TextEdit::insert(e.range.start, removed), Origin::History);
        }
        s.recompute();
        s.check();
        prop_assert_eq!(&s.text, &text);
        prop_assert_eq!(s.review.baseline(), baseline.as_str());
        prop_assert_eq!(s.ids(), ids);
        prop_assert!(s.hunks().iter().all(|h| !h.user_touched));
    }

    /// 4. A Reject followed by a simulated GTK undo (delete the restored range, insert the agent
    /// text at its start, both History) ⇒ a hunk with the same HunkId.
    #[test]
    fn undoing_a_reject_reopens_the_same_hunk(
        base in document(), ops in prop::collection::vec(agent_op(), 1..8), pick in any::<Index>(),
    ) {
        let mut s = agent_session(&base, &ops, 0);
        prop_assume!(!s.hunks().is_empty());
        let ids = s.ids();
        let id = ids[pick.index(ids.len())];
        let (edit, replaced) = s.reject(id);
        prop_assert!(s.review.hunk(id).is_none());
        s.undo_replace(&edit, &replaced);
        s.recompute();
        s.check();
        prop_assert!(s.review.hunk(id).is_some());
        prop_assert_eq!(s.ids(), ids);
    }

    /// 5. IDs of untouched hunks survive accepting or rejecting another hunk that changes the
    /// line count.
    #[test]
    fn other_ids_survive_decisions(
        base in document_of(4..16),
        first in resizing_op(),
        more in prop::collection::vec(agent_op(), 1..7),
        pick in any::<Index>(),
        accept in any::<bool>(),
    ) {
        let ops: Vec<AgentOp> = std::iter::once(first).chain(more).collect();
        let mut s = agent_session(&base, &ops, 0);
        let resizing: Vec<&Hunk> =
            s.hunks().iter().filter(|h| h.cur_lines.len() != h.base_lines.len()).collect();
        prop_assume!(!resizing.is_empty() && s.hunks().len() > 1);
        let id = resizing[pick.index(resizing.len())].id;
        let mut expected = s.ids();
        expected.retain(|&i| i != id);
        if accept {
            s.accept(id);
        } else {
            s.reject(id);
        }
        s.check();
        prop_assert_eq!(s.ids(), expected);
    }

    /// 6. from_persisted(to_persisted) reproduces the hunks; a different disk text ⇒ None.
    #[test]
    fn persistence_reproduces_the_hunks(
        base in document(), ops in prop::collection::vec(agent_op(), 1..8), extra in "[a-z\n]{1,3}",
    ) {
        let s = agent_session(&base, &ops, 0);
        let p = s.review.to_persisted("chapters/x.tex".into(), &s.text);
        let restored = FileReview::from_persisted(&p, &s.text).expect("same disk text");
        prop_assert_eq!(restored.hunks(), s.hunks());
        prop_assert_eq!(restored.baseline(), s.review.baseline());
        let other = format!("{}{extra}", s.text);
        prop_assert!(FileReview::from_persisted(&p, &other).is_none());
    }
}

/// One agent tool call: its edits (descending) and the text each one replaced.
type Action = (Vec<TextEdit>, Vec<String>);

/// Like `agent_session`, also returning the actions, for simulating buffer undo.
fn recorded_agent_session(base: &str, ops: &[AgentOp], mask: u32) -> (Session, Vec<Action>) {
    let mut s = Session::new(base);
    let mut serial = 0;
    let mut actions = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let target = changed(&s.text, op, &mut serial);
        let edits = minimal_edits(&s.text, &target);
        let replaced = edits
            .iter()
            .map(|e| s.text[e.range.clone()].to_owned())
            .collect();
        s.edits(&edits, Origin::Agent { call: i as u64 });
        actions.push((edits, replaced));
        if mask & (1 << i) != 0 {
            s.recompute();
        }
    }
    s.recompute();
    (s, actions)
}

proptest! {
    #![proptest_config(config())]

    /// 7. Undoing every agent action with the buffer's undo (History) leaves nothing to review,
    /// and the agent hears about it.
    #[test]
    fn buffer_undo_of_agent_edits_cleans_the_review(
        base in document(), ops in prop::collection::vec(agent_op(), 1..6), mask in any::<u32>(),
    ) {
        let (mut s, actions) = recorded_agent_session(&base, &ops, mask);
        let had_hunks = !s.hunks().is_empty();
        for (i, (edits, replaced)) in actions.iter().enumerate().rev() {
            s.undo_action(edits, replaced);
            if mask & (1 << (i + 8)) != 0 {
                s.recompute();
            }
        }
        s.recompute();
        prop_assert_eq!(s.text.as_str(), base.as_str());
        prop_assert_eq!(s.review.baseline(), base.as_str());
        prop_assert!(s.review.is_clean(), "{:?}", s.hunks());
        let undone = s
            .review
            .drain_events()
            .iter()
            .any(|e| matches!(e, lemma_review::ReviewEvent::UndoneByUser { .. }));
        prop_assert!(undone || !had_hunks);
    }

    /// 8. Redoing them all afterwards brings back the same hunks.
    #[test]
    fn buffer_redo_of_agent_edits_restores_the_hunks(
        base in document(), ops in prop::collection::vec(agent_op(), 1..6),
    ) {
        let (mut s, actions) = recorded_agent_session(&base, &ops, 0);
        let ids = s.ids();
        for (edits, replaced) in actions.iter().rev() {
            s.undo_action(edits, replaced);
        }
        s.recompute();
        for (edits, _) in &actions {
            s.edits(edits, Origin::History);
        }
        s.recompute();
        prop_assert_eq!(s.review.baseline(), base.as_str());
        prop_assert_eq!(s.ids(), ids);
    }
}
