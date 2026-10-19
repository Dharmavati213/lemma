//! Exact per-turn inverses: what each agent turn's edits replaced, so that a turn's own changes can
//! be taken back without touching anything else ([`FileReview::reject_turn`]).
//!
//! A *segment* is a range of the current text that one turn changed, with the text it replaced
//! (`before`): putting `before` back in place of `current[range]` undoes that turn's change there,
//! and only that. The hunk labels ([`Hunk::turns`]) come from the attribution in `turns.rs`, which
//! is conservative; the segments say exactly what a turn changed. They follow the text:
//!
//! - an edit of turn t (or the buffer's undo or redo of one) is composed into the segments of t it
//!   meets or touches: the result's `before` is still the text from before turn t;
//! - a user edit inside a hunk that changes the text of one segment becomes part of it, as it
//!   becomes part of the hunk's proposal (rejecting the turn discards it, as rejecting the hunk
//!   does);
//! - any other edit that changes a segment's text drops it: the review then only knows that the
//!   turn changed something there (`FileReview::unknown`), and leaves that place to the user;
//! - like the attribution windows, an insertion or deletion is widened over every placement in
//!   equal text with the same result, so that the segment meets the line hunk wherever the diff put
//!   it (the line diff may place a deleted `"\nfoo"` as a deleted `"foo\n"` one byte further).
//!
//! Segments are sorted by position and their interiors are disjoint; an empty segment (where a turn
//! only deleted text, before widening) may sit on another one's boundary.
//!
//! [`FileReview::reject_turn`]: super::FileReview::reject_turn
//! [`Hunk::turns`]: crate::Hunk::turns

use super::turns::{MAX_SLIDE, deletion_window, insertion_window};
use crate::text::{common_prefix, common_suffix, shift_bytes, signed};
use lemma_core::fnv1a64;
use std::ops::Range;

/// Largest segment kept (its text plus `before`): a bigger one becomes unknown, so that edits
/// composed into it do not copy that much text each.
pub(super) const MAX_SEGMENT_BYTES: usize = 256 * 1024;

/// What one turn changed in one place (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Segment {
    pub turn: u32,
    /// Half-open range in the current text.
    pub range: Range<usize>,
    /// The text the range held before the turn changed it.
    pub before: String,
    /// The turn replaced or deleted text here, not only inserted (see [`replaced`]): when the
    /// segment is lost, what the turn replaced is no longer known.
    pub replaced: bool,
}

/// Whether a turn whose segment holds `text` and replaced `before` replaced or deleted text (not
/// only inserted, the widening left out).
pub(super) fn replaced(text: &str, before: &str) -> bool {
    let p = common_prefix(text, before);
    let q = common_suffix(&text[p..], &before[p..]);
    p + q < before.len()
}

/// Where a segment stands relative to an edit `[a, b)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Entirely before it (or ending at it): unchanged.
    Before,
    /// The edit changes its text (or, for an empty segment, the text around it).
    Hit,
    /// Entirely after it (or starting at its end): shifted.
    After,
}

fn fate(r: &Range<usize>, a: usize, b: usize) -> Fate {
    if r.is_empty() {
        let p = r.start;
        if p <= a {
            Fate::Before
        } else if p >= b {
            Fate::After
        } else {
            Fate::Hit
        }
    } else if r.end <= a {
        Fate::Before
    } else if r.start >= b {
        Fate::After
    } else {
        Fate::Hit
    }
}

/// Which segments an edit is composed with.
#[derive(Clone, Copy, Debug)]
pub(super) enum Pick {
    /// Every segment of this turn that the edit meets or touches.
    Turn(u32),
    /// The one segment the edit changes (at this index).
    At(usize),
}

/// What composing an edit did besides updating the segments.
#[derive(Debug, Default)]
pub(super) struct Composed {
    /// Segments of other turns whose text the edit changed (dropped; ranges after the edit).
    pub dropped: Vec<Segment>,
    /// The turn and place of a composed segment that became too big to keep (after the edit).
    pub too_big: Option<(u32, Range<usize>)>,
    /// The turn and place (after the edit) where the turn's edits now cancel out: the text there
    /// is what it was before the turn.
    pub vanished: Option<(u32, Range<usize>)>,
}

/// The segments of one review (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Segments {
    /// Sorted by `(range.start, range.end)`; interiors disjoint (so the ends are sorted too).
    list: Vec<Segment>,
}

impl Segments {
    pub fn iter(&self) -> std::slice::Iter<'_, Segment> {
        self.list.iter()
    }

    pub fn get(&self, i: usize) -> Option<&Segment> {
        self.list.get(i)
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// Index of the first segment ending at or after `x`.
    fn first_ending_from(&self, x: usize) -> usize {
        self.list.partition_point(|s| s.range.end < x)
    }

    /// Indices of the segments touching the closed interval `[lo, hi]`.
    pub fn touching(&self, lo: usize, hi: usize) -> Range<usize> {
        let first = self.first_ending_from(lo);
        let n = self.list[first..]
            .iter()
            .take_while(|s| s.range.start <= hi)
            .count();
        first..first + n
    }

    /// Indices of the segments whose text the edit `[a, b)` changes.
    pub fn hit_by(&self, a: usize, b: usize) -> Vec<usize> {
        self.touching(a, b)
            .filter(|&i| fate(&self.list[i].range, a, b) == Fate::Hit)
            .collect()
    }

    /// The hull of `[a, b)` and the segments `pick` composes with (see [`compose`]).
    ///
    /// [`compose`]: Self::compose
    pub fn hull(&self, pick: Pick, a: usize, b: usize) -> Range<usize> {
        self.picked(pick, a, b).into_iter().fold(a..b, |h, i| {
            let r = &self.list[i].range;
            h.start.min(r.start)..h.end.max(r.end)
        })
    }

    /// Indices (ascending) of the segments `pick` composes the edit `[a, b)` with.
    fn picked(&self, pick: Pick, a: usize, b: usize) -> Vec<usize> {
        match pick {
            Pick::Turn(turn) => self
                .touching(a, b)
                .filter(|&i| self.list[i].turn == turn)
                .collect(),
            Pick::At(i) => vec![i],
        }
    }

    /// Moves the segments through the edit `[a, b) → len bytes` that belongs to none of them:
    /// those whose text it changes are taken out and returned (with their ranges moved through
    /// the edit like attribution intervals, see `turns::shift`), the others are shifted.
    pub fn shift(&mut self, a: usize, b: usize, len: usize) -> Vec<Segment> {
        let delta = signed(len) - signed(b - a);
        let mut i = self.first_ending_from(a);
        let mut hit = Vec::new();
        while i < self.list.len() {
            match fate(&self.list[i].range, a, b) {
                Fate::Before => i += 1,
                Fate::Hit => {
                    let mut s = self.list.remove(i);
                    super::turns::shift(&mut s.range, a, b, len);
                    hit.push(s);
                }
                Fate::After => break,
            }
        }
        if delta != 0 {
            for s in &mut self.list[i..] {
                shift_bytes(&mut s.range, delta);
            }
        }
        hit
    }

    /// Composes the edit `[a, b) → text` with the segments `pick` selects (see the module docs).
    /// `ctx` is the current text before the edit from `ctx_start` on, covering their hull (see
    /// [`hull`](Self::hull)) and up to [`MAX_SLIDE`] bytes on each side of it (for the widening).
    /// The `turn` of the result is `pick`'s turn, or the picked segment's.
    pub fn compose(
        &mut self,
        pick: Pick,
        a: usize,
        b: usize,
        text: &str,
        ctx: &str,
        ctx_start: usize,
    ) -> Composed {
        let picked = self.picked(pick, a, b);
        let turn = match pick {
            Pick::Turn(t) => t,
            Pick::At(i) => self.list[i].turn,
        };
        let hull = self.hull(pick, a, b);
        let at = |x: usize| x - ctx_start;
        let hull_text = &ctx[at(hull.start)..at(hull.end)];
        // The text before the turn in the hull: the picked segments give theirs back.
        let mut before = String::with_capacity(hull_text.len());
        let mut pos = hull.start;
        for &i in &picked {
            let s = &self.list[i];
            before.push_str(&ctx[at(pos)..at(s.range.start)]);
            before.push_str(&s.before);
            pos = s.range.end;
        }
        before.push_str(&ctx[at(pos)..at(hull.end)]);
        // The hull's text after the edit.
        let mut after = String::with_capacity(hull_text.len() + text.len());
        after.push_str(&ctx[at(hull.start)..at(a)]);
        after.push_str(text);
        after.push_str(&ctx[at(b)..at(hull.end)]);
        for i in picked.into_iter().rev() {
            self.list.remove(i);
        }
        let mut out = Composed {
            dropped: self.shift(a, b, text.len()),
            ..Composed::default()
        };
        let start = hull.start;
        if before.len() + after.len() > MAX_SEGMENT_BYTES {
            out.too_big = Some((turn, start..start + after.len()));
            return out;
        }
        // Trimmed to what differs.
        let p = common_prefix(&after, &before);
        let q = common_suffix(&after[p..], &before[p..]);
        let (cur, old) = (&after[p..after.len() - q], &before[p..before.len() - q]);
        if cur.is_empty() && old.is_empty() {
            out.vanished = Some((turn, start..start + after.len()));
            return out;
        }
        let mut range = start + p..start + after.len() - q;
        let replaced = !old.is_empty();
        let mut old = old.to_owned();
        if cur.is_empty() || old.is_empty() {
            // The text around it after the edit, up to MAX_SLIDE bytes and never into another
            // segment.
            let (lo, hi) = self.free_around(&range);
            let left_ctx = {
                let from = ctx_start.max(range.start.saturating_sub(MAX_SLIDE)).max(lo);
                let mut s = String::new();
                if from < hull.start {
                    s.push_str(&ctx[at(from)..at(hull.start)]);
                }
                s.push_str(&after[from.max(start) - start..p]);
                s
            };
            let right_ctx = {
                let ctx_end = ctx_start + ctx.len();
                let tail = &after[after.len() - q..];
                let mut s = String::from(tail);
                if hull.end < ctx_end {
                    s.push_str(&ctx[at(hull.end)..]);
                }
                // In the text after the edit, `s` starts at `range.end`.
                let limit = hi.saturating_sub(range.end).min(MAX_SLIDE).min(s.len());
                let mut end = limit;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s
            };
            let window = if cur.is_empty() {
                deletion_window(
                    range.start,
                    old.as_bytes(),
                    left_ctx.as_bytes(),
                    right_ctx.as_bytes(),
                )
            } else {
                insertion_window(
                    range.start,
                    cur.as_bytes(),
                    left_ctx.as_bytes(),
                    right_ctx.as_bytes(),
                )
            };
            let mut left = range.start - window.start;
            while !left_ctx.is_char_boundary(left_ctx.len() - left) {
                left -= 1;
            }
            let mut right = window.end - range.end;
            while !right_ctx.is_char_boundary(right) {
                right -= 1;
            }
            old = [
                &left_ctx[left_ctx.len() - left..],
                old.as_str(),
                &right_ctx[..right],
            ]
            .concat();
            range = range.start - left..range.end + right;
        }
        // Empty segments of other turns that now lie inside it (at the edit's ends) can no longer
        // be placed.
        let inside: Vec<usize> = self
            .touching(range.start, range.end)
            .filter(|&i| {
                let r = &self.list[i].range;
                if r.is_empty() {
                    range.start < r.start && r.start < range.end
                } else {
                    r.start < range.end && range.start < r.end
                }
            })
            .collect();
        for i in inside.into_iter().rev() {
            out.dropped.push(self.list.remove(i));
        }
        self.insert(Segment {
            turn,
            range,
            before: old,
            replaced,
        });
        out
    }

    /// The free space around `range`: the end of the last segment before it and the start of the
    /// first one after it (positions of empty segments included).
    fn free_around(&self, range: &Range<usize>) -> (usize, usize) {
        // The ends are sorted like the starts.
        let i = self.list.partition_point(|s| s.range.start <= range.start);
        let lo = i
            .checked_sub(1)
            .map_or(0, |j| self.list[j].range.end.min(range.start));
        let i = self.list.partition_point(|s| s.range.start < range.end);
        let hi = self.list.get(i).map_or(usize::MAX, |s| s.range.start);
        (lo, hi)
    }

    /// Inserts `seg` at its place (it must not overlap any segment's interior).
    pub fn insert(&mut self, seg: Segment) {
        let key = (seg.range.start, seg.range.end);
        let i = self
            .list
            .partition_point(|s| (s.range.start, s.range.end) <= key);
        self.list.insert(i, seg);
    }

    /// Whether `range` could be inserted: it overlaps no segment's interior and no empty segment
    /// lies strictly inside it (and it is not inside a segment, if empty).
    pub fn is_free(&self, range: &Range<usize>) -> bool {
        self.touching(range.start, range.end).all(|i| {
            let r = &self.list[i].range;
            match (range.is_empty(), r.is_empty()) {
                (_, true) if !range.is_empty() => !(range.start < r.start && r.start < range.end),
                (true, false) => !(r.start < range.start && range.start < r.end),
                (true, true) => true,
                _ => !(r.start < range.end && range.start < r.end),
            }
        })
    }

    /// Takes out the segments lying within the closed interval `[lo, hi]`, and drops (returns
    /// separately) those reaching from inside it across its ends while meeting `core`'s interior.
    pub fn take_within(
        &mut self,
        lo: usize,
        hi: usize,
        core: &Range<usize>,
    ) -> (Vec<Segment>, Vec<Segment>) {
        let range = self.touching(lo, hi);
        let mut within = Vec::new();
        let mut crossing = Vec::new();
        let mut kept = Vec::new();
        for s in self.list.drain(range.clone()) {
            let r = &s.range;
            if lo <= r.start && r.end <= hi {
                within.push(s);
            } else if r.start < core.end.max(core.start + 1) && core.start < r.end {
                crossing.push(s);
            } else {
                kept.push(s);
            }
        }
        self.list.splice(range.start..range.start, kept);
        (within, crossing)
    }

    /// Drops segments whose text is the text they replaced (nothing changed there any more).
    pub fn drop_unchanged(&mut self, current: &str) {
        self.list
            .retain(|s| current.get(s.range.clone()) != Some(s.before.as_str()));
    }

    /// Keeps the segments for which `keep` holds.
    pub fn retain(&mut self, keep: impl FnMut(&Segment) -> bool) {
        self.list.retain(keep);
    }

    /// Renumbers the turns with `f`.
    pub fn remap(&mut self, mut f: impl FnMut(u32) -> u32) {
        for s in &mut self.list {
            s.turn = f(s.turn);
        }
    }
}

/// Segments and unknown places taken out by a decision, to be put back when it is undone. Their
/// positions are relative to an *anchor* range of the text as it was when they were taken
/// (`start` for positions, `end` for the extent's upper end); see `FileReview::park_around`.
#[derive(Clone, Debug, Default)]
pub(super) struct Parked {
    /// The segments, with their positions relative to the anchor's start, and a hash of their
    /// text (see [`text_hash`]).
    pub segments: Vec<(Segment, isize, isize, u64)>,
    /// Unknown places `(turn, start, end)` (closed), relative to the anchor's start.
    pub unknown: Vec<(u32, isize, isize)>,
    /// Attribution intervals of the turn a partial reject took its edits back from `(turn,
    /// start, end)` (closed), relative to the anchor's start.
    pub attribution: Vec<(u32, isize, isize)>,
    /// The region the decision covered: `[anchor.start + lo, anchor.end + hi]`.
    pub lo: isize,
    pub hi: isize,
}

impl Parked {
    /// Whether nothing was taken.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.unknown.is_empty() && self.attribution.is_empty()
    }

    /// Renumbers the turns with `f`.
    pub fn remap(&mut self, f: &mut impl FnMut(u32) -> u32) {
        for s in &mut self.segments {
            s.0.turn = f(s.0.turn);
        }
        for u in self.unknown.iter_mut().chain(&mut self.attribution) {
            u.0 = f(u.0);
        }
    }
}

/// `fnv1a64` of a segment's text, to check that it is unchanged when it is put back.
pub(super) fn text_hash(text: &str) -> u64 {
    fnv1a64(&[text.as_bytes()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(turn: u32, range: Range<usize>, before: &str) -> Segment {
        Segment {
            turn,
            range,
            before: before.to_owned(),
            replaced: !before.is_empty(),
        }
    }

    fn list(s: &Segments) -> Vec<(u32, Range<usize>, String)> {
        s.iter()
            .map(|s| (s.turn, s.range.clone(), s.before.clone()))
            .collect()
    }

    #[test]
    fn shifting_drops_what_an_edit_changes() {
        let mut s = Segments::default();
        s.insert(seg(1, 2..4, "x"));
        s.insert(seg(2, 6..6, "gone"));
        s.insert(seg(1, 8..9, ""));
        // An insertion at a segment's end or at an empty segment changes nothing of theirs.
        assert!(s.shift(4, 4, 2).is_empty());
        assert!(s.shift(8, 8, 1).is_empty());
        assert_eq!(
            list(&s),
            [
                (1, 2..4, "x".into()),
                (2, 8..8, "gone".into()),
                (1, 11..12, "".into())
            ]
        );
        // A deletion around an empty segment, an insertion inside a segment.
        let hit = s.shift(7, 9, 0);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].range, 7..7);
        let hit = s.shift(3, 3, 1);
        assert_eq!(hit[0].turn, 1);
        assert_eq!(list(&s), [(1, 10..11, "".into())]);
    }

    #[test]
    fn composing_keeps_the_text_before_the_turn() {
        // "abcdef": turn 1 replaces "cd" by "XYZ" → "abXYZef", then "YZe" by "" → "abXf".
        let mut s = Segments::default();
        let text = "abcdef";
        s.compose(Pick::Turn(1), 2, 4, "XYZ", text, 0);
        assert_eq!(list(&s), [(1, 2..5, "cd".into())]);
        let text = "abXYZef";
        let hull = s.hull(Pick::Turn(1), 3, 6);
        assert_eq!(hull, 2..6);
        s.compose(Pick::Turn(1), 3, 6, "", text, 0);
        assert_eq!(list(&s), [(1, 2..3, "cde".into())]);
        // An insertion at its end is not a change of its text.
        let text = "abXf";
        let out = s.compose(Pick::Turn(2), 3, 3, "!", text, 0);
        assert!(out.dropped.is_empty());
        assert_eq!(list(&s), [(1, 2..3, "cde".into()), (2, 3..4, "".into())]);
        // Turn 2 rewrites turn 1's text: turn 1's segment is dropped and its own grows.
        let text = "abX!f";
        let out = s.compose(Pick::Turn(2), 2, 3, "Q", text, 0);
        assert_eq!(out.dropped.len(), 1);
        assert_eq!(list(&s), [(2, 2..4, "X".into())]);
        // Taking its edits back leaves nothing.
        let text = "abQ!f";
        s.compose(Pick::Turn(2), 2, 4, "X", text, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn insertions_and_deletions_are_widened() {
        // "one\nfoo\ntwo": deleting "\nfoo" after "one" equals deleting "foo\n" after "one\n".
        let mut s = Segments::default();
        let text = "one\nfoo\ntwo\n";
        s.compose(Pick::Turn(1), 3, 7, "", text, 0);
        assert_eq!(list(&s), [(1, 3..4, "\nfoo\n".into())]);
        // An insertion is not widened into another segment.
        let text = "one\ntwo\n";
        s.compose(Pick::Turn(2), 4, 4, "two\n", text, 0);
        assert_eq!(
            list(&s),
            [(1, 3..4, "\nfoo\n".into()), (2, 4..12, "two\n".into())]
        );
    }
}
