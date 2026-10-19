//! Per-turn attribution: which agent turn wrote which part of the current text.
//!
//! Every agent edit records its turn and the range it wrote, as a **closed** interval
//! `[start, end]` of the current text (`Range::start..Range::end`, both ends included as
//! positions): a pure deletion is the point where the text went, and an insertion or deletion that
//! could have been placed elsewhere in equal text (`"\nX"` before a `'\n'` is `"X\n"` after it) is
//! widened to every equivalent placement, so it meets the line hunk the diff makes of it wherever
//! the diff put it. A hunk belongs to the turns whose intervals meet its current range (also
//! closed, so a pure deletion hunk meets the intervals at its point).
//!
//! The intervals move with the text like hunk ranges do ([`shift`]): an edit that covers one
//! leaves the part that survives, and a point where it went when nothing does, so a turn whose
//! text another edit replaced still meets the hunk that edit is in. Deciding a hunk takes its
//! region out of every turn ([`Attribution::subtract`]); the review keeps the turns it took so
//! that undoing the decision can put them back. After each recompute, intervals that meet no hunk
//! are dropped ([`Attribution::retain_touching`]): what they described is no longer a change.

use crate::Hunk;
use std::ops::Range;

/// Most bytes an insertion or deletion is widened by in each direction (see the module docs).
pub(super) const MAX_SLIDE: usize = 256;

/// Whether the closed intervals `x` and `y` share a position.
#[cfg(test)]
pub(super) fn touch(x: &Range<usize>, y: &Range<usize>) -> bool {
    x.start <= y.end && y.start <= x.end
}

/// Moves the closed interval `r` through the edit `[a, b) → len bytes` (pre-edit offsets).
///
/// Intervals before the edit stay, intervals after it shift. An interval the edit meets keeps
/// the part outside the replaced text (its ends at the edit snap to the new text's ends, so it
/// still meets the new text); one the edit covers entirely becomes the point `a`.
pub(super) fn shift(r: &mut Range<usize>, a: usize, b: usize, len: usize) {
    let (s, e) = (r.start, r.end);
    if s == e {
        // A point: text inserted at it goes after it, one inside the replaced text goes to the
        // start of the new text, one at its end to the end of the new text.
        let p = if s <= a {
            s
        } else if s >= b {
            s - (b - a) + len
        } else {
            a
        };
        *r = p..p;
        return;
    }
    if e < a {
        return;
    }
    if s > b {
        *r = s - (b - a) + len..e - (b - a) + len;
        return;
    }
    let start = if s < a { s } else { a + len };
    let end = if e > b { e - (b - a) + len } else { a };
    *r = if start <= end { start..end } else { a..a };
}

/// Every turn's closed intervals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Attribution {
    /// Sorted by turn, no empty lists. Each list is sorted, and its intervals are disjoint and do
    /// not touch (touching intervals of one turn are merged).
    turns: Vec<(u32, Vec<Range<usize>>)>,
}

impl Attribution {
    /// Every `(turn, interval)`, by turn and position.
    pub fn intervals(&self) -> impl Iterator<Item = (u32, &Range<usize>)> {
        self.turns
            .iter()
            .flat_map(|(t, list)| list.iter().map(move |r| (*t, r)))
    }

    pub fn clear(&mut self) {
        self.turns.clear();
    }

    /// Attributes the closed interval `r` to `turn` too.
    pub fn add(&mut self, turn: u32, r: Range<usize>) {
        let list = match self.turns.binary_search_by_key(&turn, |(t, _)| *t) {
            Ok(i) => &mut self.turns[i].1,
            Err(i) => {
                self.turns.insert(i, (turn, Vec::new()));
                &mut self.turns[i].1
            }
        };
        // The intervals `r` touches are merged into it.
        let first = list.partition_point(|x| x.end < r.start);
        let last = first
            + list[first..]
                .iter()
                .take_while(|x| x.start <= r.end)
                .count();
        let merged = list[first..last]
            .iter()
            .fold(r, |m, x| m.start.min(x.start)..m.end.max(x.end));
        list.splice(first..last, [merged]);
    }

    /// Moves every interval through the edit `[a, b) → len` (see [`shift`]).
    pub fn shift(&mut self, a: usize, b: usize, len: usize) {
        for (_, list) in &mut self.turns {
            // [first, after) meet the edit; the rest lie after it and just move.
            let first = list.partition_point(|x| x.end < a);
            let after = first + list[first..].partition_point(|x| x.start <= b);
            if len != b - a {
                for x in &mut list[after..] {
                    *x = x.start - (b - a) + len..x.end - (b - a) + len;
                }
            }
            if first < after {
                for x in &mut list[first..after] {
                    shift(x, a, b, len);
                }
                normalize(list);
            }
        }
    }

    /// The turns with an interval touching the closed interval `r`, ascending.
    pub fn turns_touching(&self, r: &Range<usize>) -> Vec<u32> {
        self.turns
            .iter()
            .filter(|(_, list)| touches_any(list, r))
            .map(|(t, _)| *t)
            .collect()
    }

    /// Takes the closed interval `r` out of the intervals of `turn` (of every turn with `None`):
    /// intervals inside it go, intervals reaching into it keep their parts outside it. Returns
    /// the turns that had an interval touching `r`, ascending.
    pub fn subtract(&mut self, r: &Range<usize>, turn: Option<u32>) -> Vec<u32> {
        let mut out = Vec::new();
        for (t, list) in &mut self.turns {
            if turn.is_some_and(|only| only != *t) || !touches_any(list, r) {
                continue;
            }
            out.push(*t);
            let first = list.partition_point(|x| x.end < r.start);
            let last = first
                + list[first..]
                    .iter()
                    .take_while(|x| x.start <= r.end)
                    .count();
            let mut pieces = Vec::with_capacity(2);
            if let Some(x) = list
                .get(first)
                .filter(|x| x.start < r.start && first < last)
            {
                pieces.push(x.start..r.start);
            }
            if let Some(x) = last
                .checked_sub(1)
                .and_then(|l| list.get(l))
                .filter(|x| x.end > r.end && first < last)
            {
                pieces.push(r.end..x.end);
            }
            let rejoin = pieces.len() == 2;
            list.splice(first..last, pieces);
            if rejoin {
                // Taking a point out of an interval leaves it whole (it is closed).
                normalize(list);
            }
        }
        self.turns.retain(|(_, list)| !list.is_empty());
        out
    }

    /// Takes the parts of every turn's intervals that lie in the closed interval `r` out, and
    /// returns them; the parts outside `r` stay.
    pub fn extract(&mut self, r: &Range<usize>) -> Vec<(u32, Range<usize>)> {
        let mut out = Vec::new();
        for (t, list) in &self.turns {
            let first = list.partition_point(|x| x.end < r.start);
            for x in list[first..].iter().take_while(|x| x.start <= r.end) {
                out.push((*t, x.start.max(r.start)..x.end.min(r.end)));
            }
        }
        if !out.is_empty() {
            self.subtract(r, None);
        }
        out
    }

    /// Drops the intervals of `turn` touching the closed interval `r` for which `keep` does not
    /// hold, whole; returns the hull of those dropped, if any.
    pub fn drop_turn_touching(
        &mut self,
        turn: u32,
        r: &Range<usize>,
        keep: impl Fn(&Range<usize>) -> bool,
    ) -> Option<Range<usize>> {
        let i = self.turns.binary_search_by_key(&turn, |(t, _)| *t).ok()?;
        let list = &mut self.turns[i].1;
        let first = list.partition_point(|x| x.end < r.start);
        let mut dropped: Option<Range<usize>> = None;
        let mut j = first;
        while j < list.len() && list[j].start <= r.end {
            if keep(&list[j]) {
                j += 1;
            } else {
                let x = list.remove(j);
                dropped =
                    Some(dropped.map_or(x.clone(), |d| d.start.min(x.start)..d.end.max(x.end)));
            }
        }
        self.turns.retain(|(_, list)| !list.is_empty());
        dropped
    }

    /// Takes the intervals of `turn` touching the closed interval `r` out, whole, and returns
    /// them.
    pub fn take_touching(&mut self, r: &Range<usize>, turn: u32) -> Vec<Range<usize>> {
        let Ok(i) = self.turns.binary_search_by_key(&turn, |(t, _)| *t) else {
            return Vec::new();
        };
        let list = &mut self.turns[i].1;
        let first = list.partition_point(|x| x.end < r.start);
        let n = list[first..]
            .iter()
            .take_while(|x| x.start <= r.end)
            .count();
        let out: Vec<Range<usize>> = list.drain(first..first + n).collect();
        self.turns.retain(|(_, list)| !list.is_empty());
        out
    }

    /// Keeps only the intervals touching some hunk (hunks sorted and disjoint, as
    /// `FileReview::hunks` are).
    pub fn retain_touching(&mut self, hunks: &[Hunk]) {
        for (_, list) in &mut self.turns {
            list.retain(|x| {
                let i = hunks.partition_point(|h| h.cur_bytes.end < x.start);
                hunks.get(i).is_some_and(|h| h.cur_bytes.start <= x.end)
            });
        }
        self.turns.retain(|(_, list)| !list.is_empty());
    }

    /// Keeps, of the intervals touching the closed interval `r`, those for which `keep` holds.
    /// Returns the hull of the intervals dropped, if any.
    pub fn retain_near(
        &mut self,
        r: &Range<usize>,
        keep: impl Fn(&Range<usize>) -> bool,
    ) -> Option<Range<usize>> {
        let mut dropped: Option<Range<usize>> = None;
        for (_, list) in &mut self.turns {
            let first = list.partition_point(|x| x.end < r.start);
            let last = first
                + list[first..]
                    .iter()
                    .take_while(|x| x.start <= r.end)
                    .count();
            let mut i = first;
            let mut end = last;
            while i < end {
                if keep(&list[i]) {
                    i += 1;
                } else {
                    let x = list.remove(i);
                    end -= 1;
                    dropped = Some(match dropped {
                        Some(d) => d.start.min(x.start)..d.end.max(x.end),
                        None => x,
                    });
                }
            }
        }
        self.turns.retain(|(_, list)| !list.is_empty());
        dropped
    }

    /// Renumbers the turns with `f` (turns mapped to the same number are merged).
    pub fn remap(&mut self, mut f: impl FnMut(u32) -> u32) {
        let old = std::mem::take(&mut self.turns);
        for (t, list) in old {
            let t = f(t);
            for r in list {
                self.add(t, r);
            }
        }
    }
}

/// Whether one of `list`'s intervals (sorted, disjoint) touches `r`.
fn touches_any(list: &[Range<usize>], r: &Range<usize>) -> bool {
    let i = list.partition_point(|x| x.end < r.start);
    list.get(i).is_some_and(|x| x.start <= r.end)
}

/// Sorts `list` and merges intervals that touch.
fn normalize(list: &mut Vec<Range<usize>>) {
    list.sort_by_key(|x| (x.start, x.end));
    let mut out: Vec<Range<usize>> = Vec::with_capacity(list.len());
    for x in list.drain(..) {
        match out.last_mut() {
            Some(last) if x.start <= last.end => last.end = last.end.max(x.end),
            _ => out.push(x),
        }
    }
    *list = out;
}

/// Widens the record of an insertion of `inserted` at `a` to every placement with the same
/// result: `before` is the current text just before `a` and `after` the text just after the
/// insertion (each at most [`MAX_SLIDE`] bytes). Returns the closed interval.
pub(super) fn insertion_window(
    a: usize,
    inserted: &[u8],
    before: &[u8],
    after: &[u8],
) -> Range<usize> {
    // Right: the insertion can move by k while the k bytes after it equal its first k bytes.
    let right = after
        .iter()
        .zip(inserted.iter().cycle())
        .take_while(|(x, y)| x == y)
        .count();
    // Left: while the bytes before it equal its last bytes.
    let left = before
        .iter()
        .rev()
        .zip(inserted.iter().rev().cycle())
        .take_while(|(x, y)| x == y)
        .count();
    a - left..a + inserted.len() + right
}

/// Widens the point `a` where `deleted` was removed to every point with the same result (see
/// [`insertion_window`]). Returns the closed interval.
pub(super) fn deletion_window(
    a: usize,
    deleted: &[u8],
    before: &[u8],
    after: &[u8],
) -> Range<usize> {
    let right = after
        .iter()
        .zip(deleted.iter().cycle())
        .take_while(|(x, y)| x == y)
        .count();
    let left = before
        .iter()
        .rev()
        .zip(deleted.iter().rev().cycle())
        .take_while(|(x, y)| x == y)
        .count();
    a - left..a + right
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(a: &Attribution) -> Vec<(u32, Range<usize>)> {
        a.intervals().map(|(t, r)| (t, r.clone())).collect()
    }

    #[test]
    fn shift_rules() {
        let shifted = |r: Range<usize>, a, b, len| {
            let mut r = r;
            shift(&mut r, a, b, len);
            r
        };
        // Before and after the edit.
        assert_eq!(shifted(2..4, 6, 8, 5), 2..4);
        assert_eq!(shifted(10..12, 6, 8, 5), 13..15);
        // Ending at an insertion point: stays; starting at it: after the new text.
        assert_eq!(shifted(2..6, 6, 6, 3), 2..6);
        assert_eq!(shifted(6..9, 6, 6, 3), 9..12);
        // A point at the insertion stays where it is.
        assert_eq!(shifted(6..6, 6, 6, 3), 6..6);
        // An insertion strictly inside grows the interval.
        assert_eq!(shifted(2..9, 6, 6, 3), 2..12);
        // Partly replaced: the part outside survives and meets the new text.
        assert_eq!(shifted(2..7, 6, 9, 1), 2..6);
        assert_eq!(shifted(7..12, 6, 9, 1), 7..10);
        // Entirely replaced: the point where the new text starts.
        assert_eq!(shifted(7..8, 6, 9, 4), 6..6);
        assert_eq!(shifted(6..9, 6, 9, 0), 6..6);
        // A point inside a deletion goes to its start; at its end, to the end of the new text.
        assert_eq!(shifted(7..7, 6, 9, 0), 6..6);
        assert_eq!(shifted(9..9, 6, 9, 2), 8..8);
    }

    #[test]
    fn add_merges_touching_intervals_of_one_turn() {
        let mut a = Attribution::default();
        a.add(2, 10..12);
        a.add(1, 0..3);
        a.add(2, 14..14);
        a.add(2, 12..13);
        assert_eq!(at(&a), [(1, 0..3), (2, 10..13), (2, 14..14)]);
        a.add(2, 13..14);
        assert_eq!(at(&a), [(1, 0..3), (2, 10..14)]);
        assert_eq!(a.turns_touching(&(3..3)), [1]);
        assert_eq!(a.turns_touching(&(4..9)), Vec::<u32>::new());
        assert_eq!(a.turns_touching(&(0..20)), [1, 2]);
    }

    #[test]
    fn subtract_keeps_the_parts_outside() {
        let mut a = Attribution::default();
        a.add(1, 0..10);
        a.add(2, 4..5);
        a.add(1, 20..20);
        assert_eq!(a.subtract(&(3..6), Some(1)), [1]);
        assert_eq!(at(&a), [(1, 0..3), (1, 6..10), (1, 20..20), (2, 4..5)]);
        assert_eq!(a.subtract(&(18..30), None), [1]);
        assert_eq!(a.subtract(&(4..5), None), [2]);
        assert_eq!(at(&a), [(1, 0..3), (1, 6..10)]);
        assert_eq!(a.subtract(&(11..12), None), Vec::<u32>::new());
        // A point inside an interval leaves it whole.
        assert_eq!(a.subtract(&(8..8), None), [1]);
        assert_eq!(at(&a), [(1, 0..3), (1, 6..10)]);
    }

    #[test]
    fn shifting_merges_what_an_edit_joins() {
        let mut a = Attribution::default();
        a.add(1, 0..2);
        a.add(1, 5..7);
        // Deleting the gap joins them.
        a.shift(2, 5, 0);
        assert_eq!(at(&a), [(1, 0..4)]);
    }

    #[test]
    fn windows_cover_every_equivalent_placement() {
        // "foo\nX\nbar": inserting "\nX" after "foo" equals inserting "X\n" after "foo\n".
        assert_eq!(insertion_window(3, b"\nX", b"foo", b"\nbar"), 3..6);
        // "a\nb\nb\nc": a "b\n" inserted at 2 could be at 4 (or be "\nb" at 1).
        assert_eq!(insertion_window(2, b"b\n", b"a\n", b"b\nc"), 1..6);
        // Deleting "\nbar" after "foo" equals deleting "bar\n" after "foo\n".
        assert_eq!(deletion_window(3, b"\nbar", b"foo", b"\nnext"), 3..4);
        assert_eq!(deletion_window(3, b"xy", b"abc", b"def"), 3..3);
    }

    #[test]
    fn retain_and_remap() {
        let mut a = Attribution::default();
        a.add(1, 0..1);
        a.add(2, 30..31);
        a.add(3, 50..50);
        let hunk = |r: Range<usize>| Hunk {
            id: crate::HunkId(0),
            base_lines: 0..0,
            cur_lines: 0..0,
            cur_bytes: r,
            base_bytes: 0..0,
            old_text: String::new(),
            words: Vec::new(),
            user_touched: false,
            turns: Vec::new(),
        };
        a.retain_touching(&[hunk(1..5), hunk(50..60)]);
        assert_eq!(at(&a), [(1, 0..1), (3, 50..50)]);
        a.remap(|_| 7);
        assert_eq!(at(&a), [(7, 0..1), (7, 50..50)]);
    }

    #[test]
    fn extract_takes_the_parts_inside() {
        let mut a = Attribution::default();
        a.add(1, 0..10);
        a.add(2, 4..5);
        a.add(2, 20..22);
        assert_eq!(a.extract(&(3..6)), [(1, 3..6), (2, 4..5)]);
        assert_eq!(at(&a), [(1, 0..3), (1, 6..10), (2, 20..22)]);
        assert!(a.extract(&(12..15)).is_empty());
    }
}
