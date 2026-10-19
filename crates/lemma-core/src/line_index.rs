use crate::TextEdit;
use std::ops::Range;

/// Byte offsets of line starts. Line count = number of '\n' + 1 (same as GtkTextBuffer).
///
/// The index also tracks the text length, so it can be kept exact by mirroring every buffer
/// change through [`LineIndex::apply`] and compared against a fresh [`LineIndex::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineIndex {
    /// `starts[0] == 0`; `starts[i]` is the byte after the `i`-th `'\n'`.
    starts: Vec<usize>,
    len: usize,
}

impl Default for LineIndex {
    /// The index of the empty text: one empty line.
    fn default() -> Self {
        Self::new("")
    }
}

impl LineIndex {
    /// Indexes `text`.
    pub fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(1 + text.len() / 40);
        starts.push(0);
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self {
            starts,
            len: text.len(),
        }
    }

    /// Number of lines; at least 1.
    pub fn line_count(&self) -> u32 {
        // More than u32::MAX lines (> 4 GiB of newlines) is not supported.
        self.starts.len() as u32
    }

    /// Byte offset of the first byte of `line`.
    ///
    /// # Panics
    /// If `line >= self.line_count()`.
    pub fn line_start(&self, line: u32) -> usize {
        self.starts[line as usize]
    }

    /// Byte offset of (`line`, `col_byte`). `col_byte` = GtkTextIter::line_index().
    ///
    /// # Panics
    /// If `line >= self.line_count()`.
    pub fn offset(&self, line: u32, col_byte: u32) -> usize {
        self.line_start(line) + col_byte as usize
    }

    /// (line, col_byte) of a byte offset. Offsets past the end map into the last line.
    pub fn position(&self, offset: usize) -> (u32, u32) {
        let line = self.line_of(offset);
        (line, (offset - self.starts[line as usize]) as u32)
    }

    /// The line containing `offset`. The offset of a `'\n'` belongs to the line it ends;
    /// offsets past the end map to the last line.
    pub fn line_of(&self, offset: usize) -> u32 {
        // starts[0] == 0 <= offset, so the partition point is at least 1.
        (self.starts.partition_point(|&s| s <= offset) - 1) as u32
    }

    /// Length in bytes of the indexed text.
    pub fn text_len(&self) -> usize {
        self.len
    }

    /// Byte range of `line`'s content, excluding its terminating `'\n'`.
    ///
    /// # Panics
    /// If `line >= self.line_count()`.
    pub fn line_range(&self, line: u32) -> Range<usize> {
        let i = line as usize;
        let end = match self.starts.get(i + 1) {
            Some(&next) => next - 1,
            None => self.len,
        };
        self.starts[i]..end
    }

    /// Update for an edit expressed in pre-edit offsets. O(lines after the edit).
    pub fn apply(&mut self, e: &TextEdit) {
        let (a, b) = (e.range.start, e.range.end);
        debug_assert!(
            a <= b && b <= self.len,
            "edit {a}..{b} out of bounds for a text of {} bytes",
            self.len
        );
        let removed = b - a;
        let added = e.text.len();
        // Line starts in (a, b] were created by newlines inside the replaced range.
        let lo = self.starts.partition_point(|&s| s <= a);
        let hi = self.starts.partition_point(|&s| s <= b);
        for s in &mut self.starts[hi..] {
            // s > b >= removed, so this never underflows.
            *s = *s - removed + added;
        }
        self.starts
            .splice(lo..hi, e.text.match_indices('\n').map(|(i, _)| a + i + 1));
        self.len = self.len - removed + added;
    }

    /// The line-level shape of an edit: (first line, lines removed, lines added), for LineStates.
    ///
    /// Lines `[first, first + removed)` of the pre-edit text are replaced by lines
    /// `[first, first + added)` of the post-edit text; every other line is unchanged (only shifted).
    /// Both counts are at least 1, because the line containing the edit start is always reported.
    /// Call it before [`LineIndex::apply`], since `e` is in pre-edit offsets.
    pub fn line_delta(&self, e: &TextEdit) -> (u32, u32, u32) {
        let first = self.line_of(e.range.start);
        let last = self.line_of(e.range.end);
        let added = e.text.matches('\n').count() as u32 + 1;
        (first, last - first + 1, added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::Index;

    #[test]
    fn new_counts_lines_like_gtk() {
        assert_eq!(LineIndex::new("").line_count(), 1);
        assert_eq!(LineIndex::new("abc").line_count(), 1);
        assert_eq!(LineIndex::new("abc\n").line_count(), 2);
        assert_eq!(LineIndex::new("\n\n").line_count(), 3);
        assert_eq!(LineIndex::default(), LineIndex::new(""));
    }

    #[test]
    fn offsets_and_positions() {
        let text = "ab\ncdé\n\nx";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 4);
        assert_eq!(
            (0..4).map(|l| idx.line_start(l)).collect::<Vec<_>>(),
            [0, 3, 8, 9]
        );
        assert_eq!(idx.offset(1, 2), 5);
        assert_eq!(idx.position(0), (0, 0));
        assert_eq!(idx.position(2), (0, 2)); // the '\n' belongs to line 0
        assert_eq!(idx.position(3), (1, 0));
        assert_eq!(idx.position(7), (1, 4));
        assert_eq!(idx.position(8), (2, 0));
        assert_eq!(idx.position(10), (3, 1)); // end of text
        assert_eq!(idx.position(99), (3, 90)); // past the end: last line
        assert_eq!(idx.line_of(9), 3);
        assert_eq!(idx.text_len(), text.len());
    }

    #[test]
    fn line_ranges_exclude_newlines() {
        let text = "ab\ncdé\n\nx";
        let idx = LineIndex::new(text);
        let lines: Vec<&str> = (0..idx.line_count())
            .map(|l| &text[idx.line_range(l)])
            .collect();
        assert_eq!(lines, ["ab", "cdé", "", "x"]);
        let idx = LineIndex::new("a\n");
        assert_eq!(idx.line_range(1), 2..2);
    }

    #[test]
    fn apply_simple_cases() {
        let mut text = String::from("one\ntwo\nthree");
        let mut idx = LineIndex::new(&text);
        let edits = [
            // "one\nnew\ntwo\nthree"
            TextEdit::insert(3, "\nnew"),
            // "new\ntwo\nthree"
            TextEdit::delete(0..4),
            // "new\ntwo\nthree\n"
            TextEdit::insert(13, "\n"),
            TextEdit::delete(0..0),
            // "neX\nY\nZo\nthree\n"
            TextEdit {
                range: 2..7,
                text: "X\nY\nZ".into(),
            },
        ];
        for e in &edits {
            e.apply(&mut text);
            idx.apply(e);
            assert_eq!(idx, LineIndex::new(&text), "after {e:?} on {text:?}");
        }
    }

    #[test]
    fn line_delta_cases() {
        let idx = LineIndex::new("l0\nl1\nl2\nl3");
        // Typing a character inside line 1.
        assert_eq!(idx.line_delta(&TextEdit::insert(4, "x")), (1, 1, 1));
        // Pressing Enter in line 1.
        assert_eq!(idx.line_delta(&TextEdit::insert(4, "\n")), (1, 1, 2));
        // Pasting two lines at the start of the text.
        assert_eq!(idx.line_delta(&TextEdit::insert(0, "a\nb\n")), (0, 1, 3));
        // Deleting the newline that ends line 1 joins lines 1 and 2.
        assert_eq!(idx.line_delta(&TextEdit::delete(5..6)), (1, 2, 1));
        // Deleting the whole of line 1 including its newline.
        assert_eq!(idx.line_delta(&TextEdit::delete(3..6)), (1, 2, 1));
        // Replacing lines 1..=2 by one line.
        let e = TextEdit {
            range: 3..8,
            text: "joined".into(),
        };
        assert_eq!(idx.line_delta(&e), (1, 2, 1));
        // Appending at the very end.
        assert_eq!(idx.line_delta(&TextEdit::insert(11, "\nl4")), (3, 1, 2));
    }

    /// Text over a small alphabet with frequent newlines and multibyte characters.
    fn text() -> impl Strategy<Value = String> {
        "([ab é∑]{0,3}\n?){0,12}"
    }

    /// 1000 cases by default; `PROPTEST_CASES` overrides it.
    fn config() -> ProptestConfig {
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        ProptestConfig::with_cases(cases)
    }

    /// Picks a char-boundary edit in `text` from two random indices and an insertion.
    fn make_edit(text: &str, i: Index, j: Index, insert: String) -> TextEdit {
        let bounds: Vec<usize> = text
            .char_indices()
            .map(|(k, _)| k)
            .chain([text.len()])
            .collect();
        let (x, y) = (*i.get(&bounds), *j.get(&bounds));
        TextEdit {
            range: x.min(y)..x.max(y),
            text: insert,
        }
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn apply_matches_fresh_index(
            start in text(),
            ops in prop::collection::vec((any::<Index>(), any::<Index>(), text()), 1..20),
        ) {
            let mut text = start;
            let mut idx = LineIndex::new(&text);
            for (i, j, insert) in ops {
                let e = make_edit(&text, i, j, insert);
                let old_lines: Vec<String> = (0..idx.line_count())
                    .map(|l| text[idx.line_range(l)].to_owned())
                    .collect();
                let (first, removed, added) = idx.line_delta(&e);

                e.apply(&mut text);
                idx.apply(&e);
                prop_assert_eq!(&idx, &LineIndex::new(&text));

                // line_delta describes exactly which lines changed.
                let new_lines: Vec<String> = (0..idx.line_count())
                    .map(|l| text[idx.line_range(l)].to_owned())
                    .collect();
                let (f, r, a) = (first as usize, removed as usize, added as usize);
                prop_assert!(r >= 1 && a >= 1);
                prop_assert_eq!(new_lines.len(), old_lines.len() - r + a);
                prop_assert_eq!(&old_lines[..f], &new_lines[..f]);
                prop_assert_eq!(&old_lines[f + r..], &new_lines[f + a..]);
            }
        }

        #[test]
        fn position_round_trips(text in text(), i in any::<Index>()) {
            let idx = LineIndex::new(&text);
            let bounds: Vec<usize> = text.char_indices().map(|(k, _)| k).chain([text.len()]).collect();
            let offset = *i.get(&bounds);
            let (line, col) = idx.position(offset);
            prop_assert_eq!(idx.offset(line, col), offset);
            prop_assert!(idx.line_range(line).contains(&offset) || idx.line_range(line).end == offset);
        }
    }
}
