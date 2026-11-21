//! Text search on one page: the page's text (`poppler_page_get_text`) is matched here, and each
//! match is drawn from the boxes of its characters (`poppler_page_get_text_layout`, one box per
//! character of that text).
//!
//! Both sides are **folded** the same way first: compatibility forms are decomposed (the
//! ligatures `ﬁ`, `ﬂ`, `ﬀ`, `ﬃ`, `ﬄ` that PDFs hold become their letters), diacritics are dropped
//! when the query has none, and case is folded unless the search is case-sensitive, with full
//! Unicode case folding: `ß` and `ẞ` are `ss`, so `strasse` finds `Straße` and the reverse. A
//! line break reads as a space, and a hyphen that ends a line after a letter joins the lines
//! (`con-` / `travariant` matches "contravariant"); a word hyphenated across a **page break**
//! (the last line of a page, below which only lines without letters such as the page number may
//! follow, and the first line of the next page or the one after its running head) is found on
//! the page where it starts, drawn there.
//!
//! Whole words: a match is a whole word when the characters of the page text next to it are not
//! letters or digits (poppler's `WHOLE_WORDS_ONLY` rejects a word followed by punctuation:
//! "elit." in "adipiscing elit. Ut").
//!
//! A page whose layout does not have one box per character of its text is searched with
//! poppler's `find_text` instead (without the folding above), whose rectangles are read through
//! `poppler::ffi`: poppler-rs copies each `PopplerRectangle` into a plain four-field struct,
//! which drops the private "match continues on the next line" flag that a search across lines
//! sets.

use std::ffi::CString;
use std::{ptr, slice};

use glib::translate::{IntoGlib, ToGlibPtr};
use poppler::ffi;

use crate::{SearchMatch, SearchOptions};

/// A character box `(x1, y1, x2, y2)` in bp, top-left origin.
type CharBox = (f64, f64, f64, f64);

/// The matches of `query` on `page` in reading order, each as one rectangle per line it covers,
/// in bp from the page's top-left corner, and last the match that starts at the end of `page`
/// and goes on at the start of `next` (the page after it), if any. A query of white space only
/// matches nothing.
pub(crate) fn find(
    page: &poppler::Page,
    next: Option<&poppler::Page>,
    query: &str,
    options: SearchOptions,
) -> Vec<SearchMatch> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = page.text().map(|t| t.chars().collect()).unwrap_or_default();
    let boxes = text_layout(page);
    if boxes.len() != chars.len() {
        tracing::warn!("the text layout does not match the page text; searching with poppler");
        return poppler_find(page, query, options);
    }
    let fold = Fold::new(query, options);
    let needle = fold.query(query);
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = fold.text(&chars);
    let mut matches: Vec<SearchMatch> = occurrences(&hay.chars, &needle)
        .map(|(start, end)| (hay.from[start], hay.from[end - 1]))
        .filter(|&(first, last)| !options.whole_words || whole_word(&chars, first, last))
        .map(|(first, last)| rects(&chars, &boxes, first, last))
        .filter(|m| !m.is_empty())
        .collect();
    if let Some(next) = next
        && let Some(m) = find_across(&chars, &boxes, next, &fold, &needle, options)
    {
        matches.push(m);
    }
    matches
}

/// The match of `needle` that starts in the last line of a page (`chars`, `boxes`) that ends
/// with a hyphenated word and goes on in the first line of `next`, or in the line after it (a
/// running head), drawn on the first page.
fn find_across(
    chars: &[char],
    boxes: &[CharBox],
    next: &poppler::Page,
    fold: &Fold,
    needle: &[char],
    options: SearchOptions,
) -> Option<SearchMatch> {
    // Below the last line of the text only the page number and the like.
    let last = lines(chars)
        .into_iter()
        .rev()
        .take(3)
        .find(|l| chars[l.clone()].iter().any(|c| c.is_alphabetic()))?;
    let hyphen = last.end.checked_sub(1)?;
    if !is_hyphen(chars[hyphen]) || !chars[..hyphen].last().is_some_and(|c| c.is_alphabetic()) {
        return None;
    }
    let next_chars: Vec<char> = next.text()?.chars().collect();
    let heads = lines(&next_chars)
        .into_iter()
        .filter(|l| next_chars[l.clone()].iter().any(|c| c.is_alphabetic()))
        .take(2);
    for head in heads {
        // The two lines joined at the hyphen; `joint` is where the second one starts.
        let mut joined: Vec<char> = chars[last.start..hyphen].to_vec();
        let joint = joined.len();
        joined.extend_from_slice(&next_chars[head]);
        let hay = fold.text(&joined);
        let found = occurrences(&hay.chars, needle)
            .map(|(start, end)| (hay.from[start], hay.from[end - 1]))
            .find(|&(first, last)| first < joint && last >= joint);
        if let Some((first, last_char)) = found {
            if options.whole_words && !whole_word(&joined, first, last_char) {
                return None;
            }
            // Up to the letter before the hyphen, as for a match across lines.
            let m = rects(chars, boxes, last.start + first, hyphen - 1);
            return (!m.is_empty()).then_some(m);
        }
    }
    None
}

/// The character ranges of the lines of `chars` (without their `\n`).
fn lines(chars: &[char]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            out.push(start..i);
            start = i + 1;
        }
    }
    if start < chars.len() {
        out.push(start..chars.len());
    }
    out
}

fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{ad}')
}

/// Whether the hyphen at `i` joins its line to the next: it ends the line, after a letter.
fn joins_lines(chars: &[char], i: usize) -> bool {
    is_hyphen(chars[i])
        && chars.get(i + 1) == Some(&'\n')
        && i.checked_sub(1).is_some_and(|p| chars[p].is_alphabetic())
}

/// Whether the characters next to `first..=last` are not letters or digits.
fn whole_word(chars: &[char], first: usize, last: usize) -> bool {
    let word = |i: Option<usize>| {
        i.and_then(|i| chars.get(i))
            .is_some_and(|c| c.is_alphanumeric())
    };
    !word(first.checked_sub(1)) && !word(last.checked_add(1))
}

/// The start and end (exclusive) of every occurrence of `needle` in `hay`, left to right, not
/// overlapping.
fn occurrences<'a>(
    hay: &'a [char],
    needle: &'a [char],
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut at = 0;
    std::iter::from_fn(move || {
        while at + needle.len() <= hay.len() {
            let start = at;
            if hay[start..start + needle.len()] == *needle {
                at = start + needle.len();
                return Some((start, at));
            }
            at += 1;
        }
        None
    })
}

/// One rectangle per line for the characters `first..=last` of a page: the union of their
/// boxes, without the line breaks and the hyphens that join lines.
fn rects(chars: &[char], boxes: &[CharBox], first: usize, last: usize) -> SearchMatch {
    let mut out = Vec::new();
    let mut line: Option<CharBox> = None;
    let flush = |line: &mut Option<CharBox>, out: &mut SearchMatch| {
        if let Some((x1, y1, x2, y2)) = line.take()
            && x2 > x1
            && y2 > y1
        {
            out.push((x1, y1, x2 - x1, y2 - y1));
        }
    };
    for i in first..=last.min(chars.len().saturating_sub(1)) {
        if chars[i] == '\n' {
            flush(&mut line, &mut out);
            continue;
        }
        if joins_lines(chars, i) {
            continue;
        }
        let b = boxes[i];
        line = Some(match line {
            None => b,
            Some(l) => (l.0.min(b.0), l.1.min(b.1), l.2.max(b.2), l.3.max(b.3)),
        });
    }
    flush(&mut line, &mut out);
    out
}

/// How a query and a page's text are compared (see the module docs).
struct Fold {
    case_sensitive: bool,
    ignore_diacritics: bool,
}

/// A folded text, with the index of the character of the original that each character comes
/// from.
struct Folded {
    chars: Vec<char>,
    from: Vec<usize>,
}

impl Fold {
    fn new(query: &str, options: SearchOptions) -> Self {
        // As poppler does: diacritics count only when the query has some.
        let marked = query.chars().any(|c| {
            !c.is_ascii()
                && glib::normalize(c.to_string(), glib::NormalizeMode::All)
                    .chars()
                    .any(is_mark)
        });
        Fold {
            case_sensitive: options.case_sensitive,
            ignore_diacritics: !marked,
        }
    }

    fn query(&self, query: &str) -> Vec<char> {
        let chars: Vec<char> = query.chars().collect();
        let mut out = Folded {
            chars: Vec::new(),
            from: Vec::new(),
        };
        for (i, &c) in chars.iter().enumerate() {
            self.push(c, i, &mut out);
        }
        out.chars
    }

    /// `chars` folded, a line break read as a space and a hyphen that joins lines left out with
    /// its line break.
    fn text(&self, chars: &[char]) -> Folded {
        let mut out = Folded {
            chars: Vec::with_capacity(chars.len()),
            from: Vec::with_capacity(chars.len()),
        };
        let mut i = 0;
        while i < chars.len() {
            if joins_lines(chars, i) {
                i += 2;
                continue;
            }
            let c = if chars[i] == '\n' { ' ' } else { chars[i] };
            self.push(c, i, &mut out);
            i += 1;
        }
        out
    }

    fn push(&self, c: char, from: usize, out: &mut Folded) {
        if c.is_ascii() {
            out.chars.push(if self.case_sensitive {
                c
            } else {
                c.to_ascii_lowercase()
            });
            out.from.push(from);
            return;
        }
        let decomposed = glib::normalize(c.to_string(), glib::NormalizeMode::All);
        let kept: String = decomposed
            .chars()
            .filter(|&d| !(self.ignore_diacritics && is_mark(d)))
            .collect();
        let folded = if self.case_sensitive {
            kept
        } else {
            glib::casefold(&kept).to_string()
        };
        for d in folded.chars() {
            out.chars.push(d);
            out.from.push(from);
        }
    }
}

/// Whether `c` is a combining diacritical mark (the marks a decomposition of Latin, Greek and
/// Cyrillic letters gives).
fn is_mark(c: char) -> bool {
    matches!(c,
        '\u{300}'..='\u{36f}'
        | '\u{483}'..='\u{489}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe20}'..='\u{fe2f}')
}

/// The flags poppler searches with for `options`: case-insensitive unless asked, diacritics
/// ignored (poppler applies this only to queries without any), and matches across line breaks
/// (a hyphen ending a line is skipped).
fn flags(options: SearchOptions) -> poppler::FindFlags {
    let mut flags = poppler::FindFlags::MULTILINE | poppler::FindFlags::IGNORE_DIACRITICS;
    if options.case_sensitive {
        flags |= poppler::FindFlags::CASE_SENSITIVE;
    }
    flags
}

/// The matches of `query` on `page` as poppler's `find_text` finds them (see the module docs).
fn poppler_find(page: &poppler::Page, query: &str, options: SearchOptions) -> Vec<SearchMatch> {
    let Ok(text) = CString::new(query) else {
        return Vec::new();
    };
    let (_, page_h) = page.size();
    let mut matches = Vec::new();
    let mut current: SearchMatch = Vec::new();
    // SAFETY: `page` is live and `text` NUL-terminated. poppler returns a new GList of new
    // rectangles, which are read and then freed here, each exactly once.
    unsafe {
        let list = ffi::poppler_page_find_text_with_options(
            page.to_glib_none().0,
            text.as_ptr(),
            flags(options).into_glib(),
        );
        let mut node = list;
        while !node.is_null() {
            let rect = (*node).data as *mut ffi::PopplerRectangle;
            if !rect.is_null() {
                let r = &*rect;
                // PDF user space (origin bottom-left, y up), as for links.
                let (x0, x1) = (r.x1.min(r.x2), r.x1.max(r.x2));
                let (y0, y1) = (r.y1.min(r.y2), r.y1.max(r.y2));
                current.push((x0, page_h - y1, x1 - x0, y1 - y0));
                // The parts of a match spanning lines are consecutive; all but the last
                // continue.
                let continued =
                    ffi::poppler_rectangle_find_get_match_continued(rect) != glib::ffi::GFALSE;
                if !continued {
                    matches.push(std::mem::take(&mut current));
                }
                ffi::poppler_rectangle_free(rect);
            }
            node = (*node).next;
        }
        glib::ffi::g_list_free(list);
    }
    if !current.is_empty() {
        matches.push(current);
    }
    matches
}

/// The box `(x1, y1, x2, y2)` (bp, top-left origin) of every character of `page.text()`.
fn text_layout(page: &poppler::Page) -> Vec<CharBox> {
    let mut rects: *mut ffi::PopplerRectangle = ptr::null_mut();
    let mut n = 0;
    // SAFETY: `page` is a live PopplerPage; the out-parameters are valid to write to.
    let ok =
        unsafe { ffi::poppler_page_get_text_layout(page.to_glib_none().0, &mut rects, &mut n) };
    if ok == glib::ffi::GFALSE || rects.is_null() {
        return Vec::new();
    }
    // SAFETY: on success poppler returns an array of `n` rectangles, which we own.
    let boxes = unsafe { slice::from_raw_parts(rects, n as usize) }
        .iter()
        .map(|r| (r.x1, r.y1, r.x2, r.y2))
        .collect();
    // SAFETY: `rects` was allocated by poppler with g_malloc and is not used afterwards.
    unsafe { glib::ffi::g_free(rects.cast()) };
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(query: &str, text: &str, options: SearchOptions) -> Vec<String> {
        let fold = Fold::new(query, options);
        let needle = fold.query(query);
        let chars: Vec<char> = text.chars().collect();
        let hay = fold.text(&chars);
        occurrences(&hay.chars, &needle)
            .map(|(s, e)| chars[hay.from[s]..=hay.from[e - 1]].iter().collect())
            .collect()
    }

    #[test]
    fn sharp_s_ligatures_case_and_diacritics_fold() {
        let any = SearchOptions::default();
        let text = "Die Straße, die STRASSE und die Strasse. Gauß. ﬁnd the ﬂow.";
        assert_eq!(
            folded("strasse", text, any),
            ["Straße", "STRASSE", "Strasse"]
        );
        assert_eq!(
            folded("Straße", text, any),
            ["Straße", "STRASSE", "Strasse"]
        );
        assert_eq!(folded("GAUSS", text, any), ["Gauß"]);
        assert_eq!(folded("find", text, any), ["ﬁnd"]);
        assert_eq!(folded("flow", text, any), ["ﬂow"]);
        let exact = SearchOptions {
            case_sensitive: true,
            whole_words: false,
        };
        assert_eq!(folded("Straße", text, exact), ["Straße"]);
        assert_eq!(folded("Strasse", text, exact), ["Strasse"]);
        // Diacritics are ignored only when the query has none.
        assert_eq!(folded("cafe", "café cafe", any), ["café", "cafe"]);
        assert_eq!(folded("café", "café cafe", any), ["café"]);
    }

    #[test]
    fn lines_join_at_a_space_or_a_hyphen() {
        let any = SearchOptions::default();
        assert_eq!(
            folded("contravariant", "a contra-\nvariant b", any),
            ["contra-\nvariant"]
        );
        assert_eq!(folded("sit amet", "dolor sit\namet", any), ["sit\namet"]);
        // A hyphen after a digit, or not at the end of a line, stays.
        assert_eq!(folded("1-2", "pages 1-\n2", any), Vec::<String>::new());
        assert_eq!(folded("well-known", "a well-known b", any), ["well-known"]);
    }

    #[test]
    fn rectangles_per_line_without_the_joining_hyphen() {
        let chars: Vec<char> = "ab-\ncd".chars().collect();
        let boxes: Vec<CharBox> = vec![
            (0.0, 0.0, 5.0, 10.0),
            (5.0, 0.0, 10.0, 10.0),
            (10.0, 0.0, 13.0, 10.0),
            (13.0, 0.0, 13.0, 10.0),
            (0.0, 12.0, 5.0, 22.0),
            (5.0, 12.0, 10.0, 22.0),
        ];
        assert_eq!(
            rects(&chars, &boxes, 1, 4),
            [(5.0, 0.0, 5.0, 10.0), (0.0, 12.0, 5.0, 10.0)]
        );
    }
}
