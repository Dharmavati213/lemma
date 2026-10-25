//! [`FileReview`]: baseline, hunks, edit classification, accept/reject/undo and persistence.
//!
//! Between two calls to [`FileReview::recompute`] the review keeps an exact model of the current
//! text: the baseline outside the hunks, and each hunk's current text inside them. `on_edit`
//! updates that model (folding user edits into the baseline, growing or creating hunks for the
//! rest), so classification, accept and reject stay correct even before the next recompute. Hunks
//! touched by `on_edit` are *provisional*: not line-aligned, without word spans, and keeping the id
//! of the first hunk they grew from; `recompute` turns them back into line hunks.
//!
//! Agent edits are also attributed to the current turn ([`turns`]); every hunk's `turns` are the
//! turns whose attributed text it meets, kept up to date by the same operations. What each turn's
//! edits replaced is kept exactly where possible ([`segments`]), so that a turn's own changes can
//! be taken back ([`per_turn`]).

mod batch;
mod ids;
mod journal;
mod model;
mod per_turn;
mod records;
mod segments;
#[cfg(test)]
mod tests;
mod turns;

use crate::diff::LineDiff;
use crate::edits::minimal_edits;
use crate::text::{newlines, offset, shift_bytes, shift_lines, signed, trimmed_edit};
use crate::{
    EditClass, Hunk, HunkDelta, HunkId, PersistedHunk, PersistedReview, ReviewError, ReviewEvent,
    TurnEdit, TurnRange, UndoAction,
};
use journal::{Accepted, Entry, Journal};
use lemma_core::{Origin, TextEdit, fnv1a64};
use records::{Kind, Records, Step};
use segments::{Parked, Segment, Segments};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use turns::Attribution;

/// Bookkeeping kept next to each [`Hunk`] (same index in `FileReview::meta`).
#[derive(Clone, Debug)]
struct Meta {
    /// The hunk's current text, `current[cur_bytes]`.
    new_text: String,
    /// Disambiguates hunks with identical texts; part of the [`HunkId`].
    occurrence: u64,
    /// `pair_hash(old_text, new_text)` while the texts are still those the occurrence was assigned
    /// for; `None` once `on_edit` changed them.
    pair: Option<u64>,
    /// Turn in which `UserEditedInHunk` was last sent for this hunk.
    edit_reported: Option<u64>,
    /// Turn in which `UndoneByUser` was last sent for this hunk.
    undo_reported: Option<u64>,
    /// Set when undoing a reject re-opened this region.
    reopen: Option<Reopen>,
}

/// Identity of a rejected hunk, handed to the hunk that re-opens when the reject is undone: the
/// next recompute gives the overlapping hunk with the same texts the old occurrence (and so the old
/// [`HunkId`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reopen {
    pair: u64,
    occurrence: u64,
    user_touched: bool,
}

/// Hash of a hunk's texts, without the occurrence.
fn pair_hash(old: &str, new: &str) -> u64 {
    fnv1a64(&[old.as_bytes(), new.as_bytes()])
}

/// `fnv1a64(old, new, occurrence)`, the documented [`HunkId`].
fn hunk_id(old: &str, new: &str, occurrence: u64) -> HunkId {
    HunkId(fnv1a64(&[
        old.as_bytes(),
        new.as_bytes(),
        &occurrence.to_le_bytes(),
    ]))
}

/// The review of one file: its baseline and the hunks `diff(baseline, current)`.
///
/// The caller owns the current text (the buffer) and reports every change to [`on_edit`] before
/// applying it. Methods taking `current` expect exactly that text.
///
/// [`on_edit`]: FileReview::on_edit
#[derive(Clone, Debug)]
pub struct FileReview {
    baseline: String,
    hunks: Vec<Hunk>,
    meta: Vec<Meta>,
    events: Vec<ReviewEvent>,
    journal: Journal,
    records: Records,
    /// Hunk ids as of the last `recompute`, in text order; `HunkDelta` is relative to them.
    published: Vec<HunkId>,
    /// Per `pair_hash`, an occurrence below which every number is taken by a hunk id (see
    /// `free_occurrence`); cleared whenever a hunk id disappears.
    free_hints: HashMap<u64, u64>,
    /// Length of the current text according to the model (`baseline.len()` plus the net growth
    /// of every hunk), updated by every change.
    current_len: usize,
    /// Where the last change, if it was a User or History deletion (or replacement) folded into
    /// the baseline, removed its text (current and baseline offset): a History insertion there
    /// next is that change being undone.
    folded_deletion: Option<(usize, usize)>,
    turn: u64,
    /// The agent turn new agent edits are attributed to ([`FileReview::set_turn`]).
    attr_turn: u32,
    /// Which turn wrote which part of the current text.
    attribution: Attribution,
    /// Turns taken out of the attribution by a recorded step (a reject, the undo of an agent
    /// edit), by record id: reversing the step puts them back.
    parked: Vec<(u64, Vec<u32>)>,
    /// What each turn's edits replaced, where that is known exactly.
    segments: Segments,
    /// Where a turn changed text whose segment was lost (another change cut into it): hunks there
    /// are not decided per turn.
    unknown: Attribution,
    /// Segments and unknown places a reject took out, by record id: undoing the reject puts them
    /// back.
    parked_segments: Vec<(u64, Parked)>,
    /// Bumped by every change of the state (see [`FileReview::generation`]).
    generation: u64,
    created_unix: u64,
    /// While [`FileReview::on_edits`] (or Alt+U of several regions) runs: what `on_edit` consults
    /// instead of the whole hunk list (see `batch`).
    batch: Option<batch::Batch>,
}

impl FileReview {
    /// A review whose baseline is `baseline` (the file just before the agent's first edit). It has
    /// no hunks until edits arrive through [`on_edit`](Self::on_edit).
    pub fn new(baseline: String) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            current_len: baseline.len(),
            baseline,
            hunks: Vec::new(),
            meta: Vec::new(),
            events: Vec::new(),
            journal: Journal::default(),
            records: Records::default(),
            published: Vec::new(),
            free_hints: HashMap::new(),
            folded_deletion: None,
            turn: 0,
            attr_turn: 0,
            attribution: Attribution::default(),
            parked: Vec::new(),
            segments: Segments::default(),
            unknown: Attribution::default(),
            parked_segments: Vec::new(),
            generation: 0,
            created_unix,
            batch: None,
        }
    }

    /// A number that changes whenever the review's state (what [`to_persisted`] saves, the hunks,
    /// the journal) may have changed: equal numbers mean an unchanged review. Cheap, for callers
    /// that cache what they derive from a review.
    ///
    /// [`to_persisted`]: Self::to_persisted
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The baseline: the text before the agent's edits, plus accepted hunks and the user's own
    /// edits outside hunks.
    pub fn baseline(&self) -> &str {
        &self.baseline
    }

    /// All hunks, in text order, non-overlapping.
    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    /// The hunk with this id.
    pub fn hunk(&self, id: HunkId) -> Option<&Hunk> {
        self.hunks.iter().find(|h| h.id == id)
    }

    /// The hunk whose current lines contain `cur_line`, or the pure deletion anchored at it.
    pub fn hunk_at_line(&self, cur_line: u32) -> Option<&Hunk> {
        let first = self.hunks.partition_point(|h| {
            h.cur_lines.end < cur_line || (h.cur_lines.end == cur_line && !h.cur_lines.is_empty())
        });
        self.hunks[first..]
            .iter()
            .take_while(|h| h.cur_lines.start <= cur_line)
            .find(|h| h.cur_lines.contains(&cur_line) || h.cur_lines == (cur_line..cur_line))
    }

    /// Starts a new agent turn: a hunk the user edits or undoes from now on is reported again
    /// (`UserEditedInHunk` and `UndoneByUser` are sent at most once per hunk per turn).
    pub fn begin_turn(&mut self) {
        self.turn += 1;
    }

    /// Attributes agent edits from now on to agent turn `turn` (0, the default, stands for edits
    /// made outside any turn). Callers of [`begin_turn`](Self::begin_turn) call this too, with the
    /// number of the turn that starts; a review created during a turn is told its turn right
    /// away.
    ///
    /// Each `Origin::Agent` edit records the turn and the range it wrote; after
    /// [`recompute`](Self::recompute) each hunk lists the turns whose recorded ranges meet it
    /// ([`Hunk::turns`]). The ranges move with later edits like hunk ranges do; deciding a hunk
    /// drops them (undoing the decision, or the buffer undo of a reject, brings them back), and
    /// undoing an agent edit with the buffer's undo takes its turn out until it is redone.
    pub fn set_turn(&mut self, turn: u32) {
        self.attr_turn = turn;
    }

    /// The turn agent edits are attributed to (see [`set_turn`](Self::set_turn)).
    pub fn turn(&self) -> u32 {
        self.attr_turn
    }

    /// The hunks of turn `turn` alone (`turns == [turn]`), in text order. Hunks that also hold
    /// other turns' edits are not among them (see [`mixed_hunks_of_turn`](Self::mixed_hunks_of_turn)).
    pub fn hunks_of_turn(&self, turn: u32) -> Vec<&Hunk> {
        self.hunks.iter().filter(|h| h.turns == [turn]).collect()
    }

    /// How many hunks hold edits of `turn` and of other turns: [`accept_turn`] and
    /// [`reject_turn`] leave them alone, so they must be decided one by one.
    ///
    /// [`accept_turn`]: Self::accept_turn
    /// [`reject_turn`]: Self::reject_turn
    pub fn mixed_hunks_of_turn(&self, turn: u32) -> usize {
        self.hunks
            .iter()
            .filter(|h| h.turns.len() > 1 && h.turns.contains(&turn))
            .count()
    }

    /// Accepts the hunks with the ids `ids` (ids no hunk has are skipped) as one decision, like
    /// [`accept_turn`](Self::accept_turn) does for a turn. Returns how many were accepted; none
    /// when `current` is stale.
    pub fn accept_hunks(&mut self, ids: &[HunkId], current: &str) -> usize {
        let picked = self.indices_of(ids);
        if self.accept_indices(&picked, current) {
            picked.len()
        } else {
            0
        }
    }

    /// The buffer edits rejecting the hunks with the ids `ids` (ids no hunk has are skipped), in
    /// descending order, journalled as one decision, like [`reject_turn`](Self::reject_turn) does
    /// for a turn. Empty when `current` is stale.
    pub fn reject_hunks(&mut self, ids: &[HunkId], current: &str) -> Vec<TextEdit> {
        let picked = self.indices_of(ids);
        self.reject_indices(&picked, current)
    }

    /// Indices of the hunks with these ids, ascending.
    fn indices_of(&self, ids: &[HunkId]) -> Vec<usize> {
        let ids: HashSet<HunkId> = ids.iter().copied().collect();
        (0..self.hunks.len())
            .filter(|&k| ids.contains(&self.hunks[k].id))
            .collect()
    }

    /// Accepts the hunks at `picked` (ascending) as one journalled decision; false (nothing
    /// done) when there are none or `current` is stale.
    fn accept_indices(&mut self, picked: &[usize], current: &str) -> bool {
        if picked.is_empty() || !self.check_indices(current, picked) {
            return false;
        }
        self.generation += 1;
        let mut regions = Vec::with_capacity(picked.len());
        let mut events = Vec::with_capacity(picked.len());
        for &k in picked.iter().rev() {
            let (region, event) = self.accept_at(k);
            regions.push(region);
            events.push(event);
        }
        events.reverse();
        self.events.extend(events);
        self.journal.push(Entry::Accept(regions));
        true
    }

    /// The edits rejecting the hunks at `picked` (ascending), descending, journalled as one
    /// decision; empty when there are none or `current` is stale.
    fn reject_indices(&mut self, picked: &[usize], current: &str) -> Vec<TextEdit> {
        if picked.is_empty() || !self.check_indices(current, picked) {
            return Vec::new();
        }
        self.generation += 1;
        self.records.begin_decision();
        let mut edits = Vec::new();
        let mut records = Vec::new();
        let mut events = Vec::new();
        for &k in picked.iter().rev() {
            let (edit, record, event) = self.reject_at(k);
            if let Some(record) = record {
                edits.push(edit);
                records.push(record);
            }
            events.extend(event);
        }
        events.reverse();
        self.events.extend(events);
        if !records.is_empty() {
            self.journal.push(Entry::Reject(records));
        }
        edits
    }

    /// Renumbers the turns of every attribution with `f` (turns mapped to the same number become
    /// one), e.g. to move the turns of a review restored from an earlier session out of the way
    /// of the current session's.
    pub fn remap_turns(&mut self, mut f: impl FnMut(u32) -> u32) {
        self.generation += 1;
        self.attribution.remap(&mut f);
        self.unknown.remap(&mut f);
        self.segments.remap(&mut f);
        for (_, turns) in &mut self.parked {
            for t in turns.iter_mut() {
                *t = f(*t);
            }
        }
        for (_, parked) in &mut self.parked_segments {
            parked.remap(&mut f);
        }
        self.journal.remap(&mut f);
        for h in &mut self.hunks {
            h.turns = self.attribution.turns_touching(&h.cur_bytes);
        }
    }

    /// [`check`](Self::check) for the hunks at `indices`.
    fn check_indices(&self, current: &str, indices: &[usize]) -> bool {
        current.len() == self.expected_len()
            && indices.iter().all(|&k| {
                current.get(self.hunks[k].cur_bytes.clone()) == Some(self.meta[k].new_text.as_str())
            })
    }

    /// Drops the attribution that only describes hunks touching the closed interval `r` whose
    /// text went back to the baseline's (an agent edit taken back by another): no change is left
    /// there, as the next recompute would find.
    fn forget_unchanged_near(&mut self, r: &Range<usize>) {
        let first = self.hunks.partition_point(|h| h.cur_bytes.end < r.start);
        let unchanged: Vec<Range<usize>> = self.hunks[first..]
            .iter()
            .zip(&self.meta[first..])
            .take_while(|(h, _)| h.cur_bytes.start <= r.end)
            .filter(|(h, m)| h.old_text == m.new_text)
            .map(|(h, _)| h.cur_bytes.clone())
            .collect();
        for range in unchanged {
            self.prune_near(&range, None);
        }
    }

    /// Drops the intervals touching the closed interval `r` that meet no hunk with a change
    /// (other than hunk `except`, about to be decided): what is left of a decided or unchanged
    /// region describes no change, as the next recompute would find.
    fn prune_near(&mut self, r: &Range<usize>, except: Option<usize>) {
        let (hunks, meta) = (&self.hunks, &self.meta);
        let dropped = self.attribution.retain_near(r, |x| {
            let first = hunks.partition_point(|h| h.cur_bytes.end < x.start);
            hunks[first..]
                .iter()
                .zip(&meta[first..])
                .enumerate()
                .take_while(|(_, (h, _))| h.cur_bytes.start <= x.end)
                .any(|(i, (h, m))| Some(first + i) != except && h.old_text != m.new_text)
        });
        if let Some(dropped) = dropped {
            self.refresh_turns_near(&dropped);
        }
    }

    /// Recomputes the `turns` of the hunks whose current range touches the closed interval `r`.
    fn refresh_turns_near(&mut self, r: &Range<usize>) {
        let first = self.hunks.partition_point(|h| h.cur_bytes.end < r.start);
        for h in &mut self.hunks[first..] {
            if h.cur_bytes.start > r.end {
                break;
            }
            h.turns = self.attribution.turns_touching(&h.cur_bytes);
        }
    }

    /// Records that the agent edit just merged (`[a, a + inserted.len())` replacing `removed`)
    /// was written in the current turn. A pure insertion or deletion is widened to every
    /// placement in equal text with the same result (see `turns`).
    fn attribute_agent_edit(&mut self, k: usize, a: usize, inserted: &str, removed: &str) {
        let end = a + inserted.len();
        let window = if inserted.is_empty() == removed.is_empty() {
            a..end
        } else {
            let (before, after) = self.slide_context(k, a, end);
            if inserted.is_empty() {
                turns::deletion_window(a, removed.as_bytes(), &before, &after)
            } else {
                turns::insertion_window(a, inserted.as_bytes(), &before, &after)
            }
        };
        self.attribution.add(self.attr_turn, window.clone());
        self.refresh_turns_near(&window);
    }

    /// The current text just before `a` and just after `end` (at most [`turns::MAX_SLIDE`] bytes
    /// each, not past the neighbouring hunks), where `[a, end)` lies in hunk `k`: read from the
    /// hunk's text and the baseline gaps next to it, without walking the other hunks.
    fn slide_context(&self, k: usize, a: usize, end: usize) -> (Vec<u8>, Vec<u8>) {
        let (h, text) = (&self.hunks[k], self.meta[k].new_text.as_bytes());
        let (cs, ce) = (h.cur_bytes.start, h.cur_bytes.end);
        let base = self.baseline.as_bytes();
        // Gap before the hunk: current [lo, cs) is baseline [bs - (cs - lo), bs).
        let lo = k.checked_sub(1).map_or(0, |p| self.hunks[p].cur_bytes.end);
        let from = a.saturating_sub(turns::MAX_SLIDE).max(lo);
        let mut before = Vec::with_capacity(a - from);
        if from < cs {
            let bs = h.base_bytes.start;
            before.extend_from_slice(&base[bs - (cs - from)..bs]);
        }
        before.extend_from_slice(&text[from.max(cs) - cs..a - cs]);
        // Gap after it: current [ce, hi) is baseline [be, be + (hi - ce)).
        let hi = self
            .hunks
            .get(k + 1)
            .map_or(self.expected_len(), |n| n.cur_bytes.start);
        let to = end.saturating_add(turns::MAX_SLIDE).min(hi);
        let mut after = Vec::with_capacity(to.saturating_sub(end));
        after.extend_from_slice(&text[end - cs..to.min(ce).max(end) - cs]);
        if to > ce {
            let be = h.base_bytes.end;
            after.extend_from_slice(&base[be..be + (to - ce)]);
        }
        (before, after)
    }

    /// Follow-up of a completed undo or redo step of a recorded edit for the attribution: the
    /// turns of text that went away wait for the step to be reversed, and come back with it.
    fn turn_step(&mut self, claim: &records::Claim) {
        let Some(id) = claim.record else {
            return;
        };
        let range = claim.range.0..claim.range.1;
        match (claim.kind, claim.step) {
            // The agent's text is gone: only its own turn goes (others may share the hunk).
            (Kind::Agent, Step::Undo) => {
                if let Some(turn) = claim.turn {
                    self.park(id, &range, Some(turn));
                }
            }
            // A reject redone: the baseline is back.
            (Kind::Reject(_), Step::Redo) => {
                self.park(id, &range, None);
                self.drop_parked_extent(id, &(claim.range.0..claim.range.1));
            }
            // The agent's text is back (redo of its edit, undo or Alt+U of a reject).
            (Kind::Agent, Step::Redo) => self.unpark(id, &range),
            (Kind::Reject(_), Step::Undo) => {
                self.unpark(id, &range);
                self.restore_parked(id, &(claim.range.0..claim.range.1));
            }
            (_, Step::Apply) => {}
        }
    }

    /// Takes the closed interval `range` out of the attribution (of turn `only`, or of every
    /// turn) and keeps the turns it held under the record `id`.
    fn park(&mut self, id: u64, range: &Range<usize>, only: Option<u32>) {
        let turns = self.attribution.subtract(range, only);
        self.prune_near(range, None);
        let records = &self.records;
        self.parked
            .retain(|(r, _)| *r != id && records.contains(*r));
        if !turns.is_empty() {
            self.parked.push((id, turns));
        }
        self.refresh_turns_near(range);
    }

    /// Gives the turns kept under the record `id` back to the closed interval `range`.
    fn unpark(&mut self, id: u64, range: &Range<usize>) {
        if let Some(i) = self.parked.iter().position(|(r, _)| *r == id) {
            let (_, turns) = self.parked.remove(i);
            for t in turns {
                self.attribution.add(t, range.clone());
            }
            self.refresh_turns_near(range);
        }
    }

    /// Call for EVERY buffer change, before it is applied, with pre-edit byte offsets.
    /// Inside(h) for cur_bytes [s,e): (a<b && a<e && b>s) || (a==b && s<=a && a<e) || (s==e && a<s && b>s).
    /// Outside User/History edits are applied to the baseline at x - (net growth of hunks ending at or before x);
    /// Inside User/History edits mark the hunk user_touched (+ one UserEditedInHunk event per hunk per turn).
    /// Agent/Review/Reload never touch the baseline. A History edit within the restored range of the most recent
    /// unconsumed Reject is treated as Agent, so undo-of-reject re-opens the hunk with the same HunkId.
    /// Hunk ranges are shifted so classification stays valid until the next recompute.
    ///
    /// Buffer undo/redo is recognized more generally: History edits that exactly undo or redo one
    /// of our own recorded edits (an agent edit, a reject, a re-applied reject) count as Agent
    /// edits, and undoing an agent edit sends `UndoneByUser` (once per hunk per turn) instead of
    /// `UserEditedInHunk`. One undo the records may no longer cover (an agent deletion too large
    /// to record, or one recorded long ago) counts the same way: a History insertion, at a pure
    /// deletion, of exactly the deleted text.
    ///
    /// The user's own undo is recognized too: a History insertion right where the previous change
    /// (a User or History deletion or replacement folded into the baseline) removed text puts that
    /// text back into the baseline where it was, even at the start of a hunk or next to a pure
    /// deletion, so `dd` + `u` next to a hunk changes nothing. Any other History edit is classified
    /// like a User edit. The returned class always follows the formula above.
    pub fn on_edit(&mut self, e: &TextEdit, origin: Origin) -> EditClass {
        let (a, b) = (e.range.start, e.range.end);
        if a > b
            || b > self.expected_len()
            || !self.is_char_boundary(a)
            || !self.is_char_boundary(b)
        {
            // A change bypassed on_edit; `recompute` resynchronizes the hunks with the buffer.
            debug_assert!(false, "edit {:?} does not fit the reviewed text", e.range);
            return EditClass::Outside;
        }
        let (class, first) = self.classify(a, b);
        if a == b && e.text.is_empty() {
            return class;
        }
        self.generation += 1;
        // GTK's undo is last in, first out: a History insertion right where the previous change
        // folded a deletion into the baseline puts that text back.
        let own_undo = match self.folded_deletion.take() {
            Some((at, base_at)) if at == a && a == b && origin == Origin::History => Some(base_at),
            _ => None,
        };
        self.journal.track(a, b, e.text.len());
        self.attribution.shift(a, b, e.text.len());
        self.unknown.shift(a, b, e.text.len());
        let (mut claim, context) = self
            .records
            .observe(a, b, &e.text, origin, own_undo.is_none());
        if claim.is_none()
            && origin == Origin::History
            && own_undo.is_none()
            && self.reinserted_deletion(a, b, &e.text).is_some()
        {
            claim = Some(records::Claim {
                kind: records::Kind::Agent,
                step: records::Step::Undo,
                done: true,
                record: None,
                range: (a, a + e.text.len()),
                turn: None,
            });
        }
        // Before the model changes: segments read the text the edit replaces.
        let vanished = self.edit_segments(a, b, &e.text, origin, claim.as_ref(), class);
        let grown = offset(self.current_len, e.delta());
        let changed = a..a + e.text.len();
        if let Some(base_at) = own_undo
            && self.fold_back(a, base_at, &e.text)
        {
            self.current_len = grown;
            self.refresh_turns_near(&changed);
            return class;
        }
        match (claim, origin, class) {
            (None, Origin::User | Origin::History, EditClass::Outside) => {
                if let Some(at) = self.fold_into_baseline(first, a, b, &e.text) {
                    self.current_len = grown;
                    if a < b {
                        self.folded_deletion = Some((a, at));
                    }
                }
            }
            (None, Origin::User | Origin::History, EditClass::Inside(_)) => {
                self.current_len = grown;
                self.edit_inside(first, a, b, &e.text);
            }
            _ => {
                self.current_len = grown;
                let merged = self.edit_proposal(a, b, &e.text);
                if let Some(claim) = claim {
                    self.apply_claim(claim, &merged);
                }
                if let Origin::Agent { .. } = origin {
                    self.attribute_agent_edit(merged.index, a, &e.text, &merged.removed);
                    let range = a..a + e.text.len();
                    let turn = self.attr_turn;
                    self.records
                        .push_agent(range, merged.removed, e.text.clone(), context, turn);
                }
            }
        }
        if let Some(claim) = claim.filter(|c| c.done) {
            self.turn_step(&claim);
        }
        // The turn's own edits took each other back there: it changed nothing there, whatever the
        // hunk around holds (another turn's change it deleted and wrote again, say). Its
        // intervals there go, whole, unless another change of the turn is in them.
        if let Some((turn, r)) = vanished {
            let segments = &self.segments;
            let dropped = self.attribution.drop_turn_touching(turn, &r, |x| {
                segments
                    .touching(x.start, x.end)
                    .any(|i| segments.get(i).is_some_and(|s| s.turn == turn))
            });
            if let Some(dropped) = dropped {
                self.refresh_turns_near(&dropped);
            }
        }
        self.forget_unchanged_near(&changed);
        self.refresh_turns_near(&changed);
        if claim.is_some_and(|c| c.done) {
            // Records cut by this step's edit may have come back. They match the text when the
            // buffer's undo replays strictly in order; drop any that do not.
            let mut records = std::mem::take(&mut self.records);
            records.retain(|range, content| {
                self.current_text(range).is_some_and(|text| text == content)
            });
            self.records = records;
        }
        class
    }

    /// Recompute hunks: similar Myers line diff with an 8 ms timeout + iter_inline_changes for word spans.
    ///
    /// The line diff itself runs without a deadline (similar's Myers bounds its own work by the
    /// input size), so the hunks and their ids depend on the texts only, never on machine load;
    /// the 8 ms timeout applies to the word spans. A line whose changed words cover most of it gets
    /// no word spans: the whole line is the change.
    ///
    /// Hunks keep their [`HunkId`] when their texts are unchanged, `user_touched` carries over to
    /// the hunks that overlap a touched one, and a hunk re-opened by undoing a reject gets its old
    /// id back. The delta is relative to the previous `recompute`: hunks that appeared in between
    /// (from `on_edit`, `accept`, `undo_review_action`) count as added when they remain.
    pub fn recompute(&mut self, current: &str) -> HunkDelta {
        self.generation += 1;
        let raw = LineDiff::new(&self.baseline, current).hunks(true);
        let candidates: Vec<ids::Candidate> = raw
            .iter()
            .map(|h| ids::Candidate {
                cur: h.new_bytes.clone(),
                pair: pair_hash(
                    &self.baseline[h.old_bytes.clone()],
                    &current[h.new_bytes.clone()],
                ),
            })
            .collect();
        let inherited = ids::inherit(&self.hunks, &self.meta, current, &candidates);
        let mut hunks = Vec::with_capacity(raw.len());
        let mut meta = Vec::with_capacity(raw.len());
        for ((h, c), inherited) in raw.into_iter().zip(candidates).zip(inherited) {
            let old_text = self.baseline[h.old_bytes.clone()].to_owned();
            let new_text = current[h.new_bytes.clone()].to_owned();
            hunks.push(Hunk {
                id: hunk_id(&old_text, &new_text, inherited.occurrence),
                base_lines: h.old_lines,
                cur_lines: h.new_lines,
                cur_bytes: h.new_bytes,
                base_bytes: h.old_bytes,
                old_text,
                words: h.words,
                user_touched: inherited.user_touched,
                turns: Vec::new(),
            });
            meta.push(Meta {
                new_text,
                occurrence: inherited.occurrence,
                pair: Some(c.pair),
                edit_reported: inherited.edit_reported,
                undo_reported: inherited.undo_reported,
                reopen: None,
            });
        }
        for h in &mut hunks {
            h.turns = self.attribution.turns_touching(&h.cur_bytes);
        }
        // What meets no hunk any more is not a change any more.
        self.attribution.retain_touching(&hunks);
        self.unknown.retain_touching(&hunks);
        if hunks.is_empty() {
            self.segments.clear();
        } else {
            self.segments.drop_unchanged(current);
        }
        let records = &self.records;
        self.parked_segments.retain(|(id, _)| records.contains(*id));
        let delta = hunk_delta(&self.published, &self.hunks, &hunks);
        self.published = hunks.iter().map(|h| h.id).collect();
        self.hunks = hunks;
        self.meta = meta;
        self.current_len = current.len();
        self.forget_free_hints();
        delta
    }

    /// `baseline[base_lines] := current[cur_lines]`. No buffer change.
    pub fn accept(&mut self, id: HunkId, current: &str) -> Result<(), ReviewError> {
        let k = self.index_of(id)?;
        self.check(current, k..k + 1)?;
        self.generation += 1;
        let (region, event) = self.accept_at(k);
        self.events.push(event);
        self.journal.push(Entry::Accept(vec![region]));
        Ok(())
    }

    /// Returns the buffer edit that restores old_text; caller applies it with Origin::Review, then recomputes.
    ///
    /// The edit is trimmed to the bytes that differ. A `Rejected` event is queued, and the reject
    /// is journalled for [`undo_review_action`](Self::undo_review_action) and recorded so that
    /// buffer undo of the applied edit re-opens the hunk.
    pub fn reject(&mut self, id: HunkId, current: &str) -> Result<TextEdit, ReviewError> {
        let k = self.index_of(id)?;
        self.check(current, k..k + 1)?;
        self.generation += 1;
        self.records.begin_decision();
        let (edit, record, event) = self.reject_at(k);
        if let Some(record) = record {
            self.journal.push(Entry::Reject(vec![record]));
        }
        self.events.extend(event);
        Ok(edit)
    }

    /// Accepts every hunk: the baseline becomes `current`.
    pub fn accept_all(&mut self, current: &str) {
        if self.hunks.is_empty() {
            return;
        }
        self.generation += 1;
        if self.check(current, 0..self.hunks.len()).is_err() {
            // Out of sync with the buffer: still accept what the user sees.
            self.events
                .extend(self.hunks.iter().map(|h| ReviewEvent::Accepted {
                    lines: h.cur_lines.clone(),
                }));
            self.baseline = current.to_owned();
            self.current_len = current.len();
            self.hunks.clear();
            self.meta.clear();
            self.records.clear();
            self.attribution.clear();
            self.parked.clear();
            self.segments.clear();
            self.unknown.clear();
            self.parked_segments.clear();
            self.forget_free_hints();
            return;
        }
        let mut regions = Vec::with_capacity(self.hunks.len());
        let mut events = Vec::with_capacity(self.hunks.len());
        for k in (0..self.hunks.len()).rev() {
            let (region, event) = self.accept_at(k);
            regions.push(region);
            events.push(event);
        }
        events.reverse();
        self.events.extend(events);
        self.journal.push(Entry::Accept(regions));
    }

    /// Edits in descending offset order.
    ///
    /// Applying all of them (as one `Origin::Review` action) makes the current text equal the
    /// baseline. They are journalled as one action.
    pub fn reject_all(&mut self, current: &str) -> Vec<TextEdit> {
        if self.hunks.is_empty() {
            return Vec::new();
        }
        self.generation += 1;
        if self.check(current, 0..self.hunks.len()).is_err() {
            // Out of sync with the buffer: diff against the baseline instead (not undoable).
            self.events.extend(
                self.hunks
                    .iter()
                    .zip(&self.meta)
                    .map(|(h, m)| rejected(h, m)),
            );
            // The text becomes the baseline: nothing is attributed any more.
            self.attribution.clear();
            self.segments.clear();
            self.unknown.clear();
            return minimal_edits(current, &self.baseline);
        }
        self.records.begin_decision();
        let mut edits = Vec::new();
        let mut records = Vec::new();
        let mut events = Vec::new();
        for k in (0..self.hunks.len()).rev() {
            let (edit, record, event) = self.reject_at(k);
            if let Some(record) = record {
                edits.push(edit);
                records.push(record);
            }
            events.extend(event);
        }
        events.reverse();
        self.events.extend(events);
        if !records.is_empty() {
            self.journal.push(Entry::Reject(records));
        }
        edits
    }

    /// Alt+U: revert the most recent accept or reject (journal of 100).
    ///
    /// An accept is reverted in the baseline ([`UndoAction::Unaccepted`]); a reject returns the
    /// edits that re-apply the proposal (apply them as `Origin::Review`, one action). Call
    /// [`recompute`](Self::recompute) afterwards either way; the hunk comes back with its old id.
    /// Actions whose text has been edited since, or that buffer undo already reverted, cannot be
    /// reverted and are skipped. `None` when nothing is left to revert or `current` is stale.
    pub fn undo_review_action(&mut self, current: &str) -> Option<UndoAction> {
        if current.len() != self.expected_len() {
            return None;
        }
        while let Some(entry) = self.journal.pop() {
            if let Some(action) = self.undo_entry(entry, current) {
                return Some(action);
            }
        }
        None
    }

    /// Like [`undo_review_action`](Self::undo_review_action), but only ever the most recent
    /// accept or reject: when it cannot be reverted (its text was edited since, or buffer undo
    /// already reverted it), it is dropped from the journal and `None` is returned; older ones
    /// stay as they are. `None` too when nothing is journalled or `current` is stale.
    pub fn undo_last_review_action(&mut self, current: &str) -> Option<UndoAction> {
        if current.len() != self.expected_len() {
            return None;
        }
        let entry = self.journal.pop()?;
        self.undo_entry(entry, current)
    }

    /// Reverts one journalled decision, if it still can be.
    fn undo_entry(&mut self, entry: Entry, current: &str) -> Option<UndoAction> {
        self.generation += 1;
        match entry {
            Entry::Accept(regions) => self
                .unaccept(regions, current)
                .then_some(UndoAction::Unaccepted),
            Entry::Reject(records) => {
                let edits = self.records.reapply_edits(&records, current);
                (!edits.is_empty()).then_some(UndoAction::Edits(edits))
            }
        }
    }

    /// Takes the queued feedback events, oldest first.
    pub fn drain_events(&mut self) -> Vec<ReviewEvent> {
        std::mem::take(&mut self.events)
    }

    /// The current text equals the baseline: nothing is left to review.
    pub fn is_clean(&self) -> bool {
        self.hunks
            .iter()
            .zip(&self.meta)
            .all(|(h, m)| h.old_text == m.new_text)
    }

    /// The state to save: the baseline plus a hash of `current` (the text on disk), the turn
    /// attribution and what each turn replaced, and each hunk's id occurrence and `user_touched`
    /// flag.
    pub fn to_persisted(&self, path: PathBuf, current: &str) -> PersistedReview {
        PersistedReview {
            path,
            baseline: self.baseline.clone(),
            current_hash: fnv1a64(&[current.as_bytes()]),
            created_unix: self.created_unix,
            attribution: self
                .attribution
                .intervals()
                .map(|(turn, r)| TurnRange {
                    turn,
                    bytes: r.clone(),
                })
                .collect(),
            hunks: self
                .hunks
                .iter()
                .zip(&self.meta)
                .map(|(h, m)| PersistedHunk {
                    cur_bytes: h.cur_bytes.clone(),
                    occurrence: m.occurrence,
                    user_touched: h.user_touched,
                })
                .collect(),
            turn_edits: self
                .segments
                .iter()
                .map(|s| TurnEdit {
                    turn: s.turn,
                    bytes: s.range.clone(),
                    before: s.before.clone(),
                })
                .collect(),
            unknown_turn_edits: self
                .unknown
                .intervals()
                .map(|(turn, r)| TurnRange {
                    turn,
                    bytes: r.clone(),
                })
                .collect(),
        }
    }

    /// Restores only if fnv1a64(disk) == p.current_hash.
    ///
    /// The hunks are recomputed from the saved baseline. Their turns come from the saved
    /// attribution, and a hunk at the same range as a saved one takes its id occurrence (so it
    /// keeps its [`HunkId`]) and `user_touched` flag. The undo journal is not persisted, and
    /// agent edits are attributed to turn 0 until [`set_turn`](Self::set_turn).
    pub fn from_persisted(p: &PersistedReview, disk: &str) -> Option<Self> {
        if fnv1a64(&[disk.as_bytes()]) != p.current_hash {
            return None;
        }
        let mut review = Self::new(p.baseline.clone());
        review.created_unix = p.created_unix;
        let fits = |r: &Range<usize>| r.start <= r.end && r.end <= disk.len();
        for r in &p.attribution {
            if fits(&r.bytes) {
                review.attribution.add(r.turn, r.bytes.clone());
            }
        }
        for r in &p.unknown_turn_edits {
            if fits(&r.bytes) {
                review.unknown.add(r.turn, r.bytes.clone());
            }
        }
        for e in &p.turn_edits {
            match disk.get(e.bytes.clone()) {
                Some(text) if review.segments.is_free(&e.bytes) => {
                    review.segments.insert(Segment {
                        turn: e.turn,
                        range: e.bytes.clone(),
                        before: e.before.clone(),
                        replaced: segments::replaced(text, &e.before),
                    });
                }
                _ if fits(&e.bytes) => review.unknown.add(e.turn, e.bytes.clone()),
                _ => {}
            }
        }
        review.recompute(disk);
        review.restore_hunk_state(&p.hunks);
        Some(review)
    }

    /// Gives the hunks the saved occurrences and flags of saved hunks at the same ranges (see
    /// [`from_persisted`](Self::from_persisted)).
    fn restore_hunk_state(&mut self, saved: &[PersistedHunk]) {
        if saved.is_empty() {
            return;
        }
        let by_range: HashMap<(usize, usize), &PersistedHunk> = saved
            .iter()
            .map(|p| ((p.cur_bytes.start, p.cur_bytes.end), p))
            .collect();
        let mut taken: HashSet<HunkId> = HashSet::new();
        let mut wanted: Vec<Option<u64>> = Vec::with_capacity(self.hunks.len());
        for (h, m) in self.hunks.iter_mut().zip(&self.meta) {
            let saved = by_range.get(&(h.cur_bytes.start, h.cur_bytes.end));
            if let Some(p) = saved {
                h.user_touched = p.user_touched;
            }
            wanted.push(saved.map(|p| p.occurrence).filter(|o| *o != m.occurrence));
        }
        // Occurrences that stay keep their ids; a saved one is taken unless another hunk with the
        // same texts holds it (a damaged file).
        for (k, h) in self.hunks.iter().enumerate() {
            if wanted[k].is_none() {
                taken.insert(h.id);
            }
        }
        for ((h, m), wanted) in self.hunks.iter_mut().zip(&mut self.meta).zip(wanted) {
            let Some(o) = wanted else {
                continue;
            };
            // The saved occurrence, else the current one, else the smallest free one.
            let candidates = [o, m.occurrence].into_iter().chain(0..);
            for o in candidates {
                let id = hunk_id(&h.old_text, &m.new_text, o);
                if taken.insert(id) {
                    h.id = id;
                    m.occurrence = o;
                    break;
                }
            }
        }
        self.published = self.hunks.iter().map(|h| h.id).collect();
        self.forget_free_hints();
    }

    fn index_of(&self, id: HunkId) -> Result<usize, ReviewError> {
        self.hunks
            .iter()
            .position(|h| h.id == id)
            .ok_or(ReviewError::NoSuchHunk(id))
    }

    /// `current` matches the model: same length, and the hunks at `range` hold their recorded
    /// text.
    fn check(&self, current: &str, range: Range<usize>) -> Result<(), ReviewError> {
        let consistent = current.len() == self.expected_len()
            && self.hunks[range.clone()]
                .iter()
                .zip(&self.meta[range])
                .all(|(h, m)| current.get(h.cur_bytes.clone()) == Some(m.new_text.as_str()));
        consistent.then_some(()).ok_or(ReviewError::Stale)
    }

    /// Moves hunk `k` into the baseline and removes it.
    fn accept_at(&mut self, k: usize) -> (Accepted, ReviewEvent) {
        // What the turns changed around it is no change any more; Alt+U puts it back.
        let anchor = self.hunks[k].cur_bytes.clone();
        let parked = self.park_around(k, &anchor);
        let hunk = self.hunks.remove(k);
        let meta = self.meta.remove(k);
        self.forget_free_hints();
        self.baseline
            .replace_range(hunk.base_bytes.clone(), &meta.new_text);
        let delta = signed(meta.new_text.len()) - signed(hunk.old_text.len());
        let lines = i64::from(newlines(&meta.new_text)) - i64::from(newlines(&hunk.old_text));
        for h in &mut self.hunks[k..] {
            shift_bytes(&mut h.base_bytes, delta);
            shift_lines(&mut h.base_lines, lines);
        }
        // Undoing the agent's edits here is now the user's own change to the baseline.
        self.records.forget_touching(&hunk.cur_bytes);
        // The text is no change any more; Alt+U gives the turns back with the hunk.
        let turns = self.attribution.subtract(&hunk.cur_bytes, None);
        self.prune_near(&hunk.cur_bytes, None);
        self.refresh_turns_near(&hunk.cur_bytes);
        let event = ReviewEvent::Accepted {
            lines: hunk.cur_lines.clone(),
        };
        let region = Accepted {
            cur: hunk.cur_bytes,
            old_text: hunk.old_text,
            new_text: meta.new_text,
            occurrence: meta.occurrence,
            user_touched: hunk.user_touched,
            turns,
            parked,
            valid: true,
        };
        (region, event)
    }

    /// The edit rejecting hunk `k`, the record tracking it, and its event. A hunk whose texts are
    /// equal (a provisional leftover) needs no edit: it returns an empty one and nothing else.
    fn reject_at(&mut self, k: usize) -> (TextEdit, Option<u64>, Option<ReviewEvent>) {
        // The region goes back to the baseline: its turns wait for the reject to be undone.
        let region = self.hunks[k].cur_bytes.clone();
        let turns = self.attribution.subtract(&region, None);
        self.prune_near(&region, Some(k));
        self.refresh_turns_near(&region);
        let (h, m) = (&self.hunks[k], &self.meta[k]);
        let Some(edit) = trimmed_edit(h.cur_bytes.start, &m.new_text, &h.old_text) else {
            return (TextEdit::insert(h.cur_bytes.start, ""), None, None);
        };
        let proposal = m.new_text
            [edit.range.start - h.cur_bytes.start..edit.range.end - h.cur_bytes.start]
            .to_owned();
        let reopen = Reopen {
            pair: pair_hash(&h.old_text, &m.new_text),
            occurrence: m.occurrence,
            user_touched: h.user_touched,
        };
        let event = rejected(h, m);
        let record =
            self.records
                .push_reject(edit.range.clone(), proposal, edit.text.clone(), reopen);
        if !turns.is_empty() {
            self.parked.push((record, turns));
        }
        // The region goes back to the baseline: what the turns changed around it waits for the
        // reject to be undone.
        let parked = self.park_around(k, &edit.range);
        if !parked.is_empty() {
            self.parked_segments.push((record, parked));
        }
        (edit, Some(record), Some(event))
    }

    /// Reverts accepted regions in the baseline; `true` if any could be reverted.
    fn unaccept(&mut self, mut regions: Vec<Accepted>, current: &str) -> bool {
        regions.sort_by_key(|r| std::cmp::Reverse(r.cur.start));
        if regions.len() > 1 {
            // An Accept all of thousands of hunks (see `batch`).
            return self.unaccept_batch(regions, current);
        }
        self.unaccept_each(regions, current)
    }

    /// [`unaccept`](Self::unaccept) of `regions` (descending) one after the other.
    fn unaccept_each(&mut self, regions: Vec<Accepted>, current: &str) -> bool {
        let mut any = false;
        for region in regions {
            any |= self.unaccept_region(region, current).is_some();
        }
        any
    }

    /// Puts `old_text` back into the baseline under an accepted region and re-creates its hunk.
    /// Returns how far that moved the baseline after it (bytes and lines), `None` when the
    /// region can no longer be reverted.
    fn unaccept_region(&mut self, r: Accepted, current: &str) -> Option<(isize, i64)> {
        let (s, e) = (r.cur.start, r.cur.end);
        if !r.valid || current.get(s..e) != Some(r.new_text.as_str()) {
            return None;
        }
        // The region must still be unchanged text between hunks.
        let i = self.first_ending_after(s);
        if self.hunks.get(i).is_some_and(|h| h.cur_bytes.start < e) {
            return None;
        }
        let bs = crate::text::offset(s, -self.growth_before(i));
        if self.baseline.get(bs..bs + (e - s)) != Some(r.new_text.as_str()) {
            return None;
        }
        let (cur_line, base_line) = self.gap_lines(i, bs);
        self.baseline.replace_range(bs..bs + (e - s), &r.old_text);
        let delta = signed(r.old_text.len()) - signed(r.new_text.len());
        let lines = i64::from(newlines(&r.old_text)) - i64::from(newlines(&r.new_text));
        let occurrence = self.free_occurrence(&r.old_text, &r.new_text, Some(r.occurrence));
        let hunk = Hunk {
            id: hunk_id(&r.old_text, &r.new_text, occurrence),
            base_lines: crate::text::line_span(base_line, &r.old_text),
            cur_lines: crate::text::line_span(cur_line, &r.new_text),
            cur_bytes: s..e,
            base_bytes: bs..bs + r.old_text.len(),
            old_text: r.old_text,
            words: Vec::new(),
            user_touched: r.user_touched,
            turns: Vec::new(),
        };
        let meta = Meta {
            pair: Some(pair_hash(&hunk.old_text, &r.new_text)),
            new_text: r.new_text,
            occurrence,
            edit_reported: None,
            undo_reported: None,
            reopen: None,
        };
        if let Some(batch) = &mut self.batch {
            batch.merged(i, &[], hunk.id, 0);
        }
        self.hunks.insert(i, hunk);
        self.meta.insert(i, meta);
        for h in &mut self.hunks[i + 1..] {
            shift_bytes(&mut h.base_bytes, delta);
            shift_lines(&mut h.base_lines, lines);
        }
        for t in r.turns {
            self.attribution.add(t, s..e);
        }
        self.put_back(&r.parked, &(s..e));
        self.refresh_turns_near(&(s..e));
        Some((delta, lines))
    }
}

fn rejected(h: &Hunk, m: &Meta) -> ReviewEvent {
    ReviewEvent::Rejected {
        lines: h.cur_lines.clone(),
        proposed: m.new_text.clone(),
        kept: h.old_text.clone(),
    }
}

/// Which ids appeared, disappeared or changed since `published` (the previous recompute's ids).
/// `before` is the hunk list just before this recompute (positions already shifted by `on_edit`):
/// a published hunk is changed when its recomputed version differs from it.
fn hunk_delta(published: &[HunkId], before: &[Hunk], after: &[Hunk]) -> HunkDelta {
    let known: HashSet<HunkId> = published.iter().copied().collect();
    let previous: HashMap<HunkId, &Hunk> = before.iter().map(|h| (h.id, h)).collect();
    let current: HashSet<HunkId> = after.iter().map(|h| h.id).collect();
    let mut delta = HunkDelta::default();
    for h in after {
        if !known.contains(&h.id) {
            delta.added.push(h.id);
        } else if previous.get(&h.id) != Some(&h) {
            delta.changed.push(h.id);
        }
    }
    delta.removed = published
        .iter()
        .copied()
        .filter(|id| !current.contains(id))
        .collect();
    delta
}
