//! Bitmap sizes, tiling and rasterising one page (or one tile of it) with poppler into a cairo image.
//!
//! A page too large for one bitmap is rendered in tiles, and every tile costs a pass of poppler
//! over the page's whole content stream (cairo clips the drawing to the tile). So the tiles are
//! full-width bands of a few MiB rather than small squares: a stress.pdf page at scale_q 26
//! (1989 × 2574 px, a fit-width page on a HiDPI pane about 1000 px wide) takes 23–29 ms in its 5
//! bands, 77–93 ms in the 24 tiles of 512² that tiled it before, and 10–12 ms whole
//! (`perf_tiled_page`). A page wider than [`MAX_TILE_WIDTH`] is zoomed in far, and its view shows
//! only part of its width, so it is cut into tiles of about 1024² (as many bytes as a band): in
//! the middle of a stress page at scale_q 96 (7344 × 9504 px), a 1800 × 2000 px view takes 9 of
//! them (33 MiB) and 31–34 ms, against 25 tiles of 512² (25 MiB) and 77–78 ms, and 16 bands up
//! to 4096 px wide (63 MiB, more than the view's default texture cache of 48 MiB) and 52 ms.

use crate::{PageKey, Rendered, ThumbReq, Thumbnail};

/// Pages whose full bitmap would be larger than this many bytes are rendered in tiles (see
/// [`tile_grid`]).
pub const TILE_THRESHOLD_BYTES: usize = 16 * 1024 * 1024;

/// The bitmap of one tile takes at most about this many bytes: a band is as many rows of the page
/// as fit in it (and all the bands of a page are equally tall).
pub const TILE_BYTES: usize = 4 * 1024 * 1024;

/// A tiled page at most this many device pixels wide is cut into full-width bands. A wider one
/// (zoomed far in) is cut into tiles of about 1024² instead: its view shows only part of its
/// width, and bands that wide would mostly lie beside the view.
pub const MAX_TILE_WIDTH: i32 = 4096;

/// The largest column width of a page wider than [`MAX_TILE_WIDTH`]: with [`TILE_BYTES`] a tile,
/// its tiles are about square.
const SQUARE_TILE: i32 = 1024;

/// cairo refuses image surfaces wider or taller than this.
const MAX_SURFACE_DIM: i32 = 32_767;

/// Quantizes `zoom × device scale` to 1/8 steps, as stored in [`PageKey::scale_q`] (at least 1).
pub fn scale_q(zoom: f64, device_scale: f64) -> u16 {
    let q = (zoom * device_scale * 8.0).round();
    if q.is_nan() {
        return 8;
    }
    q.clamp(1.0, f64::from(u16::MAX)) as u16
}

/// The render scale (device pixels per bp) that `scale_q` stands for.
pub fn scale_of(scale_q: u16) -> f64 {
    f64::from(scale_q) / 8.0
}

/// Size in device pixels of the full bitmap of a page of `size_bp` at `scale_q` (each side at least 1).
pub fn page_pixels(size_bp: (f64, f64), scale_q: u16) -> (i32, i32) {
    let s = scale_of(scale_q);
    let px = |bp: f64| (bp * s).ceil().clamp(1.0, f64::from(i32::MAX)) as i32;
    (px(size_bp.0), px(size_bp.1))
}

/// How a page bitmap is cut: `columns × rows` tiles of `tile` device pixels, the last column and
/// row cut to the page.
struct Grid {
    page: (i32, i32),
    tile: (i32, i32),
    columns: u16,
    rows: u16,
}

/// The tiles of a page of `size_bp` at `scale_q`. They exist at every scale (an explicit tile of
/// any page can be rendered), but only the pages of [`must_tile`] are rendered in tiles.
fn grid(size_bp: (f64, f64), scale_q: u16) -> Grid {
    let (w, h) = page_pixels(size_bp, scale_q);
    // In i64: a side of a huge page is clamped to i32::MAX pixels, so rounding up would overflow
    // i32. More than u16::MAX tiles per side are cut to u16::MAX.
    let div_ceil = |a: i64, b: i64| (a + b - 1) / b;
    let (w64, h64) = (i64::from(w), i64::from(h));
    let tile_w = if w <= MAX_TILE_WIDTH {
        w64
    } else {
        div_ceil(w64, div_ceil(w64, i64::from(SQUARE_TILE)))
    };
    let band_rows = (TILE_BYTES as i64 / (4 * tile_w)).clamp(1, i64::from(MAX_SURFACE_DIM));
    let tile_h = div_ceil(h64, div_ceil(h64, band_rows));
    let count = |px: i64, tile: i64| u16::try_from(div_ceil(px, tile)).unwrap_or(u16::MAX);
    Grid {
        page: (w, h),
        // Both at most MAX_TILE_WIDTH or MAX_SURFACE_DIM.
        tile: (tile_w as i32, tile_h as i32),
        columns: count(w64, tile_w),
        rows: count(h64, tile_h),
    }
}

/// True when a page of `size_bp` at `scale_q` is too large for one bitmap: it would exceed
/// [`TILE_THRESHOLD_BYTES`] or cairo's size limit.
fn must_tile(size_bp: (f64, f64), scale_q: u16) -> bool {
    let (w, h) = page_pixels(size_bp, scale_q);
    let bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    bytes > TILE_THRESHOLD_BYTES || w > MAX_SURFACE_DIM || h > MAX_SURFACE_DIM
}

/// `(columns, rows)` of the tile grid when a page of `size_bp` at `scale_q` must be tiled: its
/// bitmap would exceed [`TILE_THRESHOLD_BYTES`] or cairo's size limit. `None` means one bitmap for
/// the page.
///
/// The tiles are full-width bands of at most about [`TILE_BYTES`] (one column), unless the page
/// is wider than [`MAX_TILE_WIDTH`]; see [`tile_size`].
pub fn tile_grid(size_bp: (f64, f64), scale_q: u16) -> Option<(u16, u16)> {
    must_tile(size_bp, scale_q).then(|| {
        let g = grid(size_bp, scale_q);
        (g.columns, g.rows)
    })
}

/// `(width, height)` in device pixels of the tiles of a page of `size_bp` at `scale_q` that must
/// be tiled (the last column and row are cut to the page); `None` when it is one bitmap.
pub fn tile_size(size_bp: (f64, f64), scale_q: u16) -> Option<(i32, i32)> {
    must_tile(size_bp, scale_q).then(|| grid(size_bp, scale_q).tile)
}

/// The tiles of a page of `size_bp` that must be tiled at `scale_q` which cover `(x, y, width,
/// height)` of its bitmap (device pixels, at least one pixel inside the page), row by row; `None`
/// when the page is one bitmap at this scale.
pub fn tiles_covering(
    size_bp: (f64, f64),
    scale_q: u16,
    (x, y, width, height): (i32, i32, i32, i32),
) -> Option<Vec<(u16, u16)>> {
    if !must_tile(size_bp, scale_q) {
        return None;
    }
    let g = grid(size_bp, scale_q);
    let (tile_w, tile_h) = g.tile;
    // The tiles under the first and the last pixel of each side, within the grid (in i64, for
    // the far end of a huge page).
    let span = |from: i32, len: i32, tile: i32, count: u16| {
        let (from, tile, end) = (
            i64::from(from.max(0)),
            i64::from(tile),
            i64::from(count) - 1,
        );
        let first = (from / tile).min(end);
        let last = ((from + i64::from(len.max(1)) - 1) / tile).min(end);
        first as u16..=last as u16
    };
    let columns = span(x, width, tile_w, g.columns);
    let rows = span(y, height, tile_h, g.rows);
    Some(
        rows.flat_map(|row| columns.clone().map(move |column| (column, row)))
            .collect(),
    )
}

/// `(x, y, width, height)` in device pixels of `tile = (column, row)` within the full page bitmap. Edge
/// tiles are cut to the page; a tile outside the page has zero width or height. Pages that are not
/// tiled at `scale_q` have tiles of the same shape, should one be asked for.
pub fn tile_rect(size_bp: (f64, f64), scale_q: u16, tile: (u16, u16)) -> (i32, i32, i32, i32) {
    let Grid {
        page: (w, h),
        tile: (tile_w, tile_h),
        ..
    } = grid(size_bp, scale_q);
    // In i64, then clamped: for a tile far beyond a huge page.
    let x = (i64::from(tile.0) * i64::from(tile_w)).min(i64::from(w)) as i32;
    let y = (i64::from(tile.1) * i64::from(tile_h)).min(i64::from(h)) as i32;
    (x, y, (w - x).min(tile_w), (h - y).min(tile_h))
}

/// Why a page could not be rasterised.
#[derive(Debug)]
pub(crate) enum RenderError {
    Cairo(cairo::Error),
    Borrow(cairo::BorrowError),
    /// The requested tile lies outside the page.
    EmptyTile,
}

impl From<cairo::Error> for RenderError {
    fn from(e: cairo::Error) -> Self {
        Self::Cairo(e)
    }
}

impl From<cairo::BorrowError> for RenderError {
    fn from(e: cairo::BorrowError) -> Self {
        Self::Borrow(e)
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cairo(e) => write!(f, "cairo: {e}"),
            Self::Borrow(e) => write!(f, "cairo surface: {e}"),
            Self::EmptyTile => f.write_str("tile outside the page"),
        }
    }
}

/// Renders `key` (the whole page, or the tile `key.tile`) of `page` on a white background into an
/// ARGB32 image (B8g8r8a8 premultiplied on little-endian), coloured for dark mode when `key.dark`.
pub(crate) fn render(page: &poppler::Page, key: PageKey) -> Result<Rendered, RenderError> {
    let size = page.size();
    let (x, y, width, height) = match key.tile {
        None => {
            let (w, h) = page_pixels(size, key.scale_q);
            (0, 0, w, h)
        }
        Some(tile) => tile_rect(size, key.scale_q, tile),
    };
    if width == 0 || height == 0 {
        return Err(RenderError::EmptyTile);
    }
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint()?;
        cr.translate(-f64::from(x), -f64::from(y));
        let s = scale_of(key.scale_q);
        cr.scale(s, s);
        page.render(&cr);
        cr.status()?;
    }
    let stride = surface.stride();
    // The context is gone, so the surface is uniquely owned and neither data nor take_data can fail
    // with NonExclusive.
    if key.dark {
        crate::dark::darken_bgra(&mut surface.data()?);
    }
    let data = surface.take_data()?;
    Ok(Rendered {
        key,
        width,
        height,
        stride,
        data,
    })
}

/// The height in device pixels of `page` rendered `width` pixels wide (at least 1).
pub(crate) fn height_for_width(size_bp: (f64, f64), width: u16) -> i32 {
    if size_bp.0 <= 0.0 {
        return 1;
    }
    let h = (size_bp.1 * f64::from(width) / size_bp.0).round();
    h.clamp(1.0, f64::from(MAX_SURFACE_DIM)) as i32
}

/// Renders the whole `page` `req.width` pixels wide (thumbnails, link previews), on a white
/// background like [`render`], coloured for dark mode when `req.dark`.
pub(crate) fn render_thumbnail(
    page: &poppler::Page,
    req: ThumbReq,
) -> Result<Thumbnail, RenderError> {
    let size = page.size();
    let width = i32::from(req.width.max(1));
    let height = height_for_width(size, req.width.max(1));
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint()?;
        if size.0 > 0.0 && size.1 > 0.0 {
            cr.scale(f64::from(width) / size.0, f64::from(height) / size.1);
            page.render(&cr);
        }
        cr.status()?;
    }
    let stride = surface.stride();
    if req.dark {
        crate::dark::darken_bgra(&mut surface.data()?);
    }
    let data = surface.take_data()?;
    Ok(Thumbnail {
        req,
        width,
        height,
        stride,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: (f64, f64) = (612.0, 792.0);

    #[test]
    fn scale_quantization() {
        assert_eq!(scale_q(1.0, 2.0), 16);
        assert_eq!(scale_q(1.3, 1.0), 10);
        assert_eq!(scale_q(0.0, 1.0), 1);
        assert_eq!(scale_q(1e9, 1.0), u16::MAX);
        assert_eq!(scale_q(f64::NAN, 1.0), 8);
        assert_eq!(scale_of(12), 1.5);
    }

    #[test]
    fn pixel_sizes_round_up() {
        assert_eq!(page_pixels(LETTER, 16), (1224, 1584));
        assert_eq!(page_pixels((100.1, 0.0), 8), (101, 1));
    }

    #[test]
    fn tiling_starts_above_16_mib() {
        // 2x letter is 7.76 MB, 3x is 17.4 MB: five full-width bands of 476 rows (3.5 MB).
        assert_eq!(tile_grid(LETTER, 16), None);
        assert_eq!(tile_size(LETTER, 16), None);
        assert_eq!(tile_grid(LETTER, 24), Some((1, 5)));
        assert_eq!(tile_size(LETTER, 24), Some((1836, 476)));
        // A very tall but narrow page exceeds cairo's dimension limit before the byte threshold.
        assert_eq!(tile_grid((10.0, 40_000.0), 8), Some((1, 2)));
        assert_eq!(tile_size((10.0, 40_000.0), 8), Some((10, 20_000)));
    }

    #[test]
    fn tiles_are_full_width_bands_within_the_byte_bound() {
        // Letter from just over the threshold to the view's largest zoom on a HiDPI display.
        for q in 24..=128 {
            let (w, h) = page_pixels(LETTER, q);
            let (columns, rows) = tile_grid(LETTER, q).unwrap();
            let (tile_w, tile_h) = tile_size(LETTER, q).unwrap();
            assert!(tile_w * tile_h * 4 <= TILE_BYTES as i32, "q {q}");
            assert!(tile_w <= MAX_TILE_WIDTH, "q {q}");
            if w <= MAX_TILE_WIDTH {
                assert_eq!((columns, tile_w), (1, w), "q {q}: one column");
            } else {
                // Equal columns of about square tiles: the last one is at most one pixel
                // narrower per column.
                assert!(tile_w <= SQUARE_TILE, "q {q}");
                assert!(
                    i32::from(columns) * tile_w - w < i32::from(columns),
                    "q {q}"
                );
            }
            // Equally tall rows, the last one cut to the page.
            assert!(i32::from(rows) * tile_h >= h, "q {q}");
            assert!(i32::from(rows) * tile_h - h < i32::from(rows), "q {q}");
        }
        // A fit-width page on a HiDPI pane about 1000 px wide: 5 bands instead of 24 tiles.
        assert_eq!(tile_grid(LETTER, 26), Some((1, 5)));
        // Bands up to 4096 px wide (4055 × 5247 px at scale_q 53), then about square tiles:
        // 4131 × 5346 px at scale_q 54 in 5 × 5 tiles of 827 × 1070.
        assert_eq!(tile_size(LETTER, 53), Some((4055, 250)));
        assert_eq!(tile_grid(LETTER, 54), Some((5, 5)));
        assert_eq!(tile_size(LETTER, 54), Some((827, 1070)));
        // Zoomed far in (9180 × 11880 px): 9 × 12 tiles of 1020 × 990.
        assert_eq!(tile_grid(LETTER, 120), Some((9, 12)));
        assert_eq!(tile_size(LETTER, 120), Some((1020, 990)));
    }

    #[test]
    fn huge_pages_keep_their_tiles_addressable() {
        // A side clamped to i32::MAX pixels: columns of SQUARE_TILE, cut to u16::MAX of them.
        assert_eq!(tile_grid((3e9, 100.0), 8), Some((u16::MAX, 1)));
        assert_eq!(tile_size((3e9, 100.0), 8), Some((SQUARE_TILE, 100)));
        let last = tile_rect((3e9, 100.0), 8, (u16::MAX - 1, 0));
        assert_eq!(last, (65_534 * SQUARE_TILE, 0, SQUARE_TILE, 100));
        // A 2e7 bp page at scale 1/8: 2442 × 2442 tiles of 1024².
        let (w, h) = page_pixels((2e7, 2e7), 1);
        let (columns, rows) = tile_grid((2e7, 2e7), 1).unwrap();
        let (tile_w, tile_h) = tile_size((2e7, 2e7), 1).unwrap();
        assert_eq!((columns, rows), (2442, 2442));
        assert!(i64::from(columns) * i64::from(tile_w) >= i64::from(w));
        assert!(i64::from(rows) * i64::from(tile_h) >= i64::from(h));
    }

    #[test]
    fn the_tiles_covering_a_region() {
        // Five bands of 476 rows.
        assert_eq!(tiles_covering(LETTER, 16, (0, 0, 10, 10)), None);
        assert_eq!(
            tiles_covering(LETTER, 24, (100, 400, 300, 200)),
            Some(vec![(0, 0), (0, 1)])
        );
        assert_eq!(
            tiles_covering(LETTER, 24, (0, 476, 1836, 476)),
            Some(vec![(0, 1)])
        );
        // Clamped to the grid.
        assert_eq!(
            tiles_covering(LETTER, 24, (-5, 2300, 5000, 5000)),
            Some(vec![(0, 4)])
        );
        // Several columns: tiles of 1020 × 990 px.
        assert_eq!(
            tiles_covering(LETTER, 120, (3000, 980, 100, 20)),
            Some(vec![(2, 0), (3, 0), (2, 1), (3, 1)])
        );
        // Every pixel of a region lies in one of its tiles.
        let region = (1234, 567, 2345, 1678);
        let tiles = tiles_covering(LETTER, 120, region).unwrap();
        for (x, y) in [(1234, 567), (3578, 2244), (3060, 990), (3059, 989)] {
            assert!(
                tiles.iter().any(|&t| {
                    let (tx, ty, tw, th) = tile_rect(LETTER, 120, t);
                    (tx..tx + tw).contains(&x) && (ty..ty + th).contains(&y)
                }),
                "({x}, {y}) in {tiles:?}"
            );
        }
    }

    #[test]
    fn an_untiled_page_has_tiles_of_the_same_shape() {
        // 1224 × 1584 px: two bands of 792 rows, were it tiled.
        assert_eq!(tile_rect(LETTER, 16, (0, 0)), (0, 0, 1224, 792));
        assert_eq!(tile_rect(LETTER, 16, (0, 1)), (0, 792, 1224, 792));
        assert_eq!(tile_rect(LETTER, 16, (0, 2)).3, 0);
        assert_eq!(tile_rect(LETTER, 16, (1, 0)).2, 0);
    }

    #[test]
    fn thumbnail_heights_follow_the_aspect() {
        assert_eq!(height_for_width(LETTER, 153), 198);
        assert_eq!(height_for_width((792.0, 612.0), 100), 77);
        assert_eq!(height_for_width((0.0, 612.0), 100), 1);
        assert_eq!(height_for_width((10.0, 1e9), 100), MAX_SURFACE_DIM);
    }

    #[test]
    fn tile_rects_cover_the_page() {
        for q in [24, 26, 64, 120] {
            let (w, h) = page_pixels(LETTER, q);
            let (cols, rows) = tile_grid(LETTER, q).unwrap();
            let (size_w, size_h) = tile_size(LETTER, q).unwrap();
            let mut area = 0i64;
            for c in 0..cols {
                for r in 0..rows {
                    let (x, y, tw, th) = tile_rect(LETTER, q, (c, r));
                    assert_eq!((x, y), (i32::from(c) * size_w, i32::from(r) * size_h));
                    assert!(tw > 0 && tw <= size_w && th > 0 && th <= size_h);
                    assert!(x + tw <= w && y + th <= h);
                    area += i64::from(tw) * i64::from(th);
                }
            }
            assert_eq!(area, i64::from(w) * i64::from(h), "q {q}");
            assert_eq!(tile_rect(LETTER, q, (cols, 0)).2, 0);
            assert_eq!(tile_rect(LETTER, q, (0, rows)).3, 0);
        }
    }
}
