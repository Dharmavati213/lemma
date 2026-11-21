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
//! Alt+U is the exception: it re-applies a reject after later actions, wherever the reject's text
//! is by then. The records the reject had cut come back moved as far as its range has moved, and
//! those that no longer fit the text stay with it for its next step: the buffer's undo of the
//! reject itself, once Alt+U's action and the actions after the reject are undone, finds the text
//! they were cut from. Alt+U's action is then the newest of the reject's in the buffer's history,
//! which is why records are told apart by when their actions were done and undone, not by age.
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
use crate::text::{shift_bytes, shift_range, signed};
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

/// Empty records at one place whose order a deletion there keeps at most (see
/// [`Records::observe`]).
const MAX_ORDERED: usize = 64;
/// Records a record keeps pinned to its end that way at most.
const MAX_PINNED: usize = 4 * MAX_ORDERED;

/// Makes the records `ids` follow `r`'s end (see [`Record::pinned_end`]), unless it has many to
/// follow it already.
fn pin(r: &mut Record, ids: &[u64]) {
    for &id in ids {
        if id != r.id && r.pinned_end.len() < MAX_PINNED && !r.pinned_end.contains(&id) {
            r.pinned_end.push(id);
        }
    }
}

/// Pieces of a text: byte ranges, sorted, disjoint and not touching.
pub(super) type Pieces = Vec<Range<usize>>;

/// Adds the range `r` to `p`.
pub(super) fn add_piece(p: &mut Pieces, r: Range<usize>) {
    if r.is_empty() {
        return;
    }
    let first = p.partition_point(|x| x.end < r.start);
    let last = first + p[first..].iter().take_while(|x| x.start <= r.end).count();
    let merged = p[first..last]
        .iter()
        .fold(r, |m, x| m.start.min(x.start)..m.end.max(x.end));
    p.splice(first..last, [merged]);
}

/// The pieces of `p` within `r`, as ranges from `r.start`.
pub(super) fn pieces_in(p: &Pieces, r: &Range<usize>) -> Pieces {
    let first = p.partition_point(|x| x.end <= r.start);
    p[first..]
        .iter()
        .take_while(|x| x.start < r.end)
        .map(|x| x.start.max(r.start) - r.start..x.end.min(r.end) - r.start)
        .filter(|x| !x.is_empty())
        .collect()
}

/// `p` without the pieces of `q`.
pub(super) fn pieces_minus(p: &Pieces, q: &Pieces) -> Pieces {
    let mut out = Pieces::new();
    for x in p {
        let mut start = x.start;
        for y in q.iter().filter(|y| y.start < x.end && y.end > x.start) {
            if y.start > start {
                out.push(start..y.start);
            }
            start = start.max(y.end);
        }
        if start < x.end {
            out.push(start..x.end);
        }
    }
    out
}

/// Whether `p` covers all of `r` (an empty `r` too).
pub(super) fn covers(p: &Pieces, r: &Range<usize>) -> bool {
    r.is_empty() || p.iter().any(|x| x.start <= r.start && r.end <= x.end)
}

/// Replaces `[r)` of the text whose pieces `p` are by `len` bytes, none of them a piece.
fn splice_pieces(p: &mut Pieces, r: &Range<usize>, len: usize) {
    if p.is_empty() {
        return;
    }
    let mut out = Pieces::with_capacity(p.len() + 1);
    for x in p.iter() {
        if x.end <= r.start {
            out.push(x.clone());
        } else if x.start >= r.end {
            out.push(x.start - r.len() + len..x.end - r.len() + len);
        } else {
            if x.start < r.start {
                out.push(x.start..r.start);
            }
            if x.end > r.end {
                out.push(r.start + len..x.end - r.len() + len);
            }
        }
    }
    // Pieces on either side of a deletion meet.
    out.dedup_by(|y, x| {
        let meet = x.end >= y.start;
        if meet {
            x.end = x.end.max(y.end);
        }
        meet
    });
    *p = out;
}

/// Where the pieces of a record's texts stand against the baseline. The agent's edits change
/// the current text only (they are proposals), so a record's steps do too, until an accept takes
/// its change into the baseline: piece by piece, as far as the hunk it took reached and as far as
/// the record's texts were its own (not text of other edits of the agent's that the record's
/// text took in or replaced, which no accept took). Those pieces its steps take out of both texts
/// and put into both, as the user's own change; the others go into the current text only.
/// Its steps carry the pieces over: what one takes out of both texts, the next puts back into
/// both. Ranges are byte ranges of the texts.
#[derive(Clone, Debug, Default)]
struct Masks {
    /// Pieces of `content` the record's next step takes out of both texts (an accept took them).
    ours: Pieces,
    /// Pieces of `content` the baseline holds besides (text the record's change did not write:
    /// a reject's restored text, what a step put back of the baseline's).
    held: Pieces,
    /// Pieces of the text the next step puts in (`step_texts().1`) that it puts into both texts:
    /// the baseline lost them when an accept took the record's change.
    taken: Pieces,
    /// Pieces of that text the baseline holds: the record's change took them out of the current
    /// text only, and an accept of that change takes them out of the baseline (they become
    /// `taken`).
    kept: Pieces,
    /// While a step is in progress, `taken` and `kept` of the text it took out, for the step after
    /// it (once one edit took it all out, see [`Records::placed`]).
    next: Option<(Pieces, Pieces)>,
    /// `ours` and `held` as the last step left them (or an accept since, while the text was
    /// still the one it put in): an edit inside the text that puts back what another took out
    /// (the buffer's undo of one, then its redo) puts back how the record held it.
    at_step: (Pieces, Pieces),
    /// What deletions inside the text took out of it, with its pieces, the last one last: the
    /// insertion that puts the same text back at the same place (the buffer's undo of the
    /// deletion, or the redo of an insertion after its undo) puts back its pieces too.
    inside: Vec<Inside>,
}

/// Text a deletion took out of a record's (see [`Masks::inside`]): where, and its pieces.
#[derive(Clone, Debug)]
struct Inside {
    at: usize,
    /// The text's length and hash.
    len: usize,
    hash: u64,
    ours: Pieces,
    held: Pieces,
}

/// Deletions inside a record's text whose pieces it keeps at most (see [`Masks::inside`]).
const MAX_INSIDE: usize = 64;

/// Which pieces of a recorded step's edit go into both texts: of the text it deletes (from the
/// start of the edit) and of the text it inserts (see [`Masks`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Placement {
    pub deleted: Pieces,
    pub inserted: Pieces,
    /// Where each piece of `deleted` and `inserted` is (or goes) in the baseline, if known.
    pub deleted_at: Vec<Option<usize>>,
    pub inserted_at: Vec<Option<usize>>,
}

impl Placement {
    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty() && self.inserted.is_empty()
    }
}

/// What an accept changed of a record's [`Masks`], for its Alt+U ([`Records::unaccept`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Taken {
    id: u64,
    /// Pieces of its content it made the record's own in the baseline.
    ours: Pieces,
    /// Pieces of the text its next step puts in that it made `taken` (they were `kept`).
    taken: Pieces,
}

/// The claimer of the edit [`Records::observe`] last saw, for [`Records::placed`].
#[derive(Clone, Debug)]
struct Placing {
    id: u64,
    /// Where the edit is in the record's content.
    at: usize,
    /// The edit deleted all of the step's from-text, and inserted all of its to-text.
    deletes_from: bool,
    inserts_to: bool,
    /// The edit completed the step.
    done: bool,
    /// `kept` of the to-text, as the step started.
    kept: Pieces,
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

/// What an unclaimed edit did to the records, for the edit's own record (if it gets one), or for
/// the user's edit's entry (see `user`).
#[derive(Clone, Debug, Default)]
pub(super) struct Context {
    /// Records the edit cut through, or moved in a way its undo cannot reverse (as they were
    /// before the edit); the edit's record keeps them.
    cut: Vec<Record>,
    /// Records whose text took the edit in; the edit's record is nested in them.
    enclosing: Vec<u64>,
    /// Empty records where a deletion's text started, before it: the edit's record follows
    /// their ends.
    before: Vec<u64>,
}

impl Context {
    /// The records whose text took the edit in.
    pub fn enclosing(&self) -> &[u64] {
        &self.enclosing
    }
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
    /// While the record is kept in another's `cut_forward` or `cut_backward`: where that one's
    /// range started when this one was cut. It comes back moved as far as that range has moved
    /// since (see `finish_step`).
    kept_at: usize,
    /// Records whose text contained this edit when it was made (or applied, undone, redone): its
    /// undo and redo restore their text too, even at their boundaries.
    enclosing: Vec<u64>,
    /// Empty records that sat at the end of this record's range when one of its steps deleted
    /// that end, and those a deletion left empty where this record was empty already: they come
    /// after its text, so they follow its end through later steps while they still sit there.
    pinned_end: Vec<u64>,
    /// When `pinned_end` was last set ([`Records::clock`]).
    pinned_at: u64,
    /// Where Alt+U's action on this reject is in the buffer's history, if it is there.
    alt_u: AltU,
    /// When the record's own action (its edit) was last made, applied, undone or redone, and
    /// when Alt+U's last was done or undone ([`Records::clock`]): the buffer's undo and redo step
    /// the last action first (see [`Record::due`]).
    stepped: u64,
    alt_u_at: u64,
    /// Which pieces of its texts its steps take out of both texts or put into both (see
    /// [`Masks`]).
    masks: Masks,
}

/// Alt+U undoes a reject with a new action of the buffer's (`Origin::Review` edits), which the
/// buffer's undo and redo step like any other: the undo of that action is the record's redo, and
/// its redo the record's undo. So a reject's history in the buffer is its own action, then maybe
/// Alt+U's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AltU {
    /// Alt+U's action is not in the buffer's history.
    None,
    /// Done (the record is undone): the buffer's undo of it is the record's redo.
    Done,
    /// Undone, on the buffer's redo stack while no new action came after it: when
    /// [`Records::actions`] is still this. Above the reject's own action: with the record done,
    /// the buffer's redo of it, and the undo of the reject's own, both undo the record.
    Undone(u64),
}

impl Record {
    /// Whether the record's next step can be part of a buffer's redo (`redo`) or of an undo
    /// (see [`AltU`]).
    fn goes(&self, redo: bool) -> bool {
        self.goes_as(self.alt_u, redo)
    }

    /// [`goes`](Self::goes) with Alt+U's action where `alt_u` says.
    fn goes_as(&self, alt_u: AltU, redo: bool) -> bool {
        match (self.state, alt_u) {
            (State::Applying, _) => false,
            (State::Done, AltU::Undone(_)) => true,
            (State::Done, _) | (State::Undone, AltU::Done) => !redo,
            (State::Undone, _) => redo,
        }
    }

    /// When the action that the buffer's next undo (`redo` false) or redo of this record would
    /// step was done or undone; 0 when there is none that way. Alt+U's action, done, is above the
    /// reject's own on the undo stack, and the reject's own, undone after it, above it on the
    /// redo stack.
    fn due(&self, redo: bool) -> u64 {
        self.due_as(self.alt_u, redo)
    }

    /// [`due`](Self::due) with Alt+U's action where `alt_u` says.
    fn due_as(&self, alt_u: AltU, redo: bool) -> u64 {
        match (self.state, alt_u, redo) {
            (State::Undone, AltU::Done, false) | (State::Done, AltU::Undone(_), true) => {
                self.alt_u_at
            }
            (State::Done, _, false) | (State::Undone, AltU::None | AltU::Undone(_), true) => {
                self.stepped
            }
            _ => 0,
        }
    }

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

    /// Whether the record's text in the current text is the agent's, not accepted: an agent
    /// edit done, or a reject undone (the proposal is back).
    fn holds_agent_text(&self) -> bool {
        let n = self.content.len();
        !(n > 0 && covers(&self.masks.ours, &(0..n)))
            && matches!(
                (self.kind, self.state),
                (Kind::Agent, State::Done) | (Kind::Reject(_), State::Undone)
            )
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
        self.accepts_at(&self.range, origin, a, b, text)
    }

    /// [`accepts`](Self::accepts) with the record at `range`.
    fn accepts_at(
        &self,
        range: &Range<usize>,
        origin: Origin,
        a: usize,
        b: usize,
        text: &str,
    ) -> bool {
        let origin_fits = match (self.kind, self.state) {
            (_, State::Applying) => origin == Origin::Review,
            // Alt+U re-applies a rejected proposal with Origin::Review.
            (Kind::Reject(_), State::Done) => matches!(origin, Origin::History | Origin::Review),
            (Kind::Agent, State::Done) | (_, State::Undone) => origin == Origin::History,
        };
        origin_fits
            && range.start <= a
            && b <= range.end
            && (self.in_progress || self.starts_step(range, a, b, text, origin))
    }

    /// The edit lies in the range, boundaries included.
    fn within(&self, a: usize, b: usize) -> bool {
        self.range.start <= a && b <= self.range.end
    }

    /// A step starts from the expected text with a full replacement, a full deletion, or an
    /// insertion of the whole target at either end of the range (where the record is). Our own
    /// review edits (`Origin::Review`: a reject, Alt+U) delete each edit's range before they
    /// insert its text, so with them an insertion starts the step only of an empty range: at the
    /// end of a range that one of them is about to replace, it is another edit's (Reject Turn
    /// puts back text the user deleted right after the turn's next edit).
    fn starts_step(
        &self,
        range: &Range<usize>,
        a: usize,
        b: usize,
        text: &str,
        origin: Origin,
    ) -> bool {
        let (from, to) = self.step_texts();
        let (s, e) = (range.start, range.end);
        let inserts_at_an_end = (a == s || a == e) && (origin != Origin::Review || s == e);
        self.content == from
            && ((a == s && b == e && (text.is_empty() || text == to))
                || (a == b && inserts_at_an_end && text == to))
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
        let r = a - s..b - s;
        splice_pieces(&mut self.masks.ours, &r, text.len());
        splice_pieces(&mut self.masks.held, &r, text.len());
    }

    /// Applies an edit inside the record's text that is not its step's: the pieces it deletes
    /// are kept for the insertion that puts them back (see [`Masks::inside`]).
    fn apply_inside(&mut self, a: usize, b: usize, text: &str) {
        let at = a - self.range.start;
        let m = &mut self.masks;
        if a < b && text.is_empty() {
            let r = at..at + (b - a);
            let (ours, held) = (pieces_in(&m.ours, &r), pieces_in(&m.held, &r));
            if !ours.is_empty() || !held.is_empty() {
                if m.inside.len() >= MAX_INSIDE {
                    m.inside.remove(0);
                }
                m.inside.push(Inside {
                    at,
                    len: r.len(),
                    hash: lemma_core::fnv1a64(&[self.content[r].as_bytes()]),
                    ours,
                    held,
                });
            }
        }
        self.apply(a, b, text);
        let m = &mut self.masks;
        if a == b
            && m.inside.last().is_some_and(|x| {
                x.at == at
                    && x.len == text.len()
                    && x.hash == lemma_core::fnv1a64(&[text.as_bytes()])
            })
        {
            let back = m.inside.pop().expect("a deletion");
            for x in back.ours {
                add_piece(&mut m.ours, x.start + at..x.end + at);
            }
            for x in back.held {
                add_piece(&mut m.held, x.start + at..x.end + at);
            }
        }
        self.took_back();
    }

    /// After an edit inside the record's text: if that put back the text the last step left
    /// (the buffer's undo and redo of edits inside it), it is what it was then (see
    /// [`Masks::at_step`]).
    fn took_back(&mut self) {
        if self.content != self.step_texts().0 {
            return;
        }
        let m = &mut self.masks;
        for x in &m.at_step.0 {
            add_piece(&mut m.ours, x.clone());
        }
        for x in &m.at_step.1 {
            add_piece(&mut m.held, x.clone());
        }
        m.held = pieces_minus(&m.held, &m.ours);
    }

    /// Ends the current step: the records cut by the opposite direction come back, moved as far
    /// as the range has moved since each was cut (see [`Records::retain_restored`]), with where
    /// they were. The buffer's undo and redo step back to the very text the records were cut
    /// from, but Alt+U re-applies a reject wherever its text is now: after edits before it, the
    /// records it had cut came back where its text used to be. One that would move before the
    /// start of the text stays.
    fn finish_step(&mut self, restored: &mut Vec<(Record, Range<usize>)>) {
        let start = self.range.start;
        let (state, back) = match self.state {
            State::Applying => (State::Done, None),
            State::Done => (State::Undone, Some(&mut self.cut_forward)),
            State::Undone => (State::Done, Some(&mut self.cut_backward)),
        };
        // The text it took out is the next step's to put in (see `Records::placed`).
        let (taken, kept) = self.masks.next.take().unwrap_or_default();
        self.masks.taken = taken;
        self.masks.kept = kept;
        if let Some(back) = back {
            let mut stay = Vec::new();
            for mut r in back.drain(..) {
                let moved = signed(start) - signed(r.kept_at);
                if r.range.start.checked_add_signed(moved).is_none() {
                    stay.push(r);
                    continue;
                }
                let was = r.range.clone();
                shift_bytes(&mut r.range, moved);
                restored.push((r, was));
            }
            *back = stay;
        }
        self.state = state;
        self.in_progress = false;
    }

    /// Keeps records cut by the current step of this record. The step's edits lie within the
    /// range, so its start stays where it was while they are cut.
    fn keep_cut(&mut self, cut: &mut Vec<Record>) {
        for r in cut.iter_mut() {
            r.kept_at = self.range.start;
        }
        match self.state {
            State::Applying | State::Undone => self.cut_forward.append(cut),
            State::Done => self.cut_backward.append(cut),
        }
    }

    /// Keeps `r`, which the step just finished brought back, for the next step that brings back
    /// what it brought back, at `was`, where it was before.
    fn keep_back(&mut self, mut r: Record, was: Range<usize>) {
        r.range = was;
        match self.state {
            State::Applying => {}
            State::Undone => self.cut_forward.push(r),
            State::Done => self.cut_backward.push(r),
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
    /// Which way the buffer's current undo or redo goes (true: a redo), when its caller says so
    /// ([`begin_history_step`](Self::begin_history_step)); else `redoing` guesses it.
    stepping: Option<bool>,
    /// Changes other than the buffer's undo and redo so far: each starts or continues a new
    /// action, which clears the buffer's redo stack (see [`Record::alt_u`]).
    actions: u64,
    /// Counts the records' completed steps (see [`Record::stepped`]).
    clock: u64,
    /// The rejects whose proposals Alt+U is re-applying ([`reapply_edits`](Self::reapply_edits)),
    /// in the order of its edits: its edits are theirs, until a change of another origin.
    reapplying: VecDeque<u64>,
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
    /// Records the last completed step (or [`bring_back`](Self::bring_back)) brought back, with
    /// where they were, and the record whose step it was, to be checked against the text (see
    /// [`retain_restored`](Self::retain_restored)).
    restored: Vec<(u64, Range<usize>)>,
    restored_by: Option<u64>,
    /// The text is in no buffer: only records of use without one are kept (see
    /// [`set_closed`](Self::set_closed)).
    closed: bool,
    /// What the records in `list` take ([`Record::cost`]).
    bytes: usize,
    /// What the newest turn's agent records in `list` (from `turn_from` on) take of it.
    turn_bytes: usize,
    /// Which pieces of the History edit [`observe`](Self::observe) last saw its claimer puts
    /// into both texts, if any (see [`take_placement`](Self::take_placement)).
    placement: Option<Placement>,
    /// That claimer, for [`placed`](Self::placed).
    placing: Option<Placing>,
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
        self.stepping = None;
        self.restored.clear();
        self.restored_by = None;
        // The next agent edit starts the newest turn's records again.
        self.turn = None;
    }

    /// The buffer starts to undo (`redo` false) or redo one action: the History edits from now on
    /// are that step's (see `FileReview::begin_history_step`).
    pub fn begin_history_step(&mut self, redo: bool) {
        self.stepping = Some(redo);
    }

    /// Which way the buffer's undo or redo steps now (true: a redo), if its caller said so; else
    /// the way the last step went, as a guess (false).
    pub fn stepping_redo(&self) -> (bool, bool) {
        match self.stepping {
            Some(redo) => (redo, true),
            None => (self.redoing, false),
        }
    }

    /// The buffer's undo or redo of something other than a record completed a step (`redo`
    /// true: a redo).
    pub fn stepped(&mut self, redo: bool) {
        self.redoing = redo;
    }

    /// The time of a step from now on: later than every step so far (see [`Record::stepped`]).
    pub fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Whether a record would take the History edit `[a, b) → text` for its step before the
    /// user's edits last stepped at `than` (see [`observe`](Self::observe)): one in the middle
    /// of its step, or going the way the buffer steps and stepped later.
    pub fn steps_first(&mut self, a: usize, b: usize, text: &str, than: u64) -> bool {
        let redo = self.stepping_redo().0;
        let ids = self.points.meeting(a, b);
        ids.into_iter().any(|id| {
            let (Some(r), Some(range)) = (self.list.get(&id), self.points.get(id)) else {
                return false;
            };
            // Alt+U's action, undone, cannot be redone after a new action (see `observe`).
            let alt_u = match r.alt_u {
                AltU::Undone(n) if n != self.actions => AltU::None,
                alt_u => alt_u,
            };
            r.accepts_at(&range, Origin::History, a, b, text)
                && (r.in_progress || (r.goes_as(alt_u, redo) && r.due_as(alt_u, redo) > than))
        })
    }

    /// Keeps what an edit of the user's (or the buffer's undo or redo of one) did to the records
    /// (`context`, from [`observe`](Self::observe)) for the step that takes it back: the records
    /// it moved in a way that step cannot reverse go out of the list, as for an agent edit's own
    /// record.
    pub fn keep_for_user(&mut self, context: Context) -> Context {
        for r in &context.cut {
            self.forget(r.id);
        }
        context
    }

    /// The buffer's undo or redo took back the user's edit that `kept` is of (see
    /// [`keep_for_user`](Self::keep_for_user)): the records it cut come back as they were before
    /// it, which the text is again (checked by [`retain_restored`](Self::retain_restored)).
    pub fn bring_back(&mut self, kept: Context) {
        for r in kept.cut {
            if !self.list.contains_key(&r.id) {
                self.restored.push((r.id, r.range.clone()));
                self.attach(Box::new(r));
            }
        }
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
    /// newest turn's records. A reload (another program's change, which the review shows as the
    /// agent's) has no turn: the buffer's undo and redo of it are recognized all the same.
    /// `kept`: the pieces of `before` the baseline holds (see [`Masks::kept`]).
    pub fn push_agent(
        &mut self,
        range: Range<usize>,
        before: String,
        after: String,
        context: Context,
        turn: Option<u32>,
        kept: Pieces,
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
        if let Some(turn) = turn
            && self.turn != Some(turn)
        {
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
        let empty = range.is_empty();
        let id = self.push(Kind::Agent, range, content, before, after, State::Done);
        if empty {
            for q in &context.before {
                if let Some(r) = self.list.get_mut(q) {
                    pin(r, &[id]);
                }
            }
        }
        if let Some(record) = self.list.get_mut(&id) {
            let mut cut = context.cut;
            for r in &mut cut {
                r.kept_at = record.range.start;
            }
            record.cut_forward = cut;
            record.enclosing = context.enclosing;
            record.turn = turn;
            record.masks.kept = kept;
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
        // The baseline holds the text it restores.
        let kept = 0..restored.len();
        let id = self.push(
            Kind::Reject(reopen),
            range,
            content,
            proposal,
            restored,
            State::Applying,
        );
        if let Some(r) = self.list.get_mut(&id) {
            add_piece(&mut r.masks.kept, kept);
        }
        id
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
        self.clock += 1;
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
            kept_at: 0,
            stepped: self.clock,
            alt_u_at: 0,
            enclosing: Vec::new(),
            pinned_end: Vec::new(),
            pinned_at: 0,
            alt_u: AltU::None,
            masks: Masks::default(),
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
        // Alt+U's action done is not on the buffer's redo stack.
        if record.state == State::Undone && record.alt_u != AltU::Done {
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
    /// as (of the records that accept it, the one the buffer steps first claims it, see below;
    /// none when `claimable` is false, for an edit known to be something else) and, when no
    /// record claimed it, what it did to the records. `rival` is when the user's own edits whose
    /// step the edit also is were last stepped, and `enclosing` the records whose text took
    /// those in (see `user`): a record claims it then only if it is in the middle of its step,
    /// or the buffer steps it first; else those records take it in, even at their boundaries.
    pub fn observe(
        &mut self,
        a: usize,
        b: usize,
        text: &str,
        origin: Origin,
        claimable: bool,
        rival: Option<(u64, &[u64])>,
    ) -> (Option<Claim>, Context) {
        self.restored.clear();
        self.restored_by = None;
        if origin != Origin::Review {
            self.reapplying.clear();
        }
        if origin != Origin::History {
            // A new action clears the buffer's redo stack.
            self.actions += 1;
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
        for r in &mut met {
            // Alt+U's action, undone, cannot be redone after a new action.
            if matches!(r.alt_u, AltU::Undone(n) if n != self.actions) {
                r.alt_u = AltU::None;
            }
        }
        // The record whose step the edit fits: one in the middle of its step, else one going the
        // way the buffer steps (see `Record::goes`), else any. A History edit can undo one edit
        // and redo another at the same time (when an agent edit reverted an earlier one, or a
        // reject the agent's text): the way the buffer's caller says it steps, or else the way
        // the user went last, as the user usually keeps stepping in the same direction. Of
        // several, the one the buffer steps first (see `Record::due`): its undo takes back the
        // latest action first and its redo brings back first what it undid last. Two agent
        // insertions of one text, the second right after the first, leave their empty records at
        // one place once undone: the newest took the first redo, and the second redo, which no
        // record claimed then, went into the baseline. And Alt+U is an action of a reject's
        // record after newer records' actions: by age, the undo of Alt+U's action went to a newer
        // agent edit that had deleted the same text there.
        let accepting = |r: &Record| r.accepts(origin, a, b, text);
        let (redo, told) = match self.stepping {
            Some(redo) if origin == Origin::History => (redo, true),
            _ => (self.redoing, false),
        };
        let first = |pick: &dyn Fn(&Record) -> bool| {
            (0..met.len())
                .filter(|&i| pick(&met[i]) && accepting(&met[i]))
                .max_by_key(|&i| (met[i].due(met[i].goes(redo) == redo), met[i].id))
        };
        // Only undo/redo and our own review edits can be a recorded step (see `accepts`).
        let claimer = if claimable && matches!(origin, Origin::History | Origin::Review) {
            // A reject's own edits are its record's, and Alt+U's those of the rejects it
            // re-applies (another reject's undo or Alt+U could also take a deletion or an
            // insertion of the same text there). The action applies them edit by edit: the one
            // in the middle of its step first (the insertion that completes it can land at the
            // end of the next one's text with that one's text: two of a turn's edits side by
            // side, which Reject Turn takes back one by one), else the oldest, as of two edits
            // at one place the one whose record came first comes first (Alt+U of two rejects
            // that put the same text back where both are empty). Alt+U's edits come in the order
            // of their records (see `reapply_edits`): the next one's record takes it first.
            let going = || first(&|r| r.goes(redo));
            let reapplying = &self.reapplying;
            let next = reapplying.front().copied();
            let ours = |r: &Record| r.state == State::Applying || reapplying.contains(&r.id);
            let oldest = |pick: &dyn Fn(&Record) -> bool| {
                (0..met.len())
                    .filter(|&i| pick(&met[i]) && accepting(&met[i]))
                    .min_by_key(|&i| met[i].id)
            };
            // A record going the other way than the buffer steps takes the edit only when that
            // way is a guess: the undo of an agent edit whose text was accepted (its record is
            // gone) deletes what a later agent deletion there, undone, would delete again, and
            // as that deletion's redo it went into the proposal.
            oldest(&|r| ours(r) && r.in_progress)
                .or_else(|| oldest(&|r| Some(r.id) == next))
                .or_else(|| oldest(&ours))
                .or_else(|| first(&|r| r.in_progress))
                .or_else(|| match rival {
                    Some((t, _)) => going().filter(|&i| met[i].due(redo) > t),
                    None if told => going(),
                    None => going().or_else(|| first(&|_| true)),
                })
        } else {
            None
        };
        let mut claimer = claimer.map(|i| met.remove(i));
        // Records enclosing the claimer take the edit into their text even at their boundary.
        let enclosing = match (&claimer, rival) {
            (Some(r), _) => r.enclosing.clone(),
            (None, Some((_, enclosing))) => enclosing.to_vec(),
            (None, None) => Vec::new(),
        };
        let claimer_end = claimer.as_ref().map(|r| r.range.end);
        let mut pinned = claimer
            .as_ref()
            .map_or(Vec::new(), |r| r.pinned_end.clone());
        // Those still there, unless they take the edit in.
        let at_end = |id: u64| {
            let range = match met.iter().find(|r| r.id == id) {
                Some(r) => Some(r.range.clone()),
                None => self.points.get(id),
            };
            claimer_end.is_some_and(|e| range == Some(e..e)) && !enclosing.contains(&id)
        };
        pinned.retain(|&id| at_end(id));
        // And those that come after those there, pinned to them since they were pinned there
        // (an older pin may be out of date): the step moves them too, but they stay theirs.
        let mut carried: Vec<u64> = Vec::new();
        let mut since: Vec<u64> = vec![claimer.as_ref().map_or(0, |r| r.pinned_at); pinned.len()];
        let mut k = 0;
        while let Some(&id) = pinned.get(k).or_else(|| carried.get(k - pinned.len())) {
            let theirs = match met.iter().find(|r| r.id == id) {
                Some(r) => Some((r.pinned_end.clone(), r.pinned_at)),
                None => self
                    .list
                    .get(&id)
                    .map(|r| (r.pinned_end.clone(), r.pinned_at)),
            };
            if let Some((after, at)) = theirs.filter(|&(_, at)| at > since[k]) {
                for q in after {
                    let new = !pinned.contains(&q) && !carried.contains(&q);
                    if new && Some(q) != claimer.as_ref().map(|r| r.id) && at_end(q) {
                        carried.push(q);
                        since.push(at);
                    }
                }
            }
            k += 1;
        }
        self.placement = None;
        self.placing = None;
        let claimed = claimer.as_mut().map(|rec| {
            let step = rec.step();
            // Which pieces of the edit go into both texts (see `Masks`): the buffer's undo and
            // redo carry them over; Lemma's own edits (a reject, Alt+U) are proposals.
            let at = a - rec.range.start;
            let (from, to) = rec.step_texts();
            let deletes_from = a < b && rec.content == from && b - a == from.len();
            let inserts_to = !text.is_empty() && text == to;
            if origin == Origin::History {
                let deleted = pieces_in(&rec.masks.ours, &(at..at + (b - a)));
                let inserted = if inserts_to {
                    rec.masks.taken.clone()
                } else {
                    Pieces::new()
                };
                let placement = Placement {
                    deleted_at: vec![None; deleted.len()],
                    inserted_at: vec![None; inserted.len()],
                    deleted,
                    inserted,
                };
                self.placement = (!placement.is_empty()).then_some(placement);
            }
            let kept = if inserts_to {
                rec.masks.kept.clone()
            } else {
                Pieces::new()
            };
            rec.apply(a, b, text);
            let done = rec.content == rec.step_texts().1;
            rec.in_progress = !done;
            self.placing = Some(Placing {
                id: rec.id,
                at,
                deletes_from,
                inserts_to,
                done,
                kept,
            });
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
        // The other way round, a pure deletion leaves the records whose text it deletes (the
        // claimer, or the edit's own record, and those that took it in) empty where empty
        // records already are, before that text: they follow those records' ends through their
        // later steps, as the records it collapses follow the claimer's. Alt+U's undo leaves two
        // rejects of a turn empty at one place: the one undone first took the first insertion
        // there, and the other one, left before that text, missed the second.
        let mut before_it = Vec::new();
        // A reject takes back every proposal in its hunk: the text the agent's deletions there
        // took out is in the current text again (an accept no longer takes them).
        let rejecting = origin == Origin::Review
            && claimed
                .as_ref()
                .is_some_and(|c| c.step == Step::Apply && matches!(c.kind, Kind::Reject(_)));
        for mut rec in met {
            if rejecting
                && rec.kind == Kind::Agent
                && rec.state == State::Done
                && rec.content.is_empty()
                && a <= rec.range.start
                && rec.range.end <= b
            {
                rec.masks.kept.clear();
            }
            let encloses = enclosing.contains(&rec.id) && rec.within(a, b);
            if rec.changes_inside(a, b) || encloses {
                if rec.in_progress {
                    // Something else interrupted the step: the record goes.
                    continue;
                }
                rec.apply_inside(a, b, text);
                trackers.push(rec.id);
                kept.push(rec);
                continue;
            }
            if collapses && !rec.in_progress && rec.range == (a..a) {
                // It is kept: a deletion after it leaves it where it is.
                before_it.push(kept.len());
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
        if before_it.len() > MAX_ORDERED {
            // So many there that their order is not kept, so that a deletion costs at most as
            // much as the records it meets.
            before_it.clear();
        }
        if !before_it.is_empty() {
            let emptied = claimed
                .as_ref()
                .filter(|c| c.range == (a, a))
                .and_then(|c| c.record);
            let follow: Vec<u64> = emptied
                .into_iter()
                .chain(
                    kept.iter()
                        .filter(|r| r.range == (a..a) && trackers.contains(&r.id))
                        .map(|r| r.id),
                )
                .collect();
            for &i in &before_it {
                pin(&mut kept[i], &follow);
            }
        }
        // Without a claimer, the edit's own record (if it gets one) follows them too.
        let before = if claimer.is_none() {
            before_it.iter().map(|&i| kept[i].id).collect()
        } else {
            Vec::new()
        };
        cut.reverse();
        let mut restored = Vec::new();
        if let Some(c) = claimed.filter(|c| c.done && origin == Origin::Review)
            && let Some(id) = c.record
        {
            self.reapplying.retain(|&r| r != id);
        }
        let claim = claimer.as_mut().zip(claimed).map(|(rec, claim)| {
            rec.keep_cut(&mut cut);
            for id in trackers.drain(..) {
                if !rec.enclosing.contains(&id) {
                    rec.enclosing.push(id);
                }
            }
            rec.pinned_end = pinned.clone();
            self.clock += 1;
            rec.pinned_at = self.clock;
            if claim.done {
                rec.finish_step(&mut restored);
                self.clock += 1;
                let now = self.clock;
                match (origin, claim.step, rec.alt_u) {
                    // Alt+U, a new action.
                    (Origin::Review, Step::Undo, _) => {
                        rec.alt_u = AltU::Done;
                        rec.alt_u_at = now;
                    }
                    // The undo of Alt+U's action.
                    (Origin::History, Step::Redo, AltU::Done) => {
                        rec.alt_u = AltU::Undone(self.actions);
                        rec.alt_u_at = now;
                        self.redoing = false;
                    }
                    (Origin::History, Step::Redo, _) => {
                        rec.stepped = now;
                        self.redoing = true;
                    }
                    // The redo of Alt+U's action, or the undo of the reject's own (Alt+U's
                    // action stays above it on the redo stack): which way the buffer steps.
                    (Origin::History, Step::Undo, AltU::Undone(_)) => {
                        if redo {
                            rec.alt_u = AltU::Done;
                            rec.alt_u_at = now;
                        } else {
                            rec.stepped = now;
                        }
                        self.redoing = redo;
                    }
                    (Origin::History, Step::Undo, _) => {
                        rec.stepped = now;
                        self.redoing = false;
                    }
                    _ => rec.stepped = now,
                }
            }
            claim
        });
        // Every record the edit did not meet lies after it or before it.
        self.points.moved(b, signed(text.len()) - signed(b - a));
        let end = claimer.as_ref().map(|r| r.range.end);
        self.restored_by = claimer.as_ref().map(|r| r.id);
        for rec in claimer.into_iter().chain(kept) {
            self.attach(rec);
        }
        if let Some(end) = end {
            for &id in pinned.iter().chain(&carried) {
                self.points.place(id, &(end..end));
            }
        }
        // Without a claimer, the edit's own record (if it gets one) keeps the collapsed records.
        cut.append(&mut displaced);
        cut.sort_by_key(|r| r.id);
        for (rec, was) in restored {
            self.restored.push((rec.id, was));
            self.attach(Box::new(rec));
        }
        (
            claim,
            Context {
                cut,
                enclosing: trackers,
                before,
            },
        )
    }

    /// Takes the records the last [`observe`](Self::observe) brought back for which
    /// `valid(range, content)` does not hold out of the list again. Every other record was kept
    /// equal to the text edit by edit; these come back as they were when an edit cut them,
    /// which matches the text when the buffer's undo replays strictly in order. Alt+U does not:
    /// it re-applies a reject after other actions, which the buffer undoes before it undoes the
    /// reject itself, so the record whose step brought them back keeps them for its next step
    /// the same way (the reject's own undo then finds the text they were cut from).
    pub fn retain_restored(&mut self, valid: impl Fn(&Range<usize>, &str) -> bool) {
        let keeper = self.restored_by.take();
        for (id, was) in std::mem::take(&mut self.restored) {
            let fits = match (self.list.get(&id), self.points.get(id)) {
                (Some(r), Some(range)) => valid(&range, &r.content),
                _ => true,
            };
            if fits {
                continue;
            }
            if let Some(r) = self.detach(id)
                && let Some(keeper) = keeper.and_then(|k| self.list.get_mut(&k))
            {
                keeper.keep_back(*r, was);
            }
        }
    }

    /// Every record's range and content, by start (for the tests' checks).
    pub fn in_text_order(&self) -> Vec<(Range<usize>, &str)> {
        self.points
            .in_order()
            .filter_map(|(id, range)| Some((range, self.list.get(&id)?.content.as_str())))
            .collect()
    }

    /// An accept took the hunk at `range` into the baseline (see [`Masks`]): the pieces of the
    /// records' text in it that were not the baseline's already are now theirs there (their next
    /// step takes them out of both texts), and of a record wholly in it, what its change took out
    /// of the current text only is now out of the baseline too (its next step puts it into both),
    /// and so for the records it keeps for that step (the edits it took the text of).
    /// Returns what it changed, for Alt+U of the accept ([`unaccept`](Self::unaccept)).
    /// `text` is the hunk's text: a deletion whose text is there again at its place (a reject
    /// put it back) is no change the accept takes.
    pub fn accept_overlapping(&mut self, range: &Range<usize>, text: &str) -> Vec<Taken> {
        // The records `r`'s next step brings back (it took their text in, or out): what its
        // change took out of the current text only is now out of the baseline too, and what is
        // left of their text (what the step that cut them did not take out) the accept took.
        fn nested(r: &mut Record, out: &mut Vec<Taken>) {
            let (back, gone) = match r.state {
                State::Done => (&mut r.cut_forward, r.before.len()),
                State::Undone => (&mut r.cut_backward, r.after.len()),
                State::Applying => return,
            };
            for k in back {
                if k.in_progress {
                    continue;
                }
                let taken = std::mem::take(&mut k.masks.kept);
                for x in &taken {
                    add_piece(&mut k.masks.taken, x.clone());
                }
                let (s, at) = (k.range.start, k.kept_at);
                let cut = at.max(s) - s..(at + gone).min(k.range.end).max(at.max(s)) - s;
                let all: Pieces = std::iter::once(0..k.content.len()).collect();
                let left = pieces_minus(&all, &std::iter::once(cut).collect());
                let mut mine = k.masks.ours.clone();
                mine.extend(k.masks.held.iter().cloned());
                mine.sort_by_key(|x| x.start);
                let ours = pieces_minus(&left, &mine);
                let as_left = k.content == k.step_texts().0;
                for x in &ours {
                    add_piece(&mut k.masks.ours, x.clone());
                    if as_left {
                        add_piece(&mut k.masks.at_step.0, x.clone());
                    }
                }
                if !taken.is_empty() || !ours.is_empty() {
                    out.push(Taken {
                        id: k.id,
                        ours,
                        taken,
                    });
                }
                nested(k, out);
            }
        }
        let mut out = Vec::new();
        for id in self.points.meeting(range.start, range.end) {
            let Some(at) = self.points.get(id) else {
                continue;
            };
            let Some(r) = self.list.get_mut(&id) else {
                continue;
            };
            if r.in_progress || r.state == State::Applying {
                continue;
            }
            let s = at.start;
            let inside = range.start.max(at.start)..range.end.min(at.end);
            let mut ours = Pieces::new();
            if inside.start < inside.end {
                let mine: Pieces = std::iter::once(inside.start - s..inside.end - s).collect();
                ours = if r.masks.ours.is_empty() && r.masks.held.is_empty() {
                    mine
                } else {
                    let mut all = r.masks.ours.clone();
                    all.extend(r.masks.held.iter().cloned());
                    all.sort_by_key(|x| x.start);
                    pieces_minus(&mine, &all)
                };
                let as_left = r.content == r.step_texts().0;
                for x in &ours {
                    add_piece(&mut r.masks.ours, x.clone());
                    if as_left {
                        add_piece(&mut r.masks.at_step.0, x.clone());
                    }
                }
            }
            let mut taken = Pieces::new();
            // The text a deletion took out is there again right at its place.
            let back = at.is_empty() && {
                let (p, gone) = (at.start - range.start, r.step_texts().1);
                let n = gone.len();
                text.get(p..p + n) == Some(gone)
                    || p.checked_sub(n).and_then(|q| text.get(q..p)) == Some(gone)
            };
            if range.start <= at.start && at.end <= range.end && !back {
                taken = std::mem::take(&mut r.masks.kept);
                for x in &taken {
                    add_piece(&mut r.masks.taken, x.clone());
                }
                nested(r, &mut out);
            }
            if !ours.is_empty() || !taken.is_empty() {
                out.push(Taken { id, ours, taken });
            }
        }
        out
    }

    /// Alt+U of an accept put back the hunk whose records' masks `taken` says it changed (see
    /// [`accept_overlapping`](Self::accept_overlapping)): it is a proposal again.
    pub fn unaccept(&mut self, taken: &[Taken]) {
        fn undo(r: &mut Record, t: &Taken) {
            let n = r.content.len();
            let ours: Pieces = t.ours.iter().filter(|x| x.end <= n).cloned().collect();
            r.masks.ours = pieces_minus(&r.masks.ours, &ours);
            r.masks.at_step.0 = pieces_minus(&r.masks.at_step.0, &ours);
            r.masks.taken = pieces_minus(&r.masks.taken, &t.taken);
            for x in &t.taken {
                add_piece(&mut r.masks.kept, x.clone());
            }
        }
        fn nested(r: &mut Record, by: &BTreeMap<u64, &Taken>) {
            for k in r.cut_forward.iter_mut().chain(&mut r.cut_backward) {
                if let Some(t) = by.get(&k.id) {
                    undo(k, t);
                }
                nested(k, by);
            }
        }
        // Those in the list directly; the others the records they are kept in hold.
        let mut kept = BTreeMap::new();
        for t in taken {
            match self.list.get_mut(&t.id) {
                Some(r) => undo(r, t),
                None => {
                    kept.insert(t.id, t);
                }
            }
        }
        if !kept.is_empty() {
            for r in self.list.values_mut() {
                nested(r, &kept);
            }
        }
    }

    pub fn clear(&mut self) {
        self.list.clear();
        self.points = Points::default();
        self.droppable.clear();
        self.undone.clear();
        self.restored.clear();
        self.restored_by = None;
        self.bytes = 0;
        self.turn_bytes = 0;
    }

    /// Whether one of the records `ids` holds the agent's text that no accept took (see
    /// [`Record::holds_agent_text`]).
    pub fn hold_agent_text(&self, ids: &[u64]) -> bool {
        ids.iter()
            .any(|id| self.list.get(id).is_some_and(|r| r.holds_agent_text()))
    }

    /// Which pieces of the History edit [`observe`](Self::observe) last saw go into both texts,
    /// if any: its claimer's (see [`Masks`]).
    pub fn take_placement(&mut self) -> Option<Placement> {
        self.placement.take()
    }

    /// The edit [`observe`](Self::observe) last saw, which its claimer took, went into the texts
    /// as `placed` says (what of it went into both), and the pieces of the text it deleted that
    /// the baseline held before (`aligned`, both from the edit's start): the claimer's masks
    /// follow (see [`Masks`]). Of a step's edit that took out all of the text it replaces, what
    /// went out of both texts the step that reverses it puts back into both, and what the
    /// baseline still holds is the change an accept would take.
    pub fn placed(&mut self, placed: &Placement, aligned: &Pieces) {
        let Some(p) = self.placing.take() else {
            return;
        };
        let Some(r) = self.list.get_mut(&p.id) else {
            return;
        };
        if p.inserts_to {
            for x in &placed.inserted {
                add_piece(&mut r.masks.ours, x.start + p.at..x.end + p.at);
            }
            for x in pieces_minus(&p.kept, &placed.inserted) {
                add_piece(&mut r.masks.held, x.start + p.at..x.end + p.at);
            }
        }
        if p.deletes_from {
            let next = (
                placed.deleted.clone(),
                pieces_minus(aligned, &placed.deleted),
            );
            if p.done {
                (r.masks.taken, r.masks.kept) = next;
            } else {
                r.masks.next = Some(next);
            }
        }
        if p.done {
            r.masks.at_step = (r.masks.ours.clone(), r.masks.held.clone());
        }
    }

    /// How many records are kept and what they take, about ([`Record::cost`]).
    pub fn held(&self) -> (usize, usize) {
        (self.list.len(), self.bytes)
    }

    /// Whether what the records are said to take is what they take.
    pub fn check_costs(&self) -> Result<(), String> {
        let bytes: usize = self.list.values().map(|r| r.cost()).sum();
        let turn: usize = self
            .list
            .range(self.turn_from..)
            .filter(|(_, r)| r.kind == Kind::Agent)
            .map(|(_, r)| r.cost())
            .sum();
        let said = (self.bytes, self.turn_bytes);
        if said == (bytes, turn) {
            Ok(())
        } else {
            Err(format!(
                "the records take {:?}, not {said:?}",
                (bytes, turn)
            ))
        }
    }

    /// The edits re-applying the proposals of these reject records (Alt+U), descending. Records
    /// whose text was changed since, or whose reject was already undone, give none.
    pub fn reapply_edits(&mut self, ids: &[u64], current: &str) -> Vec<TextEdit> {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut edits: Vec<(u64, TextEdit)> = ids
            .iter()
            .filter_map(|id| {
                let r = self.list.get(id)?;
                let range = self.points.get(*id)?;
                let reapply = matches!(r.kind, Kind::Reject(_))
                    && r.state == State::Done
                    && !r.in_progress
                    && r.content == r.after
                    && current.get(range.clone()) == Some(r.content.as_str());
                reapply.then(|| {
                    let text = r.before.clone();
                    (*id, TextEdit { range, text })
                })
            })
            .collect();
        // Descending, a replacement before an insertion at its start (records of one decision
        // can meet: a turn's deletion point next to its insertion).
        edits.sort_by_key(|(_, e)| std::cmp::Reverse((e.range.start, e.range.end)));
        // Of insertions at one place, the one applied last comes first in the text: a record
        // pinned after another (its text came after the other's, see `Record::pinned_end`)
        // takes its insertion first, else the oldest first. In the other order the first
        // insertion moved the pinned record past its text, and the second, before that text,
        // was nobody's: the undo of Alt+U's action then put the deleted text into the baseline.
        let mut k = 0;
        while k < edits.len() {
            let at = edits[k].1.range.clone();
            let n = edits[k..].iter().take_while(|(_, e)| e.range == at).count();
            if at.is_empty() && n > 1 {
                let mut group: Vec<(u64, TextEdit)> = edits.drain(k..k + n).collect();
                let after: Vec<Vec<u64>> = group
                    .iter()
                    .map(|(id, _)| self.pinned_after(*id, &at))
                    .collect();
                let mut after: BTreeMap<u64, Vec<u64>> =
                    group.iter().map(|(id, _)| *id).zip(after).collect();
                let mut ordered = Vec::with_capacity(n);
                while !group.is_empty() {
                    let i = (0..group.len())
                        .find(|&i| {
                            let mine = after.get(&group[i].0).map_or(&[][..], Vec::as_slice);
                            !group.iter().any(|(q, _)| mine.contains(q))
                        })
                        .unwrap_or(0);
                    let (id, edit) = group.remove(i);
                    after.remove(&id);
                    ordered.push((id, edit));
                }
                edits.splice(k..k, ordered);
            }
            k += n;
        }
        self.reapplying = edits.iter().map(|(id, _)| *id).collect();
        edits.into_iter().map(|(_, e)| e).collect()
    }
}

impl Records {
    /// The records at the place `at` (empty) pinned after the record `id`, directly or through
    /// others pinned there, as [`observe`](Self::observe) carries them along with the record's
    /// end (a pin older than the one it is reached through is out of date).
    fn pinned_after(&self, id: u64, at: &Range<usize>) -> Vec<u64> {
        let Some(r) = self.list.get(&id) else {
            return Vec::new();
        };
        let here = |q: u64| self.points.get(q).as_ref() == Some(at);
        let mut found: Vec<u64> = Vec::new();
        let mut queue: Vec<(u64, u64)> = r
            .pinned_end
            .iter()
            .filter(|&&q| q != id && here(q))
            .map(|&q| (q, r.pinned_at))
            .collect();
        while let Some((q, since)) = queue.pop() {
            if found.contains(&q) {
                continue;
            }
            found.push(q);
            if let Some(next) = self.list.get(&q).filter(|n| n.pinned_at > since) {
                for &p in &next.pinned_end {
                    if p != id && !found.contains(&p) && here(p) {
                        queue.push((p, next.pinned_at));
                    }
                }
            }
        }
        found
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
            Some(turn),
            Vec::new(),
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
                Some(2),
                Vec::new(),
            );
            records.check_costs().unwrap();
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
        records.push_agent(
            10..11,
            String::new(),
            "a".into(),
            Context::default(),
            Some(1),
            Vec::new(),
        );
        records.begin_decision();
        let (proposal, restored) = ("0123456789abcde", "xy");
        let reject = records.push_reject(5..20, proposal.into(), restored.into(), reopen);
        let (claim, _) = records.observe(5, 20, restored, Origin::Review, true, None);
        assert!(claim.is_some_and(|c| c.done && c.step == Step::Apply));
        assert_eq!(records.list[&reject].cut_forward.len(), 1);
        records.push_agent(
            30..31,
            String::new(),
            "b".into(),
            Context::default(),
            Some(1),
            Vec::new(),
        );
        assert_eq!(records.held().0, 2);

        records.set_closed(true);
        records.check_costs().unwrap();
        assert_eq!(records.list.keys().copied().collect::<Vec<_>>(), [reject]);
        assert!(records.list[&reject].cut_forward.is_empty());
        records.push_agent(
            40..41,
            String::new(),
            "c".into(),
            Context::default(),
            Some(1),
            Vec::new(),
        );
        assert_eq!(records.held().0, 1, "agent edits are not recorded");
        // Alt+U re-applies the proposal and undoes the reject: nothing can redo it now.
        let current = format!("{}{restored}{}", "_".repeat(5), "_".repeat(30));
        let edits = records.reapply_edits(&[reject], &current);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        let (claim, _) = records.observe(
            e.range.start,
            e.range.end,
            &e.text,
            Origin::Review,
            true,
            None,
        );
        assert!(claim.is_some_and(|c| c.done && c.step == Step::Undo));
        records.check_costs().unwrap();
        assert_eq!(records.held(), (0, 0));

        records.set_closed(false);
        records.push_agent(
            50..51,
            String::new(),
            "d".into(),
            Context::default(),
            Some(1),
            Vec::new(),
        );
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
