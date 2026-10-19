// Owner: WP1.9 structure
//! A tolerant `.bib` scanner for completion and hover.
//!
//! It reads `@type{key, field = value, …}` (or `(…)`) entries, where a value is `{…}`, `"…"`, a
//! number or a `@string` name, possibly concatenated with `#`. `@string` definitions are expanded,
//! `@preamble` and `@comment` are skipped, and text outside entries is ignored. A malformed entry is
//! dropped and scanning resumes at the next `@` after the point where it went wrong. An `@comment`
//! whose delimiter is never closed hides nothing: scanning resumes right after its opening
//! delimiter, as in BibTeX.

use crate::structure::args::{LineCounter, strip_outer_braces};
use std::collections::HashMap;

/// One `@kind{key, ...}` entry with the fields shown in completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BibEntry {
    /// The citation key, as written.
    pub key: String,
    /// Entry type, e.g. "article" (lowercased).
    pub kind: String,
    /// `author`, or `editor` when there is no author.
    pub author: Option<String>,
    /// `year`, or the leading year of biblatex's `date` (`2020-05-01` → `2020`).
    pub year: Option<String>,
    pub title: Option<String>,
    /// 0-based line of the `@`.
    pub line: u32,
}

/// Tolerant: a malformed entry is skipped, scanning resumes at the next '@'.
///
/// Field values have their outer delimiters removed and whitespace collapsed; inner braces and
/// TeX markup are kept (`Rev{\^e}tements`).
pub fn scan_bib(text: &str) -> Vec<BibEntry> {
    let mut parser = Parser {
        s: text,
        b: text.as_bytes(),
        strings: HashMap::new(),
    };
    let mut lines = LineCounter::new(text.as_bytes());
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = memchr::memchr(b'@', &parser.b[i..]) {
        let at = i + off;
        match parser.item(at) {
            Ok((Some(fields), end)) => {
                out.push(fields.into_entry(lines.line_at(at)));
                i = end;
            }
            Ok((None, end)) => i = end,
            Err(failed_at) => i = failed_at.max(at + 1),
        }
    }
    out
}

/// The fields of one entry, before it is turned into a [`BibEntry`].
#[derive(Default)]
struct Fields {
    key: String,
    kind: String,
    author: Option<String>,
    editor: Option<String>,
    year: Option<String>,
    date: Option<String>,
    title: Option<String>,
}

impl Fields {
    fn into_entry(self, line: u32) -> BibEntry {
        let year = self.year.or_else(|| {
            let date = self.date?;
            let y = date.get(..4)?;
            y.bytes().all(|c| c.is_ascii_digit()).then(|| y.to_owned())
        });
        BibEntry {
            key: self.key,
            kind: self.kind,
            author: self.author.or(self.editor),
            year,
            title: self.title,
            line,
        }
    }
}

/// A parse failure at this byte offset.
type Failed = usize;

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    /// `@string` macros, by lowercased name.
    strings: HashMap<String, String>,
}

impl Parser<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    /// Skips whitespace and, between fields, `%` comment lines (biber accepts them; inside a
    /// value `%` is literal and never reaches here).
    fn skip_ws(&self, mut i: usize) -> usize {
        loop {
            match self.byte(i) {
                c if c.is_ascii_whitespace() => i += 1,
                b'%' => i = memchr::memchr(b'\n', &self.b[i..]).map_or(self.b.len(), |o| i + o),
                _ => return i,
            }
        }
    }

    /// An identifier: entry types, field names, `@string` names and bare values.
    fn ident(&self, i: usize) -> usize {
        let mut j = i;
        while j < self.b.len()
            && !self.b[j].is_ascii_whitespace()
            && !matches!(
                self.b[j],
                b'=' | b',' | b'#' | b'{' | b'}' | b'(' | b')' | b'"' | b'%' | b'@'
            )
        {
            j += 1;
        }
        j
    }

    /// Parses the item whose `@` is at `at`. Returns the entry (or `None` for `@string`,
    /// `@preamble`, `@comment`) and the index after it.
    fn item(&mut self, at: usize) -> Result<(Option<Fields>, usize), Failed> {
        let i = self.skip_ws(at + 1);
        let type_end = self.ident(i);
        if type_end == i {
            return Err(at + 1);
        }
        let kind = self.s[i..type_end].to_ascii_lowercase();
        let open = self.skip_ws(type_end);
        let close = match self.byte(open) {
            b'{' => b'}',
            b'(' => b')',
            _ => return Err(open),
        };
        let i = open + 1;
        match kind.as_str() {
            "comment" => {
                // Up to the matching delimiter, so a commented-out entry stays hidden. Without
                // one, BibTeX's rule applies: the rest is junk up to the next `@`.
                let end = if close == b'}' {
                    self.balanced(open, false).ok()
                } else {
                    memchr::memchr(b')', &self.b[i..]).map(|o| i + o + 1)
                };
                Ok((None, end.unwrap_or(i)))
            }
            "preamble" => {
                let (_, j) = self.value(i)?;
                let j = self.skip_ws(j);
                if self.byte(j) != close {
                    return Err(j);
                }
                Ok((None, j + 1))
            }
            "string" => {
                let mut j = self.skip_ws(i);
                let name_end = self.ident(j);
                if name_end == j {
                    return Err(j);
                }
                let name = self.s[j..name_end].to_ascii_lowercase();
                j = self.skip_ws(name_end);
                if self.byte(j) != b'=' {
                    return Err(j);
                }
                let (value, j) = self.value(j + 1)?;
                let j = self.skip_ws(j);
                if self.byte(j) != close {
                    return Err(j);
                }
                self.strings.insert(name, value);
                Ok((None, j + 1))
            }
            _ => {
                let fields = Fields {
                    kind,
                    ..Fields::default()
                };
                self.entry(i, close, fields).map(|(f, end)| (Some(f), end))
            }
        }
    }

    /// The key and fields after the opening delimiter.
    fn entry(
        &mut self,
        i: usize,
        close: u8,
        mut fields: Fields,
    ) -> Result<(Fields, usize), Failed> {
        let key_start = self.skip_ws(i);
        let mut j = key_start;
        while j < self.b.len()
            && !matches!(self.b[j], b',' | b'}' | b')' | b'{' | b'"' | b'@' | b'=')
        {
            if self.b[j] == b'\n' {
                break;
            }
            j += 1;
        }
        fields.key = self.s[key_start..j].trim().to_owned();
        if fields.key.is_empty() || fields.key.contains(char::is_whitespace) {
            return Err(j);
        }
        j = self.skip_ws(j);
        loop {
            match self.byte(j) {
                c if c == close => return Ok((fields, j + 1)),
                b',' => {
                    j = self.skip_ws(j + 1);
                    continue;
                }
                _ => {}
            }
            let name_end = self.ident(j);
            if name_end == j {
                return Err(j);
            }
            let name = self.s[j..name_end].to_ascii_lowercase();
            j = self.skip_ws(name_end);
            if self.byte(j) != b'=' {
                return Err(j);
            }
            let (value, k) = self.value(j + 1)?;
            j = self.skip_ws(k);
            if !matches!(self.byte(j), b',') && self.byte(j) != close {
                return Err(j);
            }
            let slot = match name.as_str() {
                "author" => &mut fields.author,
                "editor" => &mut fields.editor,
                "year" => &mut fields.year,
                "date" => &mut fields.date,
                "title" => &mut fields.title,
                _ => continue,
            };
            if slot.is_none() && !value.is_empty() {
                *slot = Some(value);
            }
        }
    }

    /// A field value: parts joined by `#`. Returns the cleaned text and the index after it.
    fn value(&self, i: usize) -> Result<(String, usize), Failed> {
        let mut raw = String::new();
        let mut j = i;
        loop {
            j = self.skip_ws(j);
            match self.byte(j) {
                b'{' => {
                    let end = self.balanced(j, true)?;
                    raw.push_str(&self.s[j + 1..end - 1]);
                    j = end;
                }
                b'"' => {
                    let end = self.quoted(j)?;
                    raw.push_str(&self.s[j + 1..end - 1]);
                    j = end;
                }
                _ => {
                    let end = self.ident(j);
                    if end == j {
                        return Err(j);
                    }
                    let word = &self.s[j..end];
                    if word.bytes().all(|c| c.is_ascii_digit()) {
                        raw.push_str(word);
                    } else {
                        let expanded = self.strings.get(&word.to_ascii_lowercase());
                        raw.push_str(expanded.map_or(word, String::as_str));
                    }
                    j = end;
                }
            }
            let k = self.skip_ws(j);
            if self.byte(k) == b'#' {
                j = k + 1;
            } else {
                break;
            }
        }
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let value = strip_outer_braces(&collapsed);
        Ok((value.to_owned(), j))
    }

    /// The index after the `}` matching the `{` at `i`. With `guard`, a line starting with `@`
    /// inside the group is taken as the next entry and fails the parse.
    fn balanced(&self, i: usize, guard: bool) -> Result<usize, Failed> {
        debug_assert_eq!(self.byte(i), b'{');
        let mut depth = 0u32;
        let mut j = i;
        while j < self.b.len() {
            match self.b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(j + 1);
                    }
                }
                b'\n' if guard && self.entry_starts_after(j) => return Err(j + 1),
                _ => {}
            }
            j += 1;
        }
        Err(self.b.len())
    }

    /// The index after the `"` closing the one at `i` (braces inside protect quotes).
    fn quoted(&self, i: usize) -> Result<usize, Failed> {
        let mut depth = 0u32;
        let mut j = i + 1;
        while j < self.b.len() {
            match self.b[j] {
                b'{' => depth += 1,
                b'}' if depth > 0 => depth -= 1,
                b'}' => return Err(j),
                b'"' if depth == 0 => return Ok(j + 1),
                b'\n' if self.entry_starts_after(j) => return Err(j + 1),
                _ => {}
            }
            j += 1;
        }
        Err(self.b.len())
    }

    /// Whether the line after the `'\n'` at `i` starts (after indentation) with `@`: an
    /// unterminated value has probably run into the next entry.
    fn entry_starts_after(&self, i: usize) -> bool {
        let mut j = i + 1;
        while matches!(self.byte(j), b' ' | b'\t') {
            j += 1;
        }
        self.byte(j) == b'@'
    }
}
