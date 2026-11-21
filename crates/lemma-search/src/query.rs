//! What to look for ([`Query`]) and how it matches text ([`Matcher`]).
//!
//! Matching is **line by line**, as `rg` does it: a match never spans a line break, and `^` and
//! `$` are the start and end of a line. In a line that ends with `\r\n` (a CRLF file) the `\r`
//! belongs to the line break, as with `rg --crlf`: `$` matches before it and no match takes it
//! (`\s+$` finds the spaces, not the `\r`), so a replace never changes a file's line breaks.
//! Only a pattern that names a line break ([`names_line_break`]: `\r`, `\n`, the character
//! itself, but not in a class that excludes it, as `[^\n]` does) sees that `\r` as the line's
//! last character. Whole words follow `rg -w`: the match must
//! not be preceded or followed by a word character (`\b{start-half}…\b{end-half}`), so
//! `\alpha` as a whole word is found in `a \alpha b` but not in `x\alpha` or `\alphax`. As with
//! `rg`, a final line break ends the last line: no empty line follows it.

use regex::{Regex, RegexBuilder};
use std::fmt;
use std::ops::{ControlFlow, Range};

/// A search: the pattern and its options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Query {
    /// The text to find, or a regular expression (the `regex` crate's syntax) with `regex`.
    pub pattern: String,
    /// `pattern` is a regular expression.
    pub regex: bool,
    /// Upper and lower case differ (otherwise they match each other, Unicode-aware).
    pub case_sensitive: bool,
    /// Only matches not preceded or followed by a word character (letter, digit, `_`).
    pub whole_word: bool,
}

impl Query {
    /// A plain-text, case-insensitive query for `pattern`.
    pub fn plain(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            ..Self::default()
        }
    }

    /// A case-sensitive regular-expression query for `pattern`.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            regex: true,
            case_sensitive: true,
            whole_word: false,
        }
    }

    /// Compiles the query.
    pub fn matcher(&self) -> Result<Matcher, QueryError> {
        if self.pattern.is_empty() {
            return Err(QueryError::Empty);
        }
        let body = if self.regex {
            self.pattern.clone()
        } else {
            regex::escape(&self.pattern)
        };
        let pattern = if self.whole_word {
            format!(r"\b{{start-half}}(?:{body})\b{{end-half}}")
        } else {
            body
        };
        let build = |multi_line: bool| {
            RegexBuilder::new(&pattern)
                .case_insensitive(!self.case_sensitive)
                .multi_line(multi_line)
                .build()
                .map_err(|e| QueryError::Invalid(error_message(&e)))
        };
        let line = build(false)?;
        // A whole-text pass finds the lines worth matching (see `for_each_line`). It finds every
        // line match unless the pattern anchors at the start or end of the haystack, which the
        // line-by-line pass treats as the line's.
        let text = if self.regex && anchors_haystack(&self.pattern) {
            None
        } else {
            Some(build(true)?)
        };
        let sees_crlf = if self.regex {
            names_line_break(&self.pattern)
        } else {
            self.pattern.contains(['\r', '\n'])
        };
        Ok(Matcher {
            query: self.clone(),
            line,
            text,
            sees_crlf,
        })
    }
}

/// Why a query cannot be searched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// The pattern is empty.
    Empty,
    /// The regular expression does not compile; the message is short, without the pattern.
    Invalid(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the search text is empty"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for QueryError {}

/// The last line of the `regex` crate's error (`error: unclosed group`), capitalised and
/// without the `error: ` prefix; the pattern and caret above it are left out.
fn error_message(e: &regex::Error) -> String {
    let text = e.to_string();
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("invalid regular expression")
        .trim();
    let last = last.strip_prefix("error: ").unwrap_or(last);
    let mut chars = last.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Invalid regular expression".to_owned(),
    }
}

/// True when a regular expression may use `\A`, `\z`, turn multi-line mode off (`(?-m)`) or
/// CRLF mode on (`(?R)`), where matching a whole text and matching each line differ. (In CRLF
/// mode a whole text has no `$` between the `\r` and the `\n` of a CRLF line, where the line
/// alone ends.)
fn anchors_haystack(pattern: &str) -> bool {
    if pattern.contains(r"\A") || pattern.contains(r"\z") {
        return true;
    }
    // A flag group `(?…)` that negates `m` (`(?-m)`, `(?s-m:…)`, `(?i-sm)`) or sets `R`.
    let mut rest = pattern;
    while let Some(i) = rest.find("(?") {
        let flags: &str = &rest[i + 2..];
        let end = flags.find([':', ')']).unwrap_or(flags.len());
        let (set, unset) = flags[..end].split_once('-').unwrap_or((&flags[..end], ""));
        if unset.contains('m') || set.contains('R') {
            return true;
        }
        rest = &rest[i + 2..];
    }
    false
}

/// True when a regular expression names a carriage return or a line feed: as the character
/// itself, as `\r` or `\n`, or as a hexadecimal escape (`\x0D`, `\x{a}`, `\u000D`, …). `\s`,
/// `.` and classes such as `[^x]` match a `\r` without naming it, and a class that excludes it
/// (`[^\r\n]`, "the rest of the line") does not name it either.
fn names_line_break(pattern: &str) -> bool {
    // The classes open at this point, innermost last: whether each is negated.
    let mut classes: Vec<bool> = Vec::new();
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        let named = match c {
            '\r' | '\n' => true,
            '[' => {
                let negated = rest.starts_with('^');
                if negated {
                    rest = &rest[1..];
                }
                // A `]` right after the opening bracket is a literal one.
                rest = rest.strip_prefix(']').unwrap_or(rest);
                classes.push(negated);
                continue;
            }
            ']' => {
                classes.pop();
                continue;
            }
            '\\' => {
                let Some(escaped) = rest.chars().next() else {
                    break;
                };
                rest = &rest[escaped.len_utf8()..];
                match escaped {
                    'r' | 'n' => true,
                    'x' | 'u' | 'U' => {
                        let digits = match rest.strip_prefix('{') {
                            Some(braced) => braced.split('}').next().unwrap_or_default(),
                            None => {
                                let len = match escaped {
                                    'x' => 2,
                                    'u' => 4,
                                    _ => 8,
                                };
                                rest.get(..len).unwrap_or_default()
                            }
                        };
                        matches!(u32::from_str_radix(digits.trim(), 16), Ok(0x0a | 0x0d))
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        // Inside an odd number of negated classes the line break is excluded.
        if named
            && classes
                .iter()
                .filter(|&&negated| negated)
                .count()
                .is_multiple_of(2)
        {
            return true;
        }
    }
    false
}

/// One line of a text with matches (see [`Matcher::for_each_line`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHits {
    /// 0-based line number.
    pub line: u32,
    /// Byte range of the line in the text, without its `\n`, and without a CRLF line's `\r`
    /// unless the pattern names a line break (see the module docs).
    pub span: Range<usize>,
    /// The matches, as byte ranges **within the line**, in order, not overlapping.
    pub ranges: Vec<Range<usize>>,
}

/// A compiled [`Query`].
#[derive(Clone, Debug)]
pub struct Matcher {
    query: Query,
    /// Matches one line (no `\n` in the haystack).
    line: Regex,
    /// The same pattern in multi-line mode, for the whole-text pass; `None` when that pass could
    /// miss lines (see [`anchors_haystack`]).
    text: Option<Regex>,
    /// The pattern names a line break: the `\r` of a CRLF line is part of the line.
    sees_crlf: bool,
}

impl Matcher {
    /// The query this matcher was compiled from.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The compiled line regex (for capture groups).
    pub(crate) fn regex(&self) -> &Regex {
        &self.line
    }

    /// The matches in `line` (which should not contain `\n`), as byte ranges.
    pub fn find_in_line<'a>(&'a self, line: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        self.line.find_iter(line).map(|m| m.range())
    }

    /// Calls `f` for every line of `text` that has matches, in order. `f` can stop the walk.
    ///
    /// As with `rg`, no line follows a final line break: the empty line an editor shows after
    /// it is not searched, so `^` or `$` does not match there (and a replace adds nothing after
    /// the last line break). An empty text is one empty line.
    pub fn for_each_line(
        &self,
        text: &str,
        mut f: impl FnMut(LineHits) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let bytes = text.as_bytes();
        // Lines counted so far: `counted_line` is the number of the line that starts at
        // `counted_at`.
        let (mut counted_at, mut counted_line) = (0usize, 0u32);
        let mut line_of = |start: usize| {
            counted_line += memchr::memchr_iter(b'\n', &bytes[counted_at..start]).count() as u32;
            counted_at = start;
            counted_line
        };
        let mut hits = |start: usize, mut end: usize, line: u32| -> ControlFlow<()> {
            if !self.sees_crlf
                && end > start
                && bytes[end - 1] == b'\r'
                && bytes.get(end) == Some(&b'\n')
            {
                end -= 1;
            }
            let ranges = self.line_ranges(&text[start..end]);
            if ranges.is_empty() {
                return ControlFlow::Continue(());
            }
            f(LineHits {
                line,
                span: start..end,
                ranges,
            })
        };
        // The whole-text regex has no `$` before the `\r` of a CRLF line, where a line without it
        // ends: such a text is matched line by line.
        let text_pass = self
            .text
            .as_ref()
            .filter(|_| self.sees_crlf || memchr::memmem::find(bytes, b"\r\n").is_none());
        match text_pass {
            Some(regex) => {
                // Every line match is also a match of the whole text at the same place, so the
                // leftmost match from `at` is never after the next line with matches.
                let mut at = 0;
                while at <= text.len() {
                    let Some(m) = regex.find_at(text, at) else {
                        break;
                    };
                    let start = memchr::memrchr(b'\n', &bytes[..m.start()]).map_or(0, |i| i + 1);
                    if start == text.len() && start > 0 {
                        // The empty "line" after a final line break.
                        break;
                    }
                    let end = memchr::memchr(b'\n', &bytes[m.start()..])
                        .map_or(text.len(), |i| m.start() + i);
                    let line = line_of(start);
                    hits(start, end, line)?;
                    at = end + 1;
                }
            }
            None => {
                let mut start = 0;
                let mut line = 0u32;
                loop {
                    let end =
                        memchr::memchr(b'\n', &bytes[start..]).map_or(text.len(), |i| start + i);
                    hits(start, end, line)?;
                    // Nothing after the last line break.
                    if end + 1 >= text.len() {
                        break;
                    }
                    start = end + 1;
                    line += 1;
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Every line of `text` with matches (see [`Matcher::for_each_line`]).
    pub fn find_lines(&self, text: &str) -> Vec<LineHits> {
        let mut out = Vec::new();
        let _ = self.for_each_line(text, |hits| {
            out.push(hits);
            ControlFlow::Continue(())
        });
        out
    }

    /// The number of matches in `text`.
    pub fn count(&self, text: &str) -> usize {
        let mut n = 0;
        let _ = self.for_each_line(text, |hits| {
            n += hits.ranges.len();
            ControlFlow::Continue(())
        });
        n
    }

    fn line_ranges(&self, line: &str) -> Vec<Range<usize>> {
        self.find_in_line(line).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(q: &Query, text: &str) -> Vec<(u32, Vec<&'static str>)> {
        let m = q.matcher().unwrap();
        m.find_lines(text)
            .into_iter()
            .map(|h| {
                let line = &text[h.span.clone()];
                let found = h
                    .ranges
                    .iter()
                    .map(|r| &*Box::leak(line[r.clone()].to_owned().into_boxed_str()))
                    .collect();
                (h.line, found)
            })
            .collect()
    }

    #[test]
    fn plain_is_literal_and_case_insensitive_by_default() {
        let text = "a.b A.B\nxab\n\n(a.b)\n";
        assert_eq!(
            lines(&Query::plain("a.b"), text),
            [(0, vec!["a.b", "A.B"]), (3, vec!["a.b"])]
        );
        let q = Query {
            case_sensitive: true,
            ..Query::plain("a.b")
        };
        assert_eq!(lines(&q, text), [(0, vec!["a.b"]), (3, vec!["a.b"])]);
    }

    #[test]
    fn whole_word_follows_rg() {
        let text =
            "a \\alpha b\nx\\alpha y\n\\alphax\nfoo_bar foo\n(foo) -foo- foo1\nαfoo βfoo foo\n";
        let q = Query {
            whole_word: true,
            ..Query::plain("\\alpha")
        };
        assert_eq!(lines(&q, text), [(0, vec!["\\alpha"])]);
        let q = Query {
            whole_word: true,
            ..Query::plain("foo")
        };
        assert_eq!(
            lines(&q, text),
            [(3, vec!["foo"]), (4, vec!["foo", "foo"]), (5, vec!["foo"])]
        );
        let q = Query {
            whole_word: true,
            ..Query::plain("(foo)")
        };
        assert_eq!(lines(&q, text), [(4, vec!["(foo)"])]);
    }

    #[test]
    fn regex_is_per_line() {
        let text = "ab\ncd\r\nef";
        // `\s` never matches the line break.
        assert!(lines(&Query::regex(r"b\s*c"), text).is_empty());
        assert_eq!(lines(&Query::regex("^c"), text), [(1, vec!["c"])]);
        // `$` is the end of the line: in a CRLF line before the `\r` (as with `rg --crlf`), after
        // it for a pattern that names it.
        assert_eq!(lines(&Query::regex("b$"), text), [(0, vec!["b"])]);
        assert_eq!(lines(&Query::regex("d$"), text), [(1, vec!["d"])]);
        assert_eq!(lines(&Query::regex(r"d\r$"), text), [(1, vec!["d\r"])]);
        assert_eq!(lines(&Query::regex("f$"), text), [(2, vec!["f"])]);
    }

    #[test]
    fn haystack_anchors_are_line_anchors() {
        let text = "ab\nab\n";
        assert_eq!(
            lines(&Query::regex(r"\Aa"), text),
            [(0, vec!["a"]), (1, vec!["a"])]
        );
        assert_eq!(
            lines(&Query::regex(r"b\z"), text),
            [(0, vec!["b"]), (1, vec!["b"])]
        );
        assert_eq!(
            lines(&Query::regex(r"(?-m)^a"), text),
            [(0, vec!["a"]), (1, vec!["a"])]
        );
        assert!(anchors_haystack(r"(?s-m:x)"));
        assert!(!anchors_haystack(r"(?i)x-m"));
        assert!(!anchors_haystack(r"(?im)x"));
        // In CRLF mode a whole text has no `$` between the `\r` and the `\n` of a CRLF line,
        // where the line alone ends.
        assert_eq!(
            lines(&Query::regex(r"(?R)\r$"), "a\r\nb\r\nc"),
            [(0, vec!["\r"]), (1, vec!["\r"])]
        );
        assert_eq!(
            lines(&Query::regex(r"(?sR:.)$"), "a\r\nb"),
            [(0, vec!["a"]), (1, vec!["b"])]
        );
        assert!(anchors_haystack(r"(?R)x"));
        assert!(anchors_haystack(r"(?iR:x)"));
        assert!(!anchors_haystack(r"(?i-R)x"));
    }

    #[test]
    fn empty_matches_and_last_line() {
        let text = "x\n\ny";
        assert_eq!(
            lines(&Query::regex("^"), text),
            [(0, vec![""]), (1, vec![""]), (2, vec![""])]
        );
        assert_eq!(lines(&Query::regex("a*"), "baaa"), [(0, vec!["", "aaa"])]);
        assert_eq!(lines(&Query::plain("y"), text), [(2, vec!["y"])]);
        assert!(lines(&Query::plain("z"), "").is_empty());
        assert_eq!(lines(&Query::regex("^$"), ""), [(0, vec![""])]);
        // As with rg, no line follows a final line break, in the whole-text pass and in the
        // line-by-line one.
        assert_eq!(lines(&Query::regex("^"), "x\n"), [(0, vec![""])]);
        assert_eq!(lines(&Query::regex("^$"), "x\n\n"), [(1, vec![""])]);
        assert_eq!(lines(&Query::regex(r"\s*$"), "x\r\n"), [(0, vec![""])]);
        assert_eq!(lines(&Query::regex(r"(?-m)$"), "x\n"), [(0, vec![""])]);
        assert_eq!(
            lines(&Query::regex(r"\A"), "x\n\n"),
            [(0, vec![""]), (1, vec![""])]
        );
    }

    /// The `\r` of a CRLF line is the line break's, unless the pattern names a line break; a
    /// `\r` that is not before a `\n` is the line's.
    #[test]
    fn crlf_line_breaks_are_not_part_of_the_line() {
        let text = "a  \r\nb\t\r\n\r\nc\r";
        for (pattern, expect) in [
            (
                r"\s+$",
                vec![(0, vec!["  "]), (1, vec!["\t"]), (3, vec!["\r"])],
            ),
            (
                r".$",
                vec![(0, vec![" "]), (1, vec!["\t"]), (3, vec!["\r"])],
            ),
            (
                r"[^a]+$",
                vec![(0, vec!["  "]), (1, vec!["b\t"]), (3, vec!["c\r"])],
            ),
            (r"\s+\r$", vec![(0, vec!["  \r"]), (1, vec!["\t\r"])]),
            // A class that excludes the line break does not name it (as with `rg --crlf`).
            (
                r"[^\n]+$",
                vec![(0, vec!["a  "]), (1, vec!["b\t"]), (3, vec!["c\r"])],
            ),
            (r"[^\r\n]+$", vec![(0, vec!["a  "]), (1, vec!["b\t"])]),
            (
                r"\x0D",
                vec![
                    (0, vec!["\r"]),
                    (1, vec!["\r"]),
                    (2, vec!["\r"]),
                    (3, vec!["\r"]),
                ],
            ),
        ] {
            assert_eq!(lines(&Query::regex(pattern), text), expect, "{pattern}");
        }
        // Without a CRLF line, the whole-text pass finds the same.
        assert_eq!(
            lines(&Query::regex(r"\s+$"), "a  \nb\r"),
            [(0, vec!["  "]), (1, vec!["\r"])]
        );
        // A plain pattern with the character itself.
        assert_eq!(lines(&Query::plain("b\t\r"), text), [(1, vec!["b\t\r"])]);
    }

    #[test]
    fn line_breaks_named_in_a_pattern() {
        for named in [
            r"\r",
            r"a\n",
            "\r",
            r"[\r]",
            r"\x0d",
            r"\x0A",
            r"\x{D}",
            r"\x{000a}",
            r"\u000D",
            r"\U0000000a",
            r"\u{a}",
            r"[^\n]\r",
            r"[]\n]",
            r"[\[\n]",
            r"[^[^\n]]",
            r"[^a]\n",
        ] {
            assert!(names_line_break(named), "{named}");
        }
        for unnamed in [
            r"\s+$",
            r".",
            r"\\r",
            r"\\n",
            r"\x0b",
            r"\x{dd}",
            r"\u00d0",
            r"[^a]",
            r"\pC",
            r"x\",
            r"\x",
            r"\x{",
            r"[^\n]",
            r"%[^\r\n]*$",
            r"[^\x0d]",
            r"[^]\n]",
            r"[^\]\n]",
            r"[^a[\n]]",
            r"[a[^\n]]",
            r"[[:^space:]\pL&&[^\r]]",
        ] {
            assert!(!names_line_break(unnamed), "{unnamed}");
        }
    }

    #[test]
    fn errors() {
        assert_eq!(Query::plain("").matcher().unwrap_err(), QueryError::Empty);
        let e = Query::regex("(foo").matcher().unwrap_err();
        assert_eq!(e, QueryError::Invalid("Unclosed group".into()));
        // A plain pattern is never invalid.
        assert!(Query::plain("(foo").matcher().is_ok());
    }

    #[test]
    fn unicode_case_folding_and_counts() {
        let m = Query::plain("straße").matcher().unwrap();
        assert_eq!(m.count("STRASSE Straße STRAẞE"), 2);
        let m = Query::plain("é").matcher().unwrap();
        assert_eq!(m.count("É é e"), 2);
    }
}
