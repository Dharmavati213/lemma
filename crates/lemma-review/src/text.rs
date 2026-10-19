//! Byte-level text helpers shared by the diff and review code.

use lemma_core::TextEdit;
use std::ops::Range;

/// Number of `'\n'` bytes in `bytes`.
pub(crate) fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
}

/// Number of `'\n'` in `s`, as a line count.
pub(crate) fn newlines(s: &str) -> u32 {
    to_u32(count_newlines(s.as_bytes()))
}

/// Saturating conversion for line numbers and in-line byte offsets.
pub(crate) fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The lines touched by `text` when it starts on line `first`: `first..first` for an empty text,
/// otherwise every line holding at least one of its bytes (a final `'\n'` belongs to its line).
pub(crate) fn line_span(first: u32, text: &str) -> Range<u32> {
    match text.as_bytes().split_last() {
        None => first..first,
        Some((_, init)) => {
            first
                ..first
                    .saturating_add(to_u32(count_newlines(init)))
                    .saturating_add(1)
        }
    }
}

/// `x + delta`, saturating (positions never leave `0..=usize::MAX`).
pub(crate) fn offset(x: usize, delta: isize) -> usize {
    x.saturating_add_signed(delta)
}

/// Byte length as a signed delta.
pub(crate) fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

/// `line + delta`, clamped to `u32`.
pub(crate) fn add_lines(line: u32, delta: i64) -> u32 {
    u32::try_from((i64::from(line) + delta).max(0)).unwrap_or(u32::MAX)
}

/// Shifts both ends of a line range.
pub(crate) fn shift_lines(r: &mut Range<u32>, delta: i64) {
    if delta != 0 {
        *r = add_lines(r.start, delta)..add_lines(r.end, delta);
    }
}

/// Shifts both ends of a byte range.
pub(crate) fn shift_bytes(r: &mut Range<usize>, delta: isize) {
    if delta != 0 {
        *r = offset(r.start, delta)..offset(r.end, delta);
    }
}

/// Length of the longest common byte prefix.
pub(crate) fn common_prefix_bytes(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 128;
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + CHUNK <= n && a[i..i + CHUNK] == b[i..i + CHUNK] {
        i += CHUNK;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Length of the longest common byte suffix.
pub(crate) fn common_suffix_bytes(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 128;
    let n = a.len().min(b.len());
    let (la, lb) = (a.len(), b.len());
    let mut i = 0;
    while i + CHUNK <= n && a[la - i - CHUNK..la - i] == b[lb - i - CHUNK..lb - i] {
        i += CHUNK;
    }
    while i < n && a[la - i - 1] == b[lb - i - 1] {
        i += 1;
    }
    i
}

/// Longest common prefix of `a` and `b` that ends on a char boundary.
///
/// The prefix bytes are identical, so a boundary in one string is a boundary in the other.
pub(crate) fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = common_prefix_bytes(a.as_bytes(), b.as_bytes());
    while !a.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Longest common suffix of `a` and `b` that starts on a char boundary.
pub(crate) fn common_suffix(a: &str, b: &str) -> usize {
    let mut n = common_suffix_bytes(a.as_bytes(), b.as_bytes());
    while !a.is_char_boundary(a.len() - n) {
        n -= 1;
    }
    n
}

/// The smallest single edit turning `old` (found at byte `at`) into `new`: the common prefix and
/// suffix are left alone. `None` if the texts are equal.
pub(crate) fn trimmed_edit(at: usize, old: &str, new: &str) -> Option<TextEdit> {
    if old == new {
        return None;
    }
    let p = common_prefix(old, new);
    let q = common_suffix(&old[p..], &new[p..]);
    Some(TextEdit {
        range: at + p..at + old.len() - q,
        text: new[p..new.len() - q].to_owned(),
    })
}

/// Whether `[x.start, x.end)` and `[y.start, y.end)` share text; an empty range counts as a point
/// that overlaps a range when it lies within or on the boundary of it.
pub(crate) fn overlaps(x: &Range<usize>, y: &Range<usize>) -> bool {
    if x.is_empty() || y.is_empty() {
        x.start <= y.end && y.start <= x.end
    } else {
        x.start < y.end && y.start < x.end
    }
}

/// Moves `range` through the edit `[a, b) → len bytes` that does not cut into it: edits ending at
/// or before `range.start` shift it, edits starting at or after `range.end` leave it alone. Returns
/// `false` (leaving `range` untouched) when the edit removes or inserts text strictly inside it.
///
/// Text inserted at the start of a non-empty range goes before it. An empty range marks where
/// something was deleted; text inserted at that point goes after it, since the deleted text came
/// before whatever now follows the point.
pub(crate) fn shift_range(range: &mut Range<usize>, a: usize, b: usize, len: usize) -> bool {
    if range.start == range.end && a == b && a == range.start {
        true
    } else if b <= range.start {
        shift_bytes(range, signed(len) - signed(b - a));
        true
    } else {
        a >= range.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_span_rules() {
        assert_eq!(line_span(3, ""), 3..3);
        assert_eq!(line_span(3, "x"), 3..4);
        assert_eq!(line_span(3, "x\n"), 3..4);
        assert_eq!(line_span(3, "x\ny"), 3..5);
        assert_eq!(line_span(3, "x\ny\n"), 3..5);
        assert_eq!(line_span(0, "\n"), 0..1);
    }

    #[test]
    fn prefix_and_suffix_respect_char_boundaries() {
        // 'é' = C3 A9, 'è' = C3 A8: one common byte, but no common char.
        assert_eq!(common_prefix("é", "è"), 0);
        assert_eq!(common_prefix("aé", "aè"), 1);
        // '∑' = E2 88 91, '∏' = E2 88 8F: the suffix byte differs, nothing common.
        assert_eq!(common_suffix("x∑", "y∏"), 0);
        // 'Ä' = C3 84, 'Ą' = C4 84: common last byte only.
        assert_eq!(common_suffix("Ä", "Ą"), 0);
        assert_eq!(common_suffix("aÄb", "cÄb"), 3);
        let long_a = "x".repeat(300) + "a" + &"y".repeat(300);
        let long_b = "x".repeat(300) + "b" + &"y".repeat(300);
        assert_eq!(common_prefix(&long_a, &long_b), 300);
        assert_eq!(common_suffix(&long_a, &long_b), 300);
    }

    #[test]
    fn trimmed_edit_is_minimal() {
        assert_eq!(trimmed_edit(10, "abc", "abc"), None);
        let e = trimmed_edit(10, "\\alpha + x", "\\beta + x").unwrap();
        assert_eq!(e.range, 11..15);
        assert_eq!(e.text, "bet");
        let e = trimmed_edit(0, "aa", "aaa").unwrap();
        assert_eq!((e.range, e.text.as_str()), (2..2, "a"));
        let e = trimmed_edit(0, "Ä", "Ą").unwrap();
        assert_eq!((e.range, e.text.as_str()), (0..2, "Ą"));
    }

    #[test]
    fn shift_range_rules() {
        let mut r = 10..20;
        assert!(shift_range(&mut r, 2, 4, 5)); // before: +3
        assert_eq!(r, 13..23);
        assert!(shift_range(&mut r, 13, 13, 1)); // insertion at the start shifts
        assert_eq!(r, 14..24);
        assert!(shift_range(&mut r, 24, 24, 1)); // insertion at the end does not
        assert_eq!(r, 14..24);
        assert!(!shift_range(&mut r, 15, 15, 1)); // insertion inside cuts
        assert!(!shift_range(&mut r, 10, 15, 0)); // deletion overlapping the start cuts
        let mut p = 5..5;
        assert!(shift_range(&mut p, 5, 5, 2)); // insertion at the point goes after it
        assert_eq!(p, 5..5);
        assert!(shift_range(&mut p, 4, 4, 2)); // insertion before it shifts
        assert_eq!(p, 7..7);
        assert!(shift_range(&mut p, 7, 9, 0)); // deletion starting at the point
        assert_eq!(p, 7..7);
        assert!(shift_range(&mut p, 6, 7, 0)); // deletion ending at the point
        assert_eq!(p, 6..6);
        assert!(!shift_range(&mut p, 5, 8, 0)); // deletion around the point
    }

    #[test]
    fn overlap_rules() {
        assert!(overlaps(&(0..5), &(4..9)));
        assert!(!overlaps(&(0..5), &(5..9)));
        assert!(overlaps(&(5..5), &(0..5)));
        assert!(overlaps(&(5..5), &(5..5)));
        assert!(!overlaps(&(6..6), &(0..5)));
    }
}
