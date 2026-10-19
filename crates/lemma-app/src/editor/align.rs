//! Align helper (M5 WP5.3, from M4): Align Columns, Insert Row Below and Add Column.
//!
//! - **Align Columns** (`win.align-columns`, Ctrl+Shift+A, palette): in the innermost
//!   environment of the `&` family around the cursor ([`ALIGN_FAMILY`]: align, alignat,
//!   aligned, flalign, eqnarray, split, cases, tabular and friends, array, the matrices), pads
//!   the cells of every row line so that the `&` line up, and the `\\` of the full rows too.
//!   A row line is a body line (between the `\begin` and `\end` lines) with an `&` outside
//!   braces, nested environments and comments; `\&`, `\\`, `\%` and `\{` are escapes. Lines
//!   without `&` (`\hline`, a row continued from the line before), lines inside a nested
//!   environment and lines with two rows are left alone. Each cell keeps whether its `&` was
//!   followed by a space (`a &= b` stays `&=`); rows take the first row's indentation.
//! - **Insert Row Below** (`win.insert-row-below`): after the row at the cursor (the line that
//!   ends it with `\\`), an empty row with as many cells; the cursor goes to its first cell. A
//!   last row without `\\` gets one.
//! - **Add Column** (`win.add-column`): in tabular and matrix environments ([`TABLE_FAMILY`]),
//!   an empty last cell on every row line, and one more column in the column specification of
//!   tabular-like environments (`{lc}` → `{lcc}`, `{|l|c|}` → `{|l|c|c|}`).
//!
//! Each command is one undo step. The text functions ([`align_lines`], [`insert_row_edits`],
//! [`add_column_edits`]) are pure, with table tests.

use super::Editor;
use super::envtools::{self, arguments_len, indent_of, line_bounds};
use super::pairs::{self, EnvSpan};
use crate::app::{self, ShortcutGroup};
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use std::ops::Range;
use std::rc::Rc;

/// Environments whose rows are aligned with `&` (starred forms included).
pub const ALIGN_FAMILY: &[&str] = &[
    "align",
    "alignat",
    "aligned",
    "alignedat",
    "flalign",
    "xalignat",
    "xxalignat",
    "eqnarray",
    "split",
    "cases",
    "dcases",
    "rcases",
    "drcases",
    "tabular",
    "tabularx",
    "tabulary",
    "longtable",
    "tblr",
    "array",
    "matrix",
    "pmatrix",
    "bmatrix",
    "Bmatrix",
    "vmatrix",
    "Vmatrix",
    "smallmatrix",
];

/// Environments that Add Column works in.
pub const TABLE_FAMILY: &[&str] = &[
    "tabular",
    "tabularx",
    "tabulary",
    "longtable",
    "tblr",
    "array",
    "matrix",
    "pmatrix",
    "bmatrix",
    "Bmatrix",
    "vmatrix",
    "Vmatrix",
    "smallmatrix",
];

/// The actions (without `win.`), their labels and default accelerators.
const ACTIONS: &[(&str, &str, &[&str])] = &[
    ("align-columns", "Align Columns", &["<Control><Shift>a"]),
    ("insert-row-below", "Insert Row Below", &[]),
    ("add-column", "Add Column", &[]),
];

fn in_family(family: &[&str], name: &str) -> bool {
    family.contains(&name.trim_end_matches('*'))
}

// ───────────── Rows ─────────────

/// The parts of one body line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line<'a> {
    indent: &'a str,
    /// The cells' raw text (the first without the indentation).
    cells: Vec<&'a str>,
    /// From the row's `\\` to the comment, trimmed.
    tail: Option<&'a str>,
    /// From `%`.
    comment: Option<&'a str>,
    /// Inside a nested environment, or two rows on the line: leave it alone.
    skip: bool,
}

/// Splits `line` at its top-level `&`; `depth` is the nesting of environments opened on the
/// lines before (updated).
fn split_line<'a>(line: &'a str, depth: &mut i32) -> Line<'a> {
    let indent = indent_of(line);
    let b = line.as_bytes();
    let starts_nested = *depth > 0;
    let mut braces = 0i32;
    let mut amps = Vec::new();
    let mut row_end = None;
    let mut comment = None;
    let mut two_rows = false;
    let mut i = indent.len();
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let rest = &line[i + 1..];
                let word = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                if rest.starts_with('\\') {
                    if braces == 0 && *depth == 0 && row_end.is_none() {
                        row_end = Some(i);
                    }
                    i += 2;
                    continue;
                }
                if word > 0 {
                    match &rest[..word] {
                        "begin" => *depth += 1,
                        "end" => *depth -= 1,
                        _ => {}
                    }
                    i += 1 + word;
                    continue;
                }
                i += 2;
                continue;
            }
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'%' => {
                comment = Some(i);
                break;
            }
            b'&' if braces == 0 && *depth == 0 => {
                if row_end.is_some() {
                    two_rows = true;
                } else {
                    amps.push(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    let content_end = comment.unwrap_or(line.len());
    let cells_end = row_end.unwrap_or(content_end);
    let mut cells = Vec::with_capacity(amps.len() + 1);
    let mut from = indent.len();
    for &a in &amps {
        cells.push(&line[from..a]);
        from = a + 1;
    }
    cells.push(&line[from..cells_end.max(from)]);
    Line {
        indent,
        cells,
        tail: row_end.map(|r| line[r..content_end].trim()),
        comment: comment.map(|c| &line[c..]),
        skip: starts_nested || two_rows || *depth < 0,
    }
}

/// The aligned form of the body lines `lines` (`None` for each line left as it is).
pub fn align_lines(lines: &[&str]) -> Vec<Option<String>> {
    let mut depth = 0;
    let parsed: Vec<Line> = lines.iter().map(|l| split_line(l, &mut depth)).collect();
    let rows: Vec<usize> = (0..parsed.len())
        .filter(|&i| !parsed[i].skip && parsed[i].cells.len() > 1)
        .collect();
    let mut out = vec![None; lines.len()];
    let Some(&first) = rows.first() else {
        return out;
    };
    // Each cell trimmed, after one space when an `&` of its column was followed by one.
    let columns = rows
        .iter()
        .map(|&r| parsed[r].cells.len())
        .max()
        .unwrap_or(0);
    let mut lead = vec![false; columns];
    for &r in &rows {
        for (i, c) in parsed[r].cells.iter().enumerate().skip(1) {
            lead[i] |= c.starts_with([' ', '\t']);
        }
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|&r| {
            parsed[r]
                .cells
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{}{}", if lead[i] { " " } else { "" }, c.trim()))
                .collect()
        })
        .collect();
    let mut widths = vec![0usize; columns];
    for (k, row) in cells.iter().enumerate() {
        let full = row.len() == columns && parsed[rows[k]].tail.is_some();
        for (i, c) in row.iter().enumerate() {
            if i + 1 < row.len() || full {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let indent = parsed[first].indent;
    for (k, row) in cells.iter().enumerate() {
        let line = &parsed[rows[k]];
        let full = row.len() == columns && line.tail.is_some();
        let mut s = String::from(indent);
        for (i, c) in row.iter().enumerate() {
            let last = i + 1 == row.len();
            let width = if !last || full { widths[i] } else { 0 };
            let pad = width.saturating_sub(c.chars().count());
            s.push_str(c);
            s.extend(std::iter::repeat_n(' ', pad));
            if !last {
                // `&&` stays `&&` where a column is empty in every row.
                s.push_str(if c.is_empty() && pad == 0 { "&" } else { " &" });
            }
        }
        if let Some(tail) = line.tail {
            s.push(' ');
            s.push_str(tail);
        }
        if let Some(comment) = line.comment {
            s.truncate(s.trim_end().len());
            s.push(' ');
            s.push_str(comment);
        }
        s.truncate(s.trim_end().len());
        if s != lines[rows[k]] {
            out[rows[k]] = Some(s);
        }
    }
    out
}

/// The body lines of `span` in `text`: the lines strictly between its `\begin` and `\end`
/// lines, as (start offset, text without the line ending, `\r\n` or `\n`, which edits keep).
fn body_lines<'t>(text: &'t str, span: &EnvSpan) -> Vec<(usize, &'t str)> {
    let begin = line_bounds(text, span.begin.start);
    let end = line_bounds(text, span.end.start);
    if end.start <= begin.end {
        return Vec::new();
    }
    let mut at = begin.end + 1;
    text[begin.end + 1..end.start]
        .split_inclusive('\n')
        .map(|l| {
            let start = at;
            at += l.len();
            let l = l.strip_suffix('\n').unwrap_or(l);
            (start, l.strip_suffix('\r').unwrap_or(l))
        })
        .collect()
}

/// The edits of Align Columns in `span` (descending).
pub fn align_edits(text: &str, span: &EnvSpan) -> Vec<TextEdit> {
    let body = body_lines(text, span);
    let lines: Vec<&str> = body.iter().map(|(_, l)| *l).collect();
    let aligned = align_lines(&lines);
    body.iter()
        .zip(aligned)
        .rev()
        .filter_map(|((start, line), new)| {
            Some(TextEdit {
                range: *start..start + line.len(),
                text: new?,
            })
        })
        .collect()
}

/// The edits of Insert Row Below with the cursor at byte `cursor` (descending), and where the
/// cursor goes afterwards. `None` when the cursor is not on a body line.
pub fn insert_row_edits(
    text: &str,
    span: &EnvSpan,
    cursor: usize,
) -> Option<(Vec<TextEdit>, usize)> {
    let body = body_lines(text, span);
    let at = body
        .iter()
        .position(|(start, l)| (*start..=start + l.len()).contains(&cursor))?;
    let mut depth = 0;
    let parsed: Vec<Line> = body
        .iter()
        .map(|(_, l)| split_line(l, &mut depth))
        .collect();
    // The line that ends the cursor's row (else the last body line).
    let end = (at..body.len())
        .find(|&i| parsed[i].tail.is_some() && !parsed[i].skip)
        .unwrap_or(body.len() - 1);
    let columns = if parsed[at].cells.len() > 1 {
        parsed[at].cells.len()
    } else {
        parsed.iter().map(|l| l.cells.len()).max().unwrap_or(1)
    };
    let (start, line) = body[end];
    let indent = indent_of(body[at].1);
    let mut edits = Vec::new();
    let row = format!("{indent}{} \\\\", " &".repeat(columns - 1));
    let line_end = start + line.len();
    // The new line ends like the one it follows (CRLF files stay CRLF).
    let eol = if text[line_end..].starts_with('\r') {
        "\r\n"
    } else {
        "\n"
    };
    let cursor_at = if parsed[end].tail.is_some() {
        edits.push(TextEdit::insert(line_end, format!("{eol}{row}")));
        line_end + eol.len() + indent.len()
    } else {
        // The last row: it gets `\\` before its comment, the new row has none.
        let content_end = parsed[end].comment.map_or(line_end, |c| line_end - c.len());
        let content = &text[start..content_end];
        let trimmed_end = start + content.trim_end().len();
        let new_row = format!("{indent}{}", " &".repeat(columns - 1));
        let terminator = if content_end < line_end {
            " \\\\ "
        } else {
            " \\\\"
        };
        edits.push(TextEdit::insert(line_end, format!("{eol}{new_row}")));
        edits.push(TextEdit {
            range: trimmed_end..content_end,
            text: terminator.to_owned(),
        });
        // The line grew by the terminator and lost its trailing blanks.
        line_end + terminator.len() + eol.len() + indent.len() - (content_end - trimmed_end)
    };
    Some((edits, cursor_at))
}

/// The column specification of a tabular-like environment: the range of its text inside the
/// braces (`{lcr}`), after `\begin{name}` at `begin_end`.
fn spec_range(text: &str, name: &str, begin_end: usize) -> Option<Range<usize>> {
    let base = name.trim_end_matches('*');
    let wanted = match base {
        "tabular" | "longtable" | "array" | "tblr" if !name.ends_with('*') => 1,
        "tabular" | "tabularx" | "tabulary" => 2,
        _ => return None,
    };
    let line = line_bounds(text, begin_end);
    let rest = &text[begin_end..line.end];
    let mut at = 0;
    let mut seen = 0;
    loop {
        let skip = rest[at..].len() - rest[at..].trim_start().len();
        at += skip;
        let len = arguments_len(&rest[at..], false);
        let first = arguments_len(&rest[at..], true);
        if first > 0 {
            at += first;
            continue;
        }
        if len == 0 {
            return None;
        }
        // One `{…}` group.
        let group = brace_group_len(&rest[at..])?;
        seen += 1;
        if seen == wanted {
            return Some(begin_end + at + 1..begin_end + at + group - 1);
        }
        at += group;
    }
}

/// The length of the `{…}` group at the start of `s`.
fn brace_group_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The column specification with one more column: the last plain column type (`l`, `c`, `r`,
/// else `l`) added before a trailing `|` or `@{}`.
pub fn widen_spec(spec: &str) -> String {
    let mut depth = 0;
    let mut letter = 'l';
    for c in spec.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            'l' | 'c' | 'r' if depth == 0 => letter = c,
            _ => {}
        }
    }
    if let Some(stem) = spec.strip_suffix('|') {
        format!("{stem}|{letter}|")
    } else if let Some(stem) = spec.strip_suffix("@{}") {
        format!("{stem}{letter}@{{}}")
    } else {
        format!("{spec}{letter}")
    }
}

/// The edits of Add Column in `span` (descending): an empty cell at the end of every row line,
/// and the column specification widened.
pub fn add_column_edits(text: &str, span: &EnvSpan) -> Vec<TextEdit> {
    let body = body_lines(text, span);
    let mut depth = 0;
    let mut edits = Vec::new();
    for (start, line) in &body {
        let parsed = split_line(line, &mut depth);
        if parsed.skip || (parsed.cells.len() < 2 && parsed.tail.is_none()) {
            continue;
        }
        let first = start + parsed.indent.len();
        let content_len: usize = parsed.cells.iter().map(|c| c.len() + 1).sum::<usize>() - 1;
        let cells_end = first + content_len;
        let before = &text[*start..cells_end];
        let at = start + before.trim_end().len();
        edits.push(TextEdit::insert(at, " &"));
    }
    edits.reverse();
    // (A key-value specification, as tabularray's `{colspec={…}}`, is left alone.)
    if let Some(spec) = spec_range(text, &span.name, span.begin.end)
        && !text[spec.clone()].contains('=')
    {
        edits.push(TextEdit {
            range: spec.clone(),
            text: widen_spec(&text[spec]),
        });
    }
    edits
}

// ───────────── Commands ─────────────

/// Adds the actions, their default accelerators and their palette descriptions.
pub fn install(state: &Rc<WindowState>) {
    for &(name, label, _) in ACTIONS {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        let action_name = name.to_owned();
        action.connect_activate(move |_, _| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let Some(editor) = state.active_editor() else {
                return;
            };
            let result = match action_name.as_str() {
                "align-columns" => align_in(&editor),
                "insert-row-below" => insert_row_in(&editor),
                _ => add_column_in(&editor),
            };
            if let Err(message) = result {
                state.toast(&message);
            }
        });
        state.window.add_action(&action);
        app::describe_action(&format!("win.{name}"), label, ShortcutGroup::Editing, None);
    }
    app::set_default_accels(state, &[("win.align-columns", ACTIONS[0].2)]);
}

/// The innermost environment of `family` around `editor`'s cursor.
fn enclosing(editor: &Editor, family: &[&str]) -> Option<EnvSpan> {
    let mut offset = editor.cursor_offset();
    loop {
        let span = pairs::enclosing_env(editor, offset)?;
        if in_family(family, &span.name) {
            return Some(span);
        }
        offset = span.begin.start.checked_sub(1)?;
    }
}

fn apply(editor: &Editor, edits: &[TextEdit]) -> Result<(), String> {
    envtools::without_live_rename(|| editor.document().apply_edits(edits, Origin::User))
        .map_err(|e| format!("Could not change the text: {e}"))
}

/// `win.align-columns` in `editor`.
pub fn align_in(editor: &Editor) -> Result<(), String> {
    let span = enclosing(editor, ALIGN_FAMILY)
        .ok_or("Put the cursor inside an align, tabular, array or matrix environment")?;
    let edits = editor.document().with_text(|t| align_edits(t, &span));
    if edits.is_empty() {
        return Ok(());
    }
    apply(editor, &edits)
}

/// `win.insert-row-below` in `editor`.
pub fn insert_row_in(editor: &Editor) -> Result<(), String> {
    let span = enclosing(editor, ALIGN_FAMILY)
        .ok_or("Put the cursor inside a tabular, array or matrix environment")?;
    let cursor = editor.cursor_offset();
    let (edits, at) = editor
        .document()
        .with_text(|t| insert_row_edits(t, &span, cursor))
        .ok_or("Put the cursor on a row")?;
    apply(editor, &edits)?;
    editor.place_cursor_at(at);
    Ok(())
}

/// `win.add-column` in `editor`.
pub fn add_column_in(editor: &Editor) -> Result<(), String> {
    let span = enclosing(editor, TABLE_FAMILY)
        .ok_or("Put the cursor inside a tabular, array or matrix environment")?;
    let edits = editor.document().with_text(|t| add_column_edits(t, &span));
    if edits.is_empty() {
        return Ok(());
    }
    apply(editor, &edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, name: &str) -> EnvSpan {
        let b = text.find(&format!("\\begin{{{name}}}")).unwrap();
        let e = text.rfind(&format!("\\end{{{name}}}")).unwrap();
        EnvSpan {
            name: name.to_owned(),
            begin: b..b + name.len() + 8,
            end: e..e + name.len() + 6,
        }
    }

    fn apply_all(text: &str, edits: &[TextEdit]) -> String {
        assert!(
            edits.windows(2).all(|w| w[1].range.end <= w[0].range.start),
            "descending: {edits:?}"
        );
        let mut s = text.to_owned();
        for e in edits {
            s.replace_range(e.range.clone(), &e.text);
        }
        s
    }

    fn align(text: &str, name: &str) -> String {
        apply_all(text, &align_edits(text, &span(text, name)))
    }

    #[test]
    fn align_columns_table() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "align",
                "\\begin{align}\n  a &= b \\\\\n  ccc &= d + e\n\\end{align}",
                "\\begin{align}\n  a   &= b \\\\\n  ccc &= d + e\n\\end{align}",
            ),
            (
                "tabular",
                "\\begin{tabular}{lcr}\n\\hline\nName & Value & X \\\\\nLonger name & 1 & 22 \\\\ \\hline\na&b&c\\\\\n\\end{tabular}",
                "\\begin{tabular}{lcr}\n\\hline\nName        & Value & X  \\\\\nLonger name & 1     & 22 \\\\ \\hline\na           & b     & c  \\\\\n\\end{tabular}",
            ),
            // `\&` and braces are not separators; comments stay at the end.
            (
                "tabular",
                "\\begin{tabular}{ll}\n  R\\&D & {a & b} \\\\ % one\n  x & y\n\\end{tabular}",
                "\\begin{tabular}{ll}\n  R\\&D & {a & b} \\\\ % one\n  x    & y\n\\end{tabular}",
            ),
            // A nested matrix over several lines is left alone, the `&` inside it too.
            (
                "align",
                "\\begin{align}\n  f &= \\begin{pmatrix} 1 & 2 \\end{pmatrix} \\\\\n  gg &= \\begin{cases}\n    a & b\n  \\end{cases}\n\\end{align}",
                "\\begin{align}\n  f  &= \\begin{pmatrix} 1 & 2 \\end{pmatrix} \\\\\n  gg &= \\begin{cases}\n    a & b\n  \\end{cases}\n\\end{align}",
            ),
            // Empty first cells, a row with fewer cells, `\\[2pt]`.
            (
                "alignat",
                "\\begin{alignat}{2}\n  &x &&= 1 \\\\[2pt]\n  &yyy &&= 22\n\\end{alignat}",
                "\\begin{alignat}{2}\n  &x   &&= 1 \\\\[2pt]\n  &yyy &&= 22\n\\end{alignat}",
            ),
            (
                "pmatrix",
                "\\begin{pmatrix}\n  1 & 100 \\\\\n  10 & 1\n\\end{pmatrix}",
                "\\begin{pmatrix}\n  1  & 100 \\\\\n  10 & 1\n\\end{pmatrix}",
            ),
            // Already aligned: nothing changes.
            (
                "align",
                "\\begin{align}\n  a &= b\n\\end{align}",
                "\\begin{align}\n  a &= b\n\\end{align}",
            ),
            // Wide characters count as one column each.
            (
                "tabular",
                "\\begin{tabular}{ll}\nα & x \\\\\nabc & y \\\\\n\\end{tabular}",
                "\\begin{tabular}{ll}\nα   & x \\\\\nabc & y \\\\\n\\end{tabular}",
            ),
        ];
        for &(name, text, want) in cases {
            assert_eq!(align(text, name), want, "\n{text}");
            assert!(align_edits(want, &span(want, name)).is_empty(), "{want}");
        }
    }

    #[test]
    fn insert_row_table() {
        let text = "\\begin{tabular}{ll}\n  a & b \\\\\n  c & d\n\\end{tabular}";
        let s = span(text, "tabular");
        let at = text.find("a &").unwrap();
        let (edits, cursor) = insert_row_edits(text, &s, at).unwrap();
        let out = apply_all(text, &edits);
        assert_eq!(
            out,
            "\\begin{tabular}{ll}\n  a & b \\\\\n   & \\\\\n  c & d\n\\end{tabular}"
        );
        assert_eq!(cursor, out.find("   & \\\\").unwrap() + 2);
        // On the last row (no `\\`): it gets one, before its comment.
        let at = text.find("c &").unwrap();
        let (edits, cursor) = insert_row_edits(text, &s, at).unwrap();
        let out = apply_all(text, &edits);
        assert_eq!(
            out,
            "\\begin{tabular}{ll}\n  a & b \\\\\n  c & d \\\\\n   &\n\\end{tabular}"
        );
        assert_eq!(&out[cursor..cursor + 2], " &");
        let text = "\\begin{pmatrix}\n  1 & 2 % c\n\\end{pmatrix}";
        let (edits, _) =
            insert_row_edits(text, &span(text, "pmatrix"), text.find('1').unwrap()).unwrap();
        assert_eq!(
            apply_all(text, &edits),
            "\\begin{pmatrix}\n  1 & 2 \\\\ % c\n   &\n\\end{pmatrix}"
        );
        // Many blanks before the comment.
        let text = "\\begin{pmatrix}\n  1 & 2        % c\n\\end{pmatrix}";
        let (edits, cursor) =
            insert_row_edits(text, &span(text, "pmatrix"), text.find('1').unwrap()).unwrap();
        let out = apply_all(text, &edits);
        assert_eq!(
            out,
            "\\begin{pmatrix}\n  1 & 2 \\\\ % c\n   &\n\\end{pmatrix}"
        );
        assert_eq!(&out[cursor..], " &\n\\end{pmatrix}");
        // Not on a body line.
        assert!(insert_row_edits(text, &span(text, "pmatrix"), 0).is_none());
    }

    /// CRLF files keep their `\r\n` on every rewritten or inserted line.
    #[test]
    fn regress5_align_crlf() {
        let text = "\\begin{align}\r\n  a &= b \\\\\r\n  ccc &= dd\r\n\\end{align}\r\n";
        let want = "\\begin{align}\r\n  a   &= b \\\\\r\n  ccc &= dd\r\n\\end{align}\r\n";
        assert_eq!(align(text, "align"), want);
        assert!(
            align_edits(want, &span(want, "align")).is_empty(),
            "{want:?}"
        );
        let s = span(text, "align");
        let (edits, cursor) = insert_row_edits(text, &s, text.find("a &").unwrap()).unwrap();
        let out = apply_all(text, &edits);
        assert_eq!(
            out,
            "\\begin{align}\r\n  a &= b \\\\\r\n   & \\\\\r\n  ccc &= dd\r\n\\end{align}\r\n"
        );
        assert_eq!(&out[cursor..cursor + 2], " &");
        let (edits, cursor) = insert_row_edits(text, &s, text.find("ccc").unwrap()).unwrap();
        let out = apply_all(text, &edits);
        assert_eq!(
            out,
            "\\begin{align}\r\n  a &= b \\\\\r\n  ccc &= dd \\\\\r\n   &\r\n\\end{align}\r\n"
        );
        assert_eq!(&out[cursor..cursor + 2], " &");
        let text = "\\begin{tabular}{ll}\r\n  a & b \\\\\r\n\\end{tabular}\r\n";
        assert_eq!(
            apply_all(text, &add_column_edits(text, &span(text, "tabular"))),
            "\\begin{tabular}{lll}\r\n  a & b & \\\\\r\n\\end{tabular}\r\n"
        );
    }

    #[test]
    fn add_column_table() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "tabular",
                "\\begin{tabular}{|l|c|}\n  a & b \\\\\n  \\hline\n  c & d\n\\end{tabular}",
                "\\begin{tabular}{|l|c|c|}\n  a & b & \\\\\n  \\hline\n  c & d &\n\\end{tabular}",
            ),
            (
                "tabular*",
                "\\begin{tabular*}{\\textwidth}[t]{lr}\n  a & b \\\\\n\\end{tabular*}",
                "\\begin{tabular*}{\\textwidth}[t]{lrr}\n  a & b & \\\\\n\\end{tabular*}",
            ),
            (
                "bmatrix",
                "\\begin{bmatrix}\n  1 & 2 \\\\ % x\n  3 & 4\n\\end{bmatrix}",
                "\\begin{bmatrix}\n  1 & 2 & \\\\ % x\n  3 & 4 &\n\\end{bmatrix}",
            ),
        ];
        for &(name, text, want) in cases {
            let s = span(text, name);
            assert_eq!(apply_all(text, &add_column_edits(text, &s)), want);
        }
        assert_eq!(widen_spec("lc"), "lcc");
        assert_eq!(widen_spec("|l|r|"), "|l|r|r|");
        assert_eq!(widen_spec("@{}lr@{}"), "@{}lrr@{}");
        assert_eq!(widen_spec("p{3cm}"), "p{3cm}l");
    }

    #[test]
    fn families() {
        assert!(in_family(ALIGN_FAMILY, "align*"));
        assert!(in_family(ALIGN_FAMILY, "tabular*"));
        assert!(!in_family(ALIGN_FAMILY, "equation"));
        assert!(in_family(TABLE_FAMILY, "pmatrix"));
        assert!(!in_family(TABLE_FAMILY, "align"));
    }
}
