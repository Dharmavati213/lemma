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
//! replace_all and the edits around it) are all kept (up to [`MAX_TURN_RECORDS`] of the turn's,
//! taking at most [`MAX_RECORD_BYTES`]), so that the whole decision or turn is recognized when it
//! is undone and redone; older ones are dropped beyond a cap of records and of bytes. An edit
//! only meets the records next to it, so the ranges are kept in a [`Points`] index: an edit costs
//! the records it meets and a step for each chunk of up to [`CHUNK_MAX`] others between it and
//! the edit before, not every record.
//!
//! Only a buffer's undo and redo take up an agent edit's record, and they reach only the edits
//! made in that buffer: a closed document's text goes into a new buffer as not undoable. So while
//! the text is in no buffer ([`Records::set_closed`]) agent edits are not recorded, and the
//! records only undo and redo use are dropped whenever the text goes into a buffer or out of it.
//! A reject's record stays: Alt+U takes it up again with an `Origin::Review` edit, buffer or not.
//!
//! Recognition is best effort: an edit it misses is classified by the Inside rule like any other
//! History edit.

use super::Reopen;
use crate::text::{overlaps, shift_range, signed};
use lemma_core::{Origin, TextEdit};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;

/// Top-level records kept at most (oldest dropped first). The rejects of the newest decision and
/// the agent edits of the newest turn are not dropped by it, however many there are (see
/// [`Records::begin_decision`], and [`Records::push_agent`] for the turn's own bounds).
const MAX_RECORDS: usize = 512;
/// Records dropped at once when the list is full, so that a long run of agent edits (a
/// replace_all) does not shift the whole list for every one of them.
const DROP_AT_ONCE: usize = MAX_RECORDS / 8;
/// Agent records of the newest turn kept at most whatever the cap (its last ones): a few hundred
/// bytes each, so that a replace_all of every space in a big file does not take hundreds of
/// megabytes. That is 16 384 matches in an open buffer (a deletion and an insertion each), fewer
/// when they are long: the turn's records also take at most about [`MAX_RECORD_BYTES`].
const MAX_TURN_RECORDS: u64 = 1 << 15;
/// What the top-level records take at most, about (see [`Record::cost`]): beyond, the oldest are
/// dropped as beyond [`MAX_RECORDS`], and the newest turn's agent records are kept whatever the
/// caps only while they take less. [`MAX_TURN_RECORDS`] small records fit (about 14 MB), 32 of
/// the largest an open buffer records ([`MAX_AGENT_RECORD_BYTES`]). The newest decision's rejects
/// are kept beyond it: they are as many as the hunks it decided, and hold their texts.
const MAX_RECORD_BYTES: usize = 16 << 20;
/// What a record takes besides its texts, about: the record itself, its entries in the list and
/// in [`Points`], and the allocations of its texts.
const RECORD_OVERHEAD: usize = 400;

/// Agent edits whose texts are larger than this (before plus after) are not recorded: a record
/// holds copies of them, and undoing such an edit is still classified by the Inside rule.
const MAX_AGENT_RECORD_BYTES: usize = 256 * 1024;

/// Ranges a chunk of [`Points`] holds at most: a fuller one is split in two. Smaller chunks cost
/// less to change and more to move as a whole: runs of edits were fastest with 64 to 128 (with
/// 256 those among other records took 60% longer).
const CHUNK_MAX: usize = 128;
/// Of two neighbouring chunks of [`Points`], one holds at least this many ranges, so that `n`
/// records take at most `2n / CHUNK_MIN + 1` chunks: a chunk that falls below it is merged with a
/// neighbour.
const CHUNK_MIN: usize = CHUNK_MAX / 4;

/// What a record with these texts (the range's now, before and after the edit) takes, about:
/// the texts and [`RECORD_OVERHEAD`]. The records it keeps are not counted.
fn cost(texts: [&str; 3]) -> usize {
    RECORD_OVERHEAD + texts.iter().map(|t| t.len()).sum::<usize>()
}

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
    /// What the record takes, about ([`cost`]). Only an edit it meets changes its text, while it
    /// is out of the list, so it is the same when it leaves the list as when it went in.
    fn cost(&self) -> usize {
        cost([&self.content, &self.before, &self.after])
    }

    /// Whether the record is of use without a buffer: a reject that Alt+U can take up again (or
    /// whose edit is about to be applied). Only the buffer's undo and redo take up the others.
    fn outlives_buffer(&self) -> bool {
        matches!(self.kind, Kind::Reject(_)) && self.state != State::Undone
    }

    /// Drops the records kept in this one that only the buffer's undo and redo would bring
    /// back: those an undo cut (its redo restores them), and those of no use without a buffer
    /// that this edit cut (its undo, which Alt+U is for a reject, restores them).
    fn keep_what_outlives_buffer(&mut self) {
        self.cut_backward.clear();
        self.cut_forward.retain(Record::outlives_buffer);
        for kept in &mut self.cut_forward {
            kept.keep_what_outlives_buffer();
        }
    }

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
    /// The text is in no buffer: only records of use without one are kept (see
    /// [`set_closed`](Self::set_closed)).
    closed: bool,
    /// What the records in `list` take ([`Record::cost`]).
    bytes: usize,
    /// What the newest turn's agent records in `list` (from `turn_from` on) take of it.
    turn_bytes: usize,
}

impl Records {
    /// The text went out of every buffer (`closed`), or into a new one, whose undo history starts
    /// empty: no buffer undo or redo can reach the edits recorded so far, so the records only
    /// they take up go (those of agent edits, and of rejects undone since). While the text is in
    /// no buffer, agent edits are not recorded, and a reject's record goes once Alt+U undid it.
    /// The other rejects' records stay, for Alt+U.
    pub fn set_closed(&mut self, closed: bool) {
        if closed == self.closed {
            return;
        }
        self.closed = closed;
        let gone: Vec<u64> = self
            .list
            .iter()
            .filter(|(_, r)| !r.outlives_buffer())
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.forget(id);
        }
        for r in self.list.values_mut() {
            r.keep_what_outlives_buffer();
        }
        self.redoing = false;
        self.restored.clear();
        // The next agent edit starts the newest turn's records again.
        self.turn = None;
    }

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
        self.turn_bytes = 0;
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
        if self.closed {
            // No buffer can undo it (see `set_closed`).
            return;
        }
        if self.turn != Some(turn) {
            self.begin_turn(turn);
        }
        // A huge turn keeps its last records only: at most MAX_TURN_RECORDS of them, taking at
        // most MAX_RECORD_BYTES.
        let cost = cost([&after, &before, &after]);
        while self.turn_from < self.next_id
            && (self.next_id - self.turn_from >= MAX_TURN_RECORDS
                || self.turn_bytes + cost > MAX_RECORD_BYTES)
        {
            if let Some(r) = self.list.get(&self.turn_from)
                && r.kind == Kind::Agent
            {
                self.turn_bytes -= r.cost();
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
        // And beyond the bytes, as many as it takes.
        let cost = cost([&content, &before, &after]);
        while self.bytes + cost > MAX_RECORD_BYTES {
            let Some(id) = self.droppable.pop_first() else {
                break;
            };
            self.forget(id);
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

    /// Puts `record` into the list at its `range`. Without a buffer, a record of no use without
    /// one (a reject Alt+U just undid) is dropped instead.
    fn attach(&mut self, record: Box<Record>) {
        if self.closed && !record.outlives_buffer() {
            return;
        }
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
        let cost = record.cost();
        self.bytes += cost;
        if record.kind == Kind::Agent && kept {
            self.turn_bytes += cost;
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
        let cost = record.cost();
        self.bytes -= cost;
        if record.kind == Kind::Agent && id >= self.turn_from {
            self.turn_bytes -= cost;
        }
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
        self.points.moved(b, signed(text.len()) - signed(b - a));
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
            .in_order()
            .filter_map(|(id, range)| Some((range, self.list.get(&id)?.content.as_str())))
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
        self.bytes = 0;
        self.turn_bytes = 0;
    }

    /// How many records are kept and what they take, about ([`Record::cost`]).
    pub fn held(&self) -> (usize, usize) {
        (self.list.len(), self.bytes)
    }

    /// Panics when what the records are said to take is not what they take.
    #[cfg(test)]
    pub fn check_costs(&self) {
        let bytes: usize = self.list.values().map(|r| r.cost()).sum();
        let turn: usize = self
            .list
            .range(self.turn_from..)
            .filter(|(_, r)| r.kind == Kind::Agent)
            .map(|(_, r)| r.cost())
            .sum();
        assert_eq!((self.bytes, self.turn_bytes), (bytes, turn), "record bytes");
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

/// Hashes record ids, which are mostly consecutive: a multiplication, folded so that every bit of
/// the id counts in the low bits, which pick the bucket. Runs of edits take about 30% less time
/// than with the standard hasher.
#[derive(Clone, Copy, Debug, Default)]
struct IdHash(u64);

impl std::hash::Hasher for IdHash {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(b));
        }
    }

    fn write_u64(&mut self, n: u64) {
        let x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = x ^ (x >> 32);
    }
}

/// A map from record ids.
type IdMap<V> = std::collections::HashMap<u64, V, std::hash::BuildHasherDefault<IdHash>>;

/// A record's range in a [`Chunk`].
#[derive(Clone, Copy, Debug)]
struct Span {
    /// Its start minus the chunk's [`base`](Chunk::base).
    start: isize,
    id: u64,
    len: usize,
}

/// Some of the ranges of [`Points`], next to each other in text order.
#[derive(Clone, Debug, Default)]
struct Chunk {
    /// Added to every start in `spans` (see [`base`](Self::base)).
    shift: isize,
    /// The chunk is one of the last ones, which `Points::tail` moves too.
    in_tail: bool,
    /// By start, then id.
    spans: VecDeque<Span>,
    /// The furthest end of `spans`, minus the base.
    reach: isize,
}

impl Chunk {
    /// What is added to every start in `spans`, with `tail` the [`Points::tail`].
    fn base(&self, tail: isize) -> isize {
        if self.in_tail {
            self.shift + tail
        } else {
            self.shift
        }
    }

    /// Where `span` is.
    fn range(&self, span: &Span, tail: isize) -> Range<usize> {
        let start = usize::try_from(span.start + self.base(tail)).unwrap_or(0);
        start..start + span.len
    }

    /// The start of the first range.
    fn first(&self, tail: isize) -> isize {
        self.spans
            .front()
            .map_or(isize::MAX, |s| s.start + self.base(tail))
    }

    /// The start and id of the last range, which the chunks are sorted by.
    fn last(&self, tail: isize) -> (isize, u64) {
        self.spans
            .back()
            .map_or((isize::MIN, 0), |s| (s.start + self.base(tail), s.id))
    }

    /// Notes again how far the ranges reach.
    fn measure(&mut self) {
        self.reach = self
            .spans
            .iter()
            .map(|s| s.start + signed(s.len))
            .max()
            .unwrap_or(isize::MIN);
    }
}

/// The ranges of the records in the list, in text order.
///
/// An edit moves every range after it, and a range it does not meet lies wholly before it or
/// wholly after it: ranges only move as a whole, so each is kept as its start and its length. The
/// ranges are sorted by start into chunks of at most [`CHUNK_MAX`], each with a shift added to
/// every start in it. An edit moves the ranges after it in its own chunk one by one and the
/// chunks after that as a whole: those form the tail, which has one more shift (`tail`), so the
/// edit only moves the tail's start to the chunk after its own, across the chunks between it and
/// the edit before. The ranges an edit meets start in the chunks between its start minus the
/// longest range and its end, and each chunk knows how far its ranges reach; searches start from
/// where the last one ended. So an edit costs O(`CHUNK_MAX`) plus a step for each chunk between
/// it and the edit before, and for each chunk the longest range could reach it from, however
/// many ranges they hold: a run of edits going down or up the text (an action, its undo and its
/// redo) costs O(`CHUNK_MAX`) an edit while the ranges are short, and a jump O(chunks) more. The
/// records of a replace_all in an open buffer (a deletion and an insertion a match) fill their
/// chunks about half, so the newest turn's 32 768 take about 500. Finding a record by id costs
/// O(`CHUNK_MAX`).
#[derive(Clone, Debug, Default)]
struct Points {
    /// The chunks in text order (indices into `chunks`), none of them empty.
    order: Vec<usize>,
    /// Every chunk, those not in `order` (empty) listed in `free`.
    chunks: Vec<Chunk>,
    free: Vec<usize>,
    /// The chunk of each record's range.
    chunk_of: IdMap<usize>,
    /// Added to the starts of the chunks from `order[tail_from]` on (those `in_tail`) besides
    /// their own shift: the net length change of the edits before them since they joined it.
    tail: isize,
    tail_from: usize,
    /// How many ranges have each length: none is longer than the last.
    lengths: BTreeMap<usize, usize>,
    /// Where in `order` the last search ended: the next one starts there.
    finger: usize,
}

impl Points {
    /// Adds the record `id` at `r`.
    fn insert(&mut self, id: u64, r: &Range<usize>) {
        let key = (signed(r.start), id);
        *self.lengths.entry(r.len()).or_default() += 1;
        let (i, k) = self.slot(key);
        let c = self.order[i];
        let chunk = &mut self.chunks[c];
        let base = chunk.base(self.tail);
        let span = Span {
            start: key.0 - base,
            id,
            len: r.len(),
        };
        chunk.spans.insert(k, span);
        let reach = span.start + signed(span.len);
        if chunk.spans.len() == 1 || reach > chunk.reach {
            chunk.reach = reach;
        }
        self.chunk_of.insert(id, c);
        if self.chunks[c].spans.len() > CHUNK_MAX {
            self.split(i);
        }
    }

    /// Where the range with this start and id goes: its chunk's index in `order` and its place
    /// in the chunk. A full chunk does not take a range at either end, so that a run of edits
    /// adds chunks instead of splitting them: the chunk before takes it, or a new one.
    fn slot(&mut self, key: (isize, u64)) -> (usize, usize) {
        let tail = self.tail;
        if self.order.is_empty() {
            let c = self.new_chunk(Chunk::default());
            self.order_insert(0, c);
            return (0, 0);
        }
        // The first chunk whose last range comes after it, or the last chunk.
        let i = self
            .find(|chunk| chunk.last(tail) >= key)
            .min(self.order.len() - 1);
        let chunk = &self.chunks[self.order[i]];
        let base = chunk.base(tail);
        let k = chunk
            .spans
            .partition_point(|s| (s.start + base, s.id) < key);
        let n = chunk.spans.len();
        if n < CHUNK_MAX || (0 < k && k < n) {
            return (i, k);
        }
        let (shift, in_tail) = (chunk.shift, chunk.in_tail);
        if k == 0
            && let Some(&before) = i.checked_sub(1).and_then(|j| self.order.get(j))
            && self.chunks[before].spans.len() < CHUNK_MAX
        {
            return (i - 1, self.chunks[before].spans.len());
        }
        let at = if k == 0 { i } else { i + 1 };
        let c = self.new_chunk(Chunk {
            shift,
            in_tail,
            ..Chunk::default()
        });
        self.order_insert(at, c);
        (at, 0)
    }

    /// Removes the record `id`; returns where it was.
    fn remove(&mut self, id: u64) -> Option<Range<usize>> {
        let c = self.chunk_of.remove(&id)?;
        let chunk = &mut self.chunks[c];
        let k = chunk.spans.iter().position(|s| s.id == id)?;
        let span = chunk.spans.remove(k)?;
        if span.start + signed(span.len) >= chunk.reach {
            chunk.measure();
        }
        let range = chunk.range(&span, self.tail);
        let few = chunk.spans.len() < CHUNK_MIN;
        if let Some(n) = self.lengths.get_mut(&span.len) {
            *n -= 1;
            if *n == 0 {
                self.lengths.remove(&span.len);
            }
        }
        if few {
            self.rebalance(c);
        }
        // A table does not shrink by itself: closing a document drops most records, if not all.
        if self.chunk_of.is_empty() {
            *self = Self::default();
        } else if self.chunk_of.len() < self.chunk_of.capacity() / 4 {
            self.chunk_of.shrink_to_fit();
        }
        Some(range)
    }

    /// Moves the record `id`, if kept, to `r`.
    fn place(&mut self, id: u64, r: &Range<usize>) {
        if self.remove(id).is_some() {
            self.insert(id, r);
        }
    }

    /// Where the record `id` is.
    fn get(&self, id: u64) -> Option<Range<usize>> {
        let chunk = &self.chunks[*self.chunk_of.get(&id)?];
        let span = chunk.spans.iter().find(|s| s.id == id)?;
        Some(chunk.range(span, self.tail))
    }

    /// The records whose ranges meet the closed interval `[a, b]` (holding it, or with an end in
    /// it), ascending.
    fn meeting(&mut self, a: usize, b: usize) -> BTreeSet<u64> {
        let (a, b, tail) = (signed(a), signed(b), self.tail);
        let longest = self.lengths.last_key_value().map_or(0, |(&n, _)| signed(n));
        // No range in the chunks before this one reaches `a`.
        let from = self.find(|chunk| chunk.last(tail).0 >= a - longest);
        let mut ids = BTreeSet::new();
        for &c in &self.order[from..] {
            let chunk = &self.chunks[c];
            if chunk.first(tail) > b {
                break;
            }
            let base = chunk.base(tail);
            if chunk.reach + base < a {
                continue;
            }
            let (a, b) = (a - base, b - base);
            for s in chunk.spans.iter().take_while(|s| s.start <= b) {
                if s.start + signed(s.len) >= a {
                    ids.insert(s.id);
                }
            }
        }
        ids
    }

    /// The text changed by `delta` bytes before every range that starts after `after`, which the
    /// change does not meet (the records it meets are taken out first).
    fn moved(&mut self, after: usize, delta: isize) {
        if delta == 0 {
            return;
        }
        let (after, tail) = (signed(after), self.tail);
        // The first chunk with a range starting after `after`.
        let i = self.find(|chunk| chunk.last(tail).0 > after);
        let Some(&c) = self.order.get(i) else {
            return;
        };
        let chunk = &mut self.chunks[c];
        let base = chunk.base(tail);
        let k = chunk.spans.partition_point(|s| s.start + base <= after);
        let whole = if k == 0 {
            i
        } else {
            for s in chunk.spans.range_mut(k..) {
                s.start += delta;
            }
            chunk.measure();
            i + 1
        };
        self.retail(whole);
        self.tail += delta;
    }

    /// Makes the chunks from `order[j]` on the tail.
    fn retail(&mut self, j: usize) {
        let (tail, p) = (self.tail, self.tail_from);
        for &c in self.order.get(j..p).unwrap_or_default() {
            let chunk = &mut self.chunks[c];
            chunk.shift -= tail;
            chunk.in_tail = true;
        }
        for &c in self.order.get(p..j).unwrap_or_default() {
            let chunk = &mut self.chunks[c];
            chunk.shift += tail;
            chunk.in_tail = false;
        }
        self.tail_from = j;
        if j == self.order.len() {
            self.tail = 0;
        }
    }

    /// The first index in `order` from which `past` holds (it holds for every chunk after one it
    /// holds for), looked for from where the last search ended: next to it in O(1), anywhere in
    /// O(log chunks).
    fn find(&mut self, past: impl Fn(&Chunk) -> bool) -> usize {
        let n = self.order.len();
        let holds = |i: usize| past(&self.chunks[self.order[i]]);
        let at = self.finger.min(n);
        // Bracket it, going away from the finger in growing steps.
        let (mut lo, mut hi) = (0, n);
        if at < n && !holds(at) {
            lo = at + 1;
            let mut step = 1;
            while at + step < n {
                if holds(at + step) {
                    hi = at + step;
                    break;
                }
                lo = at + step + 1;
                step *= 2;
            }
        } else {
            hi = at;
            let mut step = 1;
            while step <= at {
                if !holds(at - step) {
                    lo = at - step + 1;
                    break;
                }
                hi = at - step;
                step *= 2;
            }
        }
        let found = lo + self.order[lo..hi].partition_point(|&c| !past(&self.chunks[c]));
        self.finger = found;
        found
    }

    /// Puts `chunk` among the chunks; returns its index.
    fn new_chunk(&mut self, chunk: Chunk) -> usize {
        if let Some(c) = self.free.pop() {
            self.chunks[c] = chunk;
            c
        } else {
            self.chunks.push(chunk);
            self.chunks.len() - 1
        }
    }

    /// Puts the chunk `c` at `order[i]`: at or before the tail's first chunk unless it is in
    /// the tail.
    fn order_insert(&mut self, i: usize, c: usize) {
        self.order.insert(i, c);
        if !self.chunks[c].in_tail {
            self.tail_from += 1;
        }
    }

    /// Takes the chunk `order[i]` out of the order, and frees it.
    fn order_remove(&mut self, i: usize) {
        let c = self.order.remove(i);
        if i < self.tail_from {
            self.tail_from -= 1;
        }
        self.chunks[c] = Chunk::default();
        self.free.push(c);
    }

    /// Splits the chunk `order[i]` into two halves.
    fn split(&mut self, i: usize) {
        let chunk = &mut self.chunks[self.order[i]];
        let mut half = Chunk {
            shift: chunk.shift,
            in_tail: chunk.in_tail,
            spans: chunk.spans.split_off(chunk.spans.len() / 2),
            reach: 0,
        };
        chunk.measure();
        half.measure();
        let h = self.new_chunk(half);
        for s in &self.chunks[h].spans {
            self.chunk_of.insert(s.id, h);
        }
        self.order_insert(i + 1, h);
    }

    /// The chunk `c` holds fewer than [`CHUNK_MIN`] ranges: it goes when empty, and is merged
    /// with a neighbour otherwise.
    fn rebalance(&mut self, c: usize) {
        let Some(i) = self.order.iter().position(|&x| x == c) else {
            return;
        };
        if self.chunks[c].spans.is_empty() {
            self.order_remove(i);
        } else if self.order.len() > 1 {
            self.merge(i.min(self.order.len() - 2));
        }
    }

    /// Merges the chunks `order[i]` and `order[i + 1]`: the ranges of the smaller go into the
    /// other, which is split again when that makes it too full.
    fn merge(&mut self, i: usize) {
        let (left, right) = (self.order[i], self.order[i + 1]);
        let into_left = self.chunks[right].spans.len() <= self.chunks[left].spans.len();
        let (from, into) = if into_left {
            (right, left)
        } else {
            (left, right)
        };
        let mut moving = std::mem::take(&mut self.chunks[from].spans);
        let rebase = self.chunks[from].base(self.tail) - self.chunks[into].base(self.tail);
        for s in &mut moving {
            s.start += rebase;
            self.chunk_of.insert(s.id, into);
        }
        let chunk = &mut self.chunks[into];
        if into_left {
            chunk.spans.append(&mut moving);
        } else {
            moving.append(&mut chunk.spans);
            chunk.spans = moving;
        }
        chunk.measure();
        let full = chunk.spans.len() > CHUNK_MAX;
        self.order_remove(if into_left { i + 1 } else { i });
        if full {
            self.split(i);
        }
    }

    /// The records and their ranges, by start.
    #[cfg(test)]
    fn in_order(&self) -> impl Iterator<Item = (u64, Range<usize>)> + '_ {
        self.order.iter().flat_map(|&c| {
            let chunk = &self.chunks[c];
            chunk
                .spans
                .iter()
                .map(|s| (s.id, chunk.range(s, self.tail)))
        })
    }

    /// Panics when the chunks are not what they should be: sorted, each holding one to
    /// [`CHUNK_MAX`] ranges and of two neighbours one at least [`CHUNK_MIN`], knowing how far they
    /// reach, found by id, in the tail from `tail_from` on; or when the lengths are not counted
    /// right, or the table of ids is mostly empty.
    #[cfg(test)]
    fn check(&self) {
        let mut previous = None;
        let mut lengths = BTreeMap::new();
        for (i, &c) in self.order.iter().enumerate() {
            let chunk = &self.chunks[c];
            let n = chunk.spans.len();
            assert!((1..=CHUNK_MAX).contains(&n), "chunk {c} holds {n} ranges");
            if let Some(&next) = self.order.get(i + 1) {
                let both = n.max(self.chunks[next].spans.len());
                assert!(
                    both >= CHUNK_MIN,
                    "chunks {c} and {next} hold {both} at most"
                );
            }
            assert_eq!(chunk.in_tail, i >= self.tail_from, "chunk {c} at {i}");
            let mut measured = chunk.clone();
            measured.measure();
            assert_eq!(chunk.reach, measured.reach, "chunk {c}");
            for s in &chunk.spans {
                let key = (s.start + chunk.base(self.tail), s.id);
                assert!(previous < Some(key), "{key:?} after {previous:?}");
                previous = Some(key);
                assert_eq!(self.chunk_of.get(&s.id), Some(&c), "record {}", s.id);
                *lengths.entry(s.len).or_default() += 1;
            }
        }
        assert!(self.tail_from <= self.order.len());
        assert_eq!(lengths, self.lengths);
        assert_eq!(self.chunk_of.len(), lengths.values().sum::<usize>());
        assert!(
            self.chunk_of.capacity() / 4 <= self.chunk_of.len(),
            "the table shrinks"
        );
        assert!(self.free.iter().all(|&c| self.chunks[c].spans.is_empty()));
        assert_eq!(self.order.len() + self.free.len(), self.chunks.len());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CHUNK_MAX, Context, Kind, MAX_RECORD_BYTES, MAX_TURN_RECORDS, Points, Records, Reopen,
        Step, cost,
    };
    use lemma_core::Origin;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ops::Range;

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

    /// The records take about `MAX_RECORD_BYTES` at most: a turn of large agent edits keeps its
    /// last ones that fit, and older records go to make room, though fewer than `MAX_RECORDS`.
    #[test]
    fn records_take_at_most_the_bytes() {
        let mut records = Records::default();
        for i in 0..100 {
            agent(&mut records, i, 1);
        }
        let large = "a".repeat(128 << 10);
        let first = records.next_id;
        for i in 0..200 {
            let at = 1_000_000 + i * (large.len() + 1);
            records.push_agent(
                at..at + large.len(),
                String::new(),
                large.clone(),
                Context::default(),
                2,
            );
            records.check_costs();
        }
        let fit = MAX_RECORD_BYTES / cost([&large, "", &large]);
        let (kept, bytes) = records.held();
        assert!(bytes <= MAX_RECORD_BYTES, "{bytes} bytes");
        assert_eq!((kept, agent_records(&records)), (fit, fit));
        assert!((first + 200 - fit as u64..first + 200).all(|id| records.contains(id)));
    }

    /// Without a buffer, only the rejects' records stay: an agent edit is not recorded, the
    /// records of earlier ones go (those a reject cut too), and a reject's goes once Alt+U
    /// undid it. In a buffer again, agent edits are recorded again.
    #[test]
    fn without_a_buffer_only_rejects_are_kept() {
        let mut records = Records::default();
        let reopen = Reopen {
            pair: 0,
            occurrence: 0,
            user_touched: false,
        };
        // An agent insertion inside a proposal that a reject takes back, which cuts its record.
        records.push_agent(10..11, String::new(), "a".into(), Context::default(), 1);
        records.begin_decision();
        let (proposal, restored) = ("0123456789abcde", "xy");
        let reject = records.push_reject(5..20, proposal.into(), restored.into(), reopen);
        let (claim, _) = records.observe(5, 20, restored, Origin::Review, true);
        assert!(claim.is_some_and(|c| c.done && c.step == Step::Apply));
        assert_eq!(records.list[&reject].cut_forward.len(), 1);
        records.push_agent(30..31, String::new(), "b".into(), Context::default(), 1);
        assert_eq!(records.held().0, 2);

        records.set_closed(true);
        records.check_costs();
        assert_eq!(records.list.keys().copied().collect::<Vec<_>>(), [reject]);
        assert!(records.list[&reject].cut_forward.is_empty());
        records.push_agent(40..41, String::new(), "c".into(), Context::default(), 1);
        assert_eq!(records.held().0, 1, "agent edits are not recorded");
        // Alt+U re-applies the proposal and undoes the reject: nothing can redo it now.
        let current = format!("{}{restored}{}", "_".repeat(5), "_".repeat(30));
        let edits = records.reapply_edits(&[reject], &current);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        let (claim, _) = records.observe(e.range.start, e.range.end, &e.text, Origin::Review, true);
        assert!(claim.is_some_and(|c| c.done && c.step == Step::Undo));
        records.check_costs();
        assert_eq!(records.held(), (0, 0));

        records.set_closed(false);
        records.push_agent(50..51, String::new(), "d".into(), Context::default(), 1);
        assert_eq!(agent_records(&records), 1);
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

    /// A pseudo-random number below `n` (0 when `n` is 0).
    fn random(seed: &mut u64, n: usize) -> usize {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        usize::try_from(*seed >> 33).unwrap() % n.max(1)
    }

    /// The index finds what a scan of every range would, through edits going down, up and
    /// anywhere, while records go and come (now and then most or all of them at once, and come
    /// back as a run), in one chunk and in many, short and now and then long.
    #[test]
    fn points_meet_what_a_scan_finds() {
        let mut seed = 7u64;
        for (count, rounds) in [(200, 400), (3000, 600)] {
            let mut points = Points::default();
            let mut ranges: BTreeMap<u64, Range<usize>> = BTreeMap::new();
            let mut len = count * 5;
            let mut next_id = 0u64;
            for round in 0..rounds {
                let keep = match round % 100 {
                    25 | 75 => count / 10,
                    50 => 0,
                    _ => ranges.len() - random(&mut seed, 2).min(ranges.len()),
                };
                while ranges.len() > keep {
                    let i = random(&mut seed, ranges.len());
                    let id = *ranges.keys().nth(i).unwrap();
                    assert_eq!(points.remove(id), ranges.remove(&id), "round {round}");
                }
                // After those, the records come back as a run of edits makes them, going down the
                // text or up, among the others or alone; one by one anywhere otherwise.
                let run = matches!(round % 100, 25 | 50 | 75);
                let down = round % 100 == 25 || (round % 100 == 50 && round / 100 % 2 == 0);
                let mut at = if down { len } else { 0 };
                while ranges.len() < count {
                    let s = if !run {
                        random(&mut seed, len)
                    } else if down {
                        at.saturating_sub(1 + random(&mut seed, 3))
                    } else {
                        (at + 1 + random(&mut seed, 3)).min(len)
                    };
                    at = s;
                    // Now and then a long one, which edits far past its start meet.
                    let longest = if random(&mut seed, 100) == 0 {
                        len / 8
                    } else {
                        30
                    };
                    let r = s..s + random(&mut seed, longest).min(len - s);
                    points.insert(next_id, &r);
                    ranges.insert(next_id, r);
                    next_id += 1;
                }
                let a = match round % 3 {
                    0 => len.saturating_sub(round * 7),
                    1 => (round * 7).min(len),
                    _ => random(&mut seed, len),
                };
                let b = (a + random(&mut seed, 20)).min(len);
                let expected: BTreeSet<u64> = ranges
                    .iter()
                    .filter(|(_, r)| r.start <= b && a <= r.end)
                    .map(|(&id, _)| id)
                    .collect();
                assert_eq!(points.meeting(a, b), expected, "round {round}");
                // The edit `[a, b) → n bytes` moves the ranges after it; those it meets are put
                // back where they were (the caller decides that).
                let n = random(&mut seed, 25);
                for &id in &expected {
                    points.remove(id);
                }
                points.moved(b, n as isize - (b - a) as isize);
                for r in ranges.values_mut() {
                    if r.start > b {
                        *r = r.start + n - (b - a)..r.end + n - (b - a);
                    }
                }
                len = len + n - (b - a);
                for id in &expected {
                    let r = ranges.get_mut(id).unwrap();
                    *r = r.start.min(len)..r.end.min(len);
                    points.insert(*id, r);
                }
                points.check();
                for (&id, r) in &ranges {
                    assert_eq!(points.get(id), Some(r.clone()), "round {round}");
                }
                let mut by_start: Vec<(u64, Range<usize>)> =
                    ranges.iter().map(|(&id, r)| (id, r.clone())).collect();
                by_start.sort_by_key(|(id, r)| (r.start, *id));
                assert!(points.in_order().eq(by_start), "round {round}");
            }
            assert!(points.order.len() > count / CHUNK_MAX, "{count} records");
            // Without records, it gives its memory back.
            for id in ranges.into_keys() {
                points.remove(id);
            }
            assert!(points.chunks.is_empty() && points.chunk_of.capacity() == 0);
        }
    }
}
