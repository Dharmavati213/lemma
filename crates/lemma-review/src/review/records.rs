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
//! The records of the newest decision (a Reject all) and the agent edits of the newest turn (a
//! replace_all and the edits around it) are all kept (up to [`MAX_TURN_RECORDS`] of the turn's),
//! so that the whole decision or turn is recognized when it is undone and redone; older ones are
//! dropped beyond a cap. An edit only meets the records next to it, so the ranges are kept in a
//! [`Points`] index: an edit costs the records it meets (and those between it and the edit
//! before), not every record.
//!
//! Recognition is best effort: an edit it misses is classified by the Inside rule like any other
//! History edit.

use super::Reopen;
use crate::text::{overlaps, shift_range, signed};
use lemma_core::{Origin, TextEdit};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// Top-level records kept at most (oldest dropped first). The rejects of the newest decision and
/// the agent edits of the newest turn are never dropped, however many there are (see
/// [`Records::begin_decision`] and [`Records::push_agent`]).
const MAX_RECORDS: usize = 512;
/// Records dropped at once when the list is full, so that a long run of agent edits (a
/// replace_all) does not shift the whole list for every one of them.
const DROP_AT_ONCE: usize = MAX_RECORDS / 8;
/// Agent records of the newest turn kept at most whatever the cap (its last ones): a few hundred
/// bytes each, so that a replace_all of every space in a big file does not take hundreds of
/// megabytes. That is 16 384 matches in an open buffer (a deletion and an insertion each), 32 768
/// in a closed document.
const MAX_TURN_RECORDS: u64 = 1 << 15;

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
    /// Range in the current text. While the record is in [`Records`]' list this is where it was
    /// when it went in: the list's [`Points`] know where it is now.
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

/// The recorded edits.
#[derive(Clone, Debug, Default)]
pub(super) struct Records {
    /// Every record kept, by id (oldest first). Boxed: a map's nodes are often half empty.
    list: BTreeMap<u64, Box<Record>>,
    /// Where the ranges of the records in `list` are.
    points: Points,
    next_id: u64,
    /// The buffer's last completed undo or redo was a redo: the user is stepping forward.
    redoing: bool,
    /// Rejects from this id on belong to the newest decision: the cap does not drop them, so that
    /// a decision on more hunks than the cap can still be undone as a whole.
    decision_from: u64,
    /// Agent records from this id on belong to the newest turn (`turn`): the cap does not drop
    /// them either, so that a replace_all with more matches than the cap, and the turn's edits
    /// after it, can be undone and redone as a whole.
    turn_from: u64,
    /// The turn of the newest agent record (see `FileReview::set_turn`).
    turn: Option<u32>,
    /// The records in `list` the cap may drop (neither the newest decision's rejects nor the
    /// newest turn's agent edits), oldest first.
    droppable: BTreeSet<u64>,
    /// The undone records in `list`: a new action clears the buffer's redo stack.
    undone: BTreeSet<u64>,
    /// Records the last completed step brought back, to be checked against the text (see
    /// [`retain_restored`](Self::retain_restored)).
    restored: Vec<u64>,
}

impl Records {
    /// A decision starts: the reject records pushed from now on (until the next decision starts)
    /// are kept whatever the cap.
    pub fn begin_decision(&mut self) {
        // The rejects of the decision before are like any other record now.
        let older: Vec<u64> = self
            .list
            .range(self.decision_from..)
            .filter(|(_, r)| matches!(r.kind, Kind::Reject(_)))
            .map(|(id, _)| *id)
            .collect();
        self.droppable.extend(older);
        self.decision_from = self.next_id;
    }

    /// Agent edits of the turn `turn` start: their records are kept whatever the cap, until the
    /// agent edits of another turn start.
    fn begin_turn(&mut self, turn: u32) {
        let older: Vec<u64> = self
            .list
            .range(self.turn_from..)
            .filter(|(_, r)| r.kind == Kind::Agent)
            .map(|(id, _)| *id)
            .collect();
        self.droppable.extend(older);
        self.turn_from = self.next_id;
        self.turn = Some(turn);
    }

    /// Records an agent edit that turned `before` into `after`, now at `range`, with what the edit
    /// did to the other records. An agent edit of another turn than the one before starts the
    /// newest turn's records.
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
        // The records it moved in a way its undo cannot reverse are parked in it.
        for r in &context.cut {
            self.forget(r.id);
        }
        if self.turn != Some(turn) {
            self.begin_turn(turn);
        }
        if self.next_id - self.turn_from >= MAX_TURN_RECORDS {
            // A huge turn keeps its last records only.
            if self
                .list
                .get(&self.turn_from)
                .is_some_and(|r| r.kind == Kind::Agent)
            {
                self.droppable.insert(self.turn_from);
            }
            self.turn_from += 1;
        }
        let content = after.clone();
        let id = self.push(Kind::Agent, range, content, before, after, State::Done);
        if let Some(record) = self.list.get_mut(&id) {
            record.cut_forward = context.cut;
            record.enclosing = context.enclosing;
            record.turn = Some(turn);
        }
    }

    /// Whether the record `id` is still kept.
    pub fn contains(&self, id: u64) -> bool {
        self.list.contains_key(&id)
    }

    /// Renumbers the turns of the agent records, and of the records they cut, with `f` (see
    /// `FileReview::remap_turns`): the newest turn's edits after the renumbering still belong to
    /// it.
    pub fn remap_turns(&mut self, f: &mut impl FnMut(u32) -> u32) {
        fn remap(r: &mut Record, f: &mut impl FnMut(u32) -> u32) {
            r.turn = r.turn.map(&mut *f);
            for kept in r.cut_forward.iter_mut().chain(&mut r.cut_backward) {
                remap(kept, f);
            }
        }
        self.turn = self.turn.map(&mut *f);
        for r in self.list.values_mut() {
            remap(r, f);
        }
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
            // The oldest records go first, never those of the newest decision or turn.
            for _ in 0..DROP_AT_ONCE {
                let Some(id) = self.droppable.pop_first() else {
                    break;
                };
                self.forget(id);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.attach(Box::new(Record {
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
        }));
        id
    }

    /// Puts `record` into the list at its `range`.
    fn attach(&mut self, record: Box<Record>) {
        let id = record.id;
        debug_assert!(!self.list.contains_key(&id), "record {id} is kept twice");
        self.points.insert(id, &record.range);
        if record.state == State::Undone {
            self.undone.insert(id);
        }
        let kept = match record.kind {
            Kind::Reject(_) => id >= self.decision_from,
            Kind::Agent => id >= self.turn_from,
        };
        if !kept {
            self.droppable.insert(id);
        }
        self.list.insert(id, record);
    }

    /// Takes the record `id` out of the list, with its current range.
    fn detach(&mut self, id: u64) -> Option<Box<Record>> {
        let mut record = self.list.remove(&id)?;
        if let Some(range) = self.points.remove(id) {
            record.range = range;
        }
        self.undone.remove(&id);
        self.droppable.remove(&id);
        Some(record)
    }

    /// Drops the record `id`.
    fn forget(&mut self, id: u64) {
        self.detach(id);
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
        self.restored.clear();
        if origin != Origin::History {
            // A new action clears the buffer's redo stack.
            for id in std::mem::take(&mut self.undone) {
                self.forget(id);
            }
        }
        // The records the edit meets, out of the list meanwhile, newest first. The others lie
        // wholly before the edit, which leaves them alone, or wholly after it, which moves them
        // by its length change (see `Points`).
        let ids = self.points.meeting(a, b);
        let mut met: Vec<Box<Record>> =
            ids.iter().rev().filter_map(|&id| self.detach(id)).collect();
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
            met.iter()
                .position(|r| r.state == preferred && accepting(r))
                .or_else(|| met.iter().position(|r| accepting(r)))
        } else {
            None
        };
        let mut claimer = claimer.map(|i| met.remove(i));
        // Records enclosing the claimer take the edit into their text even at their boundary.
        let enclosing = claimer.as_ref().map_or(Vec::new(), |r| r.enclosing.clone());
        let claimer_end = claimer.as_ref().map(|r| r.range.end);
        let mut pinned = claimer
            .as_ref()
            .map_or(Vec::new(), |r| r.pinned_end.clone());
        pinned.retain(|id| {
            let range = match met.iter().find(|r| r.id == *id) {
                Some(r) => Some(r.range.clone()),
                None => self.points.get(*id),
            };
            claimer_end.is_some_and(|e| range == Some(e..e))
        });
        let claimed = claimer.as_mut().map(|rec| {
            let step = rec.step();
            rec.apply(a, b, text);
            let done = rec.content == rec.step_texts().1;
            rec.in_progress = !done;
            Claim {
                kind: rec.kind,
                step,
                done,
                record: Some(rec.id),
                range: (rec.range.start, rec.range.end),
                turn: rec.turn,
            }
        });
        let mut cut = Vec::new();
        let mut displaced = Vec::new();
        let mut trackers = Vec::new();
        let mut kept = Vec::with_capacity(met.len());
        // A pure deletion moves an empty record at its end onto its start, where an undo cannot
        // tell it from one that was there. Within a recognized step the record follows the end of
        // the claimer's range; an agent edit's own record keeps it as it was (see `push_agent`).
        let collapses = a < b && text.is_empty();
        for mut rec in met {
            let encloses = enclosing.contains(&rec.id) && rec.within(a, b);
            if rec.changes_inside(a, b) || encloses {
                if rec.in_progress {
                    // Something else interrupted the step: the record goes.
                    continue;
                }
                rec.apply(a, b, text);
                trackers.push(rec.id);
                kept.push(rec);
                continue;
            }
            let collapsing = collapses && !rec.in_progress && rec.range == (b..b);
            let before = (collapsing && claimer.is_none()).then(|| Record::clone(&rec));
            if collapsing && claimer_end == Some(b) && !pinned.contains(&rec.id) {
                pinned.push(rec.id);
            }
            if shift_range(&mut rec.range, a, b, text.len()) {
                displaced.extend(before);
                kept.push(rec);
            } else if !rec.in_progress {
                cut.push(*rec);
            }
        }
        cut.reverse();
        let mut restored = Vec::new();
        let claim = claimer.as_mut().zip(claimed).map(|(rec, claim)| {
            rec.keep_cut(&mut cut);
            for id in trackers.drain(..) {
                if !rec.enclosing.contains(&id) {
                    rec.enclosing.push(id);
                }
            }
            rec.pinned_end = pinned.clone();
            if claim.done {
                rec.finish_step(&mut restored);
                match claim.step {
                    Step::Undo if origin == Origin::History => self.redoing = false,
                    Step::Redo => self.redoing = true,
                    _ => {}
                }
            }
            claim
        });
        // Every record the edit did not meet lies after it or before it.
        self.points.moved(signed(text.len()) - signed(b - a));
        let end = claimer.as_ref().map(|r| r.range.end);
        for rec in claimer.into_iter().chain(kept) {
            self.attach(rec);
        }
        if let Some(end) = end {
            for &id in &pinned {
                self.points.place(id, &(end..end));
            }
        }
        // Without a claimer, the edit's own record (if it gets one) keeps the collapsed records.
        cut.append(&mut displaced);
        cut.sort_by_key(|r| r.id);
        for rec in restored {
            self.restored.push(rec.id);
            self.attach(Box::new(rec));
        }
        (
            claim,
            Context {
                cut,
                enclosing: trackers,
            },
        )
    }

    /// Drops the records the last [`observe`](Self::observe) brought back for which
    /// `valid(range, content)` does not hold. Every other record was kept equal to the text edit
    /// by edit; these come back as they were when an edit cut them, which matches the text when
    /// the buffer's undo replays strictly in order.
    pub fn retain_restored(&mut self, valid: impl Fn(&Range<usize>, &str) -> bool) {
        for id in std::mem::take(&mut self.restored) {
            let fits = match (self.list.get(&id), self.points.get(id)) {
                (Some(r), Some(range)) => valid(&range, &r.content),
                _ => true,
            };
            if !fits {
                self.forget(id);
            }
        }
    }

    /// Every record's range and content, by start (for the unit tests' checks).
    #[cfg(test)]
    pub fn in_text_order(&self) -> Vec<(Range<usize>, &str)> {
        self.points
            .starts()
            .filter_map(|id| Some((self.points.get(id)?, self.list.get(&id)?.content.as_str())))
            .collect()
    }

    /// Forgets records overlapping `range` (its text was accepted into the baseline).
    pub fn forget_touching(&mut self, range: &Range<usize>) {
        for id in self.points.meeting(range.start, range.end) {
            if self.points.get(id).is_some_and(|r| overlaps(&r, range)) {
                self.forget(id);
            }
        }
    }

    pub fn clear(&mut self) {
        self.list.clear();
        self.points = Points::default();
        self.droppable.clear();
        self.undone.clear();
        self.restored.clear();
    }

    /// The edits re-applying the proposals of these reject records (Alt+U), descending. Records
    /// whose text was changed since, or whose reject was already undone, give none.
    pub fn reapply_edits(&self, ids: &[u64], current: &str) -> Vec<TextEdit> {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut edits: Vec<TextEdit> = ids
            .iter()
            .filter_map(|id| {
                let r = self.list.get(id)?;
                let range = self.points.get(*id)?;
                let reapply = matches!(r.kind, Kind::Reject(_))
                    && r.state == State::Done
                    && !r.in_progress
                    && r.content == r.after
                    && current.get(range.clone()) == Some(r.content.as_str());
                reapply.then(|| TextEdit {
                    range,
                    text: r.before.clone(),
                })
            })
            .collect();
        // Descending, a replacement before an insertion at its start (records of one decision
        // can meet: a turn's deletion point next to its insertion).
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start, e.range.end)));
        edits
    }
}

/// Where an end of a record's range is kept (see [`Points`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    /// At this position (before `Points::split`).
    Front(usize),
    /// At this position minus `Points::shift` (at or after `Points::split`).
    Back(isize),
}

/// One end of the range of the record `id`: `id << 1`, plus one for the range's end (so that a
/// start sorts before its end at the same position).
fn end_of(id: u64, is_end: bool) -> u64 {
    id << 1 | u64::from(is_end)
}

/// The ranges of the records in the list, by id, and their ends in text order.
///
/// An edit moves every range after it. So the ends before `split` are kept as positions, which an
/// edit after them leaves alone, and the ends at or after it as positions minus `shift`, the net
/// length change of the edits made before them, which an edit before them only adds to. Each edit
/// first moves `split` to its start, converting the ends in between, so that the ranges it does
/// not meet lie wholly before `split` or wholly after the edit: a run of edits going down or up
/// the text (an action, its undo and its redo) converts each end once, not every end after each
/// edit. The ranges holding `split` are found without a search (`across`).
#[derive(Clone, Debug, Default)]
struct Points {
    /// The ends before `split`: (position, [`end_of`]).
    front: BTreeSet<(usize, u64)>,
    /// The ends at or after `split`: (position minus `shift`, [`end_of`]).
    back: BTreeSet<(isize, u64)>,
    split: usize,
    shift: isize,
    /// Each record's start and end.
    ends: BTreeMap<u64, (Key, Key)>,
    /// The records whose start is in `front` and whose end is in `back`.
    across: BTreeSet<u64>,
}

impl Points {
    /// How the position `p` is kept now.
    fn key(&self, p: usize) -> Key {
        if p < self.split {
            Key::Front(p)
        } else {
            Key::Back(signed(p) - self.shift)
        }
    }

    /// The position `k` stands for.
    fn at(&self, k: Key) -> usize {
        match k {
            Key::Front(p) => p,
            Key::Back(q) => usize::try_from(q + self.shift).unwrap_or(0),
        }
    }

    fn put(&mut self, k: Key, end: u64) {
        match k {
            Key::Front(p) => self.front.insert((p, end)),
            Key::Back(q) => self.back.insert((q, end)),
        };
    }

    fn take(&mut self, k: Key, end: u64) {
        match k {
            Key::Front(p) => self.front.remove(&(p, end)),
            Key::Back(q) => self.back.remove(&(q, end)),
        };
    }

    /// Adds the record `id` at `r`.
    fn insert(&mut self, id: u64, r: &Range<usize>) {
        let (s, e) = (self.key(r.start), self.key(r.end));
        self.put(s, end_of(id, false));
        self.put(e, end_of(id, true));
        if matches!((s, e), (Key::Front(_), Key::Back(_))) {
            self.across.insert(id);
        }
        self.ends.insert(id, (s, e));
    }

    /// Removes the record `id`; returns where it was.
    fn remove(&mut self, id: u64) -> Option<Range<usize>> {
        let (s, e) = self.ends.remove(&id)?;
        self.take(s, end_of(id, false));
        self.take(e, end_of(id, true));
        self.across.remove(&id);
        Some(self.at(s)..self.at(e))
    }

    /// Moves the record `id`, if kept, to `r`.
    fn place(&mut self, id: u64, r: &Range<usize>) {
        if self.remove(id).is_some() {
            self.insert(id, r);
        }
    }

    /// Where the record `id` is.
    fn get(&self, id: u64) -> Option<Range<usize>> {
        let (s, e) = self.ends.get(&id)?;
        Some(self.at(*s)..self.at(*e))
    }

    /// Notes that the end `end` ([`end_of`]) is now kept as `k`.
    fn rekey(&mut self, end: u64, k: Key) {
        let (id, is_end) = (end >> 1, end & 1 == 1);
        if let Some(ends) = self.ends.get_mut(&id) {
            if is_end {
                ends.1 = k;
            } else {
                ends.0 = k;
            }
        }
        // The range holds `split` while only its start is in `front`.
        let (start, finish) = self.ends.get(&id).copied().unwrap_or((k, k));
        if matches!((start, finish), (Key::Front(_), Key::Back(_))) {
            self.across.insert(id);
        } else {
            self.across.remove(&id);
        }
    }

    /// Moves `split` to `x`, converting the ends in between.
    fn split_at(&mut self, x: usize) {
        while let Some(&(q, end)) = self.back.first() {
            let p = self.at(Key::Back(q));
            if p >= x {
                break;
            }
            self.back.pop_first();
            self.front.insert((p, end));
            self.rekey(end, Key::Front(p));
        }
        while let Some(&(p, end)) = self.front.last() {
            if p < x {
                break;
            }
            self.front.pop_last();
            let q = signed(p) - self.shift;
            self.back.insert((q, end));
            self.rekey(end, Key::Back(q));
        }
        self.split = x;
    }

    /// The records whose ranges meet the closed interval `[a, b]` (holding it, or with an end in
    /// it), ascending; `split` moves to `a`.
    fn meeting(&mut self, a: usize, b: usize) -> BTreeSet<u64> {
        self.split_at(a);
        let mut ids = self.across.clone();
        let hi = signed(b) - self.shift;
        ids.extend(self.back.range(..=(hi, u64::MAX)).map(|&(_, end)| end >> 1));
        ids
    }

    /// The text changed by `delta` bytes before every end at or after `split`, which the change
    /// does not meet.
    fn moved(&mut self, delta: isize) {
        self.shift += delta;
    }

    /// The records by the start of their ranges.
    #[cfg(test)]
    fn starts(&self) -> impl Iterator<Item = u64> + '_ {
        let front = self.front.iter().map(|&(_, end)| end);
        let back = self.back.iter().map(|&(_, end)| end);
        front
            .chain(back)
            .filter(|end| end & 1 == 0)
            .map(|end| end >> 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{Context, Kind, MAX_TURN_RECORDS, Points, Records, Reopen};
    use std::collections::BTreeSet;

    /// How many agent records are kept.
    fn agent_records(records: &Records) -> usize {
        records
            .list
            .values()
            .filter(|r| r.kind == Kind::Agent)
            .count()
    }

    /// Records the `i`-th of some agent insertions of turn `turn`, each in a line of its own.
    fn agent(records: &mut Records, i: usize, turn: u32) {
        let at = 10_000 + i * 4;
        records.push_agent(
            at..at + 1,
            String::new(),
            "a".into(),
            Context::default(),
            turn,
        );
    }

    /// The newest turn's agent records stay whatever the cap of 512, their last
    /// `MAX_TURN_RECORDS` of them; once another turn starts, they go as old records do. The
    /// newest decision's rejects stay through both.
    #[test]
    fn the_newest_turn_and_decision_are_kept() {
        let mut records = Records::default();
        let reopen = Reopen {
            pair: 0,
            occurrence: 0,
            user_touched: false,
        };
        records.begin_decision();
        let rejects: Vec<u64> = (0..600)
            .map(|i| records.push_reject(i * 4..i * 4 + 1, "x".into(), "y".into(), reopen))
            .collect();
        let turn = usize::try_from(MAX_TURN_RECORDS).unwrap();
        for i in 0..turn + 100 {
            agent(&mut records, i, 1);
        }
        assert!(rejects.iter().all(|&id| records.contains(id)));
        assert_eq!(agent_records(&records), turn, "the turn's last records");
        // Another turn: the cap drops the first one's records, 64 at a time.
        for i in 0..20 {
            agent(&mut records, turn + 100 + i, 2);
        }
        assert!(rejects.iter().all(|&id| records.contains(id)));
        assert_eq!(agent_records(&records), turn - 20 * 64 + 20);
    }

    /// Renumbering the turns (a review restored from an earlier session moves the session's
    /// turns out of the way of its own) keeps the newest turn's records: the turn's edits after
    /// it come with its new number, and they are still that turn's.
    #[test]
    fn renumbered_turns_keep_the_newest_turn() {
        let mut records = Records::default();
        for i in 0..600 {
            agent(&mut records, i, 3);
        }
        records.remap_turns(&mut |t| t + 10);
        for i in 600..620 {
            agent(&mut records, i, 13);
        }
        assert_eq!(agent_records(&records), 620);
        assert!(records.list.values().all(|r| r.turn == Some(13)));
    }

    /// The index finds what a scan of every range would, through edits going down, up and
    /// anywhere.
    #[test]
    fn points_meet_what_a_scan_finds() {
        let mut points = Points::default();
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut seed = 7u64;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            usize::try_from(seed >> 33).unwrap() % n.max(1)
        };
        let mut len = 1000;
        for id in 0..200u64 {
            let s = next(len);
            let e = s + next(30).min(len - s);
            points.insert(id, &(s..e));
            ranges.push(s..e);
        }
        for round in 0..400 {
            let a = match round % 3 {
                0 => len.saturating_sub(round),
                1 => round.min(len),
                _ => next(len),
            };
            let b = (a + next(20)).min(len);
            let expected: BTreeSet<u64> = (0..ranges.len())
                .filter(|&i| ranges[i].start <= b && a <= ranges[i].end)
                .map(|i| i as u64)
                .collect();
            assert_eq!(points.meeting(a, b), expected, "round {round}");
            // The edit `[a, b) → n bytes` moves the ranges after it; those it meets are put
            // back where they were (the caller decides that).
            let n = next(25);
            for &id in &expected {
                points.remove(id);
            }
            points.moved(n as isize - (b - a) as isize);
            for r in &mut ranges {
                if r.start > b {
                    *r = r.start + n - (b - a)..r.end + n - (b - a);
                }
            }
            len = len + n - (b - a);
            for &id in &expected {
                let r = &mut ranges[id as usize];
                *r = r.start.min(len)..r.end.min(len);
                points.insert(id, r);
            }
            for (id, r) in ranges.iter().enumerate() {
                assert_eq!(points.get(id as u64), Some(r.clone()), "round {round}");
            }
        }
    }
}
