//! Locating runaway arguments, which TeX reports without a line number.

use crate::LogReport;
use lemma_core::Diagnostic;
use std::path::PathBuf;

/// Below this many non-space characters a match says little about where the text came from.
const MIN_NEEDLE: usize = 8;

/// Finds where the text of a `Runaway argument?` starts in `sources`; returns the file and the
/// 1-based line.
///
/// TeX prints the tokens it read: control words get a trailing space, blank lines become `\par`,
/// comments disappear and long text ends in `\ETC.`. So `\ETC.` and `\par` are removed from the
/// runaway text, comments from the sources, and all whitespace from both before searching. When
/// the whole text is not found (for example because it spans an explicit `\par`), shorter and
/// shorter prefixes are tried, down to eight characters. Sources are searched in order; the
/// first match wins.
pub fn locate_runaway(runaway_text: &str, sources: &[(PathBuf, String)]) -> Option<(PathBuf, u32)> {
    let needle = squash(&strip_par(runaway_text.trim().trim_end_matches("\\ETC.")));
    if needle.len() < MIN_NEEDLE {
        return None;
    }
    let squashed: Vec<Squashed> = sources
        .iter()
        .map(|(_, text)| Squashed::new(text))
        .collect();
    let mut len = needle.len();
    loop {
        let prefix = &needle[..floor_char_boundary(&needle, len)];
        for ((path, _), source) in sources.iter().zip(&squashed) {
            if let Some(at) = source.text.find(prefix) {
                return Some((path.clone(), source.line_of(at)));
            }
        }
        if len <= MIN_NEEDLE {
            return None;
        }
        len = (len * 2 / 3).max(MIN_NEEDLE);
    }
}

/// Moves each runaway-argument error (see [`parse_log`](crate::parse_log)) to the location
/// [`locate_runaway`] finds in `sources`; errors whose text is not found keep TeX's location.
/// [`Build::wait`](crate::Build::wait) calls it with the project files listed in the build's
/// `.fls`.
pub fn locate_runaways(report: &mut LogReport, sources: &[(PathBuf, String)]) {
    for d in &mut report.diagnostics {
        let Some(text) = runaway_text(d) else {
            continue;
        };
        if let Some((path, line)) = locate_runaway(text, sources) {
            d.path = path;
            d.line = Some(line);
        }
    }
}

/// The runaway text of an error that [`parse_log`](crate::parse_log) merged a runaway into.
pub(crate) fn runaway_text(d: &Diagnostic) -> Option<&str> {
    let context = d.context.as_deref()?.strip_prefix("Runaway ")?;
    Some(context.split_once("? ")?.1)
}

/// Removes the control word `\par` (but not `\parbox` and the like).
fn strip_par(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\\par") {
        let after = &rest[at + 4..];
        out.push_str(&rest[..at]);
        if after.starts_with(|c: char| c.is_ascii_alphabetic()) {
            out.push_str("\\par");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A source without whitespace and comments, with the squashed offset at which each line starts.
struct Squashed {
    text: String,
    line_starts: Vec<usize>,
}

impl Squashed {
    fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut line_starts = Vec::new();
        for line in source.split('\n') {
            line_starts.push(text.len());
            let mut escaped = false;
            for c in line.chars() {
                if c == '%' && !escaped {
                    break;
                }
                escaped = c == '\\' && !escaped;
                if !c.is_whitespace() {
                    text.push(c);
                }
            }
        }
        Squashed { text, line_starts }
    }

    /// The 1-based line holding squashed offset `at`.
    fn line_of(&self, at: usize) -> u32 {
        let line = self.line_starts.partition_point(|&start| start <= at);
        u32::try_from(line).unwrap_or(u32::MAX)
    }
}

fn floor_char_boundary(s: &str, mut at: usize) -> usize {
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(files: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        files
            .iter()
            .map(|(p, t)| (PathBuf::from(p), (*t).to_owned()))
            .collect()
    }

    #[test]
    fn finds_unclosed_cite() {
        let src = sources(&[
            ("/p/ch.tex", "\\section{Other}\nNothing here.\n"),
            (
                "/p/main.tex",
                "Intro.\n% a comment {SGA1\nThe theory, initiated by Grothendieck~\\cite{SGA1, is treated at length in~\\cite{Vistoli}.\nWe follow the conventions of~\\cite{Stacks}.\n",
            ),
        ]);
        let text = "{SGA1, is treated at length in~\\cite {Vistoli}. We follow the convent\\ETC.";
        assert_eq!(
            locate_runaway(text, &src),
            Some((PathBuf::from("/p/main.tex"), 3))
        );
    }

    #[test]
    fn par_stands_for_blank_lines() {
        let src = sources(&[(
            "/p/main.tex",
            "a\n\\begin{theorem}\nis an \\emph{equivalence of categories.\n\\end{theorem}\n\n\\begin{proof}\nBy~\\cite{X}.\n",
        )]);
        let text =
            "{equivalence of categories. \\end {theorem} \\par \\begin {proof} By~\\cite \\ETC.";
        assert_eq!(
            locate_runaway(text, &src),
            Some((PathBuf::from("/p/main.tex"), 3))
        );
    }

    #[test]
    fn comments_inside_the_argument_and_escaped_percent() {
        let src = sources(&[(
            "/p/main.tex",
            "x\n\\footnote{Fifty \\% of all % a comment\nfootnotes never end.\n",
        )]);
        let text = "{Fifty \\% of all footnotes never end. \\par \\ETC.";
        assert_eq!(
            locate_runaway(text, &src),
            Some((PathBuf::from("/p/main.tex"), 2))
        );
    }

    #[test]
    fn shorter_prefix_when_the_text_differs_later() {
        let src = sources(&[(
            "/p/main.tex",
            "one\ntwo \\emph{unclosed text here\n\\par more\n",
        )]);
        let text = "{unclosed text here more\\ETC.";
        assert_eq!(
            locate_runaway(text, &src),
            Some((PathBuf::from("/p/main.tex"), 2))
        );
    }

    #[test]
    fn no_match_or_too_short() {
        let src = sources(&[("/p/main.tex", "nothing to see\n")]);
        assert_eq!(
            locate_runaway("{completely different text\\ETC.", &src),
            None
        );
        assert_eq!(locate_runaway("{ab\\ETC.", &src), None);
    }

    #[test]
    fn locate_runaways_moves_errors_to_the_runaway_start() {
        use lemma_core::{DiagSource, Severity};
        let error = |line: Option<u32>, context: &str| Diagnostic {
            path: PathBuf::from("/p/main.tex"),
            line,
            severity: Severity::Error,
            message: "File ended while scanning use of \\emph .".into(),
            context: Some(context.into()),
            source: DiagSource::Tex,
        };
        let mut report = LogReport {
            diagnostics: vec![
                // TeX noticed it at the `\input` line of the includer.
                error(
                    Some(4),
                    "Runaway argument? {unclosed text that runs on \\par until the file ends.",
                ),
                error(None, "Runaway argument? {text found nowhere at all\\ETC."),
                error(Some(9), "x <HERE> y"),
            ],
            ..LogReport::default()
        };
        let src = sources(&[
            ("/p/main.tex", "\\input{sec}\n"),
            (
                "/p/sec.tex",
                "Some text.\nMore \\emph{unclosed text that runs on\n\nuntil the file ends.\n",
            ),
        ]);
        locate_runaways(&mut report, &src);
        let located: Vec<_> = report
            .diagnostics
            .iter()
            .map(|d| (d.path.to_str(), d.line))
            .collect();
        assert_eq!(
            located,
            [
                (Some("/p/sec.tex"), Some(2)),
                (Some("/p/main.tex"), None),
                (Some("/p/main.tex"), Some(9)),
            ]
        );
    }

    #[test]
    fn strip_par_keeps_longer_control_words() {
        assert_eq!(strip_par("a \\par b \\parbox{x}\\par"), "a  b \\parbox{x}");
    }
}
