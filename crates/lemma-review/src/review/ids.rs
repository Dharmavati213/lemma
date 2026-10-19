//! Hunk identity across recomputes: which occurrence number (and so which [`HunkId`]) each new
//! hunk gets, and which flags it inherits.
//!
//! [`HunkId`]: crate::HunkId

use super::Meta;
use crate::Hunk;
use crate::text::overlaps;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

/// A hunk found by the diff.
pub(super) struct Candidate {
    pub cur: Range<usize>,
    /// `pair_hash(old_text, new_text)`.
    pub pair: u64,
}

/// What a new hunk takes over from the hunks it replaces.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Inherited {
    pub occurrence: u64,
    pub user_touched: bool,
    pub edit_reported: Option<u64>,
    pub undo_reported: Option<u64>,
}

/// Matches the new hunks against the previous ones (whose ranges `on_edit` kept current in
/// `current`).
///
/// Occurrences, in order of preference: the one a re-opening reject pinned on an overlapping
/// hunk; the one of an overlapping previous hunk with the same texts; the one of any previous hunk
/// with the same texts (the diff may have placed it differently); the smallest unused one. Flags
/// come from every overlapping previous hunk. Previous hunks count with the whole lines they
/// touch, since provisional hunks need not be line-aligned.
pub(super) fn inherit(
    prev: &[Hunk],
    meta: &[Meta],
    current: &str,
    new: &[Candidate],
) -> Vec<Inherited> {
    let mut out = vec![Inherited::default(); new.len()];
    let mut occurrence: Vec<Option<u64>> = vec![None; new.len()];
    let mut taken: HashSet<(u64, u64)> = HashSet::new();
    let mut prev_used = vec![false; prev.len()];
    let lines: Vec<Range<usize>> = prev
        .iter()
        .map(|h| whole_lines(current, &h.cur_bytes))
        .collect();
    let overlapping: Vec<Vec<usize>> = new.iter().map(|c| overlapping(&lines, &c.cur)).collect();

    for (k, c) in new.iter().enumerate() {
        for &p in &overlapping[k] {
            let inherited = &mut out[k];
            inherited.user_touched |= prev[p].user_touched;
            inherited.edit_reported = inherited.edit_reported.max(meta[p].edit_reported);
            inherited.undo_reported = inherited.undo_reported.max(meta[p].undo_reported);
            if let Some(pin) = meta[p].reopen
                && pin.pair == c.pair
                && occurrence[k].is_none()
                && taken.insert((c.pair, pin.occurrence))
            {
                occurrence[k] = Some(pin.occurrence);
                inherited.user_touched |= pin.user_touched;
                // The re-opened hunk replaces this one: its own (new) occurrence is not offered to
                // other hunks with the same texts.
                prev_used[p] = true;
            }
        }
    }
    for (k, c) in new.iter().enumerate() {
        if occurrence[k].is_some() {
            continue;
        }
        for &p in &overlapping[k] {
            if !prev_used[p]
                && meta[p].pair == Some(c.pair)
                && taken.insert((c.pair, meta[p].occurrence))
            {
                occurrence[k] = Some(meta[p].occurrence);
                prev_used[p] = true;
                break;
            }
        }
    }
    let mut by_pair: HashMap<u64, VecDeque<u64>> = HashMap::new();
    for (p, m) in meta.iter().enumerate() {
        if let (false, Some(pair)) = (prev_used[p], m.pair) {
            by_pair.entry(pair).or_default().push_back(m.occurrence);
        }
    }
    for (k, c) in new.iter().enumerate() {
        if occurrence[k].is_some() {
            continue;
        }
        if let Some(queue) = by_pair.get_mut(&c.pair) {
            while let Some(o) = queue.pop_front() {
                if taken.insert((c.pair, o)) {
                    occurrence[k] = Some(o);
                    break;
                }
            }
        }
    }
    // Per pair, every occurrence below the cursor is taken (`taken` only grows from here on), so
    // many hunks with equal texts are numbered in linear time.
    let mut cursor: HashMap<u64, u64> = HashMap::new();
    for (k, c) in new.iter().enumerate() {
        let o = occurrence[k].unwrap_or_else(|| {
            let from = cursor.get(&c.pair).copied().unwrap_or(0);
            let o = (from..)
                .find(|o| !taken.contains(&(c.pair, *o)))
                .unwrap_or_default();
            taken.insert((c.pair, o));
            cursor.insert(c.pair, o.saturating_add(1));
            o
        });
        out[k].occurrence = o;
    }
    out
}

/// Indices of the previous hunks (as `whole_lines` ranges, sorted) overlapping `range`.
fn overlapping(prev: &[Range<usize>], range: &Range<usize>) -> Vec<usize> {
    let first = prev.partition_point(|r| r.end < range.start);
    prev[first..]
        .iter()
        .take_while(|r| r.start <= range.end)
        .enumerate()
        .filter(|(_, r)| overlaps(r, range))
        .map(|(i, _)| first + i)
        .collect()
}

/// `range` widened to the lines of `text` it touches: from the start of its first line to the
/// end (after the `'\n'`) of its last. A line-aligned range stays as it is.
fn whole_lines(text: &str, range: &Range<usize>) -> Range<usize> {
    let bytes = text.as_bytes();
    let (s, e) = (range.start.min(bytes.len()), range.end.min(bytes.len()));
    let line_start = |i: usize| i == 0 || bytes[i - 1] == b'\n';
    let start = bytes[..s]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let end = if line_start(e) && (s < e || line_start(s)) {
        e
    } else {
        bytes[e..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(bytes.len(), |i| e + i + 1)
    };
    start..end
}
