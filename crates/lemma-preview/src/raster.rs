//! A preview PDF turned into pixels: poppler renders its first page on a transparent surface at
//! the requested scale, the ink is cropped with a small margin, and the result is either an
//! **alpha mask** (black and grey ink: the app paints it in the current text colour, so it reads
//! in light and dark mode) or, when the formula has colour, the **colour image** itself.

/// The largest surface rendered, in pixels (a huge diagram is rendered at a smaller scale).
const MAX_PIXELS: f64 = 16.0 * 1024.0 * 1024.0;
/// A pixel this saturated (max − min of its unpremultiplied channels, 0–255) counts as colour.
const CHROMA: u32 = 60;
/// Coloured pixels needed to keep the colours (antialiasing alone makes a few).
const MIN_COLOURED: usize = 12;

/// The pixels of a rendered formula, rows packed without padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pixels {
    /// One coverage byte per pixel (0 transparent … 255 opaque), already weighted by lightness
    /// (grey ink is lighter than black).
    Alpha(Vec<u8>),
    /// Four bytes per pixel, B G R A premultiplied (cairo's ARGB32 on little-endian, GDK's
    /// `B8g8r8a8Premultiplied`).
    Bgra(Vec<u8>),
}

/// A rendered formula.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Pixels per TeX big point (bp) it was rendered at.
    pub px_per_pt: f64,
    pub pixels: Pixels,
}

impl Image {
    /// Bytes of pixel data.
    pub fn byte_size(&self) -> usize {
        match &self.pixels {
            Pixels::Alpha(p) | Pixels::Bgra(p) => p.len(),
        }
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        match &self.pixels {
            Pixels::Alpha(_) => self.width as usize,
            Pixels::Bgra(_) => self.width as usize * 4,
        }
    }

    /// True for an alpha mask.
    pub fn is_mask(&self) -> bool {
        matches!(self.pixels, Pixels::Alpha(_))
    }
}

/// Renders page 1 of `pdf` at `px_per_pt` and crops it to its ink plus `margin_pt` on each side.
/// `Err` for a PDF poppler cannot read, or a page without ink.
pub fn rasterize(pdf: Vec<u8>, px_per_pt: f64, margin_pt: f64) -> Result<Image, String> {
    let bytes = glib::Bytes::from_owned(pdf);
    let doc = poppler::Document::from_bytes(&bytes, None).map_err(|e| e.to_string())?;
    let page = doc.page(0).ok_or("the preview has no page")?;
    let (w_pt, h_pt) = page.size();
    let mut scale = px_per_pt.clamp(0.1, 16.0);
    if w_pt * h_pt * scale * scale > MAX_PIXELS {
        scale = (MAX_PIXELS / (w_pt * h_pt)).sqrt();
    }
    let width = (w_pt * scale).ceil().max(1.0) as i32;
    let height = (h_pt * scale).ceil().max(1.0) as i32;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
        .map_err(|e| e.to_string())?;
    {
        let cr = cairo::Context::new(&surface).map_err(|e| e.to_string())?;
        cr.scale(scale, scale);
        page.render(&cr);
        cr.status().map_err(|e| e.to_string())?;
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().map_err(|e| e.to_string())?;
    let (w, h) = (width as usize, height as usize);
    let bbox = ink_box(&data, stride, w, h).ok_or("the formula has no ink")?;
    let margin = (margin_pt * scale).round() as usize;
    let x0 = bbox.0.saturating_sub(margin);
    let y0 = bbox.1.saturating_sub(margin);
    let x1 = (bbox.2 + margin).min(w);
    let y1 = (bbox.3 + margin).min(h);
    let (cw, ch) = (x1 - x0, y1 - y0);
    let coloured = coloured_pixels(&data, stride, (x0, y0, x1, y1)) >= MIN_COLOURED;
    let pixels = if coloured {
        let mut out = Vec::with_capacity(cw * ch * 4);
        for y in y0..y1 {
            out.extend_from_slice(&data[y * stride + x0 * 4..y * stride + x1 * 4]);
        }
        Pixels::Bgra(out)
    } else {
        let mut out = Vec::with_capacity(cw * ch);
        for y in y0..y1 {
            let row = &data[y * stride..];
            for x in x0..x1 {
                let p = &row[x * 4..x * 4 + 4];
                out.push(mask_value(p));
            }
        }
        Pixels::Alpha(out)
    };
    Ok(Image {
        width: cw as u32,
        height: ch as u32,
        px_per_pt: scale,
        pixels,
    })
}

/// A premultiplied B G R A pixel as mask coverage: its alpha minus its (premultiplied) lightness,
/// so black ink keeps its coverage, grey ink gets lighter and white ink disappears.
fn mask_value(p: &[u8]) -> u8 {
    let (b, g, r, a) = (
        u32::from(p[0]),
        u32::from(p[1]),
        u32::from(p[2]),
        u32::from(p[3]),
    );
    // Rec. 709 luma in 1/10000.
    let luma = (2126 * r + 7152 * g + 722 * b) / 10000;
    a.saturating_sub(luma).min(255) as u8
}

/// The smallest box (x0, y0, x1, y1), exclusive ends, holding every pixel with any coverage.
fn ink_box(data: &[u8], stride: usize, w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let row_has_ink = |y: usize| (0..w).any(|x| data[y * stride + x * 4 + 3] != 0);
    let y0 = (0..h).find(|&y| row_has_ink(y))?;
    let y1 = (y0..h).rev().find(|&y| row_has_ink(y))? + 1;
    let col_has_ink = |x: usize| (y0..y1).any(|y| data[y * stride + x * 4 + 3] != 0);
    let x0 = (0..w).find(|&x| col_has_ink(x))?;
    let x1 = (x0..w).rev().find(|&x| col_has_ink(x))? + 1;
    Some((x0, y0, x1, y1))
}

/// How many clearly coloured (saturated, mostly opaque) pixels the box holds.
fn coloured_pixels(
    data: &[u8],
    stride: usize,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
) -> usize {
    let mut n = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = &data[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = u32::from(p[3]);
            if a < 96 {
                continue;
            }
            let (b, g, r) = (u32::from(p[0]), u32::from(p[1]), u32::from(p[2]));
            let spread = r.max(g).max(b) - r.min(g).min(b);
            // Unpremultiplied spread > CHROMA.
            if spread * 255 > CHROMA * a {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page `w`×`h` bp PDF filling the rectangles `(x, y, width, height, [r, g, b])`
    /// (top-left origin), written by hand (cairo's PDF surface is not enabled here).
    fn pdf(w: f64, h: f64, rects: &[(f64, f64, f64, f64, [f64; 3])]) -> Vec<u8> {
        let mut content = String::new();
        for (x, y, rw, rh, [r, g, b]) in rects {
            content.push_str(&format!(
                "{r} {g} {b} rg {x} {} {rw} {rh} re f\n",
                h - y - rh
            ));
        }
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents 4 0 R >>"),
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            ),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for o in offsets {
            out.push_str(&format!("{o:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        out.into_bytes()
    }

    #[test]
    fn black_ink_becomes_a_cropped_mask() {
        let doc = pdf(200.0, 100.0, &[(50.0, 20.0, 40.0, 10.0, [0.0, 0.0, 0.0])]);
        let img = rasterize(doc, 2.0, 1.0).unwrap();
        assert!(img.is_mask());
        // 40 × 10 bp at 2 px/bp plus 2 px of margin on each side.
        assert_eq!((img.width, img.height), (84, 24));
        let Pixels::Alpha(p) = &img.pixels else {
            unreachable!()
        };
        assert_eq!(p.len(), 84 * 24);
        assert_eq!(p[0], 0, "margin");
        assert_eq!(p[12 * 84 + 40], 255, "ink");
        assert_eq!(img.stride(), 84);
        assert_eq!(img.byte_size(), 84 * 24);
    }

    #[test]
    fn grey_ink_is_lighter_and_colour_is_kept() {
        let doc = pdf(100.0, 100.0, &[(10.0, 10.0, 20.0, 20.0, [0.5, 0.5, 0.5])]);
        let img = rasterize(doc, 1.0, 0.0).unwrap();
        let Pixels::Alpha(p) = &img.pixels else {
            panic!("grey is not colour")
        };
        let mid = p[10 * img.width as usize + 10];
        assert!((120..=136).contains(&mid), "{mid}");
        let doc = pdf(100.0, 100.0, &[(10.0, 10.0, 20.0, 20.0, [0.8, 0.1, 0.1])]);
        let img = rasterize(doc, 1.0, 0.0).unwrap();
        assert!(!img.is_mask());
        let Pixels::Bgra(p) = &img.pixels else {
            unreachable!()
        };
        assert_eq!(p.len(), 20 * 20 * 4);
        let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
        assert!(
            (24..=27).contains(&b) && (24..=27).contains(&g),
            "{:?}",
            &p[0..4]
        );
        assert_eq!((r, a), (204, 255));
    }

    #[test]
    fn empty_pages_and_garbage_are_errors() {
        let doc = pdf(50.0, 50.0, &[]);
        assert!(rasterize(doc, 1.0, 1.0).is_err());
        assert!(rasterize(b"not a pdf".to_vec(), 1.0, 1.0).is_err());
    }

    #[test]
    fn huge_pages_render_at_a_smaller_scale() {
        let doc = pdf(
            4000.0,
            4000.0,
            &[(0.0, 0.0, 4000.0, 4000.0, [0.0, 0.0, 0.0])],
        );
        let img = rasterize(doc, 4.0, 0.0).unwrap();
        assert!(f64::from(img.width) * f64::from(img.height) <= MAX_PIXELS * 1.01);
        assert!(img.px_per_pt < 4.0);
    }
}
