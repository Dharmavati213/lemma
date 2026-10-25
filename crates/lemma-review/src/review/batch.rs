//! [`FileReview::on_edits`]: the changes of one action at once, such as a replace_all with
//! thousands of matches.
//!
//! The result is exactly that of [`FileReview::on_edit`] for each edit in turn, and for user,
//! history and review edits that is what `on_edits` does. Agent and reload edits are the ones
//! that come in thousands (the agent's replace_all, Replace in Project, a reformat), and there
//! `on_edit` costs O(hunks) per edit: the edits of an action descend, so each one lands below the
//! hunks, segments and attribution intervals the edits before it made, and `on_edit` moves all of
//! them (and puts its hunk in front of them in the hunk list).
//!
//! So before each edit, `on_edits` sets aside everything that starts beyond the edit's *reach*
//! (below), except the first hunk and the first segment there, which `on_edit` reads as the next
//! neighbour. Every later edit lies below what was set aside, so all it does to it is move it by
//! its own length change (and its hunks by its line change): a chunk set aside notes the sum of
//! those changes so far and is moved by what was added since when it comes back, at the end. Each
//! element goes aside and comes back once, and `on_edit` only ever sees what lies within reach of
//! its edit, so a batch costs O((edits + hunks) log) instead of O(edits × hunks).
//!
//! An edit's reach covers everything `on_edit` reads or changes for it, in the text before the
//! edit: the edit, the hunks and segments reaching past its end, the text up to [`MAX_SLIDE`]
//! bytes past those (where insertions and deletions are widened, and the text around a segment),
//! and then whatever starts within that and reaches further (a long hunk, segment or attribution
//! interval, whose end then counts, until nothing reaches across). Nothing set aside touches what
//! `on_edit` looks at, so it does exactly what it does on the whole review.
//!
//! What `on_edit` reads across the whole text is kept for the whole review meanwhile ([`Batch`]):
//! which hunk ids exist (`free_occurrence`), and the net growth of the hunks before a point
//! (`growth_before`, from prefix sums over the hunks no edit has reached yet). The records (at
//! most 512, plus the rejects of the newest decision) stay up to date edit by edit as before. The
//! journal is moved through all the edits at the end (`Journal::track_all`).
//!
//! Alt+U of an Accept all puts thousands of regions back the same way, from the last one down
//! ([`FileReview::unaccept_batch`]): each one inserts its hunk below the ones put back before it
//! and moves the baseline after it, so what lies beyond the region (and what it puts back around
//! it) is set aside meanwhile and comes back at the end, its baseline offsets and lines moved.

use super::journal::Accepted;
use super::segments::Segment;
use super::turns::{Attribution, MAX_SLIDE};
use super::{FileReview, Meta, model};
use crate::text::{offset, shift_bytes, shift_lines};
use crate::{Hunk, HunkId};
use lemma_core::{Origin, TextEdit};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;

/// How far past an edit and what it meets `on_edit` reads: [`MAX_SLIDE`] bytes, plus a few to
/// reach a char boundary.
const MARGIN: usize = MAX_SLIDE + 8;

/// What `on_edit` (or `unaccept_region`) consults during [`FileReview::on_edits`] (or
/// `unaccept_batch`) instead of the whole hunk list (see the module docs).
#[derive(Clone, Debug)]
pub(super) struct Batch {
    /// How many hunks have each id, those set aside included.
    ids: HashMap<HunkId, usize>,
    /// `growth[i]`: the net growth of `hunks[..i]`, for `i <= clean`.
    growth: Vec<isize>,
    /// The hunks before this index are those the batch started with.
    clean: usize,
    /// The lines the last edit added to the text after it (from `merge_edit`).
    lines: i64,
}

impl Batch {
    fn new(hunks: &[Hunk]) -> Self {
        let mut ids = HashMap::with_capacity(hunks.len());
        let mut growth = Vec::with_capacity(hunks.len() + 1);
        growth.push(0);
        for h in hunks {
            *ids.entry(h.id).or_insert(0) += 1;
            growth.push(growth[growth.len() - 1] + model::growth(h));
        }
        Self {
            ids,
            growth,
            clean: hunks.len(),
            lines: 0,
        }
    }

    /// Whether a hunk has the id `id`.
    pub fn has(&self, id: HunkId) -> bool {
        self.ids.contains_key(&id)
    }

    /// `FileReview::growth_before(i)` for the hunk list `hunks`.
    pub fn growth_before(&self, hunks: &[Hunk], i: usize) -> isize {
        let clean = self.clean.min(i);
        self.growth[clean] + hunks[clean..i].iter().map(model::growth).sum::<isize>()
    }

    /// The hunks `old`, from index `i` on, were replaced by one hunk with the id `id` (`merge_edit`;
    /// with no `old`, a hunk inserted at `i`), which moved the lines after it by `lines`.
    pub fn merged(&mut self, i: usize, old: &[Hunk], id: HunkId, lines: i64) {
        for h in old {
            if let Entry::Occupied(mut n) = self.ids.entry(h.id) {
                *n.get_mut() -= 1;
                if *n.get() == 0 {
                    n.remove();
                }
            }
        }
        *self.ids.entry(id).or_insert(0) += 1;
        self.clean = self.clean.min(i);
        self.lines += lines;
    }
}

/// How far the changes of a batch so far moved what lies after them: in the current text (the
/// edits of `on_edits`) and in the baseline (the regions `unaccept_batch` puts back).
#[derive(Clone, Copy, Debug, Default)]
struct Shift {
    bytes: isize,
    lines: i64,
    base_bytes: isize,
    base_lines: i64,
}

impl Shift {
    /// What was added since `earlier`.
    fn since(self, earlier: Shift) -> Shift {
        Shift {
            bytes: self.bytes - earlier.bytes,
            lines: self.lines - earlier.lines,
            base_bytes: self.base_bytes - earlier.base_bytes,
            base_lines: self.base_lines - earlier.base_lines,
        }
    }
}

/// Attribution intervals set aside: where the lowest starts, and the intervals by turn.
type Intervals = (usize, Vec<(u32, Vec<Range<usize>>)>);

/// Elements of one list set aside together (ascending), with the [`Shift`] at that moment.
struct Chunk<T> {
    at: Shift,
    items: T,
}

/// What `on_edits` set aside, per list, the lowest chunk last.
#[derive(Default)]
struct Aside {
    hunks: Vec<Chunk<(Vec<Hunk>, Vec<Meta>)>>,
    segments: Vec<Chunk<Vec<Segment>>>,
    attribution: Vec<Chunk<Intervals>>,
    unknown: Vec<Chunk<Intervals>>,
}

impl FileReview {
    /// Call for the changes of one action at once, before they are applied, instead of
    /// [`on_edit`](Self::on_edit) for each: `edits` in the order they are applied, with the
    /// pre-edit offsets of their moment, each one ending at or before the start of the one
    /// before it (descending and disjoint, as `lemma_core::apply_all` takes them).
    ///
    /// The result is exactly that of `on_edit` for each edit in turn. Agent and reload edits
    /// cost O(log hunks) each instead of O(hunks), so a replace_all with tens of thousands of
    /// matches is fed in milliseconds (see `batch.rs`); edits out of that order are fed one by
    /// one.
    pub fn on_edits(&mut self, edits: &[TextEdit], origin: Origin) {
        let batched = matches!(origin, Origin::Agent { .. } | Origin::Reload)
            && edits.len() > 1
            && edits.windows(2).all(|w| w[1].range.end <= w[0].range.start);
        if !batched {
            for e in edits {
                self.on_edit(e, origin);
            }
            return;
        }
        self.batch = Some(Batch::new(&self.hunks));
        // Nothing reads the journal while the edits arrive: it follows all of them at the end.
        let mut journal = std::mem::take(&mut self.journal);
        let mut aside = Aside::default();
        let mut moved = Shift::default();
        let mut applied = Vec::with_capacity(edits.len());
        for e in edits {
            self.make_room(&mut aside, moved, e.range.end);
            let generation = self.generation;
            self.on_edit(e, origin);
            // Edits that change nothing (or do not fit) leave everything as it was.
            if self.generation != generation {
                applied.push((e.range.start, e.range.end, e.text.len()));
                moved.bytes += e.delta();
                moved.lines += self
                    .batch
                    .as_mut()
                    .map_or(0, |batch| std::mem::take(&mut batch.lines));
            }
        }
        self.bring_back(&mut aside, moved, usize::MAX);
        self.batch = None;
        journal.track_all(&applied);
        self.journal = journal;
    }

    /// `unaccept_each` of `regions` (descending), with the same result: O(log hunks) per region
    /// instead of O(hunks), besides moving the baseline after it (see the module docs). Returns
    /// whether any region could be reverted.
    pub(super) fn unaccept_batch(&mut self, regions: Vec<Accepted>, current: &str) -> bool {
        self.batch = Some(Batch::new(&self.hunks));
        let mut aside = Aside::default();
        let mut moved = Shift::default();
        let mut any = false;
        for region in regions {
            // What it reads and changes: the region, its neighbours and what it puts back.
            self.make_room(&mut aside, moved, region.parked.end(&region.cur));
            if let Some((bytes, lines)) = self.unaccept_region(region, current) {
                moved.base_bytes += bytes;
                moved.base_lines += lines;
                any = true;
            }
        }
        self.bring_back(&mut aside, moved, usize::MAX);
        self.batch = None;
        any
    }

    /// Leaves in the review exactly what lies within reach of an edit ending at `b` (see the
    /// module docs), with the first hunk and the first segment beyond.
    fn make_room(&mut self, aside: &mut Aside, now: Shift, b: usize) {
        let near = self.reach(aside, now, b);
        let reach = self.reach(aside, now, near.saturating_add(MARGIN));
        self.set_aside(aside, now, reach);
    }

    /// The first point from `x` on that no hunk, segment or interval reaches across (starting at
    /// or before it and ending after it). What starts at or before it is brought back first.
    fn reach(&mut self, aside: &mut Aside, now: Shift, x: usize) -> usize {
        let mut hi = x;
        loop {
            self.bring_back(aside, now, hi);
            // The ends are sorted like the starts in each list.
            let i = self.hunks.partition_point(|h| h.cur_bytes.start <= hi);
            let next = i
                .checked_sub(1)
                .map_or(hi, |k| hi.max(self.hunks[k].cur_bytes.end))
                .max(self.segments.reach(hi))
                .max(self.attribution.reach(hi))
                .max(self.unknown.reach(hi));
            if next == hi {
                return hi;
            }
            hi = next;
        }
    }

    /// Sets aside everything starting after `x`, except the first hunk and the first segment.
    fn set_aside(&mut self, aside: &mut Aside, now: Shift, x: usize) {
        let keep = self.hunks.partition_point(|h| h.cur_bytes.start <= x) + 1;
        if keep < self.hunks.len() {
            let items = (self.hunks.split_off(keep), self.meta.split_off(keep));
            if let Some(batch) = &mut self.batch {
                batch.clean = batch.clean.min(keep);
            }
            aside.hunks.push(Chunk { at: now, items });
        }
        let segments = self.segments.split_off_above(x);
        if !segments.is_empty() {
            aside.segments.push(Chunk {
                at: now,
                items: segments,
            });
        }
        set_aside_intervals(&mut self.attribution, &mut aside.attribution, now, x);
        set_aside_intervals(&mut self.unknown, &mut aside.unknown, now, x);
    }

    /// Brings back what was set aside, moved by what the edits added since, until everything
    /// starting at or before `x` is in the review, with the first hunk and segment after it.
    fn bring_back(&mut self, aside: &mut Aside, now: Shift, x: usize) {
        while self.hunks.last().is_none_or(|h| h.cur_bytes.start <= x)
            && let Some(Chunk { at, items }) = aside.hunks.pop()
        {
            let (mut hunks, meta) = items;
            let d = now.since(at);
            for h in &mut hunks {
                shift_bytes(&mut h.cur_bytes, d.bytes);
                shift_lines(&mut h.cur_lines, d.lines);
                shift_bytes(&mut h.base_bytes, d.base_bytes);
                shift_lines(&mut h.base_lines, d.base_lines);
            }
            self.hunks.extend(hunks);
            self.meta.extend(meta);
        }
        while self.segments.last_start().is_none_or(|s| s <= x)
            && let Some(Chunk { at, items }) = aside.segments.pop()
        {
            let mut segments = items;
            let d = now.since(at).bytes;
            for s in &mut segments {
                shift_bytes(&mut s.range, d);
            }
            self.segments.append(segments);
        }
        bring_back_intervals(&mut self.attribution, &mut aside.attribution, now, x);
        bring_back_intervals(&mut self.unknown, &mut aside.unknown, now, x);
    }
}

/// Sets aside the intervals of `list` starting after `x`.
fn set_aside_intervals(
    list: &mut Attribution,
    aside: &mut Vec<Chunk<Intervals>>,
    now: Shift,
    x: usize,
) {
    let lists = list.split_off_above(x);
    if let Some(low) = lists
        .iter()
        .filter_map(|(_, l)| l.first())
        .map(|r| r.start)
        .min()
    {
        aside.push(Chunk {
            at: now,
            items: (low, lists),
        });
    }
}

/// Brings back intervals set aside until none starting at or before `x` is left aside.
fn bring_back_intervals(
    list: &mut Attribution,
    aside: &mut Vec<Chunk<Intervals>>,
    now: Shift,
    x: usize,
) {
    while let Some(Chunk { at, items }) =
        aside.pop_if(|c| offset(c.items.0, now.since(c.at).bytes) <= x)
    {
        let d = now.since(at).bytes;
        for (turn, mut intervals) in items.1 {
            for r in &mut intervals {
                shift_bytes(r, d);
            }
            list.append(turn, intervals);
        }
    }
}
