//! The document outline (bookmarks) and page labels.
//!
//! poppler-rs does not wrap `poppler_index_iter_get_action`, so the action of each entry is read
//! through `poppler::ffi`, like the link actions in `links.rs`.

use std::ffi::CStr;

use glib::translate::{FromGlibPtrFull, ToGlibPtr, ToGlibPtrMut};
use poppler::ffi;

use crate::{LinkDest, OutlineItem, links};

/// Deeper levels are cut off (a malformed outline must not exhaust the stack).
const MAX_DEPTH: usize = 32;
/// At most this many entries are read (a malformed outline must not loop for ever).
const MAX_ITEMS: usize = 20_000;

/// The outline of `doc` (empty when it has none).
pub(crate) fn outline(doc: &poppler::Document) -> Vec<OutlineItem> {
    // SAFETY: `doc` is a live document; poppler returns a new iterator or NULL when the
    // document has no outline, which `from_glib_full` maps to `None`.
    let root: Option<poppler::IndexIter> = unsafe {
        FromGlibPtrFull::from_glib_full(ffi::poppler_index_iter_new(doc.to_glib_none().0))
    };
    let mut budget = MAX_ITEMS;
    root.map(|iter| level(doc, iter, 0, &mut budget))
        .unwrap_or_default()
}

/// The entries of `iter`'s level and below.
fn level(
    doc: &poppler::Document,
    mut iter: poppler::IndexIter,
    depth: usize,
    budget: &mut usize,
) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    loop {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let (title, dest) = entry(doc, &mut iter);
        let children = if depth + 1 < MAX_DEPTH {
            iter.child()
                .map(|child| level(doc, child, depth + 1, budget))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        items.push(OutlineItem {
            title,
            dest,
            open: iter.is_open(),
            children,
        });
        if !iter.next() {
            break;
        }
    }
    items
}

/// The title and destination of the entry `iter` points at.
fn entry(doc: &poppler::Document, iter: &mut poppler::IndexIter) -> (String, LinkDest) {
    // SAFETY: `iter` is valid; poppler returns a new action (or NULL), which we free.
    unsafe {
        let action = ffi::poppler_index_iter_get_action(iter.to_glib_none_mut().0);
        if action.is_null() {
            return (String::new(), LinkDest::Other);
        }
        // Every member of the union starts with the type tag and the title (`PopplerActionAny`).
        let title = (*action).any.title;
        let title = if title.is_null() {
            String::new()
        } else {
            clean_title(&CStr::from_ptr(title).to_string_lossy())
        };
        let dest = links::action_dest(doc, action);
        ffi::poppler_action_free(action);
        (title, dest)
    }
}

/// `title` with runs of white space collapsed to one space and trimmed.
fn clean_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One label per page of `doc`: the PDF's page label, else the 1-based page number.
pub(crate) fn page_labels(doc: &poppler::Document) -> Vec<String> {
    (0..doc.n_pages())
        .map(|i| {
            doc.page(i)
                .and_then(|p| p.label())
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| (i + 1).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(clean_title("  1\u{a0}Intro\nduction  "), "1 Intro duction");
        assert_eq!(clean_title(""), "");
    }
}
