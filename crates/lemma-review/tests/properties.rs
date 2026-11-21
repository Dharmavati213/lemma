//! The WP1.1 acceptance properties (ARCHITECTURE §10.7). 2000 cases each by default;
//! `PROPTEST_CASES` overrides it.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit};
use lemma_review::{EditClass, FileReview, Hunk, HunkId, UndoAction, minimal_edits, str_replace};
use proptest::prelude::*;
use proptest::sample::Index;
use std::collections::HashMap;

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
        // But the word spans: each side's recompute has 8 ms for them, which a loaded machine
        // can run out of on one side only.
        let without_words = |hunks: &[Hunk]| -> Vec<Hunk> {
            hunks.iter().map(|h| Hunk { words: Vec::new(), ..h.clone() }).collect()
        };
        prop_assert_eq!(without_words(restored.hunks()), without_words(s.hunks()));
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

/// Fewer cases for property 9, whose documents have hundreds of lines (a fortieth, at least 10).
fn big_config() -> ProptestConfig {
    let cases = config().cases;
    ProptestConfig::with_cases((cases / 40).max(10))
}

/// A document of `pairs` lines `l<i> …` (with two spaces each), each followed by a line `m<i>`
/// without one, so that changing every space leaves a hunk per pair.
fn spaced_document(pairs: std::ops::Range<usize>) -> impl Strategy<Value = String> {
    prop::collection::vec(
        (prop::sample::select(WORDS), prop::sample::select(WORDS)),
        pairs,
    )
    .prop_map(|lines| {
        lines
            .iter()
            .enumerate()
            .map(|(i, (a, b))| format!("l{i} {a} {b}\nm{i}\n"))
            .collect()
    })
}

/// The changes of `edits` as an open buffer reports them: a deletion, then an insertion, for each
/// (descending, as `apply_edits` makes them).
fn as_open_buffer(edits: &[TextEdit]) -> Vec<TextEdit> {
    edits
        .iter()
        .flat_map(|e| {
            let delete = (!e.range.is_empty()).then(|| TextEdit::delete(e.range.clone()));
            let insert = (!e.text.is_empty()).then(|| TextEdit::insert(e.range.start, &e.text));
            delete.into_iter().chain(insert)
        })
        .collect()
}

proptest! {
    #![proptest_config(big_config())]

    /// 9. Properties 7 and 8 for an action with more changes than the review records of older
    /// turns (512): a replace_all of the spaces of hundreds of lines (two a line), between
    /// smaller actions, reported as a closed document or an open buffer reports it.
    #[test]
    fn buffer_undo_and_redo_of_an_action_beyond_the_record_cap(
        base in spaced_document(260..300),
        before in prop::collection::vec(agent_op(), 0..3),
        after in prop::collection::vec(agent_op(), 0..3),
        to in prop::sample::select(vec!["  ", "_", ""]),
        open in any::<bool>(),
    ) {
        let (mut s, mut actions) = recorded_agent_session(&base, &before, 0);
        let all = lemma_review::str_replace(&s.text, " ", to, true).unwrap();
        let edits = if open { as_open_buffer(&all) } else { all };
        prop_assert!(edits.len() > 512);
        let replaced: Vec<String> = edits
            .iter()
            .map(|e| s.text[e.range.clone()].to_owned())
            .collect();
        s.edits(&edits, Origin::Agent { call: 99 });
        s.recompute();
        actions.push((edits, replaced));
        let mut serial = 1000;
        for (i, op) in after.iter().enumerate() {
            let target = changed(&s.text, op, &mut serial);
            let edits = minimal_edits(&s.text, &target);
            let replaced = edits
                .iter()
                .map(|e| s.text[e.range.clone()].to_owned())
                .collect();
            s.edits(&edits, Origin::Agent { call: 100 + i as u64 });
            actions.push((edits, replaced));
        }
        s.recompute();
        let ids = s.ids();
        s.review.drain_events();

        for (edits, replaced) in actions.iter().rev() {
            s.undo_action(edits, replaced);
        }
        s.recompute();
        prop_assert_eq!(s.text.as_str(), base.as_str());
        prop_assert_eq!(s.review.baseline(), base.as_str());
        prop_assert!(s.review.is_clean(), "{} hunks left", s.hunks().len());
        let edited = s
            .review
            .drain_events()
            .into_iter()
            .find(|e| matches!(e, lemma_review::ReviewEvent::UserEditedInHunk { .. }));
        prop_assert!(edited.is_none(), "the undo looked like the user's: {:?}", edited);

        for (edits, _) in &actions {
            s.edits(edits, Origin::History);
        }
        s.recompute();
        prop_assert_eq!(s.review.baseline(), base.as_str());
        prop_assert_eq!(s.ids(), ids);
    }
}

/// Starting texts for properties 10 and 11: empty, with and without a final newline, multibyte
/// and `\r`, and one with a text on several lines for the agent to replace them all.
const BASES: &[&str] = &[
    "",
    "a\n",
    "a\nb\nc\n",
    "one\ntwo\nthree\nfour\n",
    "x\ny",
    "é\n∑\r\nz",
    "a a\nb a\na\n",
];

/// Texts the agent and the user write in properties 10 and 11.
const TEXTS: &[&str] = &["", "x", "yz", "\n", "a\nb", "é", "\n\n", "∑\n", "\r\n"];

/// Texts the agent replaces everywhere in properties 10 and 11.
const REPLACED: &[&str] = &["a", "\n", "é", "x"];

/// One step of a session in an open buffer with its undo history.
#[derive(Clone, Debug)]
enum Step {
    /// The agent replaces `len` bytes at `at` (taken modulo the text) by `text`.
    Agent {
        at: usize,
        len: usize,
        text: &'static str,
    },
    /// The agent replaces every `from` by `to` (a replace_all): one action of several edits.
    AgentAll {
        from: &'static str,
        to: &'static str,
    },
    /// The agent's edits from now on are those of this turn.
    Turn(u32),
    /// Reject the hunk at this index (taken modulo the hunks).
    Reject(usize),
    RejectAll,
    /// Reject Turn: the turn's own edits taken back, as one action.
    RejectTurn(u32),
    /// Alt+U: the last accept or reject reverted, a new action of the buffer's.
    AltU,
    /// The user types `text` over `len` bytes at `at` (a deletion, then an insertion).
    User {
        at: usize,
        len: usize,
        text: &'static str,
    },
    /// The user types the keys one by one at `at`, which the buffer merges into one action:
    /// its undo deletes them at once.
    Typing {
        at: usize,
        keys: Vec<&'static str>,
    },
    /// Backspace `n` times before `at`, merged into one action as well.
    Backspace {
        at: usize,
        n: usize,
    },
    /// The Delete key `n` times at `at`, merged into one action as well.
    Delete {
        at: usize,
        n: usize,
    },
    /// Ctrl+Z and Ctrl+Shift+Z, which the review is told of.
    Undo,
    Redo,
    Recompute,
    /// Properties 12 and 13: the agent replaces `len` bytes at `at` (taken modulo the text, or
    /// within the hunk at index `over` when there is one, both taken modulo) by `text` followed
    /// by a marker (see [`marker`]).
    AgentMarked {
        over: Option<usize>,
        at: usize,
        len: usize,
        text: &'static str,
    },
    /// Alt+Y on the hunk at this index (taken modulo the hunks), on the hunks as they are
    /// (`fresh` false: typing 50 ms ago left them provisional) or recomputed first. In a closed
    /// document, a row of the Changes panel.
    Accept {
        pick: usize,
        fresh: bool,
    },
    /// Alt+N on the hunks as they are (`Reject` recomputes first, as a row of the Changes panel
    /// does).
    RejectNow(usize),
    /// Accept all of the document (Alt+Shift+Y), on the hunks as they are.
    AcceptAll,
    /// Keep Turn.
    KeepTurn(u32),
    /// Rows of the Changes panel kept, or rejected, as one decision: the hunks whose index
    /// (modulo 8) has its bit set.
    AcceptSome(u8),
    RejectSome(u8),
    /// Property 13: Ctrl+S.
    Save,
    /// Property 13: the tab closes: the document stays, closed, with its review (dropped if
    /// nothing is left to review); unsaved changes are saved first or, with `discard`, the
    /// closed document goes back to the file's text.
    Close {
        discard: bool,
    },
    /// Property 13: the document is opened again, in a new buffer without undo history.
    Open,
    /// Property 13: another program writes the file outside an agent turn: its text with `len`
    /// bytes at `at` replaced by `text` and a marker. A document without unsaved changes
    /// reloads (as the user's own change when nothing is under review); one with them shows the
    /// conflict bar, whose Reload (`keep_mine` false) reloads as the user's own edit.
    Reload {
        at: usize,
        len: usize,
        text: &'static str,
        keep_mine: bool,
    },
}

/// The steps of property 10, and with `user` those of property 11 too.
fn step(user: bool) -> BoxedStrategy<Step> {
    let edit = || (any::<usize>(), 0usize..8, prop::sample::select(TEXTS));
    let agent = edit().prop_map(|(at, len, text)| Step::Agent { at, len, text });
    let all = (prop::sample::select(REPLACED), prop::sample::select(TEXTS))
        .prop_map(|(from, to)| Step::AgentAll { from, to });
    let mut steps: Vec<(u32, BoxedStrategy<Step>)> = vec![
        (4, agent.boxed()),
        (1, all.boxed()),
        (1, (0u32..3).prop_map(Step::Turn).boxed()),
        (1, any::<usize>().prop_map(Step::Reject).boxed()),
        (1, Just(Step::RejectAll).boxed()),
        (1, (0u32..3).prop_map(Step::RejectTurn).boxed()),
        (1, Just(Step::AltU).boxed()),
        (4, Just(Step::Undo).boxed()),
        (2, Just(Step::Redo).boxed()),
        (1, Just(Step::Recompute).boxed()),
    ];
    if user {
        let key = prop::sample::select(&TEXTS[1..]);
        steps.extend([
            (
                2,
                edit()
                    .prop_map(|(at, len, text)| Step::User {
                        at,
                        len: len % 4,
                        text,
                    })
                    .boxed(),
            ),
            (
                1,
                (any::<usize>(), prop::collection::vec(key, 2..4))
                    .prop_map(|(at, keys)| Step::Typing { at, keys })
                    .boxed(),
            ),
            (
                1,
                (any::<usize>(), 2usize..4)
                    .prop_map(|(at, n)| Step::Backspace { at, n })
                    .boxed(),
            ),
            (
                1,
                (any::<usize>(), 2usize..4)
                    .prop_map(|(at, n)| Step::Delete { at, n })
                    .boxed(),
            ),
        ]);
    }
    prop::strategy::Union::new_weighted(steps).boxed()
}

/// One undoable action of the buffer: its edits as applied, each with the text it replaced.
type BufferAction = Vec<(TextEdit, String)>;

/// A buffer with its undo and redo stacks; every change is reported as an open buffer reports
/// it (a deletion, then an insertion). With `gui`, the review is recomputed after each undo and
/// redo, as the GUI does before the next frame. The review checks its invariants inside every
/// change, and against the text after it.
struct Buffer {
    s: Session,
    undo: Vec<BufferAction>,
    redo: Vec<BufferAction>,
    gui: bool,
    /// Properties 12 and 13: the document around the buffer, as the app drives the review.
    app: Option<App>,
}

/// What properties 12 and 13 follow of the app besides the buffer (`review_ui`, `Document`,
/// the file monitor), to call the review as it does.
struct App {
    /// The window's decisions not reverted yet: Alt+U reverts the newest
    /// (`review_ui::undo_last`, which reverts only the document's newest journal entry).
    decisions: usize,
    /// The document's tab is closed: its text is in no buffer.
    closed: bool,
    /// The document has a review: a review with nothing left to decide is dropped when its tab
    /// closes, and before another program's change reloads it; the next agent edit starts one.
    reviewing: bool,
    /// The file's text.
    disk: String,
    /// The window's agent turn.
    turn: u32,
    /// Markers handed out so far: the `n`-th marks the `n`-th marked edit (see [`marker`]).
    marked: u32,
    /// Which marked edit wrote each byte of the text (0: none), and the label the text inserted
    /// from now on takes.
    labels: Vec<u32>,
    label: u32,
    /// The labels before each action of the buffer's undo stack, and after each of its redo
    /// stack: the buffer's undo and redo give them back (its history is last in, first out).
    undo_labels: Vec<Vec<u32>>,
    redo_labels: Vec<Vec<u32>>,
    /// Marked edits some of whose text an accept took (how many accepts not reverted), whose
    /// text is the user's from then on: undoing them, or the user's deletion of part of them,
    /// is the user's own change in the baseline (see `FileReview::accept`).
    accepted: HashMap<u32, u32>,
    /// Marked edits whose markers the user's own edits wrote (Reload on the conflict bar, a
    /// tab closed discarding changes), or that were in the text a review started with.
    owned: HashMap<u32, u32>,
    /// The document's Alt+U journal.
    journal: Vec<Decided>,
    /// Agent edits without a marker so far: they are labelled from 0x100 on.
    unmarked: u32,
    /// The text when the buffer's history started (the session's start, the reopened tab):
    /// undoing every action gives it back.
    history_text: String,
    /// The baseline then, if the review has been there since and no accept (or Alt+U of one)
    /// changed it since: undoing every action gives it back. After an accept, undoing an agent
    /// edit whose text it took is the user's own change, but an edit of the agent's whose text
    /// it took only part of (a later agent edit had taken out the rest) is taken out as a
    /// proposal.
    history_baseline: Option<String>,
}

/// An entry of the document's Alt+U journal, as properties 12 and 13 follow it.
#[derive(Debug)]
enum Decided {
    /// The text of each hunk an accept took, with the agent edits whose text it held.
    Accept(Vec<(String, Vec<u32>)>),
    /// The texts a reject took out, with their labels: Alt+U puts them back.
    Reject(Vec<(String, Vec<u32>)>),
}

/// The `n`-th marker: a character no other text of the properties has, appended to the agent's
/// text in properties 12 and 13 (and to another program's), so that the baseline can be
/// searched for it.
fn marker(n: u32) -> char {
    assert!(n < 0x100, "too many markers");
    char::from_u32(0x2460 + n).expect("a character")
}

/// The number of the marker `c`, if it is one.
fn marked(c: char) -> Option<u32> {
    ('\u{2460}'..'\u{2560}')
        .contains(&c)
        .then(|| c as u32 - 0x2460)
}

impl Buffer {
    fn new(base: &str, gui: bool) -> Self {
        let mut s = Session::new(base);
        s.review.check_every_change();
        Self {
            s,
            undo: Vec::new(),
            redo: Vec::new(),
            gui,
            app: None,
        }
    }

    /// A buffer in the app (properties 12 and 13).
    fn in_app(base: &str, gui: bool) -> Self {
        let mut b = Self::new(base, gui);
        b.app = Some(App {
            decisions: 0,
            closed: false,
            reviewing: true,
            disk: base.to_owned(),
            turn: 0,
            marked: 0,
            labels: vec![0; base.len()],
            label: 0,
            undo_labels: Vec::new(),
            redo_labels: Vec::new(),
            accepted: HashMap::new(),
            owned: HashMap::new(),
            journal: Vec::new(),
            unmarked: 0,
            history_text: base.to_owned(),
            history_baseline: Some(base.to_owned()),
        });
        b
    }

    /// Whether the document has a review (always, outside properties 12 and 13).
    fn reviewing(&self) -> bool {
        self.app.as_ref().is_none_or(|app| app.reviewing)
    }

    fn closed(&self) -> bool {
        self.app.as_ref().is_some_and(|app| app.closed)
    }

    /// Panics when the review does not describe the text exactly; `after` names the change.
    fn check(&self, after: impl std::fmt::Debug) {
        if !self.reviewing() {
            return;
        }
        if let Err(e) = self.s.review.check_invariants(&self.s.text) {
            panic!("{e} after {after:?}");
        }
    }

    /// One buffer change.
    fn edit(&mut self, e: &TextEdit, origin: Origin) {
        if self.reviewing() {
            self.s.edit(e, origin);
        } else {
            e.apply(&mut self.s.text);
        }
        self.relabel(std::slice::from_ref(e));
        self.check((e, origin));
    }

    /// Several changes at once, as `Document::apply_edits` reports them.
    fn edits(&mut self, edits: &[TextEdit], origin: Origin) {
        if self.reviewing() {
            self.s.edits(edits, origin);
        } else {
            lemma_core::apply_all(&mut self.s.text, edits);
        }
        self.relabel(edits);
        self.check((edits, origin));
    }

    /// Follows the changes `edits` (descending) in the labels: their text takes `App::label`.
    fn relabel(&mut self, edits: &[TextEdit]) {
        if let Some(app) = &mut self.app {
            for e in edits {
                let label = app.label;
                app.labels
                    .splice(e.range.clone(), std::iter::repeat_n(label, e.text.len()));
            }
        }
    }

    fn recompute(&mut self) {
        if !self.reviewing() {
            return;
        }
        self.s.recompute();
        self.check("a recompute");
    }

    fn change(&mut self, e: &TextEdit, origin: Origin) {
        if !e.range.is_empty() {
            self.edit(&TextEdit::delete(e.range.clone()), origin);
        }
        if !e.text.is_empty() {
            self.edit(&TextEdit::insert(e.range.start, &e.text), origin);
        }
    }

    /// Applies descending `edits` as one action.
    fn act(&mut self, edits: &[TextEdit], origin: Origin) {
        if edits.is_empty() {
            return;
        }
        let mut action = Vec::new();
        for e in edits {
            action.push((e.clone(), self.s.text[e.range.clone()].to_owned()));
            self.change(e, origin);
        }
        self.done(action);
        if origin != Origin::User {
            self.recompute();
        }
    }

    /// Applies descending `edits` as one action, reported at once as an open buffer reports
    /// them (`Document::apply_edits`).
    fn act_at_once(&mut self, edits: &[TextEdit], origin: Origin) {
        if edits.is_empty() {
            return;
        }
        let action: BufferAction = edits
            .iter()
            .map(|e| (e.clone(), self.s.text[e.range.clone()].to_owned()))
            .collect();
        self.edits(&as_open_buffer(edits), origin);
        self.done(action);
        self.recompute();
    }

    /// `action` is the newest one.
    fn done(&mut self, action: BufferAction) {
        self.undo.push(action);
        self.redo.clear();
        if let Some(app) = &mut self.app {
            app.redo_labels.clear();
        }
    }

    fn undo(&mut self) {
        let Some(action) = self.undo.pop() else {
            return;
        };
        if self.reviewing() {
            self.s.review.begin_history_step(false);
        }
        if let Some(app) = &mut self.app {
            app.redo_labels.push(app.labels.clone());
        }
        for (e, replaced) in action.iter().rev() {
            let start = e.range.start;
            let back = TextEdit {
                range: start..start + e.text.len(),
                text: replaced.clone(),
            };
            self.change(&back, Origin::History);
        }
        if let Some(app) = &mut self.app {
            app.labels = app.undo_labels.pop().expect("the labels before the action");
        }
        self.redo.push(action);
        if self.gui {
            self.recompute();
        }
    }

    fn redo(&mut self) {
        let Some(action) = self.redo.pop() else {
            return;
        };
        if self.reviewing() {
            self.s.review.begin_history_step(true);
        }
        if let Some(app) = &mut self.app {
            app.undo_labels.push(app.labels.clone());
        }
        for (e, _) in &action {
            self.change(e, Origin::History);
        }
        if let Some(app) = &mut self.app {
            app.labels = app.redo_labels.pop().expect("the labels after the action");
        }
        self.undo.push(action);
        if self.gui {
            self.recompute();
        }
    }

    /// `at` taken modulo the text, moved back to a char boundary.
    fn place(&self, at: usize) -> usize {
        self.floor(at % (self.s.text.len() + 1))
    }

    /// `at`, at most the end of the text, moved back to a char boundary.
    fn floor(&self, at: usize) -> usize {
        let text = &self.s.text;
        let mut i = at.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn play(&mut self, step: &Step) {
        if let Some(app) = &self.app {
            // A new action of the buffer's keeps the labels before it for its undo.
            let (labels, actions) = (app.labels.clone(), self.undo.len());
            let history = matches!(step, Step::Undo | Step::Redo);
            if !self.play_in_app(step) {
                self.play_in_buffer(step);
            }
            let grew = !history && self.undo.len() > actions;
            let app = self.app();
            app.label = 0;
            if grew {
                app.undo_labels.push(labels);
            }
            self.check_marks(step);
        } else {
            self.play_in_buffer(step);
        }
    }

    fn play_in_buffer(&mut self, step: &Step) {
        match step {
            Step::Agent { at, len, text } | Step::User { at, len, text } => {
                let a = self.place(*at);
                let b = self.floor(a + len);
                if a < b || !text.is_empty() {
                    let e = TextEdit {
                        range: a..b,
                        text: (*text).to_owned(),
                    };
                    let origin = match step {
                        Step::Agent { .. } => Origin::Agent { call: 1 },
                        _ => Origin::User,
                    };
                    self.act(&[e], origin);
                }
            }
            Step::AgentAll { from, to } => {
                let edits = str_replace(&self.s.text, from, to, true).unwrap_or_default();
                self.act_at_once(&edits, Origin::Agent { call: 2 });
            }
            Step::Turn(turn) => self.s.review.set_turn(*turn),
            Step::Typing { at, keys } => {
                let a = self.place(*at);
                let mut end = a;
                for key in keys {
                    self.edit(&TextEdit::insert(end, *key), Origin::User);
                    end += key.len();
                }
                self.done(vec![(TextEdit::insert(a, keys.concat()), String::new())]);
            }
            Step::Backspace { at, n } => {
                let end = self.place(*at);
                let before = self.s.text[..end].to_owned();
                let mut a = end;
                for _ in 0..*n {
                    let Some(c) = self.s.text[..a].chars().next_back() else {
                        break;
                    };
                    let e = TextEdit::delete(a - c.len_utf8()..a);
                    a = e.range.start;
                    self.edit(&e, Origin::User);
                }
                if a < end {
                    self.done(vec![(TextEdit::delete(a..end), before[a..].to_owned())]);
                }
            }
            Step::Delete { at, n } => {
                let a = self.place(*at);
                let mut removed = String::new();
                for _ in 0..*n {
                    let Some(c) = self.s.text[a..].chars().next() else {
                        break;
                    };
                    removed.push(c);
                    self.edit(&TextEdit::delete(a..a + c.len_utf8()), Origin::User);
                }
                if !removed.is_empty() {
                    let e = TextEdit::delete(a..a + removed.len());
                    self.done(vec![(e, removed)]);
                }
            }
            Step::Reject(pick) => {
                self.recompute();
                let ids = self.s.ids();
                if !ids.is_empty() {
                    let id = ids[pick % ids.len()];
                    let e = self.s.review.reject(id, &self.s.text).expect("reject");
                    self.act(&[e], Origin::Review);
                }
            }
            Step::RejectAll => {
                self.recompute();
                let edits = self.s.review.reject_all(&self.s.text);
                self.act(&edits, Origin::Review);
            }
            Step::RejectTurn(turn) => {
                self.recompute();
                let edits = self.s.review.reject_turn(*turn, &self.s.text);
                self.act(&edits, Origin::Review);
            }
            Step::AltU => {
                self.recompute();
                let action = self.s.review.undo_review_action(&self.s.text);
                if let Some(UndoAction::Edits(edits)) = action {
                    self.act(&edits, Origin::Review);
                }
                self.recompute();
            }
            Step::Undo => self.undo(),
            Step::Redo => self.redo(),
            Step::Recompute => self.recompute(),
            _ => panic!("{step:?} is a step of properties 12 and 13"),
        }
    }

    fn app(&mut self) -> &mut App {
        self.app.as_mut().expect("a buffer in the app")
    }

    /// The id of the hunk at `pick` (taken modulo the hunks).
    fn pick(&self, pick: usize) -> Option<HunkId> {
        let hunks = self.s.hunks();
        (self.reviewing() && !hunks.is_empty()).then(|| hunks[pick % hunks.len()].id)
    }

    /// The ids of the hunks whose index (modulo 8) has its bit set in `mask`.
    fn picked(&self, mask: u8) -> Vec<HunkId> {
        self.s
            .hunks()
            .iter()
            .enumerate()
            .filter(|(k, _)| mask & (1 << (k % 8)) != 0)
            .map(|(_, h)| h.id)
            .collect()
    }

    /// Plays `step` as the app does where that differs from the buffer alone (`play_in_buffer`
    /// plays the rest); false when it is not such a step.
    fn play_in_app(&mut self, step: &Step) -> bool {
        let in_buffer = matches!(
            step,
            Step::User { .. }
                | Step::Typing { .. }
                | Step::Backspace { .. }
                | Step::Delete { .. }
                | Step::Undo
                | Step::Redo
                | Step::Save
        );
        if self.closed() && in_buffer {
            // A closed document has no buffer to type in, undo or save.
            return true;
        }
        match step {
            Step::Agent { at, len, text } => {
                let a = self.place(*at);
                let b = self.floor(a + len);
                if a < b || !text.is_empty() {
                    self.agent(&[TextEdit {
                        range: a..b,
                        text: (*text).to_owned(),
                    }]);
                }
            }
            Step::AgentMarked {
                over,
                at,
                len,
                text,
            } => {
                let hunk = over
                    .filter(|_| self.reviewing() && !self.s.hunks().is_empty())
                    .map(|k| self.s.hunks()[k % self.s.hunks().len()].cur_bytes.clone());
                let a = match hunk {
                    Some(r) => self.floor(r.start + at % (r.len() + 1)),
                    None => self.place(*at),
                };
                let b = self.floor(a + len);
                let app = self.app();
                app.marked += 1;
                app.label = app.marked;
                let text = format!("{text}{}", marker(app.marked));
                self.agent(&[TextEdit { range: a..b, text }]);
            }
            Step::AgentAll { from, to } => {
                let edits = str_replace(&self.s.text, from, to, true).unwrap_or_default();
                self.agent(&edits);
            }
            Step::Turn(turn) => {
                self.app().turn = *turn;
                if self.reviewing() {
                    self.s.review.set_turn(*turn);
                }
            }
            Step::Accept { pick, fresh } => {
                // Alt+Y (`review_ui::decide`) on the hunks as they are, or a row of the Changes
                // panel (`decide_picked`, recomputed first), the only way in a closed document.
                let panel = self.closed();
                if *fresh || panel {
                    self.recompute();
                }
                if let Some(id) = self.pick(*pick) {
                    self.accept_with(Some(&[id]), |r, text| {
                        if panel {
                            r.accept_hunks(&[id], text) > 0
                        } else {
                            r.accept(id, text).is_ok()
                        }
                    });
                }
            }
            Step::AcceptAll => {
                // `decide_all_in`: on the hunks as they are.
                if self.reviewing() && !self.s.hunks().is_empty() {
                    self.accept_with(None, |r, text| {
                        r.accept_all(text);
                        true
                    });
                }
            }
            Step::KeepTurn(turn) => {
                self.recompute();
                if self.reviewing() {
                    let own: Vec<HunkId> = self
                        .s
                        .review
                        .hunks_of_turn(*turn)
                        .iter()
                        .map(|h| h.id)
                        .collect();
                    self.accept_with(Some(&own), |r, text| {
                        let before = r.hunks().len();
                        r.accept_turn(*turn, text);
                        r.hunks().len() < before
                    });
                }
            }
            Step::AcceptSome(mask) => {
                self.recompute();
                let ids = self.picked(*mask);
                if self.reviewing() && !ids.is_empty() {
                    self.accept_with(Some(&ids), |r, text| r.accept_hunks(&ids, text) > 0);
                }
            }
            Step::Reject(pick) => {
                // A row of the Changes panel (`decide_picked`).
                self.recompute();
                if let Some(id) = self.pick(*pick) {
                    let edits = self.s.review.reject_hunks(&[id], &self.s.text);
                    self.reject_with(edits);
                }
            }
            Step::RejectNow(pick) => {
                if self.closed() {
                    return self.play_in_app(&Step::Reject(*pick));
                }
                // Alt+N (`review_ui::decide`) on the hunks as they are: a decision when it
                // rejected something (not a hunk whose texts became equal).
                if let Some(id) = self.pick(*pick) {
                    match self.s.review.reject(id, &self.s.text) {
                        Ok(e) => self.reject_with(vec![e]),
                        Err(_) => self.recompute(),
                    }
                }
            }
            Step::RejectAll => {
                // `decide_all_in`: on the hunks as they are.
                if self.reviewing() && !self.s.hunks().is_empty() {
                    let edits = self.s.review.reject_all(&self.s.text);
                    self.reject_with(edits);
                }
            }
            Step::RejectTurn(turn) => {
                self.recompute();
                if self.reviewing() {
                    let edits = self.s.review.reject_turn(*turn, &self.s.text);
                    self.reject_with(edits);
                }
            }
            Step::RejectSome(mask) => {
                self.recompute();
                let ids = self.picked(*mask);
                if self.reviewing() && !ids.is_empty() {
                    let edits = self.s.review.reject_hunks(&ids, &self.s.text);
                    self.reject_with(edits);
                }
            }
            Step::AltU => self.alt_u(),
            Step::Save => {
                self.app().disk = self.s.text.clone();
            }
            Step::Close { discard } => self.close(*discard),
            Step::Open => self.open(),
            Step::Reload {
                at,
                len,
                text,
                keep_mine,
            } => self.reload(*at, *len, text, *keep_mine),
            _ => return false,
        }
        true
    }

    /// The agent's tool call (`agent_edits::apply`): its edits (descending) applied at once as
    /// one action, in a review started first if there is none, recomputed, and saved.
    fn agent(&mut self, edits: &[TextEdit]) {
        // The agent's edits come from `str_replace` or `minimal_edits`, which leave out what the
        // texts have in common at either end.
        let edits: Vec<TextEdit> = edits
            .iter()
            .filter_map(|e| trimmed(&self.s.text, e))
            .collect();
        if edits.is_empty() {
            return;
        }
        let app = self.app();
        if app.label == 0 {
            // An edit without a marker has a label all the same, to tell the agent's edits apart.
            app.unmarked += 1;
            app.label = 0x100 + app.unmarked;
        }
        self.start_review();
        self.apply(&edits, Origin::Agent { call: 1 });
        self.recompute();
        self.app().disk = self.s.text.clone();
    }

    /// Applies descending `edits` as `Document::apply_edits` does: one action of the buffer's
    /// (reported at once as an open buffer reports them), or in a closed document whole, out of
    /// any history.
    fn apply(&mut self, edits: &[TextEdit], origin: Origin) {
        let edits: Vec<TextEdit> = edits
            .iter()
            .filter(|e| !(e.range.is_empty() && e.text.is_empty()))
            .cloned()
            .collect();
        if edits.is_empty() {
            return;
        }
        if self.closed() {
            self.edits(&edits, origin);
            return;
        }
        let action: BufferAction = edits
            .iter()
            .map(|e| (e.clone(), self.s.text[e.range.clone()].to_owned()))
            .collect();
        self.edits(&as_open_buffer(&edits), origin);
        self.done(action);
    }

    /// `review_ui::start_review`: a review whose baseline is the text, told the window's turn and
    /// whether the text is in a buffer. Whatever markers the text holds are its baseline's.
    fn start_review(&mut self) {
        let text = self.s.text.clone();
        let app = self.app();
        if app.reviewing {
            return;
        }
        app.reviewing = true;
        app.history_baseline = None;
        app.journal.clear();
        for n in text.chars().filter_map(marked) {
            *app.owned.entry(n).or_default() += 1;
        }
        let (turn, closed) = (app.turn, app.closed);
        let mut review = FileReview::new(text);
        review.check_every_change();
        review.set_turn(turn);
        review.set_in_buffer(!closed);
        self.s.review = review;
    }

    /// `review_ui::drop_review`.
    fn drop_review(&mut self) {
        let app = self.app();
        app.reviewing = false;
        app.history_baseline = None;
    }

    /// A decision that accepts: `decide` decides on the review and says whether the window
    /// records it. The baseline must take the current text of the hunks that went (`only` among
    /// them, if given), and change nothing else, and the hunks left must be as they were. Their
    /// markers are the baseline's from now on.
    fn accept_with(
        &mut self,
        only: Option<&[HunkId]>,
        decide: impl FnOnce(&mut FileReview, &str) -> bool,
    ) {
        let before: Vec<Hunk> = self.s.hunks().to_vec();
        let mut expected = self.s.review.baseline().to_owned();
        let decided = decide(&mut self.s.review, &self.s.text);
        self.check("an accept");
        let after = self.s.hunks();
        let mut taken: Vec<std::ops::Range<usize>> = Vec::new();
        for h in before.iter().rev() {
            match after.iter().find(|x| x.id == h.id) {
                Some(x) => assert!(
                    x.cur_bytes == h.cur_bytes && x.old_text == h.old_text,
                    "an accept changed the hunk {h:?} into {x:?}"
                ),
                None => {
                    assert!(
                        only.is_none_or(|ids| ids.contains(&h.id)),
                        "an accept took {h:?}, not one of {only:?}"
                    );
                    let text = &self.s.text[h.cur_bytes.clone()];
                    expected.replace_range(h.base_bytes.clone(), text);
                    taken.push(h.cur_bytes.clone());
                }
            }
        }
        assert!(
            after.iter().all(|x| before.iter().any(|h| h.id == x.id)),
            "an accept made hunks"
        );
        assert_eq!(
            self.s.review.baseline(),
            expected,
            "an accept changed the baseline elsewhere than the hunks it took"
        );
        // The marked edits whose text it took (some of it, or their markers), by hunk.
        let app = self.app.as_ref().expect("a buffer in the app");
        let regions: Vec<(String, Vec<u32>)> = taken
            .iter()
            .map(|r| {
                let mut edits: Vec<u32> = app.labels[r.clone()]
                    .iter()
                    .copied()
                    .filter(|&n| n != 0)
                    .chain(self.s.text[r.clone()].chars().filter_map(marked))
                    .collect();
                edits.sort_unstable();
                edits.dedup();
                (self.s.text[r.clone()].to_owned(), edits)
            })
            .collect();
        let mut edits: Vec<u32> = regions.iter().flat_map(|(_, e)| e.clone()).collect();
        edits.sort_unstable();
        edits.dedup();
        let app = self.app();
        for &n in &edits {
            *app.accepted.entry(n).or_default() += 1;
        }
        if !taken.is_empty() {
            app.journal.push(Decided::Accept(regions));
            app.history_baseline = None;
        }
        if decided {
            app.decisions += 1;
        }
        self.recompute();
    }

    /// A decision that rejects, whose edits are `edits`: applied as one action (`Origin::Review`),
    /// recomputed, saved, and recorded by the window when there were edits.
    fn reject_with(&mut self, edits: Vec<TextEdit>) {
        let any = edits
            .iter()
            .any(|e| !(e.range.is_empty() && e.text.is_empty()));
        let app = self.app.as_ref().expect("a buffer in the app");
        let taken: Vec<(String, Vec<u32>)> = edits
            .iter()
            .map(|e| {
                let r = e.range.clone();
                (self.s.text[r.clone()].to_owned(), app.labels[r].to_vec())
            })
            .collect();
        self.apply(&edits, Origin::Review);
        self.recompute();
        let text = self.s.text.clone();
        let app = self.app();
        if any {
            app.disk = text;
            app.journal.push(Decided::Reject(taken));
        }
        if any {
            app.decisions += 1;
        }
    }

    /// Alt+U (`review_ui::undo_last`): the window's newest decision, reverted in the document
    /// (`undo_in`) only if its newest journal entry can be.
    fn alt_u(&mut self) {
        let app = self.app();
        if app.decisions == 0 {
            return;
        }
        app.decisions -= 1;
        if !app.reviewing {
            return;
        }
        // The document's newest journal entry goes, reverted or not.
        let entry = self.app().journal.pop();
        let had: Vec<HunkId> = self.s.ids();
        match self.s.review.undo_last_review_action(&self.s.text) {
            Some(UndoAction::Unaccepted) => {
                self.check("Alt+U of an accept");
                let Some(Decided::Accept(regions)) = entry else {
                    panic!("Alt+U reverted an accept, and the journal's newest was {entry:?}");
                };
                // The regions that came back are the hunks it brought back, whose text is the
                // region's (a region edited since stays accepted). An edit is no longer
                // accepted when every region holding it came back.
                let mut back: Vec<String> = self
                    .s
                    .hunks()
                    .iter()
                    .filter(|h| !had.contains(&h.id))
                    .map(|h| self.s.text[h.cur_bytes.clone()].to_owned())
                    .collect();
                let came: Vec<bool> = regions
                    .iter()
                    .map(|(text, _)| match back.iter().position(|t| t == text) {
                        Some(k) => {
                            back.swap_remove(k);
                            true
                        }
                        None => false,
                    })
                    .collect();
                let mut edits: Vec<u32> = regions.iter().flat_map(|(_, e)| e.clone()).collect();
                edits.sort_unstable();
                edits.dedup();
                let app = self.app();
                for n in edits {
                    let all_back = regions
                        .iter()
                        .zip(&came)
                        .filter(|((_, e), _)| e.contains(&n))
                        .all(|(_, &c)| c);
                    if all_back && let Some(k) = app.accepted.get_mut(&n) {
                        *k = k.saturating_sub(1);
                    }
                }
                app.history_baseline = None;
                self.recompute();
            }
            Some(UndoAction::Edits(edits)) => {
                let Some(Decided::Reject(mut taken)) = entry else {
                    panic!("Alt+U reverted a reject, and the journal's newest was {entry:?}");
                };
                self.apply(&edits, Origin::Review);
                // The texts it puts back are those the reject took out, with their labels.
                let app = self.app();
                let mut shift: isize = 0;
                for e in edits.iter().rev() {
                    let start = e.range.start.strict_add_signed(shift);
                    if let Some(k) = taken.iter().position(|(t, _)| *t == e.text) {
                        let (_, labels) = taken.swap_remove(k);
                        app.labels.splice(start..start + e.text.len(), labels);
                    }
                    shift += e.text.len() as isize - e.range.len() as isize;
                }
                self.recompute();
                self.app().disk = self.s.text.clone();
            }
            None => self.recompute(),
        }
    }

    /// The tab closes (`review_ui::detach_editor`, `Workspace::release`,
    /// `Document::close_buffer`): a review with nothing left to decide is dropped; unsaved
    /// changes are saved, or discarded: the closed document then goes back to the file's text
    /// (`Document::reload`, as the user's own edits). The buffer and its history are gone.
    fn close(&mut self, discard: bool) {
        if self.closed() {
            return;
        }
        if self.reviewing() && self.s.review.is_clean() {
            self.drop_review();
        }
        let text = self.s.text.clone();
        let reviewing = self.reviewing();
        let app = self.app();
        if !discard {
            app.disk = text.clone();
        }
        app.closed = true;
        app.undo_labels.clear();
        app.redo_labels.clear();
        let disk = app.disk.clone();
        self.undo.clear();
        self.redo.clear();
        if !reviewing {
            // The document leaves the workspace: it is read again when it is needed. Whatever
            // the file holds is the user's then (unsaved changes discarded, as the user's own
            // edit: see the report on the agent's text an undo had taken out).
            let app = self.app();
            if text != disk {
                for n in disk.chars().filter_map(marked) {
                    *app.owned.entry(n).or_default() += 1;
                }
            }
            app.labels = vec![0; disk.len()];
            self.s.text = disk;
            return;
        }
        self.s.review.set_in_buffer(false);
        if text != disk {
            let edits = minimal_edits(&text, &disk);
            self.user_edits(&edits);
        }
    }

    /// The user's own edits from elsewhere than the keyboard (`Document::reload`): applied as
    /// one action; their markers are the user's.
    fn user_edits(&mut self, edits: &[TextEdit]) {
        let app = self.app();
        for n in edits.iter().flat_map(|e| e.text.chars().filter_map(marked)) {
            *app.owned.entry(n).or_default() += 1;
        }
        self.apply(edits, Origin::User);
    }

    /// The document opens in a new buffer (`Document::open_in_buffer`): its history starts
    /// there.
    fn open(&mut self) {
        if !self.closed() {
            return;
        }
        let reviewing = self.reviewing();
        if reviewing {
            self.s.review.set_in_buffer(true);
        }
        let (text, baseline) = (self.s.text.clone(), self.s.review.baseline().to_owned());
        let app = self.app();
        app.closed = false;
        app.history_text = text;
        app.history_baseline = reviewing.then_some(baseline);
    }

    /// Another program writes the file outside a turn (`watcher`): its text with `[at, at +
    /// len)` replaced by `text` and a marker. A document without unsaved changes reloads (one
    /// action, `Origin::Reload`; with a review that has no hunks, or none, the user's own change,
    /// and a closed document then leaves the workspace with its review); one with them shows the
    /// conflict bar: Reload (`Document::reload`, the user's own edits) or Keep mine (the next
    /// save overwrites it).
    fn reload(&mut self, at: usize, len: usize, text: &str, keep_mine: bool) {
        let app = self.app();
        app.marked += 1;
        let mark = marker(app.marked);
        let mut new = app.disk.clone();
        let a = floor(&new, at % (new.len() + 1));
        let b = floor(&new, a + len);
        new.replace_range(a..b, &format!("{text}{mark}"));
        let disk = app.disk.clone();
        let dirty = self.s.text != disk;
        if dirty {
            if keep_mine {
                self.app().disk = new;
            } else {
                let edits = minimal_edits(&self.s.text, &new);
                self.user_edits(&edits);
                self.app().disk = new;
            }
            return;
        }
        let decided = self.reviewing() && self.s.hunks().is_empty();
        if self.closed() && (decided || !self.reviewing()) {
            // The document leaves the workspace (its review with it): it is read again when it
            // is needed.
            if decided {
                self.drop_review();
            }
            // Not under review: the other program's text is the user's.
            let app = self.app();
            *app.owned.entry(app.marked).or_default() += 1;
            app.labels = vec![0; new.len()];
            app.disk = new.clone();
            self.s.text = new;
            return;
        }
        let edits = minimal_edits(&self.s.text, &new);
        if decided || !self.reviewing() {
            // Nothing to review: the user's own change (an open document keeps its review).
            self.user_edits(&edits);
        } else {
            self.app().label = mark as u32 - 0x2460;
            self.apply(&edits, Origin::Reload);
        }
        self.app().disk = new;
        if self.gui || self.closed() {
            self.recompute();
        }
    }

    /// Panics when the baseline holds a marker of text nobody accepted (see `App::accepted`).
    fn check_marks(&self, after: &Step) {
        let Some(app) = &self.app else {
            return;
        };
        assert_eq!(
            app.labels.len(),
            self.s.text.len(),
            "the labels after {after:?}"
        );
        if !app.reviewing {
            return;
        }
        let baseline = self.s.review.baseline();
        for n in baseline.chars().filter_map(marked) {
            let held = |m: &HashMap<u32, u32>| m.get(&n).is_some_and(|&k| k > 0);
            assert!(
                held(&app.accepted) || held(&app.owned),
                "the baseline {baseline:?} holds {:?}, of text not accepted, after {after:?}",
                marker(n),
            );
        }
    }
}

/// `e` without what its text and the text it replaces have in common at either end, as
/// `str_replace` and `minimal_edits` make the agent's edits; `None` when nothing is left.
fn trimmed(text: &str, e: &TextEdit) -> Option<TextEdit> {
    let (old, new) = (&text[e.range.clone()], e.text.as_str());
    let p: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    let q: usize = old[p..]
        .chars()
        .rev()
        .zip(new[p..].chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    let range = e.range.start + p..e.range.end - q;
    let text = &new[p..new.len() - q];
    (!range.is_empty() || !text.is_empty()).then(|| TextEdit {
        range,
        text: text.to_owned(),
    })
}

/// `at`, at most the end of `text`, moved back to a char boundary.
fn floor(text: &str, at: usize) -> usize {
    let mut i = at.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

proptest! {
    #![proptest_config(config())]

    /// 10. However the agent's edits (one at a time or a replace_all, in several turns),
    /// rejects (of a hunk, of all, of a turn), Alt+U and the buffer's undo and redo (told which
    /// way they step) interleave, nothing goes into the baseline, undoing every action leaves
    /// nothing to review, and redoing them all gives back the text the session ended with,
    /// whether the review is recomputed after each undo and redo (as the GUI does before the
    /// next frame) or not (several in a row, as Vim's `3u`). The review checks its invariants
    /// inside every change and after it. Reject, Ctrl+Z, Ctrl+Shift+Z, another change, then
    /// Ctrl+Z back past the reject used to put the agent's text into the baseline; so did a
    /// reject that an older reject's record took for its Alt+U, and Alt+U after a newer agent
    /// edit before the reject's text, which brought the records the reject had cut back where
    /// its text used to be (see the tests below).
    #[test]
    fn the_buffers_undo_and_redo_never_reach_the_baseline(
        base in prop::sample::select(BASES),
        steps in prop::collection::vec(step(false), 1..STEPS),
        gui in any::<bool>(),
    ) {
        let baseline = undo_and_redo_all(base, &steps, gui)?;
        prop_assert_eq!(baseline.as_str(), base);
    }

    /// 11. With the user's own edits among those steps (typing over a selection, typing key by
    /// key, Backspace, Delete), undoing every action gives back the text and the baseline the
    /// session started with, with nothing to review, and redoing them all gives back the text
    /// and the baseline it ended with: each of the user's edits is taken back (and made again)
    /// where it went, into the baseline or into a hunk's proposal, wherever the hunks lie by
    /// then. Typing over a selection that ran past the end of a hunk's text left the typed text
    /// in the baseline after Ctrl+Z (the next recompute had widened the hunk over it), or put
    /// the agent's text into the baseline; so did Ctrl+Z and Ctrl+Shift+Z of Reject Turn and of
    /// the agent's replace_all next to the user's deletions (see the tests below).
    #[test]
    fn the_buffers_undo_and_redo_of_the_users_edits_go_back_where_they_went(
        base in prop::sample::select(BASES),
        steps in prop::collection::vec(step(true), 1..STEPS),
        gui in any::<bool>(),
    ) {
        undo_and_redo_all(base, &steps, gui)?;
    }
}

/// Plays `steps` from `base`, then undoes every action and redoes them all: the text and the
/// baseline are those the session started with once every action is undone, with nothing to
/// review, and those it ended with once they are all redone. Returns the baseline it ended with.
fn undo_and_redo_all(base: &str, steps: &[Step], gui: bool) -> Result<String, TestCaseError> {
    let mut b = Buffer::new(base, gui);
    for step in steps {
        b.play(step);
    }
    b.recompute();
    b.s.check();
    let (text, baseline) = (b.s.text.clone(), b.s.review.baseline().to_owned());
    let done = b.undo.len();
    while !b.undo.is_empty() {
        b.undo();
    }
    b.recompute();
    prop_assert_eq!(b.s.text.as_str(), base);
    prop_assert_eq!(b.s.review.baseline(), base);
    prop_assert!(b.s.review.is_clean(), "{:?}", b.s.hunks());
    for _ in 0..done {
        b.redo();
    }
    b.recompute();
    prop_assert_eq!(&b.s.text, &text);
    prop_assert_eq!(b.s.review.baseline(), baseline.as_str());
    Ok(baseline)
}

/// Sessions the properties found once they had Reject Turn, the agent's replace_all and the
/// Delete key (and the review of the second pass found), played through, undone and redone, with
/// and without the GUI's recomputes.
#[test]
fn the_buffers_undo_and_redo_after_these_sessions_go_back_where_they_went() {
    use Step::{AgentAll, AltU, Backspace, Delete, Redo, Reject, RejectTurn, Typing, Undo};
    let agent = |at, len, text| Step::Agent { at, len, text };
    let user = |at, len, text| Step::User { at, len, text };
    let cases: Vec<(&str, Vec<Step>)> = vec![
        // The undo of Reject Turn, whose rejects of two of the turn's edits side by side gave
        // each other's text back: the user's own line, deleted before, then went into the
        // baseline as a pending change.
        (
            "a\nb\nc\n",
            vec![
                Delete { at: 1, n: 2 },
                AgentAll {
                    from: "\n",
                    to: "yz",
                },
                Backspace { at: 4, n: 2 },
                RejectTurn(0),
            ],
        ),
        // The redo of the agent's replace_all after the redo of the user's deletion of its text
        // and before the redo of Reject Turn: part of it went into the baseline. Reject Turn's
        // first edit puts back the agent's third line break, which the user had deleted right
        // after the turn's second change: the second change's reject took that insertion.
        (
            "a\nb\nc\n",
            vec![
                AgentAll {
                    from: "\n",
                    to: "yz",
                },
                Delete { at: 6, n: 3 },
                RejectTurn(0),
            ],
        ),
        // Alt+U of Reject Turn after the user's deletion in the agent's text, then Ctrl+Z of
        // Alt+U and of Reject Turn: the undo of Alt+U left the turn's two rejects empty at one
        // place, the one undone first took the first insertion there, and the second insertion,
        // the other one's, went into the baseline.
        (
            "",
            vec![
                agent(0, 0, "yz"),
                user(0, 0, "\n\n"),
                Reject(0),
                agent(0, 0, "a\nb"),
                Delete { at: 2, n: 2 },
                AltU,
                RejectTurn(0),
                AltU,
            ],
        ),
        // The same with typing and more rejects (the review of the second pass found it).
        (
            "é\n∑\r\nz",
            vec![
                agent(8299206728638650713, 0, "x"),
                RejectTurn(1),
                Redo,
                agent(8129023500143092722, 0, "é"),
                user(8625729804327, 0, "x"),
                Undo,
                agent(10205698512426234919, 2, "x"),
                Reject(0),
                agent(8162955915575332129, 0, "\n"),
                AltU,
                user(7741663396868186923, 0, "x"),
                RejectTurn(0),
                AltU,
                Typing {
                    at: 0,
                    keys: vec!["x", "x"],
                },
            ],
        ),
        // An agent deletion right where a rejected edit's text had been, then Alt+U of the
        // reject: the deletion's record comes after the text Alt+U puts back there.
        (
            "",
            vec![
                agent(0, 0, "x"),
                Reject(0),
                agent(0, 0, "a\nb"),
                agent(0, 1, ""),
                AltU,
            ],
        ),
        // The agent's edits only: a reject, Alt+U and Reject Turn at one place among empty
        // records, then Ctrl+Z of everything (property 10 found it before Reject Turn's undo
        // kept their order).
        (
            "",
            vec![
                agent(0, 0, "\n"),
                agent(3231276185022773652, 1, "\n"),
                Undo,
                Redo,
                agent(0, 0, "yz"),
                agent(858747155591717503, 0, "x"),
                Reject(0),
                agent(0, 0, "yz"),
                agent(0, 0, "yz"),
                agent(322817645861121405, 1, ""),
                Undo,
                AltU,
                RejectTurn(0),
                AltU,
            ],
        ),
        // Alt+U of Reject Turn, whose two rejects put the same text back at one place: the
        // newer one's record took the older one's edit, the older one's record was left as it
        // was, and the redo of the user's later typing over the agent's text went into the
        // baseline.
        (
            "a\n",
            vec![
                user(586155715438537823, 0, "\n\n"),
                AgentAll {
                    from: "\n",
                    to: "∑\n",
                },
                Backspace {
                    at: 11632738269508587219,
                    n: 3,
                },
                RejectTurn(0),
                AltU,
                agent(6096525648285586468, 0, "\n"),
                user(10073576708683627345, 0, "é"),
                user(12072391138972663909, 0, "\n"),
                user(1315393849147972147, 3, "x"),
            ],
        ),
        // Alt+U of Reject Turn, whose two rejects' records had come to one place, one pinned
        // after the other through the agent's later edit there: Alt+U applied the first one's
        // text first, which moved the second record past it, and the second insertion, before
        // that text, was nobody's; the undo of Alt+U's action then put it into the baseline.
        (
            "",
            vec![
                agent(0, 0, "yz"),
                Typing {
                    at: 256281837559119900,
                    keys: vec!["\n", "\n\n"],
                },
                Delete {
                    at: 16885975705746792649,
                    n: 3,
                },
                Typing {
                    at: 9545188879090933604,
                    keys: vec!["\n\n", "\r\n"],
                },
                agent(6703970844722850452, 1, "\n\n"),
                RejectTurn(0),
                Reject(0),
                agent(14689107471668542686, 3, "yz"),
                Undo,
                Redo,
                AltU,
            ],
        ),
        // Nine records empty at one place (agent edits and rejects taking each other back):
        // their order there decides which of them takes each undo and redo.
        (
            "a\nb\nc\n",
            vec![
                agent(142185073052548961, 2, ""),
                agent(16922368329099188472, 2, "\n"),
                Reject(0),
                AgentAll { from: "\n", to: "" },
                agent(6755038048274679552, 3, "x"),
                agent(3245506348648216302, 1, "\n"),
                Reject(0),
                agent(0, 0, "a\nb"),
                agent(8321432160126882578, 1, "x"),
            ],
        ),
    ];
    // Without the user's edits, the baseline the session ends with is the one it started with
    // (property 10).
    let users = |steps: &[Step]| {
        steps.iter().any(|s| {
            matches!(
                s,
                Step::User { .. } | Typing { .. } | Backspace { .. } | Delete { .. }
            )
        })
    };
    let failed: Vec<String> = cases
        .iter()
        .flat_map(|(base, steps)| [false, true].map(|gui| (base, steps, gui)))
        .filter_map(|(base, steps, gui)| {
            let e = match undo_and_redo_all(base, steps, gui) {
                Ok(baseline) if baseline != *base && !users(steps) => {
                    format!("the session ended with the baseline {baseline:?}")
                }
                Ok(_) => return None,
                Err(e) => e.to_string(),
            };
            Some(format!("{base:?} {steps:?} {gui}: {e}"))
        })
        .collect();
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Most steps of a session in properties 10 and 11.
const STEPS: usize = 40;

/// Sessions property 10 found, each its own way of losing the record of a reject (so that
/// undoing back past the reject brought the agent's text back as the user's own), played through
/// and undone, with and without the GUI's recomputes.
#[test]
fn the_buffers_undo_after_these_rejects_keeps_the_baseline() {
    use Step::{Agent, AltU, Redo, Reject, Undo};
    let agent = |at, len, text| Agent { at, len, text };
    let cases: Vec<(&str, Vec<Step>)> = vec![
        // A reject's own deletion was taken by an older reject's record for its Alt+U (it
        // deletes the same text there): the newer reject's record was never applied.
        (
            "a\nb\nc\n",
            vec![
                agent(5, 1, "x"),
                Reject(0),
                agent(5, 0, "\n"),
                agent(0, 0, "x"),
                Undo,
                Reject(0),
                Undo,
                agent(6, 1, "yz"),
            ],
        ),
        // Alt+U after an agent edit before the reject's text: the records the reject had cut
        // came back where its text used to be, did not match it, and were dropped.
        (
            "x\ny",
            vec![
                agent(1, 2, "\n"),
                agent(0, 0, "x"),
                Reject(0),
                agent(0, 0, "x"),
                AltU,
            ],
        ),
        // The same where the records the reject cut reach before its text, into text an agent
        // edit then deleted: they cannot come back with Alt+U, and are kept for the reject's own
        // undo.
        (
            "é\n∑\r\nz",
            vec![
                agent(12405973194016086522, 1, "é"),
                agent(7007254250960065586, 7, ""),
                agent(0, 3, "\n"),
                Reject(0),
                agent(12780292439273302929, 0, "x"),
                agent(577883053651147411, 4, ""),
                Reject(0),
                agent(2072456345156857470, 2, ""),
                AltU,
            ],
        ),
        // The undo of Alt+U's action was taken by an agent edit's record (it inserts the text the
        // agent had deleted there), which is older than Alt+U's action.
        (
            "a\n",
            vec![
                agent(1, 1, ""),
                Reject(0),
                agent(2, 0, "\n"),
                agent(0, 0, "x"),
                Undo,
                Redo,
                Undo,
                agent(2, 1, ""),
                AltU,
                Undo,
                Undo,
            ],
        ),
    ];
    for (base, steps) in &cases {
        for gui in [false, true] {
            let mut b = Buffer::new(base, gui);
            for step in steps {
                b.play(step);
            }
            b.recompute();
            assert_eq!(b.s.review.baseline(), *base, "{steps:?} {gui}");
            while !b.undo.is_empty() {
                b.undo();
            }
            b.recompute();
            assert_eq!(b.s.text, *base, "{steps:?} {gui}");
            assert_eq!(b.s.review.baseline(), *base, "{steps:?} {gui}");
            assert!(b.s.review.is_clean(), "{steps:?} {gui}");
        }
    }
}

/// The steps of property 12 (those of property 11, accepts and the agent's edits marked, some
/// over its hunks), and with `buffers` those of property 13 too.
fn app_step(buffers: bool) -> BoxedStrategy<Step> {
    let edit = || (any::<usize>(), 0usize..8, prop::sample::select(TEXTS));
    let marked =
        (proptest::option::of(any::<usize>()), edit()).prop_map(|(over, (at, len, text))| {
            Step::AgentMarked {
                over,
                at,
                len,
                text,
            }
        });
    let mut steps: Vec<(u32, BoxedStrategy<Step>)> = vec![
        (12, step(true)),
        (4, marked.boxed()),
        (
            3,
            (any::<usize>(), any::<bool>())
                .prop_map(|(pick, fresh)| Step::Accept { pick, fresh })
                .boxed(),
        ),
        (1, any::<usize>().prop_map(Step::RejectNow).boxed()),
        (1, Just(Step::AcceptAll).boxed()),
        (1, (0u32..3).prop_map(Step::KeepTurn).boxed()),
        (1, any::<u8>().prop_map(Step::AcceptSome).boxed()),
        (1, any::<u8>().prop_map(Step::RejectSome).boxed()),
        (2, Just(Step::AltU).boxed()),
    ];
    if buffers {
        let reload =
            (edit(), any::<bool>()).prop_map(|((at, len, text), keep_mine)| Step::Reload {
                at,
                len,
                text,
                keep_mine,
            });
        steps.extend([
            (1, Just(Step::Save).boxed()),
            (
                2,
                any::<bool>()
                    .prop_map(|discard| Step::Close { discard })
                    .boxed(),
            ),
            (2, Just(Step::Open).boxed()),
            (2, reload.boxed()),
        ]);
    }
    prop::strategy::Union::new_weighted(steps).boxed()
}

proptest! {
    #![proptest_config(config())]

    /// 12. With accepts among the steps of property 11 (of a hunk, on the hunks as they are or
    /// recomputed; of several; Keep Turn; Accept all) and Alt+U as the app does it (the
    /// window's newest decision, reverted only if it still can be), and the agent editing over
    /// its earlier hunks: an accept changes the baseline by exactly the current text of the
    /// hunks it takes, and nothing else; after every step, and every undo and redo, the
    /// baseline holds the agent's text only when an accept took some of that edit's text (the
    /// user's undo in accepted text is the user's own change, and the rest of the edit's text
    /// comes back as a change to review), the agent's edits ending with markers no other text
    /// has; undoing every action gives back the text the session started with, and without
    /// accepts its baseline too, with nothing to review; redoing them all gives back the text
    /// and the baseline it ended with.
    #[test]
    fn accepts_change_the_baseline_by_what_they_accept_only(
        base in prop::sample::select(BASES),
        steps in prop::collection::vec(app_step(false), 1..STEPS),
        gui in any::<bool>(),
    ) {
        decide_undo_and_redo_all(base, &steps, gui)?;
    }

    /// 13. And with the buffer going away and coming back while the review stays: the tab
    /// closed (its unsaved changes saved or discarded, a review with nothing left to decide
    /// dropped) and opened again in a new buffer without history, decisions and agent edits in
    /// the closed document, Ctrl+S, and another program's change reloaded (one action of the
    /// buffer's, `Origin::Reload`) or, over unsaved changes, reloaded from the conflict bar as
    /// the user's own edit or kept out (or, with nothing under review, as the user's own change).
    /// As in property 12, for the agent's and the other program's text alike, and for the
    /// buffer's history since the document was last opened.
    #[test]
    fn the_review_outlives_its_buffer_and_takes_reloads(
        base in prop::sample::select(BASES),
        steps in prop::collection::vec(app_step(true), 1..STEPS),
        gui in any::<bool>(),
    ) {
        decide_undo_and_redo_all(base, &steps, gui)?;
    }
}

/// Sessions properties 12 and 13 found, played through, undone and redone (with and without the
/// GUI's recomputes): each its own way of putting the agent's text into the baseline after an
/// accept, or a reload, or of leaving it there after Ctrl+Z and Ctrl+Shift+Z, or of leaving the
/// baseline other than it was after Ctrl+Z, Ctrl+Shift+Z of every action.
#[test]
fn the_buffers_undo_and_redo_after_these_accepts_and_reloads_go_back_where_they_went() {
    use Step::{
        Accept, AcceptAll, AcceptSome, AgentAll, AltU, Backspace, Close, Delete, Open, Redo,
        Reject, RejectNow, RejectSome, RejectTurn, Reload, Turn, Typing, Undo,
    };
    let agent = |at, len, text| Step::Agent { at, len, text };
    let marked = |at, len, text| Step::AgentMarked {
        over: None,
        at,
        len,
        text,
    };
    let user = |at, len, text| Step::User { at, len, text };
    let alt_y = Accept {
        pick: 0,
        fresh: false,
    };
    let reload = |at, len, text| Reload {
        at,
        len,
        text,
        keep_mine: false,
    };
    let cases: Vec<(&str, Vec<Step>)> = vec![
        // The undo of the agent's second edit left a hunk whose texts were equal over the
        // first, accepted one: the undo of the first went into the proposal only (and was taken
        // for the redo of the second); its redo then put the second into the baseline.
        ("", vec![marked(0, 0, ""), alt_y.clone(), marked(0, 3, "")]),
        (
            "",
            vec![
                marked(0, 0, ""),
                Redo,
                RejectTurn(1),
                alt_y.clone(),
                marked(0, 3, ""),
            ],
        ),
        // The accept of the agent's next edit at the place of a reject forgot the reject's
        // record: its undo put the rejected text into the baseline.
        (
            "",
            vec![marked(0, 0, ""), Reject(0), agent(0, 0, "x"), alt_y.clone()],
        ),
        // And with the reject undone by Alt+U and rejected again: the accepted edit's record,
        // forgotten, took the first reject's record with it.
        (
            "",
            vec![
                marked(0, 0, ""),
                Reject(0),
                agent(0, 0, "x"),
                Reject(0),
                AltU,
                alt_y.clone(),
            ],
        ),
        // An agent edit the reject had taken out counted as accepted.
        (
            "",
            vec![agent(0, 0, "x"), Reject(0), marked(0, 0, ""), alt_y.clone()],
        ),
        // The agent's deletion of its own text counted as accepted with the hunk next to it.
        (
            "",
            vec![
                marked(0, 0, ""),
                agent(1525082044054406076, 3, "x"),
                Reject(0),
                AltU,
                alt_y.clone(),
            ],
        ),
        // Alt+U of the accept left its records accepted.
        (
            "a a\nb a\na\n",
            vec![
                marked(1547885728478700456, 0, "yz"),
                alt_y.clone(),
                AltU,
                Step::AgentMarked {
                    over: Some(0),
                    at: 6773594543879885028,
                    len: 5,
                    text: "\n",
                },
                alt_y.clone(),
            ],
        ),
        // The undo of the user's typing, then of a reject at that place: the second undo was
        // taken for the first one's insertion taken back, and records could not claim it.
        (
            "",
            vec![
                marked(0, 0, ""),
                Reject(0),
                marked(0, 0, ""),
                Undo,
                Typing {
                    at: 0,
                    keys: vec!["x", "x"],
                },
                alt_y.clone(),
            ],
        ),
        // The accept forgot the user's edits: the undo of Backspace held over the rejected
        // place was not taken apart, and the reject's undo was not recognized.
        (
            "",
            vec![
                user(0, 0, "x"),
                marked(726446474518206253, 0, ""),
                RejectNow(0),
                user(0, 0, "x"),
                Backspace {
                    at: 8717033138388630605,
                    n: 2,
                },
                agent(0, 0, "x"),
                alt_y.clone(),
            ],
        ),
        // Alt+U of a reject after an accept and its undo and redo: the undo of Alt+U's action
        // was paired with the redo of the reject.
        (
            "",
            vec![
                agent(0, 0, "x"),
                RejectNow(0),
                Undo,
                AcceptAll,
                Redo,
                AltU,
                AltU,
            ],
        ),
        // The user's deletion of the agent's text whose record a later edit had cut.
        (
            "",
            vec![
                marked(0, 0, ""),
                marked(1934493952459504268, 3, ""),
                Undo,
                Delete {
                    at: 30408401526643248,
                    n: 2,
                },
                alt_y.clone(),
            ],
        ),
        (
            "one\ntwo\nthree\nfour\n",
            vec![
                agent(18094300307608980344, 1, "x"),
                agent(2262783969976648764, 0, "x"),
                RejectNow(0),
                marked(11129965987557423700, 0, ""),
                AltU,
                Delete {
                    at: 13509529382929420926,
                    n: 2,
                },
                AcceptSome(137),
                Redo,
            ],
        ),
        // Property 13. The buffer's undo of a reload went into the baseline.
        (
            "é\n∑\r\nz",
            vec![
                agent(7914031583994820647, 2, "\n"),
                Close { discard: false },
                marked(2860605760818804965, 0, "\r\n"),
                alt_y.clone(),
                Open,
                agent(2653242632133225316, 5, "∑\n"),
                reload(17045238902194335540, 1, "\r\n"),
            ],
        ),
        // A reload forgot the user's edits.
        (
            "",
            vec![
                marked(0, 0, ""),
                Backspace {
                    at: 899944974502563231,
                    n: 2,
                },
                agent(0, 0, "x"),
                reload(0, 0, ""),
            ],
        ),
        // A reload dropped the review with nothing left to decide: the reject's undo back past
        // it put the rejected text into the next review's baseline.
        (
            "",
            vec![
                marked(0, 0, ""),
                RejectSome(43),
                reload(0, 0, ""),
                agent(0, 0, "x"),
            ],
        ),
        (
            "a\n",
            vec![
                marked(0, 0, ""),
                AgentAll { from: "a", to: "" },
                RejectSome(1),
                reload(0, 0, ""),
                marked(0, 0, ""),
            ],
        ),
        // The undo of a reload counted as the undo of the turn whose change it touched: Reject
        // Turn then put the other program's text back as what the turn had replaced.
        (
            "x\ny",
            vec![
                marked(12242907064904609119, 0, ""),
                reload(2479585681371592516, 1, ""),
                Undo,
                RejectTurn(0),
                user(2170169534388014528, 3, ""),
                Reject(0),
                AltU,
                AcceptSome(5),
            ],
        ),
        // Alt+U of a reject whose proposal an accept had taken (and the reject's redo had
        // taken out again), after the tab closed and opened again: the buffer's undo of Alt+U's
        // action went where the user's change would.
        (
            "a\n",
            vec![
                reload(1273858947104882489, 0, "\n\n"),
                alt_y.clone(),
                Open,
                agent(3943532309248917040, 1, ""),
                reload(62886354837811, 5, ""),
                RejectNow(1549150338952477987),
                Undo,
                AcceptSome(102),
                Redo,
                AltU,
                Close { discard: false },
                Open,
                AltU,
            ],
        ),
        // Alt+U of an accept that could not be reverted any more made the user's edits forget
        // where they went: the Ctrl+Z of the user's typing went into the hunk next to it.
        (
            "x\ny",
            vec![
                marked(440568689716306184, 3, "x"),
                Typing {
                    at: 0,
                    keys: vec!["x", "x"],
                },
                Undo,
                alt_y.clone(),
                marked(1657577213962673111, 3, "é"),
                agent(2238174734820851604, 5, "x"),
                alt_y.clone(),
                marked(9028107500803448624, 0, "x"),
                Delete {
                    at: 8128978437794204735,
                    n: 2,
                },
                Close { discard: true },
                Open,
                user(2316848911476304068, 0, "x"),
                AltU,
                Open,
            ],
        ),
        // An agent edit that took out the rest of an earlier one (and baseline text with it),
        // accepted: its undo puts the baseline's text back into both texts and the earlier edit's
        // as a change to review, and the earlier edit's deletion, which the accept took too, goes
        // back into both.
        (
            "a\nb\nc\n",
            vec![
                agent(8448147030800287096, 0, "x"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                Undo,
                Redo,
                marked(2670781877066027236, 2, "é"),
                agent(6624519437652692766, 7, "yz"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                agent(0, 0, ""),
            ],
        ),
        // Another program's text an agent edit took out, accepted: its undo brings that text back
        // as a change to review, not into the baseline.
        (
            "",
            vec![
                Close { discard: false },
                marked(0, 0, "é"),
                Reload {
                    at: 5690743063101598118,
                    len: 3,
                    text: "é",
                    keep_mine: false,
                },
                Open,
                marked(14943679215606899954, 5, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // Baseline text and accepted text an agent edit took out, accepted: its undo and redo go
        // into both texts where the two texts hold that text (here next to a hunk whose texts are
        // equal).
        (
            "a\n",
            vec![
                marked(195768130673200237, 0, "x"),
                agent(5081728529777187142, 1, "yz"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                marked(206834213535744630, 0, ""),
                Undo,
                marked(1982472702878388136, 2, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // Reloads, the user's typing and an agent edit of every line, accepted with the tab
        // reopened.
        (
            "a\nb\nc\n",
            vec![
                Typing {
                    at: 0,
                    keys: vec!["x", "yz"],
                },
                marked(6439087325824621120, 0, "x"),
                Reload {
                    at: 5552830767295794972,
                    len: 2,
                    text: "yz",
                    keep_mine: false,
                },
                AgentAll { from: "\n", to: "" },
                Redo,
                agent(0, 0, ""),
                marked(0, 0, ""),
                Open,
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // An agent edit that took out part of an earlier one's text, which it kept: the accept took
        // what was left of that text.
        (
            "one\ntwo\nthree\nfour\n",
            vec![
                agent(13789592721103523424, 3, "∑\n"),
                agent(378491469298580270, 1, "x"),
                RejectTurn(1),
                marked(1719399652482290968, 2, "a\nb"),
                marked(3932077208681092393, 0, ""),
                marked(1765390481588189427, 0, "yz"),
                marked(1349938048070207046, 2, ""),
                AltU,
                AcceptAll,
            ],
        ),
        // Accepts, reloads and Accept of several hunks with the GUI's recomputes: the redo of a
        // deletion inside a run of equal characters.
        (
            "a\nb\nc\n",
            vec![
                Typing {
                    at: 0,
                    keys: vec!["é", "yz"],
                },
                marked(11704401301927010614, 1, "yz"),
                Reload {
                    at: 3670703012587950311,
                    len: 2,
                    text: "",
                    keep_mine: false,
                },
                Accept {
                    pick: 0,
                    fresh: false,
                },
                agent(8060838569489984447, 0, "yz"),
                AgentAll { from: "\n", to: "" },
                AcceptSome(125),
                Undo,
                marked(2725874926341992576, 3, "x"),
                Reload {
                    at: 16489159083456041082,
                    len: 6,
                    text: "",
                    keep_mine: false,
                },
                agent(0, 0, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // The user's typing inside text an agent edit took out, its undo and redo before that
        // edit's.
        (
            "one\ntwo\nthree\nfour\n",
            vec![
                agent(1192375926108560440, 6, "x"),
                user(7074237400767223656, 1, "∑\n"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                Backspace {
                    at: 5007205368448373434,
                    n: 3,
                },
                marked(7370751996733714863, 3, "é"),
                agent(2469932530682766006, 2, "yz"),
                Reload {
                    at: 0,
                    len: 0,
                    text: "",
                    keep_mine: false,
                },
                AcceptSome(49),
                Undo,
            ],
        ),
        // A reject undone by Alt+U, the agent's edits over it, accepted.
        (
            "a\n",
            vec![
                agent(0, 0, ""),
                Turn(0),
                Backspace { at: 0, n: 2 },
                marked(12214350254321754167, 0, "é"),
                marked(15187896559511860296, 1, "\n"),
                marked(1203604173212954338, 6, ""),
                RejectSome(115),
                AltU,
                agent(1955552938376719617, 0, "a\nb"),
                agent(0, 4, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // An agent deletion inside a reject's proposal that Alt+U brought back, accepted, then
        // undone: the text it puts back is the proposal's as accepted.
        (
            "",
            vec![
                Reload {
                    at: 0,
                    len: 0,
                    text: "yz",
                    keep_mine: false,
                },
                marked(6170025168694638554, 3, "a\nb"),
                RejectNow(0),
                marked(3358268019726017287, 1, "a\nb"),
                marked(5311748050184894252, 1, "yz"),
                AltU,
                user(10036142904459809065, 0, "a\nb"),
                user(387746882178691366, 0, "x"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                Undo,
                agent(7258138146969258687, 1, ""),
            ],
        ),
        // A deletion of accepted text that a reject took back, then accepted with the agent's next
        // edit: the accept did not take the deletion.
        (
            "a\nb\nc\n",
            vec![
                marked(185841280700631297, 1, "é"),
                RejectNow(0),
                AltU,
                AcceptAll,
                AgentAll { from: "é", to: "" },
                RejectSome(7),
                marked(552750769802704, 0, ""),
                AcceptAll,
            ],
        ),
        // A reject undone by Alt+U, then the undo of Alt+U's action and of the reject's own: two
        // actions of one record, the second where the first left the baseline.
        (
            "a\n",
            vec![
                marked(1553666125774288449, 2, "é"),
                marked(7554518352159224838, 2, "\r\n"),
                RejectSome(89),
                AltU,
                Backspace {
                    at: 3962751215112397304,
                    n: 2,
                },
                marked(15945831678540557176, 1, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                Undo,
            ],
        ),
        // The user's typing in a hunk that Alt+U brought back, then Reject Turn and Accept all.
        (
            "a a\nb a\na\n",
            vec![
                agent(7557232001482129049, 4, "x"),
                Typing {
                    at: 8082101657730900244,
                    keys: vec!["a\nb", "x", "x"],
                },
                RejectSome(27),
                AltU,
                agent(0, 0, ""),
                marked(0, 0, "x"),
                agent(559921640564, 1, ""),
                agent(0, 0, ""),
                RejectTurn(1),
                RejectTurn(1),
                AcceptAll,
                Undo,
            ],
        ),
        // The user's deletion of the agent's text, accepted: its undo brings that text back as a
        // change to review.
        (
            "a a\nb a\na\n",
            vec![
                marked(7603803179241855975, 6, "yz"),
                user(7909384492882766536, 0, "é"),
                Delete {
                    at: 8947002778252968498,
                    n: 2,
                },
                Undo,
                agent(9082288730481337093, 7, "é"),
                user(9745320651045974809, 3, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // Reloads over the agent's text, the user's deletions in them, accepted.
        (
            "",
            vec![
                marked(0, 0, "yz"),
                Reload {
                    at: 7009208396056393493,
                    len: 0,
                    text: "\n\n",
                    keep_mine: false,
                },
                Backspace {
                    at: 4286892917702558080,
                    n: 3,
                },
                marked(12006224857185215454, 0, ""),
                Reload {
                    at: 1705461044893142297,
                    len: 3,
                    text: "yz",
                    keep_mine: false,
                },
                marked(9369169343018936609, 3, "x"),
                Delete {
                    at: 2191411031541959873,
                    n: 2,
                },
                agent(0, 0, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // The user's deletion of baseline text inside a hunk with changes on both sides of it,
        // accepted: its undo puts that text back into both texts.
        (
            "a\nb\nc\n",
            vec![
                user(2305144719560066335, 2, "a\nb"),
                agent(2249487657678164010, 1, "∑\n"),
                marked(8346263745347018482, 6, ""),
                agent(3006579901265822792, 6, "é"),
                Delete {
                    at: 172089537007073352,
                    n: 2,
                },
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // A reload before the review started, then the review's own changes: the buffer's undo of
        // that reload, which the review has no step of, leaves where the later ones went.
        (
            "",
            vec![
                Close { discard: false },
                Reload {
                    at: 0,
                    len: 0,
                    text: "",
                    keep_mine: false,
                },
                Open,
                Reload {
                    at: 0,
                    len: 0,
                    text: "yz",
                    keep_mine: false,
                },
                marked(8186652304994973263, 0, ""),
                Delete {
                    at: 1972620073832524272,
                    n: 2,
                },
                Accept {
                    pick: 0,
                    fresh: false,
                },
                Open,
                Open,
            ],
        ),
        // Alt+U of a reject over the user's Backspace in its proposal, accepted: the steps of the
        // reject's record and of Alt+U's action, undone and redone, go back exactly where they
        // went.
        (
            "x\ny",
            vec![
                agent(10671085778405339605, 2, "yz"),
                marked(907105448760255148, 1, "x"),
                user(16547765286547032405, 0, "\n"),
                Accept {
                    pick: 0,
                    fresh: false,
                },
                marked(2393298531838622846, 1, "yz"),
                RejectNow(0),
                Backspace {
                    at: 7177462107351237882,
                    n: 2,
                },
                AltU,
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // A replace_all rejected, the agent's edits over the text the reject put back, accepted,
        // with the GUI's recomputes: the agent's text that a reject took out is not the baseline's,
        // whatever characters it shares with it.
        (
            "a a\nb a\na\n",
            vec![
                AgentAll {
                    from: "a",
                    to: "∑\n",
                },
                RejectNow(0),
                marked(1840594007987876565, 1, "a\nb"),
                Redo,
                Reject(0),
                marked(2864429743588611518, 7, ""),
                Accept {
                    pick: 0,
                    fresh: false,
                },
            ],
        ),
        // The user's Delete, three times over the agent's text and the baseline's end: the steps of
        // the edits it merged, undone and redone, each where it went.
        (
            "a\nb\nc\n",
            vec![
                marked(6290595884662168614, 1, "\n"),
                marked(674603031246749911, 7, "a\nb"),
                Delete {
                    at: 3240367399269211710,
                    n: 3,
                },
                Accept {
                    pick: 0,
                    fresh: false,
                },
                agent(0, 0, ""),
            ],
        ),
    ];
    let failed: Vec<String> = cases
        .iter()
        .flat_map(|(base, steps)| [false, true].map(|gui| (base, steps, gui)))
        .filter_map(|(base, steps, gui)| {
            let e = decide_undo_and_redo_all(base, steps, gui).err()?;
            Some(format!("{base:?} {steps:?} {gui}: {e}"))
        })
        .collect();
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Plays `steps` in the app from `base`, then (in an open document) undoes every action of the
/// buffer's history and redoes them all: the text is the one the history started with once they
/// are undone, and so is the baseline, unless the review was not there since or an accept (or
/// Alt+U of one) changed it (`App::history_baseline`); and both texts are the ones the session
/// ended with once they are all redone. The markers are checked after every step, every undo and
/// every redo.
fn decide_undo_and_redo_all(base: &str, steps: &[Step], gui: bool) -> Result<(), TestCaseError> {
    let mut b = Buffer::in_app(base, gui);
    for step in steps {
        b.play(step);
    }
    if b.closed() {
        return Ok(());
    }
    b.recompute();
    let reviewing = b.reviewing();
    if reviewing {
        b.s.check();
    }
    let (text, baseline) = (b.s.text.clone(), b.s.review.baseline().to_owned());
    let done = b.undo.len();
    while !b.undo.is_empty() {
        b.undo();
        b.check_marks(&Step::Undo);
    }
    b.recompute();
    let app = b.app.as_ref().expect("a buffer in the app");
    prop_assert_eq!(&b.s.text, &app.history_text);
    if reviewing && let Some(started) = &app.history_baseline {
        prop_assert_eq!(b.s.review.baseline(), started.as_str());
    }
    for _ in 0..done {
        b.redo();
        b.check_marks(&Step::Redo);
    }
    b.recompute();
    prop_assert_eq!(&b.s.text, &text);
    if reviewing {
        prop_assert_eq!(b.s.review.baseline(), baseline.as_str());
    }
    Ok(())
}
