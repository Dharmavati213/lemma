//! How `on_edit` keeps the hunk list an exact description of the current text.
//!
//! Invariant: hunks are sorted and disjoint in both texts, `baseline[base_bytes] == old_text`,
//! `current[cur_bytes] == meta.new_text`, and the text between consecutive hunks (and before the
//! first and after the last) is identical in baseline and current. `cur_lines`/`base_lines` follow
//! `text::line_span` from the line of the range start.

use super::records::{Claim, Kind, Pieces, Placement, Step, add_piece};
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

/// Bytes looked at, at most, at either end of a hunk's two texts for the text they have in common
/// there (see [`FileReview::aligned`]): an edit further into a hunk than this counts as within
/// its change, so that the edits of a replace_all deep inside one long hunk cost nothing more.
const ALIGN_MAX: usize = 4096;

/// How many bytes `x` and `y` have in common at their start, up to `max`, ending on a char
/// boundary.
fn common_start(x: &str, y: &str, max: usize) -> usize {
    let n = x
        .bytes()
        .zip(y.bytes())
        .take(max)
        .take_while(|(p, q)| p == q)
        .count();
    (0..=n).rev().find(|&k| x.is_char_boundary(k)).unwrap_or(0)
}

/// How many bytes `x` and `y` have in common at their end, up to `max`, starting on a char
/// boundary.
fn common_end(x: &str, y: &str, max: usize) -> usize {
    let n = x
        .bytes()
        .rev()
        .zip(y.bytes().rev())
        .take(max)
        .take_while(|(p, q)| p == q)
        .count();
    (0..=n)
        .rev()
        .find(|&k| x.is_char_boundary(x.len() - k))
        .unwrap_or(0)
}

/// `0..len` in pieces, each with whether it is one of `both`'s.
fn split(len: usize, both: &Pieces) -> Vec<(Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    for x in both.iter().filter(|x| x.start < len) {
        if pos < x.start {
            out.push((pos..x.start, false));
        }
        out.push((x.start..x.end.min(len), true));
        pos = x.end.min(len);
    }
    if pos < len {
        out.push((pos..len, false));
    }
    out
}

/// Net length change of a hunk (current minus baseline).
pub(super) fn growth(h: &Hunk) -> isize {
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
    ///
    /// Summed from the nearer end of the list: the growth of all the hunks is the current length
    /// minus the baseline's, so the hunks from `i` on can be taken off that instead. `on_edit`
    /// and the decisions change `current_len` together with the hunks and the baseline, so that
    /// it is the baseline's length plus the hunks' growth wherever this is called (the unit
    /// tests check it).
    pub(super) fn growth_before(&self, i: usize) -> isize {
        match &self.batch {
            Some(batch) => batch.growth_before(&self.hunks, i),
            None if 2 * i <= self.hunks.len() => self.hunks[..i].iter().map(growth).sum(),
            None => {
                let all = signed(self.current_len) - signed(self.baseline.len());
                let before = all - self.hunks[i..].iter().map(growth).sum::<isize>();
                if self.checks() {
                    assert_eq!(
                        before,
                        self.hunks[..i].iter().map(growth).sum::<isize>(),
                        "current_len is not the baseline's length plus the growth of the hunks"
                    );
                }
                before
            }
        }
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
        let mut out = String::with_capacity(range.len());
        self.current_pieces(range, |piece| {
            out.push_str(piece);
            true
        })
        .then_some(out)
    }

    /// Whether `current[range]` according to the model is `text` (what
    /// [`current_text`](Self::current_text) would return, without building it).
    pub(super) fn current_text_is(&self, range: &Range<usize>, text: &str) -> bool {
        if range.len() != text.len() {
            return false;
        }
        let mut rest = text;
        self.current_pieces(range, |piece| match rest.strip_prefix(piece) {
            Some(after) => {
                rest = after;
                true
            }
            None => false,
        })
    }

    /// Whether `current[range]` according to the model is `text`, where `hunks[..i]` end at or
    /// before `range.start` and grow the text by `grown`: a walk from hunk `i` on, so that
    /// `check_records` checks every record in one pass over the hunks. The bytes are
    /// compared: equal to a string's, they start and end on char boundaries.
    pub(super) fn text_is_at(
        &self,
        range: &Range<usize>,
        text: &str,
        mut i: usize,
        mut grown: isize,
    ) -> bool {
        if range.start > range.end || range.end > self.expected_len() || text.len() != range.len() {
            return false;
        }
        let mut pos = range.start;
        if range.is_empty() {
            return match self.hunks.get(i) {
                Some(h) if h.cur_bytes.start <= pos => self.meta[i]
                    .new_text
                    .is_char_boundary(pos - h.cur_bytes.start),
                _ => self.baseline.is_char_boundary(offset(pos, -grown)),
            };
        }
        while pos < range.end {
            let (piece, end) = match self.hunks.get(i) {
                Some(h) if h.cur_bytes.start <= pos => {
                    let (s, end) = (h.cur_bytes.start, range.end.min(h.cur_bytes.end));
                    grown += growth(h);
                    i += 1;
                    (
                        self.meta[i - 1].new_text.as_bytes().get(pos - s..end - s),
                        end,
                    )
                }
                next => {
                    let end = next.map_or(range.end, |h| h.cur_bytes.start.min(range.end));
                    let at = offset(pos, -grown);
                    (self.baseline.as_bytes().get(at..at + (end - pos)), end)
                }
            };
            if piece != text.as_bytes().get(pos - range.start..end - range.start) {
                return false;
            }
            pos = end;
        }
        true
    }

    /// Hands the pieces of `current[range]` (hunk texts and baseline gaps, in order) to `piece`
    /// while it returns true. False when `range` does not fit the text or `piece` stopped.
    fn current_pieces(&self, range: &Range<usize>, mut piece: impl FnMut(&str) -> bool) -> bool {
        if range.start > range.end
            || range.end > self.expected_len()
            || !self.is_char_boundary(range.start)
            || !self.is_char_boundary(range.end)
        {
            return false;
        }
        let mut pos = range.start;
        let mut i = self.first_ending_after(pos);
        // The growth of the hunks before `i`, once it is needed.
        let mut grown: Option<isize> = None;
        while pos < range.end {
            match self.hunks.get(i) {
                Some(h) if h.cur_bytes.start <= pos => {
                    let end = range.end.min(h.cur_bytes.end);
                    let s = h.cur_bytes.start;
                    if !piece(&self.meta[i].new_text[pos - s..end - s]) {
                        return false;
                    }
                    pos = end;
                    grown = grown.map(|g| g + growth(h));
                    i += 1;
                }
                next => {
                    let end = next.map_or(range.end, |h| h.cur_bytes.start.min(range.end));
                    let g = *grown.get_or_insert_with(|| self.growth_before(i));
                    let at = offset(pos, -g);
                    match self.baseline.get(at..at + (end - pos)) {
                        Some(gap) if piece(gap) => pos = end,
                        _ => return false,
                    }
                }
            }
        }
        true
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
        let (hunks, batch) = (&self.hunks, &self.batch);
        let free = |o: &u64| {
            let id = hunk_id(old, new, *o);
            match batch {
                // It knows the hunks `on_edits` set aside too.
                Some(batch) => !batch.has(id),
                None => hunks.iter().all(|h| h.id != id),
            }
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

    /// The buffer's undo or redo of the user's edits that went into the baseline (see `user`):
    /// `[a, b)` of the current text and `[base, base + b - a)` of the baseline, which hold the
    /// same text, replaced by `text` in both, wherever the hunks lie now. A recompute since may
    /// have widened a hunk over the place (the line it is on holds a change), where the Inside
    /// rule would take the edit for one of the proposal, or paired equal lines differently, so
    /// that the place in one text is no longer the other's: the hunks touching either place then
    /// become one, with the edit made in both of its texts, which gives both texts back exactly.
    /// False (nothing changed) when the two places do not hold the same text.
    pub(super) fn fold_both(&mut self, a: usize, b: usize, base: usize, text: &str) -> bool {
        let n = b - a;
        if self.batch.is_some()
            || base + n > self.baseline.len()
            || !self.baseline.is_char_boundary(base)
            || !self.baseline.is_char_boundary(base + n)
        {
            return false;
        }
        let (c1, c2) = self.touching(a, b);
        let d1 = self.hunks.partition_point(|h| h.base_bytes.end < base);
        let d2 = d1
            + self.hunks[d1..]
                .iter()
                .take_while(|h| h.base_bytes.start <= base + n)
                .count();
        let (i, j) = (c1.min(d1), c2.max(d2));
        let before = self.growth_before(i);
        if i == j && offset(base, before) == a {
            // The same place between hunks in both texts, whose text is the same there.
            return self.fold_at(i, base, a, b, text);
        }
        let after = before + self.hunks[i..j].iter().map(growth).sum::<isize>();
        // The region: the hunks, and the places in the gaps next to them.
        let (mut start, mut end) = (a.min(offset(base, before)), b.max(offset(base + n, after)));
        if let (Some(first), Some(last)) = (self.hunks[i..j].first(), self.hunks[i..j].last()) {
            start = start.min(first.cur_bytes.start);
            end = end.max(last.cur_bytes.end);
        }
        let (base_start, base_end) = (offset(start, -before), offset(end, -after));
        let (cur_line, base_line) = match self.hunks.get(i) {
            Some(first) if i < j && start == first.cur_bytes.start => {
                (first.cur_lines.start, first.base_lines.start)
            }
            _ => self.gap_lines(i, base_start),
        };
        let mut cur = String::with_capacity(end - start + text.len());
        let mut pos = base_start;
        for (h, m) in self.hunks[i..j].iter().zip(&self.meta[i..j]) {
            cur.push_str(&self.baseline[pos..h.base_bytes.start]);
            cur.push_str(&m.new_text);
            pos = h.base_bytes.end;
        }
        cur.push_str(&self.baseline[pos..base_end]);
        let mut old = self.baseline[base_start..base_end].to_owned();
        let (at, base_at) = (a - start, base - base_start);
        if cur.get(at..at + n).is_none() || cur.get(at..at + n) != old.get(base_at..base_at + n) {
            return false;
        }
        let lines = i64::from(newlines(text)) - i64::from(newlines(&cur[at..at + n]));
        let delta = signed(text.len()) - signed(n);
        cur.replace_range(at..at + n, text);
        old.replace_range(base_at..base_at + n, text);
        self.baseline.replace_range(base..base + n, text);
        let (id, meta) = if i < j {
            let merged = &self.meta[i..j];
            let meta = Meta {
                new_text: cur,
                occurrence: merged[0].occurrence,
                pair: None,
                edit_reported: merged.iter().filter_map(|m| m.edit_reported).max(),
                undo_reported: merged.iter().filter_map(|m| m.undo_reported).max(),
                reopen: merged.iter().find_map(|m| m.reopen),
                exact_words: false,
            };
            (self.hunks[i].id, meta)
        } else {
            let occurrence = self.free_occurrence(&old, &cur, None);
            let meta = Meta {
                pair: Some(pair_hash(&old, &cur)),
                new_text: cur,
                occurrence,
                edit_reported: None,
                undo_reported: None,
                reopen: None,
                exact_words: false,
            };
            (hunk_id(&old, &meta.new_text, occurrence), meta)
        };
        let hunk = Hunk {
            id,
            base_lines: line_span(base_line, &old),
            cur_lines: line_span(cur_line, &meta.new_text),
            cur_bytes: start..offset(end, delta),
            base_bytes: base_start..offset(base_end, delta),
            old_text: old,
            words: Vec::new(),
            user_touched: self.hunks[i..j].iter().any(|h| h.user_touched),
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
            shift_bytes(&mut h.base_bytes, delta);
            shift_lines(&mut h.cur_lines, lines);
            shift_lines(&mut h.base_lines, lines);
        }
        true
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

    /// A User or History edit where the Inside rule puts it: into the baseline between hunks,
    /// into the proposal inside one, into both texts inside hunks that hold no change any more
    /// (see `on_edit`). Returns where it went in the baseline, `None` for the proposal.
    pub(super) fn edit_as_classified(&mut self, a: usize, b: usize, text: &str) -> Option<usize> {
        match self.classify(a, b) {
            (EditClass::Outside, first) => self.fold_into_baseline(first, a, b, text),
            (EditClass::Inside(_), first) => {
                if self.inside_unchanged(first, a, b) {
                    let at = offset(a, -self.growth_before(first));
                    if self.fold_both(a, b, at, text) {
                        return Some(at);
                    }
                }
                self.edit_inside(first, a, b, text);
                None
            }
        }
    }

    /// Whether every hunk the edit `[a, b)` is inside (from `first` on) holds no change: its
    /// texts are equal (a provisional hunk the next recompute drops).
    pub(super) fn inside_unchanged(&self, first: usize, a: usize, b: usize) -> bool {
        self.hunks[first..]
            .iter()
            .zip(&self.meta[first..])
            .take_while(|(h, _)| inside(&h.cur_bytes, a, b))
            .all(|(h, m)| h.old_text == m.new_text)
    }

    /// A user edit overlapping hunks (`first` is the first of them): it becomes part of the
    /// proposal and marks the hunk touched.
    pub(super) fn edit_inside(&mut self, first: usize, a: usize, b: usize, text: &str) {
        let end = first
            + self.hunks[first..]
                .iter()
                .take_while(|h| inside(&h.cur_bytes, a, b))
                .count();
        self.edit_hunks_inside(first, end, a, b, text);
    }

    /// The undo (or redo) of a user change that took text out of a hunk's proposal: it puts the
    /// text back into the proposal, merged with every hunk it touches, including at their
    /// boundaries (at the end of what the change left of a hunk, or at the pure deletion it left),
    /// or as a hunk of its own when none is left there (a recompute dropped one the change had
    /// made equal to the baseline).
    pub(super) fn edit_inside_touching(&mut self, a: usize, b: usize, text: &str) {
        let (first, end) = self.touching(a, b);
        self.edit_hunks_inside(first, end, a, b, text);
    }

    /// The hunks `[first, end)` touching the closed interval `[a, b]`, boundaries included.
    pub(super) fn touching(&self, a: usize, b: usize) -> (usize, usize) {
        let first = self.hunks.partition_point(|h| h.cur_bytes.end < a);
        let end = first
            + self.hunks[first..]
                .iter()
                .take_while(|h| h.cur_bytes.start <= b)
                .count();
        (first, end)
    }

    /// A user edit merged with the hunks `[first, end)` into one: it becomes part of the proposal
    /// and marks the hunk touched.
    fn edit_hunks_inside(&mut self, first: usize, end: usize, a: usize, b: usize, text: &str) {
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
        let (first, end) = self.touching(a, b);
        self.merge_edit(first, end, a, b, text)
    }

    /// The pieces of the current text's `[a, b)` that the baseline holds at the same place (from
    /// `a`): in the gaps between hunks, and in what a hunk's two texts have in common at its start
    /// or its end (looked for within [`ALIGN_MAX`] bytes of them). Not what a diff of a hunk's
    /// texts would pair up inside it: the agent's text has characters the baseline has too, and
    /// those are not the baseline's.
    pub(super) fn aligned(&self, a: usize, b: usize) -> Pieces {
        let mut out = Pieces::new();
        let mut i = self.first_ending_after(a);
        let mut pos = a;
        while pos < b {
            match self.hunks.get(i) {
                Some(h) if h.cur_bytes.start < b => {
                    let (s, e) = (h.cur_bytes.start, h.cur_bytes.end);
                    if pos < s {
                        add_piece(&mut out, pos - a..s - a);
                        pos = s;
                    }
                    let end = e.min(b);
                    let (new, old) = (self.meta[i].new_text.as_str(), h.old_text.as_str());
                    let room = new.len().min(old.len());
                    let mut start = 0;
                    if pos - s < ALIGN_MAX {
                        start = common_start(new, old, (end - s).min(room));
                        if s + start > pos {
                            add_piece(&mut out, pos - a..(s + start).min(end) - a);
                        }
                    }
                    if e - end < ALIGN_MAX {
                        let tail = common_end(new, old, (e - pos).min(room - start));
                        if e - tail < end {
                            add_piece(&mut out, (e - tail).max(pos) - a..end - a);
                        }
                    }
                    pos = end;
                    i += 1;
                }
                _ => {
                    add_piece(&mut out, pos - a..b - a);
                    pos = b;
                }
            }
        }
        out
    }

    /// Where the current text's `[x, y)` is in the baseline, if both texts hold it at one place
    /// (see [`aligned`](Self::aligned)); for an empty range, the place in the baseline where text
    /// inserted there goes as well.
    fn base_of(&self, x: usize, y: usize) -> Option<usize> {
        let i = self.first_ending_after(x);
        let Some(h) = self
            .hunks
            .get(i)
            .filter(|h| h.cur_bytes.start < y.max(x + 1))
        else {
            return Some(offset(x, -self.growth_before(i)));
        };
        let (s, e) = (h.cur_bytes.start, h.cur_bytes.end);
        if x == y && x == s {
            return Some(offset(x, -self.growth_before(i)));
        }
        // One hunk only, and the range reaches past it only where its texts are equal.
        if y > e
            && self
                .hunks
                .get(i + 1)
                .is_some_and(|next| next.cur_bytes.start < y)
        {
            return None;
        }
        let (new, old) = (self.meta[i].new_text.as_str(), h.old_text.as_str());
        let equal = new == old;
        if (x < s || y > e) && !equal {
            // Its common start or end reaches the gap next to it.
            if x < s && y <= e {
                let k = y - s;
                let fits = k <= ALIGN_MAX.min(new.len()).min(old.len());
                return (fits && common_start(new, old, k) == k)
                    .then(|| offset(x, -self.growth_before(i)));
            }
            if x >= s && y > e {
                let k = e - x;
                let fits = k <= ALIGN_MAX.min(new.len()).min(old.len());
                return (fits && common_end(new, old, k) == k).then(|| h.base_bytes.end - (e - x));
            }
            return None;
        }
        if equal {
            return Some(offset(x, -self.growth_before(i)));
        }
        let room = new.len().min(old.len());
        if y - s <= ALIGN_MAX.min(room) && common_start(new, old, y - s) == y - s {
            return Some(h.base_bytes.start + (x - s));
        }
        if e - x <= ALIGN_MAX.min(room) && common_end(new, old, e - x) == e - x {
            return Some(h.base_bytes.end - (e - x));
        }
        None
    }

    /// Where in `[a, b)` the pieces of `placement.deleted` (from `a`) go out of both texts, and
    /// where they are in the baseline: where the step before put them (`deleted_at`), if the
    /// baseline holds them there; else where both texts hold them, or else where both hold the
    /// same text within `[a, b)` (a run of equal characters, whose proposal a recompute may have
    /// paired with another of them). A piece none fits is left out (it goes out of the current
    /// text only).
    fn deletion_targets(
        &self,
        a: usize,
        b: usize,
        placement: &Placement,
    ) -> Vec<(Range<usize>, usize)> {
        let Some(text) = self.current_text(&(a..b)) else {
            return Vec::new();
        };
        let at = |r: &Range<usize>| {
            (text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
                .then(|| self.base_of(a + r.start, a + r.end))
                .flatten()
        };
        let mut out: Vec<(Range<usize>, usize)> = Vec::new();
        let hints = placement
            .deleted_at
            .iter()
            .copied()
            .chain(std::iter::repeat(None));
        for (r, hint) in placement.deleted.iter().zip(hints) {
            let Some(piece) = text.get(r.clone()) else {
                continue;
            };
            let n = r.len();
            let free = |k: usize, out: &[(Range<usize>, usize)]| {
                out.iter().all(|(x, _)| x.end <= k || k + n <= x.start)
            };
            // Where the step before put it: checked once the pieces after it are out (the
            // pieces went in from the first one on, each where the ones before it had gone).
            let found = hint.map(|base| (r.clone(), base)).or_else(|| {
                at(r).map(|base| (r.clone(), base)).or_else(|| {
                    (0..=text.len().saturating_sub(n)).find_map(|k| {
                        let x = k..k + n;
                        (free(k, &out) && text.get(x.clone()) == Some(piece))
                            .then(|| at(&x).map(|base| (x, base)))
                            .flatten()
                    })
                })
            });
            if let Some((x, base)) = found.filter(|(x, _)| free(x.start, &out)) {
                let i = out.partition_point(|(y, _)| y.start < x.start);
                out.insert(i, (x, base));
            }
        }
        out
    }

    /// The edit `[a, b) → text`, the step of a record, with the pieces `placement` names going
    /// into both texts where both hold the same place (where the step before took them out or
    /// put them in, if that is known), and the others into the current text only (a proposal).
    /// `current_len` follows it. Returns the pieces that went into both, with where they are in
    /// the baseline, and the hunk the others went into, if any.
    pub(super) fn edit_placed(
        &mut self,
        a: usize,
        b: usize,
        text: &str,
        placement: &Placement,
    ) -> (Placement, Option<usize>) {
        let mut placed = Placement::default();
        let mut proposal = false;
        let targets = self.deletion_targets(a, b, placement);
        let both: Pieces = targets.iter().map(|(x, _)| x.clone()).collect();
        // The deleted pieces from the last one down, so that those before stay where they are.
        let mut gone = Vec::new();
        for (r, is_both) in split(b - a, &both).into_iter().rev() {
            let (x, y) = (a + r.start, a + r.end);
            let base = targets
                .iter()
                .find(|(t, _)| *t == r)
                .map(|(_, base)| *base)
                .filter(|_| is_both);
            // There, if the baseline holds the piece there; else where both texts hold it.
            let held = |base: &usize| {
                let piece = self.current_text(&(x..y));
                piece.is_some() && self.baseline.get(*base..*base + (y - x)) == piece.as_deref()
            };
            let base = base
                .filter(held)
                .or_else(|| base.and_then(|_| self.base_of(x, y)))
                .filter(|&base| self.fold_both(x, y, base, ""));
            if let Some(base) = base {
                gone.push((r, base));
            } else {
                self.edit_proposal(x, y, "");
                proposal = true;
            }
            self.current_len -= y - x;
        }
        gone.reverse();
        for (r, base) in gone {
            placed.deleted.push(r);
            placed.deleted_at.push(Some(base));
        }
        let mut pos = a;
        let hints = placement
            .inserted_at
            .iter()
            .copied()
            .chain(std::iter::repeat(None));
        let mut hinted = placement.inserted.iter().cloned().zip(hints);
        for (r, is_both) in split(text.len(), &placement.inserted) {
            let (Some(piece), true) = (text.get(r.clone()), text.is_char_boundary(r.end)) else {
                // Not on char boundaries: what is left goes as a proposal.
                let rest = &text[r.start..];
                self.edit_proposal(pos, pos, rest);
                self.current_len += rest.len();
                proposal = true;
                break;
            };
            let hint = is_both
                .then(|| hinted.find(|(x, _)| *x == r).and_then(|(_, h)| h))
                .flatten();
            let base = is_both
                .then(|| {
                    hint.filter(|&base| {
                        self.baseline.is_char_boundary(base)
                            && self.fold_both(pos, pos, base, piece)
                    })
                    .or_else(|| {
                        self.base_of(pos, pos)
                            .filter(|&base| self.fold_both(pos, pos, base, piece))
                    })
                })
                .flatten();
            if let Some(base) = base {
                placed.inserted.push(r);
                placed.inserted_at.push(Some(base));
            } else {
                self.edit_proposal(pos, pos, piece);
                proposal = true;
            }
            self.current_len += piece.len();
            pos += piece.len();
        }
        let index = proposal
            .then(|| {
                let (first, end) = self.touching(a, a + text.len());
                (first < end).then_some(first)
            })
            .flatten();
        (placed, index)
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
        // A reload's record (no turn) is not the agent's edit: its undo is no news for it.
        let reload = claim.record.is_some() && claim.turn.is_none();
        match (claim.kind, claim.step) {
            (Kind::Agent, Step::Undo) if claim.done && !reload => {
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
                exact_words: false,
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
                exact_words: false,
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
        if let Some(batch) = &mut self.batch {
            batch.merged(i, &self.hunks[i..j], hunk.id, lines);
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
