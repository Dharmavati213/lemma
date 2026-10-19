//! Hyperlinks of a page, from poppler's link mapping.
//!
//! poppler-rs exposes `LinkMapping`, `Action` and `Dest` only as opaque boxed types, so the fields are
//! read through `poppler::ffi`.

use std::ffi::CStr;

use glib::translate::{ToGlibPtr, from_glib_full};
use poppler::ffi;

use crate::{Link, LinkDest};

/// The links on `page` of `doc`, with areas in bp from the page's top-left corner.
pub(crate) fn page_links(doc: &poppler::Document, page: &poppler::Page) -> Vec<Link> {
    let (_, page_h) = page.size();
    page.link_mapping()
        .iter()
        .map(|mapping| {
            let raw: *const ffi::PopplerLinkMapping = mapping.to_glib_none().0;
            // SAFETY: `mapping` owns the boxed PopplerLinkMapping for the duration of this closure.
            let (r, action) = unsafe { ((*raw).area, (*raw).action) };
            let (x0, x1) = (r.x1.min(r.x2), r.x1.max(r.x2));
            let (y0, y1) = (r.y1.min(r.y2), r.y1.max(r.y2));
            Link {
                // poppler reports PDF user space (origin bottom-left, y up).
                area: (x0, page_h - y1, x1 - x0, y1 - y0),
                // SAFETY: the action belongs to the mapping, which outlives the call.
                dest: unsafe { action_dest(doc, action) },
            }
        })
        .collect()
}

/// # Safety
/// `action` is null or points to a valid `PopplerAction`.
pub(crate) unsafe fn action_dest(
    doc: &poppler::Document,
    action: *const ffi::PopplerAction,
) -> LinkDest {
    if action.is_null() {
        return LinkDest::Other;
    }
    // SAFETY: every member of the PopplerAction union starts with its type tag, which selects the
    // member that is valid to read.
    unsafe {
        match (*action).type_ {
            ffi::POPPLER_ACTION_GOTO_DEST => goto_dest(doc, (*action).goto_dest.dest),
            ffi::POPPLER_ACTION_URI => {
                let uri = (*action).uri.uri;
                if uri.is_null() {
                    LinkDest::Other
                } else {
                    LinkDest::Uri(CStr::from_ptr(uri).to_string_lossy().into_owned())
                }
            }
            _ => LinkDest::Other,
        }
    }
}

/// # Safety
/// `dest` is null or points to a valid `PopplerDest`.
unsafe fn goto_dest(doc: &poppler::Document, dest: *const ffi::PopplerDest) -> LinkDest {
    if dest.is_null() {
        return LinkDest::Other;
    }
    // SAFETY: fields are read through the raw pointer one at a time; the struct is valid per the
    // contract. `named_dest` is passed back to poppler unchanged, as poppler_document_find_dest expects.
    unsafe {
        if (*dest).type_ != ffi::POPPLER_DEST_NAMED {
            return explicit_dest(doc, dest);
        }
        let name = (*dest).named_dest;
        if name.is_null() {
            return LinkDest::Other;
        }
        let found: Option<poppler::Dest> =
            from_glib_full(ffi::poppler_document_find_dest(doc.to_glib_none().0, name));
        match found {
            Some(found) => {
                let ptr: *const ffi::PopplerDest = found.to_glib_none().0;
                if (*ptr).type_ == ffi::POPPLER_DEST_NAMED {
                    LinkDest::Other
                } else {
                    explicit_dest(doc, ptr)
                }
            }
            None => LinkDest::Other,
        }
    }
}

/// # Safety
/// `dest` points to a valid, explicit (not named) `PopplerDest`.
unsafe fn explicit_dest(doc: &poppler::Document, dest: *const ffi::PopplerDest) -> LinkDest {
    // SAFETY: per the contract; only the leading, non-bitfield fields are read.
    let (kind, page_num, top) = unsafe { ((*dest).type_, (*dest).page_num, (*dest).top) };
    // `page_num` is 1-based; a destination outside the document is a dangling link.
    if page_num < 1 || page_num > doc.n_pages() {
        return LinkDest::Other;
    }
    let page = (page_num - 1) as u32;
    // poppler stores an unspecified `top` (e.g. `/XYZ left null zoom`) as 0, i.e. the page bottom,
    // which is never a useful target; treat it as "top of the page".
    let has_top = matches!(
        kind,
        ffi::POPPLER_DEST_XYZ
            | ffi::POPPLER_DEST_FITH
            | ffi::POPPLER_DEST_FITBH
            | ffi::POPPLER_DEST_FITR
    ) && top > 0.0;
    let y_bp = if has_top {
        doc.page(page_num - 1).map(|p| {
            let h = p.size().1;
            (h - top).clamp(0.0, h)
        })
    } else {
        None
    };
    LinkDest::Page { page, y_bp }
}
