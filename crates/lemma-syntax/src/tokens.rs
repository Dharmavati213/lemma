// Owner: WP1.7 scanner
//! Token kinds produced per line for highlighting and spell-check exclusion.

/// What a span of a line is, for highlighting and the `no-spell-check` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Ordinary prose; the only kind that is spell-checked.
    Text,
    /// A control sequence in text, such as `\emph` or `\\`.
    Command,
    /// A sectioning command (`\section`, `\chapter*`, KOMA `\addsec`, …); the title is `Text`.
    Sectioning,
    /// `\begin`.
    Begin,
    /// `\end`.
    End,
    /// The environment name of `\begin{…}`/`\end{…}`.
    EnvName,
    /// A brace in text, or a brace delimiting a command argument.
    Brace,
    /// A bracket or parenthesis delimiting an optional argument.
    Bracket,
    /// A special character in text (`~ & # ^ _`) or an escaped one (`\$`, `\%`, `\&`, …).
    Special,
    /// A math delimiter: `$`, `$$`, `\(`, `\)`, `\[`, `\]`.
    MathDelim,
    /// Math content.
    Math,
    /// A control sequence inside math.
    MathCommand,
    /// A `%` comment, or the body of a `comment` environment.
    Comment,
    /// Verbatim text: verbatim environments, `\verb|…|`, `\lstinline`, `\mintinline`, URLs.
    Verbatim,
    /// The argument of `\label`.
    LabelArg,
    /// The argument of `\ref` and its relatives.
    RefArg,
    /// Citation keys.
    CiteArg,
    /// File names (`\input`, `\includegraphics`, `\bibliography`, …).
    FileArg,
    /// Package and class names (`\usepackage`, `\documentclass`, …).
    PackageArg,
    /// Key-value options and identifiers (`\usepackage[…]`, `\hypersetup{…}`, counter names, …).
    KeyArg,
}

impl TokenKind {
    /// True for kinds that carry the `gtksourceview:context-classes:no-spell-check` tag:
    /// everything except [`TokenKind::Text`].
    pub fn no_spell(self) -> bool {
        self != TokenKind::Text
    }
}

/// A span of one line; `start..end` are byte offsets within the line.
///
/// The tokens of a line tile it: they are sorted, adjacent and cover every byte, and two
/// neighbouring tokens never have the same kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub start: u32,
    pub end: u32,
    pub kind: TokenKind,
}

/// Receives the spans the lexer classifies.
pub(crate) trait Sink {
    /// False when tokens are discarded; the lexer then skips ordinary text faster.
    const WANTS_TOKENS: bool;

    /// Classifies `start..end` (non-empty, in order, adjacent to the previous span).
    fn token(&mut self, start: usize, end: usize, kind: TokenKind);
}

/// A sink for state-only scans.
pub(crate) struct Discard;

impl Sink for Discard {
    const WANTS_TOKENS: bool = false;

    #[inline]
    fn token(&mut self, _start: usize, _end: usize, _kind: TokenKind) {}
}

/// Appends tokens to a vector, merging neighbours of the same kind.
pub(crate) struct Collect<'a> {
    out: &'a mut Vec<Token>,
    /// Tokens before this index belong to the caller and are never merged into.
    first: usize,
}

impl<'a> Collect<'a> {
    pub(crate) fn new(out: &'a mut Vec<Token>) -> Self {
        let first = out.len();
        Self { out, first }
    }
}

impl Sink for Collect<'_> {
    const WANTS_TOKENS: bool = true;

    fn token(&mut self, start: usize, end: usize, kind: TokenKind) {
        // Lines longer than 4 GiB are not supported; offsets are u32 by contract.
        let (start, end) = (start as u32, end as u32);
        if self.out.len() > self.first
            && let Some(last) = self.out.last_mut()
            && last.kind == kind
            && last.end == start
        {
            last.end = end;
            return;
        }
        self.out.push(Token { start, end, kind });
    }
}
