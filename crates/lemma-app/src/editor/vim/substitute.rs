//! Vim's `:[range]s/pattern/replacement/[flags]`, with GtkSourceView's meaning, as one edit.
//!
//! GtkSourceView 5.22 substitutes in a loop of `gtk_source_search_context_forward` and
//! `gtk_source_search_context_replace`, and its regex search context scans the buffer again
//! from the start after every change, so each replacement costs the whole buffer: `:%s` of 3300
//! matches in 10 000 lines froze the window for 18–50 s, of 20 000 in 60 000 lines for more
//! than ten minutes. [`execute`] finds the same matches in one pass over the text and applies
//! them as one [`Document::apply_edits`] (`Origin::User`, one undo step), as the find bar's
//! Replace All is one: 0.27 s and 1.5 s.
//!
//! **What it means** is what GtkSourceView's command does (`gtksourcevimcommand.c`), which is
//! not quite Vim's:
//! - the pattern is a GRegex (PCRE2) pattern, taken as typed (`\(`, `\+` and `\<` mean what
//!   PCRE2 says), compiled as GtkSourceView compiles a search: multi-line, caseless with `i`,
//!   no empty matches; only the flags `g` and `i` count (other characters are ignored);
//! - the replacement is expanded by GRegex's rules (`\0`, `\1`, `\g<name>`, `\n`, `\U`…; `&`
//!   is an `&`);
//! - the range is `%`, or two addresses `A,B` (a line number, `.`, `$`, `+N`, `'<`, `'>`) read
//!   as GtkSourceView reads them: a line number and `.` are the start of their line, so
//!   `:1,3s` stops before line 3. A single address (`:5s`, `:.s`) is no command at all. Without
//!   a range, it substitutes from the cursor (not the start of its line) to the end of the
//!   cursor's line;
//! - without `g` only a line's first match is replaced, a line being one of GTK's after the
//!   replacements before it (taking the `\n` of a `\r\n` joins two lines, and an inserted `\r`
//!   ends one even before a `\n`);
//! - each search goes on one character after the end of the replacement (or of a match left
//!   alone), and finds the next of the matches a search of the whole buffer from its start
//!   finds: a match that starts right where the previous one ended is left, so `:s/a/b/g`
//!   makes `aaa` `bab`, and `:s/aa/X/g` makes `aaaaaa` `XaaX` (not `XaXa`);
//! - the cursor goes to the first non-blank character of the line of the last replacement; the
//!   pattern becomes Vim's search pattern (for `n`), and the cursor's place before goes on Vim's
//!   jump list (`Ctrl+O`). For those two, [`execute`] runs GtkSourceView's own command on an
//!   empty range at the top of the buffer, which finds the first match and stops there.
//!
//! The matches are those GtkSourceView finds, except where GtkSourceView's searches see its own
//! replacements (Lemma's see the text before the substitution): a replacement that makes a
//! match with the text around it, or a lookbehind longer than one character that looks back
//! into one. And GtkSourceView's replace of a match that could go on (`ab?`, `(a)(b)?`) right
//! before another match replaces that one too, and then fails (a critical; the command stops
//! there) or, when the texts happen to fit, leaves a wrong text: Lemma replaces each match
//! once. Lines are counted as GTK splits a text it reads: a lone `\r` that earlier edits left
//! inside one of GTK's lines (by taking the `\n` after it) counts as a line break. Live
//! environment rename stays out of a substitution, as out of the find bar's replacements.
//!
//! Anything else is left to GtkSourceView, which then does what it always did: a separator
//! that is not ASCII punctuation, other marks than `'<` and `'>` (Vim's own marks are not
//! visible from outside), a pattern or a replacement that does not compile or expand, and a
//! command that replaces nothing (there GtkSourceView costs one search).

use crate::document::{self, Document};
use crate::editor::{Editor, envtools};
use gtk::prelude::*;
use lemma_core::{Origin, TextEdit};

/// An address of a range, as GtkSourceView's `parse_position` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Address {
    /// `N`: the start of line N (1-based; `0` is the first line too).
    Line(u32),
    /// `.`: the start of the cursor's line.
    Current,
    /// `$`: the end of the buffer.
    End,
    /// `+N`: the end of the line N lines below the cursor's.
    Below(u32),
    /// `'<`: the start of the line where the selection starts.
    SelectionStart,
    /// `'>`: the end of the selection, moved back over a line break it starts after.
    SelectionEnd,
}

/// The range of a substitution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    /// None given: from the cursor to the end of its line.
    Cursor,
    /// `%`: the whole buffer.
    Whole,
    /// `A,B`.
    Between(Address, Address),
}

/// A parsed substitution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Substitute<'a> {
    pub range: Range,
    pub pattern: String,
    pub replacement: String,
    /// `g`: every match of a line.
    pub global: bool,
    /// `i`: case does not matter.
    pub caseless: bool,
    /// The command after its range (`s/…`), as typed.
    pub command: &'a str,
}

/// Parses `command` (`:` optional) when GtkSourceView would run it as a substitution that
/// [`execute`] can do the same way; `None` otherwise (see the module docs).
pub fn parse(command: &str) -> Option<Substitute<'_>> {
    let line = command.strip_prefix(':').unwrap_or(command);
    let (range, rest) = parse_range(line)?;
    // GtkSourceView looks the command up in its table first; `:set` and `:sort` are the
    // only ones there that start with `s`, also with arguments after a space.
    let name = rest.split(' ').next().unwrap_or(rest);
    if matches!(name, "set" | "sort") {
        return None;
    }
    let options = rest.strip_prefix('s')?;
    let separator = options.chars().next()?;
    if !separator.is_ascii_punctuation() || separator == '\\' {
        return None;
    }
    let (pattern, replacement, flags) = split_options(&options[1..], separator)?;
    if pattern.is_empty() {
        return None;
    }
    Some(Substitute {
        range,
        pattern,
        replacement,
        global: flags.contains('g'),
        caseless: flags.contains('i'),
        command: rest,
    })
}

/// The range at the start of `line` and the rest, as GtkSourceView's `parse_range` reads it:
/// `%`, two addresses with a `,` between them, or none. `None` when `line` starts with another
/// address: GtkSourceView then takes the whole line as the command, which does not start with
/// `s`, so it runs nothing; or the address is a mark Lemma cannot see.
fn parse_range(line: &str) -> Option<(Range, &str)> {
    if let Some(rest) = line.strip_prefix('%') {
        return Some((Range::Whole, rest));
    }
    if !line.starts_with([
        '\'', '.', '$', '+', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
    ]) {
        return Some((Range::Cursor, line));
    }
    let (first, rest) = parse_address(line)?;
    let rest = rest.strip_prefix(',')?;
    let (second, rest) = parse_address(rest)?;
    Some((Range::Between(first, second), rest))
}

/// One address at the start of `s` and the rest; `None` when GtkSourceView would not read one
/// there, or reads a mark Lemma cannot see.
fn parse_address(s: &str) -> Option<(Address, &str)> {
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    // GtkSourceView adds the digits up in an `int`; more than nine would overflow it.
    let number = |s: &str| (s.len() <= 9).then(|| s.parse::<u32>().ok()).flatten();
    match s.as_bytes().first()? {
        b'\'' => match s.as_bytes().get(1)? {
            b'<' => Some((Address::SelectionStart, &s[2..])),
            b'>' => Some((Address::SelectionEnd, &s[2..])),
            _ => None,
        },
        b'.' => Some((Address::Current, &s[1..])),
        b'$' => Some((Address::End, &s[1..])),
        b'+' => {
            let n = digits(&s[1..]);
            (n > 0).then_some(())?;
            Some((Address::Below(number(&s[1..=n])?), &s[1 + n..]))
        }
        b'0'..=b'9' => {
            let n = digits(s);
            Some((Address::Line(number(&s[..n])?), &s[n..]))
        }
        _ => None,
    }
}

/// Splits what follows `s` and its separator into the pattern, the replacement and the flags,
/// as GtkSourceView's `gtk_source_vim_command_parse_search_and_replace` does: a backslash
/// before the separator makes it part of the text (without the backslash); any other backslash
/// stays. A missing closing separator ends the text there. `None` when it ends in a backslash.
fn split_options(s: &str, separator: char) -> Option<(String, String, &str)> {
    /// The text up to the next unescaped separator, and what follows that separator (`None`:
    /// there was none).
    fn part(s: &str, separator: char) -> Option<(String, Option<&str>)> {
        let mut out = String::new();
        let mut escaped = false;
        for (i, c) in s.char_indices() {
            if escaped {
                escaped = false;
                if c == separator {
                    out.pop();
                }
            } else if c == '\\' {
                escaped = true;
            } else if c == separator {
                return Some((out, Some(&s[i + c.len_utf8()..])));
            }
            out.push(c);
        }
        (!escaped).then_some((out, None))
    }
    let (pattern, rest) = part(s, separator)?;
    let Some(rest) = rest else {
        return Some((pattern, String::new(), ""));
    };
    let (replacement, flags) = part(rest, separator)?;
    Some((pattern, replacement, flags.unwrap_or("")))
}

/// The edits of a substitution and the line of its last replacement.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Descending, as [`Document::apply_edits`] takes them; empty when nothing is replaced.
    pub edits: Vec<TextEdit>,
    /// The line (GTK's numbering, in the text after the edits) of the last replacement.
    pub last_line: i32,
}

/// The pattern compiled as GtkSourceView's search compiles it (multi-line, caseless with `i`,
/// no empty matches). `OPTIMIZE` (PCRE2's JIT) finds the same matches, and its matching skips
/// the UTF-8 check of the rest of the text that every other search would repeat.
pub fn compile(pattern: &str, caseless: bool) -> Option<glib::Regex> {
    let mut flags = glib::RegexCompileFlags::MULTILINE | glib::RegexCompileFlags::OPTIMIZE;
    if caseless {
        flags |= glib::RegexCompileFlags::CASELESS;
    }
    glib::Regex::new(pattern, flags, glib::RegexMatchFlags::NOTEMPTY)
        .ok()
        .flatten()
}

/// What GtkSourceView's loop replaces in `text` (see the module docs): starting at byte
/// `start`, up to the matches that start at or after byte `end`, or, without an `end`, to the
/// end of the line of `start`. `None` when a match or a replacement fails (GtkSourceView then
/// stops at that match).
pub fn plan(
    text: &glib::GStr,
    regex: &glib::Regex,
    replacement: &str,
    start: usize,
    end: Option<usize>,
    global: bool,
) -> Option<Plan> {
    let mut lines = Lines::new(text.as_bytes());
    let first_line = i64::from(lines.line_at(start));
    let mut edits = Vec::new();
    let mut last_line = -1;
    // Lines added by the replacements so far, less those they removed, as GTK counts them: a
    // deletion removes the line ends inside it (taking the `\r` of a `\r\n` removes none, and
    // taking its `\n` joins two lines even though a lone `\r` is left), and an insertion adds
    // the line breaks of its own text (a `\r` at its end is one, also before a `\n`).
    let mut moved = 0i64;
    // Where GtkSourceView's next search starts.
    let mut from = start;
    let found = regex
        .match_full(text, 0, glib::RegexMatchFlags::empty())
        .ok()?;
    while found.matches() {
        let (s, e) = found.fetch_pos(0)?;
        let (s, e) = (usize::try_from(s).ok()?, usize::try_from(e).ok()?);
        if s >= from {
            let line = i64::from(lines.line_at(s)) + moved;
            let past = match end {
                Some(end) => s >= end,
                None => line != first_line,
            };
            if past {
                break;
            }
            if global || line != last_line {
                last_line = line;
                let with = found
                    .expand_references(replacement)
                    .ok()?
                    .map(String::from)
                    .unwrap_or_default();
                moved += line_breaks(&with) - i64::from(breaks_in(text.as_bytes(), s, e));
                edits.push(TextEdit {
                    range: s..e,
                    text: with,
                });
            }
            from = e + text[e..].chars().next().map_or(0, char::len_utf8);
        }
        found.next().ok()?;
    }
    edits.reverse();
    Some(Plan {
        edits,
        last_line: i32::try_from(last_line).ok()?,
    })
}

/// GTK's line numbers of byte offsets of a text, asked for in increasing order. A line ends
/// after `\n`, `\r\n`, a lone `\r` or U+2029 (Pango's paragraph separators); an offset between
/// the `\r` and the `\n` of a line break is on the line that ends there.
struct Lines<'a> {
    bytes: &'a [u8],
    at: usize,
    line: u32,
}

impl<'a> Lines<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            line: 0,
        }
    }

    fn line_at(&mut self, offset: usize) -> u32 {
        let offset = offset.min(self.bytes.len());
        if offset > self.at {
            self.line += breaks_in(self.bytes, self.at, offset);
            self.at = offset;
        }
        self.line
    }
}

/// The line breaks that end in `bytes[from..to]` (see [`Lines`]).
fn breaks_in(bytes: &[u8], from: usize, to: usize) -> u32 {
    let mut n = 0;
    for i in from..to {
        n += u32::from(match bytes[i] {
            b'\n' => true,
            b'\r' => bytes.get(i + 1) != Some(&b'\n'),
            0xE2 => bytes[i + 1..].starts_with(&[0x80, 0xA9]),
            _ => false,
        });
    }
    n
}

/// The line breaks in `s` on its own (as GTK splits a text it inserts into lines).
fn line_breaks(s: &str) -> i64 {
    i64::from(breaks_in(s.as_bytes(), 0, s.len()))
}

/// GLib's `g_unichar_isspace`: what GtkSourceView skips to the first non-blank character.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\r' | '\u{c}' | '\u{b}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Runs the ex command `command` (as `execute-command` gives it) in `editor` when it is a
/// substitution [`parse`] reads and it replaces something; true when it did. Otherwise
/// GtkSourceView runs it (see the module docs).
pub fn execute(editor: &Editor, command: &str) -> bool {
    let Some(im) = editor.vim() else {
        return false;
    };
    let Some(substitute) = parse(command) else {
        return false;
    };
    let Some(regex) = compile(&substitute.pattern, substitute.caseless) else {
        return false;
    };
    let doc = editor.document();
    let buffer = editor.buffer();
    let (start, end) = bounds(doc, buffer, substitute.range);
    let text = document::buffer_text(buffer);
    let Some(plan) = plan(
        text.as_gstr(),
        &regex,
        &substitute.replacement,
        start,
        end,
        substitute.global,
    ) else {
        return false;
    };
    if plan.edits.is_empty() {
        return false;
    }
    drop(text);
    // GtkSourceView's own command, on the empty range at the top of the buffer: it makes the
    // pattern Vim's search pattern, puts the cursor's place on the jump list (there is a
    // match), and stops at that match, before replacing anything.
    im.execute_command(&format!(":1,1{}", substitute.command));
    // A substitution changes only its matches, as the find bar's replacements do.
    let applied = envtools::without_live_rename(|| doc.apply_edits(&plan.edits, Origin::User));
    if let Err(e) = applied {
        tracing::warn!("{command}: {e}");
        return true;
    }
    let mut cursor = buffer
        .iter_at_line(plan.last_line)
        .unwrap_or_else(|| buffer.end_iter());
    while !cursor.ends_line() && is_space(cursor.char()) {
        cursor.forward_char();
    }
    buffer.select_range(&cursor, &cursor);
    true
}

/// The byte offsets where GtkSourceView's loop starts for `range` and where it stops (`None`:
/// at the end of the line it starts on).
fn bounds(doc: &Document, buffer: &sourceview5::Buffer, range: Range) -> (usize, Option<usize>) {
    let (begin, end) = match range {
        Range::Cursor => {
            // The selection's start, or the cursor.
            let (start, _) = buffer.selection_bounds().unwrap_or_else(|| {
                let cursor = buffer.iter_at_mark(&buffer.get_insert());
                (cursor, cursor)
            });
            return (doc.byte_offset(&start), None);
        }
        Range::Whole => buffer.bounds(),
        Range::Between(a, b) => (position(buffer, a), position(buffer, b)),
    };
    (doc.byte_offset(&begin), Some(doc.byte_offset(&end)))
}

/// The place of `address` in `buffer`, as GtkSourceView's `parse_position` finds it.
fn position(buffer: &sourceview5::Buffer, address: Address) -> gtk::TextIter {
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let selection = buffer.iter_at_mark(&buffer.selection_bound());
    let line_start = |line: i32| {
        buffer
            .iter_at_line(line)
            .unwrap_or_else(|| buffer.end_iter())
    };
    match address {
        Address::Line(n) => line_start(i32::try_from(n.saturating_sub(1)).unwrap_or(i32::MAX)),
        Address::Current => line_start(cursor.line()),
        Address::End => buffer.end_iter(),
        Address::Below(n) => {
            let mut iter = cursor;
            iter.forward_lines(i32::try_from(n).unwrap_or(i32::MAX));
            if !iter.ends_line() {
                iter.forward_to_line_end();
            }
            iter
        }
        Address::SelectionStart => {
            let mut iter = cursor.min(selection);
            if !iter.starts_line() {
                iter.set_line_offset(0);
            }
            iter
        }
        Address::SelectionEnd => {
            let mut iter = cursor.max(selection);
            if !iter.ends_line() && iter.starts_line() {
                iter.backward_char();
            }
            iter
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(command: &str) -> Option<Substitute<'_>> {
        parse(command)
    }

    #[test]
    fn commands_gtksourceview_runs_as_substitutions() {
        let s = sub(":%s/with/WITH/g").unwrap();
        assert_eq!(s.range, Range::Whole);
        assert_eq!(
            (s.pattern.as_str(), s.replacement.as_str()),
            ("with", "WITH")
        );
        assert!(s.global && !s.caseless);
        assert_eq!(s.command, "s/with/WITH/g");
        let s = sub(":s/a/b/").unwrap();
        assert_eq!(s.range, Range::Cursor);
        assert!(!s.global);
        assert_eq!(
            sub(":10,20s/a/b/gi").unwrap().range,
            Range::Between(Address::Line(10), Address::Line(20))
        );
        assert_eq!(
            sub(":.,$s/a/b/").unwrap().range,
            Range::Between(Address::Current, Address::End)
        );
        assert_eq!(
            sub(":.,+3s/a/b/").unwrap().range,
            Range::Between(Address::Current, Address::Below(3))
        );
        assert_eq!(
            sub(":'<,'>s/a/b/").unwrap().range,
            Range::Between(Address::SelectionStart, Address::SelectionEnd)
        );
        // Only `g` and `i` count, wherever they are.
        let s = sub(":s/a/b/xgcI").unwrap();
        assert!(s.global && !s.caseless);
        assert!(sub(":s/a/b/ix").unwrap().caseless);
        // Other punctuation separates too; a backslash before the separator makes it text.
        let s = sub(":s#a/b#c\\#d#").unwrap();
        assert_eq!((s.pattern.as_str(), s.replacement.as_str()), ("a/b", "c#d"));
        let s = sub(":s/a\\/b/\\1\\n/").unwrap();
        assert_eq!(
            (s.pattern.as_str(), s.replacement.as_str()),
            ("a/b", "\\1\\n")
        );
        // Missing separators end the parts.
        let s = sub(":s/foo").unwrap();
        assert_eq!((s.pattern.as_str(), s.replacement.as_str()), ("foo", ""));
        let s = sub(":s/foo/bar").unwrap();
        assert_eq!((s.pattern.as_str(), s.replacement.as_str()), ("foo", "bar"));
        // Spaces are part of the texts.
        let s = sub(":s/a b/c d/").unwrap();
        assert_eq!((s.pattern.as_str(), s.replacement.as_str()), ("a b", "c d"));
        assert!(sub("%s/x/y/").is_some(), "without the colon");
    }

    #[test]
    fn other_commands_stay_with_gtksourceview() {
        for command in [
            ":s",
            ":s/",
            ":s//x/",
            ":s/a\\",
            ":sort",
            ":set hls",
            ":sav x.tex",
            ":sx",
            ":s a b ",
            ":s\\a\\b\\",
            ":5s/a/b/",
            ":.s/a/b/",
            ":'a,'bs/x/y/",
            ":1,s/x/y/",
            ":+,3s/x/y/",
            ":1234567890,2s/x/y/",
            ": s/x/y/",
            ":%sort",
            ":w",
            ":%d",
        ] {
            assert_eq!(sub(command), None, "{command}");
        }
    }

    fn run(
        text: &str,
        pattern: &str,
        with: &str,
        start: usize,
        end: Option<usize>,
        g: bool,
    ) -> String {
        let regex = compile(pattern, false).unwrap();
        let text = glib::GString::from(text);
        let planned = plan(text.as_gstr(), &regex, with, start, end, g).unwrap();
        let mut out = text.to_string();
        for edit in &planned.edits {
            out.replace_range(edit.range.clone(), &edit.text);
        }
        out
    }

    #[test]
    fn matches_are_those_gtksourceview_replaces() {
        // Measured against GtkSourceView 5.22 (and checked again in `gtk_vim`).
        assert_eq!(run("aaa\n", "a", "b", 0, None, true), "bab\n");
        assert_eq!(run("aaaaaa\n", "aa", "X", 0, None, true), "XaaX\n");
        assert_eq!(run("a a a\n", "a", "b", 2, None, true), "a b b\n");
        assert_eq!(run("a a a\n", "a", "b", 2, None, false), "a b a\n");
        let text = "x a a\nb a a\n";
        assert_eq!(
            run(text, "a", "Z", 0, Some(text.len()), false),
            "x Z a\nb Z a\n"
        );
        assert_eq!(
            run(text, "a", "Z", 0, Some(text.len()), true),
            "x Z Z\nb Z Z\n"
        );
        // A line number is the start of its line: `:1,3s` leaves line 3.
        let text = "x a\nb a\nc a\nd a\n";
        assert_eq!(
            run(text, "a", "Z", 0, Some(8), false),
            "x Z\nb Z\nc a\nd a\n"
        );
        assert_eq!(
            run("foo bar\n", "(foo) (bar)", "\\2 \\1", 0, None, false),
            "bar foo\n"
        );
        assert_eq!(
            run("foo bar\n", "(?<x>foo)", "\\g<x>\\g<0>", 0, None, false),
            "foofoo bar\n"
        );
        assert_eq!(run("abc\n", "b", "\\U&x", 0, None, false), "a&Xc\n");
        assert_eq!(
            run("ab\ncd\nef\n", "b\\ncd\\ne", "X", 0, Some(9), false),
            "aXf\n"
        );
        assert_eq!(run("caf\u{e9} x\n", "\\w+", "W", 0, None, true), "W W\n");
        // A replacement that adds a line ends a substitution without a range (the next match is
        // on another line now), and moves the lines of a range.
        assert_eq!(run("a a\na\n", "a", "\\n", 0, None, true), "\n a\na\n");
        assert_eq!(
            run("a\na\na\n", "a", "b\\nc", 0, Some(6), false),
            "b\nc\nb\nc\nb\nc\n"
        );
        assert_eq!(run("aXa aXa\n", "a", "aa", 0, None, true), "aaXaa aaXaa\n");
        // Taking the `\r` of a `\r\n` moves no line; taking its `\n` joins two (GTK does not
        // split at the lone `\r` left), so the next line's first match is on the same line.
        let crlf = "a\r\nb\r\nc\r\n";
        assert_eq!(
            run(crlf, "\\r", "", 0, Some(crlf.len()), false),
            "a\nb\nc\n"
        );
        assert_eq!(run(crlf, "\\r", "", 0, None, true), "a\nb\r\nc\r\n");
        assert_eq!(
            run(crlf, "\\n", ";", 0, Some(crlf.len()), false),
            "a\r;b\r\nc\r;"
        );
        // Patterns that match nothing but an empty string replace nothing.
        assert_eq!(run("x\ny\n", "^", "> ", 0, Some(4), false), "x\ny\n");
    }

    #[test]
    fn plan_reports_the_line_of_the_last_replacement() {
        let regex = compile("a", false).unwrap();
        let text = glib::GString::from("a\na\nb\na\n");
        let text = text.as_gstr();
        let planned = plan(text, &regex, "x\\ny", 0, Some(text.len()), false).unwrap();
        assert_eq!(planned.edits.len(), 3);
        assert!(
            planned
                .edits
                .windows(2)
                .all(|w| w[1].range.end <= w[0].range.start)
        );
        // Two lines added before the last match, on line 3 before.
        assert_eq!(planned.last_line, 5);
        // GTK makes a line of an inserted `\r`, also before a `\n`.
        let three = glib::GString::from("a\na\na\n");
        let planned = plan(three.as_gstr(), &regex, "x\\r", 0, Some(6), false).unwrap();
        assert_eq!(planned.last_line, 4);
        // A replacement that fails to expand leaves the command to GtkSourceView; `\x00` adds
        // nothing, in GLib's expansion as in GtkSourceView's.
        assert_eq!(plan(text, &regex, "\\g", 0, None, false), None);
        let planned = plan(text, &regex, "\\x00y", 0, None, false).unwrap();
        assert_eq!(
            planned.edits,
            [TextEdit {
                range: 0..1,
                text: "y".into()
            }]
        );
    }

    #[test]
    fn lines_follow_gtk() {
        let text = "a\nb\r\nc\rd\u{2029}e";
        let mut lines = Lines::new(text.as_bytes());
        let at = |s: &str| text.find(s).unwrap();
        assert_eq!(lines.line_at(at("a")), 0);
        assert_eq!(lines.line_at(at("b")), 1);
        // Between `\r` and `\n`: still the line that ends there.
        assert_eq!(lines.line_at(at("\r\n") + 1), 1);
        assert_eq!(lines.line_at(at("c")), 2);
        assert_eq!(lines.line_at(at("d")), 3);
        assert_eq!(lines.line_at(at("e")), 4);
        assert_eq!(line_breaks("x\r\ny\nz\r"), 3);
        // In a text, a `\r` before a `\n` is not a line break of its own.
        assert_eq!(breaks_in(b"a\r\nb", 1, 2), 0);
        assert_eq!(breaks_in(b"a\r\nb", 2, 3), 1);
        assert!(is_space('\u{3000}') && !is_space('\u{85}') && !is_space('x'));
    }
}
