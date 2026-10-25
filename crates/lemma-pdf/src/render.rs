//! Bitmap sizes, tiling and rasterising one page (or one tile of it) with poppler into a cairo image.

use crate::{PageKey, Rendered, ThumbReq, Thumbnail};

/// Edge length of a render tile in device pixels.
pub const TILE_SIZE: i32 = 512;

/// Pages whose full bitmap would be larger than this many bytes are rendered as [`TILE_SIZE`]² tiles.
pub const TILE_THRESHOLD_BYTES: usize = 16 * 1024 * 1024;

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

/// `(columns, rows)` of the tile grid when a page of `size_bp` at `scale_q` must be tiled: its bitmap
/// would exceed [`TILE_THRESHOLD_BYTES`] or cairo's size limit. `None` means one bitmap for the page.
pub fn tile_grid(size_bp: (f64, f64), scale_q: u16) -> Option<(u16, u16)> {
    let (w, h) = page_pixels(size_bp, scale_q);
    let bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    if bytes <= TILE_THRESHOLD_BYTES && w <= MAX_SURFACE_DIM && h <= MAX_SURFACE_DIM {
        return None;
    }
    // In i64: a side of a huge page is clamped to i32::MAX pixels, so adding TILE_SIZE - 1 would
    // overflow i32. More than u16::MAX tiles per side are cut to u16::MAX.
    let count = |px: i32| {
        let tiles = (i64::from(px) + i64::from(TILE_SIZE) - 1) / i64::from(TILE_SIZE);
        u16::try_from(tiles).unwrap_or(u16::MAX)
    };
    Some((count(w), count(h)))
}

/// `(x, y, width, height)` in device pixels of `tile = (column, row)` within the full page bitmap. Edge
/// tiles are cut to the page; a tile outside the page has zero width or height.
pub fn tile_rect(size_bp: (f64, f64), scale_q: u16, tile: (u16, u16)) -> (i32, i32, i32, i32) {
    let (w, h) = page_pixels(size_bp, scale_q);
    let x = i32::from(tile.0) * TILE_SIZE;
    let y = i32::from(tile.1) * TILE_SIZE;
    (
        x,
        y,
        (w - x).clamp(0, TILE_SIZE),
        (h - y).clamp(0, TILE_SIZE),
    )
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
        // 2x letter is 7.76 MB, 3x is 17.4 MB.
        assert_eq!(tile_grid(LETTER, 16), None);
        assert_eq!(tile_grid(LETTER, 24), Some((4, 5)));
        // A very tall but narrow page exceeds cairo's dimension limit before the byte threshold.
        assert_eq!(tile_grid((10.0, 40_000.0), 8), Some((1, 79)));
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
        let (w, h) = page_pixels(LETTER, 24);
        let (cols, rows) = tile_grid(LETTER, 24).unwrap();
        let mut area = 0i64;
        for c in 0..cols {
            for r in 0..rows {
                let (x, y, tw, th) = tile_rect(LETTER, 24, (c, r));
                assert_eq!((x, y), (i32::from(c) * 512, i32::from(r) * 512));
                assert!(tw > 0 && tw <= 512 && th > 0 && th <= 512);
                area += i64::from(tw) * i64::from(th);
            }
        }
        assert_eq!(area, i64::from(w) * i64::from(h));
        assert_eq!(tile_rect(LETTER, 24, (cols, 0)).2, 0);
    }
}
