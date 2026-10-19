//! Recognizes buffer undo and redo (`Origin::History`) of our own edits.
//!
//! Each agent edit and each reject is recorded with its range in the current text (kept up to date
//! through later edits), the text the range holds, and the texts before and after the edit. While
//! the range still holds the edit's result, a History edit that deletes or replaces the whole
//! range, or inserts the old text at one of its ends, starts an undo; the following edits inside
//! the range continue it until the text before the edit is back. Redo works the other way round
//! until a new action clears the buffer's redo stack.
//!
//! The buffer's undo stack is linear: when the undo of an edit completes, every later edit has been
//! undone too, so the text is exactly what it was before that edit. That lets a record keep the
//! records its edit cut through (a later agent edit overlapping an earlier one, a reject over an
//! agent edit) and put them back, unchanged, when its edit is undone; records cut by the undo come
//! back when the edit is redone. Positions are otherwise ambiguous only where empty records
//! (deleted text) coincide, so a record also remembers which records enclose it and which empty
//! records come right after it.
//!
//! Recognition is best effort: an edit it misses is classified by the Inside rule like any other
//! History edit.

use super::Reopen;
use crate::text::{overlaps, shift_range};
use lemma_core::{Origin, TextEdit};
use std::ops::Range;

/// Top-level records kept at most (oldest dropped first). The records of the newest decision are
/// never dropped, however many there are (see [`Records::begin_decision`]).
const MAX_RECORDS: usize = 512;
/// Records dropped at once when the list is full, so that a long run of agent edits (a
/// replace_all) does not shift the whole list for every one of them.
const DROP_AT_ONCE: usize = MAX_RECORDS / 8;

/// Agent edits whose texts are larger than this (before plus after) are not recorded: a record
/// holds copies of them, and undoing such an edit is still classified by the Inside rule.
const MAX_AGENT_RECORD_BYTES: usize = 256 * 1024;

/// What a record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// An agent edit.
    Agent,
    /// A reject; undoing it re-opens the hunk with this identity.
    Reject(Reopen),
}

/// Where a recorded edit stands in the buffer's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Returned to the caller, not applied yet (rejects: waiting for the `Origin::Review` edit).
    Applying,
    /// Applied; History edits may undo it.
    Done,
    /// Undone; History edits may redo it.
    Undone,
}

/// The step an edit was recognized as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Apply,
    Undo,
    Redo,
}

/// An edit recognized as (part of) applying, undoing or redoing a recorded edit.
#[derive(Clone, Copy, Debug)]
pub(super) struct Claim {
    pub kind: Kind,
    pub step: Step,
    /// The edit completes the step.
    pub done: bool,
    /// The record (`None` for an undo recognized without one).
    pub record: Option<u64>,
    /// The record's range after the edit (start, end).
    pub range: (usize, usize),
    /// The turn of a recorded agent edit.
    pub turn: Option<u32>,
}

/// What an unclaimed edit did to the records, for the edit's own record (if it gets one).
#[derive(Debug, Default)]
pub(super) struct Context {
    /// Records the edit cut through, or moved in a way its undo cannot reverse (as they were
    /// before the edit); the edit's record keeps them.
    cut: Vec<Record>,
    /// Records whose text took the edit in; the edit's record is nested in them.
    enclosing: Vec<u64>,
}

#[derive(Clone, Debug)]
struct Record {
    id: u64,
    kind: Kind,
    /// The turn of an agent edit (see `FileReview::set_turn`).
    turn: Option<u32>,
    /// Range in the current text.
    range: Range<usize>,
    /// `current[range]`.
    content: String,
    /// The range's text before the recorded edit.
    before: String,
    /// The range's text after the recorded edit.
    after: String,
    state: State,
    /// Some but not all edits of the current step have arrived.
    in_progress: bool,
    /// Records cut by applying or redoing this edit; restored when it is undone.
    cut_forward: Vec<Record>,
    /// Records cut by undoing this edit; restored when it is redone.
    cut_backward: Vec<Record>,
    /// Records whose text contained this edit when it was made (or applied, undone, redone): its
    /// undo and redo restore their text too, even at their boundaries.
    enclosing: Vec<u64>,
    /// Empty records that sat at the end of this record's range when one of its steps deleted
    /// that end: they come after its text, so they follow its end through later steps while they
    /// still sit there.
    pinned_end: Vec<u64>,
}

impl Record {
    fn step(&self) -> Step {
        match self.state {
            State::Applying => Step::Apply,
            State::Done => Step::Undo,
            State::Undone => Step::Redo,
        }
    }

    /// The text the range holds before / after the next step.
    fn step_texts(&self) -> (&str, &str) {
        match self.state {
            State::Applying | State::Undone => (&self.before, &self.after),
            State::Done => (&self.after, &self.before),
        }
    }

    /// Whether the edit `[a, b) → text` is (part of) this record's next step.
    fn accepts(&self, origin: Origin, a: usize, b: usize, text: &str) -> bool {
        let origin_fits = match (self.kind, self.state) {
            (_, State::Applying) => origin == Origin::Review,
            // Alt+U re-applies a rejected proposal with Origin::Review.
            (Kind::Reject(_), State::Done) => matches!(origin, Origin::History | Origin::Review),
            (Kind::Agent, State::Done) | (_, State::Undone) => origin == Origin::History,
        };
        origin_fits && self.within(a, b) && (self.in_progress || self.starts_step(a, b, text))
    }

    /// The edit lies in the range, boundaries included.
    fn within(&self, a: usize, b: usize) -> bool {
        self.range.start <= a && b <= self.range.end
    }

    /// A step starts from the expected text with a full replacement, a full deletion, or an
    /// insertion of the whole target at either end of the range.
    fn starts_step(&self, a: usize, b: usize, text: &str) -> bool {
        let (from, to) = self.step_texts();
        let (s, e) = (self.range.start, self.range.end);
        self.content == from
            && ((a == s && b == e && (text.is_empty() || text == to))
                || (a == b && (a == s || a == e) && text == to))
    }

    /// The edit changes text inside the range (an insertion strictly inside, or a deletion within).
    fn changes_inside(&self, a: usize, b: usize) -> bool {
        let (s, e) = (self.range.start, self.range.end);
        if a < b {
            s <= a && b <= e
        } else {
            s < a && a < e
        }
    }

    /// Applies an edit lying within the range to the tracked content.
    fn apply(&mut self, a: usize, b: usize, text: &str) {
        let s = self.range.start;
        self.content.replace_range(a - s..b - s, text);
        self.range.end = s + self.content.len();
    }

    /// Ends the current step: the records cut by the opposite direction come back.
    fn finish_step(&mut self, restored: &mut Vec<Record>) {
        let (state, back) = match self.state {
            State::Applying => (State::Done, None),
            State::Done => (State::Undone, Some(&mut self.cut_forward)),
            State::Undone => (State::Done, Some(&mut self.cut_backward)),
        };
        if let Some(back) = back {
            restored.append(back);
        }
        self.state = state;
        self.in_progress = false;
    }

    /// Keeps records cut by the current step of this record.
    fn keep_cut(&mut self, cut: &mut Vec<Record>) {
        match self.state {
            State::Applying | State::Undone => self.cut_forward.append(cut),
            State::Done => self.cut_backward.append(cut),
        }
    }
}

/// The recorded edits, oldest first (by id).
#[derive(Clone, Debug, Default)]
pub(super) struct Records {
    list: Vec<Record>,
    next_id: u64,
    /// The buffer's last completed undo or redo was a redo: the user is stepping forward.
    redoing: bool,
    /// Records from this id on belong to the newest decision: the cap does not drop them, so that
    /// a decision on more hunks than the cap can still be undone as a whole.
    protected_from: u64,
}

impl Records {
    /// A decision starts: the reject records pushed from now on (until the next decision starts)
    /// are kept whatever the cap.
    pub fn begin_decision(&mut self) {
        self.protected_from = self.next_id;
    }

    /// Records an agent edit that turned `before` into `after`, now at `range`, with what the edit
    /// did to the other records.
    pub fn push_agent(
        &mut self,
        range: Range<usize>,
        before: String,
        after: String,
        context: Context,
        turn: u32,
    ) {
        if before.len() + after.len() > MAX_AGENT_RECORD_BYTES {
            return;
        }
        let parked: Vec<u64> = context.cut.iter().map(|r| r.id).collect();
        self.list.retain(|r| !parked.contains(&r.id));
        let content = after.clone();
        let id = self.push(Kind::Agent, range, content, before, after, State::Done);
        if let Some(record) = self.list.iter_mut().find(|r| r.id == id) {
            record.cut_forward = context.cut;
            record.enclosing = context.enclosing;
            record.turn = Some(turn);
        }
    }

    /// Whether the record `id` is still kept.
    pub fn contains(&self, id: u64) -> bool {
        // The list is sorted by id.
        self.list.binary_search_by_key(&id, |r| r.id).is_ok()
    }

    /// Records a reject edit about to replace `proposal` (at `range`) by `restored`.
    pub fn push_reject(
        &mut self,
        range: Range<usize>,
        proposal: String,
        restored: String,
        reopen: Reopen,
    ) -> u64 {
        let content = proposal.clone();
        self.push(
            Kind::Reject(reopen),
            range,
            content,
            proposal,
            restored,
            State::Applying,
        )
    }

    fn push(
        &mut self,
        kind: Kind,
        range: Range<usize>,
        content: String,
        before: String,
        after: String,
        state: State,
    ) -> u64 {
        if self.list.len() >= MAX_RECORDS {
            // The oldest records go first, never the rejects of the newest decision.
            let protected = self.protected_from;
            let mut dropped = 0;
            self.list.retain(|r| {
                let keep = dropped == DROP_AT_ONCE
                    || (matches!(r.kind, Kind::Reject(_)) && r.id >= protected);
                if !keep {
                    dropped += 1;
                }
                keep
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Record {
            id,
            kind,
            turn: None,
            range,
            content,
            before,
            after,
            state,
            in_progress: false,
            cut_forward: Vec::new(),
            cut_backward: Vec::new(),
            enclosing: Vec::new(),
            pinned_end: Vec::new(),
        });
        id
    }

    /// Tracks the edit `[a, b) → text` through every record. Returns what the edit was recognized
    /// as (the newest record that accepts it claims it; none when `claimable` is false, for an
    /// edit known to be something else) and, when no record claimed it, what it did to the
    /// records.
    pub fn observe(
        &mut self,
        a: usize,
        b: usize,
        text: &str,
        origin: Origin,
        claimable: bool,
    ) -> (Option<Claim>, Context) {
        // The newest record whose step the edit fits. A History edit can undo one edit and redo
        // another at the same time when an agent edit reverted an earlier one; the user usually
        // keeps stepping in the same direction, so that direction goes first.
        let accepting = |r: &Record| r.accepts(origin, a, b, text);
        let preferred = if self.redoing {
            State::Undone
        } else {
            State::Done
        };
        // Only undo/redo and our own review edits can be a recorded step (see `accepts`).
        let claimer = if claimable && matches!(origin, Origin::History | Origin::Review) {
            self.list
                .iter()
                .rposition(|r| r.state == preferred && accepting(r))
                .or_else(|| self.list.iter().rposition(accepting))
        } else {
            None
        };
        // Records enclosing the claimer take the edit into their text even at their boundary.
        let enclosing = claimer
            .map_or(&[][..], |i| &self.list[i].enclosing)
            .to_vec();
        let claimer_end = claimer.map(|i| self.list[i].range.end);
        let mut pinned = claimer.map_or(Vec::new(), |i| self.list[i].pinned_end.clone());
        pinned.retain(|id| {
            self.list
                .iter()
                .any(|r| r.id == *id && Some(r.range.clone()) == claimer_end.map(|e| e..e))
        });
        let mut claimed: Option<(usize, Claim)> = None;
        let mut cut = Vec::new();
        let mut displaced = Vec::new();
        let mut trackers = Vec::new();
        let mut keep = vec![true; self.list.len()];
        // A pure deletion moves an empty record at its end onto its start, where an undo cannot
        // tell it from one that was there. Within a recognized step the record follows the end of
        // the claimer's range; an agent edit's own record keeps it as it was (see `push_agent`).
        let collapses = a < b && text.is_empty();
        for (i, rec) in self.list.iter_mut().enumerate().rev() {
            let encloses = enclosing.contains(&rec.id) && rec.within(a, b);
            if origin != Origin::History && rec.state == State::Undone {
                // A new action clears the buffer's redo stack.
                keep[i] = false;
            } else if Some(i) == claimer {
                let step = rec.step();
                rec.apply(a, b, text);
                let done = rec.content == rec.step_texts().1;
                rec.in_progress = !done;
                let claim = Claim {
                    kind: rec.kind,
                    step,
                    done,
                    record: Some(rec.id),
                    range: (rec.range.start, rec.range.end),
                    turn: rec.turn,
                };
                claimed = Some((i, claim));
            } else if rec.changes_inside(a, b) || encloses {
                if rec.in_progress {
                    // Something else interrupted the step.
                    keep[i] = false;
                } else {
                    rec.apply(a, b, text);
                    trackers.push(rec.id);
                }
            } else {
                let collapsing = collapses && !rec.in_progress && rec.range == (b..b);
                let before = (collapsing && claimer.is_none()).then(|| rec.clone());
                if collapsing && claimer_end == Some(b) && !pinned.contains(&rec.id) {
                    pinned.push(rec.id);
                }
                if !shift_range(&mut rec.range, a, b, text.len()) {
                    keep[i] = false;
                    if !rec.in_progress {
                        cut.push(rec.clone());
                    }
                } else if let Some(before) = before {
                    displaced.push(before);
                }
            }
        }
        cut.reverse();
        let mut restored = Vec::new();
        let claim = claimed.map(|(i, claim)| {
            let rec = &mut self.list[i];
            rec.keep_cut(&mut cut);
            for id in trackers.drain(..) {
                if !rec.enclosing.contains(&id) {
                    rec.enclosing.push(id);
                }
            }
            let end = rec.range.end;
            rec.pinned_end = pinned.clone();
            if claim.done {
                rec.finish_step(&mut restored);
                match claim.step {
                    Step::Undo if origin == Origin::History => self.redoing = false,
                    Step::Redo => self.redoing = true,
                    _ => {}
                }
            }
            for other in self.list.iter_mut().filter(|r| pinned.contains(&r.id)) {
                other.range = end..end;
            }
            claim
        });
        let mut keep = keep.into_iter();
        self.list.retain(|_| keep.next().unwrap_or(true));
        // Without a claimer, the edit's own record (if it gets one) keeps the collapsed records.
        cut.append(&mut displaced);
        cut.sort_by_key(|r| r.id);
        if !restored.is_empty() {
            self.list.append(&mut restored);
            self.list.sort_by_key(|r| r.id);
        }
        (
            claim,
            Context {
                cut,
                enclosing: trackers,
            },
        )
    }

    /// Keeps the records for which `valid(range, content)` holds.
    pub fn retain(&mut self, valid: impl Fn(&Range<usize>, &str) -> bool) {
        self.list.retain(|r| valid(&r.range, &r.content));
    }

    /// Forgets records overlapping `range` (its text was accepted into the baseline).
    pub fn forget_touching(&mut self, range: &Range<usize>) {
        self.list.retain(|r| !overlaps(&r.range, range));
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// The edits re-applying the proposals of these reject records (Alt+U), descending. Records
    /// whose text was changed since, or whose reject was already undone, give none.
    pub fn reapply_edits(&self, ids: &[u64], current: &str) -> Vec<TextEdit> {
        let mut edits: Vec<TextEdit> = self
            .list
            .iter()
            .filter(|r| {
                ids.contains(&r.id)
                    && matches!(r.kind, Kind::Reject(_))
                    && r.state == State::Done
                    && !r.in_progress
                    && r.content == r.after
                    && current.get(r.range.clone()) == Some(r.content.as_str())
            })
            .map(|r| TextEdit {
                range: r.range.clone(),
                text: r.before.clone(),
            })
            .collect();
        // Descending, a replacement before an insertion at its start (records of one decision
        // can meet: a turn's deletion point next to its insertion).
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start, e.range.end)));
        edits
    }
}
