//! Pure geometry and colour helpers of the PDF view: render order, zoom anchoring, tile
//! visibility and the dark-mode colour transforms. No GTK here, so everything is unit-tested.

use lemma_pdf::dark::{HUE_180, INK as DARK_INK, PAPER as DARK_PAPER};
use lemma_pdf::{PageLayout, scale_of, tile_grid, tile_rect, tile_size};
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

/// The tile grid of a page tiled at `scale_q` in logical pixels, for a page drawn at `page_rect`:
/// `(columns, rows)` and the width and height of a tile (see `tile_view_rect`). `None` when the page
/// is not tiled at this scale, or has no size.
fn tile_edges(
    size_bp: (f64, f64),
    scale_q: u16,
    page_rect: Rect,
) -> Option<((u16, u16), (f64, f64))> {
    let grid = tile_grid(size_bp, scale_q)?;
    let (tile_w, tile_h) = tile_size(size_bp, scale_q)?;
    // Logical pixels per device pixel of the bitmap.
    let k = if size_bp.0 > 0.0 {
        page_rect.width / size_bp.0 / scale_of(scale_q)
    } else {
        0.0
    };
    let edges = (f64::from(tile_w) * k, f64::from(tile_h) * k);
    let valid = |edge: f64| edge.is_finite() && edge > 0.0;
    (valid(edges.0) && valid(edges.1)).then_some((grid, edges))
}

/// The tiles from `origin` on (at most `count`, each `edge` long) that the span `from..to` can
/// touch.
fn tile_span(from: f64, to: f64, origin: f64, edge: f64, count: u16) -> Range<u16> {
    let first = ((from - origin) / edge).floor().max(0.0);
    let end = ((to - origin) / edge).ceil().min(f64::from(count));
    if end > first {
        first as u16..end as u16
    } else {
        0..0
    }
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
    let Some(((columns, rows), (edge_x, edge_y))) = tile_edges(size_bp, scale_q, page_rect) else {
        return Vec::new();
    };
    let column_span = tile_span(
        viewport.x,
        viewport.x + viewport.width,
        page_rect.x,
        edge_x,
        columns,
    );
    let row_span = tile_span(
        viewport.y,
        viewport.y + viewport.height,
        page_rect.y,
        edge_y,
        rows,
    );
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

/// The row of tiles that scrolling down brings into view next, of a page that must be tiled at
/// `scale_q`: the first row below `viewport` (its tiles under the viewport's columns; for a page
/// tiled in full-width bands, one band). For a page below the viewport, that is its first row.
/// Empty when the page's last row is in view or above it, or the page is not tiled at this scale.
pub fn tiles_below(
    size_bp: (f64, f64),
    scale_q: u16,
    page_rect: Rect,
    viewport: Rect,
) -> Vec<(u16, u16)> {
    let Some(((columns, rows), (edge_x, edge_y))) = tile_edges(size_bp, scale_q, page_rect) else {
        return Vec::new();
    };
    // The first row whose top edge is at or below the viewport's bottom edge (touching is not
    // overlapping, see `Rect::intersects`).
    let row = ((viewport.y + viewport.height - page_rect.y) / edge_y)
        .ceil()
        .max(0.0);
    if row >= f64::from(rows) {
        return Vec::new();
    }
    let row = row as u16;
    tile_span(
        viewport.x,
        viewport.x + viewport.width,
        page_rect.x,
        edge_x,
        columns,
    )
    .map(|column| (column, row))
    .filter(|&tile| {
        let rect = tile_view_rect(size_bp, scale_q, tile, page_rect);
        rect.x < viewport.x + viewport.width && viewport.x < rect.x + rect.width
    })
    .collect()
}

/// The colour transform of dark mode ([`lemma_pdf::dark`]), as the `(matrix, offset)` arguments of
/// `gtk::Snapshot::push_color_matrix` (GTK computes `pixel = transpose(matrix) · pixel + offset`
/// on unpremultiplied RGBA; graphene matrices are row-major).
///
/// It is CSS `invert(1) hue-rotate(180deg)`, which swaps light and dark but keeps hues (a red
/// link stays red), with the result mapped into [`DARK_PAPER`]..[`DARK_INK`] so paper becomes a
/// soft dark grey and text a soft light grey. Alpha is unchanged. The PDF thread colours the dark
/// bitmaps of the view the same way; the matrix is for what is drawn from light bitmaps.
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

/// The inverse of [`dark_color_transform`] (same conventions): draws a dark bitmap as the light
/// page, while the view waits for the light bitmap after dark mode was turned off.
///
/// `in = (DARK_INK − H · out) / contrast`, because the hue rotation `H` by 180° is its own inverse
/// and its rows sum to 1.
pub fn light_color_transform() -> ([f32; 16], [f32; 4]) {
    let contrast = DARK_INK - DARK_PAPER;
    let mut matrix = [0.0; 16];
    for (i, row) in HUE_180.iter().enumerate() {
        for (j, h) in row.iter().enumerate() {
            matrix[4 * j + i] = -h / contrast;
        }
    }
    matrix[15] = 1.0;
    let offset = DARK_INK / contrast;
    (matrix, [offset, offset, offset, 0.0])
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

    /// Scale 4 (zoom 2 at device scale 2): an A4 page is 2382×3368 px, 8 full-width bands of
    /// 421 rows, 210.5 logical px each.
    const BANDS_Q: u16 = 32;
    fn banded_page() -> Rect {
        Rect::new(100.0, 50.0, A4.0 * 2.0, A4.1 * 2.0)
    }
    /// Scale 15 (zoom 7.5 at device scale 2): 8930×12629 px, wider than a band can be, so 9 × 12
    /// tiles of 993 × 1053 px, that is 496.5 × 526.5 logical px.
    const COLUMNS_Q: u16 = 120;
    fn wide_page() -> Rect {
        Rect::new(12.0, 12.0, A4.0 * 7.5, A4.1 * 7.5)
    }

    #[test]
    fn bands_cover_the_page_and_follow_the_viewport() {
        let (q, page) = (BANDS_Q, banded_page());
        let all = visible_tiles(A4, q, page, page);
        assert_eq!(all.len(), 8);
        assert!(all.iter().all(|&(column, _)| column == 0), "{all:?}");
        let area: f64 = all
            .iter()
            .map(|&t| {
                let r = tile_view_rect(A4, q, t, page);
                assert!(close(r.width, page.width, 1.0), "full width: {r:?}");
                r.width * r.height
            })
            .sum();
        assert!(close(area, page.width * page.height, page.width * 2.0));

        // A viewport over the top of the page sees the first two bands, nearest first.
        let viewport = Rect::new(100.0, 50.0, 300.0, 300.0);
        assert_eq!(visible_tiles(A4, q, page, viewport), vec![(0, 0), (0, 1)]);
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
        for (q, page) in [(BANDS_Q, banded_page()), (COLUMNS_Q, wide_page())] {
            let mut viewports = vec![page];
            for (x, y) in [(0.0, 0.0), (100.0, 50.0), (356.0, 306.0), (900.0, 1500.0)] {
                for (w, h) in [(300.0, 200.0), (256.0, 256.0), (1.0, 1.0), (2000.0, 4000.0)] {
                    viewports.push(Rect::new(x, y, w, h));
                }
            }
            // Beside and below the page.
            viewports.push(Rect::new(page.x + page.width, 0.0, 300.0, 300.0));
            viewports.push(Rect::new(0.0, page.y + page.height, 300.0, 300.0));
            // Edges exactly on tile boundaries only touch: bands 1 and 2, and on the wide page
            // the columns 1 and 2 of rows 1 and 2.
            viewports.push(Rect::new(100.0, 260.5, 300.0, 421.0));
            viewports.push(Rect::new(508.5, 538.5, 993.0, 1053.0));
            for viewport in viewports {
                let mut tiles = visible_tiles(A4, q, page, viewport);
                tiles.sort();
                assert_eq!(tiles, scan_tiles(A4, q, page, viewport), "{viewport:?}");
            }
        }
        let boundaries = |q, page, viewport| {
            let mut tiles = visible_tiles(A4, q, page, viewport);
            tiles.sort();
            tiles
        };
        assert_eq!(
            boundaries(
                BANDS_Q,
                banded_page(),
                Rect::new(100.0, 260.5, 300.0, 421.0)
            ),
            vec![(0, 1), (0, 2)]
        );
        let wide = boundaries(
            COLUMNS_Q,
            wide_page(),
            Rect::new(508.5, 538.5, 993.0, 1053.0),
        );
        assert_eq!(wide, vec![(1, 1), (1, 2), (2, 1), (2, 2)]);
    }

    #[test]
    fn the_band_below_the_viewport_comes_next() {
        let (q, page) = (BANDS_Q, banded_page());
        let at = |y: f64, height: f64| Rect::new(0.0, y, 1400.0, height);
        // Bands 0 to 2 in view: band 3 is next; a bottom edge on a boundary only touches.
        assert_eq!(tiles_below(A4, q, page, at(0.0, 500.0)), vec![(0, 3)]);
        assert_eq!(tiles_below(A4, q, page, at(0.0, 471.0)), vec![(0, 2)]);
        // A page below the viewport: its first band.
        assert_eq!(tiles_below(A4, q, page, at(-500.0, 400.0)), vec![(0, 0)]);
        // Its last band in view, or the page above the viewport: nothing more on this page.
        assert!(tiles_below(A4, q, page, at(1500.0, 400.0)).is_empty());
        assert!(tiles_below(A4, q, page, at(5000.0, 400.0)).is_empty());
        // Not tiled at this scale.
        assert!(tiles_below(A4, 8, page, at(0.0, 500.0)).is_empty());
        // Several columns: those under the viewport.
        let viewport = Rect::new(1400.0, 1000.0, 900.0, 600.0);
        assert_eq!(
            tiles_below(A4, COLUMNS_Q, wide_page(), viewport),
            vec![(2, 4), (3, 4), (4, 4)]
        );
        // Beside the page.
        let beside = Rect::new(-1000.0, 0.0, 500.0, 500.0);
        assert!(tiles_below(A4, q, page, beside).is_empty());
    }

    #[test]
    fn visible_tiles_of_a_huge_page_are_found_without_scanning_it() {
        // A 2e7 bp page at the smallest zoom: 2442 × 2442 tiles, of which a 800 × 700 view
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
    fn the_light_transform_undoes_the_dark_one() {
        let (dark, light) = (dark_color_transform(), light_color_transform());
        for c in [
            [1.0, 1.0, 1.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.8, 0.1, 0.1, 1.0],
            [0.1, 0.3, 0.9, 0.5],
            [0.5, 0.5, 0.5, 1.0],
        ] {
            let back = apply(&light, apply(&dark, c));
            for i in 0..4 {
                assert!((back[i] - c[i]).abs() < 2e-3, "{c:?} -> {back:?}");
            }
        }
        // Matches the bitmaps the PDF thread colours (both clamp to the colour range).
        for c in [[0.8, 0.1, 0.1], [0.2, 0.6, 0.3], [1.0, 1.0, 1.0]] {
            let rgb = lemma_pdf::dark::darken_rgb(c);
            let m = apply(&dark, [c[0], c[1], c[2], 1.0]);
            for i in 0..3 {
                assert!((rgb[i] - m[i].clamp(0.0, 1.0)).abs() < 1e-6, "{c:?}");
            }
        }
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
