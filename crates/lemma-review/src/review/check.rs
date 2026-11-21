//! Checks of the invariants the review keeps, for the tests: the hunks describe the current text
//! exactly (`model`), every hunk's turns are those of the attribution (`turns`), the segments lie
//! in the text (`segments`), and every record of Lemma's own edits holds the text at its range
//! (`records`). The unit tests run them after every change; the integration tests ask for them
//! ([`FileReview::check_invariants`], [`FileReview::check_every_change`]), which costs a release
//! build nothing besides a flag tested per change.

use super::FileReview;
use super::model;
use super::turns::touch;
use crate::text::{count_newlines, line_span, to_u32};

impl FileReview {
    /// For tests: whether the review describes `current` exactly (see the module docs). It walks
    /// every hunk, interval, segment and record.
    #[doc(hidden)]
    pub fn check_invariants(&self, current: &str) -> Result<(), String> {
        check_model(self, current)?;
        self.check_records()
    }

    /// For tests: from now on every change checks, as the unit tests' changes do, what it counts
    /// on as it goes (that the current length is the baseline's plus the growth of the hunks,
    /// wherever `growth_before` reads it) and every record once it is done, and panics when that
    /// fails. A pass over the hunks and the records per change.
    #[doc(hidden)]
    pub fn check_every_change(&mut self) {
        self.checking = true;
    }

    /// Whether the checks of [`check_every_change`](Self::check_every_change) run.
    pub(super) fn checks(&self) -> bool {
        cfg!(test) || self.checking
    }

    /// Whether every record holds the text at its range, and what they take is counted right. In
    /// text order, so that the hunk list is walked once.
    pub(super) fn check_records(&self) -> Result<(), String> {
        self.records.check_costs()?;
        let (mut i, mut grown) = (0, 0);
        for (range, content) in self.records.in_text_order() {
            while self
                .hunks
                .get(i)
                .is_some_and(|h| h.cur_bytes.end <= range.start)
            {
                grown += model::growth(&self.hunks[i]);
                i += 1;
            }
            if !self.text_is_at(&range, content, i, grown) {
                return Err(format!(
                    "the record at {range:?} ({content:?}) does not match the text"
                ));
            }
        }
        Ok(())
    }
}

/// Checks that the hunk list describes `current` exactly (see `model.rs`).
pub(super) fn check_model(r: &FileReview, current: &str) -> Result<(), String> {
    if r.hunks.len() != r.meta.len() {
        return Err("hunks and meta differ in length".into());
    }
    let (mut cur, mut base) = (0, 0);
    for (k, (h, m)) in r.hunks.iter().zip(&r.meta).enumerate() {
        if h.cur_bytes.start < cur || h.base_bytes.start < base {
            return Err(format!("hunk {k} overlaps its predecessor: {h:?}"));
        }
        if current.get(cur..h.cur_bytes.start) != r.baseline.get(base..h.base_bytes.start) {
            return Err(format!("gap before hunk {k} differs"));
        }
        if current.get(h.cur_bytes.clone()) != Some(m.new_text.as_str()) {
            return Err(format!(
                "hunk {k} new_text is not current[cur_bytes]: {h:?} {m:?}"
            ));
        }
        if r.baseline.get(h.base_bytes.clone()) != Some(h.old_text.as_str()) {
            return Err(format!("hunk {k} old_text is not baseline[base_bytes]"));
        }
        let cur_line = to_u32(count_newlines(&current.as_bytes()[..h.cur_bytes.start]));
        let base_line = to_u32(count_newlines(&r.baseline.as_bytes()[..h.base_bytes.start]));
        if h.cur_lines != line_span(cur_line, &m.new_text)
            || h.base_lines != line_span(base_line, &h.old_text)
        {
            return Err(format!("hunk {k} has wrong lines: {h:?}"));
        }
        if r.hunks.iter().filter(|o| o.id == h.id).count() != 1 {
            return Err(format!("hunk id {:?} is not unique", h.id));
        }
        cur = h.cur_bytes.end;
        base = h.base_bytes.end;
    }
    if current.get(cur..) != r.baseline.get(base..) {
        return Err("text after the last hunk differs".into());
    }
    if r.expected_len() != current.len() {
        return Err("expected_len differs".into());
    }
    let growth: isize = r
        .hunks
        .iter()
        .map(|h| h.cur_bytes.len() as isize - h.base_bytes.len() as isize)
        .sum();
    if r.baseline.len() as isize + growth != current.len() as isize {
        return Err("the hunks' growth does not add up to the current length".into());
    }
    check_attribution(r, current)
}

/// The attribution lists are sorted, disjoint and inside the text, and every hunk lists exactly
/// the turns whose intervals touch it.
fn check_attribution(r: &FileReview, current: &str) -> Result<(), String> {
    let mut last: Option<(u32, std::ops::Range<usize>)> = None;
    for (turn, x) in r.attribution.intervals() {
        if x.start > x.end || x.end > current.len() {
            return Err(format!("turn {turn} interval {x:?} is outside the text"));
        }
        if let Some((t, prev)) = &last
            && (*t > turn || (*t == turn && prev.end >= x.start))
        {
            return Err(format!(
                "turn {turn}: {prev:?} and {x:?} are not sorted and apart"
            ));
        }
        last = Some((turn, x.clone()));
    }
    for h in &r.hunks {
        let mut expected: Vec<u32> = r
            .attribution
            .intervals()
            .filter(|(_, x)| touch(x, &h.cur_bytes))
            .map(|(t, _)| t)
            .collect();
        expected.dedup();
        if h.turns != expected {
            return Err(format!("hunk {h:?} should have turns {expected:?}"));
        }
    }
    check_segments(r, current)
}

/// The segments lie in the text on char boundaries, sorted, with disjoint interiors (an empty one
/// never strictly inside another).
fn check_segments(r: &FileReview, current: &str) -> Result<(), String> {
    let mut prev: Option<std::ops::Range<usize>> = None;
    for s in r.segments.iter() {
        if current.get(s.range.clone()).is_none() {
            return Err(format!("segment {s:?} does not fit the text"));
        }
        if let Some(p) = &prev {
            let (x, y) = (&p, &s.range);
            let sorted = (x.start, x.end) <= (y.start, y.end);
            let apart = if x.is_empty() || y.is_empty() {
                !(x.start < y.start && y.start < x.end)
            } else {
                x.end <= y.start
            };
            if !sorted || !apart {
                return Err(format!(
                    "segments {x:?} and {y:?} overlap or are not sorted"
                ));
            }
        }
        prev = Some(s.range.clone());
    }
    for (_, x) in r.unknown.intervals() {
        if x.start > x.end || x.end > current.len() {
            return Err(format!("unknown interval {x:?} is outside the text"));
        }
    }
    Ok(())
}
