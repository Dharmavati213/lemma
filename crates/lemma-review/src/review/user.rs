//! Recognizes the buffer's undo and redo (`Origin::History`) of the user's own edits, so that they
//! are taken back the way they went: in the baseline where the user's edit went into it (between
//! hunks), in the proposal where it went into a hunk's.
//!
//! Without this, such a History edit is classified by where it falls when it comes, and the hunks
//! may have moved since the edit: the next recompute widens a hunk to whole lines (what the user
//! typed over a selection that ran past the end of a hunk's text went into the baseline, and its
//! undo then fell inside the hunk), and drops a hunk the user's edit made equal to the baseline
//! (the user deleted the agent's line, and its undo then fell between hunks). The undo put the
//! user's text into the proposal and left it in the baseline (a phantom change), or put the
//! agent's text into the baseline.
//!
//! The buffer's undo is last in, first out: when it undoes one of the user's edits, every later
//! action has been undone (by its own undo), so the text, and the baseline, are what they were
//! right after that edit; when it redoes one, what they were right before. So an entry keeps the
//! edit's offsets as they were, and a History edit is matched against the newest entry not undone
//! (or, for a redo, the last one undone). The buffer merges consecutive typing, and consecutive
//! deleting, into one step (its undo of "abc" typed key by key deletes the three characters at
//! once): a History edit is matched against the newest entries that, taken back (or made again)
//! one after the other, make that one edit, and the caller takes it apart into those edits. A
//! match is checked against the text by the caller.
//! An entry also keeps what its edit did to the records of Lemma's own edits (the records it cut,
//! those whose text took it in): taking it back puts those back as they were, as a record does
//! for the records its edit cut.
//!
//! Only the order of the buffer's history is assumed, nothing about how it groups edits; a History
//! edit that matches nothing is classified as before. A decision that changes the baseline outside
//! the buffer's history (an accept, or Alt+U of one) forgets where the entries went in the
//! baseline (their offsets no longer hold): their steps are still taken apart, and still bring back
//! the records they kept, but go where the Inside rule puts them, and their reverse the same way.
//! A new buffer forgets the entries.

use super::records::{Context, Pieces};
use std::collections::VecDeque;

/// Entries kept at most (the oldest go first).
const MAX_ENTRIES: usize = 1024;
/// What the entries' texts take at most (the oldest go first); a larger edit is not recorded.
const MAX_BYTES: usize = 4 << 20;

/// One of the user's edits.
#[derive(Clone, Debug)]
struct Entry {
    /// Where it started, in the text just before it.
    at: usize,
    removed: String,
    inserted: String,
    /// Where it went: into the baseline, at this offset (of `at`), or into a hunk's proposal.
    base: Option<usize>,
    /// `base` holds: no decision changed the baseline outside the buffer's history since.
    placed: bool,
    /// The text it removed, which its undo brings back, was the agent's (not accepted), and so
    /// was the text it inserted, which its redo brings back.
    removed_agents: bool,
    inserted_agents: bool,
    /// When it was made, undone or redone (`Records::tick`).
    when: u64,
    /// What it (or its last undo or redo) did to the records.
    kept: Context,
    /// Of the text it removed, and of the text it inserted, the pieces the baseline held where
    /// the edit went into the proposal only (from their start): an accept since took them out
    /// of the baseline with the rest of the hunk, and the step that brings them back puts them
    /// back into both texts (see `Part::held`).
    removed_held: Pieces,
    inserted_held: Pieces,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

/// One edit of the user's taken back or made again, as part of a [`Step`]: `[at, at + len) →
/// text`, in the text as the parts before it left it, which went into the baseline at `base`
/// (of `at`) or into the proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Part {
    pub at: usize,
    pub len: usize,
    pub text: String,
    pub base: Option<usize>,
    /// `base` holds (see `Entry::placed`).
    pub placed: bool,
    /// The text it brings back was the agent's (see `Entry::removed_agents`).
    pub agents: bool,
    /// The pieces of that text the baseline held (see `Entry::removed_held`).
    pub held: Pieces,
}

/// A History edit recognized as the undo or redo of the user's newest edits.
#[derive(Clone, Debug)]
pub(super) struct Step {
    /// How many entries it takes back or makes again.
    count: usize,
    pub redo: bool,
    /// The edits it is, one an entry, in order: each goes where its entry went (a step merged by
    /// the buffer, Backspace over the end of a hunk say, may take back edits that went different
    /// ways, and the records see each of them).
    pub parts: Vec<Part>,
    /// The text the edit removes, which the caller checks against the text.
    pub removed: String,
    /// When the first of those entries (the one the buffer steps first) was made, undone or
    /// redone.
    pub when: u64,
    /// The records whose text took in the edits it takes back (or makes again).
    pub enclosing: Vec<u64>,
}

/// The user's edits that the buffer's undo and redo may step (see the module docs).
#[derive(Clone, Debug, Default)]
pub(super) struct UserEdits {
    /// Not undone, oldest first.
    done: VecDeque<Entry>,
    /// Undone, the last one undone last.
    undone: Vec<Entry>,
    bytes: usize,
}

impl UserEdits {
    /// Forgets every entry.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Forgets where the entries went: the baseline changed outside the buffer's history.
    pub fn forget_places(&mut self) {
        for e in self.done.iter_mut().chain(&mut self.undone) {
            e.placed = false;
        }
    }

    /// Notes where the entry the last [`take`](Self::take) of a step the way `redo` says moved
    /// went this time (see [`forget_places`](Self::forget_places)).
    pub fn place_last(&mut self, redo: bool, base: Option<usize>) {
        let entry = if redo {
            self.done.back_mut()
        } else {
            self.undone.last_mut()
        };
        if let Some(e) = entry {
            e.base = base;
            e.placed = true;
        }
    }

    /// A change other than the buffer's undo and redo: a new action, which clears the buffer's
    /// redo stack.
    pub fn new_action(&mut self) {
        for e in self.undone.drain(..) {
            self.bytes -= e.bytes();
        }
    }

    /// Records the user's edit `[at, at + removed.len()) → inserted`, which went into the
    /// baseline at `base` or into the proposal (`None`), at `when`, with what it did to the
    /// records; `agents`: the text it removed was the agent's; `held`: the pieces of that text the
    /// baseline held (see `Entry::removed_held`).
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        at: usize,
        removed: String,
        inserted: String,
        base: Option<usize>,
        when: u64,
        kept: Context,
        agents: bool,
        held: Pieces,
    ) {
        self.new_action();
        let entry = Entry {
            at,
            removed,
            inserted,
            base,
            placed: true,
            removed_agents: agents,
            inserted_agents: false,
            when,
            kept,
            removed_held: held,
            inserted_held: Pieces::new(),
        };
        if entry.bytes() > MAX_BYTES {
            // Its undo matches nothing; the entries before it still match theirs after it.
            return;
        }
        self.bytes += entry.bytes();
        self.done.push_back(entry);
        while self.done.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            let Some(e) = self.done.pop_front() else {
                break;
            };
            self.bytes -= e.bytes();
        }
    }

    /// Records the buffer's undo (`redo` false) or redo `[at, at + removed.len()) → inserted`
    /// of an action that nothing recognized (its record is gone: an agent edit whose text was
    /// accepted, which is the user's own change now), which went into the baseline at `base` or
    /// into the proposal, at `when`, with what it did to the records: as an entry undone (its
    /// action made `inserted` into `removed`) or made again, so that the buffer's next step of
    /// that action is recognized and goes the same way. `agents`: the text it removed was the
    /// agent's; `held`: the pieces of that text the baseline held.
    #[allow(clippy::too_many_arguments)]
    pub fn push_step(
        &mut self,
        at: usize,
        removed: String,
        inserted: String,
        base: Option<usize>,
        when: u64,
        kept: Context,
        redo: bool,
        agents: bool,
        held: Pieces,
    ) {
        let (removed, inserted, removed_held, inserted_held) = if redo {
            (removed, inserted, held, Pieces::new())
        } else {
            (inserted, removed, Pieces::new(), held)
        };
        let entry = Entry {
            at,
            removed,
            inserted,
            base,
            placed: true,
            removed_agents: agents && redo,
            inserted_agents: agents && !redo,
            when,
            kept,
            removed_held,
            inserted_held,
        };
        if entry.bytes() > MAX_BYTES {
            return;
        }
        self.bytes += entry.bytes();
        if redo {
            self.done.push_back(entry);
        } else {
            self.undone.push(entry);
        }
        while self.done.len() + self.undone.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            // The oldest done, else the undone one the buffer's redo reaches last.
            let e = match self.done.pop_front() {
                Some(e) => e,
                None => self.undone.remove(0),
            };
            self.bytes -= e.bytes();
        }
    }

    /// The step the History edit `[a, b) → text` is, if it is the undo or the redo of the newest
    /// entries: the way the buffer steps (`redo`), or when that is only a guess (not `told`),
    /// that way first. The undo of the agent's edit can look like the redo of the user's that
    /// took it back.
    pub fn matching(&self, a: usize, b: usize, text: &str, redo: bool, told: bool) -> Option<Step> {
        self.step(a, b, text, redo)
            .or_else(|| (!told).then(|| self.step(a, b, text, !redo)).flatten())
    }

    /// Takes the entries of `step` out of the done ones (an undo) or the undone ones, at `when`,
    /// with what the step did to the records (`kept`), for the step that takes it back. Returns
    /// what their edits (or their last steps) had done.
    pub fn take(&mut self, step: &Step, when: u64, kept: Context) -> Vec<Context> {
        let mut kept = Some(kept);
        let mut had = Vec::with_capacity(step.count);
        for _ in 0..step.count {
            let entry = if step.redo {
                self.undone.pop()
            } else {
                self.done.pop_back()
            };
            let Some(mut entry) = entry else {
                break;
            };
            entry.when = when;
            had.push(std::mem::replace(
                &mut entry.kept,
                kept.take().unwrap_or_default(),
            ));
            if step.redo {
                self.done.push_back(entry);
            } else {
                self.undone.push(entry);
            }
        }
        had
    }

    /// [`matching`](Self::matching) one way.
    fn step(&self, a: usize, b: usize, text: &str, redo: bool) -> Option<Step> {
        // The `i`-th entry the step would take, in the order it takes them, as an edit: where,
        // what goes, what comes. Only as many are looked at as the edit can be.
        let piece = |i: usize| {
            let e = if redo {
                self.undone.get(self.undone.len().checked_sub(i + 1)?)?
            } else {
                self.done.get(self.done.len().checked_sub(i + 1)?)?
            };
            Some(if redo {
                (e, e.removed.as_str(), e.inserted.as_str())
            } else {
                (e, e.inserted.as_str(), e.removed.as_str())
            })
        };
        let (first, gone, came) = piece(0)?;
        let (count, removed) = if first.at == a && gone.len() == b - a && came == text {
            (1, gone.to_owned())
        } else if a == b && !text.is_empty() {
            // Insertions one after the other, all within what the first ones inserted.
            let mut block = String::new();
            let mut count = 0;
            while block.len() < text.len() {
                let (e, gone, came) = piece(count)?;
                if !gone.is_empty() || came.is_empty() {
                    return None;
                }
                block.insert_str(within(&block, e.at.checked_sub(a)?)?, came);
                count += 1;
            }
            (block == text).then_some(())?;
            (count, String::new())
        } else if a < b && text.is_empty() {
            // Deletions one after the other, all within what is left of `[a, b)`.
            let mut end = b;
            let mut count = 0;
            while end > a {
                let (e, gone, came) = piece(count)?;
                if !came.is_empty() || gone.is_empty() || e.at < a || e.at + gone.len() > end {
                    return None;
                }
                end -= gone.len();
                count += 1;
            }
            // What they delete: their deletions taken back, the last one first.
            let mut block = String::new();
            for i in (0..count).rev() {
                let (e, gone, _) = piece(i)?;
                block.insert_str(within(&block, e.at.checked_sub(a)?)?, gone);
            }
            (count, block)
        } else {
            return None;
        };
        let pieces: Vec<_> = (0..count).filter_map(piece).collect();
        let parts = pieces
            .iter()
            .map(|&(e, gone, came)| Part {
                at: e.at,
                len: gone.len(),
                text: came.to_owned(),
                base: e.base,
                placed: e.placed,
                agents: if redo {
                    e.inserted_agents
                } else {
                    e.removed_agents
                },
                held: if redo {
                    e.inserted_held.clone()
                } else {
                    e.removed_held.clone()
                },
            })
            .collect();
        let mut enclosing: Vec<u64> = pieces
            .iter()
            .flat_map(|(e, _, _)| e.kept.enclosing().iter().copied())
            .collect();
        enclosing.sort_unstable();
        enclosing.dedup();
        Some(Step {
            count,
            redo,
            parts,
            removed,
            when: first.when,
            enclosing,
        })
    }
}

/// `k`, if it is a place in `block` (at a char boundary).
fn within(block: &str, k: usize) -> Option<usize> {
    block.is_char_boundary(k).then_some(k)
}

#[cfg(test)]
mod tests {
    use super::{Context, UserEdits};

    /// Typing "abc" key by key and its undo in one deletion, then the redo in one insertion;
    /// Backspace three times and its undo in one insertion.
    #[test]
    fn merged_steps_match_the_entries_they_take() {
        let mut u = UserEdits::default();
        for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
            u.push(
                10 + i,
                String::new(),
                c.into(),
                Some(5 + i),
                i as u64,
                Context::default(),
                false,
                Vec::new(),
            );
        }
        assert!(
            u.matching(10, 12, "", false, true).is_none(),
            "only part of it"
        );
        let step = u.matching(10, 13, "", false, true).expect("the undo");
        assert_eq!((step.count, step.redo), (3, false));
        let parts: Vec<_> = step.parts.iter().map(|p| (p.at, p.len, p.base)).collect();
        assert_eq!(
            parts,
            [(12, 1, Some(7)), (11, 1, Some(6)), (10, 1, Some(5))]
        );
        assert_eq!(step.removed, "abc");
        u.take(&step, 7, Context::default());
        assert!(
            u.matching(10, 10, "abc", false, true).is_none(),
            "not an undo"
        );
        let step = u
            .matching(10, 10, "abc", false, false)
            .expect("the redo, guessed");
        assert_eq!((step.count, step.redo), (3, true));
        assert_eq!(step.parts[0].text, "a");
        u.take(&step, 8, Context::default());
        // Backspace over "cba" (the last key first), into a hunk's proposal.
        let mut u = UserEdits::default();
        for (i, c) in ["c", "b", "a"].into_iter().enumerate() {
            u.push(
                12 - i,
                c.into(),
                String::new(),
                None,
                i as u64,
                Context::default(),
                false,
                Vec::new(),
            );
        }
        let step = u.matching(10, 10, "abc", false, true).expect("the undo");
        assert_eq!(step.count, 3);
        assert!(step.parts.iter().all(|p| p.base.is_none()));
        // The Delete key, three times at one place.
        let mut u = UserEdits::default();
        for (i, c) in ["a", "b", "c"].into_iter().enumerate() {
            u.push(
                10,
                c.into(),
                String::new(),
                None,
                i as u64,
                Context::default(),
                false,
                Vec::new(),
            );
        }
        let step = u.matching(10, 10, "abc", false, true).expect("the undo");
        assert_eq!(step.count, 3);
        u.take(&step, 4, Context::default());
        let step = u.matching(10, 13, "", true, true).expect("the redo");
        assert_eq!((step.count, step.removed.as_str()), (3, "abc"));
    }

    /// A step merged by the buffer takes back edits that went different ways each its way.
    #[test]
    fn a_step_takes_each_edit_back_its_way() {
        let mut u = UserEdits::default();
        u.push(
            10,
            String::new(),
            "a".into(),
            Some(10),
            0,
            Context::default(),
            false,
            Vec::new(),
        );
        u.push(
            11,
            String::new(),
            "b".into(),
            None,
            1,
            Context::default(),
            false,
            Vec::new(),
        );
        let step = u.matching(10, 12, "", false, true).expect("both");
        let parts: Vec<_> = step.parts.iter().map(|p| (p.at, p.base)).collect();
        assert_eq!(parts, [(11, None), (10, Some(10))]);
    }
}
