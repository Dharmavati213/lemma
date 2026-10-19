//! Pages turned for display by their `/Rotate` entry.
//!
//! poppler reports the size of such a page as displayed and renders it turned, so the view lays it
//! out and draws it in display space. SyncTeX boxes and edit queries are in TeX's unturned page
//! space, so the worker maps them between the two spaces with the page's [`Rotation`].
//!
//! poppler-glib does not expose `/Rotate`. A quarter turn shows in the page size (the crop box
//! with width and height swapped); whether it is clockwise or anticlockwise is read from the
//! direction the page's text runs on the displayed page, and is clockwise (`/Rotate 90`, the
//! common case) for pages without text. Upside-down pages (`/Rotate 180`) keep their size, so they
//! are taken for upright ones.
//!
//! Pages whose content is itself rotated to compensate (pdflscape's landscape pages) cannot be
//! mapped from SyncTeX data at all: SyncTeX does not record the content's rotation.

use std::{ptr, slice};

use glib::translate::ToGlibPtr;
use poppler::ffi;

use crate::SyncBox;

/// How a page is turned clockwise for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rotation {
    Upright,
    /// `/Rotate 90`: TeX's top edge is on the right.
    Quarter,
    /// `/Rotate 270`: TeX's top edge is on the left.
    ThreeQuarters,
}

impl Rotation {
    /// `b` (TeX's page space) on the page as displayed, whose size is `display` (bp).
    pub(crate) fn box_to_display(self, b: SyncBox, display: (f64, f64)) -> SyncBox {
        let (dw, dh) = (display.0 as f32, display.1 as f32);
        let (x, y, w, h) = (b.h, b.v_top, b.width, b.height);
        let (h_, v_top, width, height) = match self {
            Self::Upright => (x, y, w, h),
            Self::Quarter => (dw - y - h, x, h, w),
            Self::ThreeQuarters => (y, dh - x - w, h, w),
        };
        SyncBox {
            page: b.page,
            h: h_,
            v_top,
            width,
            height,
        }
    }

    /// The point `(x, y)` of the page as displayed, whose size is `display` (bp), in TeX's page
    /// space.
    pub(crate) fn point_from_display(self, (x, y): (f64, f64), display: (f64, f64)) -> (f64, f64) {
        let (dw, dh) = display;
        match self {
            Self::Upright => (x, y),
            Self::Quarter => (y, dw - x),
            Self::ThreeQuarters => (dh - y, x),
        }
    }
}

/// True when `page` (displayed at `size`, bp) is turned by a quarter: its size is its crop box
/// with width and height swapped. Square pages never are.
pub(crate) fn is_quarter_turned(page: &poppler::Page, size: (f64, f64)) -> bool {
    let mut crop = ffi::PopplerRectangle {
        x1: 0.0,
        y1: 0.0,
        x2: 0.0,
        y2: 0.0,
    };
    // SAFETY: `page` is a live PopplerPage and `crop` is a valid rectangle to write to.
    unsafe { ffi::poppler_page_get_crop_box(page.to_glib_none().0, &mut crop) };
    let (cw, ch) = ((crop.x2 - crop.x1).abs(), (crop.y2 - crop.y1).abs());
    let same = |a: f64, b: f64| (a - b).abs() < 1e-3;
    same(size.0, ch) && same(size.1, cw) && !(same(size.0, cw) && same(size.1, ch))
}

/// The quarter turn of a page for which [`is_quarter_turned`] holds: anticlockwise when most of
/// its text runs upwards on the displayed page, else clockwise.
pub(crate) fn quarter_turn(page: &poppler::Page) -> Rotation {
    let (down, up) = text_runs(page);
    if up > down {
        Rotation::ThreeQuarters
    } else {
        Rotation::Quarter
    }
}

/// How often consecutive characters of `page`'s text follow each other downwards and upwards
/// on the displayed page (neighbours only, so line breaks do not count).
fn text_runs(page: &poppler::Page) -> (usize, usize) {
    let mut rects: *mut ffi::PopplerRectangle = ptr::null_mut();
    let mut n = 0;
    // SAFETY: `page` is a live PopplerPage; the out-parameters are valid to write to.
    let ok =
        unsafe { ffi::poppler_page_get_text_layout(page.to_glib_none().0, &mut rects, &mut n) };
    if ok == glib::ffi::GFALSE || rects.is_null() {
        return (0, 0);
    }
    let (mut down, mut up) = (0, 0);
    {
        // SAFETY: on success poppler returns an array of `n` rectangles, which we own.
        let rects = unsafe { slice::from_raw_parts(rects, n as usize) };
        for pair in rects.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let center = |r: &ffi::PopplerRectangle| ((r.x1 + r.x2) / 2.0, (r.y1 + r.y2) / 2.0);
            let extent = |r: &ffi::PopplerRectangle| (r.x2 - r.x1).abs().max((r.y2 - r.y1).abs());
            let ((ax, ay), (bx, by)) = (center(a), center(b));
            let (dx, dy) = (bx - ax, by - ay);
            let near = dx.hypot(dy) <= 2.0 * extent(a).max(extent(b));
            if near && dy.abs() > dx.abs() {
                if dy > 0.0 {
                    down += 1;
                } else {
                    up += 1;
                }
            }
        }
    }
    // SAFETY: `rects` was allocated by poppler with g_malloc and is not used afterwards.
    unsafe { glib::ffi::g_free(rects.cast()) };
    (down, up)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: (f64, f64) = (612.0, 792.0);

    fn sync_box(h: f32, v_top: f32, width: f32, height: f32) -> SyncBox {
        SyncBox {
            page: 3,
            h,
            v_top,
            width,
            height,
        }
    }

    fn display(r: Rotation) -> (f64, f64) {
        match r {
            Rotation::Quarter | Rotation::ThreeQuarters => (LETTER.1, LETTER.0),
            Rotation::Upright => LETTER,
        }
    }

    #[test]
    fn boxes_turn_with_the_page() {
        let b = sync_box(72.0, 172.0, 300.0, 20.0);
        let turned = |r| Rotation::box_to_display(r, b, display(r));
        assert_eq!(turned(Rotation::Upright), b);
        // Clockwise: TeX's top edge is on the right of the 792 × 612 page.
        assert_eq!(
            turned(Rotation::Quarter),
            sync_box(600.0, 72.0, 20.0, 300.0)
        );
        assert_eq!(
            turned(Rotation::ThreeQuarters),
            sync_box(172.0, 240.0, 20.0, 300.0)
        );
        assert!(
            [Rotation::Quarter, Rotation::ThreeQuarters]
                .iter()
                .all(|&r| turned(r).page == 3)
        );
    }

    #[test]
    fn points_map_back_to_the_box_they_lie_in() {
        let b = sync_box(72.0, 172.0, 300.0, 20.0);
        for r in [
            Rotation::Upright,
            Rotation::Quarter,
            Rotation::ThreeQuarters,
        ] {
            let d = r.box_to_display(b, display(r));
            // The displayed box's top-left corner is one of TeX's box corners, and its centre
            // is the centre.
            let centre = (
                f64::from(d.h + d.width / 2.0),
                f64::from(d.v_top + d.height / 2.0),
            );
            assert_eq!(r.point_from_display(centre, display(r)), (222.0, 182.0));
            let corners = [(72.0, 172.0), (372.0, 172.0), (72.0, 192.0), (372.0, 192.0)];
            let corner = r.point_from_display((f64::from(d.h), f64::from(d.v_top)), display(r));
            assert!(corners.contains(&corner), "{r:?}: {corner:?}");
        }
    }
}
