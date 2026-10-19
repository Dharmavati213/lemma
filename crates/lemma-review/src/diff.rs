//! Line diff shared by `FileReview::recompute`, `minimal_edits` and `unified_diff`.
//!
//! Lines are split at `'\n'` only and keep it (the last line may lack it), so `'\r'` is ordinary
//! content and line `i` here is line `i` of `lemma_core::LineIndex`. The common line-aligned prefix
//! and suffix are cut off before Myers runs, which keeps big files with small changes cheap.
//!
//! The line diff has no time limit: it is similar's Myers, whose search is bounded by the size of
//! the input (a Git/xdiff-style work limit) rather than by the clock. Equal texts therefore always
//! give equal hunks (and equal [`HunkId`](crate::HunkId)s), whatever the machine load. Only the
//! intra-line word spans, which affect the drawing alone, are refined under [`WORD_TIMEOUT`].

use crate::text::{common_prefix_bytes, common_suffix_bytes, count_newlines, to_u32};
use crate::{Side, WordSpan};
use similar::{Algorithm, ChangeTag, DiffOp, DiffTag, InlineChangeOptions, TextDiff};
use std::ops::Range;
use std::time::{Duration, Instant};

/// Time budget for the word spans of one diff; past it, the remaining replacements get coarser
/// (or no) word spans. The line hunks never depend on it.
pub(crate) const WORD_TIMEOUT: Duration = Duration::from_millis(8);

/// Replacements with more lines than this on either side get no word spans.
const MAX_WORD_LINES: usize = 200;
/// Replacements with more bytes than this on either side get no word spans.
const MAX_WORD_BYTES: usize = 64 * 1024;
/// A line whose changed words cover more than this share of its non-blank characters is mostly
/// rewritten: it gets no word spans and is shown as a whole changed line (as VS Code and Zed do).
const MAX_WORD_SHARE: f64 = 0.5;

/// A run of changed lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineHunk {
    pub old_lines: Range<u32>,
    pub new_lines: Range<u32>,
    pub old_bytes: Range<usize>,
    pub new_bytes: Range<usize>,
    /// Word spans; lines relative to the hunk's first line on each side.
    pub words: Vec<WordSpan>,
}

/// The lines of one side's differing middle part.
struct Lines<'a> {
    lines: Vec<&'a str>,
    /// Absolute byte offset of every line, followed by the end of the middle part.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(middle: &'a str, at: usize) -> Self {
        let lines: Vec<&str> = middle.split_inclusive('\n').collect();
        let mut starts = Vec::with_capacity(lines.len() + 1);
        let mut pos = at;
        for line in &lines {
            starts.push(pos);
            pos += line.len();
        }
        starts.push(pos);
        Self { lines, starts }
    }

    fn bytes(&self, lines: &Range<usize>) -> Range<usize> {
        self.starts[lines.start]..self.starts[lines.end]
    }
}

/// A line diff of two texts.
pub(crate) struct LineDiff<'a> {
    /// Lines of the common prefix; line 0 of the middle parts is this line of both texts.
    prefix_lines: usize,
    old: Lines<'a>,
    new: Lines<'a>,
    diff: TextDiff<'a, 'a, str>,
}

impl<'a> LineDiff<'a> {
    /// Diffs `old` against `new`.
    pub fn new(old: &'a str, new: &'a str) -> Self {
        let (ob, nb) = (old.as_bytes(), new.as_bytes());
        let prefix = {
            let p = common_prefix_bytes(ob, nb);
            ob[..p]
                .iter()
                .rposition(|&c| c == b'\n')
                .map_or(0, |i| i + 1)
        };
        // The common suffix after the prefix, moved forward to a line start in both texts.
        let mut suffix = common_suffix_bytes(&ob[prefix..], &nb[prefix..]);
        let line_start = |t: &[u8], i: usize| i == prefix || t[i - 1] == b'\n';
        if suffix > 0 && !(line_start(ob, ob.len() - suffix) && line_start(nb, nb.len() - suffix)) {
            let from = ob.len() - suffix;
            suffix = match ob[from..].iter().position(|&c| c == b'\n') {
                Some(r) => suffix - (r + 1),
                None => 0,
            };
        }
        let old_lines = Lines::new(&old[prefix..old.len() - suffix], prefix);
        let new_lines = Lines::new(&new[prefix..new.len() - suffix], prefix);
        // No deadline: the result must depend on the texts only (see the module docs).
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_slices(&old_lines.lines, &new_lines.lines);
        Self {
            prefix_lines: count_newlines(&ob[..prefix]),
            old: old_lines,
            new: new_lines,
            diff,
        }
    }

    /// The changed regions in text order. With `words`, replacements get intra-line word spans
    /// (refined for at most [`WORD_TIMEOUT`]).
    pub fn hunks(&self, words: bool) -> Vec<LineHunk> {
        let deadline = Instant::now() + WORD_TIMEOUT;
        let ops = self.diff.ops();
        let mut out = Vec::new();
        let mut i = 0;
        while i < ops.len() {
            if ops[i].tag() == DiffTag::Equal {
                i += 1;
                continue;
            }
            let j = ops[i..]
                .iter()
                .position(|op| op.tag() == DiffTag::Equal)
                .map_or(ops.len(), |n| i + n);
            let group = &ops[i..j];
            let old = group[0].old_range().start..group[group.len() - 1].old_range().end;
            let new = group[0].new_range().start..group[group.len() - 1].new_range().end;
            let mut spans = Vec::new();
            if words {
                for op in group.iter().filter(|op| op.tag() == DiffTag::Replace) {
                    self.word_spans(op, old.start, new.start, deadline, &mut spans);
                }
            }
            let absolute = |r: &Range<usize>| {
                to_u32(self.prefix_lines + r.start)..to_u32(self.prefix_lines + r.end)
            };
            out.push(LineHunk {
                old_lines: absolute(&old),
                new_lines: absolute(&new),
                old_bytes: self.old.bytes(&old),
                new_bytes: self.new.bytes(&new),
                words: spans,
            });
            i = j;
        }
        out
    }

    /// Appends the emphasized words of a replacement op; `old0`/`new0` are the hunk's first lines.
    fn word_spans(
        &self,
        op: &DiffOp,
        old0: usize,
        new0: usize,
        deadline: Instant,
        out: &mut Vec<WordSpan>,
    ) {
        let (o, n) = (op.old_range(), op.new_range());
        let too_big = |lines: &Lines, r: &Range<usize>| {
            r.len() > MAX_WORD_LINES || lines.bytes(r).len() > MAX_WORD_BYTES
        };
        if too_big(&self.old, &o) || too_big(&self.new, &n) {
            return;
        }
        let changes = self.diff.iter_inline_changes_with_options_deadline(
            op,
            InlineChangeOptions::new(),
            Some(deadline),
        );
        let mut spans: Vec<Range<usize>> = Vec::new();
        for change in changes {
            let (side, line) = match (change.tag(), change.old_index(), change.new_index()) {
                (ChangeTag::Delete, Some(i), _) => (Side::Old, i - old0),
                (ChangeTag::Insert, _, Some(i)) => (Side::New, i - new0),
                _ => continue,
            };
            spans.clear();
            let (mut changed, mut total) = (0usize, 0usize);
            let mut at = 0;
            for &(emphasized, value) in change.values() {
                let end = at + value.len();
                let visible = value.chars().filter(|c| !c.is_whitespace()).count();
                total += visible;
                if emphasized && end > at {
                    changed += visible;
                    match spans.last_mut() {
                        Some(r) if r.end == at => r.end = end,
                        _ => spans.push(at..end),
                    }
                }
                at = end;
            }
            if changed as f64 > MAX_WORD_SHARE * total as f64 {
                // Mostly rewritten: the whole line is the change.
                continue;
            }
            out.extend(spans.iter().map(|bytes| WordSpan {
                side,
                line: to_u32(line),
                bytes: to_u32(bytes.start)..to_u32(bytes.end),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunks(old: &str, new: &str) -> Vec<LineHunk> {
        LineDiff::new(old, new).hunks(true)
    }

    fn summary(old: &str, new: &str) -> Vec<(Range<u32>, Range<u32>, String, String)> {
        hunks(old, new)
            .into_iter()
            .map(|h| {
                (
                    h.old_lines,
                    h.new_lines,
                    old[h.old_bytes].to_owned(),
                    new[h.new_bytes].to_owned(),
                )
            })
            .collect()
    }

    fn h(
        o: Range<u32>,
        n: Range<u32>,
        a: &str,
        b: &str,
    ) -> (Range<u32>, Range<u32>, String, String) {
        (o, n, a.to_owned(), b.to_owned())
    }

    #[test]
    fn identical_texts_have_no_hunks() {
        assert!(hunks("", "").is_empty());
        assert!(hunks("a\nb", "a\nb").is_empty());
        assert!(hunks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn insertions_and_deletions() {
        assert_eq!(summary("a\nc\n", "a\nb\nc\n"), [h(1..1, 1..2, "", "b\n")]);
        assert_eq!(summary("b\nc\n", "a\nb\nc\n"), [h(0..0, 0..1, "", "a\n")]);
        assert_eq!(summary("a\n", "a\nb\n"), [h(1..1, 1..2, "", "b\n")]);
        assert_eq!(summary("a\nb\nc\n", "a\nc\n"), [h(1..2, 1..1, "b\n", "")]);
        assert_eq!(summary("a\nb\n", "a\n"), [h(1..2, 1..1, "b\n", "")]);
        assert_eq!(summary("", "x\ny\n"), [h(0..0, 0..2, "", "x\ny\n")]);
        assert_eq!(summary("x\ny\n", ""), [h(0..2, 0..0, "x\ny\n", "")]);
    }

    #[test]
    fn missing_final_newline() {
        assert_eq!(summary("a\nb", "a\nb\n"), [h(1..2, 1..2, "b", "b\n")]);
        assert_eq!(summary("a\nb\n", "a\nb"), [h(1..2, 1..2, "b\n", "b")]);
        assert_eq!(summary("a\nb", "a\nc"), [h(1..2, 1..2, "b", "c")]);
        assert_eq!(summary("a\nb", "a\n"), [h(1..2, 1..1, "b", "")]);
        assert_eq!(summary("a", ""), [h(0..1, 0..0, "a", "")]);
    }

    #[test]
    fn carriage_returns_are_content() {
        // '\r' alone does not end a line; "\r\n" lines differ from "\n" lines.
        assert_eq!(
            summary("a\rb\nc\n", "a\rB\nc\n"),
            [h(0..1, 0..1, "a\rb\n", "a\rB\n")]
        );
        assert_eq!(
            summary("a\r\nb\r\n", "a\r\nb\n"),
            [h(1..2, 1..2, "b\r\n", "b\n")]
        );
    }

    #[test]
    fn suffix_cut_stays_line_aligned() {
        // Common byte suffix "c\n" starts mid-line in both texts.
        assert_eq!(
            summary("xabc\n", "yabc\n"),
            [h(0..1, 0..1, "xabc\n", "yabc\n")]
        );
        // The common suffix would overlap the prefix.
        assert_eq!(summary("ab\nab\n", "ab\n"), [h(1..2, 1..1, "ab\n", "")]);
        assert_eq!(summary("aaa", "aaaa"), [h(0..1, 0..1, "aaa", "aaaa")]);
    }

    #[test]
    fn two_separate_hunks() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "1\nX\n3\n4\nY\n";
        assert_eq!(
            summary(old, new),
            [h(1..2, 1..2, "2\n", "X\n"), h(4..5, 4..5, "5\n", "Y\n")]
        );
    }

    #[test]
    fn word_spans_on_both_sides() {
        let hs = hunks("a\nlet x = alpha + 1;\nb\n", "a\nlet x = beta + 1;\nb\n");
        assert_eq!(hs.len(), 1);
        let words = &hs[0].words;
        assert!(words.contains(&WordSpan {
            side: Side::Old,
            line: 0,
            bytes: 8..13
        }));
        assert!(words.contains(&WordSpan {
            side: Side::New,
            line: 0,
            bytes: 8..12
        }));
        assert_eq!(words.len(), 2);
    }
}
