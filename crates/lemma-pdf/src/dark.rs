//! Dark mode for page bitmaps: CSS `invert(1) hue-rotate(180deg)`, which swaps light and dark
//! but keeps hues (a red link stays red), mapped into [`PAPER`]..[`INK`] so white paper becomes a
//! soft dark grey and black ink a soft light grey.
//!
//! The PDF thread applies it to the bitmaps of [`PageKey::dark`](crate::PageKey::dark) requests
//! (and of [`ThumbReq::dark`](crate::ThumbReq::dark) thumbnails), once per bitmap, so the view
//! and the thumbnail strip draw dark pages as plain textures. (A colour matrix over the
//! pages at draw time costs GTK's cairo renderer a per-pixel pass over the whole view on every
//! frame, which made scrolling miss frames.) The PDF view builds the same transform as a colour
//! matrix from these constants for the brief moments it shows a page of the other mode.

/// Lightness of white paper in dark mode.
pub const PAPER: f32 = 0.16;
/// Lightness of black ink in dark mode.
pub const INK: f32 = 0.92;

/// The 3×3 hue rotation by 180° of CSS `hue-rotate(180deg)` (Filter Effects spec), row-major over
/// RGB: `out[i] = Σ_j HUE_180[i][j] · in[j]`. Its rows sum to 1, so grey stays grey, and it is its
/// own inverse (up to the rounding of the coefficients).
pub const HUE_180: [[f32; 3]; 3] = [
    [-0.574, 1.430, 0.144],
    [0.426, 0.430, 0.144],
    [0.426, 1.430, -0.856],
];

/// One colour (straight RGB in 0..=1) in dark mode: `INK − (INK − PAPER) · HUE_180 · rgb`,
/// clamped to 0..=1.
pub fn darken_rgb(rgb: [f32; 3]) -> [f32; 3] {
    let contrast = INK - PAPER;
    HUE_180.map(|row| {
        let v = INK - contrast * (row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2]);
        v.clamp(0.0, 1.0)
    })
}

/// Colours a bitmap of BGRA premultiplied pixels (cairo's ARGB32 on little-endian, as rendered)
/// in dark mode, in place; alpha is unchanged. A trailing partial pixel is left alone.
///
/// Opaque pixels (every pixel of a rendered page) go through integer lookup tables; the others are
/// unpremultiplied, transformed and premultiplied again. Most of a page is white paper and the rest
/// mostly grey ink (r = g = b), whose pixels are looked up whole in tables made with the same
/// arithmetic, so the result is the same to the bit: a 1563 × 2210 page (fit width on a HiDPI
/// screen) takes about 2.5 ms instead of 12 ms with three table lookups per channel.
pub fn darken_bgra(data: &mut [u8]) {
    let contrast = INK - PAPER;
    // lut[c][j][v]: the share of input channel j at level v in output channel c (R, G, B), in
    // 1/256 of a level.
    let mut lut = [[[0i32; 256]; 3]; 3];
    for (c, row) in HUE_180.iter().enumerate() {
        for (j, h) in row.iter().enumerate() {
            for (v, entry) in lut[c][j].iter_mut().enumerate() {
                *entry = (contrast * h * v as f32 * 256.0).round() as i32;
            }
        }
    }
    let base = (INK * 255.0 * 256.0).round() as i32;
    let level = |c: usize, r: u8, g: u8, b: u8| -> u8 {
        let sum = lut[c][0][usize::from(r)] + lut[c][1][usize::from(g)] + lut[c][2][usize::from(b)];
        ((base - sum + 128) >> 8).clamp(0, 255) as u8
    };
    // greys[v]: the opaque grey pixel of level v, coloured (BGRA, as below).
    let greys: [[u8; 4]; 256] = std::array::from_fn(|v| {
        let v = v as u8;
        [level(2, v, v, v), level(1, v, v, v), level(0, v, v, v), 255]
    });
    let paper = greys[255];
    for pixel in data.as_chunks_mut::<4>().0 {
        if *pixel == [255; 4] {
            *pixel = paper;
            continue;
        }
        let (b, g, r, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        match a {
            255 if b == g && g == r => *pixel = greys[usize::from(b)],
            255 => {
                pixel[2] = level(0, r, g, b);
                pixel[1] = level(1, r, g, b);
                pixel[0] = level(2, r, g, b);
            }
            // Fully transparent stays transparent (premultiplied colour must be 0).
            0 => pixel[..3].fill(0),
            _ => {
                let alpha = f32::from(a) / 255.0;
                let straight = |v: u8| (f32::from(v) / 255.0 / alpha).min(1.0);
                let out = darken_rgb([straight(r), straight(g), straight(b)]);
                let premultiplied = |v: f32| (v * alpha * 255.0).round() as u8;
                pixel[2] = premultiplied(out[0]);
                pixel[1] = premultiplied(out[1]);
                pixel[0] = premultiplied(out[2]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(v: f32) -> u8 {
        (v * 255.0).round() as u8
    }

    #[test]
    fn paper_and_ink_swap_and_alpha_stays() {
        // BGRA: white, black, transparent.
        let mut data = vec![255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 0];
        darken_bgra(&mut data);
        let (paper, ink) = (level(PAPER), level(INK));
        assert_eq!(&data[..4], &[paper, paper, paper, 255]);
        assert_eq!(&data[4..8], &[ink, ink, ink, 255]);
        assert_eq!(&data[8..], &[0, 0, 0, 0]);
    }

    #[test]
    fn opaque_pixels_match_the_float_transform() {
        let mut worst = 0;
        for r in (0..=255u8).step_by(15) {
            for g in (0..=255u8).step_by(17) {
                for b in (0..=255u8).step_by(51) {
                    let mut data = [b, g, r, 255];
                    darken_bgra(&mut data);
                    let rgb = [r, g, b].map(|v| f32::from(v) / 255.0);
                    let want = darken_rgb(rgb).map(level);
                    for (got, want) in [data[2], data[1], data[0]].into_iter().zip(want) {
                        worst = worst.max(got.abs_diff(want));
                    }
                    assert_eq!(data[3], 255);
                }
            }
        }
        assert!(worst <= 1, "off by {worst} levels");
    }

    #[test]
    fn hues_survive() {
        // A red link stays reddish, a blue one bluish.
        let mut data = [26, 26, 204, 255, 204, 26, 26, 255];
        darken_bgra(&mut data);
        assert!(data[2] > data[1] && data[2] > data[0], "{data:?}");
        assert!(data[4] > data[5] && data[4] > data[6], "{data:?}");
    }

    #[test]
    fn translucent_pixels_are_unpremultiplied_first() {
        // Half-transparent white: dark paper at half alpha.
        let mut data = [128, 128, 128, 128];
        darken_bgra(&mut data);
        let want = (PAPER * 128.0).round() as u8;
        for v in &data[..3] {
            assert!(v.abs_diff(want) <= 1, "{data:?}");
        }
        assert_eq!(data[3], 128);
    }
}
