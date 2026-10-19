// Owner: WP1.7 scanner
//! Incremental per-line scanner states.

use crate::context;
use crate::lex::{self, Frame, FxBuild, Tables};
use crate::tokens::{Collect, Discard};
use crate::{Context, Lines, ScanConfig, Token};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

/// Id of the empty state (document text level), interned first.
const TEXT_STATE: u32 = 0;

/// Interned states are compacted when this many exist, so a long editing session cannot grow
/// the table without bound.
const COMPACT_AT: usize = 4096;

/// Interned scanner state per line start (~4 bytes/line).
///
/// `states[i]` is the id of the state at the start of line `i`. A document has few distinct
/// states (text level, inside display math, inside a multi-line argument, …), so they are
/// interned and each line stores a `u32`.
///
/// A blank line ends `$…$`, `$$…$$`, `\(…\)`, `\[…\]` and unclosed command arguments, as TeX's
/// `\par` does; this also keeps the rescan after typing an unmatched delimiter within the
/// paragraph. Math environments stay open across blank lines, as in VimTeX.
pub struct LineStates {
    states: Vec<u32>,
    cfg: Arc<ScanConfig>,
    tables: Tables,
    interner: Interner,
}

impl LineStates {
    /// Empty states; call [`rebuild`](Self::rebuild) before use.
    pub fn new(cfg: Arc<ScanConfig>) -> Self {
        let tables = Tables::new(&cfg);
        Self {
            states: Vec::new(),
            cfg,
            tables,
            interner: Interner::new(),
        }
    }

    /// The configuration the states were built with.
    pub fn config(&self) -> &Arc<ScanConfig> {
        &self.cfg
    }

    /// Number of lines whose start state is known (0 before the first [`rebuild`](Self::rebuild)).
    pub fn line_count(&self) -> u32 {
        // LineIndex limits documents to u32 lines.
        self.states.len() as u32
    }

    /// Scans the whole document.
    pub fn rebuild(&mut self, src: &impl Lines) {
        let count = src.line_count();
        self.interner = Interner::new();
        self.states.clear();
        self.states.reserve(count as usize);
        let mut frames = Vec::new();
        let mut state = TEXT_STATE;
        for i in 0..count {
            self.states.push(state);
            state = self.advance(state, &mut frames, src.line(i).as_bytes());
        }
    }

    /// Lines [first, first+removed) were replaced by `added` lines (already applied to `src`).
    /// Rescans until the state at a line start equals the old one. Returns lines whose tokens may differ.
    ///
    /// The returned range is in post-edit line numbers and always contains the `added` lines.
    /// The counts are those of [`LineIndex::line_delta`](lemma_core::LineIndex::line_delta). If they
    /// do not match `src` (or [`rebuild`](Self::rebuild) was never called), the whole document is
    /// rescanned and every line is returned.
    pub fn apply_edit(
        &mut self,
        src: &impl Lines,
        first: u32,
        removed: u32,
        added: u32,
    ) -> Range<u32> {
        let total = src.line_count();
        let old = self.line_count();
        let consistent = first < old
            && first
                .checked_add(removed)
                .is_some_and(|end| end <= old && old - removed + added == total);
        if !consistent {
            self.rebuild(src);
            return 0..total;
        }
        if self.interner.len() > COMPACT_AT {
            self.compact();
        }

        // Line `first` starts in the same state as before: only lines before it determine it.
        let start = self.states[first as usize];
        let at = first as usize;
        self.states.splice(
            at..at + removed as usize,
            std::iter::repeat_n(start, added as usize),
        );
        if first >= total {
            return first..first;
        }
        // With no added line, line `first` is an old line whose start state may have changed.
        self.states[at] = start;
        let rescan_to = first + added.max(1);

        let mut frames = Vec::new();
        let mut state = start;
        let mut line = first;
        loop {
            state = self.advance(state, &mut frames, src.line(line).as_bytes());
            line += 1;
            if line >= total {
                break;
            }
            let slot = &mut self.states[line as usize];
            if line >= rescan_to && *slot == state {
                break;
            }
            *slot = state;
        }
        first..line
    }

    /// Appends the tokens of line `line_no` (whose text is `line`) to `out`.
    ///
    /// The tokens tile the line (see [`Token`]). A line past the known states is scanned from the
    /// text level.
    pub fn tokens(&self, line_no: u32, line: &str, out: &mut Vec<Token>) {
        let mut frames = self.start_frames(line_no);
        lex::scan(
            &self.tables,
            &mut frames,
            line.as_bytes(),
            line.len(),
            Collect::new(out),
        );
    }

    /// `byte` = cursor byte offset within `line`. Must take < 50 µs.
    ///
    /// The context is decided by the text before the cursor, so the cursor between the two
    /// dollars of `$$` (the smart `$` pair) is in inline math. A `byte` past the end of the
    /// line means its end.
    pub fn context_at(&self, line_no: u32, line: &str, byte: u32) -> Context {
        let mut frames = self.start_frames(line_no);
        let bytes = line.as_bytes();
        let end = (byte as usize).min(bytes.len());
        let scanned = lex::scan(&self.tables, &mut frames, bytes, end, Discard);
        context::resolve(
            &self.tables,
            &frames,
            scanned.stop,
            scanned.arg_start,
            bytes,
            end,
        )
    }

    /// The state at the start of `line_no` as a frame stack.
    fn start_frames(&self, line_no: u32) -> Vec<Frame> {
        let id = self
            .states
            .get(line_no as usize)
            .copied()
            .unwrap_or(TEXT_STATE);
        let mut frames = Vec::with_capacity(8);
        frames.extend_from_slice(self.interner.get(id));
        frames
    }

    /// Scans one line from `state` and returns the state at the start of the next line.
    fn advance(&mut self, state: u32, frames: &mut Vec<Frame>, line: &[u8]) -> u32 {
        frames.clear();
        frames.extend_from_slice(self.interner.get(state));
        lex::scan(&self.tables, frames, line, line.len(), Discard);
        if frames.as_slice() == self.interner.get(state) {
            state
        } else {
            self.interner.intern(frames)
        }
    }

    /// Re-interns the states still in use.
    fn compact(&mut self) {
        let mut fresh = Interner::new();
        let mut remap = vec![u32::MAX; self.interner.len()];
        for id in &mut self.states {
            let new = &mut remap[*id as usize];
            if *new == u32::MAX {
                *new = fresh.intern(self.interner.get(*id));
            }
            *id = *new;
        }
        self.interner = fresh;
    }
}

impl PartialEq for LineStates {
    /// Equal when built with equal configurations and every line starts in the same state.
    fn eq(&self, other: &Self) -> bool {
        self.cfg == other.cfg
            && self.states.len() == other.states.len()
            && self
                .states
                .iter()
                .zip(&other.states)
                .all(|(&a, &b)| self.interner.get(a) == other.interner.get(b))
    }
}

impl fmt::Debug for LineStates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineStates")
            .field("lines", &self.states.len())
            .field("interned_states", &self.interner.len())
            .finish_non_exhaustive()
    }
}

/// Distinct frame stacks, numbered in order of appearance.
struct Interner {
    states: Vec<Box<[Frame]>>,
    ids: HashMap<Box<[Frame]>, u32, FxBuild>,
}

impl Interner {
    fn new() -> Self {
        let mut interner = Self {
            states: Vec::new(),
            ids: HashMap::default(),
        };
        interner.intern(&[]);
        interner
    }

    fn len(&self) -> usize {
        self.states.len()
    }

    fn get(&self, id: u32) -> &[Frame] {
        self.states.get(id as usize).map_or(&[], |s| s)
    }

    fn intern(&mut self, frames: &[Frame]) -> u32 {
        if let Some(&id) = self.ids.get(frames) {
            return id;
        }
        // At most MAX_FRAMES-deep stacks over small alphabets; never near u32::MAX entries
        // because `apply_edit` compacts.
        let id = self.states.len() as u32;
        let boxed: Box<[Frame]> = frames.into();
        self.states.push(boxed.clone());
        self.ids.insert(boxed, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StrLines;
    use lemma_core::LineIndex;

    fn states_for(text: &str) -> LineStates {
        let index = LineIndex::new(text);
        let mut states = LineStates::new(Arc::new(ScanConfig::default()));
        states.rebuild(&StrLines {
            text,
            index: &index,
        });
        states
    }

    #[test]
    fn states_can_move_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LineStates>();
    }

    #[test]
    fn interning_shares_states() {
        let states = states_for("a\n\\[\nx\ny\n\\]\nb\n$c\nd$\n");
        assert_eq!(states.line_count(), 9);
        // Text level, inside \[, inside $.
        assert_eq!(states.interner.len(), 3);
        assert_eq!(states.states, [0, 0, 1, 1, 1, 0, 0, 2, 0]);
    }

    #[test]
    fn compaction_keeps_states() {
        let text = "\\cite{a,\nb}\n\\[\n x\n\\]\n";
        let mut states = states_for(text);
        let before: Vec<Vec<Frame>> = (0..states.line_count())
            .map(|l| states.interner.get(states.states[l as usize]).to_vec())
            .collect();
        for depth in 0..50u8 {
            states.interner.intern(&[Frame::Ensure { depth }]);
        }
        states.compact();
        let after: Vec<Vec<Frame>> = (0..states.line_count())
            .map(|l| states.interner.get(states.states[l as usize]).to_vec())
            .collect();
        assert_eq!(before, after);
        assert_eq!(states.interner.len(), 3);
    }

    #[test]
    fn inconsistent_edit_rebuilds() {
        let text = "a\nb\n";
        let index = LineIndex::new(text);
        let src = StrLines {
            text,
            index: &index,
        };
        let mut states = LineStates::new(Arc::new(ScanConfig::default()));
        assert_eq!(states.apply_edit(&src, 0, 1, 1), 0..3);
        assert_eq!(states.apply_edit(&src, 1, 5, 1), 0..3);
        assert_eq!(states.line_count(), 3);
    }
}
