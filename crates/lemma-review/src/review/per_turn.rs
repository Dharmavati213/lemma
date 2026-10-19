//! Deciding the changes of one agent turn ([`FileReview::accept_turn`],
//! [`FileReview::reject_turn`]), and keeping the segments that say exactly what each turn changed
//! (see [`segments`](super::segments)) up to date through edits and decisions.
//!
//! A turn's hunks (`Hunk::turns == [turn]`) are grouped into *clusters*: a hunk, the turn's
//! segments touching it, and every other hunk of the turn those reach into. The line diff pairs
//! changes by their texts, not by who made them, so a hunk of one turn may also hold another
//! change: with repeated lines (blank lines between paragraphs, `\item`) it may pair one turn's
//! insertion with a paragraph that is only shifted, or with another turn's deletion next to it, or
//! a reload may have changed a line next to the turn's. So a cluster is
//!
//! - *explained* when putting the turn's segments back gives the baseline's text: the hunks are
//!   exactly the turn's change and are decided as a whole;
//! - *partial* when the hunks hold more than the turn's change: rejecting the turn takes back only
//!   the turn's segments (the rest stays as a change to review). Keeping the turn keeps the hunks
//!   only when everything else they hold lies inside text the turn replaced (another turn's
//!   change that this turn deleted again, say), so that the baseline takes nothing but the
//!   turn's change; otherwise they are left to be decided one by one;
//! - *undecidable* when another turn's hunk is in the way or what the turn changed there is not
//!   known exactly: the hunks are left to be decided one by one.

use super::records::{Claim, Kind, Step};
use super::segments::{self, Composed, Parked, Pick, Segment};
use super::turns::MAX_SLIDE;
use super::{FileReview, Reopen, journal::Entry, pair_hash};
use crate::text::{common_prefix, common_suffix, line_span, newlines, signed, trimmed_edit};
use crate::{EditClass, ReviewEvent};
use lemma_core::{Origin, TextEdit};
use std::ops::Range;

/// How a cluster of a turn's hunks can be decided (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClusterKind {
    Explained,
    Partial,
    Undecidable,
}

/// Hunks of one turn decided together (see the module docs).
#[derive(Clone, Debug)]
pub(super) struct Cluster {
    /// Indices of its hunks (all of the turn alone), ascending.
    pub hunks: Vec<usize>,
    /// Indices of the turn's segments in it, ascending.
    pub segments: Vec<usize>,
    /// The hull of both (a range of the current text).
    pub hull: Range<usize>,
    pub kind: ClusterKind,
    /// Keeping its hunks keeps nothing but the turn's change (see the module docs).
    pub keepable: bool,
}

impl FileReview {
    /// Keeps every hunk of `turn` alone ([`hunks_of_turn`](Self::hunks_of_turn)) that is exactly
    /// that turn's change, as one decision: the baseline takes their text, `current` does not
    /// change, and one [`undo_review_action`](Self::undo_review_action) reverts them all.
    ///
    /// Returns how many of the turn's hunks were left alone, e.g. for a toast: the mixed ones
    /// ([`mixed_hunks_of_turn`](Self::mixed_hunks_of_turn)), and those that also hold another
    /// change the line diff paired with the turn's (see [`reject_turn`](Self::reject_turn)). Nothing
    /// happens when `current` is stale (call [`recompute`](Self::recompute) first).
    pub fn accept_turn(&mut self, turn: u32, current: &str) -> usize {
        let mixed = self.mixed_hunks_of_turn(turn);
        if self.check(current, 0..self.hunks.len()).is_err() {
            return mixed;
        }
        let mut picked: Vec<usize> = self
            .clusters(turn, current)
            .into_iter()
            .filter(|c| c.keepable)
            .flat_map(|c| c.hunks)
            .collect();
        picked.sort_unstable();
        let own = self.hunks.iter().filter(|h| h.turns == [turn]).count();
        self.accept_indices(&picked, current);
        mixed + own - picked.len()
    }

    /// The buffer edits taking back what turn `turn` changed in its own hunks, in descending
    /// order: apply them all as one `Origin::Review` action, as for
    /// [`reject_all`](Self::reject_all). They are journalled as one decision. Empty when `current`
    /// is stale (call [`recompute`](Self::recompute) first).
    ///
    /// A hunk of the turn alone that is exactly the turn's change is rejected as a whole. One
    /// that also holds another change (the line diff may pair the turn's new paragraph with a
    /// paragraph that only moved, or with another turn's deletion next to it, or with a reloaded
    /// change on the next line) gets only the turn's own edits taken back; the rest stays as a
    /// change to review. Mixed hunks ([`mixed_hunks_of_turn`](Self::mixed_hunks_of_turn)), and
    /// hunks where what the turn changed is no longer known exactly (another change cut into it),
    /// are never touched.
    ///
    /// After two turns, rejecting the second restores exactly the text after the first wherever
    /// the second did not edit the first one's hunks.
    pub fn reject_turn(&mut self, turn: u32, current: &str) -> Vec<TextEdit> {
        if self.check(current, 0..self.hunks.len()).is_err() {
            return Vec::new();
        }
        let clusters = self.clusters(turn, current);
        let mut whole: Vec<usize> = Vec::new();
        // (segment, the hunk it belongs to, the cluster's hull)
        let mut parts: Vec<(Segment, usize, Range<usize>)> = Vec::new();
        for c in &clusters {
            match c.kind {
                ClusterKind::Explained => whole.extend(&c.hunks),
                ClusterKind::Partial => {
                    for &i in &c.segments {
                        let Some(seg) = self.segments.get(i) else {
                            continue;
                        };
                        let k = c
                            .hunks
                            .iter()
                            .copied()
                            .find(|&k| {
                                let h = &self.hunks[k].cur_bytes;
                                seg.range.start <= h.end && h.start <= seg.range.end
                            })
                            .unwrap_or(c.hunks[0]);
                        parts.push((seg.clone(), k, c.hull.clone()));
                    }
                }
                ClusterKind::Undecidable => {}
            }
        }
        if whole.is_empty() && parts.is_empty() {
            return Vec::new();
        }
        self.generation += 1;
        self.records.begin_decision();
        let mut edits = Vec::new();
        let mut records = Vec::new();
        let mut events = Vec::new();
        // The turn's own edits in partial clusters first, so that the whole rejects below do not
        // take their segments along with the hunks' neighbourhoods.
        let mut taken: Vec<Range<usize>> = Vec::new();
        for (seg, k, hull) in parts.into_iter().rev() {
            self.segments
                .retain(|s| !(s.turn == seg.turn && s.range == seg.range));
            // The turn's attribution there goes, and comes back with the first edit's undo.
            let mut attribution = Vec::new();
            if !taken.contains(&hull) {
                attribution = self.attribution.take_touching(&hull, turn);
                let gone = attribution
                    .iter()
                    .fold(hull.clone(), |h, r| h.start.min(r.start)..h.end.max(r.end));
                self.refresh_turns_near(&gone);
                taken.push(hull);
            }
            let Some(text) = current.get(seg.range.clone()) else {
                continue;
            };
            let Some(edit) = trimmed_edit(seg.range.start, text, &seg.before) else {
                continue;
            };
            let (h, m) = (&self.hunks[k], &self.meta[k]);
            let reopen = Reopen {
                pair: pair_hash(&h.old_text, &m.new_text),
                occurrence: m.occurrence,
                user_touched: h.user_touched,
            };
            let proposal = current[edit.range.clone()].to_owned();
            let line = self.line_at(k, edit.range.start, current);
            events.push(ReviewEvent::Rejected {
                lines: line_span(line, &proposal),
                proposed: proposal.clone(),
                kept: edit.text.clone(),
            });
            let record =
                self.records
                    .push_reject(edit.range.clone(), proposal, edit.text.clone(), reopen);
            let rel = |x: usize| signed(x) - signed(edit.range.start);
            let parked = Parked {
                segments: vec![(
                    seg.clone(),
                    rel(seg.range.start),
                    rel(seg.range.end),
                    segments::text_hash(text),
                )],
                unknown: Vec::new(),
                attribution: attribution
                    .into_iter()
                    .map(|r| (turn, rel(r.start), rel(r.end)))
                    .collect(),
                lo: rel(seg.range.start),
                hi: signed(seg.range.end) - signed(edit.range.end),
            };
            self.parked_segments.push((record, parked));
            edits.push(edit);
            records.push(record);
        }
        whole.sort_unstable();
        for &k in whole.iter().rev() {
            let (edit, record, event) = self.reject_at(k);
            if let Some(record) = record {
                edits.push(edit);
                records.push(record);
            }
            events.extend(event);
        }
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start, e.range.end)));
        events.sort_by_key(|e| match e {
            ReviewEvent::Rejected { lines, .. } => lines.start,
            _ => 0,
        });
        self.events.extend(events);
        if !records.is_empty() {
            self.journal.push(Entry::Reject(records));
        }
        edits
    }

    /// The current line of byte `x`, counted from hunk `k`.
    fn line_at(&self, k: usize, x: usize, current: &str) -> u32 {
        let h = &self.hunks[k];
        let s = h.cur_bytes.start;
        if x >= s {
            h.cur_lines.start + current.get(s..x).map_or(0, newlines)
        } else {
            h.cur_lines
                .start
                .saturating_sub(current.get(x..s).map_or(0, newlines))
        }
    }

    /// The clusters of `turn`'s hunks (see the module docs); `current` must be consistent with
    /// the model.
    pub(super) fn clusters(&self, turn: u32, current: &str) -> Vec<Cluster> {
        let mut out = Vec::new();
        let mut seen = vec![false; self.hunks.len()];
        let mut seen_segment = vec![false; self.segments.len()];
        for k in 0..self.hunks.len() {
            if seen[k] || self.hunks[k].turns != [turn] {
                continue;
            }
            seen[k] = true;
            let mut hull = self.hunks[k].cur_bytes.clone();
            let mut hunks = vec![k];
            let mut segs: Vec<usize> = Vec::new();
            let mut blocked = false;
            loop {
                let mut grew = false;
                // The line diff may place a change at the start of the next line, or a line
                // further, when the line in between is the baseline's (another change made it
                // so): the turn's segments on the lines next to the hull belong to it too.
                // (Only segments there that belong to no other hunk.)
                let (lo, hi) = lines_around(current, &hull);
                for i in self.segments.touching(lo, hi) {
                    let Some(seg) = self.segments.get(i) else {
                        continue;
                    };
                    if seg.turn != turn || seen_segment[i] {
                        continue;
                    }
                    let r = &seg.range;
                    let near = r.start <= hull.end && hull.start <= r.end;
                    if !near
                        && (self.touches_other_hunk(r, &hunks)
                            || self.other_hunk_on_lines(lines_of(current, r), &hunks, turn))
                    {
                        continue;
                    }
                    seen_segment[i] = true;
                    segs.push(i);
                    hull = hull.start.min(r.start)..hull.end.max(r.end);
                    grew = true;
                }
                // Hunks reaching into the hull (a pure deletion strictly inside it).
                let first = self
                    .hunks
                    .partition_point(|h| h.cur_bytes.end <= hull.start);
                let mut j = first;
                while j < self.hunks.len() && self.hunks[j].cur_bytes.start < hull.end {
                    if !hunks.contains(&j) {
                        if self.hunks[j].turns == [turn] {
                            let r = &self.hunks[j].cur_bytes;
                            hull = hull.start.min(r.start)..hull.end.max(r.end);
                            hunks.push(j);
                            seen[j] = true;
                            grew = true;
                        } else {
                            blocked = true;
                        }
                    }
                    j += 1;
                }
                if !grew {
                    break;
                }
            }
            hunks.sort_unstable();
            segs.sort_unstable();
            let (kind, keepable) =
                if blocked || segs.is_empty() || self.unknown.turns_touching(&hull).contains(&turn)
                {
                    (ClusterKind::Undecidable, false)
                } else {
                    match self.explains(&segs, &hunks, &hull, current) {
                        Some(true) => (ClusterKind::Explained, true),
                        Some(false) => (
                            ClusterKind::Partial,
                            self.keepable(&segs, &hunks, &hull, current)
                                .unwrap_or(false),
                        ),
                        None => (ClusterKind::Undecidable, false),
                    }
                };
            out.push(Cluster {
                hunks,
                segments: segs,
                hull,
                kind,
                keepable,
            });
        }
        out
    }

    /// Whether the closed interval `r` touches a hunk other than `members`.
    fn touches_other_hunk(&self, r: &Range<usize>, members: &[usize]) -> bool {
        let first = self.hunks.partition_point(|h| h.cur_bytes.end < r.start);
        (first..self.hunks.len())
            .take_while(|&j| self.hunks[j].cur_bytes.start <= r.end)
            .any(|j| !members.contains(&j))
    }

    /// Whether a hunk other than `members` is on the lines `lines` (overlapping them, or a pure
    /// deletion at their start or end), or a hunk that also holds `turn` borders them (taking the
    /// turn's change there back would join that hunk).
    fn other_hunk_on_lines(&self, lines: Range<usize>, members: &[usize], turn: u32) -> bool {
        let first = self
            .hunks
            .partition_point(|h| h.cur_bytes.end < lines.start);
        (first..self.hunks.len())
            .take_while(|&j| self.hunks[j].cur_bytes.start <= lines.end)
            .filter(|j| !members.contains(j))
            .any(|j| {
                let h = &self.hunks[j];
                let r = &h.cur_bytes;
                r.is_empty()
                    || h.turns.contains(&turn)
                    || (r.start < lines.end && lines.start < r.end)
            })
    }

    /// Whether putting the segments `segs` back in `hull` gives the baseline's text there (the
    /// hunks `hunks`' old texts between unchanged text). `None` if a range does not fit `current`.
    fn explains(
        &self,
        segs: &[usize],
        hunks: &[usize],
        hull: &Range<usize>,
        current: &str,
    ) -> Option<bool> {
        let mut without = String::new();
        let mut pos = hull.start;
        for &i in segs {
            let s = self.segments.get(i)?;
            without.push_str(current.get(pos..s.range.start)?);
            without.push_str(&s.before);
            pos = s.range.end;
        }
        without.push_str(current.get(pos..hull.end)?);
        Some(without == self.base_text(hunks, hull, current)?)
    }

    /// The baseline's text of `hull`: the hunks `hunks`' old texts between unchanged text.
    fn base_text(&self, hunks: &[usize], hull: &Range<usize>, current: &str) -> Option<String> {
        let mut base = String::new();
        let mut pos = hull.start;
        for &k in hunks {
            let h = &self.hunks[k];
            base.push_str(current.get(pos..h.cur_bytes.start)?);
            base.push_str(&h.old_text);
            pos = h.cur_bytes.end;
        }
        base.push_str(current.get(pos..hull.end)?);
        Some(base)
    }

    /// Whether the baseline's text of `hull` differs from the current one only inside text the
    /// segments `segs` replaced: then keeping the hunks keeps nothing but the turn's change (what
    /// the turn replaced may hold another turn's change, which is gone with it). The segments'
    /// widening is left out (it replaced nothing).
    fn keepable(
        &self,
        segs: &[usize],
        hunks: &[usize],
        hull: &Range<usize>,
        current: &str,
    ) -> Option<bool> {
        // The current text outside the replaced parts, as literal pieces; each replaced part
        // (non-empty before) matches anything in the baseline, an inserted one nothing.
        let mut pieces: Vec<String> = vec![String::new()];
        let mut pos = hull.start;
        for &i in segs {
            let s = self.segments.get(i)?;
            let text = current.get(s.range.clone())?;
            let p = common_prefix(text, &s.before);
            let q = common_suffix(&text[p..], &s.before[p..]);
            let core = s.range.start + p..s.range.end - q;
            pieces.last_mut()?.push_str(current.get(pos..core.start)?);
            if p + q < s.before.len() {
                pieces.push(String::new());
            }
            pos = core.end;
        }
        pieces.last_mut()?.push_str(current.get(pos..hull.end)?);
        Some(wildcard_match(
            &self.base_text(hunks, hull, current)?,
            &pieces,
        ))
    }

    // ----- keeping the segments -----

    /// Updates the segments for the edit `[a, b) → text` (see the `segments` module docs); called
    /// by `on_edit` before the model changes. Segments the edit cut into become unknown places.
    /// Returns the turn and place (after the edit) where an agent edit took back what its own
    /// turn wrote, so that the turn changed nothing there: `on_edit` takes the turn's attribution
    /// there away once the edit is attributed.
    pub(super) fn edit_segments(
        &mut self,
        a: usize,
        b: usize,
        text: &str,
        origin: Origin,
        claim: Option<&Claim>,
        class: EditClass,
    ) -> Option<(u32, Range<usize>)> {
        let agent_step = claim.is_some_and(|c| c.kind == Kind::Agent);
        if self.segments.is_empty() && !matches!(origin, Origin::Agent { .. }) && !agent_step {
            return None;
        }
        let pick = match (origin, claim) {
            (Origin::Agent { .. }, _) => Some(Pick::Turn(self.attr_turn)),
            // Buffer undo or redo of an agent edit: that turn's edit. An undo only takes back
            // what a segment knows (without one, it would describe the edit backwards).
            (Origin::History | Origin::Review, Some(c)) if c.kind == Kind::Agent => match c.turn {
                Some(t) if c.step != Step::Undo || self.has_segment_touching(t, a, b) => {
                    Some(Pick::Turn(t))
                }
                Some(_) => None,
                // Recognized without a record: the turn of the segment it undoes.
                None => self.only_turn_touching(a, b).map(Pick::Turn),
            },
            // Typing inside a hunk becomes part of the one change it edits.
            (Origin::User | Origin::History, None) if matches!(class, EditClass::Inside(_)) => {
                match self.segments.hit_by(a, b)[..] {
                    [i] => Some(Pick::At(i)),
                    _ => None,
                }
            }
            _ => None,
        };
        let composed = match pick {
            Some(pick) => {
                let hull = self.segments.hull(pick, a, b);
                match self.text_around(&hull) {
                    Some((ctx, start)) => self.segments.compose(pick, a, b, text, &ctx, start),
                    None => Composed {
                        dropped: self.segments.shift(a, b, text.len()),
                        ..Composed::default()
                    },
                }
            }
            None => Composed {
                dropped: self.segments.shift(a, b, text.len()),
                ..Composed::default()
            },
        };
        for s in composed.dropped.into_iter().filter(|s| s.replaced) {
            self.unknown.add(s.turn, s.range);
        }
        if let Some((turn, r)) = composed.too_big {
            self.unknown.add(turn, r);
        }
        // The buffer's undo of an agent edit keeps its turn for the redo (see `turn_step`).
        composed
            .vanished
            .filter(|_| matches!(origin, Origin::Agent { .. }))
    }

    /// Whether a segment of `turn` touches `[a, b]`.
    pub(super) fn has_segment_touching(&self, turn: u32, a: usize, b: usize) -> bool {
        self.segments
            .touching(a, b)
            .any(|i| self.segments.get(i).is_some_and(|s| s.turn == turn))
    }

    /// The turn of the segments touching `[a, b]`, if they are all of one turn.
    fn only_turn_touching(&self, a: usize, b: usize) -> Option<u32> {
        let mut turns = self
            .segments
            .touching(a, b)
            .filter_map(|i| self.segments.get(i).map(|s| s.turn));
        let first = turns.next()?;
        turns.all(|t| t == first).then_some(first)
    }

    /// The current text of `range` and up to [`MAX_SLIDE`] bytes around it, with its start.
    fn text_around(&self, range: &Range<usize>) -> Option<(String, usize)> {
        let len = self.expected_len();
        let mut lo = range.start.saturating_sub(MAX_SLIDE);
        while lo > 0 && !self.is_char_boundary(lo) {
            lo -= 1;
        }
        let mut hi = range.end.saturating_add(MAX_SLIDE).min(len);
        while hi < len && !self.is_char_boundary(hi) {
            hi += 1;
        }
        self.current_text(&(lo..hi)).map(|t| (t, lo))
    }

    /// Takes the segments and unknown places around hunk `k` (from the end of the previous hunk
    /// to the start of the next) out, for a decision on it; positions relative to `anchor`.
    pub(super) fn park_around(&mut self, k: usize, anchor: &Range<usize>) -> Parked {
        let lo = k.checked_sub(1).map_or(0, |p| self.hunks[p].cur_bytes.end);
        let hi = self
            .hunks
            .get(k + 1)
            .map_or(self.expected_len(), |n| n.cur_bytes.start);
        let core = self.hunks[k].cur_bytes.clone();
        let (within, crossing) = self.segments.take_within(lo, hi, &core);
        for s in crossing.into_iter().filter(|s| s.replaced) {
            self.unknown.add(s.turn, s.range);
        }
        let rel = |x: usize| signed(x) - signed(anchor.start);
        let segments = within
            .into_iter()
            .map(|s| {
                let hash = self
                    .current_text(&s.range)
                    .map_or(0, |t| segments::text_hash(&t));
                let (start, end) = (rel(s.range.start), rel(s.range.end));
                (s, start, end, hash)
            })
            .collect();
        let unknown = self
            .unknown
            .extract(&(lo..hi))
            .into_iter()
            .map(|(t, r)| (t, rel(r.start), rel(r.end)))
            .collect();
        Parked {
            segments,
            unknown,
            attribution: Vec::new(),
            lo: rel(lo),
            hi: signed(hi) - signed(anchor.end),
        }
    }

    /// Puts parked segments and unknown places back, relative to `anchor`. A segment whose text
    /// changed since, or whose place another one took, becomes an unknown place.
    pub(super) fn put_back(&mut self, parked: &Parked, anchor: &Range<usize>) {
        let len = self.expected_len();
        let abs = |x: isize| usize::try_from(signed(anchor.start) + x).ok();
        for (seg, s, e, hash) in &parked.segments {
            let (Some(s), Some(e)) = (abs(*s), abs(*e)) else {
                continue;
            };
            if s > e || e > len {
                continue;
            }
            let range = s..e;
            let present = self.segments.touching(s, e).any(|i| {
                self.segments.get(i).is_some_and(|x| {
                    x.turn == seg.turn && x.range == range && x.before == seg.before
                })
            });
            if present {
                continue;
            }
            let unchanged = self
                .current_text(&range)
                .is_some_and(|t| segments::text_hash(&t) == *hash);
            if unchanged && self.segments.is_free(&range) {
                self.segments.insert(Segment {
                    range,
                    ..seg.clone()
                });
            } else if seg.replaced {
                self.unknown.add(seg.turn, range);
            }
        }
        let mut touched: Option<Range<usize>> = None;
        for &(turn, s, e) in &parked.unknown {
            if let (Some(s), Some(e)) = (abs(s), abs(e))
                && s <= e
                && e <= len
            {
                self.unknown.add(turn, s..e);
            }
        }
        for &(turn, s, e) in &parked.attribution {
            if let (Some(s), Some(e)) = (abs(s), abs(e))
                && s <= e
                && e <= len
            {
                self.attribution.add(turn, s..e);
                touched = Some(touched.map_or(s..e, |t| t.start.min(s)..t.end.max(e)));
            }
        }
        if let Some(r) = touched {
            self.refresh_turns_near(&r);
        }
    }

    /// The reject recorded as `id` was undone (its proposal is back at `anchor`): what it took
    /// out comes back. The parked copy stays, for the next time.
    pub(super) fn restore_parked(&mut self, id: u64, anchor: &Range<usize>) {
        if let Some((_, parked)) = self.parked_segments.iter().find(|(r, _)| *r == id) {
            let parked = parked.clone();
            self.put_back(&parked, anchor);
        }
    }

    /// The reject recorded as `id` was redone (the baseline's text is back at `anchor`): the
    /// segments and unknown places around it describe no change any more.
    pub(super) fn drop_parked_extent(&mut self, id: u64, anchor: &Range<usize>) {
        let Some((_, parked)) = self.parked_segments.iter().find(|(r, _)| *r == id) else {
            return;
        };
        let at = |base: usize, x: isize| usize::try_from(signed(base) + x).unwrap_or(0);
        let lo = at(anchor.start, parked.lo);
        let hi = at(anchor.end, parked.hi).min(self.expected_len());
        if lo > hi {
            return;
        }
        let (_, crossing) = self.segments.take_within(lo, hi, anchor);
        for s in crossing.into_iter().filter(|s| s.replaced) {
            self.unknown.add(s.turn, s.range);
        }
        self.unknown.extract(&(lo..hi));
        if !parked.attribution.is_empty() {
            let turns: Vec<u32> = parked.attribution.iter().map(|a| a.0).collect();
            let mut gone = lo..hi;
            for t in turns {
                for r in self.attribution.take_touching(&(lo..hi), t) {
                    gone = gone.start.min(r.start)..gone.end.max(r.end);
                }
            }
            self.refresh_turns_near(&gone);
        }
    }
}

/// Whether `text` is `pieces[0]`, then anything, then `pieces[1]`, … then `pieces[n]` (literal
/// pieces separated by wildcards; one piece: equality).
fn wildcard_match(text: &str, pieces: &[String]) -> bool {
    let Some((first, rest)) = pieces.split_first() else {
        return text.is_empty();
    };
    let Some(rest_text) = text.strip_prefix(first.as_str()) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return rest_text.is_empty();
    };
    let Some(mut inner) = rest_text.strip_suffix(last.as_str()) else {
        return false;
    };
    for piece in middle {
        match inner.find(piece.as_str()) {
            Some(at) => inner = &inner[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// From the start of the line holding the byte before `range` (the line before, when `range`
/// starts at a line start) to the end of the line holding `range`'s end.
fn lines_around(text: &str, range: &Range<usize>) -> (usize, usize) {
    let bytes = text.as_bytes();
    let start = range.start.min(bytes.len());
    let before = start.saturating_sub(1);
    let lo = bytes[..before]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let end = range.end.min(bytes.len());
    let hi = bytes[end..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |i| end + i + 1);
    (lo, hi)
}

/// The lines `range` is on: from the start of its first line to the end of its last (a non-empty
/// range ending at a line start ends there).
fn lines_of(text: &str, range: &Range<usize>) -> Range<usize> {
    let bytes = text.as_bytes();
    let (start, end) = (range.start.min(bytes.len()), range.end.min(bytes.len()));
    let lo = bytes[..start]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let at_line_start = end > start && bytes[end - 1] == b'\n';
    let hi = if at_line_start {
        end
    } else {
        bytes[end..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(bytes.len(), |i| end + i + 1)
    };
    lo..hi
}

#[cfg(test)]
mod tests {
    use super::{lines_around, lines_of, wildcard_match};

    #[test]
    fn line_ranges() {
        let text = "ab\ncd\nef";
        // A pure deletion at the start of line 1 is next to line 0 too.
        assert_eq!(lines_around(text, &(3..3)), (0, 6));
        assert_eq!(lines_around(text, &(4..7)), (3, 8));
        assert_eq!(lines_around(text, &(0..1)), (0, 3));
        assert_eq!(lines_of(text, &(4..5)), 3..6);
        // A range ending with its line's newline ends there.
        assert_eq!(lines_of(text, &(4..6)), 3..6);
        assert_eq!(lines_of(text, &(1..4)), 0..6);
        assert_eq!(lines_of(text, &(7..8)), 6..8);
    }

    #[test]
    fn wildcards() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wildcard_match("abc", &p(&["abc"])));
        assert!(!wildcard_match("abcd", &p(&["abc"])));
        assert!(wildcard_match("a-x-b", &p(&["a-", "-b"])));
        assert!(wildcard_match("ab", &p(&["a", "b"])));
        assert!(!wildcard_match("ab", &p(&["ab", "b"])));
        assert!(wildcard_match("a1m2b", &p(&["a", "m", "b"])));
        assert!(!wildcard_match("amb", &p(&["a", "x", "b"])));
        assert!(wildcard_match("", &p(&["", ""])));
    }
}
