//! Continuous vertical page layout: pages stacked top to bottom, centred horizontally.

use std::ops::Range;

/// The smallest zoom [`PageLayout::fit_width_zoom`] and [`PageLayout::fit_page_zoom`] return, so a tiny
/// view never yields a zero zoom.
const MIN_FIT_ZOOM: f64 = 0.05;

/// Continuous vertical layout. Zoom 1.0 = 1 bp per logical pixel; gap between pages in logical px.
///
/// All view coordinates are document coordinates in logical pixels (before scrolling): pages are
/// stacked top to bottom with `gap` between them and a `gap` margin above the first and below the last
/// page. Horizontally each page is centred in `max(view_width, content width)`, where the content width
/// is the widest page plus a `gap` margin on each side. The gap does not scale with zoom.
///
/// Page indices are 0-based. Page coordinates are PDF big points with the origin at the page's
/// top-left corner and y growing downward (the SyncTeX convention).
#[derive(Clone, Debug)]
pub struct PageLayout {
    sizes: Vec<(f64, f64)>,
    gap: f64,
    /// Prefix sums of the page heights in bp: `heights_before[i]` is the total height of pages `0..i`.
    heights_before: Vec<f64>,
    max_width: f64,
    max_height: f64,
}

/// Scroll anchor that survives reloads: page and fraction of the page height at the viewport top, plus x.
///
/// `frac` is negative when the viewport top lies in the gap above `page`. `x` is the horizontal scroll
/// offset, carried unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub page: u32,
    pub frac: f64,
    pub x: f64,
}

impl PageLayout {
    /// Creates a layout for pages of the given `(width, height)` in bp. A negative gap is treated as 0.
    pub fn new(sizes: Vec<(f64, f64)>, gap: f64) -> Self {
        let mut heights_before = Vec::with_capacity(sizes.len() + 1);
        let mut total = 0.0;
        heights_before.push(total);
        for &(_, h) in &sizes {
            total += h;
            heights_before.push(total);
        }
        let max_width = sizes.iter().map(|s| s.0).fold(0.0, f64::max);
        let max_height = sizes.iter().map(|s| s.1).fold(0.0, f64::max);
        Self {
            sizes,
            gap: gap.max(0.0),
            heights_before,
            max_width,
            max_height,
        }
    }

    /// Number of pages.
    pub fn page_count(&self) -> u32 {
        self.sizes.len() as u32
    }

    /// Size of `page` in bp, or `None` if it does not exist.
    pub fn page_size(&self, page: u32) -> Option<(f64, f64)> {
        self.sizes.get(page as usize).copied()
    }

    /// The sizes of every page in bp.
    pub fn sizes(&self) -> &[(f64, f64)] {
        &self.sizes
    }

    /// Width and height of the whole document in logical px, margins included.
    pub fn content_size(&self, zoom: f64) -> (f64, f64) {
        let n = self.sizes.len();
        (
            self.max_width * zoom + 2.0 * self.gap,
            self.heights_before[n] * zoom + (n as f64 + 1.0) * self.gap,
        )
    }

    /// `(x, y, width, height)` of `page` in logical px. A page past the end is clamped to the last page;
    /// an empty layout yields an empty rectangle.
    pub fn page_rect(&self, page: u32, zoom: f64, view_width: f64) -> (f64, f64, f64, f64) {
        let Some(i) = self.clamp_page(page) else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        let (w, h) = self.sizes[i];
        let (w, h) = (w * zoom, h * zoom);
        let span = view_width.max(self.content_size(zoom).0);
        ((span - w) / 2.0, self.top(i, zoom), w, h)
    }

    /// Pages whose rectangles intersect the vertical span `[y0, y1)` (empty when `y1 <= y0`).
    pub fn visible(&self, y0: f64, y1: f64, zoom: f64) -> Range<u32> {
        let n = self.sizes.len();
        let first = partition_point(n, |i| self.bottom(i, zoom) <= y0);
        let end = if y1 > y0 {
            partition_point(n, |i| self.top(i, zoom) < y1).max(first)
        } else {
            first
        };
        first as u32..end as u32
    }

    /// Converts a point on `page` (bp, top-left origin) to document coordinates in logical px.
    pub fn to_view(
        &self,
        page: u32,
        x_bp: f64,
        y_bp: f64,
        zoom: f64,
        view_width: f64,
    ) -> (f64, f64) {
        let (x, y, _, _) = self.page_rect(page, zoom, view_width);
        (x + x_bp * zoom, y + y_bp * zoom)
    }

    /// Converts a point in document coordinates to `(page, x_bp, y_bp)`, or `None` if the point is not on
    /// a page (in a gap or margin). Page edges are inclusive.
    pub fn to_page(&self, x: f64, y: f64, zoom: f64, view_width: f64) -> Option<(u32, f64, f64)> {
        let n = self.sizes.len();
        if zoom <= 0.0 {
            return None;
        }
        let i = partition_point(n, |i| self.bottom(i, zoom) < y);
        if i == n {
            return None;
        }
        let (px, py, pw, _) = self.page_rect(i as u32, zoom, view_width);
        if y < py || x < px || x > px + pw {
            return None;
        }
        Some((i as u32, (x - px) / zoom, (y - py) / zoom))
    }

    /// The anchor for a viewport whose top edge is at `y` and whose horizontal scroll offset is `x`.
    ///
    /// The anchor page is the first page whose bottom edge lies below `y` (the last page when `y` is
    /// past the end).
    pub fn anchor_at(&self, y: f64, x: f64, zoom: f64) -> Anchor {
        let n = self.sizes.len();
        if n == 0 {
            return Anchor {
                page: 0,
                frac: 0.0,
                x,
            };
        }
        let i = partition_point(n, |i| self.bottom(i, zoom) <= y).min(n - 1);
        let h = self.sizes[i].1 * zoom;
        let frac = if h > 0.0 {
            (y - self.top(i, zoom)) / h
        } else {
            0.0
        };
        Anchor {
            page: i as u32,
            frac,
            x,
        }
    }

    /// The viewport top for `a` in this layout. Clamps the page to the new page count and the result to
    /// `[0, content height]`.
    pub fn y_for_anchor(&self, a: Anchor, zoom: f64) -> f64 {
        let Some(i) = self.clamp_page(a.page) else {
            return 0.0;
        };
        let y = self.top(i, zoom) + a.frac * self.sizes[i].1 * zoom;
        y.clamp(0.0, self.content_size(zoom).1)
    }

    /// The zoom at which the widest page plus its margins exactly fills `view_width` (1.0 when there are
    /// no pages; never below 0.05).
    pub fn fit_width_zoom(&self, view_width: f64) -> f64 {
        if self.max_width <= 0.0 {
            return 1.0;
        }
        ((view_width - 2.0 * self.gap) / self.max_width).max(MIN_FIT_ZOOM)
    }

    /// The largest zoom at which every page, with the gaps above and below it, fits a view of
    /// `view_width` × `view_height`: at most [`PageLayout::fit_width_zoom`], and the tallest page
    /// fills the height (1.0 when there are no pages; never below 0.05).
    pub fn fit_page_zoom(&self, view_width: f64, view_height: f64) -> f64 {
        let width = self.fit_width_zoom(view_width);
        if self.max_height <= 0.0 {
            return width;
        }
        ((view_height - 2.0 * self.gap) / self.max_height)
            .min(width)
            .max(MIN_FIT_ZOOM)
    }

    fn clamp_page(&self, page: u32) -> Option<usize> {
        let n = self.sizes.len();
        (n > 0).then(|| (page as usize).min(n - 1))
    }

    fn top(&self, i: usize, zoom: f64) -> f64 {
        self.gap * (i as f64 + 1.0) + self.heights_before[i] * zoom
    }

    fn bottom(&self, i: usize, zoom: f64) -> f64 {
        self.gap * (i as f64 + 1.0) + self.heights_before[i + 1] * zoom
    }
}

/// The first index in `0..n` for which `pred` is false, given that `pred` is true for a prefix.
fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const LETTER: (f64, f64) = (612.0, 792.0);

    fn letters(n: usize) -> PageLayout {
        PageLayout::new(vec![LETTER; n], 10.0)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn rects_stack_with_gaps_and_centre() {
        let l = letters(3);
        assert_eq!(l.page_count(), 3);
        assert_eq!(l.content_size(1.0), (632.0, 3.0 * 792.0 + 40.0));
        assert_eq!(l.page_rect(0, 1.0, 1000.0), (194.0, 10.0, 612.0, 792.0));
        assert_eq!(l.page_rect(1, 1.0, 1000.0), (194.0, 812.0, 612.0, 792.0));
        assert_eq!(l.page_rect(2, 2.0, 1000.0).1, 30.0 + 4.0 * 792.0);
        // A view narrower than the content scrolls horizontally: the page sits at the left margin.
        assert_eq!(l.page_rect(0, 1.0, 300.0).0, 10.0);
        // Past-the-end pages clamp to the last page.
        assert_eq!(l.page_rect(7, 1.0, 1000.0), l.page_rect(2, 1.0, 1000.0));
    }

    #[test]
    fn mixed_sizes_centre_in_content_width() {
        let l = PageLayout::new(vec![LETTER, (792.0, 612.0)], 0.0);
        assert_eq!(l.content_size(1.0), (792.0, 792.0 + 612.0));
        assert_eq!(l.page_rect(0, 1.0, 100.0), (90.0, 0.0, 612.0, 792.0));
        assert_eq!(l.page_rect(1, 1.0, 100.0), (0.0, 792.0, 792.0, 612.0));
        assert_eq!(l.page_size(1), Some((792.0, 612.0)));
        assert_eq!(l.page_size(2), None);
    }

    #[test]
    fn visible_ranges() {
        let l = letters(5);
        assert_eq!(l.visible(0.0, 5.0, 1.0), 0..0);
        assert_eq!(l.visible(0.0, 11.0, 1.0), 0..1);
        assert_eq!(l.visible(0.0, 900.0, 1.0), 0..2);
        // Entirely inside the gap between pages 0 and 1.
        assert_eq!(l.visible(803.0, 811.0, 1.0), 1..1);
        assert_eq!(l.visible(805.0, 2000.0, 1.0), 1..3);
        assert_eq!(l.visible(-100.0, 1e9, 1.0), 0..5);
        assert_eq!(l.visible(1e9, 2e9, 1.0), 5..5);
        assert_eq!(l.visible(500.0, 400.0, 1.0), 0..0);
        let (_, y, _, h) = l.page_rect(3, 0.5, 800.0);
        assert_eq!(l.visible(y + 1.0, y + h - 1.0, 0.5), 3..4);
    }

    #[test]
    fn to_page_rejects_gaps_and_margins() {
        let l = letters(2);
        assert_eq!(l.to_page(300.0, 5.0, 1.0, 1000.0), None);
        assert_eq!(l.to_page(300.0, 806.0, 1.0, 1000.0), None);
        assert_eq!(l.to_page(100.0, 400.0, 1.0, 1000.0), None);
        assert_eq!(l.to_page(300.0, 1e6, 1.0, 1000.0), None);
        assert_eq!(l.to_page(194.0, 10.0, 1.0, 1000.0), Some((0, 0.0, 0.0)));
        assert_eq!(
            l.to_page(806.0, 1604.0, 1.0, 1000.0),
            Some((1, 612.0, 792.0))
        );
    }

    #[test]
    fn anchor_clamps_to_new_page_count() {
        let long = letters(10);
        let a = long.anchor_at(long.page_rect(8, 1.0, 0.0).1 + 396.0, 12.0, 1.0);
        assert_eq!(a.page, 8);
        assert!(close(a.frac, 0.5));
        assert_eq!(a.x, 12.0);
        let short = letters(3);
        assert_eq!(
            short.y_for_anchor(a, 1.0),
            short.page_rect(2, 1.0, 0.0).1 + 396.0
        );
        assert_eq!(PageLayout::new(Vec::new(), 10.0).y_for_anchor(a, 1.0), 0.0);
        // Past the end of the content the result is clamped.
        let far = Anchor {
            page: 2,
            frac: 50.0,
            x: 0.0,
        };
        assert_eq!(short.y_for_anchor(far, 1.0), short.content_size(1.0).1);
    }

    #[test]
    fn anchor_survives_reload_with_changed_sizes() {
        let old = letters(4);
        let a = old.anchor_at(old.page_rect(2, 1.5, 0.0).1 + 0.25 * 792.0 * 1.5, 0.0, 1.5);
        let new = PageLayout::new(vec![(612.0, 500.0), LETTER, LETTER, LETTER], 10.0);
        let y = new.y_for_anchor(a, 1.5);
        assert!(close(y, new.page_rect(2, 1.5, 0.0).1 + 0.25 * 792.0 * 1.5));
    }

    #[test]
    fn fit_width_fills_the_view() {
        let l = PageLayout::new(vec![LETTER, (500.0, 700.0)], 12.0);
        let z = l.fit_width_zoom(800.0);
        assert!(close(z, (800.0 - 24.0) / 612.0));
        let (x, _, w, _) = l.page_rect(0, z, 800.0);
        assert!(close(x, 12.0));
        assert!(close(w, 776.0));
        assert!(close(l.content_size(z).0, 800.0));
        assert_eq!(l.fit_width_zoom(1.0), MIN_FIT_ZOOM);
        assert_eq!(PageLayout::new(Vec::new(), 12.0).fit_width_zoom(800.0), 1.0);
    }

    #[test]
    fn fit_page_shows_the_tallest_page_whole() {
        let l = PageLayout::new(vec![LETTER, (500.0, 900.0)], 12.0);
        // A tall view: the width decides, as at fit width.
        assert_eq!(l.fit_page_zoom(800.0, 5000.0), l.fit_width_zoom(800.0));
        // A wide one: the tallest page and its two gaps fill the height.
        let z = l.fit_page_zoom(800.0, 600.0);
        assert!(close(z, (600.0 - 24.0) / 900.0));
        let (_, y, w, h) = l.page_rect(1, z, 800.0);
        assert!(close(h + 2.0 * 12.0, 600.0));
        assert!(w < 800.0);
        assert_eq!(l.visible(y - 12.0, y - 12.0 + 600.0, z), 1..2);
        assert_eq!(l.fit_page_zoom(800.0, 1.0), MIN_FIT_ZOOM);
        assert_eq!(
            PageLayout::new(Vec::new(), 12.0).fit_page_zoom(800.0, 600.0),
            1.0
        );
    }

    #[test]
    fn empty_layout_is_harmless() {
        let l = PageLayout::new(Vec::new(), 8.0);
        assert_eq!(l.page_count(), 0);
        assert_eq!(l.visible(0.0, 100.0, 1.0), 0..0);
        assert_eq!(l.to_page(10.0, 10.0, 1.0, 100.0), None);
        assert_eq!(l.page_rect(0, 1.0, 100.0), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(
            l.anchor_at(10.0, 3.0, 1.0),
            Anchor {
                page: 0,
                frac: 0.0,
                x: 3.0
            }
        );
    }

    fn layout() -> impl Strategy<Value = PageLayout> {
        (
            prop::collection::vec((50.0f64..2000.0, 50.0f64..2000.0), 1..40),
            prop_oneof![Just(0.0), 1.0f64..40.0],
        )
            .prop_map(|(sizes, gap)| PageLayout::new(sizes, gap))
    }

    proptest! {
        #[test]
        fn view_page_round_trip(l in layout(), page in 0u32..40, fx in 0.0f64..=1.0, fy in 0.0f64..1.0,
                                zoom in 0.05f64..8.0, view_width in 0.0f64..5000.0) {
            let page = page % l.page_count();
            let (w, h) = l.page_size(page).unwrap();
            let (x_bp, y_bp) = (fx * w, fy * h);
            let (x, y) = l.to_view(page, x_bp, y_bp, zoom, view_width);
            let (p, bx, by) = l.to_page(x, y, zoom, view_width).expect("a point on a page maps back");
            if l.gap > 0.0 || fy > 0.0 {
                prop_assert_eq!(p, page);
                prop_assert!((bx - x_bp).abs() < 1e-6, "{} vs {}", bx, x_bp);
                prop_assert!((by - y_bp).abs() < 1e-6, "{} vs {}", by, y_bp);
            } else {
                // Without a gap, a shared page edge belongs to the upper page; the point is still the same.
                let back = l.to_view(p, bx, by, zoom, view_width);
                prop_assert!((back.0 - x).abs() < 1e-6 && (back.1 - y).abs() < 1e-6);
            }
        }

        #[test]
        fn anchor_round_trip(l in layout(), fy in 0.0f64..=1.0, x in 0.0f64..100.0, zoom in 0.05f64..8.0) {
            let y = fy * l.content_size(zoom).1;
            let a = l.anchor_at(y, x, zoom);
            prop_assert!(a.page < l.page_count());
            prop_assert_eq!(a.x, x);
            let back = l.y_for_anchor(a, zoom);
            prop_assert!((back - y).abs() <= 1e-6 * y.max(1.0), "{} vs {}", back, y);
        }

        #[test]
        fn visible_matches_brute_force(l in layout(), y0 in -100.0f64..60000.0, len in 0.0f64..5000.0, zoom in 0.05f64..4.0) {
            let y1 = y0 + len;
            let expected: Vec<u32> = (0..l.page_count())
                .filter(|&p| {
                    let (_, top, _, h) = l.page_rect(p, zoom, 0.0);
                    y0 < y1 && top < y1 && top + h > y0
                })
                .collect();
            let got: Vec<u32> = l.visible(y0, y1, zoom).collect();
            prop_assert_eq!(got, expected);
        }
    }
}
