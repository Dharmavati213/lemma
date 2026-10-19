//! Edit helpers for the agent tools: Claude-Edit `str_replace`, `write` as `minimal_edits`, and the
//! plain unified diff shown to screen readers and returned by `get_review_state`.

use crate::diff::{LineDiff, LineHunk};
use crate::text::{common_prefix, common_suffix, to_u32, trimmed_edit};
use lemma_core::TextEdit;
use std::fmt::{self, Write as _};

/// Why [`str_replace`] found nothing to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// `old` does not occur in the text.
    NotFound,
    /// `old` occurs this many times and `all` was not set.
    NotUnique(usize),
    /// `old` is empty.
    Empty,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("old_string not found"),
            Self::NotUnique(n) => write!(f, "old_string matches {n} times"),
            Self::Empty => f.write_str("old_string is empty"),
        }
    }
}

impl std::error::Error for EditError {}

/// Claude-Edit semantics: `old` must occur exactly once unless `all`. Returned edits are minimal
/// (common prefix/suffix of old/new trimmed) and in descending order.
///
/// Occurrences are counted without overlap, left to right. Trimming respects UTF-8 boundaries.
/// Replacing `old` by an identical `new` yields no edits.
pub fn str_replace(
    current: &str,
    old: &str,
    new: &str,
    all: bool,
) -> Result<Vec<TextEdit>, EditError> {
    if old.is_empty() {
        return Err(EditError::Empty);
    }
    let starts: Vec<usize> = current.match_indices(old).map(|(at, _)| at).collect();
    match starts.len() {
        0 => return Err(EditError::NotFound),
        n if n > 1 && !all => return Err(EditError::NotUnique(n)),
        _ => {}
    }
    if old == new {
        return Ok(Vec::new());
    }
    let p = common_prefix(old, new);
    let q = common_suffix(&old[p..], &new[p..]);
    let text = &new[p..new.len() - q];
    Ok(starts
        .iter()
        .rev()
        .map(|&at| TextEdit {
            range: at + p..at + old.len() - q,
            text: text.to_owned(),
        })
        .collect())
}

/// Minimal line-based edits turning `current` into `new` (for the `write` tool on an existing
/// file).
///
/// One edit per changed run of lines, trimmed further to the changed bytes, in descending order
/// (ready for `lemma_core::apply_all`).
pub fn minimal_edits(current: &str, new: &str) -> Vec<TextEdit> {
    if current == new {
        return Vec::new();
    }
    LineDiff::new(current, new)
        .hunks(false)
        .iter()
        .rev()
        .filter_map(|h| {
            trimmed_edit(
                h.old_bytes.start,
                &current[h.old_bytes.clone()],
                &new[h.new_bytes.clone()],
            )
        })
        .collect()
}

/// Lines of context around each change in [`unified_diff`].
const CONTEXT: u32 = 3;

/// Unified diff (3 lines context) for the Changes panel's plain-text view and for get_review_state.
///
/// GNU/git format with `a/<path>` and `b/<path>` headers; a line without a final newline is
/// followed by `\ No newline at end of file`. Equal texts give an empty string.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let hunks = LineDiff::new(old, new).hunks(false);
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut rest = &hunks[..];
    while !rest.is_empty() {
        // Changes closer than twice the context share one hunk.
        let n = 1 + rest
            .windows(2)
            .take_while(|w| w[1].old_lines.start - w[0].old_lines.end <= 2 * CONTEXT)
            .count();
        let (group, tail) = rest.split_at(n);
        write_group(&mut out, group, &old_lines, &new_lines);
        rest = tail;
    }
    out
}

/// Writes one `@@` hunk covering `group` plus context.
fn write_group(out: &mut String, group: &[LineHunk], old_lines: &[&str], new_lines: &[&str]) {
    let (first, last) = (&group[0], &group[group.len() - 1]);
    let old_lo = first.old_lines.start.saturating_sub(CONTEXT);
    let new_lo = first.new_lines.start - (first.old_lines.start - old_lo);
    let old_hi = (last.old_lines.end + CONTEXT).min(to_u32(old_lines.len()));
    let new_hi = last.new_lines.end + (old_hi - last.old_lines.end);
    let _ = writeln!(
        out,
        "@@ -{} +{} @@",
        header_range(old_lo, old_hi - old_lo),
        header_range(new_lo, new_hi - new_lo)
    );
    let mut line = old_lo;
    for h in group {
        for l in line..h.old_lines.start {
            push_line(out, ' ', old_lines[l as usize]);
        }
        for l in h.old_lines.clone() {
            push_line(out, '-', old_lines[l as usize]);
        }
        for l in h.new_lines.clone() {
            push_line(out, '+', new_lines[l as usize]);
        }
        line = h.old_lines.end;
    }
    for l in line..old_hi {
        push_line(out, ' ', old_lines[l as usize]);
    }
}

/// `start,count` of a hunk header (1-based start; an empty range names the line before it).
fn header_range(start: u32, count: u32) -> String {
    match count {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{count}", start + 1),
    }
}

fn push_line(out: &mut String, marker: char, line: &str) {
    out.push(marker);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}
