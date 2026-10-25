//! The unified diff of a review's hunks, as `get_review_state` gives it to the agent: the
//! format and the `@@` grouping of [`crate::unified_diff`], made from the hunks the review holds
//! (no diff is computed again), and cut after a limit with a count of what it leaves out, so
//! that a review of thousands of changes stays a reply the agent can take.

use crate::Hunk;
use crate::diff::{LineDiff, LineHunk};
use crate::edits::{CONTEXT, write_group};
use std::ops::RangeInclusive;

/// How much of one file's diff [`review_diff`] writes: at most `changes` changes and `bytes`
/// bytes (a change is written whole or not at all).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLimit {
    pub changes: usize,
    pub bytes: usize,
}

/// The changes [`review_diff`] left out of a file's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOut {
    /// How many there are.
    pub changes: usize,
    /// The 1-based lines of the current text they span (a deletion counts as the line after
    /// it).
    pub lines: RangeInclusive<u32>,
}

/// The unified diff of `hunks` (a review's hunks of `baseline` against `current`, as
/// `FileReview::hunks` gives them after a recompute), as [`crate::unified_diff`] writes it:
/// equal to `unified_diff(path, baseline, current)` when nothing is left out. Writes `@@` hunks
/// within `limit` (the last one cut short at a change when it would pass it); the changes after
/// them are only counted. A change longer than what is left of the limit (a file the agent wrote
/// whole) is counted too, and when no change is written the diff is empty. Hunks that do not fit
/// the texts (a review not recomputed since the last edit) are computed again from the texts.
pub fn review_diff(
    path: &str,
    baseline: &str,
    current: &str,
    hunks: &[Hunk],
    limit: DiffLimit,
) -> (String, Option<LeftOut>) {
    if hunks.is_empty() || baseline == current {
        return (String::new(), None);
    }
    let old_lines: Vec<&str> = baseline.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = current.split_inclusive('\n').collect();
    let fits = hunks.iter().all(|h| {
        h.base_lines.end as usize <= old_lines.len() && h.cur_lines.end as usize <= new_lines.len()
    });
    let line_hunks: Vec<LineHunk> = if fits {
        hunks
            .iter()
            .map(|h| {
                LineHunk::of_ranges(
                    h.base_lines.clone(),
                    h.cur_lines.clone(),
                    h.base_bytes.clone(),
                    h.cur_bytes.clone(),
                )
            })
            .collect()
    } else {
        LineDiff::new(baseline, current).hunks(false)
    };
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut written = 0;
    // A hunk was cut short: the next one's context would overlap it.
    let mut cut = false;
    let mut rest = &line_hunks[..];
    // The changes from `rest` on are left out: the diff so far (none without a change), and
    // their count.
    let left_out = |out: String, rest: &[LineHunk], written: usize| {
        let first = rest[0].new_lines.start + 1;
        let last = rest
            .iter()
            .map(|h| h.new_lines.end.max(h.new_lines.start + 1))
            .max()
            .unwrap_or(first);
        let out = if written == 0 { String::new() } else { out };
        let left_out = LeftOut {
            changes: rest.len(),
            lines: first..=last,
        };
        (out, Some(left_out))
    };
    while !rest.is_empty() {
        if cut || written >= limit.changes || out.len() >= limit.bytes {
            return left_out(out, rest, written);
        }
        // Changes closer than twice the context share one hunk, as in `unified_diff`. A hunk
        // that would pass the limit is cut short at a change (halving until it fits), its
        // context stopping before the next change, and is the last; a single change that does
        // not fit is left out.
        let whole = 1 + rest
            .windows(2)
            .take_while(|w| w[1].old_lines.start - w[0].old_lines.end <= 2 * CONTEXT)
            .count();
        let mut n = whole.min(limit.changes - written);
        let mut text = String::new();
        loop {
            text.clear();
            let (group, tail) = rest.split_at(n);
            let end = tail.first().map_or(old_lines.len(), |next| {
                (next.old_lines.start as usize).min(old_lines.len())
            });
            write_group(&mut text, group, &old_lines[..end], &new_lines);
            if out.len() + text.len() <= limit.bytes {
                break;
            }
            if n == 1 {
                return left_out(out, rest, written);
            }
            n /= 2;
        }
        out.push_str(&text);
        written += n;
        cut = n < whole;
        rest = &rest[n..];
    }
    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileReview, unified_diff};
    use lemma_core::{Origin, TextEdit};

    const ALL: DiffLimit = DiffLimit {
        changes: usize::MAX,
        bytes: usize::MAX,
    };

    /// A review of `baseline` after the agent's `edits` (in order, each on the text before
    /// it), recomputed.
    fn reviewed(baseline: &str, edits: &[(&str, &str)]) -> (FileReview, String) {
        let mut review = FileReview::new(baseline.to_owned());
        let mut text = baseline.to_owned();
        for (old, new) in edits {
            let at = text.find(old).expect("the edit's text");
            let edit = TextEdit {
                range: at..at + old.len(),
                text: (*new).to_owned(),
            };
            review.on_edit(&edit, Origin::Agent { call: 1 });
            text.replace_range(edit.range.clone(), &edit.text);
        }
        review.recompute(&text);
        (review, text)
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn equals_the_unified_diff() {
        let base = lines(60);
        let cases: Vec<Vec<(&str, &str)>> = vec![
            // Replacements far apart and close together (one `@@` hunk), an insertion, a
            // deletion at the end and one at the start.
            vec![("line 3\n", "line three\n"), ("line 40\n", "line forty\n")],
            vec![
                ("line 10\n", "x\n"),
                ("line 14\n", "y\n"),
                ("line 20\n", "z\n"),
            ],
            vec![("line 30\n", "line 30\nnew\nnewer\n")],
            vec![("line 59\nline 60\n", "")],
            vec![("line 1\n", ""), ("line 33\n", "")],
        ];
        for edits in cases {
            let (review, text) = reviewed(&base, &edits);
            let (diff, left_out) = review_diff("a.tex", &base, &text, review.hunks(), ALL);
            assert_eq!(diff, unified_diff("a.tex", &base, &text), "{edits:?}");
            assert_eq!(left_out, None);
        }
        // A last line without a newline.
        let base = "a\nb\nc";
        let (review, text) = reviewed(base, &[("c", "C")]);
        let (diff, _) = review_diff("b.tex", base, &text, review.hunks(), ALL);
        assert_eq!(diff, unified_diff("b.tex", base, &text));
        assert!(
            diff.ends_with("+C\n\\ No newline at end of file\n"),
            "{diff}"
        );
    }

    #[test]
    fn cut_after_the_limit_with_a_count() {
        let base = lines(100);
        let edits: Vec<(String, String)> = (0..10)
            .map(|i| {
                (
                    format!("line {}\n", 10 * i + 5),
                    format!("line {}!\n", 10 * i + 5),
                )
            })
            .collect();
        let edits: Vec<(&str, &str)> = edits
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (review, text) = reviewed(&base, &edits);
        assert_eq!(review.hunks().len(), 10);
        let full = unified_diff("c.tex", &base, &text);
        // Four changes: the first four `@@` hunks, then the other six with their lines.
        let limit = DiffLimit {
            changes: 4,
            bytes: usize::MAX,
        };
        let (diff, left_out) = review_diff("c.tex", &base, &text, review.hunks(), limit);
        assert!(full.starts_with(&diff));
        assert_eq!(diff.matches("@@ -").count(), 4);
        assert_eq!(
            left_out,
            Some(LeftOut {
                changes: 6,
                lines: 45..=95
            })
        );
        // A byte limit: hunks whole, while they fit.
        let limit = DiffLimit {
            changes: usize::MAX,
            bytes: 300,
        };
        let (diff, left_out) = review_diff("c.tex", &base, &text, review.hunks(), limit);
        assert!(full.starts_with(&diff) && diff.len() <= 300, "{diff}");
        assert_eq!(diff.matches("@@ -").count(), 3, "{diff}");
        assert_eq!(left_out.map(|l| l.changes), Some(7));
        // Changes close together share an `@@` hunk; cut short, it ends before the change left
        // out, whose line is not context.
        let base = lines(20);
        let close = [
            ("line 2\n", "line 2!\n"),
            ("line 5\n", "line 5!\n"),
            ("line 8\n", "line 8!\n"),
            ("line 11\n", "line 11!\n"),
        ];
        let (cut, cut_text) = reviewed(&base, &close);
        let limit = DiffLimit {
            changes: 3,
            bytes: usize::MAX,
        };
        let (diff, left_out) = review_diff("d.tex", &base, &cut_text, cut.hunks(), limit);
        assert_eq!(
            diff,
            "--- a/d.tex\n+++ b/d.tex\n@@ -1,10 +1,10 @@\n line 1\n-line 2\n+line 2!\n line 3\n \
             line 4\n-line 5\n+line 5!\n line 6\n line 7\n-line 8\n+line 8!\n line 9\n line 10\n"
        );
        assert_eq!(
            left_out,
            Some(LeftOut {
                changes: 1,
                lines: 11..=11
            })
        );
        // A byte limit halfway through such a hunk cuts it at a change, and it is the last.
        let limit = DiffLimit {
            changes: usize::MAX,
            bytes: 90,
        };
        let (diff, left_out) = review_diff("d.tex", &base, &cut_text, cut.hunks(), limit);
        assert!(
            diff.contains("+line 2!\n") && !diff.contains("line 5!"),
            "{diff}"
        );
        assert_eq!(diff.matches("@@ -").count(), 1, "{diff}");
        assert_eq!(left_out.map(|l| l.changes), Some(3));
        // A change longer than the limit by itself is only counted, and so are the changes
        // after it; a file the agent wrote whole is one such change, and its diff is empty.
        let base = lines(20);
        let long: String = (0..40).map(|i| format!("a new line {i}\n")).collect();
        let grown = format!("line 15\n{long}");
        let (review, text) = reviewed(
            &base,
            &[("line 3\n", "line 3!\n"), ("line 15\n", grown.as_str())],
        );
        let limit = DiffLimit {
            changes: usize::MAX,
            bytes: 200,
        };
        let (diff, left_out) = review_diff("e.tex", &base, &text, review.hunks(), limit);
        assert!(
            diff.contains("+line 3!\n") && !diff.contains("a new line"),
            "{diff}"
        );
        assert_eq!(
            left_out,
            Some(LeftOut {
                changes: 1,
                lines: 16..=55
            })
        );
        let mut created = FileReview::new(String::new());
        created.on_edit(
            &TextEdit::insert(0, long.as_str()),
            Origin::Agent { call: 1 },
        );
        created.recompute(&long);
        let (diff, left_out) = review_diff("f.tex", "", &long, created.hunks(), limit);
        assert_eq!(diff, "");
        assert_eq!(
            left_out,
            Some(LeftOut {
                changes: 1,
                lines: 1..=40
            })
        );
        // Hunks that no longer fit the text (it lost lines since): computed again.
        let base = lines(100);
        let shorter = lines(20);
        let (diff, _) = review_diff("c.tex", &base, &shorter, review.hunks(), ALL);
        assert_eq!(diff, unified_diff("c.tex", &base, &shorter));
    }
}
