//! Finding the formula around a point: the whole inline, display or environment math the point
//! is in (from the scanner's line states, so comments and verbatim never count), or the whole
//! environment of a configured list (`tikzcd`, which the scanner does not treat as math).
//!
//! A formula is found from its delimiters: a math delimiter (`$`, `$$`, `\(`, `\[`, `\)`, `\]`)
//! or `\begin{env}`/`\end{env}` is an **opener** where the scanner context changes from not-math
//! to math across it and a **closer** where it changes back. The search walks back from the point
//! to the outermost opener (`$x$` inside `\text{…}` inside a display belongs to the display),
//! then forward to its closer, both bounded by [`MAX_LINES`]. A formula the scanner sees as
//! unterminated (a blank line ended `$…`) is not found, nor is the argument of `\ensuremath`
//! (it has no delimiter).

use lemma_syntax::{Context, LineStates, Lines, Token, TokenKind};
use std::ops::Range;

/// Lines searched before and after the point (a longer formula is not previewed).
pub const MAX_LINES: u32 = 300;
/// Longest formula source previewed, in bytes.
pub const MAX_BYTES: usize = 32 * 1024;

/// A position in a document: 0-based line and byte column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub line: u32,
    pub col: u32,
}

impl Point {
    pub fn new(line: u32, col: u32) -> Self {
        Self { line, col }
    }
}

/// What kind of formula was found.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FormulaKind {
    /// `$…$`, `\(…\)`, or an inline math environment (`math`).
    Inline,
    /// `$$…$$`, `\[…\]`, or a display math environment (`equation`, `align*`, …).
    Display,
    /// An environment of the configured list (`tikzcd`), outside math.
    Environment(String),
}

impl FormulaKind {
    /// True for content typeset in a paragraph of the text width (display math, environments).
    pub fn is_block(&self) -> bool {
        !matches!(self, FormulaKind::Inline)
    }
}

/// A formula: its kind, its source (delimiters included) and where it is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Formula {
    pub kind: FormulaKind,
    /// The opening delimiter's first byte.
    pub start: Point,
    /// Just after the closing delimiter.
    pub end: Point,
    /// The source from `start` to `end`, lines joined with `\n` (a `\r` before a line end is
    /// dropped).
    pub text: String,
    /// The math environment's name (with its `*`) when the formula is one, e.g. `align*`.
    pub env: Option<String>,
}

impl Formula {
    /// True when `p` lies inside the formula (its delimiters included).
    pub fn contains(&self, p: Point) -> bool {
        self.start <= p && p < self.end
    }
}

/// The formula at `at` (see the module docs): the outermost math around it, or the innermost
/// environment of `envs` around it, whichever encloses the other. `None` outside math and those
/// environments, and for a formula longer than [`MAX_LINES`] lines or [`MAX_BYTES`] bytes.
pub fn find_formula(
    states: &LineStates,
    lines: &impl Lines,
    at: Point,
    envs: &[String],
) -> Option<Formula> {
    if at.line >= lines.line_count() {
        return None;
    }
    let finder = Finder { states, lines };
    let math = finder.math(at);
    let env = finder.environment(at, envs);
    let (start, end, kind, env_name) = match (math, env) {
        (Some(m), Some(e)) if e.0 <= m.0 && m.1 <= e.1 => e,
        (Some(m), _) => m,
        (None, Some(e)) => e,
        (None, None) => return None,
    };
    let text = finder.text(start, end)?;
    Some(Formula {
        kind,
        start,
        end,
        text,
        env: env_name,
    })
}

/// A delimiter that opens or closes math (or a configured environment).
#[derive(Clone, Debug)]
struct Event {
    line: u32,
    range: Range<u32>,
    open: bool,
    /// The context before an opener (to tell a formula nested in `\text{…}`) or after a closer.
    outside: Context,
    /// The context just inside the formula (after an opener).
    inside: Context,
}

type Found = (Point, Point, FormulaKind, Option<String>);

struct Finder<'a, L: Lines> {
    states: &'a LineStates,
    lines: &'a L,
}

fn is_math(c: &Context) -> bool {
    matches!(c, Context::Math { .. })
}

impl<L: Lines> Finder<'_, L> {
    /// The context of the text before `col`, with a command name or a tracked argument
    /// (`\label{…}`, `\eqref{…}`) resolved to the mode around the command.
    fn mode(&self, line: u32, text: &str, col: u32) -> Context {
        let mut col = col;
        for _ in 0..8 {
            match self.states.context_at(line, text, col) {
                Context::CommandName { start } if start < col => col = start,
                Context::Arg { start, .. } if start > 0 && start - 1 < col => col = start - 1,
                c => return c,
            }
        }
        Context::Text
    }

    fn tokens(&self, line: u32, text: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        self.states.tokens(line, text, &mut tokens);
        tokens
    }

    /// The math openers and closers of `line`, in order.
    fn math_events(&self, line: u32, text: &str) -> Vec<Event> {
        let mut out = Vec::new();
        for token in self.tokens(line, text) {
            match token.kind {
                TokenKind::MathDelim => {
                    for unit in delimiter_units(text, token.start..token.end) {
                        self.classify(line, text, unit, &mut out);
                    }
                }
                TokenKind::Begin | TokenKind::End => {
                    if let Some((range, _)) = env_command(text, token.start) {
                        self.classify(line, text, range, &mut out);
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn classify(&self, line: u32, text: &str, range: Range<u32>, out: &mut Vec<Event>) {
        let before = self.mode(line, text, range.start);
        let after = self.mode(line, text, range.end);
        let (open, outside, inside) = match (is_math(&before), is_math(&after)) {
            (false, true) => (true, before, after),
            (true, false) => (false, after, before),
            _ => return,
        };
        out.push(Event {
            line,
            range,
            open,
            outside,
            inside,
        });
    }

    /// True when the scanner state at the start of `line` is inside math (or text in math).
    fn starts_inside(&self, line: u32, text: &str) -> bool {
        matches!(
            self.states.context_at(line, text, 0),
            Context::Math { .. } | Context::TextInMath | Context::Arg { .. }
        )
    }

    fn math(&self, at: Point) -> Option<Found> {
        let text = self.lines.line(at.line);
        let col = at.col.min(text.len() as u32);
        let events = self.math_events(at.line, &text);
        // On a delimiter: the point counts as inside the formula it opens or closes.
        let mut col = col;
        if let Some(e) = events
            .iter()
            .find(|e| e.range.start <= col && col < e.range.end)
        {
            col = if e.open { e.range.end } else { e.range.start };
        }
        let here = self.mode(at.line, &text, col);
        if !(is_math(&here) || here == Context::TextInMath) {
            return None;
        }
        let opener = self.opener(at.line, col, &events)?;
        let closer = self.closer(&opener)?;
        let start = Point::new(opener.line, opener.range.start);
        let end = Point::new(closer.line, closer.range.end);
        if end <= Point::new(at.line, col.min(at.col)) {
            return None;
        }
        let (kind, env) = match &opener.inside {
            Context::Math { display, env } => (
                if *display {
                    FormulaKind::Display
                } else {
                    FormulaKind::Inline
                },
                env.as_ref().map(|e| e.to_string()),
            ),
            _ => (FormulaKind::Inline, None),
        };
        Some((start, end, kind, env))
    }

    /// The outermost opener before `(line, col)`.
    fn opener(&self, line: u32, col: u32, first: &[Event]) -> Option<Event> {
        let mut depth = 0u32;
        let mut found: Option<Event> = None;
        let lowest = line.saturating_sub(MAX_LINES);
        let mut l = line;
        loop {
            let events = if l == line {
                first
                    .iter()
                    .filter(|e| e.range.end <= col)
                    .cloned()
                    .collect()
            } else {
                let text = self.lines.line(l);
                self.math_events(l, &text)
            };
            for e in events.into_iter().rev() {
                if !e.open {
                    depth += 1;
                } else if depth > 0 {
                    depth -= 1;
                } else {
                    let nested = e.outside == Context::TextInMath;
                    found = Some(e);
                    if !nested {
                        return found;
                    }
                }
            }
            if l == 0 || l <= lowest {
                return found;
            }
            let text = self.lines.line(l);
            if !self.starts_inside(l, &text) {
                return found;
            }
            l -= 1;
        }
    }

    /// The closer that matches `opener`.
    fn closer(&self, opener: &Event) -> Option<Event> {
        let mut depth = 0u32;
        let last = opener
            .line
            .saturating_add(MAX_LINES)
            .min(self.lines.line_count().saturating_sub(1));
        for l in opener.line..=last {
            let text = self.lines.line(l);
            if l > opener.line && !self.starts_inside(l, &text) {
                return None;
            }
            let events = self.math_events(l, &text);
            for e in events {
                if l == opener.line && e.range.start < opener.range.end {
                    continue;
                }
                if e.open {
                    depth += 1;
                } else if depth > 0 {
                    depth -= 1;
                } else {
                    return Some(e);
                }
            }
        }
        None
    }

    /// The innermost environment of `envs` around `at`.
    fn environment(&self, at: Point, envs: &[String]) -> Option<Found> {
        if envs.is_empty() {
            return None;
        }
        let wanted = |name: &str| envs.iter().any(|e| e == name);
        let mut depth: Vec<(String, u32)> = Vec::new();
        let lowest = at.line.saturating_sub(MAX_LINES);
        let mut opener: Option<(u32, Range<u32>, String)> = None;
        'lines: for l in (lowest..=at.line).rev() {
            let text = self.lines.line(l);
            if !text.contains("\\begin") && !text.contains("\\end") {
                continue;
            }
            let mut events = self.env_commands(l, &text);
            if l == at.line {
                events.retain(|(_, range, begin, _)| {
                    if *begin {
                        range.start <= at.col
                    } else {
                        range.end <= at.col
                    }
                });
            }
            for (_, range, begin, name) in events.into_iter().rev() {
                if !wanted(&name) {
                    continue;
                }
                let slot = match depth.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, d)) => d,
                    None => {
                        depth.push((name.clone(), 0));
                        &mut depth.last_mut().expect("just pushed").1
                    }
                };
                if !begin {
                    *slot += 1;
                } else if *slot > 0 {
                    *slot -= 1;
                } else {
                    opener = Some((l, range, name));
                    break 'lines;
                }
            }
        }
        let (line, range, name) = opener?;
        let mut nested = 0u32;
        let last = line
            .saturating_add(MAX_LINES)
            .min(self.lines.line_count().saturating_sub(1));
        for l in line..=last {
            let text = self.lines.line(l);
            if !text.contains("\\begin") && !text.contains("\\end") {
                continue;
            }
            for (_, r, begin, n) in self.env_commands(l, &text) {
                if n != name || (l == line && r.start < range.end) {
                    continue;
                }
                if begin {
                    nested += 1;
                } else if nested > 0 {
                    nested -= 1;
                } else {
                    let start = Point::new(line, range.start);
                    let end = Point::new(l, r.end);
                    if end <= at {
                        return None;
                    }
                    return Some((start, end, FormulaKind::Environment(name), None));
                }
            }
        }
        None
    }

    /// `\begin{…}` and `\end{…}` of `line` (outside comments and verbatim): line, range, begin?,
    /// name.
    fn env_commands(&self, line: u32, text: &str) -> Vec<(u32, Range<u32>, bool, String)> {
        self.tokens(line, text)
            .iter()
            .filter(|t| matches!(t.kind, TokenKind::Begin | TokenKind::End))
            .filter_map(|t| {
                let (range, name) = env_command(text, t.start)?;
                Some((line, range, t.kind == TokenKind::Begin, name))
            })
            .collect()
    }

    /// The source between two points, or `None` when it is too long.
    fn text(&self, start: Point, end: Point) -> Option<String> {
        if end.line - start.line > MAX_LINES {
            return None;
        }
        let mut out = String::new();
        for l in start.line..=end.line {
            let line = self.lines.line(l);
            let from = if l == start.line {
                start.col as usize
            } else {
                0
            };
            let to = if l == end.line {
                end.col as usize
            } else {
                line.len()
            };
            let piece = line.get(from.min(to)..to.min(line.len()))?;
            out.push_str(if l == end.line {
                piece
            } else {
                piece.strip_suffix('\r').unwrap_or(piece)
            });
            if l != end.line {
                out.push('\n');
            }
            if out.len() > MAX_BYTES {
                return None;
            }
        }
        Some(out)
    }
}

/// The single delimiters of a math-delimiter token (neighbouring delimiters share one token):
/// `\(`, `\)`, `\[`, `\]`, `$$` and `$`.
fn delimiter_units(text: &str, range: Range<u32>) -> Vec<Range<u32>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = range.start as usize;
    let end = (range.end as usize).min(bytes.len());
    while i < end {
        let len = match bytes[i] {
            b'\\' => 2,
            b'$' if bytes.get(i + 1) == Some(&b'$') && i + 1 < end => 2,
            _ => 1,
        };
        let stop = (i + len).min(end);
        out.push(i as u32..stop as u32);
        i = stop;
    }
    out
}

/// `\begin{name}` or `\end{name}` starting at byte `start` of `text`: its range (up to the
/// closing brace) and the name.
fn env_command(text: &str, start: u32) -> Option<(Range<u32>, String)> {
    let bytes = text.as_bytes();
    let mut i = start as usize + 1;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    let name_start = i + 1;
    let close = name_start + text[name_start..].find('}')?;
    let name = text[name_start..close].trim();
    if name.is_empty() {
        return None;
    }
    Some((start..close as u32 + 1, name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_core::LineIndex;
    use lemma_syntax::{ScanConfig, StrLines};
    use std::sync::Arc;

    struct Doc {
        text: String,
        index: LineIndex,
        states: LineStates,
    }

    impl Doc {
        fn new(text: &str) -> Self {
            let index = LineIndex::new(text);
            let mut states = LineStates::new(Arc::new(ScanConfig::default()));
            states.rebuild(&StrLines {
                text,
                index: &index,
            });
            Self {
                text: text.to_owned(),
                index,
                states,
            }
        }

        fn lines(&self) -> StrLines<'_> {
            StrLines {
                text: &self.text,
                index: &self.index,
            }
        }

        /// The formula at the first `needle` (plus `delta` bytes).
        fn at(&self, needle: &str, delta: usize) -> Option<Formula> {
            let offset = self.text.find(needle).expect("needle") + delta;
            self.at_offset(offset)
        }

        fn at_offset(&self, offset: usize) -> Option<Formula> {
            let (line, col) = self.index.position(offset);
            find_formula(
                &self.states,
                &self.lines(),
                Point::new(line, col),
                &["tikzcd".to_owned()],
            )
        }
    }

    fn text_of(f: Option<Formula>) -> Option<String> {
        f.map(|f| f.text)
    }

    #[test]
    fn inline_math() {
        let d = Doc::new("Let $X$ be a scheme and \\(\\cO_X\\) its sheaf.\n");
        let f = d.at("X$", 0).unwrap();
        assert_eq!(f.text, "$X$");
        assert_eq!(f.kind, FormulaKind::Inline);
        assert_eq!(f.start, Point::new(0, 4));
        assert_eq!(f.end, Point::new(0, 7));
        assert_eq!(text_of(d.at("cO_X", 0)), Some("\\(\\cO_X\\)".into()));
        assert_eq!(text_of(d.at("cO_X", 2)), Some("\\(\\cO_X\\)".into()));
        assert_eq!(d.at("be a", 0), None, "text");
        assert_eq!(d.at("Let", 0), None);
    }

    #[test]
    fn the_point_on_a_delimiter_counts() {
        let d = Doc::new("a $x+y$ b \\[ z \\] c\n");
        assert_eq!(text_of(d.at("$x", 0)), Some("$x+y$".into()), "opener");
        assert_eq!(text_of(d.at("$ b", 0)), Some("$x+y$".into()), "closer");
        assert_eq!(text_of(d.at("\\[", 0)), Some("\\[ z \\]".into()));
        assert_eq!(text_of(d.at("\\[", 1)), Some("\\[ z \\]".into()));
        assert_eq!(text_of(d.at("\\]", 1)), Some("\\[ z \\]".into()));
        assert_eq!(d.at(" c", 1), None);
    }

    #[test]
    fn display_math_over_lines() {
        let src = "Text\n\\[\n\\Hom(h_X,F)\\cong F(X),\n\\eta\\mapsto\\eta_X\n\\]\nafter $a$\n";
        let d = Doc::new(src);
        let f = d.at("cong", 0).unwrap();
        assert_eq!(f.kind, FormulaKind::Display);
        assert_eq!(
            f.text,
            "\\[\n\\Hom(h_X,F)\\cong F(X),\n\\eta\\mapsto\\eta_X\n\\]"
        );
        assert_eq!(f.start, Point::new(1, 0));
        assert_eq!(f.end, Point::new(4, 2));
        assert_eq!(text_of(d.at("a$", 0)), Some("$a$".into()));
        assert_eq!(d.at("Text", 1), None);
        assert_eq!(d.at("after", 0), None);
    }

    #[test]
    fn math_environments() {
        let src = "x\n\\begin{align*}\n a &= b \\label{eq:1}\\\\\n c &\\le \\frac{1}{2}\n\
\\end{align*}\ny\n";
        let d = Doc::new(src);
        let f = d.at("le", 1).unwrap();
        assert_eq!(f.kind, FormulaKind::Display);
        assert_eq!(f.env.as_deref(), Some("align*"));
        assert!(f.text.starts_with("\\begin{align*}\n a &= b"));
        assert!(f.text.ends_with("\\end{align*}"));
        assert_eq!(d.at("begin{align", 3), Some(f.clone()), "on \\begin");
        assert_eq!(d.at("end{align", 2), Some(f.clone()), "on \\end");
        assert_eq!(d.at("eq:1", 1), Some(f.clone()), "in a \\label");
        assert_eq!(d.at("frac", 0), Some(f), "on a command");
        let d = Doc::new("\\begin{math}a+b\\end{math}\n");
        assert_eq!(d.at("a+b", 1).unwrap().kind, FormulaKind::Inline);
    }

    #[test]
    fn nested_text_belongs_to_the_outer_formula() {
        let src =
            "\\[ f(x) = \\begin{cases} 1 & \\text{if $x>0$, and $y$} \\\\ 0 \\end{cases} \\]\n";
        let d = Doc::new(src);
        let whole = src.trim_end().to_owned();
        assert_eq!(
            text_of(d.at("x>0", 0)),
            Some(whole.clone()),
            "inner formula"
        );
        assert_eq!(text_of(d.at("y$", 0)), Some(whole.clone()));
        assert_eq!(text_of(d.at("if", 0)), Some(whole.clone()), "text in math");
        assert_eq!(text_of(d.at("0 \\end", 0)), Some(whole.clone()), "after it");
        assert_eq!(text_of(d.at("f(x)", 0)), Some(whole), "before it");
    }

    #[test]
    fn comments_and_verbatim_are_not_math() {
        let src = "% $a$ in a comment\n\\verb|$b$| and $c$\n\\begin{verbatim}\n$d$\n\
\\end{verbatim}\n";
        let d = Doc::new(src);
        assert_eq!(d.at("a$", 0), None);
        assert_eq!(d.at("b$", 0), None);
        assert_eq!(text_of(d.at("c$", 0)), Some("$c$".into()));
        assert_eq!(d.at("d$", 0), None);
        // A commented-out closer is skipped.
        let d = Doc::new("\\[ a % \\]\n b \\]\n");
        assert_eq!(text_of(d.at("b", 0)), Some("\\[ a % \\]\n b \\]".into()));
    }

    #[test]
    fn unterminated_inline_math_is_not_a_formula() {
        let d = Doc::new("an open $x + y\n\nnext paragraph $z$\n");
        assert_eq!(d.at("x +", 0), None);
        assert_eq!(text_of(d.at("z$", 0)), Some("$z$".into()));
        let d = Doc::new("\\begin{align}\n a = b\n");
        assert_eq!(d.at("a =", 0), None, "no \\end");
    }

    #[test]
    fn tikzcd_environments() {
        let src = "\\[\n\\begin{tikzcd}\nA \\arrow[r, \"f\"] & B\n\\end{tikzcd}\n\\]\n\
Outside:\n\\begin{tikzcd}\nC \\arrow[r, \"$g$\"] & D\n\\end{tikzcd}\n";
        let d = Doc::new(src);
        // Inside math: the display around the diagram.
        let f = d.at("A \\arrow", 0).unwrap();
        assert_eq!(f.kind, FormulaKind::Display);
        assert!(f.text.starts_with("\\[\n\\begin{tikzcd}") && f.text.ends_with("\\]"));
        // Outside math: the environment, also from inline math in a label.
        let f = d.at("C \\arrow", 0).unwrap();
        assert_eq!(f.kind, FormulaKind::Environment("tikzcd".into()));
        assert_eq!(
            f.text,
            "\\begin{tikzcd}\nC \\arrow[r, \"$g$\"] & D\n\\end{tikzcd}"
        );
        assert_eq!(d.at("g$", 0), Some(f.clone()));
        assert_eq!(d.at("Outside", 0), None);
        let last = src.rfind("\\end{tikzcd}").unwrap();
        assert_eq!(d.at_offset(last + 3), Some(f.clone()), "on \\end");
        assert_eq!(d.at_offset(last + 12), None, "after it");
        let begin = src.rfind("\\begin{tikzcd}").unwrap();
        assert_eq!(d.at_offset(begin), Some(f));
        // Not configured: nothing.
        let (line, col) = d.index.position(src.find("C \\arrow").unwrap());
        assert_eq!(
            find_formula(&d.states, &d.lines(), Point::new(line, col), &[]),
            None
        );
    }

    #[test]
    fn neighbouring_delimiters_are_split() {
        let d = Doc::new("\\(a\\)\\(b\\) and $$c$$\n");
        assert_eq!(text_of(d.at("a", 0)), Some("\\(a\\)".into()));
        assert_eq!(text_of(d.at("b", 0)), Some("\\(b\\)".into()));
        let f = d.at("c$", 0).unwrap();
        assert_eq!(f.text, "$$c$$");
        assert_eq!(f.kind, FormulaKind::Display);
    }

    #[test]
    fn crlf_lines_lose_their_carriage_returns() {
        let d = Doc::new("\\[\r\na+b\r\n\\]\r\n");
        assert_eq!(text_of(d.at("a+b", 0)), Some("\\[\na+b\n\\]".into()));
    }

    #[test]
    fn the_fixture_formulas() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/proj/main.tex"
        ))
        .unwrap();
        let d = Doc::new(&src);
        let f = d.at("\\int_{U\\in\\cC}", 2).unwrap();
        assert_eq!(f.env.as_deref(), Some("align"));
        assert!(f.text.starts_with("\\begin{align}\n\\Hom_{\\Sh(\\cC)}"));
        let f = d.at("p_{13}^*", 0).unwrap();
        assert_eq!(f.env.as_deref(), Some("equation"));
        assert!(f.text.contains("\\text{on } S'\\times_S"));
        assert_eq!(text_of(d.at("\\cO_X", 1)), Some("$\\cO_X$".into()));
        let f = d.at("shift left", 0).unwrap();
        assert_eq!(f.kind, FormulaKind::Display);
        assert!(f.text.contains("\\begin{tikzcd}"));
        assert_eq!(d.at("Lorem", 0), None);
    }

    #[test]
    fn long_formulas_are_cut_off() {
        let mut src = String::from("\\begin{align}\n");
        for _ in 0..(MAX_LINES + 10) {
            src.push_str("a \\\\\n");
        }
        src.push_str("\\end{align}\n");
        let d = Doc::new(&src);
        assert_eq!(d.at("a \\\\", 0), None);
        let big = format!("${}$\n", "x".repeat(MAX_BYTES + 1));
        let d = Doc::new(&big);
        assert_eq!(d.at("x", 0), None);
    }

    #[test]
    fn delimiter_units_split_runs() {
        assert_eq!(delimiter_units("\\)\\(", 0..4), vec![0..2, 2..4]);
        assert_eq!(delimiter_units("$$", 0..2), vec![0..2]);
        assert_eq!(delimiter_units("$", 0..1), vec![0..1]);
        assert_eq!(delimiter_units("x\\]$", 1..4), vec![1..3, 3..4]);
    }

    #[test]
    fn env_commands_parse() {
        assert_eq!(
            env_command("\\begin{align*} x", 0),
            Some((0..14, "align*".into()))
        );
        assert_eq!(
            env_command("a \\end {tikzcd}", 2),
            Some((2..15, "tikzcd".into()))
        );
        assert_eq!(env_command("\\begin x", 0), None);
        assert_eq!(env_command("\\begin{x", 0), None);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Any point of a random mix of formulas and text: a formula found contains the point,
        /// starts and ends at a delimiter, and is the same from every point inside it.
        #[test]
        fn found_formulas_are_consistent(parts in proptest::collection::vec(0usize..8, 1..16)) {
            let pieces = [
                "text ", "$a+b$ ", "\\(c\\) ", "\\[\nd\n\\] ", "\\begin{align*}\ne&=f\n\\end{align*}\n",
                "\\text{t $g$} ", "% $h$\n", "\n",
            ];
            let mut src = String::new();
            for p in &parts {
                // `\text` only inside math.
                if *p == 5 {
                    src.push_str("\\[ x ");
                    src.push_str(pieces[5]);
                    src.push_str("\\] ");
                } else {
                    src.push_str(pieces[*p]);
                }
            }
            let d = Doc::new(&src);
            for offset in 0..src.len() {
                if !src.is_char_boundary(offset) {
                    continue;
                }
                let Some(f) = d.at_offset(offset) else { continue };
                let (line, col) = d.index.position(offset);
                proptest::prop_assert!(f.contains(Point::new(line, col)), "{f:?} at {offset}");
                let start = d.index.offset(f.start.line, f.start.col);
                let end = d.index.offset(f.end.line, f.end.col);
                proptest::prop_assert_eq!(&src[start..end], f.text.as_str());
                proptest::prop_assert!(
                    ["$", "\\(", "\\[", "\\begin"].iter().any(|d| f.text.starts_with(d)),
                    "{:?}", f.text
                );
                proptest::prop_assert!(
                    ["$", "\\)", "\\]", "}"].iter().any(|d| f.text.ends_with(d)),
                    "{:?}", f.text
                );
                for inner in start..end {
                    if src.is_char_boundary(inner) {
                        proptest::prop_assert_eq!(d.at_offset(inner), Some(f.clone()));
                    }
                }
            }
        }
    }
}
