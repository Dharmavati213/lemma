//! What to look for ([`Query`]) and how it matches text ([`Matcher`]).
//!
//! Matching is **line by line**, as `rg` does it: a match never spans a line break, `^` and `$`
//! are the start and end of a line, and the line's own `\r` (a CRLF file) is part of the line, so
//! `$` does not match before it (`rg` without `--crlf`). Whole words follow `rg -w`: the match must
//! not be preceded or followed by a word character (`\b{start-half}…\b{end-half}`), so
//! `\alpha` as a whole word is found in `a \alpha b` but not in `x\alpha` or `\alphax`.

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
        Ok(Matcher {
            query: self.clone(),
            line,
            text,
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

/// True when a regular expression may use `\A`, `\z` or turn multi-line mode off (`(?-m)`),
/// where matching a whole text and matching each line differ.
fn anchors_haystack(pattern: &str) -> bool {
    if pattern.contains(r"\A") || pattern.contains(r"\z") {
        return true;
    }
    // A flag group `(?…)` that negates `m`: `(?-m)`, `(?s-m:…)`, `(?i-sm)`.
    let mut rest = pattern;
    while let Some(i) = rest.find("(?") {
        let flags: &str = &rest[i + 2..];
        let end = flags.find([':', ')']).unwrap_or(flags.len());
        if let Some(minus) = flags[..end].find('-')
            && flags[minus..end].contains('m')
        {
            return true;
        }
        rest = &rest[i + 2..];
    }
    false
}

/// One line of a text with matches (see [`Matcher::for_each_line`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHits {
    /// 0-based line number.
    pub line: u32,
    /// Byte range of the line in the text, without its `\n` (with a CRLF line's `\r`).
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
        let mut hits = |start: usize, end: usize, line: u32| -> ControlFlow<()> {
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
        match &self.text {
            Some(regex) => {
                // Every line match is also a match of the whole text at the same place, so the
                // leftmost match from `at` is never after the next line with matches.
                let mut at = 0;
                while at <= text.len() {
                    let Some(m) = regex.find_at(text, at) else {
                        break;
                    };
                    let start = memchr::memrchr(b'\n', &bytes[..m.start()]).map_or(0, |i| i + 1);
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
                    if end >= text.len() {
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
        // `$` is the end of the line: in a CRLF line `\r` comes first (as with rg).
        assert_eq!(lines(&Query::regex("b$"), text), [(0, vec!["b"])]);
        assert!(lines(&Query::regex("d$"), text).is_empty());
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
