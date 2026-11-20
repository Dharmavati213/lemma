//! Bitmap sizes, tiling and rasterising one page (or one tile of it) with poppler into a cairo image.
//!
//! A page too large for one bitmap is rendered in tiles, and every tile costs a pass of poppler
//! over the page's whole content stream (cairo clips the drawing to the tile). So the tiles are
//! full-width bands of a few MiB rather than small squares: a stress.pdf page at scale 3.25
//! (1989 × 2574 px, a fit-width page on a HiDPI pane about 1000 px wide) takes 23–29 ms in its 5
//! bands, 77–93 ms in the 24 tiles of 512² that tiled it before, and 10–12 ms whole
//! (`perf_tiled_page`). A page wider than [`MAX_TILE_WIDTH`] is zoomed in far, and its view shows
//! only part of its width, so it is cut into tiles of about 1024² (as many bytes as a band): in
//! the middle of a stress page at scale 12 (7344 × 9504 px), a 1800 × 2000 px view takes 9 of
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

/// [`PageKey::scale_q`] steps per unit of render scale. Fine enough that a page up to 1024 bp
/// rendered at the quantized scale has every point within half a device pixel of where the exact
/// zoom puts it, so the view can draw the bitmap pixel for pixel rather than resample it (which
/// blurs text); coarse enough that a `u16` reaches a scale of about 64 (zoom 8 at device scale 8).
pub const SCALE_STEPS: u16 = 1024;

/// Quantizes `zoom × device scale` to 1/[`SCALE_STEPS`] steps, as stored in
/// [`PageKey::scale_q`] (at least 1).
pub fn scale_q(zoom: f64, device_scale: f64) -> u16 {
    let q = (zoom * device_scale * f64::from(SCALE_STEPS)).round();
    if q.is_nan() {
        return SCALE_STEPS;
    }
    q.clamp(1.0, f64::from(u16::MAX)) as u16
}

/// The render scale (device pixels per bp) that `scale_q` stands for.
pub fn scale_of(scale_q: u16) -> f64 {
    f64::from(scale_q) / f64::from(SCALE_STEPS)
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

/// Below this render scale (device pixels per bp), strokes that poppler drew too thin are drawn
/// again (see [`page_image`]): a line 0.4 pt wide, TeX's rule and TikZ's default line, is a
/// pixel wide at scale 2.51, and poppler draws wider lines as they are. A thinner line that
/// poppler loses at a larger scale stays lost.
const THIN_STROKE_SCALE: f64 = 2.6;

/// How poppler draws a page into a bitmap (see [`page_image`]).
#[derive(Clone, Copy)]
enum Pass {
    /// For the screen: strokes thinner than a device pixel are widened to one, and their
    /// horizontal and vertical segments moved onto the pixel grid.
    Screen,
    /// As for printing: every stroke at its own width.
    Unadjusted,
    /// As for the screen, but every stroke a device pixel wide, whatever its width.
    Hairline,
}

/// A transparent `size` (device pixels) image of what `pass` draws of `page` through `transform`
/// (which maps bp to device pixels).
fn draw(
    page: &poppler::Page,
    (width, height): (i32, i32),
    transform: &dyn Fn(&cairo::Context),
    pass: Pass,
) -> Result<cairo::ImageSurface, RenderError> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    transform(&cr);
    match pass {
        Pass::Screen => page.render(&cr),
        Pass::Unadjusted => page.render_for_printing(&cr),
        Pass::Hairline => {
            cr.set_hairline(true);
            page.render(&cr);
        }
    }
    cr.status()?;
    Ok(surface)
}

/// The alpha of one ARGB32 pixel (four bytes in native order).
fn alpha(pixel: &[u8; 4]) -> u8 {
    (u32::from_ne_bytes(*pixel) >> 24) as u8
}

/// True when `unadjusted` has more ink than `screen` in some pixel (both ARGB32 on a transparent
/// background, of one size): a stroke that poppler widened for the screen came out thinner than
/// it is, or beside where it is.
fn lost_ink(screen: &[u8], unadjusted: &[u8]) -> bool {
    let (screen, unadjusted) = (screen.as_chunks::<4>().0, unadjusted.as_chunks::<4>().0);
    screen
        .iter()
        .zip(unadjusted)
        .any(|(s, u)| alpha(u) > alpha(s))
}

/// Puts the strokes that `screen` lost back from `hairline` (three ARGB32 bitmaps on a
/// transparent background, `row` pixels a row, drawn by the [`Pass`]es they are named after):
/// the pixels where both `unadjusted` and `hairline` have more ink than `screen`, and `hairline`
/// has more than `screen` in a pixel beside them and in one above or below them as well.
///
/// `hairline` alone is not to be trusted: cairo measures the dashes of a hairline in device
/// pixels (a dashed arrow comes out with other dashes, at any scale), and it draws wide strokes
/// thin. `unadjusted` has every stroke where it belongs, so it tells which of `screen`'s pixels
/// lack ink, and `hairline` has those strokes a pixel wide, as `screen` has the others.
///
/// The last condition leaves the horizontal and vertical lines alone. poppler moves their ends
/// onto the pixel grid, by up to half a pixel, and the dashes with them, so `unadjusted` has a
/// sliver of each dash where `screen` has a gap, and `hairline` may have one of its own dashes
/// there. But such a line is one row or column of pixels in `hairline` too, with nothing beside
/// it, and a stroke that `screen` lost is slanted or curved: it shades the pixels around it.
fn restore_strokes(screen: &mut [u8], unadjusted: &[u8], hairline: &[u8], row: usize) {
    let (screen, unadjusted, hairline) = (
        screen.as_chunks_mut::<4>().0,
        unadjusted.as_chunks::<4>().0,
        hairline.as_chunks::<4>().0,
    );
    let pixels = screen.len().min(unadjusted.len()).min(hairline.len());
    let row = row.max(1);
    let more = |i: usize| alpha(&hairline[i]) > alpha(&screen[i]);
    let lost: Vec<usize> = (0..pixels)
        .filter(|&i| alpha(&unadjusted[i]) > alpha(&screen[i]) && more(i))
        .filter(|&i| {
            let x = i % row;
            let beside = (x > 0 && more(i - 1)) || (x + 1 < row && i + 1 < pixels && more(i + 1));
            let above_or_below = (i >= row && more(i - row)) || (i + row < pixels && more(i + row));
            beside && above_or_below
        })
        .collect();
    for i in lost {
        screen[i] = hairline[i];
    }
}

/// `page` drawn through `transform` (bp to device pixels, by `scale` pixels per bp) on white
/// paper, `size` device pixels large.
///
/// poppler widens a stroke thinner than a device pixel to one pixel, so that a rule does not
/// fade as the page is zoomed out. It measures that pixel along the axes of the stroke's own
/// coordinate system (`CairoOutputDev::updateLineWidth`), so in one turned by 45° the width comes
/// out as nothing: the tip of a slanted or curved TikZ arrow, which pgf draws in a system turned
/// with the arrow, thins and disappears below [`THIN_STROKE_SCALE`], while the arrow's line
/// stays. Such strokes are found by drawing the page as for printing too, with no stroke widened,
/// and taken from a third bitmap with every stroke a pixel wide ([`restore_strokes`]). A page
/// with no thin stroke costs the second pass, one with thin strokes all three: the 42 pages of a
/// thesis with TikZ diagrams on 29 of them take 710 ms instead of 380 at scale 1.36, and 950
/// instead of 470 at scale 2.4.
///
/// The page is drawn on a transparent bitmap and the paper laid under it, as pdftocairo and
/// Evince do: the alpha tells ink from paper, and a blend mode at the top level of a page has
/// no paper to blend with (the page is a transparency group of its own).
fn page_image(
    page: &poppler::Page,
    size: (i32, i32),
    scale: f64,
    transform: &dyn Fn(&cairo::Context),
) -> Result<cairo::ImageSurface, RenderError> {
    let mut surface = draw(page, size, transform, Pass::Screen)?;
    if scale < THIN_STROKE_SCALE {
        let mut unadjusted = draw(page, size, transform, Pass::Unadjusted)?;
        let unadjusted = unadjusted.data()?;
        if lost_ink(&surface.data()?, &unadjusted) {
            let mut hairline = draw(page, size, transform, Pass::Hairline)?;
            let row = surface.stride() as usize / 4;
            restore_strokes(&mut surface.data()?, &unadjusted, &hairline.data()?, row);
        }
    }
    lay_paper(&surface)?;
    Ok(surface)
}

/// Lays white paper under what is drawn on `surface`.
fn lay_paper(surface: &cairo::ImageSurface) -> Result<(), RenderError> {
    let cr = cairo::Context::new(surface)?;
    cr.set_operator(cairo::Operator::DestOver);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.paint()?;
    Ok(())
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
    let s = scale_of(key.scale_q);
    let mut surface = page_image(page, (width, height), s, &|cr| {
        cr.translate(-f64::from(x), -f64::from(y));
        cr.scale(s, s);
    })?;
    let stride = surface.stride();
    // The contexts are gone, so the surface is uniquely owned and neither data nor take_data can
    // fail with NonExclusive.
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
    let mut surface = if size.0 > 0.0 && size.1 > 0.0 {
        let (sx, sy) = (f64::from(width) / size.0, f64::from(height) / size.1);
        page_image(page, (width, height), sx.min(sy), &|cr| cr.scale(sx, sy))?
    } else {
        let blank = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
        lay_paper(&blank)?;
        blank
    };
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

    /// The `scale_q` of render scale `scale` (device pixels per bp).
    fn at(scale: f64) -> u16 {
        scale_q(scale, 1.0)
    }

    #[test]
    fn scale_quantization() {
        assert_eq!(scale_q(1.0, 2.0), 2048);
        assert_eq!(scale_q(1.3, 1.0), 1331);
        assert_eq!(scale_q(0.0, 1.0), 1);
        assert_eq!(scale_q(1e9, 1.0), u16::MAX);
        assert_eq!(scale_q(f64::NAN, 1.0), SCALE_STEPS);
        assert_eq!(scale_of(1536), 1.5);
        // Fine enough to draw the bitmap pixel for pixel: across 1024 bp, a fit-width zoom's
        // quantized scale is off by at most half a device pixel.
        for zoom in [0.6123, 1.0, 1.2378, 1.4853, 2.75, 7.9999] {
            for device_scale in [1.0, 1.25, 2.0] {
                let exact = zoom * device_scale;
                let q = scale_q(zoom, device_scale);
                assert!(
                    (scale_of(q) - exact).abs() * 1024.0 <= 0.5,
                    "{zoom} × {device_scale}"
                );
            }
        }
    }

    #[test]
    fn pixel_sizes_round_up() {
        assert_eq!(page_pixels(LETTER, at(2.0)), (1224, 1584));
        assert_eq!(page_pixels((100.1, 0.0), at(1.0)), (101, 1));
    }

    #[test]
    fn tiling_starts_above_16_mib() {
        // 2x letter is 7.76 MB, 3x is 17.4 MB: five full-width bands of 476 rows (3.5 MB).
        assert_eq!(tile_grid(LETTER, at(2.0)), None);
        assert_eq!(tile_size(LETTER, at(2.0)), None);
        assert_eq!(tile_grid(LETTER, at(3.0)), Some((1, 5)));
        assert_eq!(tile_size(LETTER, at(3.0)), Some((1836, 476)));
        // A very tall but narrow page exceeds cairo's dimension limit before the byte threshold.
        assert_eq!(tile_grid((10.0, 40_000.0), at(1.0)), Some((1, 2)));
        assert_eq!(tile_size((10.0, 40_000.0), at(1.0)), Some((10, 20_000)));
    }

    #[test]
    fn tiles_are_full_width_bands_within_the_byte_bound() {
        // Letter from just over the threshold to the view's largest zoom on a HiDPI display.
        for q in (24..=128).map(|eighths| eighths * (SCALE_STEPS / 8)) {
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
        assert_eq!(tile_grid(LETTER, at(3.25)), Some((1, 5)));
        // Bands up to 4096 px wide (4055 × 5247 px at scale 6.625), then about square tiles:
        // 4131 × 5346 px at scale 6.75 in 5 × 5 tiles of 827 × 1070.
        assert_eq!(tile_size(LETTER, at(6.625)), Some((4055, 250)));
        assert_eq!(tile_grid(LETTER, at(6.75)), Some((5, 5)));
        assert_eq!(tile_size(LETTER, at(6.75)), Some((827, 1070)));
        // Zoomed far in (9180 × 11880 px): 9 × 12 tiles of 1020 × 990.
        assert_eq!(tile_grid(LETTER, at(15.0)), Some((9, 12)));
        assert_eq!(tile_size(LETTER, at(15.0)), Some((1020, 990)));
    }

    #[test]
    fn huge_pages_keep_their_tiles_addressable() {
        // A side clamped to i32::MAX pixels: columns of SQUARE_TILE, cut to u16::MAX of them.
        assert_eq!(tile_grid((3e9, 100.0), at(1.0)), Some((u16::MAX, 1)));
        assert_eq!(tile_size((3e9, 100.0), at(1.0)), Some((SQUARE_TILE, 100)));
        let last = tile_rect((3e9, 100.0), at(1.0), (u16::MAX - 1, 0));
        assert_eq!(last, (65_534 * SQUARE_TILE, 0, SQUARE_TILE, 100));
        // A 2e7 bp page at scale 1/8: 2442 × 2442 tiles of 1024².
        let (w, h) = page_pixels((2e7, 2e7), at(0.125));
        let (columns, rows) = tile_grid((2e7, 2e7), at(0.125)).unwrap();
        let (tile_w, tile_h) = tile_size((2e7, 2e7), at(0.125)).unwrap();
        assert_eq!((columns, rows), (2442, 2442));
        assert!(i64::from(columns) * i64::from(tile_w) >= i64::from(w));
        assert!(i64::from(rows) * i64::from(tile_h) >= i64::from(h));
    }

    #[test]
    fn the_tiles_covering_a_region() {
        // Five bands of 476 rows.
        assert_eq!(tiles_covering(LETTER, at(2.0), (0, 0, 10, 10)), None);
        assert_eq!(
            tiles_covering(LETTER, at(3.0), (100, 400, 300, 200)),
            Some(vec![(0, 0), (0, 1)])
        );
        assert_eq!(
            tiles_covering(LETTER, at(3.0), (0, 476, 1836, 476)),
            Some(vec![(0, 1)])
        );
        // Clamped to the grid.
        assert_eq!(
            tiles_covering(LETTER, at(3.0), (-5, 2300, 5000, 5000)),
            Some(vec![(0, 4)])
        );
        // Several columns: tiles of 1020 × 990 px.
        assert_eq!(
            tiles_covering(LETTER, at(15.0), (3000, 980, 100, 20)),
            Some(vec![(2, 0), (3, 0), (2, 1), (3, 1)])
        );
        // Every pixel of a region lies in one of its tiles.
        let region = (1234, 567, 2345, 1678);
        let tiles = tiles_covering(LETTER, at(15.0), region).unwrap();
        for (x, y) in [(1234, 567), (3578, 2244), (3060, 990), (3059, 989)] {
            assert!(
                tiles.iter().any(|&t| {
                    let (tx, ty, tw, th) = tile_rect(LETTER, at(15.0), t);
                    (tx..tx + tw).contains(&x) && (ty..ty + th).contains(&y)
                }),
                "({x}, {y}) in {tiles:?}"
            );
        }
    }

    #[test]
    fn an_untiled_page_has_tiles_of_the_same_shape() {
        // 1224 × 1584 px: two bands of 792 rows, were it tiled.
        assert_eq!(tile_rect(LETTER, at(2.0), (0, 0)), (0, 0, 1224, 792));
        assert_eq!(tile_rect(LETTER, at(2.0), (0, 1)), (0, 792, 1224, 792));
        assert_eq!(tile_rect(LETTER, at(2.0), (0, 2)).3, 0);
        assert_eq!(tile_rect(LETTER, at(2.0), (1, 0)).2, 0);
    }

    /// A one-page PDF, 200 bp square, with `content` as its content stream.
    fn pdf(content: &str) -> poppler::Document {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> \
             /Contents 4 0 R >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            ),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
        }
        let xref = pdf.len();
        pdf.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        let bytes = glib::Bytes::from_owned(pdf.into_bytes());
        poppler::Document::from_bytes(&bytes, None).unwrap()
    }

    /// A thin dashed line whose middle lies 0.045 px above a pixel row at scale 1.5 (so its own
    /// width reaches into the next row, where the screen's line, moved onto the grid, has no
    /// ink), and a line 3 bp wide.
    const LINES: &str =
        "0.4 w [3 3] 0 d 20 50.03 m 180 50.03 l S\n[] 0 d 3 w 20 150 m 180 150 l S\n";

    /// A thin line in a coordinate system turned by 45°, as pgf draws the tip of a slanted
    /// arrow: from the middle of the page towards its top right corner.
    const TURNED: &str = "q 0.4 w 0.7071 0.7071 -0.7071 0.7071 100 100 cm 0 0 m 40 0 l S Q\n";

    /// The whole first page of `doc` at render scale `scale`.
    fn page_at(doc: &poppler::Document, scale: f64) -> Rendered {
        let key = PageKey {
            generation: 1,
            page: 0,
            scale_q: at(scale),
            tile: None,
            dark: false,
        };
        render(&doc.page(0).unwrap(), key).unwrap()
    }

    /// The green level of the pixel `(x, y)`: 255 on white paper, 0 in black ink.
    fn level(bitmap: &Rendered, x: usize, y: usize) -> u8 {
        let at = y * bitmap.stride as usize + x * 4;
        let pixel: [u8; 4] = bitmap.data[at..at + 4].try_into().unwrap();
        (u32::from_ne_bytes(pixel) >> 8) as u8
    }

    #[test]
    fn a_thin_stroke_in_a_turned_system_keeps_its_ink() {
        // 0.4 bp at scale 1.5 is 0.6 px: poppler widens the line to a pixel, measured along the
        // turned axes, which makes it nothing. It runs along the diagonals of the pixels
        // (150 + k, 149 - k).
        let doc = pdf(&format!("{LINES}{TURNED}"));
        let bitmap = page_at(&doc, 1.5);
        for k in [5, 20, 38] {
            assert!(
                level(&bitmap, 150 + k, 149 - k) < 64,
                "pixel {k} of the line"
            );
        }
        // A pixel wide, like the other thin lines: nothing two pixels beside it.
        assert_eq!(level(&bitmap, 172, 129), 255);
        assert_eq!(level(&bitmap, 168, 129), 255);
        // And nowhere else: beyond its end, and before its start.
        assert_eq!(level(&bitmap, 150 + 70, 149 - 70), 255);
        assert_eq!(level(&bitmap, 140, 159), 255);
        // The thumbnail of the page (300 px wide: the same scale) has it too.
        let req = ThumbReq {
            generation: 1,
            page: 0,
            width: 300,
            dark: false,
        };
        let thumb = render_thumbnail(&doc.page(0).unwrap(), req).unwrap();
        let at = 129 * thumb.stride as usize + 170 * 4;
        assert!(thumb.data[at + 1] < 64);
    }

    #[test]
    fn other_strokes_are_drawn_as_poppler_draws_them() {
        // Dashes of 4.5 px from x = 30 on row 224: cairo would draw a hairline's dashes 3 px
        // long, with ink at x = 36..39, in the line's first gap.
        let doc = pdf(LINES);
        let bitmap = page_at(&doc, 1.5);
        assert_eq!(level(&bitmap, 31, 224), 0);
        for x in 35..39 {
            assert_eq!(level(&bitmap, x, 224), 255, "x {x}");
        }
        assert_eq!(level(&bitmap, 41, 224), 0);
        // The row below has no ink, though the line's own width reaches into it.
        assert_eq!(level(&bitmap, 31, 225), 255);
        // The wide line is 4.5 px wide around y = 75.
        assert_eq!(level(&bitmap, 100, 71), 255);
        assert_eq!(level(&bitmap, 100, 75), 0);
        assert_eq!(level(&bitmap, 100, 78), 255);

        // Pixel for pixel what poppler draws for the screen, with and without the turned line
        // on the page (beside that line), below the scale that looks for lost strokes and above.
        let turned = pdf(&format!("{LINES}{TURNED}"));
        for scale in [0.8, 1.5, 3.0] {
            let page = doc.page(0).unwrap();
            let size = page_pixels(page.size(), at(scale));
            let s = scale_of(at(scale));
            let mut screen = draw(&page, size, &|cr| cr.scale(s, s), Pass::Screen).unwrap();
            lay_paper(&screen).unwrap();
            let screen = screen.data().unwrap().to_vec();
            assert!(page_at(&doc, scale).data[..] == screen[..], "scale {scale}");
            if scale < THIN_STROKE_SCALE {
                // The lower half of the page, where the turned line is not.
                let half = screen.len() / 2;
                assert!(
                    page_at(&turned, scale).data[half..] == screen[half..],
                    "scale {scale}"
                );
            }
        }
    }

    #[test]
    fn lost_strokes_come_from_the_hairlines() {
        // Rows of black ARGB32 pixels of the alphas given, five pixels a row.
        let px = |rows: [[u8; 5]; 3]| -> Vec<u8> {
            rows.as_flattened()
                .iter()
                .flat_map(|&a| (u32::from(a) << 24).to_ne_bytes())
                .collect()
        };
        // A slanted stroke from the bottom left to the top right that the screen lost but for
        // a trace in the middle, under a pixel of a wide stroke (top left).
        let screen = px([[255, 0, 0, 0, 0], [0, 0, 20, 0, 0], [0, 0, 0, 0, 0]]);
        let unadjusted = px([[255, 0, 0, 60, 90], [0, 60, 120, 60, 0], [90, 60, 0, 0, 0]]);
        let hairline = px([
            [90, 0, 40, 200, 230],
            [40, 200, 230, 200, 40],
            [230, 200, 40, 0, 0],
        ]);
        assert!(lost_ink(&screen, &unadjusted));
        assert!(!lost_ink(&screen, &screen));
        assert!(!lost_ink(&unadjusted, &screen));
        let mut restored = screen.clone();
        restore_strokes(&mut restored, &unadjusted, &hairline, 5);
        // Its pixels with ink of its own come back; the hairline's faint edge does not, and the
        // wide stroke stays.
        assert_eq!(
            restored,
            px([
                [255, 0, 0, 200, 230],
                [0, 200, 230, 200, 0],
                [230, 200, 0, 0, 0]
            ])
        );

        // A horizontal dashed line: the screen's dash is the two pixels on the left, the
        // unadjusted one half a pixel further right, and the hairline's dashes are others. In
        // the row below, the unadjusted line's own width.
        let screen = px([[0; 5], [255, 255, 0, 0, 0], [0; 5]]);
        let unadjusted = px([[0; 5], [40, 80, 40, 0, 0], [20, 40, 20, 0, 0]]);
        let hairline = px([[0; 5], [255, 0, 255, 255, 0], [0; 5]]);
        assert!(lost_ink(&screen, &unadjusted));
        let mut restored = screen.clone();
        restore_strokes(&mut restored, &unadjusted, &hairline, 5);
        assert_eq!(restored, screen);
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
        for q in [3.0, 3.25, 8.0, 15.0].map(at) {
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
