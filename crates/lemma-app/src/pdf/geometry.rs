//! Pure geometry and colour helpers of the PDF view: render order, zoom anchoring, tile
//! visibility and the dark-mode colour transform. No GTK here, so everything is unit-tested.

use lemma_pdf::{PageLayout, TILE_SIZE, scale_of, tile_grid, tile_rect};
use std::ops::Range;

/// The smallest zoom the view allows (1.0 = 1 bp per logical pixel).
pub const MIN_ZOOM: f64 = 0.1;
/// The largest zoom the view allows.
pub const MAX_ZOOM: f64 = 8.0;

/// `zoom` limited to [`MIN_ZOOM`]..=[`MAX_ZOOM`]; 1.0 for NaN or infinities.
pub fn clamp_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// An axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// True when the two rectangles overlap in an area (touching edges do not count).
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// True when `other` lies completely inside `self`.
    pub fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }

    fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// Pages to render, most urgent first: the visible pages from `center` outward (the next page
/// before the previous one at each distance), then the pages just after and just before the
/// visible range as prefetch.
pub fn render_order(visible: Range<u32>, center: u32, page_count: u32) -> Vec<u32> {
    let visible = visible.start.min(page_count)..visible.end.min(page_count);
    if visible.is_empty() {
        return Vec::new();
    }
    let center = center.clamp(visible.start, visible.end - 1);
    let mut order = vec![center];
    for distance in 1.. {
        let after = center.checked_add(distance).filter(|p| *p < visible.end);
        let before = center.checked_sub(distance).filter(|p| *p >= visible.start);
        if after.is_none() && before.is_none() {
            break;
        }
        order.extend(after);
        order.extend(before);
    }
    if visible.end < page_count {
        order.push(visible.end);
    }
    if visible.start > 0 {
        order.push(visible.start - 1);
    }
    order
}

/// The scroll offsets that keep the document point under `pointer` (widget coordinates) still
/// when the zoom changes from `old_zoom` to `new_zoom`.
///
/// `view_width` is the widget width (pages are centred in it), `scroll` the current scroll
/// offsets. The point is tracked in the coordinates of the page it lies on (or the page below
/// the gap it lies in), so it stays put although the gaps between pages do not scale. The result
/// is not clamped to the scrollable range; the adjustments do that.
pub fn anchored_scroll(
    layout: &PageLayout,
    old_zoom: f64,
    new_zoom: f64,
    view_width: f64,
    scroll: (f64, f64),
    pointer: (f64, f64),
) -> (f64, f64) {
    if layout.page_count() == 0 || old_zoom <= 0.0 {
        return scroll;
    }
    let (doc_x, doc_y) = (scroll.0 + pointer.0, scroll.1 + pointer.1);
    let page = layout.anchor_at(doc_y, doc_x, old_zoom).page;
    let (page_x, page_y, _, _) = layout.page_rect(page, old_zoom, view_width);
    let x_bp = (doc_x - page_x) / old_zoom;
    let y_bp = (doc_y - page_y) / old_zoom;
    let (new_x, new_y) = layout.to_view(page, x_bp, y_bp, new_zoom, view_width);
    (new_x - pointer.0, new_y - pointer.1)
}

/// Where in the view (as a fraction of its height) the "current page" is read: 0 at the top of
/// the document, 1 at its end, proportional to the scroll position in between.
pub fn probe_fraction(scroll_y: f64, view_height: f64, content_height: f64) -> f64 {
    let range = content_height - view_height;
    if range <= 0.0 {
        0.0
    } else {
        (scroll_y / range).clamp(0.0, 1.0)
    }
}

/// The rectangle of `tile` of a page bitmap rendered at `scale_q`, in the coordinates of
/// `page_rect` (the page's rectangle at the current zoom). Works for any `scale_q`, so tiles of an
/// older scale are stretched to the right place.
pub fn tile_view_rect(
    size_bp: (f64, f64),
    scale_q: u16,
    tile: (u16, u16),
    page_rect: Rect,
) -> Rect {
    let (x, y, w, h) = tile_rect(size_bp, scale_q, tile);
    // Logical pixels per device pixel of the bitmap.
    let k = if size_bp.0 > 0.0 {
        page_rect.width / size_bp.0 / scale_of(scale_q)
    } else {
        0.0
    };
    Rect::new(
        page_rect.x + f64::from(x) * k,
        page_rect.y + f64::from(y) * k,
        f64::from(w) * k,
        f64::from(h) * k,
    )
}

/// The tiles of a page that must be tiled at `scale_q` and intersect `viewport`, nearest to the
/// viewport's centre first. Empty when the page is not tiled at this scale.
///
/// Only the columns and rows under the viewport are looked at, so the work is bounded by the
/// viewport's size in tiles, however large the page.
pub fn visible_tiles(
    size_bp: (f64, f64),
    scale_q: u16,
    page_rect: Rect,
    viewport: Rect,
) -> Vec<(u16, u16)> {
    let Some((columns, rows)) = tile_grid(size_bp, scale_q) else {
        return Vec::new();
    };
    // The edge of a tile in logical pixels (see `tile_view_rect`).
    let edge = if size_bp.0 > 0.0 {
        f64::from(TILE_SIZE) * page_rect.width / size_bp.0 / scale_of(scale_q)
    } else {
        0.0
    };
    if !(edge.is_finite() && edge > 0.0) {
        return Vec::new();
    }
    // The tiles from `origin` on (at most `count`) that the span `from..to` can touch.
    let span = |from: f64, to: f64, origin: f64, count: u16| -> Range<u16> {
        let first = ((from - origin) / edge).floor().max(0.0);
        let end = ((to - origin) / edge).ceil().min(f64::from(count));
        if end > first {
            first as u16..end as u16
        } else {
            0..0
        }
    };
    let column_span = span(
        viewport.x,
        viewport.x + viewport.width,
        page_rect.x,
        columns,
    );
    let row_span = span(viewport.y, viewport.y + viewport.height, page_rect.y, rows);
    let (cx, cy) = viewport.center();
    let mut tiles: Vec<((u16, u16), f64)> = row_span
        .flat_map(|row| column_span.clone().map(move |column| (column, row)))
        .filter_map(|tile| {
            let rect = tile_view_rect(size_bp, scale_q, tile, page_rect);
            let (tx, ty) = rect.center();
            rect.intersects(&viewport)
                .then(|| (tile, (tx - cx).powi(2) + (ty - cy).powi(2)))
        })
        .collect();
    tiles.sort_by(|a, b| a.1.total_cmp(&b.1));
    tiles.into_iter().map(|(tile, _)| tile).collect()
}

/// Lightness of white paper in dark mode (a soft dark grey instead of pure black).
const DARK_PAPER: f32 = 0.16;
/// Lightness of black ink in dark mode (a soft light grey instead of pure white).
const DARK_INK: f32 = 0.92;

/// The 3×3 hue rotation by 180° of CSS `hue-rotate(180deg)` (Filter Effects spec), row-major:
/// `out[i] = Σ_j HUE_180[i][j] · in[j]`. Its rows sum to 1, so grey stays grey.
const HUE_180: [[f32; 3]; 3] = [
    [-0.574, 1.430, 0.144],
    [0.426, 0.430, 0.144],
    [0.426, 1.430, -0.856],
];

/// The colour transform of dark mode, as the `(matrix, offset)` arguments of
/// `gtk::Snapshot::push_color_matrix` (GTK computes `pixel = transpose(matrix) · pixel + offset`
/// on unpremultiplied RGBA; graphene matrices are row-major).
///
/// It is CSS `invert(1) hue-rotate(180deg)`, which swaps light and dark but keeps hues (a red
/// link stays red), with the result mapped into [`DARK_PAPER`]..[`DARK_INK`] so paper becomes a
/// soft dark grey and text a soft light grey. Alpha is unchanged.
pub fn dark_color_transform() -> ([f32; 16], [f32; 4]) {
    let contrast = DARK_INK - DARK_PAPER;
    let mut matrix = [0.0; 16];
    for (i, row) in HUE_180.iter().enumerate() {
        for (j, h) in row.iter().enumerate() {
            // out_i = DARK_INK - contrast · Σ_j H[i][j] in_j, stored transposed: m[j][i].
            matrix[4 * j + i] = -contrast * h;
        }
    }
    matrix[15] = 1.0;
    (matrix, [DARK_INK, DARK_INK, DARK_INK, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    const A4: (f64, f64) = (595.276, 841.89);

    fn layout(n: usize) -> PageLayout {
        PageLayout::new(vec![A4; n], 12.0)
    }

    /// Applies a colour transform the way GTK does: `transpose(m) · c + offset`.
    fn apply(transform: &([f32; 16], [f32; 4]), c: [f32; 4]) -> [f32; 4] {
        let (m, offset) = transform;
        let mut out = [0.0; 4];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (0..4).map(|j| m[4 * j + i] * c[j]).sum::<f32>() + offset[i];
        }
        out
    }

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn render_order_goes_from_the_centre_outward_then_prefetches() {
        assert_eq!(render_order(3..6, 4, 10), vec![4, 5, 3, 6, 2]);
        assert_eq!(render_order(3..6, 3, 10), vec![3, 4, 5, 6, 2]);
        assert_eq!(render_order(0..2, 0, 2), vec![0, 1]);
        assert_eq!(render_order(0..1, 0, 50), vec![0, 1]);
        // The centre is clamped into the visible range; empty and out-of-range inputs.
        assert_eq!(render_order(5..7, 9, 7), vec![6, 5, 4]);
        assert!(render_order(4..4, 4, 10).is_empty());
        assert!(render_order(12..15, 13, 10).is_empty());
    }

    #[test]
    fn anchored_zoom_keeps_the_point_under_the_pointer() {
        let l = layout(20);
        let view_width = 500.0;
        for &(old, new) in &[(0.8, 1.2), (1.0, 0.5), (2.0, 2.2), (0.3, 3.0)] {
            for &(scroll, pointer) in &[
                ((0.0, 0.0), (250.0, 300.0)),
                ((40.0, 5000.0), (10.0, 20.0)),
                ((0.0, 2600.0), (400.0, 590.0)),
            ] {
                let doc = (scroll.0 + pointer.0, scroll.1 + pointer.1);
                let (page, x_bp, y_bp) =
                    l.to_page(doc.0, doc.1, old, view_width)
                        .unwrap_or((u32::MAX, 0.0, 0.0));
                let new_scroll = anchored_scroll(&l, old, new, view_width, scroll, pointer);
                let new_doc = (new_scroll.0 + pointer.0, new_scroll.1 + pointer.1);
                if page == u32::MAX {
                    continue; // in a gap or margin: only pages have coordinates to compare
                }
                let (p2, x2, y2) = l
                    .to_page(new_doc.0, new_doc.1, new, view_width)
                    .expect("still on a page");
                assert_eq!(p2, page, "zoom {old}->{new}, pointer {pointer:?}");
                assert!(close(x2, x_bp, 1e-6) && close(y2, y_bp, 1e-6));
            }
        }
    }

    #[test]
    fn anchored_zoom_without_pages_keeps_the_scroll() {
        let empty = PageLayout::new(Vec::new(), 12.0);
        assert_eq!(
            anchored_scroll(&empty, 1.0, 2.0, 400.0, (3.0, 4.0), (5.0, 6.0)),
            (3.0, 4.0)
        );
    }

    #[test]
    fn tiles_cover_the_page_and_follow_the_viewport() {
        // At scale 4 an A4 page is 2382×3368 px: 5×7 tiles of 512 px.
        let q = 32;
        let page = Rect::new(100.0, 50.0, A4.0 * 2.0, A4.1 * 2.0); // zoom 2, device scale 2
        let all = visible_tiles(A4, q, page, page);
        assert_eq!(all.len(), 35);
        let area: f64 = all
            .iter()
            .map(|&t| {
                let r = tile_view_rect(A4, q, t, page);
                r.width * r.height
            })
            .sum();
        assert!(close(area, page.width * page.height, page.width * 2.0));

        // A viewport over the top-left corner sees only the first tiles, nearest first.
        let viewport = Rect::new(100.0, 50.0, 300.0, 200.0);
        let tiles = visible_tiles(A4, q, page, viewport);
        assert_eq!(tiles.first(), Some(&(0, 0)));
        assert!(tiles.iter().all(|&(c, r)| c <= 2 && r <= 1), "{tiles:?}");
        for t in &tiles {
            assert!(tile_view_rect(A4, q, *t, page).intersects(&viewport));
        }
        // Pages below the tiling threshold have no tiles.
        assert!(visible_tiles(A4, 8, page, page).is_empty());
    }

    /// Every tile of the grid that intersects the viewport, by brute force.
    fn scan_tiles(size: (f64, f64), q: u16, page: Rect, viewport: Rect) -> Vec<(u16, u16)> {
        let (columns, rows) = tile_grid(size, q).unwrap();
        let mut tiles: Vec<(u16, u16)> = (0..rows)
            .flat_map(|r| (0..columns).map(move |c| (c, r)))
            .filter(|&t| tile_view_rect(size, q, t, page).intersects(&viewport))
            .collect();
        tiles.sort();
        tiles
    }

    #[test]
    fn visible_tiles_match_a_scan_of_the_whole_grid() {
        let q = 32;
        let page = Rect::new(100.0, 50.0, A4.0 * 2.0, A4.1 * 2.0);
        let mut viewports = vec![page];
        for (x, y) in [(0.0, 0.0), (100.0, 50.0), (356.0, 306.0), (900.0, 1500.0)] {
            for (w, h) in [(300.0, 200.0), (256.0, 256.0), (1.0, 1.0), (2000.0, 4000.0)] {
                viewports.push(Rect::new(x, y, w, h));
            }
        }
        // Edges exactly on tile boundaries (tiles are 256 logical px here) only touch.
        viewports.push(Rect::new(356.0, 50.0, 256.0, 256.0));
        // Beside and below the page.
        viewports.push(Rect::new(2000.0, 0.0, 300.0, 300.0));
        viewports.push(Rect::new(0.0, 4000.0, 300.0, 300.0));
        for viewport in viewports {
            let mut tiles = visible_tiles(A4, q, page, viewport);
            tiles.sort();
            assert_eq!(tiles, scan_tiles(A4, q, page, viewport), "{viewport:?}");
        }
    }

    #[test]
    fn visible_tiles_of_a_huge_page_are_found_without_scanning_it() {
        // A 2e7 bp page at the smallest zoom: 4883 × 4883 tiles, of which a 800 × 700 view
        // sees a few.
        let size = (2e7, 2e7);
        let q = 1;
        let zoom = MIN_ZOOM;
        let page = Rect::new(12.0, 12.0, size.0 * zoom, size.1 * zoom);
        let viewport = Rect::new(1e6, 1e6, 800.0, 700.0);
        let started = std::time::Instant::now();
        let tiles = visible_tiles(size, q, page, viewport);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert!(!tiles.is_empty() && tiles.len() <= 9, "{tiles:?}");
        for t in &tiles {
            assert!(tile_view_rect(size, q, *t, page).intersects(&viewport));
        }
    }

    #[test]
    fn dark_mode_swaps_paper_and_ink_but_keeps_hues() {
        let t = dark_color_transform();
        let white = apply(&t, [1.0, 1.0, 1.0, 1.0]);
        let black = apply(&t, [0.0, 0.0, 0.0, 1.0]);
        for c in 0..3 {
            assert!((white[c] - DARK_PAPER).abs() < 1e-3, "{white:?}");
            assert!((black[c] - DARK_INK).abs() < 1e-3, "{black:?}");
        }
        assert_eq!(white[3], 1.0);
        let half = apply(&t, [1.0, 1.0, 1.0, 0.5]);
        assert_eq!(half[3], 0.5, "alpha is unchanged");
        // Red stays reddish: its red channel stays the largest.
        let red = apply(&t, [0.8, 0.1, 0.1, 1.0]);
        assert!(red[0] > red[1] && red[0] > red[2], "{red:?}");
        // A blue link stays bluish.
        let blue = apply(&t, [0.1, 0.1, 0.8, 1.0]);
        assert!(blue[2] > blue[0] && blue[2] > blue[1], "{blue:?}");
    }

    #[test]
    fn the_current_page_probe_moves_with_the_scroll_position() {
        assert_eq!(probe_fraction(0.0, 600.0, 3000.0), 0.0);
        assert_eq!(probe_fraction(2400.0, 600.0, 3000.0), 1.0);
        assert_eq!(probe_fraction(1200.0, 600.0, 3000.0), 0.5);
        assert_eq!(
            probe_fraction(0.0, 600.0, 400.0),
            0.0,
            "content shorter than the view"
        );
    }

    #[test]
    fn rectangles() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(a.intersects(&Rect::new(9.0, 9.0, 5.0, 5.0)));
        assert!(
            !a.intersects(&Rect::new(10.0, 0.0, 5.0, 5.0)),
            "edges only touch"
        );
        assert!(a.contains(&Rect::new(1.0, 1.0, 9.0, 9.0)));
        assert!(!a.contains(&Rect::new(1.0, 1.0, 10.0, 9.0)));
        assert_eq!(clamp_zoom(f64::NAN), 1.0);
        assert_eq!(clamp_zoom(100.0), MAX_ZOOM);
        assert_eq!(clamp_zoom(0.0), MIN_ZOOM);
    }
}
