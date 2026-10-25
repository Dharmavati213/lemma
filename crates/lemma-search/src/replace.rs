//! Replacing matches: the replacement text ([`Template`]) and the edits that apply it
//! ([`plan_replace`]).
//!
//! In plain-text mode the replacement is inserted as it is. With a regular expression,
//! `$1`…`$99`, `$name` and `${name}` insert a capture group (`$0` the whole match) and `$$` a
//! dollar sign; a reference to a group the expression does not have stays as typed, so the
//! LaTeX in `$x$` survives. Backslashes are never escapes (`\textbf` stays `\textbf`).
//!
//! The editor's find bar uses the same syntax with GLib's regular expressions (GtkSourceView's
//! search): [`Template::for_groups`] parses for another engine's groups, and
//! [`Template::to_glib`] writes the template in `g_regex_replace`'s syntax.

use crate::query::Matcher;
use std::ops::{ControlFlow, Range};

/// A parsed replacement text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Chunk {
    Text(String),
    /// A capture group by index (names are resolved when parsing).
    Group(usize),
}

impl Template {
    /// Parses `template` for `matcher` (see the module docs).
    pub fn new(matcher: &Matcher, template: &str) -> Self {
        if !matcher.query().regex {
            return Self::literal(template);
        }
        let regex = matcher.regex();
        Self::for_groups(template, regex.captures_len(), |name| {
            regex.capture_names().position(|n| n == Some(name))
        })
    }

    /// Parses `template` (see the module docs) for a regular expression of any engine with
    /// `groups` capture groups, the whole match included (so at least 1), and the named group
    /// that `named` gives the index of.
    pub fn for_groups(
        template: &str,
        groups: usize,
        named: impl Fn(&str) -> Option<usize>,
    ) -> Self {
        let name = |name: &str| -> Option<usize> {
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
                return name.parse::<usize>().ok().filter(|i| *i < groups);
            }
            named(name).filter(|i| !name.is_empty() && *i < groups)
        };
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut text = String::new();
        let mut rest = template;
        while let Some(dollar) = rest.find('$') {
            text.push_str(&rest[..dollar]);
            let after = &rest[dollar + 1..];
            let (group, used) = if after.starts_with('$') {
                text.push('$');
                (None, 1)
            } else if let Some(inner) = after.strip_prefix('{') {
                match inner.find('}') {
                    Some(close) => match name(&inner[..close]) {
                        Some(i) => (Some(i), close + 2),
                        None => {
                            text.push('$');
                            (None, 0)
                        }
                    },
                    None => {
                        text.push('$');
                        (None, 0)
                    }
                }
            } else if after.starts_with(|c: char| c.is_ascii_digit()) {
                let digits = after.bytes().take_while(u8::is_ascii_digit).count().min(2);
                match (name(&after[..digits]), name(&after[..1])) {
                    (Some(i), _) => (Some(i), digits),
                    (None, Some(i)) => (Some(i), 1),
                    (None, None) => {
                        text.push('$');
                        (None, 0)
                    }
                }
            } else if after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                let len = after
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .count();
                match name(&after[..len]) {
                    Some(i) => (Some(i), len),
                    None => {
                        text.push('$');
                        (None, 0)
                    }
                }
            } else {
                text.push('$');
                (None, 0)
            };
            if let Some(group) = group {
                if !text.is_empty() {
                    chunks.push(Chunk::Text(std::mem::take(&mut text)));
                }
                chunks.push(Chunk::Group(group));
            }
            rest = &after[used..];
        }
        text.push_str(rest);
        if !text.is_empty() {
            chunks.push(Chunk::Text(text));
        }
        Self { chunks }
    }

    /// A replacement inserted as it is.
    pub fn literal(text: &str) -> Self {
        let chunks = if text.is_empty() {
            Vec::new()
        } else {
            vec![Chunk::Text(text.to_owned())]
        };
        Self { chunks }
    }

    /// True when no capture group is used: every match is replaced by the same text.
    pub fn is_literal(&self) -> bool {
        self.chunks.iter().all(|c| matches!(c, Chunk::Text(_)))
    }

    /// The template in GLib's replacement syntax (`g_regex_replace`, which GtkSourceView's
    /// search replaces with): each group as `\g<N>`, backslashes doubled, other text as it is.
    pub fn to_glib(&self) -> String {
        let mut out = String::new();
        for chunk in &self.chunks {
            match chunk {
                Chunk::Text(t) => out.push_str(&t.replace('\\', "\\\\")),
                Chunk::Group(i) => out.push_str(&format!("\\g<{i}>")),
            }
        }
        out
    }

    /// The text every match is replaced with, when [`Template::is_literal`].
    fn literal_text(&self) -> Option<String> {
        self.is_literal().then(|| {
            self.chunks
                .iter()
                .map(|c| match c {
                    Chunk::Text(t) => t.as_str(),
                    Chunk::Group(_) => "",
                })
                .collect()
        })
    }

    fn expand_captures(&self, caps: &regex::Captures<'_>) -> String {
        let mut out = String::new();
        for chunk in &self.chunks {
            match chunk {
                Chunk::Text(t) => out.push_str(t),
                Chunk::Group(i) => out.push_str(caps.get(*i).map_or("", |m| m.as_str())),
            }
        }
        out
    }

    /// The replacement of the match at `range` of `line` (a range [`Matcher::find_in_line`]
    /// returned for it).
    pub fn expand(&self, matcher: &Matcher, line: &str, range: Range<usize>) -> String {
        if let Some(text) = self.literal_text() {
            return text;
        }
        matcher
            .regex()
            .captures_at(line, range.start)
            .filter(|caps| caps.get(0).is_some_and(|m| m.range() == range))
            .map_or_else(String::new, |caps| self.expand_captures(&caps))
    }
}

/// One edit of a replace: the text's bytes `range` become `text`. It covers the replaced
/// matches of one line, from the first to the last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub range: Range<usize>,
    pub text: String,
    /// The 0-based line.
    pub line: u32,
    /// The matches it replaces.
    pub matches: usize,
}

/// A match that a replace is about to change (see [`plan_replace`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit<'a> {
    /// 0-based line.
    pub line: u32,
    /// Byte offset of the match in its line.
    pub column: usize,
    /// The matched text.
    pub text: &'a str,
}

/// The edits that replace every match of `matcher` in `text` with `template`, one per line with
/// matches, in **descending** order (ready for `lemma_core::apply_all` and
/// `Document::apply_edits`). `keep` decides each match; matches it refuses stay. Lines whose
/// text would not change get no edit.
pub fn plan_replace(
    matcher: &Matcher,
    template: &Template,
    text: &str,
    mut keep: impl FnMut(&Hit<'_>) -> bool,
) -> Vec<Replacement> {
    let literal = template.literal_text();
    let mut out = Vec::new();
    let _ = matcher.for_each_line(text, |hits| {
        let line = &text[hits.span.clone()];
        // (range in the line, replacement) of the kept matches.
        let mut parts: Vec<(Range<usize>, String)> = Vec::new();
        let mut decide = |range: Range<usize>, replacement: &dyn Fn() -> String| {
            let hit = Hit {
                line: hits.line,
                column: range.start,
                text: &line[range.clone()],
            };
            if keep(&hit) {
                parts.push((range, replacement()));
            }
        };
        match &literal {
            Some(literal) => {
                for range in &hits.ranges {
                    decide(range.clone(), &|| literal.clone());
                }
            }
            None => {
                for caps in matcher.regex().captures_iter(line) {
                    let Some(m) = caps.get(0) else { continue };
                    decide(m.range(), &|| template.expand_captures(&caps));
                }
            }
        }
        if let (Some(first), Some(last)) = (parts.first(), parts.last()) {
            let (start, end) = (first.0.start, last.0.end);
            let mut new = String::new();
            let mut at = start;
            for (range, replacement) in &parts {
                new.push_str(&line[at..range.start]);
                new.push_str(replacement);
                at = range.end;
            }
            if new != line[start..end] {
                out.push(Replacement {
                    range: hits.span.start + start..hits.span.start + end,
                    text: new,
                    line: hits.line,
                    matches: parts.len(),
                });
            }
        }
        ControlFlow::Continue(())
    });
    out.reverse();
    out
}

/// The replacement of each match `ranges` of `line` (what the replace preview shows).
pub fn preview(
    matcher: &Matcher,
    template: &Template,
    line: &str,
    ranges: &[Range<usize>],
) -> Vec<String> {
    ranges
        .iter()
        .map(|r| template.expand(matcher, line, r.clone()))
        .collect()
}

/// `text` with `edits` (descending, not overlapping, as [`plan_replace`] returns them) applied.
pub fn apply(text: &str, edits: &[Replacement]) -> String {
    let mut out = text.to_owned();
    for edit in edits {
        out.replace_range(edit.range.clone(), &edit.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;

    fn replace(q: &Query, template: &str, text: &str) -> String {
        let m = q.matcher().unwrap();
        let t = Template::new(&m, template);
        apply(text, &plan_replace(&m, &t, text, |_| true))
    }

    #[test]
    fn plain_replacement_is_literal() {
        let q = Query::plain("foo");
        assert_eq!(
            replace(&q, "$1\\textbf{x}", "a foo b FOO\nfoo"),
            "a $1\\textbf{x} b $1\\textbf{x}\n$1\\textbf{x}"
        );
        assert_eq!(replace(&q, "", "foo-foo"), "-");
    }

    #[test]
    fn capture_groups() {
        let q = Query::regex(r"\\emph\{([^}]*)\}");
        assert_eq!(
            replace(&q, r"\textit{$1}", r"a \emph{x} and \emph{yz}."),
            r"a \textit{x} and \textit{yz}."
        );
        let q = Query::regex(r"(?P<a>\w+)=(?P<b>\w+)");
        assert_eq!(replace(&q, "$b=$a", "x=y, p=q"), "y=x, q=p");
        assert_eq!(replace(&q, "${b}_${a}!", "x=y"), "y_x!");
        assert_eq!(replace(&q, "$0;", "x=y"), "x=y;");
        assert_eq!(replace(&q, "$$", "x=y"), "$");
    }

    #[test]
    fn unknown_groups_stay_as_typed() {
        // LaTeX math in a regex replacement: `$x$` is not a group reference.
        let q = Query::regex(r"\bvar\b");
        assert_eq!(replace(&q, "$x$", "a var b"), "a $x$ b");
        assert_eq!(replace(&q, "${x}", "var"), "${x}");
        assert_eq!(replace(&q, "$1", "var"), "$1");
        assert_eq!(replace(&q, "$", "var"), "$");
        assert_eq!(replace(&q, "${", "var"), "${");
        assert_eq!(replace(&q, "$ ", "var"), "$ ");
        // `$12` takes group 1 and a literal 2 when there is no group 12.
        let q = Query::regex(r"(a)(b)");
        assert_eq!(replace(&q, "$12", "ab"), "a2");
        assert_eq!(replace(&q, "${2}1", "ab"), "b1");
        assert_eq!(replace(&q, "$2$1", "ab"), "ba");
    }

    #[test]
    fn edits_are_per_line_descending_and_skip_no_ops() {
        let q = Query::plain("a");
        let m = q.matcher().unwrap();
        let t = Template::new(&m, "b");
        let text = "a a\nx\naxa\n";
        let edits = plan_replace(&m, &t, text, |_| true);
        assert_eq!(
            edits,
            [
                Replacement {
                    range: 6..9,
                    text: "bxb".into(),
                    line: 2,
                    matches: 2
                },
                Replacement {
                    range: 0..3,
                    text: "b b".into(),
                    line: 0,
                    matches: 2
                },
            ]
        );
        // Matches the caller refuses stay.
        let edits = plan_replace(&m, &t, text, |h| !(h.line == 2 && h.column == 0));
        assert_eq!(apply(text, &edits), "b b\nx\naxb\n");
        // Replacing a match with itself changes nothing.
        let same = Template::new(&m, "a");
        assert!(plan_replace(&m, &same, text, |_| true).is_empty());
        // Case-insensitive matches are replaced too.
        assert_eq!(replace(&q, "b", "A"), "b");
    }

    #[test]
    fn preview_expands_each_match() {
        let q = Query::regex(r"(\w)(\d)");
        let m = q.matcher().unwrap();
        let t = Template::new(&m, "$2$1");
        let line = "a1 b2";
        let ranges: Vec<_> = m.find_in_line(line).collect();
        assert_eq!(preview(&m, &t, line, &ranges), ["1a", "2b"]);
        assert!(!t.is_literal());
        assert!(Template::new(&m, "x$9").is_literal());
    }

    #[test]
    fn templates_for_other_engines_in_glib_syntax() {
        // Two groups besides the whole match; `b` is group 2.
        let named = |name: &str| (name == "b").then_some(2);
        let glib = |template: &str| Template::for_groups(template, 3, named).to_glib();
        assert_eq!(glib(r"\textbf{$1}"), r"\\textbf{\g<1>}");
        assert_eq!(glib("$b-${b}-$0-$$"), r"\g<2>-\g<2>-\g<0>-$");
        // References to groups the expression does not have stay as typed, as with `new`.
        assert_eq!(glib("$x$ ${c} $3 $12"), r"$x$ ${c} $3 \g<1>2");
        assert_eq!(glib(r"a\nb"), r"a\\nb");
        // A resolver that names a group past the count is not trusted.
        let wrong = Template::for_groups("$z", 1, |_| Some(4));
        assert_eq!(wrong.to_glib(), "$z");
        assert_eq!(Template::literal(r"\x").to_glib(), r"\\x");
        // `new` parses the same way.
        let q = Query::regex(r"(?P<a>\w+)=(?P<b>\w+)");
        let m = q.matcher().unwrap();
        assert_eq!(Template::new(&m, "$b=$a$c").to_glib(), r"\g<2>=\g<1>$c");
    }

    #[test]
    fn crlf_lines_keep_their_ends() {
        let q = Query::plain("x");
        assert_eq!(replace(&q, "yy", "x\r\nax\r\n"), "yy\r\nayy\r\n");
    }
}
