use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Replace `range` (byte offsets in the pre-edit text) with `text`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub text: String,
}

impl TextEdit {
    /// An insertion of `text` at byte offset `at`.
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: at..at,
            text: text.into(),
        }
    }

    /// A deletion of `range`.
    pub fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }

    /// Applies the edit to `s`.
    ///
    /// # Panics
    /// If the range is out of bounds, inverted, or does not lie on `char` boundaries.
    pub fn apply(&self, s: &mut String) {
        s.replace_range(self.range.clone(), &self.text);
    }

    /// Net change in length (inserted - removed), in bytes.
    pub fn delta(&self) -> isize {
        // Allocations never exceed isize::MAX bytes, so both casts are lossless.
        self.text.len() as isize - self.range.len() as isize
    }
}

/// Apply non-overlapping edits given in DESCENDING `range.start` order.
///
/// Every range refers to the text before any of the edits, which is exactly what applying them
/// back to front preserves.
///
/// # Panics
/// Under the same conditions as [`TextEdit::apply`]. Debug builds also assert the ordering.
pub fn apply_all(s: &mut String, edits: &[TextEdit]) {
    debug_assert!(
        edits.windows(2).all(|w| w[1].range.end <= w[0].range.start),
        "edits must be non-overlapping and in descending order: {edits:?}"
    );
    for edit in edits {
        edit.apply(s);
    }
}

/// Who caused a buffer change. Set by the code that performs the change; read by buffer signal handlers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// Typing and every other change not attributed below (the default).
    #[default]
    User,
    /// TextBuffer undo/redo (detected via the `undo`/`redo` signals, before + after handlers).
    History,
    /// An agent tool call; `call` is the MCP call id.
    Agent { call: u64 },
    /// Our own Accept/Reject buffer edit.
    Review,
    /// Buffer reloaded from disk.
    Reload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_delete_constructors() {
        assert_eq!(
            TextEdit::insert(3, "xy"),
            TextEdit {
                range: 3..3,
                text: "xy".into()
            }
        );
        assert_eq!(
            TextEdit::delete(1..4),
            TextEdit {
                range: 1..4,
                text: String::new()
            }
        );
    }

    #[test]
    fn apply_insert_delete_replace() {
        let mut s = String::from("hello world");
        TextEdit::insert(5, ",").apply(&mut s);
        assert_eq!(s, "hello, world");
        TextEdit::delete(0..7).apply(&mut s);
        assert_eq!(s, "world");
        TextEdit {
            range: 0..1,
            text: "W".into(),
        }
        .apply(&mut s);
        assert_eq!(s, "World");
    }

    #[test]
    fn apply_respects_multibyte_boundaries() {
        let mut s = String::from("aé∑b");
        // 'é' is 2 bytes (1..3), '∑' is 3 bytes (3..6).
        TextEdit {
            range: 1..6,
            text: "–".into(),
        }
        .apply(&mut s);
        assert_eq!(s, "a–b");
    }

    #[test]
    #[should_panic]
    fn apply_inside_a_char_panics() {
        let mut s = String::from("é");
        TextEdit::insert(1, "x").apply(&mut s);
    }

    #[test]
    fn delta_counts_bytes() {
        assert_eq!(TextEdit::insert(0, "abc").delta(), 3);
        assert_eq!(TextEdit::delete(2..7).delta(), -5);
        let e = TextEdit {
            range: 0..2,
            text: "∑".into(),
        };
        assert_eq!(e.delta(), 1);
        assert_eq!(TextEdit::insert(4, "").delta(), 0);
    }

    #[test]
    fn apply_all_uses_pre_edit_offsets_in_descending_order() {
        let mut s = String::from("0123456789");
        let edits = [
            TextEdit::insert(10, "!"),
            TextEdit {
                range: 6..8,
                text: "six-seven".into(),
            },
            TextEdit::delete(2..4),
            TextEdit::insert(0, ">"),
        ];
        apply_all(&mut s, &edits);
        assert_eq!(s, ">0145six-seven89!");
    }

    #[test]
    fn apply_all_adjacent_edits() {
        let mut s = String::from("abc");
        let edits = [TextEdit::insert(2, "X"), TextEdit::delete(1..2)];
        apply_all(&mut s, &edits);
        assert_eq!(s, "aXc");
    }

    #[test]
    #[should_panic(expected = "descending")]
    #[cfg(debug_assertions)]
    fn apply_all_rejects_ascending_order() {
        let mut s = String::from("abcdef");
        apply_all(&mut s, &[TextEdit::delete(0..1), TextEdit::delete(3..4)]);
    }

    #[test]
    fn origin_defaults_to_user() {
        assert_eq!(Origin::default(), Origin::User);
    }
}
