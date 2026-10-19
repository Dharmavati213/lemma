//! Text search on one page, with poppler's `find_text`.
//!
//! The rectangles are read through `poppler::ffi`: poppler-rs copies each `PopplerRectangle`
//! into a plain four-field struct, which drops the private "match continues on the next line"
//! flag that a search across lines sets.
//!
//! Whole words are checked here rather than by poppler, whose `WHOLE_WORDS_ONLY` rejects a word
//! followed by punctuation ("elit." in "adipiscing elit. Ut"): a match is a whole word when the
//! characters of the page text next to it are not letters or digits.

use std::ffi::CString;
use std::{ptr, slice};

use glib::translate::{IntoGlib, ToGlibPtr};
use poppler::ffi;

use crate::{SearchMatch, SearchOptions};

/// The flags poppler searches with for `options`: case-insensitive unless asked, diacritics
/// ignored (poppler applies this only to queries without any), and matches across line breaks
/// (a hyphen ending a line is skipped, so `con-` / `travariant` matches "contravariant").
fn flags(options: SearchOptions) -> poppler::FindFlags {
    let mut flags = poppler::FindFlags::MULTILINE | poppler::FindFlags::IGNORE_DIACRITICS;
    if options.case_sensitive {
        flags |= poppler::FindFlags::CASE_SENSITIVE;
    }
    flags
}

/// The matches of `query` on `page` in reading order, each as one rectangle per line it covers,
/// in bp from the page's top-left corner. A query of white space only (or with a NUL) matches
/// nothing.
pub(crate) fn find(page: &poppler::Page, query: &str, options: SearchOptions) -> Vec<SearchMatch> {
    if query.trim().is_empty() {
        return Vec::new();
    }
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
    if options.whole_words && !matches.is_empty() {
        retain_whole_words(page, &mut matches);
    }
    matches
}

/// Keeps the matches whose neighbouring characters in the page text are not letters or digits.
fn retain_whole_words(page: &poppler::Page, matches: &mut Vec<SearchMatch>) {
    let Some(text) = page.text() else {
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    let boxes = text_layout(page);
    if boxes.len() != chars.len() {
        tracing::warn!("the text layout does not match the page text; whole words are not checked");
        return;
    }
    let near = |a: f64, b: f64| (a - b).abs() < 0.5;
    let word = |i: Option<usize>| {
        i.and_then(|i| chars.get(i))
            .is_some_and(|c| c.is_alphanumeric())
    };
    matches.retain(|m| {
        let (Some(first), Some(last)) = (m.first(), m.last()) else {
            return false;
        };
        let start = boxes
            .iter()
            .position(|b| near(b.0, first.0) && near(b.1, first.1));
        let Some(start) = start else {
            return true;
        };
        let right = last.0 + last.2;
        let end = boxes[start..]
            .iter()
            .position(|b| near(b.2, right) && near(b.1, last.1))
            .map(|i| start + i);
        let Some(end) = end else {
            return true;
        };
        !word(start.checked_sub(1)) && !word(end.checked_add(1))
    });
}

/// The box `(x1, y1, x2, y2)` (bp, top-left origin) of every character of `page.text()`.
fn text_layout(page: &poppler::Page) -> Vec<(f64, f64, f64, f64)> {
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
