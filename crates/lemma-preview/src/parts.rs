//! What a document gives a preview — the preamble of a complete document and the definitions
//! outside it ([`doc_parts`]) — and whether an edit can change that ([`EditWatch::keeps`]), so
//! that the app reads the whole text again only after an edit that can: typing in a document's
//! body, outside its definitions and environment names, cannot.

use crate::body::split_preamble;
use crate::defs::watched_definitions;
use std::ops::Range;

/// What a document gives a preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocParts<'a> {
    /// The preamble of a complete document: the text before its `\begin{document}`, when that
    /// has `\documentclass` or `\input` (a preamble may load the class from a file of its own).
    pub preamble: Option<&'a str>,
    /// The [`definitions`](crate::definitions) after the preamble, or in the whole text of a
    /// document that is not complete.
    pub definitions: String,
}

/// Where an edit could change a document's [`DocParts`] (see [`EditWatch::keeps`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditWatch {
    /// Where the body of a complete document starts (0 for other documents).
    body_start: usize,
    /// What the definitions depend on besides comments and control words (the arguments of
    /// definition commands and of `\begin`, verbatim environments): sorted, disjoint byte ranges.
    spans: Vec<Range<usize>>,
}

/// The [`DocParts`] of `text`, and where an edit could change them.
pub fn doc_parts(text: &str) -> (DocParts<'_>, EditWatch) {
    let (preamble, body_start) = match split_preamble(text) {
        Some((preamble, body))
            if preamble.contains("\\documentclass") || preamble.contains("\\input") =>
        {
            (Some(preamble), text.len() - body.len())
        }
        _ => (None, 0),
    };
    let (definitions, found) = watched_definitions(&text[body_start..]);
    let mut spans: Vec<Range<usize>> = Vec::with_capacity(found.len());
    for span in found {
        let span = body_start + span.start..body_start + span.end;
        match spans.last_mut() {
            Some(last) if span.start < last.end => last.end = last.end.max(span.end),
            _ => spans.push(span),
        }
    }
    (
        DocParts {
            preamble,
            definitions,
        },
        EditWatch { body_start, spans },
    )
}

impl EditWatch {
    /// Whether replacing `range` of the text by `new` leaves its [`DocParts`] as they are: the
    /// edit is in the body of a complete document (anywhere in another one, on a line that has
    /// no `\begin{document}` before or after it), touches no span (not even its ends), removes
    /// and inserts no `\`, `%` or line break, and does not continue a control word (the text
    /// before it does not end with a backslash or with the letters of a control word). `line`
    /// is the text's line that holds `range.start`, without its line break; it starts at byte
    /// `line_start`.
    pub fn keeps(&self, line: &str, line_start: usize, range: Range<usize>, new: &str) -> bool {
        const SPECIAL: [char; 4] = ['\\', '%', '\n', '\r'];
        const BEGIN_DOCUMENT: &str = "\\begin{document}";
        if range.start < self.body_start || range.start < line_start || range.end < range.start {
            return false;
        }
        let (from, to) = (range.start - line_start, range.end - line_start);
        // Past the end of the line: the edit removes its line break.
        let Some(removed) = line.get(from..to) else {
            return false;
        };
        if removed.contains(SPECIAL) || new.contains(SPECIAL) {
            return false;
        }
        // The preamble ends at the first `\begin{document}` of a line's code, also one the
        // definitions scanner reads as something else (after `\\`, as the token of a `\let`),
        // so no span covers it: without a complete preamble, an edit must not make or break one.
        if self.body_start == 0
            && (line.contains(BEGIN_DOCUMENT)
                || [&line[..from], new, &line[to..]]
                    .concat()
                    .contains(BEGIN_DOCUMENT))
        {
            return false;
        }
        let before = &line.as_bytes()[..from];
        let letters = before
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic() || **c == b'@')
            .count();
        if before.len() > letters && before[before.len() - letters - 1] == b'\\' {
            return false;
        }
        // The first span that ends after the edit's start: the edit removes some of it, or
        // inserts inside it, when the span starts before the edit's end.
        let i = self.spans.partition_point(|s| s.end <= range.start);
        !self.spans.get(i).is_some_and(|s| s.start < range.end)
    }

    /// Follows an edit that [`EditWatch::keeps`] allowed: `range` became `new_len` bytes.
    pub fn follow(&mut self, range: Range<usize>, new_len: usize) {
        let after = self.spans.partition_point(|s| s.start < range.end);
        let removed = range.len();
        for span in &mut self.spans[after..] {
            span.start = span.start - removed + new_len;
            span.end = span.end - removed + new_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether replacing `range` of `text` by `new` keeps its parts, asked as the app asks (with
    /// the line that holds `range.start`).
    fn keeps(text: &str, range: Range<usize>, new: &str) -> bool {
        let (_, watch) = doc_parts(text);
        let (line, line_start) = line_at(text, range.start);
        watch.keeps(line, line_start, range, new)
    }

    /// The line of `text` that holds byte `at`, without its line break, and where it starts.
    fn line_at(text: &str, at: usize) -> (&str, usize) {
        let start = text[..at].rfind('\n').map_or(0, |k| k + 1);
        let end = text[at..].find('\n').map_or(text.len(), |k| at + k);
        (&text[start..end], start)
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap_or_else(|| panic!("{needle:?}"))
    }

    const DOC: &str = "\\documentclass{article}\n\\newcommand{\\pre}{P}\n\\begin{document}\n\
Text and $x + y$ here.\n\\newcommand{\\cF}{\\mathcal{F}} after\n\\begin{equation}\na = b\n\
\\end{equation}\n% a comment \\newcommand{\\no}{N}\n\\begin{verbatim}\nverbatim \\def\\v{V}\n\
\\end{verbatim}\nEnd.\n\\end{document}\n";

    #[test]
    fn parts_of_a_complete_document_and_a_chapter() {
        let (parts, _) = doc_parts(DOC);
        assert_eq!(
            parts.preamble,
            Some("\\documentclass{article}\n\\newcommand{\\pre}{P}\n")
        );
        assert_eq!(
            parts.definitions,
            "\\makeatletter\n\\providecommand{\\cF}{\\mathcal{F}}\n\\makeatother\n"
        );
        let chapter = "\\section{S}\n\\newcommand{\\cG}{G}\nText.\n";
        let (parts, _) = doc_parts(chapter);
        assert_eq!(parts.preamble, None);
        assert_eq!(
            parts.definitions,
            "\\makeatletter\n\\providecommand{\\cG}{G}\n\\makeatother\n"
        );
        // A preamble that inputs the class.
        let (parts, _) = doc_parts("\\input{preamble}\n\\begin{document}\nx\n");
        assert_eq!(parts.preamble, Some("\\input{preamble}\n"));
    }

    #[test]
    fn typing_in_the_body_keeps_the_parts() {
        let text = at(DOC, "Text and") + 4;
        assert!(keeps(DOC, text..text, "s"));
        assert!(keeps(DOC, text..text + 4, ""), "a deletion");
        // In math, in a display, in a comment, at the end.
        let math = at(DOC, "x + y") + 1;
        assert!(keeps(DOC, math..math, " + z^2"));
        let display = at(DOC, "a = b") + 5;
        assert!(keeps(DOC, display..display, "+c"));
        let comment = at(DOC, "a comment") + 2;
        assert!(keeps(DOC, comment..comment, "longer "));
        let end = at(DOC, "End.");
        assert!(keeps(DOC, end..end + 3, "Fin"));
        // A word a little after a definition.
        let after = at(DOC, "}} after") + 4;
        assert!(keeps(DOC, after..after, "x"));
    }

    #[test]
    fn edits_that_can_change_the_parts() {
        // The preamble.
        let pre = at(DOC, "{P}") + 1;
        assert!(!keeps(DOC, pre..pre, "Q"));
        // A definition, from its backslash to the byte after it.
        let def = at(DOC, "\\mathcal{F}");
        assert!(!keeps(DOC, def + 1..def + 1, "x"));
        let close = at(DOC, "}} after") + 2;
        assert!(!keeps(DOC, close - 1..close - 1, "x"));
        assert!(!keeps(DOC, close..close + 1, ""), "the byte after it");
        // An environment name (it could become `verbatim`) and a verbatim environment.
        let name = at(DOC, "{equation}") + 3;
        assert!(!keeps(DOC, name..name, "x"));
        let verbatim = at(DOC, "verbatim \\def") + 2;
        assert!(!keeps(DOC, verbatim..verbatim, "x"));
        // Backslashes, comments and line breaks, typed or removed.
        let text = at(DOC, "Text and") + 4;
        for typed in ["\\", "%", "\n", "\r\n", "a\\b"] {
            assert!(!keeps(DOC, text..text, typed), "{typed:?}");
        }
        let line_end = at(DOC, "here.\n") + 5;
        assert!(!keeps(DOC, line_end..line_end + 1, ""), "a line break");
        let percent = at(DOC, "% a comment");
        assert!(!keeps(DOC, percent..percent + 1, ""));
        // Continuing a control word, or right after a backslash.
        let word = at(DOC, "\\end{document}") + 4;
        assert!(!keeps(DOC, word..word, "x"));
        let mid = at(DOC, "\\mathcal") + 1;
        assert!(!keeps(DOC, mid..mid, "x"));
    }

    #[test]
    fn a_chapter_keeps_its_parts_while_typing_outside_its_definitions() {
        let chapter = "Some \\textbf{text}.\n\\newcommandx{\\a}{A}\n\\documentclas text\n";
        let typed = at(chapter, "Some") + 4;
        assert!(keeps(chapter, typed..typed, " more"));
        // A command that would become a definition command, or `\documentclass`.
        let x = at(chapter, "x{\\a}");
        assert!(!keeps(chapter, x..x + 1, ""));
        let class = at(chapter, "clas ") + 4;
        assert!(!keeps(chapter, class..class, "s"));
    }

    #[test]
    fn a_begin_document_the_scanner_does_not_see() {
        // After `\\` and as the token of a `\let`, the scanner reads no `\begin`, but the
        // preamble still ends there: the edit would make these complete documents.
        for text in [
            "\\documentclass{article}\n\\\\begin{docment}\nx\n",
            "\\documentclass{article}\n\\let\\a\\begin{docment}\nx\n",
        ] {
            let typed = at(text, "docment") + 3;
            assert!(!keeps(text, typed..typed, "u"), "{text:?}");
            assert_eq!(doc_parts(text).0.preamble, None);
        }
        // And the other way round: the preamble had no class, a later one has.
        let text = "\\\\begin{document}\n\\documentclass{article}\n\\begin{document}\nx\n";
        assert_eq!(doc_parts(text).0.preamble, None);
        let name = at(text, "document}") + 3;
        assert!(!keeps(text, name..name + 1, ""));
        // Typing elsewhere in such a document is still kept.
        let x = at(text, "\nx\n") + 1;
        assert!(keeps(text, x..x, "y"));
    }

    #[test]
    fn a_definition_that_does_not_parse_watches_everything_after_it() {
        let text = "\\newcommand{\\a}\nlater text\n";
        let later = at(text, "later") + 5;
        assert!(!keeps(text, later..later, "x"));
        let text = "Before \\newcommand{\\a}\nlater\n";
        assert!(keeps(text, 2..2, "x"), "before it");
    }

    #[test]
    fn spans_follow_kept_edits() {
        let text = at(DOC, "Text and") + 4;
        let (line, line_start) = line_at(DOC, text);
        let (_, mut watch) = doc_parts(DOC);
        assert!(watch.keeps(line, line_start, text..text, "s and so on"));
        watch.follow(text..text, 11);
        let edited = format!("{}s and so on{}", &DOC[..text], &DOC[text..]);
        assert_eq!(watch, doc_parts(&edited).1);
        // A deletion before the definitions.
        let (_, mut watch) = doc_parts(&edited);
        watch.follow(text..text + 11, 0);
        assert_eq!(watch, doc_parts(DOC).1);
    }

    /// What the random documents are made of: definitions that parse and that do not, verbatim
    /// and other environments, comments, line breaks, braces and plain text.
    const FRAGMENTS: &[&str] = &[
        "\\documentclass{article}\n",
        "\\input{defs}",
        "\\begin{document}\n",
        // One the definitions scanner reads as something else.
        "\\\\begin{document}",
        "\\let\\a\\begin{document}",
        "\\newcommand{\\a}{b}",
        "\\newcommand",
        "\\renewcommand*\\c[1]{#1}",
        "\\def\\x#1{y}",
        "\\def",
        "\\let\\a=\\b",
        "\\let",
        "\\DeclareMathOperator{\\H}{H}",
        "\\begin{verbatim}",
        "\\end{verbatim}",
        "\\begin{equation}",
        "\\begin{",
        "verbatim",
        "document",
        "{",
        "}",
        "[",
        "]",
        "% ",
        "\n",
        "\n\n",
        " ",
        "x",
        "ab",
        "\\x",
        "\\\\",
        "\\%",
        "=",
        "*",
        "@",
        "$",
        "é",
    ];
    /// What the random edits type.
    const TYPED: &[&str] = &[
        "", "x", "s", "ab", " ", "{", "}", "[", "]", "=", "*", "@", "é", "ument", "batim",
    ];

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(96))]

        /// Every edit of a random document (each typed text at each position, replacing up to
        /// two characters) that `keeps` allows leaves the document's parts as they were, and
        /// the followed watch is the new text's.
        #[test]
        fn kept_edits_change_nothing(
            parts in proptest::collection::vec(0usize..FRAGMENTS.len(), 1..24)
        ) {
            let text: String = parts.iter().map(|&p| FRAGMENTS[p]).collect();
            let (old_parts, watch) = doc_parts(&text);
            let bounds: Vec<usize> =
                (0..=text.len()).filter(|&i| text.is_char_boundary(i)).collect();
            for (k, &start) in bounds.iter().enumerate() {
                let (line, line_start) = line_at(&text, start);
                for removed in 0..3 {
                    let Some(&end) = bounds.get(k + removed) else {
                        break;
                    };
                    for typed in TYPED {
                        if !watch.keeps(line, line_start, start..end, typed) {
                            continue;
                        }
                        let edited = format!("{}{typed}{}", &text[..start], &text[end..]);
                        let (new_parts, new_watch) = doc_parts(&edited);
                        proptest::prop_assert_eq!(&new_parts, &old_parts, "{:?} → {:?}", text, edited);
                        let mut followed = watch.clone();
                        followed.follow(start..end, typed.len());
                        proptest::prop_assert_eq!(&followed, &new_watch, "{:?} → {:?}", text, edited);
                    }
                }
            }
        }
    }
}
