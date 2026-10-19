// Owner: WP1.9 structure
//! Byte-level helpers for reading TeX arguments straight from source text.
//!
//! All positions are byte offsets into the same `&[u8]`. The readers never panic on malformed input:
//! they return `None` and let the caller resume scanning after the command.

use std::cell::RefCell;
use std::ops::Range;

/// Converts byte offsets into 0-based line numbers; cheap for (mostly) increasing offsets.
pub(crate) struct LineCounter<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: u32,
}

impl<'a> LineCounter<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            line: 0,
        }
    }

    /// The line containing byte `pos`.
    pub(crate) fn line_at(&mut self, pos: usize) -> u32 {
        let pos = pos.min(self.bytes.len());
        if pos >= self.pos {
            self.line += count_newlines(&self.bytes[self.pos..pos]);
        } else {
            self.line -= count_newlines(&self.bytes[pos..self.pos]);
        }
        self.pos = pos;
        self.line
    }
}

fn count_newlines(b: &[u8]) -> u32 {
    let n = memchr::memchr_iter(b'\n', b).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The byte at `i`, or 0 past the end (0 never occurs in TeX source we care about).
#[inline]
pub(crate) fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// Whether a byte may appear in a control word (`@` included, as in `.sty` files).
#[inline]
pub(crate) fn is_letter(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'@'
}

/// The end of the line containing `i` (the index of its `'\n'`, or the text length).
pub(crate) fn line_end(b: &[u8], i: usize) -> usize {
    memchr::memchr(b'\n', &b[i.min(b.len())..]).map_or(b.len(), |o| i + o)
}

/// Skips spaces, line ends and `%` comments like TeX does between arguments.
///
/// Stops at a blank line (a paragraph break), returning the index of its second `'\n'`.
pub(crate) fn skip_space(b: &[u8], mut i: usize) -> usize {
    let mut newline = false;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                if newline {
                    return i;
                }
                newline = true;
                i += 1;
            }
            b'%' => {
                // A comment swallows its own line end.
                i = line_end(b, i) + 1;
                newline = false;
            }
            _ => break,
        }
    }
    i.min(b.len())
}

/// Skips spaces and tabs only.
pub(crate) fn skip_blanks(b: &[u8], mut i: usize) -> usize {
    while matches!(at(b, i), b' ' | b'\t') {
        i += 1;
    }
    i
}

/// Whether the `'\n'` at `i` starts a blank line.
fn blank_line_after(b: &[u8], i: usize) -> bool {
    let mut j = i + 1;
    while matches!(at(b, j), b' ' | b'\t' | b'\r') {
        j += 1;
    }
    at(b, j) == b'\n'
}

/// A delimited argument starting at `i` (`{…}`, `[…]` or `(…)`, chosen by `b[i]`).
///
/// Braces nest; `\x` escapes are skipped; `%` comments are skipped. For `[` and `(` only a closing
/// delimiter at brace depth 0 ends the argument. Unless `long`, a blank line aborts the read, as it
/// would in TeX for a short command. Returns the content range (without delimiters) and the index
/// after the closing delimiter.
///
/// Inside [`with_text_index`], reads of the indexed text are answered from a [`TextIndex`] once
/// direct reads have scanned a lot (unclosed groups in a long text): same answers, in O(log n).
pub(crate) fn group(b: &[u8], i: usize, long: bool) -> Option<(Range<usize>, usize)> {
    let close = match at(b, i) {
        b'{' => b'}',
        b'[' => b']',
        b'(' => b')',
        _ => return None,
    };
    if let Some(answer) = indexed(b, |index| index.group(b, i, close, long)) {
        return answer;
    }
    let start = i + 1;
    let mut depth = 0u32;
    let mut j = start;
    let result = loop {
        if j >= b.len() {
            break None;
        }
        match b[j] {
            b'\\' => {
                j += 2;
                continue;
            }
            b'%' => {
                j = line_end(b, j);
                continue;
            }
            b'\n' if !long && blank_line_after(b, j) => break None,
            b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            // An unbalanced `}` inside `[…]` or `(…)` means the argument is malformed.
            b'}' if close != b'}' => break None,
            c if c == close && depth == 0 => break Some((start..j, j + 1)),
            _ => {}
        }
        j += 1;
    };
    scanned(b, j.min(b.len()) - i);
    result
}

/// A `{…}` group read verbatim: braces nest but `%` and `\` are ordinary (for `\url`, `\lstinline`).
pub(crate) fn raw_group(b: &[u8], i: usize) -> Option<(Range<usize>, usize)> {
    if at(b, i) != b'{' {
        return None;
    }
    if let Some(answer) = indexed(b, |index| index.raw_group(b, i)) {
        return answer;
    }
    let mut depth = 0u32;
    let mut result = None;
    let mut end = b.len();
    for (j, &c) in b.iter().enumerate().skip(i + 1) {
        match c {
            b'{' => depth += 1,
            b'}' if depth == 0 => {
                result = Some((i + 1..j, j + 1));
                end = j;
                break;
            }
            b'}' => depth -= 1,
            b'\n' if blank_line_after(b, j) => {
                end = j;
                break;
            }
            _ => {}
        }
    }
    scanned(b, end - i);
    result
}

// ----- the text index -----

/// How much direct group reads may scan in the indexed text (bytes per byte of text, plus a
/// constant) before the [`TextIndex`] is built.
const SCAN_FACTOR: usize = 4;
const SCAN_ALLOWANCE: usize = 64 << 10;

/// The text being indexed ([`with_text_index`]) and, once built, its [`TextIndex`].
struct Scope {
    ptr: usize,
    len: usize,
    scanned: usize,
    index: Option<TextIndex>,
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Runs `f` with `b` as the text whose group reads may be answered from a [`TextIndex`]: in a
/// text with many unclosed groups (`\section{\section{…`, generated or broken files), every read
/// would otherwise scan to the end of the text, which made the structure pass quadratic.
pub(crate) fn with_text_index<R>(b: &[u8], f: impl FnOnce() -> R) -> R {
    struct Reset(Option<Scope>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let previous = self.0.take();
            SCOPE.with_borrow_mut(|scope| *scope = previous);
        }
    }
    let previous = SCOPE.with_borrow_mut(|scope| {
        scope.replace(Scope {
            ptr: b.as_ptr() as usize,
            len: b.len(),
            scanned: 0,
            index: None,
        })
    });
    let _reset = Reset(previous);
    f()
}

/// The answer of the [`TextIndex`] of `b`, when `b` is the indexed text and its index is built
/// (`None` otherwise: read directly).
fn indexed<T>(b: &[u8], f: impl FnOnce(&TextIndex) -> Option<T>) -> Option<T> {
    SCOPE.with_borrow(|scope| {
        let scope = scope.as_ref()?;
        if scope.ptr != b.as_ptr() as usize || scope.len != b.len() {
            return None;
        }
        f(scope.index.as_ref()?)
    })
}

/// Counts `n` bytes scanned by a direct read of `b`; builds the index of the indexed text once
/// the reads have scanned more than a few times its length.
fn scanned(b: &[u8], n: usize) {
    SCOPE.with_borrow_mut(|scope| {
        let Some(scope) = scope.as_mut() else {
            return;
        };
        if scope.ptr != b.as_ptr() as usize || scope.len != b.len() || scope.index.is_some() {
            return;
        }
        scope.scanned += n;
        if scope.scanned > SCAN_FACTOR * b.len() + SCAN_ALLOWANCE {
            scope.index = Some(TextIndex::new(b));
        }
    });
}

/// Where every group of a text ends, as [`group`] and [`raw_group`] read them, computed in one
/// pass: the matching closer of each `{`, `[` and `(` (or none), and the blank lines.
#[derive(Debug, Default)]
pub(crate) struct TextIndex {
    /// `(opening, closing)` byte positions, sorted; `u32::MAX`: never closed (or, for `[`/`(`,
    /// ended by an unbalanced `}`).
    braces: Vec<(u32, u32)>,
    brackets: Vec<(u32, u32)>,
    parens: Vec<(u32, u32)>,
    /// The `'\n'`s that start a blank line, as [`group`] meets them.
    blanks: Vec<u32>,
    /// The same for [`raw_group`]'s reading (no escapes, no comments).
    raw_braces: Vec<(u32, u32)>,
    raw_blanks: Vec<u32>,
}

const OPEN: u32 = u32::MAX;

impl TextIndex {
    /// Indexes `b` (positions must fit in a `u32`: texts are at most a few MB).
    pub(crate) fn new(b: &[u8]) -> Self {
        let mut index = Self::default();
        let pos = |j: usize| u32::try_from(j).unwrap_or(OPEN - 1);
        // [`group`]'s reading: escapes and comments skipped.
        struct Level {
            brace: Option<usize>,
            brackets: Vec<usize>,
            parens: Vec<usize>,
        }
        let mut levels = vec![Level {
            brace: None,
            brackets: Vec::new(),
            parens: Vec::new(),
        }];
        let close_all = |list: &mut Vec<usize>, out: &mut Vec<(u32, u32)>, at: u32| {
            out.extend(list.drain(..).map(|o| (pos(o), at)));
        };
        let mut j = 0;
        while j < b.len() {
            match b[j] {
                b'\\' => {
                    j += 2;
                    continue;
                }
                b'%' => {
                    j = line_end(b, j);
                    continue;
                }
                b'\n' if blank_line_after(b, j) => index.blanks.push(pos(j)),
                b'{' => levels.push(Level {
                    brace: Some(j),
                    brackets: Vec::new(),
                    parens: Vec::new(),
                }),
                b'}' => {
                    let level = levels.last_mut().expect("the outer level");
                    // An unbalanced `}` ends the `[…]`/`(…)` reads of its level unclosed.
                    close_all(&mut level.brackets, &mut index.brackets, OPEN);
                    close_all(&mut level.parens, &mut index.parens, OPEN);
                    if level.brace.is_some() {
                        let level = levels.pop().expect("an inner level");
                        index.braces.push((pos(level.brace.unwrap_or(0)), pos(j)));
                    }
                }
                b'[' => levels.last_mut().expect("a level").brackets.push(j),
                b']' => {
                    let level = levels.last_mut().expect("a level");
                    close_all(&mut level.brackets, &mut index.brackets, pos(j));
                }
                b'(' => levels.last_mut().expect("a level").parens.push(j),
                b')' => {
                    let level = levels.last_mut().expect("a level");
                    close_all(&mut level.parens, &mut index.parens, pos(j));
                }
                _ => {}
            }
            j += 1;
        }
        for mut level in levels {
            close_all(&mut level.brackets, &mut index.brackets, OPEN);
            close_all(&mut level.parens, &mut index.parens, OPEN);
            if let Some(open) = level.brace {
                index.braces.push((pos(open), OPEN));
            }
        }
        // [`raw_group`]'s reading.
        let mut opens = Vec::new();
        for (j, &c) in b.iter().enumerate() {
            match c {
                b'{' => opens.push(j),
                b'}' => {
                    if let Some(open) = opens.pop() {
                        index.raw_braces.push((pos(open), pos(j)));
                    }
                }
                b'\n' if blank_line_after(b, j) => index.raw_blanks.push(pos(j)),
                _ => {}
            }
        }
        index
            .raw_braces
            .extend(opens.into_iter().map(|o| (pos(o), OPEN)));
        for list in [
            &mut index.braces,
            &mut index.brackets,
            &mut index.parens,
            &mut index.raw_braces,
        ] {
            list.sort_unstable();
        }
        index
    }

    /// [`group`] at `i` (whose byte is the opener of `close`); `None` when `i` was not read as an
    /// opener here (inside a comment, escaped): read directly then.
    fn group(
        &self,
        b: &[u8],
        i: usize,
        close: u8,
        long: bool,
    ) -> Option<Option<(Range<usize>, usize)>> {
        let list = match close {
            b'}' => &self.braces,
            b']' => &self.brackets,
            _ => &self.parens,
        };
        let blanks = if long { &[][..] } else { &self.blanks[..] };
        Self::answer(b, list, blanks, i)
    }

    /// [`raw_group`] at `i`.
    fn raw_group(&self, b: &[u8], i: usize) -> Option<Option<(Range<usize>, usize)>> {
        Self::answer(b, &self.raw_braces, &self.raw_blanks, i)
    }

    fn answer(
        b: &[u8],
        list: &[(u32, u32)],
        blanks: &[u32],
        i: usize,
    ) -> Option<Option<(Range<usize>, usize)>> {
        let i32 = u32::try_from(i).ok()?;
        let k = list.binary_search_by_key(&i32, |&(open, _)| open).ok()?;
        let close = list[k].1;
        if close == OPEN {
            return Some(None);
        }
        // A blank line between the opener and the closer ends a short read first.
        let first_blank = blanks.partition_point(|&p| p <= i32);
        if blanks.get(first_blank).is_some_and(|&p| p < close) {
            return Some(None);
        }
        let close = close as usize;
        debug_assert!(close < b.len());
        Some(Some((i + 1..close, close + 1)))
    }
}

/// A short `{…}` argument starting exactly at `i`.
pub(crate) fn brace(b: &[u8], i: usize) -> Option<(Range<usize>, usize)> {
    if at(b, i) == b'{' {
        group(b, i, false)
    } else {
        None
    }
}

/// A short `[…]` argument starting exactly at `i`.
pub(crate) fn bracket(b: &[u8], i: usize) -> Option<(Range<usize>, usize)> {
    if at(b, i) == b'[' {
        group(b, i, false)
    } else {
        None
    }
}

/// Skips optional `[…]` arguments (at most `max`), returning the index after the last one.
pub(crate) fn skip_optionals(b: &[u8], mut i: usize, max: usize) -> usize {
    for _ in 0..max {
        match bracket(b, skip_space(b, i)) {
            Some((_, end)) => i = end,
            None => break,
        }
    }
    i
}

/// Skips spaces and an optional `*`, returning (starred, index after).
pub(crate) fn star(b: &[u8], i: usize) -> (bool, usize) {
    let j = skip_space(b, i);
    if at(b, j) == b'*' {
        (true, j + 1)
    } else {
        (false, i)
    }
}

/// The name range of the control sequence whose backslash is at `i`, and the index after it.
///
/// A control word is a run of letters (`@` included); otherwise it is a single control symbol.
pub(crate) fn control_sequence(b: &[u8], i: usize) -> Option<(Range<usize>, usize)> {
    if at(b, i) != b'\\' || i + 1 >= b.len() {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < b.len() && is_letter(b[j]) {
        j += 1;
    }
    if j == start {
        // A control symbol; step over a whole UTF-8 character.
        j += utf8_len(b[start]);
    }
    Some((start..j.min(b.len()), j.min(b.len())))
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// Reads a defined command name: `\foo`, `{\foo}` or `{ \foo }`. Returns the name without the
/// backslash and the index after it.
pub(crate) fn defined_name(s: &str, i: usize) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    let i = skip_space(b, i);
    match at(b, i) {
        b'\\' => {
            let (name, end) = control_sequence(b, i)?;
            Some((&s[name], end))
        }
        b'{' => {
            let (content, end) = group(b, i, false)?;
            let inner = skip_space(b, content.start);
            let (name, after) = control_sequence(b, inner)?;
            (skip_space(b, after) >= content.end).then(|| (&s[name], end))
        }
        _ => None,
    }
}

/// Reads an optional `[n]` argument count (0–9) after `i`.
pub(crate) fn arg_count(s: &str, i: usize) -> Option<(u8, usize)> {
    let b = s.as_bytes();
    let j = skip_space(b, i);
    if at(b, j) != b'[' {
        return None;
    }
    let (content, end) = group(b, j, false)?;
    let n = s[content].trim().parse::<u8>().ok()?;
    Some((n.min(9), end))
}

/// Removes comments, collapses whitespace runs to one space and trims.
pub(crate) fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut pending_space = false;
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                // A comment also eats its line end and the next line's indentation.
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
                while chars.peek().is_some_and(|d| *d == ' ' || *d == '\t') {
                    chars.next();
                }
            }
            c if c.is_whitespace() => pending_space = true,
            _ => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(c);
                if c == '\\'
                    && let Some(d) = chars.next()
                {
                    if d.is_whitespace() {
                        // A control space.
                        out.push(' ');
                    } else {
                        out.push(d);
                    }
                }
            }
        }
    }
    out
}

/// Splits a comma list (`a, b,%\n c`) into trimmed, non-empty items.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    let cleaned = clean(s);
    cleaned
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Strips one pair of braces enclosing the whole string (`{{x}}` → `{x}`, `{a}{b}` unchanged).
pub(crate) fn strip_outer_braces(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'{' && b[b.len() - 1] == b'}' {
        let mut depth = 0i32;
        for (k, &c) in b.iter().enumerate() {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 && k != b.len() - 1 {
                        return s;
                    }
                }
                _ => {}
            }
        }
        if depth == 0 {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text index answers every group read exactly as a direct read does.
    #[test]
    fn text_index_answers_like_direct_reads() {
        let mut texts: Vec<Vec<u8>> = [
            &b"{a{b}\\}c%}\nd}x"[..],
            b"[a{]}b]{c}[a}b]",
            b"{a\n\nb} {a\n  \nb} [x\n\n] (a{)}b) (c",
            b"\\{x} \\\\{y} {%}\n}",
            b"\\section{\\section{\\section{",
            b"[[[]] ((()) {{{}}} [a{b]c}d] ([)] {[}]",
            b"\\url{a%b}\n{\\}}",
        ]
        .iter()
        .map(|t| t.to_vec())
        .collect();
        // Generated texts over the interesting bytes (a fixed linear congruential sequence).
        let alphabet = b"{}[]()\\%\n x";
        let mut seed = 12345u64;
        for len in [8usize, 40, 200] {
            for _ in 0..200 {
                let text: Vec<u8> = (0..len)
                    .map(|_| {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        alphabet[usize::try_from(seed >> 59).unwrap_or(0) % alphabet.len()]
                    })
                    .collect();
                texts.push(text);
            }
        }
        for b in &texts {
            let index = TextIndex::new(b);
            for i in 0..b.len() {
                let close = match b[i] {
                    b'{' => b'}',
                    b'[' => b']',
                    b'(' => b')',
                    _ => continue,
                };
                for long in [false, true] {
                    if let Some(answer) = index.group(b, i, close, long) {
                        assert_eq!(
                            answer,
                            group(b, i, long),
                            "group at {i} (long {long}) of {:?}",
                            String::from_utf8_lossy(b)
                        );
                    }
                }
                if close == b'}'
                    && let Some(answer) = index.raw_group(b, i)
                {
                    assert_eq!(
                        answer,
                        raw_group(b, i),
                        "raw group at {i} of {:?}",
                        String::from_utf8_lossy(b)
                    );
                }
            }
        }
        // Inside the scope, many unclosed groups build the index; the answers stay the same.
        let text = b"\\section{".repeat(20_000);
        let direct: Vec<_> = (0..200).map(|k| group(&text, k * 9 + 8, false)).collect();
        let scoped: Vec<_> = with_text_index(&text, || {
            (0..text.len() / 9)
                .map(|k| group(&text, k * 9 + 8, false))
                .collect::<Vec<_>>()
        });
        assert_eq!(direct[..], scoped[..200]);
    }

    #[test]
    fn groups_nest_and_skip_escapes_and_comments() {
        let s = br"{a{b}\}c%}
d}x";
        let (r, end) = group(s, 0, false).unwrap();
        assert_eq!(&s[r], b"a{b}\\}c%}\nd");
        assert_eq!(s[end], b'x');
    }

    #[test]
    fn optional_arguments_close_at_depth_zero() {
        let s = b"[a{]}b]{c}";
        let (r, end) = group(s, 0, false).unwrap();
        assert_eq!(&s[r], b"a{]}b");
        assert_eq!(s[end], b'{');
        assert!(group(b"[a}b]", 0, false).is_none());
    }

    #[test]
    fn short_groups_stop_at_blank_lines() {
        assert!(group(b"{a\n\nb}", 0, false).is_none());
        assert!(group(b"{a\n  \nb}", 0, true).is_some());
        assert!(group(b"{a\nb}", 0, false).is_some());
        assert!(group(b"{abc", 0, true).is_none());
    }

    #[test]
    fn skip_space_handles_comments_and_paragraphs() {
        let s = b"  % c\n  {x}";
        assert_eq!(s[skip_space(s, 0)], b'{');
        let s = b" \n {x}";
        assert_eq!(s[skip_space(s, 0)], b'{');
        let s = b" \n\n{x}";
        assert_eq!(skip_space(s, 0), 2);
    }

    #[test]
    fn control_sequences() {
        let s = r"\foo@bar1\%\é".as_bytes();
        assert_eq!(control_sequence(s, 0), Some((1..8, 8)));
        assert_eq!(control_sequence(s, 9), Some((10..11, 11)));
        assert_eq!(control_sequence(s, 11), Some((12..14, 14)));
    }

    #[test]
    fn defined_names() {
        assert_eq!(defined_name(r"{\cO}{x}", 0), Some(("cO", 5)));
        assert_eq!(defined_name(r" { \cO } {x}", 0), Some(("cO", 8)));
        assert_eq!(defined_name(r"\cO{x}", 0), Some(("cO", 3)));
        assert_eq!(defined_name(r"{cO}", 0), None);
        assert_eq!(defined_name(r"{\a\b}", 0), None);
    }

    #[test]
    fn cleaning() {
        assert_eq!(clean("  a\n  b % c\n   d\\% e  "), "a b d\\% e");
        assert_eq!(clean("a%\n  b"), "ab");
        assert_eq!(split_list(" a, b,% c\n d,,"), vec!["a", "b", "d"]);
        assert_eq!(strip_outer_braces("{{x}}"), "{x}");
        assert_eq!(strip_outer_braces("{a}{b}"), "{a}{b}");
        assert_eq!(strip_outer_braces("{a"), "{a");
    }

    #[test]
    fn line_counter_goes_both_ways() {
        let s = b"a\nb\nc\n";
        let mut lc = LineCounter::new(s);
        assert_eq!(lc.line_at(4), 2);
        assert_eq!(lc.line_at(2), 1);
        assert_eq!(lc.line_at(0), 0);
        assert_eq!(lc.line_at(100), 3);
    }
}
