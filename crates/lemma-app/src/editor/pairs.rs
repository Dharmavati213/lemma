//! Delimiter pairs: the partner of the delimiter at the cursor is
//! highlighted, and `%` (Vim normal mode) or Ctrl+% (`win.jump-pair`) jumps to it.
//!
//! # What pairs
//!
//! | Kind ([`PairKind`]) | Delimiters |
//! |---|---|
//! | `Env` | `\begin{x}` ↔ `\end{x}` (the same name) |
//! | `Brace`, `Bracket`, `Paren` | `{` ↔ `}`, `[` ↔ `]`, `(` ↔ `)` |
//! | `EscapedBrace` | `\{` ↔ `\}` |
//! | `Named` | `\langle` ↔ `\rangle`, `\lvert` ↔ `\rvert`, … ([`NAMED`]) |
//! | `LeftRight` | `\left` ↔ `\right`, whatever their delimiters (`\left.` … `\right|` too) |
//! | `Sized` | `\bigl` ↔ `\bigr` in every size (`\Bigl`, `\biggl`, `\Biggl`, sizes may differ), and `\big(` ↔ `\big)` (the direction from the delimiter) |
//! | `Dollar`, `DoubleDollar`, `MathParen`, `MathBracket` | `$` ↔ `$`, `$$` ↔ `$$`, `\(` ↔ `\)`, `\[` ↔ `\]` |
//!
//! A delimiter is one token: `\begin{x}` with its name, `\left(` and `\bigl\langle` with their
//! delimiter (whose `(` is then not a `Paren` of its own), `\{`, `$$`. Delimiters inside comments
//! and verbatim text (the scanner's `Comment` and `Verbatim` tokens: `%` comments, the `comment`
//! and verbatim environments, `\verb|…|`, `\url{…}`) do not count, nor do escaped characters
//! (`\$`, `\%`, the `{` of `\{`) and the `[` after `\\`. A `$` counts only where the scanner sees a
//! math delimiter, and the scanner's context before it says whether it opens or closes
//! (`$a \text{$b$} c$` pairs as TeX reads it; `$$` after an inline formula is a `$` closing it
//! and a `$` opening the next).
//!
//! A delimiter pairs with the nearest one of its kind beyond it that balances the ones of that
//! kind in between (for environments: of the same name, as VimTeX's `searchpair()`; a mismatched
//! `\end{y}` does not close a `\begin{x}`). Searches stop at a blank line for everything but
//! braces and environments — TeX's `\par` ends inline and display math, and brackets, `\left`
//! and sized delimiters never span paragraphs in practice — and after [`MAX_SCAN_LINES`] lines
//! for braces. A search that ends at a blank line or at the end of the document finds the
//! delimiter **unmatched**; one that gives up after [`MAX_SCAN_LINES`] knows nothing
//! ([`Partner::Unknown`]: no highlight, and `%` is Vim's own).
//!
//! # Highlight
//!
//! The delimiter at the cursor (in Vim normal and visual mode the character under the block
//! cursor; otherwise the character after the cursor, else the one before it) and its partner get
//! [`MATCH_TAG`]: the style scheme's `bracket-match` style when it has a background, else a tint
//! of the accent colour with the scheme's `bracket-match` boldness (Adwaita's is bold only). An
//! unmatched delimiter gets [`UNMATCHED_TAG`], a subtle red tint. GtkSourceView's own bracket
//! matching (which does not know comments, escapes or `\left`) is switched off in LaTeX buffers
//! while this module is attached. Non-LaTeX documents (`.bib`, Markdown) keep GtkSourceView's.
//!
//! The highlight follows the cursor, the text and the Vim mode from a high-priority idle (before
//! the frame is drawn), and is only re-applied when it changes.
//!
//! # Jump
//!
//! - **`%` in Vim normal mode** (a key-layer handler at [`PRIORITY_PERCENT_KEY`], below the Vim
//!   bindings): with nothing pending, it jumps to the partner of the delimiter under the cursor,
//!   or else of the next delimiter on the line (as Vim's `%` looks forward); an unmatched one
//!   rings the bell. Without a delimiter on the rest of the line (or when the search gave up), and
//!   with a count (`50%`), an operator (`d%`, `y%`), a register or any other pending command, the
//!   key is Vim's own. Visual mode keeps Vim's `%`.
//! - **Ctrl+%** (`win.jump-pair`, [`jump`]) does the same everywhere (in Vim visual mode it
//!   extends the selection).
//!
//! The cursor lands on the first character of the partner (the backslash of `\end{x}`), so a
//! second jump comes back; the view scrolls only as far as needed. Jumps do not enter the
//! back-jump list (Ctrl+O): `%` again returns.
//!
//! # Cost
//!
//! Nothing runs until an editor shows a LaTeX document. Per cursor move: the scanner tokens of
//! the cursor's line, then for a partner:
//! - groups and math delimiters: the lines up to the partner, read in windows of the buffer and
//!   tokenized only when they hold a delimiter of the kind sought;
//! - environments: an index of the `\begin`/`\end` delimiters of every line ([`EnvCache`]), built
//!   on the first query (from one copy of the text) and kept in step with every change (changed
//!   lines are read again when the next query needs them; the lines the scanner re-tags, such as
//!   those after a new `\begin{verbatim}`, too), then a walk over the delimiters of that name.
//!
//! `perf_pairs_cursor_moves_large_tex` (`tests/gtk_pairs.rs`) holds this to ≤ 0.5 ms per cursor
//! move on `large.tex` (measured: p50 17 µs, p99 0.1 ms; an unmatched brace, the longest search,
//! 0.3 ms; building the environment cache of its 60k lines once, 7–10 ms). The structure index's
//! `EnvPair`s (the project index) are not read here: the index is rebuilt 300 ms after typing
//! stops and knows lines only, so it lags the text being edited; the cache pairs environments
//! by the same rule on the live text, and the tests check that every `EnvPair` of
//! `lemma_syntax::index_file` on the fixtures (`large.tex` included) pairs the same way.
//!
//! Hooks: [`attach`] / [`detach`] per editor tab; tests and other modules use [`pair_at`],
//! [`highlighted`], [`refresh_now`] and [`jump_in`].

use super::{Editor, TexView, VimMode, keys};
use crate::document::{Document, ObserverId};
use crate::window::WindowState;
use adw::prelude::*;
use glib::translate::IntoGlib;
use gtk::gdk;
use lemma_core::LineIndex;
use lemma_syntax::{Context, LineStates, Token, TokenKind};
use sourceview5::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::time::Duration;

/// The tag on the delimiter at the cursor and its partner.
pub const MATCH_TAG: &str = "lemma-pair-match";
/// The tag on a delimiter at the cursor that has no partner.
pub const UNMATCHED_TAG: &str = "lemma-pair-unmatched";
/// Priority of the `%` handler in the key layer: below the Vim bindings
/// (`keys::PRIORITY_VIM_BINDINGS`), so a binding (or a held prefix) gets the key first.
pub const PRIORITY_PERCENT_KEY: i32 = 60;
/// How many lines a search for the partner of a brace reads in each direction before it gives
/// up (the other kinds stop at a blank line, environments never give up).
pub const MAX_SCAN_LINES: u32 = 400;
/// After a change of more lines than this, the environment cache is rebuilt from the whole text
/// instead of reading the changed lines one by one.
const ENV_REFILL_AT: usize = 256;
/// Lines read from the buffer at once by a search (doubling each time).
const FIRST_WINDOW: u32 = 32;

/// The named delimiter pairs (control words without the backslash), [`PairKind::Named`]'s index.
pub const NAMED: &[(&str, &str)] = &[
    ("langle", "rangle"),
    ("lvert", "rvert"),
    ("lVert", "rVert"),
    ("lfloor", "rfloor"),
    ("lceil", "rceil"),
    ("lbrace", "rbrace"),
    ("lbrack", "rbrack"),
    ("lgroup", "rgroup"),
    ("lmoustache", "rmoustache"),
    ("llbracket", "rrbracket"),
];

/// The sizes of `\big` and friends, longest first (so `\Biggl` is `Bigg` + `l`).
const SIZES: &[&str] = &["Bigg", "bigg", "Big", "big"];

// ───────────── Delimiters ─────────────

/// What a delimiter pairs with: only delimiters of the same kind pair (environments also need
/// the same name). See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PairKind {
    /// `\begin{x}` ↔ `\end{x}`.
    Env,
    /// `{` ↔ `}`.
    Brace,
    /// `[` ↔ `]`.
    Bracket,
    /// `(` ↔ `)`.
    Paren,
    /// `\{` ↔ `\}`.
    EscapedBrace,
    /// A pair of [`NAMED`] (its index): `\langle` ↔ `\rangle`, …
    Named(u8),
    /// `\left` ↔ `\right`.
    LeftRight,
    /// `\bigl` ↔ `\bigr` (every size), `\big(` ↔ `\big)`.
    Sized,
    /// `$` ↔ `$`.
    Dollar,
    /// `$$` ↔ `$$`.
    DoubleDollar,
    /// `\(` ↔ `\)`.
    MathParen,
    /// `\[` ↔ `\]`.
    MathBracket,
}

impl PairKind {
    /// Whether a blank line ends the search for a partner (see the module docs).
    fn stops_at_paragraph(self) -> bool {
        !matches!(self, PairKind::Env | PairKind::Brace)
    }

    fn is_dollar(self) -> bool {
        matches!(self, PairKind::Dollar | PairKind::DoubleDollar)
    }

    /// True when a line without any of these bytes cannot hold a delimiter of this kind.
    fn may_be_on(self, line: &str) -> bool {
        let has = |bytes: &[u8]| line.bytes().any(|b| bytes.contains(&b));
        match self {
            PairKind::Brace => has(b"{}"),
            PairKind::Bracket => has(b"[]"),
            PairKind::Paren => has(b"()"),
            PairKind::Dollar | PairKind::DoubleDollar => has(b"$"),
            PairKind::Env
            | PairKind::EscapedBrace
            | PairKind::Named(_)
            | PairKind::LeftRight
            | PairKind::Sized
            | PairKind::MathParen
            | PairKind::MathBracket => has(b"\\"),
        }
    }
}

/// A delimiter on one line; offsets are bytes of the line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LineDelim {
    start: usize,
    end: usize,
    kind: PairKind,
    open: bool,
    /// The environment's name (`Env` only; empty otherwise).
    name: Range<usize>,
}

/// Which delimiters [`lex_line`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    All,
    Kind(PairKind),
}

impl Want {
    fn takes(self, kind: PairKind) -> bool {
        match self {
            Want::All => true,
            Want::Kind(k) => k == kind,
        }
    }

    /// Whether `$` runs must be classified (which costs a context query per run).
    fn dollars(self) -> bool {
        match self {
            Want::All => true,
            Want::Kind(k) => k.is_dollar(),
        }
    }
}

fn is_letter(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'@'
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\x0c')
}

/// A line of blanks only: TeX's `\par`.
fn is_blank_line(line: &str) -> bool {
    line.bytes().all(is_blank)
}

/// Length of the UTF-8 character starting with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xff => 4,
        _ => 1,
    }
}

/// `name` as a sized delimiter command: its suffix (`"l"`, `"r"`, `"m"` or `""`).
fn sized_suffix(name: &str) -> Option<&str> {
    SIZES.iter().find_map(|size| {
        let rest = name.strip_prefix(size)?;
        matches!(rest, "" | "l" | "r" | "m").then_some(rest)
    })
}

/// `name` as a [`NAMED`] delimiter: (index, opening).
fn named(name: &str) -> Option<(u8, bool)> {
    NAMED.iter().enumerate().find_map(|(i, (open, close))| {
        let i = u8::try_from(i).ok()?;
        if name == *open {
            Some((i, true))
        } else if name == *close {
            Some((i, false))
        } else {
            None
        }
    })
}

/// Which way the delimiter after `\big` (as written, e.g. `(` or `\langle`) points.
fn direction(delimiter: &str) -> Option<bool> {
    match delimiter {
        "(" | "[" | "\\{" => Some(true),
        ")" | "]" | "\\}" => Some(false),
        _ => {
            let name = delimiter.strip_prefix('\\')?;
            named(name).map(|(_, open)| open)
        }
    }
}

/// The tokens of one line, as the scanner classified them (`None`: no scanner states; then
/// everything counts as text).
struct Kinds<'a>(Option<&'a [Token]>);

impl Kinds<'_> {
    /// The token holding byte `i`.
    fn at(&self, i: usize) -> Option<Token> {
        let tokens = self.0?;
        let k = tokens.partition_point(|t| t.end as usize <= i);
        tokens.get(k).copied().filter(|t| t.start as usize <= i)
    }

    fn kind(&self, i: usize) -> Option<TokenKind> {
        self.at(i).map(|t| t.kind)
    }
}

/// The delimiters of one line (see the module docs) that `want` asks for, in order.
/// `context(i)` is the scanner context before byte `i` of the line (asked only to classify `$`).
fn lex_line(
    text: &str,
    tokens: Option<&[Token]>,
    want: Want,
    context: &mut dyn FnMut(usize) -> Context,
    out: &mut Vec<LineDelim>,
) {
    let b = text.as_bytes();
    let kinds = Kinds(tokens);
    let push = |out: &mut Vec<LineDelim>, range: Range<usize>, kind: PairKind, open: bool| {
        if want.takes(kind) {
            out.push(LineDelim {
                start: range.start,
                end: range.end,
                kind,
                open,
                name: 0..0,
            });
        }
    };
    let mut i = 0;
    while i < b.len() {
        if let Some(t) = kinds.at(i)
            && matches!(t.kind, TokenKind::Comment | TokenKind::Verbatim)
        {
            i = t.end as usize;
            continue;
        }
        match b[i] {
            b'\\' => i = lex_backslash(text, &kinds, i, want, out),
            c @ (b'{' | b'}') => {
                push(out, i..i + 1, PairKind::Brace, c == b'{');
                i += 1;
            }
            c @ (b'[' | b']') => {
                push(out, i..i + 1, PairKind::Bracket, c == b'[');
                i += 1;
            }
            c @ (b'(' | b')') => {
                push(out, i..i + 1, PairKind::Paren, c == b'(');
                i += 1;
            }
            b'$' => {
                let delim =
                    |j: usize| kinds.0.is_some() && kinds.kind(j) == Some(TokenKind::MathDelim);
                let mut j = i;
                while j < b.len() && b[j] == b'$' && delim(j) {
                    j += 1;
                }
                if j == i {
                    // Not a math delimiter (inside `\(…\)` or an argument).
                    i += 1;
                    continue;
                }
                if want.dollars() {
                    classify_dollars(i..j, context, &mut |range, kind, open| {
                        push(out, range, kind, open);
                    });
                }
                i = j;
            }
            _ => i += 1,
        }
    }
}

/// Splits a run of `$` math delimiters (`$`, `$$`, `$$` closing one formula and opening the
/// next, …) by the scanner context before each.
fn classify_dollars(
    run: Range<usize>,
    context: &mut dyn FnMut(usize) -> Context,
    emit: &mut dyn FnMut(Range<usize>, PairKind, bool),
) {
    let mut p = run.start;
    while p < run.end {
        let two = p + 2 <= run.end;
        match context(p) {
            // Only `$$` closes display math with `$`s (a lone `$` there is not a delimiter).
            Context::Math { display: true, .. } => {
                let n = if two { 2 } else { 1 };
                if n == 2 {
                    emit(p..p + 2, PairKind::DoubleDollar, false);
                }
                p += n;
            }
            Context::Math { display: false, .. } => {
                emit(p..p + 1, PairKind::Dollar, false);
                p += 1;
            }
            _ if two => {
                emit(p..p + 2, PairKind::DoubleDollar, true);
                p += 2;
            }
            _ => {
                emit(p..p + 1, PairKind::Dollar, true);
                p += 1;
            }
        }
    }
}

/// Lexes the control sequence at byte `i` (a backslash); returns the byte after it.
fn lex_backslash(
    text: &str,
    kinds: &Kinds<'_>,
    i: usize,
    want: Want,
    out: &mut Vec<LineDelim>,
) -> usize {
    let b = text.as_bytes();
    let j = i + 1;
    let Some(&c) = b.get(j) else {
        return b.len();
    };
    let mut push = |range: Range<usize>, kind: PairKind, open: bool, name: Range<usize>| {
        if want.takes(kind) {
            out.push(LineDelim {
                start: range.start,
                end: range.end,
                kind,
                open,
                name,
            });
        }
    };
    if !is_letter(c) {
        return match c {
            b'{' | b'}' => {
                push(i..j + 1, PairKind::EscapedBrace, c == b'{', 0..0);
                j + 1
            }
            b'(' | b')' => {
                push(i..j + 1, PairKind::MathParen, c == b'(', 0..0);
                j + 1
            }
            b'[' | b']' => {
                push(i..j + 1, PairKind::MathBracket, c == b'[', 0..0);
                j + 1
            }
            _ => (j + utf8_len(c)).min(b.len()),
        };
    }
    let mut k = j + 1;
    while k < b.len() && is_letter(b[k]) {
        k += 1;
    }
    let name = &text[j..k];
    match name {
        "begin" | "end" => {
            // The scanner reads `\begin{name}` only with the whole name on the line.
            if kinds.0.is_some()
                && !matches!(kinds.kind(i), Some(TokenKind::Begin | TokenKind::End))
            {
                return k;
            }
            let mut p = k;
            while p < b.len() && is_blank(b[p]) {
                p += 1;
            }
            if b.get(p) != Some(&b'{') {
                return k;
            }
            let Some(len) = b[p + 1..].iter().position(|&x| x == b'}') else {
                return k;
            };
            let close = p + 1 + len;
            push(i..close + 1, PairKind::Env, name == "begin", p + 1..close);
            close + 1
        }
        "left" | "right" => {
            let e = delimiter_end(text, kinds, k);
            push(i..e, PairKind::LeftRight, name == "left", 0..0);
            e
        }
        "middle" => delimiter_end(text, kinds, k),
        _ => {
            if let Some(suffix) = sized_suffix(name) {
                let e = delimiter_end(text, kinds, k);
                let open = match suffix {
                    "l" => Some(true),
                    "r" => Some(false),
                    "m" => None,
                    _ => direction(text[k..e].trim_start()),
                };
                if let Some(open) = open {
                    push(i..e, PairKind::Sized, open, 0..0);
                }
                return e;
            }
            if let Some((index, open)) = named(name) {
                push(i..k, PairKind::Named(index), open, 0..0);
            }
            k
        }
    }
}

/// The end of the delimiter after `\left`, `\bigl`, … at byte `k` (blanks allowed before it):
/// one character (`(`, `.`, `|`) or a control sequence (`\{`, `\langle`, `\|`). `k` itself
/// when none follows on the line (a brace or a comment is not a delimiter).
fn delimiter_end(text: &str, kinds: &Kinds<'_>, k: usize) -> usize {
    let b = text.as_bytes();
    let mut p = k;
    while p < b.len() && is_blank(b[p]) {
        p += 1;
    }
    let Some(&c) = b.get(p) else {
        return k;
    };
    if matches!(c, b'{' | b'}' | b'%')
        || matches!(
            kinds.kind(p),
            Some(TokenKind::Comment | TokenKind::Verbatim)
        )
    {
        return k;
    }
    if c != b'\\' {
        return (p + utf8_len(c)).min(b.len());
    }
    let Some(&d) = b.get(p + 1) else {
        return k;
    };
    if !is_letter(d) {
        return (p + 1 + utf8_len(d)).min(b.len());
    }
    let mut e = p + 2;
    while e < b.len() && is_letter(b[e]) {
        e += 1;
    }
    e
}

// ───────────── Text access ─────────────

/// What the matcher reads: the text (in windows of whole lines), its line index and the scanner
/// states. An open document reads its buffer ([`DocSource`]); tests read a string.
trait Source {
    fn index(&self) -> &LineIndex;
    fn states(&self) -> &LineStates;
    /// The text of `lines` (in range, not empty), with their line breaks but the last one's
    /// (it is there unless the last line is the text's last).
    fn lines(&self, lines: Range<u32>) -> Option<Cow<'_, str>>;
    /// Runs `f` on the whole text.
    fn with_text(&self, f: &mut dyn FnMut(&str));
}

/// A run of whole lines of the text.
struct Window<'s> {
    lines: Range<u32>,
    base: usize,
    text: Cow<'s, str>,
}

impl Window<'_> {
    fn read<'s>(src: &'s dyn Source, lines: Range<u32>) -> Option<Window<'s>> {
        let index = src.index();
        let count = index.line_count();
        let lines = lines.start.min(count)..lines.end.min(count);
        if lines.is_empty() {
            return None;
        }
        let base = index.line_start(lines.start);
        let text = src.lines(lines.clone())?;
        Some(Window { lines, base, text })
    }

    /// Line `line` (in the window) without its `\n`.
    fn line(&self, index: &LineIndex, line: u32) -> Option<&str> {
        let range = index.line_range(line);
        self.text
            .get(range.start.checked_sub(self.base)?..range.end.checked_sub(self.base)?)
    }
}

/// The delimiters on line `line` (text `text`) that `want` asks for.
fn line_delims(src: &dyn Source, line: u32, text: &str, want: Want) -> Vec<LineDelim> {
    let states = src.states();
    let mut tokens = Vec::new();
    states.tokens(line, text, &mut tokens);
    let mut context = |byte: usize| states.context_at(line, text, byte as u32);
    let mut out = Vec::new();
    lex_line(text, Some(&tokens), want, &mut context, &mut out);
    out
}

// ───────────── Environment cache ─────────────

/// The id of the environment name `name`.
fn intern(names: &mut HashMap<Box<str>, u32>, name: &str) -> u32 {
    if let Some(&id) = names.get(name) {
        return id;
    }
    let id = u32::try_from(names.len()).unwrap_or(u32::MAX);
    names.insert(name.into(), id);
    id
}

/// One `\begin{x}` or `\end{x}` in [`EnvCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EnvTok {
    line: u32,
    /// Byte range in the line.
    start: u32,
    end: u32,
    /// The interned name.
    name: u32,
    open: bool,
}

/// The `\begin`/`\end` delimiters of every line of a document, kept in step with its changes
/// (see the module docs, "Cost").
#[derive(Default)]
struct EnvCache {
    built: bool,
    line_count: u32,
    /// Sorted by (line, start).
    toks: Vec<EnvTok>,
    /// Lines changed since they were read, sorted (at most [`ENV_REFILL_AT`]; more: rebuilt).
    unknown: Vec<u32>,
    names: HashMap<Box<str>, u32>,
    /// Inside a change (between the document's edit and lines-changed observers).
    in_change: bool,
    /// Lines the scanner re-tagged inside the change (post-change numbers), applied after the
    /// line shift.
    retagged: Option<Range<u32>>,
}

impl EnvCache {
    fn reset(&mut self) {
        self.built = false;
        self.toks = Vec::new();
        self.unknown = Vec::new();
        self.names.clear();
        self.retagged = None;
    }

    /// The tokens of line `line`, as an index range of `toks`.
    fn line_range(&self, line: u32) -> Range<usize> {
        let lo = self.toks.partition_point(|t| t.line < line);
        let hi = self.toks.partition_point(|t| t.line <= line);
        lo..hi
    }

    /// Lines `[first, first + removed)` became `[first, first + added)`.
    fn lines_changed(&mut self, first: u32, removed: u32, added: u32) {
        self.in_change = false;
        let retagged = self.retagged.take();
        if !self.built {
            return;
        }
        let Some(end) = first.checked_add(removed).filter(|&e| e <= self.line_count) else {
            self.reset();
            return;
        };
        self.line_count = self.line_count - removed + added;
        let lo = self.toks.partition_point(|t| t.line < first);
        let hi = self.toks.partition_point(|t| t.line < end);
        self.toks.drain(lo..hi);
        for t in &mut self.toks[lo..] {
            t.line = t.line - removed + added;
        }
        self.unknown.retain(|&l| l < first || l >= end);
        for l in &mut self.unknown {
            if *l >= end {
                *l = *l - removed + added;
            }
        }
        let at = self.unknown.partition_point(|&l| l < first);
        self.unknown.splice(at..at, first..first + added);
        if self.unknown.len() > ENV_REFILL_AT {
            self.reset();
            return;
        }
        if let Some(lines) = retagged {
            self.mark_unknown(lines);
        }
    }

    /// The scanner re-tagged `lines` (post-change numbers).
    fn retag(&mut self, lines: Range<u32>) {
        if !self.built {
            return;
        }
        if self.in_change {
            let merged = match self.retagged.take() {
                Some(r) => r.start.min(lines.start)..r.end.max(lines.end),
                None => lines,
            };
            self.retagged = Some(merged);
        } else {
            self.mark_unknown(lines);
        }
    }

    fn mark_unknown(&mut self, lines: Range<u32>) {
        let lines = lines.start.min(self.line_count)..lines.end.min(self.line_count);
        if lines.len() > ENV_REFILL_AT {
            self.reset();
            return;
        }
        let lo = self.toks.partition_point(|t| t.line < lines.start);
        let hi = self.toks.partition_point(|t| t.line < lines.end);
        self.toks.drain(lo..hi);
        for line in lines {
            if let Err(at) = self.unknown.binary_search(&line) {
                self.unknown.insert(at, line);
            }
        }
        if self.unknown.len() > ENV_REFILL_AT {
            self.reset();
        }
    }

    /// Makes the cache describe the text of `src`: built from the whole text on first use (and
    /// after large changes), else by reading the lines changed since.
    fn ensure(&mut self, src: &dyn Source) {
        let count = src.index().line_count();
        if self.built && self.line_count != count {
            self.reset();
        }
        if !self.built {
            self.fill(src);
            return;
        }
        if self.unknown.is_empty() {
            return;
        }
        for line in std::mem::take(&mut self.unknown) {
            let Some(window) = Window::read(src, line..line + 1) else {
                continue;
            };
            let Some(text) = window.line(src.index(), line) else {
                continue;
            };
            let found = Self::read_line(src, line, text);
            let toks: Vec<EnvTok> = found
                .into_iter()
                .map(|d| self.tok(line, text, &d))
                .collect();
            let at = self.line_range(line);
            self.toks.splice(at, toks);
        }
    }

    fn tok(&mut self, line: u32, text: &str, d: &LineDelim) -> EnvTok {
        EnvTok {
            line,
            start: d.start as u32,
            end: d.end as u32,
            name: intern(&mut self.names, &text[d.name.clone()]),
            open: d.open,
        }
    }

    /// The environment delimiters of line `line` (text `text`).
    fn read_line(src: &dyn Source, line: u32, text: &str) -> Vec<LineDelim> {
        if !(text.contains("\\begin") || text.contains("\\end")) {
            return Vec::new();
        }
        line_delims(src, line, text, Want::Kind(PairKind::Env))
    }

    /// Reads every line of `src` (one copy of the text; only lines with `\begin` or `\end` are
    /// tokenized).
    fn fill(&mut self, src: &dyn Source) {
        self.reset();
        let index = src.index();
        let count = index.line_count();
        let mut toks = Vec::new();
        let names = &mut self.names;
        src.with_text(&mut |text: &str| {
            let mut candidates: Vec<u32> = text
                .match_indices("\\begin")
                .chain(text.match_indices("\\end"))
                .map(|(at, _)| index.line_of(at))
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            for line in candidates {
                let Some(line_text) = text.get(index.line_range(line)) else {
                    continue;
                };
                for d in line_delims(src, line, line_text, Want::Kind(PairKind::Env)) {
                    toks.push(EnvTok {
                        line,
                        start: d.start as u32,
                        end: d.end as u32,
                        name: intern(names, &line_text[d.name.clone()]),
                        open: d.open,
                    });
                }
            }
        });
        self.toks = toks;
        self.line_count = count;
        self.built = true;
    }

    /// The partner of the token at (`line`, `start`): `Some(Some(tok))`, `Some(None)` when it
    /// has none, `None` when there is no such token.
    fn partner(&self, line: u32, start: u32) -> Option<Option<EnvTok>> {
        let range = self.line_range(line);
        let at = range.start + self.toks[range].iter().position(|t| t.start == start)?;
        let me = self.toks[at];
        let mut depth = 0u32;
        let mut step = |t: &EnvTok| -> Option<EnvTok> {
            if t.name != me.name {
                return None;
            }
            if t.open == me.open {
                depth += 1;
                None
            } else if depth == 0 {
                Some(*t)
            } else {
                depth -= 1;
                None
            }
        };
        let found = if me.open {
            self.toks[at + 1..].iter().find_map(&mut step)
        } else {
            self.toks[..at].iter().rev().find_map(&mut step)
        };
        Some(found)
    }
}

// ───────────── Matching ─────────────

/// Where the partner of a delimiter is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Partner {
    /// Its document byte range.
    Found(Range<usize>),
    /// It has none: the search reached a blank line (for the kinds that stop there) or the end
    /// of the document.
    Unmatched,
    /// The search gave up ([`MAX_SCAN_LINES`]) or could not read the text.
    Unknown,
}

/// A delimiter and its partner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub kind: PairKind,
    /// True for an opening delimiter (`\begin{x}`, `{`, `\left`, an opening `$`).
    pub open: bool,
    /// The delimiter's document byte range.
    pub token: Range<usize>,
    pub partner: Partner,
}

/// Which delimiter "at the cursor" means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The one holding the character at the offset (Vim's block cursor).
    On,
    /// The one holding the character at the offset, else the one before it (a bar cursor
    /// between two characters).
    OnOrBefore,
    /// [`Side::On`], else the first one after the offset on its line (Vim's `%`).
    OnOrAfter,
    /// [`Side::OnOrBefore`], else the first one after the offset on its line.
    Near,
}

/// The delimiter at `offset` (as `side` says) and its partner, from `src`.
fn find_pair(
    src: &dyn Source,
    envs: &RefCell<EnvCache>,
    offset: usize,
    side: Side,
) -> Option<Pair> {
    let index = src.index();
    if offset > index.text_len() {
        return None;
    }
    let (line, col) = index.position(offset);
    let col = col as usize;
    let window = Window::read(src, line..line + 1)?;
    let text = window.line(index, line)?;
    let delims = line_delims(src, line, text, Want::All);
    let on = |c: usize| delims.iter().position(|d| d.start <= c && c < d.end);
    let before = || col.checked_sub(1).and_then(on);
    let after = || delims.iter().position(|d| d.start >= col);
    let at = match side {
        Side::On => on(col),
        Side::OnOrBefore => on(col).or_else(before),
        Side::OnOrAfter => on(col).or_else(after),
        Side::Near => on(col).or_else(before).or_else(after),
    }?;
    let d = &delims[at];
    let base = index.line_start(line);
    let partner = if d.kind == PairKind::Env {
        let mut cache = envs.borrow_mut();
        cache.ensure(src);
        match cache.partner(line, d.start as u32) {
            Some(Some(t)) => {
                let start = index.line_start(t.line);
                Partner::Found(start + t.start as usize..start + t.end as usize)
            }
            Some(None) => Partner::Unmatched,
            None => Partner::Unknown,
        }
    } else {
        search(src, line, &delims, at)
    };
    Some(Pair {
        kind: d.kind,
        open: d.open,
        token: base + d.start..base + d.end,
        partner,
    })
}

/// Looks for the partner of `delims[at]` (the delimiters of line `line`) line by line.
fn search(src: &dyn Source, line: u32, delims: &[LineDelim], at: usize) -> Partner {
    let index = src.index();
    let me = &delims[at];
    let kind = me.kind;
    let mut depth = 0u32;
    // Steps over one delimiter of the kind; true when it is the partner.
    let mut step = |d: &LineDelim| -> bool {
        if d.kind != kind {
            return false;
        }
        if d.open == me.open {
            depth += 1;
            false
        } else if depth == 0 {
            true
        } else {
            depth -= 1;
            false
        }
    };
    let found = |line: u32, d: &LineDelim| {
        let start = index.line_start(line);
        Partner::Found(start + d.start..start + d.end)
    };
    // The cursor's line.
    if me.open {
        if let Some(d) = delims[at + 1..].iter().find(|d| step(d)) {
            return found(line, d);
        }
    } else if let Some(d) = delims[..at].iter().rev().find(|d| step(d)) {
        return found(line, d);
    }
    let count = index.line_count();
    let limit = MAX_SCAN_LINES;
    let mut scanned = 0u32;
    let mut size = FIRST_WINDOW;
    let mut next = line;
    loop {
        // The next window of lines in the search direction.
        let lines = if me.open {
            let first = next + 1;
            if first >= count {
                return Partner::Unmatched;
            }
            first..first.saturating_add(size).min(count)
        } else {
            if next == 0 {
                return Partner::Unmatched;
            }
            next.saturating_sub(size)..next
        };
        let Some(window) = Window::read(src, lines.clone()) else {
            return Partner::Unknown;
        };
        let mut visit = |l: u32| -> Option<Partner> {
            let text = window.line(index, l)?;
            if kind.stops_at_paragraph() && is_blank_line(text) {
                return Some(Partner::Unmatched);
            }
            if !kind.may_be_on(text) {
                return None;
            }
            let ds = line_delims(src, l, text, Want::Kind(kind));
            let hit = if me.open {
                ds.iter().find(|d| step(d))
            } else {
                ds.iter().rev().find(|d| step(d))
            };
            hit.map(|d| found(l, d))
        };
        let result = if me.open {
            window.lines.clone().find_map(&mut visit)
        } else {
            window.lines.clone().rev().find_map(&mut visit)
        };
        if let Some(result) = result {
            return result;
        }
        scanned += window.lines.len() as u32;
        if scanned >= limit && !kind.stops_at_paragraph() {
            return Partner::Unknown;
        }
        if scanned >= limit.saturating_mul(4) {
            // A paragraph of thousands of lines.
            return Partner::Unknown;
        }
        next = if me.open {
            window.lines.end - 1
        } else {
            window.lines.start
        };
        size = size.saturating_mul(2);
    }
}

// ───────────── Documents ─────────────

/// Whether GTK numbers `doc`'s lines as its line index does: not while the text holds a lone
/// `\r` or U+2029 (GTK breaks lines there). Then byte offsets and iters are converted through
/// one copy of the text per version ([`Texts`]) instead of the buffer's lines.
fn same_lines(doc: &Document, buffer: &sourceview5::Buffer) -> bool {
    u32::try_from(buffer.line_count()).is_ok_and(|n| n == doc.line_index().line_count())
}

/// The whole text of a document whose lines GTK numbers differently (see [`same_lines`]),
/// copied once per version.
#[derive(Default)]
struct Texts(RefCell<Option<(u64, Rc<str>)>>);

impl Texts {
    /// `None` while GTK's lines are the document's (nothing to convert through).
    fn get(&self, doc: &Document, buffer: &sourceview5::Buffer) -> Option<Rc<str>> {
        if same_lines(doc, buffer) {
            self.0.replace(None);
            return None;
        }
        let version = doc.version();
        if let Some((v, text)) = &*self.0.borrow()
            && *v == version
        {
            return Some(text.clone());
        }
        let text: Rc<str> = doc.text().into();
        self.0.replace(Some((version, text.clone())));
        Some(text)
    }
}

/// The document byte offset of `iter`: through `whole` (the text, while GTK's lines differ
/// from the document's) by counting characters, without copying.
fn offset_of(doc: &Document, whole: Option<&str>, iter: &gtk::TextIter) -> usize {
    match whole {
        None => doc.byte_offset(iter),
        Some(text) => {
            let chars = usize::try_from(iter.offset()).unwrap_or(0);
            text.char_indices()
                .nth(chars)
                .map_or(text.len(), |(b, _)| b)
        }
    }
}

/// The iters at the document byte `offsets` (clamped, moved back to a character boundary); with
/// `whole`, from one pass over the text.
fn iters_at(editor: &Editor, whole: Option<&str>, offsets: &[usize]) -> Vec<Option<gtk::TextIter>> {
    let Some(text) = whole else {
        return offsets.iter().map(|&o| editor.iter_near(o)).collect();
    };
    let buffer = editor.buffer();
    let mut order: Vec<usize> = (0..offsets.len()).collect();
    order.sort_by_key(|&i| offsets[i]);
    let mut out = vec![None; offsets.len()];
    let (mut pos, mut chars) = (0, 0);
    for i in order {
        let mut at = offsets[i].min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        chars += text[pos..at].chars().count();
        pos = at;
        out[i] = i32::try_from(chars).ok().map(|c| buffer.iter_at_offset(c));
    }
    out
}

/// An open document as a [`Source`]: its buffer (or `whole`, see [`Texts`]), line index and
/// scanner states.
struct DocSource<'a> {
    doc: &'a Document,
    buffer: sourceview5::Buffer,
    whole: Option<Rc<str>>,
    index: Ref<'a, LineIndex>,
    states: Ref<'a, LineStates>,
}

impl<'a> DocSource<'a> {
    /// `None` while the document has no buffer or no scanner states (closed, or a large one
    /// still being scanned). `whole` is the text while GTK's lines differ from the document's.
    fn new(doc: &'a Document, whole: Option<Rc<str>>) -> Option<Self> {
        let buffer = doc.buffer()?;
        let states = Ref::filter_map(doc.line_states.borrow(), Option::as_ref).ok()?;
        let index = doc.line_index();
        if states.line_count() != index.line_count() {
            return None;
        }
        Some(Self {
            doc,
            buffer,
            whole,
            index,
            states,
        })
    }
}

impl Source for DocSource<'_> {
    fn index(&self) -> &LineIndex {
        &self.index
    }

    fn states(&self) -> &LineStates {
        &self.states
    }

    fn lines(&self, lines: Range<u32>) -> Option<Cow<'_, str>> {
        let index = &self.index;
        let count = index.line_count();
        let start = index.line_start(lines.start);
        let end = if lines.end >= count {
            index.text_len()
        } else {
            index.line_start(lines.end)
        };
        if let Some(whole) = &self.whole {
            return whole.get(start..end).map(Cow::Borrowed);
        }
        let (a, b) = if same_lines(self.doc, &self.buffer) {
            let a = self.buffer.iter_at_line(i32::try_from(lines.start).ok()?)?;
            let b = if lines.end >= count {
                self.buffer.end_iter()
            } else {
                self.buffer.iter_at_line(i32::try_from(lines.end).ok()?)?
            };
            (a, b)
        } else {
            (self.doc.iter_at_byte(start)?, self.doc.iter_at_byte(end)?)
        };
        let text = self.buffer.slice(&a, &b, true);
        (text.len() == end - start).then(|| Cow::Owned(text.into()))
    }

    fn with_text(&self, f: &mut dyn FnMut(&str)) {
        match &self.whole {
            Some(whole) => f(whole),
            None => self.doc.with_text(|text| f(text)),
        }
    }
}

// ───────────── Per-view state ─────────────

/// What an editor highlights.
struct Shown {
    /// The document version the ranges were computed for.
    version: u64,
    /// The highlighted ranges (document bytes, in order) and whether they match.
    ranges: Vec<Range<usize>>,
    matched: bool,
    tag: gtk::TextTag,
    /// Marks around each range, so the tag can be removed after edits moved it.
    marks: Vec<(gtk::TextMark, gtk::TextMark)>,
}

/// The pairs state of one editor.
struct ViewPairs {
    view: glib::WeakRef<TexView>,
    editor: Weak<Editor>,
    envs: RefCell<EnvCache>,
    texts: Texts,
    shown: RefCell<Option<Shown>>,
    /// The update idle, while one is queued.
    pending: RefCell<Option<glib::SourceId>>,
    observers: RefCell<Vec<ObserverId>>,
    signals: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    key_handler: Cell<Option<keys::HandlerId>>,
    /// Vim's pending command plus the last key it got in normal mode (see `keys::waits_invisibly`).
    typed: RefCell<String>,
    /// GtkSourceView's bracket matching before attach (restored on detach).
    brackets_were_on: Cell<bool>,
}

thread_local! {
    static VIEWS: RefCell<Vec<Rc<ViewPairs>>> = const { RefCell::new(Vec::new()) };
}

fn lookup(view: &TexView) -> Option<Rc<ViewPairs>> {
    VIEWS.with_borrow(|views| {
        views
            .iter()
            .find(|v| v.view.upgrade().is_some_and(|w| &w == view))
            .cloned()
    })
}

/// The side of the cursor that counts in `editor`'s current mode.
fn cursor_side(editor: &Editor) -> Side {
    match editor.vim_status().map(|s| s.mode) {
        Some(VimMode::Normal | VimMode::Visual | VimMode::VisualLine | VimMode::VisualBlock) => {
            Side::On
        }
        _ => Side::OnOrBefore,
    }
}

/// The delimiter of `editor`'s document at the byte `offset` (as `side` says) and its partner.
/// `None` when there is none there, or the document has no scanner states yet.
pub fn pair_at(editor: &Editor, offset: usize, side: Side) -> Option<Pair> {
    let doc = editor.document();
    match lookup(editor.view()) {
        Some(vp) => {
            let whole = vp.texts.get(doc, editor.buffer());
            let src = DocSource::new(doc, whole)?;
            find_pair(&src, &vp.envs, offset, side)
        }
        None => {
            let src = DocSource::new(doc, None)?;
            find_pair(&src, &RefCell::default(), offset, side)
        }
    }
}

/// An environment: its `\begin{name}` and `\end{name}` delimiters (M5 WP5.3, `envtools`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvSpan {
    pub name: String,
    /// The `\begin{name}` delimiter's document byte range.
    pub begin: Range<usize>,
    /// The `\end{name}` delimiter's document byte range.
    pub end: Range<usize>,
}

/// The innermost environment of `editor`'s document around the byte `offset`: from the start
/// of its `\begin{…}` to the end of its `\end{…}`, both included. Environments without an
/// `\end` do not count. From the environment cache (the `\begin`/`\end` delimiters of the
/// lines, see "Cost"), as the partner of each `\begin` before `offset` (innermost first) until
/// one closes after it. `None` when there is none, or the document has no scanner states yet.
pub fn enclosing_env(editor: &Editor, offset: usize) -> Option<EnvSpan> {
    let doc = editor.document();
    let (whole, own_cache) = match lookup(editor.view()) {
        Some(vp) => (vp.texts.get(doc, editor.buffer()), Some(vp)),
        None => (None, None),
    };
    let src = DocSource::new(doc, whole)?;
    let fallback = RefCell::new(EnvCache::default());
    let envs = own_cache.as_ref().map_or(&fallback, |vp| &vp.envs);
    let mut cache = envs.borrow_mut();
    cache.ensure(&src);
    let index = src.index();
    let at = |t: &EnvTok| index.line_start(t.line) + t.start as usize;
    let first_after = cache.toks.partition_point(|t| at(t) <= offset);
    for i in (0..first_after).rev() {
        let t = cache.toks[i];
        if !t.open {
            continue;
        }
        let Some(Some(end)) = cache.partner(t.line, t.start) else {
            continue;
        };
        let end_range = index.line_start(end.line) + end.start as usize
            ..index.line_start(end.line) + end.end as usize;
        if end_range.end < offset {
            continue;
        }
        let name = cache
            .names
            .iter()
            .find_map(|(n, &id)| (id == t.name).then(|| n.to_string()))?;
        return Some(EnvSpan {
            name,
            begin: at(&t)..index.line_start(t.line) + t.end as usize,
            end: end_range,
        });
    }
    None
}

/// What `editor` highlights: the delimiter at the cursor and its partner (document byte
/// ranges, in order), and whether they match (false: one unmatched delimiter). After an edit
/// that the highlight has not caught up with yet, the ranges are read from the buffer.
pub fn highlighted(editor: &Editor) -> Option<(Vec<Range<usize>>, bool)> {
    let vp = lookup(editor.view())?;
    let shown = vp.shown.borrow();
    let shown = shown.as_ref()?;
    let doc = editor.document();
    if shown.version == doc.version() {
        return Some((shown.ranges.clone(), shown.matched));
    }
    let buffer = editor.buffer();
    let mut ranges: Vec<Range<usize>> = shown
        .marks
        .iter()
        .map(|(a, b)| {
            doc.byte_offset(&buffer.iter_at_mark(a))..doc.byte_offset(&buffer.iter_at_mark(b))
        })
        .collect();
    ranges.sort_by_key(|r| r.start);
    Some((ranges, shown.matched))
}

/// Brings `editor`'s highlight up to date now (instead of from the idle a cursor move queues).
pub fn refresh_now(editor: &Editor) {
    if let Some(vp) = lookup(editor.view()) {
        if let Some(id) = vp.pending.take() {
            id.remove();
        }
        update(&vp);
    }
}

/// Queues an update of the highlight (once per main-loop iteration, before the next frame).
fn schedule(vp: &Rc<ViewPairs>) {
    if vp.pending.borrow().is_some() {
        return;
    }
    let weak = Rc::downgrade(vp);
    let id = glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
        if let Some(vp) = weak.upgrade() {
            vp.pending.take();
            update(&vp);
        }
        glib::ControlFlow::Break
    });
    vp.pending.replace(Some(id));
}

/// The cursor's document byte offset, through `whole` while GTK's lines differ.
fn cursor_offset(editor: &Editor, whole: Option<&str>) -> usize {
    let buffer = editor.buffer();
    offset_of(
        editor.document(),
        whole,
        &buffer.iter_at_mark(&buffer.get_insert()),
    )
}

/// Recomputes the highlight at the cursor and applies it if it changed.
fn update(vp: &Rc<ViewPairs>) {
    let Some(editor) = vp.editor.upgrade() else {
        return;
    };
    let doc = editor.document();
    let buffer = editor.buffer();
    let whole = vp.texts.get(doc, buffer);
    let wanted = {
        let cursor = cursor_offset(&editor, whole.as_deref());
        let side = cursor_side(&editor);
        DocSource::new(doc, whole.clone()).and_then(|src| find_pair(&src, &vp.envs, cursor, side))
    };
    let wanted: Option<(Vec<Range<usize>>, bool)> = wanted.and_then(|pair| match pair.partner {
        Partner::Found(range) => {
            let mut ranges = vec![pair.token, range];
            ranges.sort_by_key(|r| r.start);
            Some((ranges, true))
        }
        Partner::Unmatched => Some((vec![pair.token], false)),
        Partner::Unknown => None,
    });
    let version = doc.version();
    let unchanged = match (&*vp.shown.borrow(), &wanted) {
        (None, None) => true,
        (Some(shown), Some((ranges, matched))) => {
            shown.version == version && shown.ranges == *ranges && shown.matched == *matched
        }
        _ => false,
    };
    if unchanged {
        return;
    }
    clear(vp, &editor);
    let Some((ranges, matched)) = wanted else {
        return;
    };
    let (match_tag, unmatched_tag) = tags(buffer);
    let tag = if matched { match_tag } else { unmatched_tag };
    let offsets: Vec<usize> = ranges.iter().flat_map(|r| [r.start, r.end]).collect();
    let iters = iters_at(&editor, whole.as_deref(), &offsets);
    let mut marks = Vec::with_capacity(ranges.len());
    for pair in iters.chunks(2) {
        let [Some(a), Some(b)] = pair else {
            continue;
        };
        buffer.apply_tag(&tag, a, b);
        marks.push((
            buffer.create_mark(None, a, true),
            buffer.create_mark(None, b, false),
        ));
    }
    vp.shown.replace(Some(Shown {
        version,
        ranges,
        matched,
        tag,
        marks,
    }));
}

/// Removes the highlight.
fn clear(vp: &ViewPairs, editor: &Editor) {
    let Some(shown) = vp.shown.take() else {
        return;
    };
    let buffer = editor.buffer();
    for (start, end) in shown.marks {
        if !start.is_deleted() && !end.is_deleted() {
            let a = buffer.iter_at_mark(&start);
            let b = buffer.iter_at_mark(&end);
            buffer.remove_tag(&shown.tag, &a, &b);
        }
        for mark in [&start, &end] {
            if !mark.is_deleted() {
                buffer.delete_mark(mark);
            }
        }
    }
}

// ───────────── Tags and their look ─────────────

/// The buffer's match and unmatched tags, created (and styled) on first use.
fn tags(buffer: &sourceview5::Buffer) -> (gtk::TextTag, gtk::TextTag) {
    let table = buffer.tag_table();
    let get = |name: &str| {
        table.lookup(name).unwrap_or_else(|| {
            let tag = gtk::TextTag::new(Some(name));
            // Added last: above the syntax colours.
            table.add(&tag);
            style_tag(buffer, &tag);
            tag
        })
    };
    (get(MATCH_TAG), get(UNMATCHED_TAG))
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        a,
    )
}

/// Whether `buffer`'s style scheme is dark (its text background, else the system style).
fn is_dark(buffer: &sourceview5::Buffer) -> bool {
    let background = buffer
        .style_scheme()
        .and_then(|s| s.style("text"))
        .filter(|s| s.is_background_set())
        .and_then(|s| s.background())
        .and_then(|c| gdk::RGBA::parse(c.as_str()).ok());
    match background {
        Some(c) => 0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue() < 0.5,
        None => adw::StyleManager::default().is_dark(),
    }
}

/// Styles one of the two tags for `buffer`'s style scheme (see the module docs).
fn style_tag(buffer: &sourceview5::Buffer, tag: &gtk::TextTag) {
    let dark = is_dark(buffer);
    for property in ["foreground-set", "background-set", "weight-set"] {
        tag.set_property(property, false);
    }
    if tag.name().as_deref() == Some(UNMATCHED_TAG) {
        // Adwaita's error red: a tint, with the text in that red.
        let (tint, text) = if dark {
            (rgba(0xf6, 0x61, 0x51, 0.26), rgba(0xff, 0x93, 0x8c, 1.0))
        } else {
            (rgba(0xe0, 0x1b, 0x24, 0.14), rgba(0xc0, 0x1c, 0x28, 1.0))
        };
        tag.set_background_rgba(Some(&tint));
        tag.set_foreground_rgba(Some(&text));
        return;
    }
    let style = buffer.style_scheme().and_then(|s| s.style("bracket-match"));
    let color = |value: Option<glib::GString>| value.and_then(|v| gdk::RGBA::parse(&v).ok());
    let background = style
        .as_ref()
        .filter(|s| s.is_background_set())
        .and_then(|s| color(s.background()));
    match background {
        Some(background) => {
            tag.set_background_rgba(Some(&background));
            if let Some(fg) = style
                .as_ref()
                .filter(|s| s.is_foreground_set())
                .and_then(|s| color(s.foreground()))
            {
                tag.set_foreground_rgba(Some(&fg));
            }
        }
        None => {
            let accent = adw::StyleManager::default().accent_color_rgba();
            // Stronger than the math tint, which may lie under it.
            let alpha = if dark { 0.32 } else { 0.25 };
            let tint = gdk::RGBA::new(accent.red(), accent.green(), accent.blue(), alpha);
            tag.set_background_rgba(Some(&tint));
        }
    }
    if style
        .as_ref()
        .is_none_or(|s| s.is_bold_set() && s.is_bold())
    {
        tag.set_weight(pango::Weight::Bold.into_glib());
    }
}

/// Restyles the buffer's tags (the scheme or the accent colour changed).
fn restyle(buffer: &sourceview5::Buffer) {
    let table = buffer.tag_table();
    for name in [MATCH_TAG, UNMATCHED_TAG] {
        if let Some(tag) = table.lookup(name) {
            style_tag(buffer, &tag);
        }
    }
}

// ───────────── Jumping ─────────────

/// What a jump did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    /// The cursor moved to the partner.
    Moved,
    /// The delimiter has no partner (the bell rang).
    Unmatched,
    /// No delimiter at the cursor or after it on its line, or the search gave up.
    Nothing,
}

/// Jumps from the delimiter at `editor`'s cursor (or the next one on its line) to its partner:
/// the cursor goes to the partner's first character and the view scrolls as little as needed.
/// In Vim visual mode the selection is extended instead. The bell rings for an unmatched
/// delimiter.
pub fn jump_in(editor: &Editor) -> Jump {
    let side = match cursor_side(editor) {
        Side::On => Side::OnOrAfter,
        _ => Side::Near,
    };
    let whole =
        lookup(editor.view()).and_then(|vp| vp.texts.get(editor.document(), editor.buffer()));
    let cursor = cursor_offset(editor, whole.as_deref());
    let Some(pair) = pair_at(editor, cursor, side) else {
        return Jump::Nothing;
    };
    let target = match pair.partner {
        Partner::Found(range) => range,
        Partner::Unmatched => {
            editor.view().error_bell();
            return Jump::Unmatched;
        }
        Partner::Unknown => return Jump::Nothing,
    };
    let Some(Some(iter)) = iters_at(editor, whole.as_deref(), &[target.start]).pop() else {
        return Jump::Nothing;
    };
    let buffer = editor.buffer();
    let visual = matches!(
        editor.vim_status().map(|s| s.mode),
        Some(VimMode::Visual | VimMode::VisualLine | VimMode::VisualBlock)
    );
    if visual {
        buffer.move_mark(&buffer.get_insert(), &iter);
    } else {
        buffer.place_cursor(&iter);
    }
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.05, false, 0.0, 0.0);
    if let Some(vp) = lookup(editor.view()) {
        schedule(&vp);
    }
    Jump::Moved
}

/// `win.jump-pair` (Ctrl+%): [`jump_in`] in the active editor; the bell when there is nothing
/// to jump to.
pub fn jump(state: &Rc<WindowState>) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    if jump_in(&editor) == Jump::Nothing {
        editor.view().error_bell();
    }
}

/// The `%` key in Vim normal mode (see the module docs).
fn on_key(
    vp: &Rc<ViewPairs>,
    editor: &Rc<Editor>,
    event: &keys::KeyEvent,
    mode: VimMode,
) -> glib::Propagation {
    // The mode or the cursor may change with any key.
    schedule(vp);
    let typed = vp.typed.take();
    if !event.vim || mode != VimMode::Normal {
        return glib::Propagation::Proceed;
    }
    let Some(name) = keys::key_name(event.keyval, event.state) else {
        vp.typed.replace(typed);
        return glib::Propagation::Proceed;
    };
    let pending = editor.vim_status().map(|s| s.command).unwrap_or_default();
    vp.typed.replace(format!("{pending}{name}"));
    if name != "%" || !pending.is_empty() || keys::waits_invisibly(&typed) {
        return glib::Propagation::Proceed;
    }
    match jump_in(editor) {
        Jump::Moved | Jump::Unmatched => {
            vp.typed.borrow_mut().clear();
            glib::Propagation::Stop
        }
        Jump::Nothing => glib::Propagation::Proceed,
    }
}

// ───────────── Hooks ─────────────

/// Gives `editor` delimiter pairs (see the module docs). Called for every new editor tab; a
/// document that is not LaTeX keeps GtkSourceView's bracket matching and gets nothing.
pub fn attach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    if lookup(view).is_some() || !super::completion::is_latex(editor.document().path()) {
        return;
    }
    let buffer = editor.buffer().clone();
    let vp = Rc::new(ViewPairs {
        view: view.downgrade(),
        editor: Rc::downgrade(editor),
        envs: RefCell::default(),
        texts: Texts::default(),
        shown: RefCell::default(),
        pending: RefCell::default(),
        observers: RefCell::default(),
        signals: RefCell::default(),
        key_handler: Cell::new(None),
        typed: RefCell::default(),
        brackets_were_on: Cell::new(buffer.is_highlight_matching_brackets()),
    });
    VIEWS.with_borrow_mut(|views| {
        views.retain(|v| v.view.upgrade().is_some());
        views.push(vp.clone());
    });
    buffer.set_highlight_matching_brackets(false);

    let doc = editor.document();
    let mut observers = Vec::new();
    let weak = Rc::downgrade(&vp);
    observers.push(doc.connect_edit(move |_, _, _| {
        if let Some(vp) = weak.upgrade() {
            vp.envs.borrow_mut().in_change = true;
        }
    }));
    let weak = Rc::downgrade(&vp);
    observers.push(doc.connect_retag(move |lines| {
        if let Some(vp) = weak.upgrade() {
            vp.envs.borrow_mut().retag(lines);
            schedule(&vp);
        }
    }));
    let weak = Rc::downgrade(&vp);
    observers.push(doc.connect_lines_changed(move |first, removed, added| {
        if let Some(vp) = weak.upgrade() {
            vp.envs.borrow_mut().lines_changed(first, removed, added);
            schedule(&vp);
        }
    }));
    vp.observers.replace(observers);

    let mut signals: Vec<(glib::Object, glib::SignalHandlerId)> = Vec::new();
    let weak = Rc::downgrade(&vp);
    signals.push((
        buffer.clone().upcast(),
        buffer.connect_cursor_position_notify(move |_| {
            if let Some(vp) = weak.upgrade() {
                schedule(&vp);
            }
        }),
    ));
    signals.push((
        buffer.clone().upcast(),
        buffer.connect_style_scheme_notify(restyle),
    ));
    let style = adw::StyleManager::default();
    let weak_buffer = buffer.downgrade();
    signals.push((
        style.clone().upcast(),
        style.connect_accent_color_rgba_notify(move |_| {
            if let Some(buffer) = weak_buffer.upgrade() {
                restyle(&buffer);
            }
        }),
    ));
    vp.signals.replace(signals);

    let (weak, ed) = (Rc::downgrade(&vp), Rc::downgrade(editor));
    let handler = keys::register(view, PRIORITY_PERCENT_KEY, move |event, mode| {
        match (weak.upgrade(), ed.upgrade()) {
            (Some(vp), Some(editor)) => on_key(&vp, &editor, event, mode),
            _ => glib::Propagation::Proceed,
        }
    });
    vp.key_handler.set(handler);
    schedule(&vp);
}

/// Removes everything [`attach`] added (the highlight included) and gives the buffer back its
/// bracket matching. Called when the editor's tab closes.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    let Some(vp) = lookup(view) else {
        return;
    };
    VIEWS.with_borrow_mut(|views| views.retain(|v| !Rc::ptr_eq(v, &vp)));
    if let Some(id) = vp.pending.take() {
        id.remove();
    }
    clear(&vp, editor);
    let doc = editor.document();
    for id in vp.observers.take() {
        doc.disconnect(id);
    }
    for (object, id) in vp.signals.take() {
        object.disconnect(id);
    }
    if let Some(id) = vp.key_handler.take() {
        keys::unregister(view, id);
    }
    editor
        .buffer()
        .set_highlight_matching_brackets(vp.brackets_were_on.get());
    vp.envs.borrow_mut().reset();
    vp.texts.0.replace(None);
}

// ───────────── Demo ─────────────

/// The demo document used without FILE.
const DEMO_TEX: &str = r"\documentclass{article}
\usepackage{amsmath,amsthm}
\newtheorem{theorem}{Theorem}

\begin{document}
\section{Pairs}

\begin{theorem}\label{thm:pairs}
For $f\colon X \to Y$ and every $x$,
\begin{equation}
  \left( \sum_{i=1}^{n} \bigl\langle f(x_i), y \bigr\rangle \right)^{2}
  \le \Bigl\{ \lvert x \rvert \Bigr\}
\end{equation}
\end{theorem}
\end{document}
";

/// How long the demo waits for the editor and its scanner states.
const DEMO_TIMEOUT: Duration = Duration::from_secs(20);

/// `--demo pairs`: puts the cursor on a delimiter whose partner is a few lines away and resolves
/// once both are highlighted (the `q-pairs.png` snapshot). The cursor goes to the first
/// occurrence of `$LEMMA_DEMO_PAIR` when it is set, else to the first `\begin{…}` after the
/// first `\section` (else after `\begin{document}`) whose `\end` is at most 30 lines further.
/// Without FILE it writes a small document into a temporary directory and opens it.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    let editor = match state.active_editor() {
        Some(editor) => editor,
        None => {
            let dir = std::env::temp_dir().join(format!("lemma-pairs-demo-{}", std::process::id()));
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = dir.join("pairs.tex");
            std::fs::write(&path, DEMO_TEX).map_err(|e| format!("{}: {e}", path.display()))?;
            state.open_file(&path).await?
        }
    };
    let doc = editor.document().clone();
    crate::testmode::wait_until(DEMO_TIMEOUT, "the scanner states", || doc.has_line_states())
        .await?;
    let text = doc.text();
    let offset = match std::env::var("LEMMA_DEMO_PAIR") {
        Ok(needle) if !needle.is_empty() => text
            .find(&needle)
            .ok_or_else(|| format!("LEMMA_DEMO_PAIR {needle:?} is not in the document"))?,
        _ => demo_offset(&editor, &text).ok_or("no environment with its end nearby")?,
    };
    editor.place_cursor_at(offset);
    editor.view().grab_focus();
    let buffer = editor.buffer().clone();
    let view = editor.view().clone();
    crate::testmode::wait_until(DEMO_TIMEOUT, "the view", || {
        state.window.queue_draw();
        view.is_mapped() && view.height() > 0
    })
    .await?;
    view.scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.3);
    refresh_now(&editor);
    crate::testmode::wait_until(DEMO_TIMEOUT, "the highlight", || {
        highlighted(&editor).is_some()
    })
    .await?;
    crate::testmode::wait_for_frames(&state.window, 1).await;
    Ok(())
}

/// The first `\begin{…}` after the first `\section` (else after `\begin{document}`) whose
/// `\end` is at most 30 lines away.
fn demo_offset(editor: &Editor, text: &str) -> Option<usize> {
    let body = text
        .find("\\section")
        .or_else(|| text.find("\\begin{document}").map(|i| i + 1))
        .unwrap_or(0);
    let index = LineIndex::new(text);
    text[body..]
        .match_indices("\\begin{")
        .map(|(i, _)| body + i)
        .find(|&at| {
            let Some(Pair {
                partner: Partner::Found(end),
                ..
            }) = pair_at(editor, at, Side::On)
            else {
                return false;
            };
            index.line_of(end.start) - index.line_of(at) <= 30
        })
}

#[cfg(test)]
mod tests {
    //! The matcher on plain strings (the GTK side is `tests/gtk_pairs.rs`).

    use super::*;
    use lemma_core::TextEdit;
    use lemma_syntax::{ScanConfig, StrLines};
    use std::sync::Arc;

    /// A string with its line index and scanner states.
    struct StrSource {
        text: String,
        index: LineIndex,
        states: LineStates,
    }

    impl StrSource {
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

        /// Applies `edit` as the document does, telling `cache` what changed.
        fn edit(&mut self, edit: &TextEdit, cache: &mut EnvCache) {
            cache.in_change = true;
            let (first, removed, added) = self.index.line_delta(edit);
            self.index.apply(edit);
            edit.apply(&mut self.text);
            let retag = self.states.apply_edit(
                &StrLines {
                    text: &self.text,
                    index: &self.index,
                },
                first,
                removed,
                added,
            );
            cache.retag(retag);
            cache.lines_changed(first, removed, added);
        }
    }

    impl Source for StrSource {
        fn index(&self) -> &LineIndex {
            &self.index
        }

        fn states(&self) -> &LineStates {
            &self.states
        }

        fn lines(&self, lines: Range<u32>) -> Option<Cow<'_, str>> {
            let start = self.index.line_start(lines.start);
            let end = if lines.end >= self.index.line_count() {
                self.text.len()
            } else {
                self.index.line_start(lines.end)
            };
            self.text.get(start..end).map(Cow::Borrowed)
        }

        fn with_text(&self, f: &mut dyn FnMut(&str)) {
            f(&self.text);
        }
    }

    /// The cursor in a test text.
    const CURSOR: char = '‸';

    /// The pair at the `‸` of `marked` (removed): (kind, the token's text, the partner's text
    /// or `"UNMATCHED"` / `"UNKNOWN"`).
    fn pair_side(marked: &str, side: Side) -> Option<(PairKind, String, String)> {
        let offset = marked.find(CURSOR).expect("a cursor");
        let text = marked.replacen(CURSOR, "", 1);
        let src = StrSource::new(&text);
        let pair = find_pair(&src, &RefCell::default(), offset, side)?;
        let partner = match &pair.partner {
            Partner::Found(r) => text[r.clone()].to_owned(),
            Partner::Unmatched => "UNMATCHED".to_owned(),
            Partner::Unknown => "UNKNOWN".to_owned(),
        };
        Some((pair.kind, text[pair.token.clone()].to_owned(), partner))
    }

    fn pair(marked: &str) -> Option<(PairKind, String, String)> {
        pair_side(marked, Side::On)
    }

    /// The partner's offset in the text without the cursor mark.
    fn partner_offset(marked: &str) -> Option<usize> {
        let offset = marked.find(CURSOR).expect("a cursor");
        let text = marked.replacen(CURSOR, "", 1);
        let src = StrSource::new(&text);
        match find_pair(&src, &RefCell::default(), offset, Side::On)?.partner {
            Partner::Found(r) => Some(r.start),
            _ => None,
        }
    }

    fn found(kind: PairKind, token: &str, partner: &str) -> Option<(PairKind, String, String)> {
        Some((kind, token.to_owned(), partner.to_owned()))
    }

    use PairKind::*;

    #[test]
    fn environments_pair_by_name_and_nesting() {
        let doc = "\\begin{a}\n  \\begin{a}\n  \\end{a}\n\\end{a}\n";
        let marked = doc.replacen("\\begin{a}\n", "‸\\begin{a}\n", 1);
        assert_eq!(pair(&marked), found(Env, "\\begin{a}", "\\end{a}"));
        assert_eq!(partner_offset(&marked), doc.rfind("\\end{a}"));
        // From the outer end back to the outer begin, the cursor anywhere in the token.
        let marked = "\\begin{a}\n  \\begin{a}\n  \\end{a}\n\\end{a‸}\n";
        assert_eq!(partner_offset(marked), Some(0));
        // Other names do not count; a mismatched end does not close it.
        let marked = "‸\\begin{x}\n\\begin{y}\n\\end{z}\n\\end{y}\n\\end{x}\n";
        assert_eq!(
            partner_offset(marked),
            Some(marked.find("\\end{x}").unwrap() - 3)
        );
        assert_eq!(
            pair("\\begin{x}\n‸\\end{z}\n\\end{x}\n"),
            found(Env, "\\end{z}", "UNMATCHED")
        );
        assert_eq!(
            pair("‸\\begin{x}\n\\begin{x}\n\\end{x}\n"),
            found(Env, "\\begin{x}", "UNMATCHED")
        );
        // Blanks before the name, stars, optional arguments after it.
        assert_eq!(
            pair("\\begin {align*}[t] x \\end{align*}‸"),
            None,
            "past the end of the line"
        );
        assert_eq!(
            pair("\\begin {align*}[t] x ‸\\end{align*}"),
            found(Env, "\\end{align*}", "\\begin {align*}")
        );
        // Blank lines do not stop environments; several on one line.
        assert_eq!(
            pair("‸\\begin{proof}\n\nA \\begin{x}\\end{x}.\n\n\\end{proof}"),
            found(Env, "\\begin{proof}", "\\end{proof}")
        );
        assert_eq!(
            pair("\\begin{x}\\begin{x}\\end{x}‸\\end{x}"),
            found(Env, "\\end{x}", "\\begin{x}")
        );
        assert_eq!(
            partner_offset("\\begin{x}\\begin{x}\\end{x}‸\\end{x}"),
            Some(0)
        );
        // An unfinished name is not a delimiter.
        assert_eq!(pair("‸\\begin{equ\n\\end{equ}"), None);
    }

    #[test]
    fn comments_and_verbatim_do_not_count() {
        assert_eq!(
            pair("‸\\begin{x}\n% \\end{x}\n\\end{x}"),
            found(Env, "\\begin{x}", "\\end{x}")
        );
        assert_eq!(
            partner_offset("‸\\begin{x}\n% \\end{x}\n\\end{x}"),
            Some(20)
        );
        assert_eq!(pair("% ‸\\begin{x}\n\\end{x}"), None, "in a comment");
        let verbatim = "‸\\begin{verbatim}\n\\end{x} { $\n\\end{verbatim}\n";
        assert_eq!(
            pair(verbatim),
            found(Env, "\\begin{verbatim}", "\\end{verbatim}")
        );
        assert_eq!(
            pair("\\begin{verbatim}\n\\end{x} ‸{ $\n\\end{verbatim}\n"),
            None
        );
        assert_eq!(pair("‸{ \\verb|}| %}\n}"), found(Brace, "{", "}"));
        assert_eq!(partner_offset("‸{ \\verb|}| %}\n}"), Some(14));
        assert_eq!(
            pair("‸\\begin{comment}\n{\n\\end{comment}"),
            found(Env, "\\begin{comment}", "\\end{comment}")
        );
        assert_eq!(pair("\\begin{comment}\n‸{\n\\end{comment}"), None);
        // `\url{…}`: the braces count, the address does not.
        assert_eq!(pair("\\url‸{http://x/{a}\n}"), found(Brace, "{", "}"));
    }

    #[test]
    fn groups_and_escapes() {
        assert_eq!(pair("‸{a{b}c}"), found(Brace, "{", "}"));
        assert_eq!(partner_offset("‸{a{b}c}"), Some(6));
        assert_eq!(partner_offset("{a‸{b}c}"), Some(4));
        assert_eq!(partner_offset("{a{b}c‸}"), Some(0));
        assert_eq!(pair("‸{ \\} }"), found(Brace, "{", "}"));
        assert_eq!(partner_offset("‸{ \\} }"), Some(5));
        assert_eq!(pair("$‸\\{ x \\}$"), found(EscapedBrace, "\\{", "\\}"));
        assert_eq!(pair("‸[a[b]c]"), found(Bracket, "[", "]"));
        assert_eq!(partner_offset("‸[a[b]c]"), Some(6));
        assert_eq!(pair("‸(a(b)c)"), found(Paren, "(", ")"));
        assert_eq!(partner_offset("(a(b)c‸)"), Some(0));
        // `\\[2pt]` is a line break and an optional argument, not display math.
        assert_eq!(pair("a \\\\‸[2pt] b"), found(Bracket, "[", "]"));
        // `\$`, `\%` are characters.
        assert_eq!(pair("‸\\$ x"), None);
        assert_eq!(pair("‸{ \\% }"), found(Brace, "{", "}"));
        // Braces span blank lines; brackets and parentheses do not.
        assert_eq!(pair("‸{\n\na\n}"), found(Brace, "{", "}"));
        assert_eq!(pair("‸(\n\na\n)"), found(Paren, "(", "UNMATCHED"));
        assert_eq!(pair("‸[ a"), found(Bracket, "[", "UNMATCHED"));
        assert_eq!(pair("a ]‸"), None);
        assert_eq!(pair("a ‸]"), found(Bracket, "]", "UNMATCHED"));
        // Different kinds are independent.
        assert_eq!(partner_offset("‸[0,1) ]"), Some(6));
    }

    #[test]
    fn sized_and_named_delimiters() {
        assert_eq!(
            pair("$‸\\left. x \\right|$"),
            found(LeftRight, "\\left.", "\\right|")
        );
        assert_eq!(
            pair("$\\left( \\left[ x \\right] ‸\\right)$"),
            found(LeftRight, "\\right)", "\\left(")
        );
        assert_eq!(
            pair("$\\left( ‸\\left[ x \\right] \\right)$"),
            found(LeftRight, "\\left[", "\\right]")
        );
        // The delimiter is part of the token: its `(` is not a parenthesis of its own.
        assert_eq!(
            pair("$\\left‸( x \\right)$"),
            found(LeftRight, "\\left(", "\\right)")
        );
        assert_eq!(
            pair("$‸\\left\\langle x \\right\\rangle$"),
            found(LeftRight, "\\left\\langle", "\\right\\rangle")
        );
        assert_eq!(
            pair("$‸\\left \\{ x \\right\\}$"),
            found(LeftRight, "\\left \\{", "\\right\\}")
        );
        assert_eq!(
            pair("$‸\\bigl( x \\bigr)$"),
            found(Sized, "\\bigl(", "\\bigr)")
        );
        assert_eq!(
            pair("$‸\\Bigl\\{ \\bigl( x \\bigr) \\Biggr\\}$"),
            found(Sized, "\\Bigl\\{", "\\Biggr\\}")
        );
        assert_eq!(pair("$‸\\big( x \\big)$"), found(Sized, "\\big(", "\\big)"));
        assert_eq!(pair("$‸\\big| x \\big|$"), None, "no direction");
        assert_eq!(pair("$\\bigm‸| x$"), None);
        assert_eq!(
            pair("$‸\\langle x, \\langle y \\rangle \\rangle$"),
            found(Named(0), "\\langle", "\\rangle")
        );
        assert_eq!(
            partner_offset("$‸\\langle x, \\langle y \\rangle \\rangle$"),
            Some(30)
        );
        assert_eq!(
            pair("$\\lvert x ‸\\rvert$"),
            found(Named(1), "\\rvert", "\\lvert")
        );
        // `\left` at the end of a line, and an unmatched one.
        assert_eq!(
            pair("$‸\\left\nx \\right)$"),
            found(LeftRight, "\\left", "\\right)")
        );
        assert_eq!(
            pair("$‸\\left( x$\n\n$\\right)$"),
            found(LeftRight, "\\left(", "UNMATCHED")
        );
    }

    #[test]
    fn math_delimiters() {
        assert_eq!(pair("‸$a$ and $b$"), found(Dollar, "$", "$"));
        assert_eq!(partner_offset("‸$a$ and $b$"), Some(2));
        assert_eq!(partner_offset("$a$ and $b‸$"), Some(8));
        // A formula inside text inside a formula.
        assert_eq!(partner_offset("‸$a \\text{$b$} c$"), Some(15));
        assert_eq!(partner_offset("$a \\text{‸$b$} c$"), Some(11));
        // `$$`: display math; `$x$$y$`: two inline formulas.
        assert_eq!(pair("‸$$ x $$"), found(DoubleDollar, "$$", "$$"));
        assert_eq!(partner_offset("‸$$ x $$"), Some(5));
        assert_eq!(pair("$x‸$$y$"), found(Dollar, "$", "$"));
        assert_eq!(partner_offset("$x‸$$y$"), Some(0));
        assert_eq!(partner_offset("$x$‸$y$"), Some(5));
        assert_eq!(pair("‸\\(a\\)"), found(MathParen, "\\(", "\\)"));
        assert_eq!(pair("‸\\[\n a \n\\]"), found(MathBracket, "\\[", "\\]"));
        // A `$` inside `\(…\)` is not a delimiter.
        assert_eq!(pair("\\( a ‸$ b \\)"), None);
        // A blank line ends math: unmatched.
        assert_eq!(pair("‸$a\n\nb$"), found(Dollar, "$", "UNMATCHED"));
        assert_eq!(pair("a\\) b ‸\\)"), found(MathParen, "\\)", "UNMATCHED"));
        // Across lines of one paragraph.
        assert_eq!(pair("x ‸$a\nb$ y"), found(Dollar, "$", "$"));
        assert_eq!(partner_offset("x ‸$a\nb$ y"), Some(6));
    }

    #[test]
    fn cursor_sides() {
        // On: the character under the cursor only.
        assert_eq!(pair_side("a}‸ b", Side::On), None);
        assert_eq!(
            pair_side("{a}‸ b", Side::OnOrBefore),
            found(Brace, "}", "{")
        );
        // The character after the cursor wins.
        assert_eq!(
            pair_side("{a}‸[b]", Side::OnOrBefore),
            found(Bracket, "[", "]")
        );
        // Vim's `%` looks forward on the line.
        assert_eq!(
            pair_side("‸ab \\left( x \\right)", Side::OnOrAfter),
            found(LeftRight, "\\left(", "\\right)")
        );
        assert_eq!(pair_side("‸ab\n{}", Side::OnOrAfter), None);
        assert_eq!(pair_side("{}‸ ab", Side::Near), found(Brace, "}", "{"));
    }

    #[test]
    fn brace_searches_give_up_after_max_lines() {
        let near = format!("‸{{\n{}}}", "a\n".repeat(MAX_SCAN_LINES as usize - 50));
        assert_eq!(pair(&near), found(Brace, "{", "}"));
        let far = format!("‸{{\n{}}}", "a\n".repeat(MAX_SCAN_LINES as usize + 100));
        assert_eq!(pair(&far), found(Brace, "{", "UNKNOWN"));
        let back = format!("{{\n{}‸}}", "a\n".repeat(MAX_SCAN_LINES as usize + 100));
        assert_eq!(pair(&back), found(Brace, "}", "UNKNOWN"));
        // Environments never give up.
        let env = format!("‸\\begin{{x}}\n{}\\end{{x}}", "a\n".repeat(5000));
        assert_eq!(pair(&env), found(Env, "\\begin{x}", "\\end{x}"));
    }

    /// The cache's tokens as (line, start, end, name, open), for comparisons.
    fn toks(cache: &EnvCache) -> Vec<(u32, u32, u32, String, bool)> {
        let names: HashMap<u32, &str> = cache.names.iter().map(|(n, &i)| (i, &**n)).collect();
        cache
            .toks
            .iter()
            .map(|t| (t.line, t.start, t.end, names[&t.name].to_owned(), t.open))
            .collect()
    }

    fn fresh(src: &StrSource) -> EnvCache {
        let mut cache = EnvCache::default();
        cache.ensure(src);
        cache
    }

    #[test]
    fn env_cache_follows_edits() {
        let text =
            "\\begin{document}\n\\begin{a}\nx\n\\end{a}\n\\begin{b}\\end{b}\n\\end{document}\n";
        let mut src = StrSource::new(text);
        let mut cache = fresh(&src);
        assert_eq!(cache.toks.len(), 6);
        // A new environment in the middle.
        src.edit(
            &TextEdit::insert(text.find("x\n").unwrap(), "\\begin{c}\n\\end{c}\n"),
            &mut cache,
        );
        cache.ensure(&src);
        assert_eq!(toks(&cache), toks(&fresh(&src)));
        assert_eq!(cache.toks.len(), 8);
        // An end goes.
        let start = src.text.find("\\end{a}").unwrap();
        src.edit(&TextEdit::delete(start..start + 8), &mut cache);
        cache.ensure(&src);
        assert_eq!(toks(&cache), toks(&fresh(&src)));
        // `\begin{verbatim}` on the second line: the lines after it become verbatim.
        let second = src.index.line_start(1);
        src.edit(&TextEdit::insert(second, "\\begin{verbatim}\n"), &mut cache);
        cache.ensure(&src);
        assert_eq!(toks(&cache), toks(&fresh(&src)));
        assert!(
            toks(&cache)
                .iter()
                .all(|t| t.0 <= 1 || t.3 == "document" || t.3 == "verbatim"),
            "{:?}",
            toks(&cache)
        );
        // And closing it brings them back.
        let third = src.index.line_start(2);
        src.edit(&TextEdit::insert(third, "\\end{verbatim}\n"), &mut cache);
        cache.ensure(&src);
        assert_eq!(toks(&cache), toks(&fresh(&src)));
        assert!(toks(&cache).iter().any(|t| t.3 == "b"));
    }

    #[test]
    fn env_cache_random_edits_match_a_fresh_fill() {
        let pieces = [
            "\\begin{a}",
            "\\end{a}",
            "\\begin{b}",
            "\\end{b}",
            "\n",
            "\n\n",
            "x ",
            "% ",
            "\\begin{verbatim}",
            "\\end{verbatim}",
            "{",
            "}",
            "$",
            "\\verb|",
            "|",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            seed.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };
        for _round in 0..40 {
            let mut text = String::new();
            for _ in 0..30 {
                text.push_str(pieces[(next() % pieces.len() as u64) as usize]);
            }
            let mut src = StrSource::new(&text);
            let mut cache = fresh(&src);
            for _ in 0..25 {
                let len = src.text.len();
                let a = (next() as usize) % (len + 1);
                let b = (a + (next() as usize) % 12).min(len);
                let edit = if next() % 2 == 0 {
                    TextEdit::insert(a, pieces[(next() % pieces.len() as u64) as usize])
                } else {
                    TextEdit::delete(a..b)
                };
                src.edit(&edit, &mut cache);
                if next() % 3 == 0 {
                    cache.ensure(&src);
                    assert_eq!(toks(&cache), toks(&fresh(&src)), "{:?}", src.text);
                }
            }
            cache.ensure(&src);
            assert_eq!(toks(&cache), toks(&fresh(&src)), "{:?}", src.text);
        }
    }

    /// On the fixtures, every environment the structure index pairs (`EnvPair`) pairs here too.
    #[test]
    fn env_pairs_agree_with_the_structure_index() {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        for file in ["proj/main.tex", "proj/chapters/yoneda.tex", "large.tex"] {
            let text = std::fs::read_to_string(fixtures.join(file)).unwrap();
            let src = StrSource::new(&text);
            let cache = RefCell::default();
            let index = lemma_syntax::index_file(&text);
            let line = |l: u32| &text[src.index.line_range(l)];
            let mut checked = 0;
            for env in &index.envs {
                let Some(end_line) = env.end_line else {
                    continue;
                };
                let begin = format!("\\begin{{{}}}", env.name);
                let end = format!("\\end{{{}}}", env.name);
                // Lines with one such command, so the column is known.
                if line(env.begin_line).matches(&begin).count() != 1
                    || line(end_line).matches(&end).count() != 1
                {
                    continue;
                }
                let at = src.index.line_start(env.begin_line)
                    + line(env.begin_line).find(&begin).unwrap();
                let pair = find_pair(&src, &cache, at, Side::On).expect("a delimiter");
                let want = src.index.line_start(end_line) + line(end_line).find(&end).unwrap();
                assert_eq!(
                    pair.partner,
                    Partner::Found(want..want + end.len()),
                    "{file}: {} at line {}",
                    env.name,
                    env.begin_line + 1
                );
                // And back.
                let back = find_pair(&src, &cache, want, Side::On).expect("a delimiter");
                assert_eq!(back.partner, Partner::Found(at..at + begin.len()));
                checked += 1;
            }
            assert!(
                checked * 10 >= index.envs.len() * 9,
                "{file}: {checked} of {}",
                index.envs.len()
            );
        }
    }

    #[test]
    fn large_changes_rebuild_the_cache() {
        let text = "\\begin{a}\n\\end{a}\n";
        let mut src = StrSource::new(text);
        let mut cache = fresh(&src);
        let many = "\\begin{b}\n\\end{b}\n".repeat(ENV_REFILL_AT);
        src.edit(&TextEdit::insert(0, many), &mut cache);
        assert!(!cache.built, "too many changed lines");
        cache.ensure(&src);
        assert_eq!(cache.toks.len(), 2 * ENV_REFILL_AT + 2);
    }
}
