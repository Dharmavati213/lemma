//! How `on_edit` keeps the hunk list an exact description of the current text.
//!
//! Invariant: hunks are sorted and disjoint in both texts, `baseline[base_bytes] == old_text`,
//! `current[cur_bytes] == meta.new_text`, and the text between consecutive hunks (and before the
//! first and after the last) is identical in baseline and current. `cur_lines`/`base_lines` follow
//! `text::line_span` from the line of the range start.

use super::records::{Claim, Kind, Step};
use super::{FileReview, Meta, hunk_id, pair_hash};
use crate::text::{
    count_newlines, line_span, newlines, offset, shift_bytes, shift_lines, signed, to_u32,
};
use crate::{EditClass, Hunk, ReviewEvent};
use std::ops::Range;

/// The contract's Inside test for an edit `[a, b)` against a hunk's current range `[s, e)`.
fn inside(cur: &Range<usize>, a: usize, b: usize) -> bool {
    let (s, e) = (cur.start, cur.end);
    (a < b && a < e && b > s) || (a == b && s <= a && a < e) || (s == e && a < s && b > s)
}

/// Net length change of a hunk (current minus baseline).
fn growth(h: &Hunk) -> isize {
    signed(h.cur_bytes.len()) - signed(h.base_bytes.len())
}

/// The hunk an edit was merged into.
pub(super) struct Merged {
    pub index: usize,
    /// The text the edit replaced.
    pub removed: String,
}

impl FileReview {
    /// Index of the first hunk that does not end at or before `x`.
    pub(super) fn first_ending_after(&self, x: usize) -> usize {
        self.hunks.partition_point(|h| h.cur_bytes.end <= x)
    }

    /// Classifies `[a, b)` by the Inside formula; also returns the index of the first hunk that
    /// does not end at or before `a` (where an Inside hunk, if any, is).
    pub(super) fn classify(&self, a: usize, b: usize) -> (EditClass, usize) {
        let i = self.first_ending_after(a);
        match self.hunks.get(i) {
            Some(h) if inside(&h.cur_bytes, a, b) => (EditClass::Inside(h.id), i),
            _ => (EditClass::Outside, i),
        }
    }

    /// Net growth of `hunks[..i]`: current offset minus baseline offset in the gap after them.
    pub(super) fn growth_before(&self, i: usize) -> isize {
        self.hunks[..i].iter().map(growth).sum()
    }

    /// Length of the current text according to the model: the baseline's length plus the net
    /// growth of every hunk (kept in `current_len`, so that it costs nothing per edit).
    pub(super) fn expected_len(&self) -> usize {
        self.current_len
    }

    /// Whether `x` is a char boundary of the current text according to the model (inside a
    /// hunk's text or in the baseline between hunks).
    pub(super) fn is_char_boundary(&self, x: usize) -> bool {
        let i = self.first_ending_after(x);
        match self.hunks.get(i) {
            Some(h) if h.cur_bytes.start <= x => self.meta[i]
                .new_text
                .is_char_boundary(x - h.cur_bytes.start),
            _ => self
                .baseline
                .is_char_boundary(offset(x, -self.growth_before(i))),
        }
    }

    /// `current[range]` according to the model, if `range` fits the text.
    pub(super) fn current_text(&self, range: &Range<usize>) -> Option<String> {
        if range.start > range.end
            || range.end > self.expected_len()
            || !self.is_char_boundary(range.start)
            || !self.is_char_boundary(range.end)
        {
            return None;
        }
        let mut out = String::with_capacity(range.len());
        let mut pos = range.start;
        let mut i = self.first_ending_after(pos);
        while pos < range.end {
            match self.hunks.get(i) {
                Some(h) if h.cur_bytes.start <= pos => {
                    let end = range.end.min(h.cur_bytes.end);
                    let s = h.cur_bytes.start;
                    out.push_str(&self.meta[i].new_text[pos - s..end - s]);
                    pos = end;
                    i += 1;
                }
                next => {
                    let end = next.map_or(range.end, |h| h.cur_bytes.start.min(range.end));
                    let at = offset(pos, -self.growth_before(i));
                    out.push_str(self.baseline.get(at..at + (end - pos))?);
                    pos = end;
                }
            }
        }
        Some(out)
    }

    /// Current and baseline line of the baseline offset `at`, which lies in the gap before
    /// `hunks[i]`.
    ///
    /// Newlines are counted from the nearer neighbouring hunk (the gap text is the same in both
    /// texts), so a run of edits landing next to each other, like a replace_all, does not rescan
    /// the file for each of them.
    pub(super) fn gap_lines(&self, i: usize, at: usize) -> (u32, u32) {
        let bytes = self.baseline.as_bytes();
        let (cur, base, from) = match i.checked_sub(1) {
            Some(k) => {
                let h = &self.hunks[k];
                (
                    h.cur_lines
                        .start
                        .saturating_add(newlines(&self.meta[k].new_text)),
                    h.base_lines.start.saturating_add(newlines(&h.old_text)),
                    h.base_bytes.end,
                )
            }
            None => (0, 0, 0),
        };
        if let Some(next) = self.hunks.get(i)
            && next.base_bytes.start - at < at - from
        {
            let n = to_u32(count_newlines(&bytes[at..next.base_bytes.start]));
            return (
                next.cur_lines.start.saturating_sub(n),
                next.base_lines.start.saturating_sub(n),
            );
        }
        let n = to_u32(count_newlines(&bytes[from..at]));
        (cur.saturating_add(n), base.saturating_add(n))
    }

    /// An occurrence number for a hunk with these texts whose id no other hunk has, `preferred` if
    /// possible, else the smallest one.
    ///
    /// `free_hints` remembers, per pair of texts, an occurrence below which every number is taken,
    /// so N hunks with equal texts (a replace_all) cost O(N) probes, not O(N²). Removing a hunk
    /// clears the hints ([`FileReview::forget_free_hints`]).
    pub(super) fn free_occurrence(&mut self, old: &str, new: &str, preferred: Option<u64>) -> u64 {
        let hunks = &self.hunks;
        let free = |o: &u64| {
            let id = hunk_id(old, new, *o);
            hunks.iter().all(|h| h.id != id)
        };
        if let Some(o) = preferred.filter(free) {
            return o;
        }
        let pair = pair_hash(old, new);
        let start = self.free_hints.get(&pair).copied().unwrap_or(0);
        let o = (start..).find(free).unwrap_or_default();
        self.free_hints.insert(pair, o.saturating_add(1));
        o
    }

    /// Forgets the [`free_occurrence`](Self::free_occurrence) hints: call whenever a hunk id
    /// disappears.
    pub(super) fn forget_free_hints(&mut self) {
        self.free_hints.clear();
    }

    /// A user edit between hunks: applied to the baseline as well, so it never becomes a hunk.
    /// `i` is the index of the first hunk after the edit. Returns the baseline offset of the
    /// edit; `None` (nothing changed) if the edit does not map into the baseline, which
    /// `on_edit`'s checks rule out.
    pub(super) fn fold_into_baseline(
        &mut self,
        i: usize,
        a: usize,
        b: usize,
        text: &str,
    ) -> Option<usize> {
        let at = offset(a, -self.growth_before(i));
        self.fold_at(i, at, a, b, text).then_some(at)
    }

    /// The user undoing their own change that removed text at baseline offset `base_at`: an
    /// insertion at `a` that goes back into the baseline exactly where the text was removed
    /// (before or after an adjacent pure deletion, at the start of a hunk), so that it never
    /// becomes part of a hunk. False (nothing changed) when `a` and `base_at` are no longer the
    /// same place between hunks (a recompute placed the hunks differently meanwhile).
    pub(super) fn fold_back(&mut self, a: usize, base_at: usize, text: &str) -> bool {
        let i = self.hunks.partition_point(|h| h.base_bytes.start < base_at);
        let in_gap = i.checked_sub(1).is_none_or(|k| {
            let h = &self.hunks[k];
            h.base_bytes.end <= base_at && h.cur_bytes.end <= a
        }) && self.hunks.get(i).is_none_or(|h| a <= h.cur_bytes.start);
        in_gap
            && offset(base_at, self.growth_before(i)) == a
            && self.fold_at(i, base_at, a, a, text)
    }

    /// Replaces `[a, b)` of the current text, which is `[at, at + b - a)` of the baseline in the
    /// gap before `hunks[i]`, by `text` in both texts.
    fn fold_at(&mut self, i: usize, at: usize, a: usize, b: usize, text: &str) -> bool {
        let Some(removed) = self.baseline.get(at..at + (b - a)) else {
            debug_assert!(false, "edit {a}..{b} does not map into the baseline");
            return false;
        };
        let lines = i64::from(newlines(text)) - i64::from(newlines(removed));
        self.baseline.replace_range(at..at + (b - a), text);
        let delta = signed(text.len()) - signed(b - a);
        for h in &mut self.hunks[i..] {
            shift_bytes(&mut h.cur_bytes, delta);
            shift_bytes(&mut h.base_bytes, delta);
            shift_lines(&mut h.cur_lines, lines);
            shift_lines(&mut h.base_lines, lines);
        }
        true
    }

    /// A user edit overlapping hunks (`first` is the first of them): it becomes part of the
    /// proposal and marks the hunk touched.
    pub(super) fn edit_inside(&mut self, first: usize, a: usize, b: usize, text: &str) {
        let end = first
            + self.hunks[first..]
                .iter()
                .take_while(|h| inside(&h.cur_bytes, a, b))
                .count();
        for k in first..end {
            if self.meta[k].edit_reported != Some(self.turn) {
                self.events.push(ReviewEvent::UserEditedInHunk {
                    lines: self.hunks[k].cur_lines.clone(),
                });
            }
        }
        let merged = self.merge_edit(first, end, a, b, text);
        self.hunks[merged.index].user_touched = true;
        self.meta[merged.index].edit_reported = Some(self.turn);
    }

    /// An edit that is part of a proposal (agent, review, reload, recognized undo/redo): merged
    /// with every hunk it touches, including at their boundaries, or a new hunk of its own.
    pub(super) fn edit_proposal(&mut self, a: usize, b: usize, text: &str) -> Merged {
        let first = self.hunks.partition_point(|h| h.cur_bytes.end < a);
        let end = first
            + self.hunks[first..]
                .iter()
                .take_while(|h| h.cur_bytes.start <= b)
                .count();
        self.merge_edit(first, end, a, b, text)
    }

    /// The pure deletion at `a` whose deleted text is exactly `text`, for an insertion `[a, a)`:
    /// most likely the undo of that deletion when no record covers it. Returns its index.
    pub(super) fn reinserted_deletion(&self, a: usize, b: usize, text: &str) -> Option<usize> {
        if a != b || text.is_empty() {
            return None;
        }
        let k = self.first_ending_after(a).checked_sub(1)?;
        let h = &self.hunks[k];
        (h.cur_bytes == (a..a) && h.old_text == text).then_some(k)
    }

    /// Follow-up for an edit recognized as undo/redo of one of our own edits.
    pub(super) fn apply_claim(&mut self, claim: Claim, merged: &Merged) {
        let k = merged.index;
        match (claim.kind, claim.step) {
            (Kind::Agent, Step::Undo) if claim.done => {
                if self.meta[k].undo_reported != Some(self.turn) {
                    self.events.push(ReviewEvent::UndoneByUser {
                        lines: self.hunks[k].cur_lines.clone(),
                    });
                    self.meta[k].undo_reported = Some(self.turn);
                }
            }
            (Kind::Reject(reopen), Step::Undo) if claim.done => self.meta[k].reopen = Some(reopen),
            _ => {}
        }
    }

    /// Replaces `hunks[i..j]` (all touching the edit `[a, b) → text`) by one hunk covering them
    /// and the edit, with the edit applied to its current text. The baseline is unchanged.
    fn merge_edit(&mut self, i: usize, j: usize, a: usize, b: usize, text: &str) -> Merged {
        let touched = &self.hunks[i..j];
        // Region start: the first hunk's start, or `a` when it lies in the gap before it.
        let (start, base_start, cur_line, base_line) = match touched.first() {
            Some(h) if h.cur_bytes.start <= a => (
                h.cur_bytes.start,
                h.base_bytes.start,
                h.cur_lines.start,
                h.base_lines.start,
            ),
            _ => {
                let at = offset(a, -self.growth_before(i));
                let (cur_line, base_line) = self.gap_lines(i, at);
                (a, at, cur_line, base_line)
            }
        };
        // Region end: the last hunk's end, or `b` when it lies in the gap after it.
        let (end, base_end) = match touched.last() {
            Some(h) if h.cur_bytes.end >= b => (h.cur_bytes.end, h.base_bytes.end),
            _ => (b, offset(b, -self.growth_before(j))),
        };
        // The region's current text: baseline gaps and the hunks' texts.
        let mut cur = String::with_capacity(end - start + text.len());
        let mut pos = base_start;
        for (h, m) in touched.iter().zip(&self.meta[i..j]) {
            cur.push_str(&self.baseline[pos..h.base_bytes.start]);
            cur.push_str(&m.new_text);
            pos = h.base_bytes.end;
        }
        cur.push_str(&self.baseline[pos..base_end]);
        debug_assert_eq!(cur.len(), end - start);

        let removed = cur[a - start..b - start].to_owned();
        let lines = i64::from(newlines(text)) - i64::from(newlines(&removed));
        let delta = signed(text.len()) - signed(b - a);
        cur.replace_range(a - start..b - start, text);
        let old_text = self.baseline[base_start..base_end].to_owned();

        let user_touched = touched.iter().any(|h| h.user_touched);
        let (id, meta) = if i < j {
            let merged = &self.meta[i..j];
            let meta = Meta {
                new_text: cur,
                occurrence: merged[0].occurrence,
                pair: None,
                edit_reported: merged.iter().filter_map(|m| m.edit_reported).max(),
                undo_reported: merged.iter().filter_map(|m| m.undo_reported).max(),
                reopen: merged.iter().find_map(|m| m.reopen),
            };
            (self.hunks[i].id, meta)
        } else {
            let occurrence = self.free_occurrence(&old_text, &cur, None);
            let meta = Meta {
                pair: Some(pair_hash(&old_text, &cur)),
                new_text: cur,
                occurrence,
                edit_reported: None,
                undo_reported: None,
                reopen: None,
            };
            (hunk_id(&old_text, &meta.new_text, occurrence), meta)
        };
        let hunk = Hunk {
            id,
            base_lines: line_span(base_line, &old_text),
            cur_lines: line_span(cur_line, &meta.new_text),
            cur_bytes: start..offset(end, delta),
            base_bytes: base_start..base_end,
            old_text,
            words: Vec::new(),
            user_touched,
            // `on_edit` sets them once the edit is attributed.
            turns: Vec::new(),
        };
        if j > i + 1 {
            // The ids of the hunks merged into the first one are gone.
            self.forget_free_hints();
        }
        self.hunks.splice(i..j, [hunk]);
        self.meta.splice(i..j, [meta]);
        for h in &mut self.hunks[i + 1..] {
            shift_bytes(&mut h.cur_bytes, delta);
            shift_lines(&mut h.cur_lines, lines);
        }
        Merged { index: i, removed }
    }
}
