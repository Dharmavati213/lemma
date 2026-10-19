//! The Alt+U journal: the last accepts and rejects, newest last.

use super::segments::Parked;
use crate::text::shift_range;
use std::collections::VecDeque;
use std::ops::Range;

/// Actions kept at most (oldest dropped first).
const MAX_ENTRIES: usize = 100;

/// An accepted hunk, enough to put it back.
#[derive(Clone, Debug)]
pub(super) struct Accepted {
    /// The accepted text's range in the current text (kept up to date through later edits).
    pub cur: Range<usize>,
    pub old_text: String,
    pub new_text: String,
    pub occurrence: u64,
    pub user_touched: bool,
    /// The turns the hunk belonged to (they come back with it).
    pub turns: Vec<u32>,
    /// The segments and unknown places taken out around it (anchored at `cur`).
    pub parked: Parked,
    /// No edit has cut into the range since.
    pub valid: bool,
}

/// One user decision.
#[derive(Clone, Debug)]
pub(super) enum Entry {
    /// Accept or Accept all.
    Accept(Vec<Accepted>),
    /// Reject or Reject all: ids of the reject records.
    Reject(Vec<u64>),
}

#[derive(Clone, Debug, Default)]
pub(super) struct Journal {
    entries: VecDeque<Entry>,
}

impl Journal {
    pub fn push(&mut self, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn pop(&mut self) -> Option<Entry> {
        self.entries.pop_back()
    }

    /// Renumbers the turns kept with accepted regions.
    pub fn remap(&mut self, f: &mut impl FnMut(u32) -> u32) {
        for entry in &mut self.entries {
            if let Entry::Accept(regions) = entry {
                for r in regions {
                    for t in r.turns.iter_mut() {
                        *t = f(*t);
                    }
                    r.parked.remap(f);
                }
            }
        }
    }

    /// Moves accepted ranges through the edit `[a, b) → len bytes`; ranges it cuts into can no
    /// longer be reverted.
    pub fn track(&mut self, a: usize, b: usize, len: usize) {
        for entry in &mut self.entries {
            if let Entry::Accept(regions) = entry {
                for r in regions.iter_mut().filter(|r| r.valid) {
                    r.valid = shift_range(&mut r.cur, a, b, len);
                }
            }
        }
    }
}
