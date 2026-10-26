//! `PdfView` (ARCHITECTURE §6): one custom widget implementing `gtk::Scrollable` that draws every
//! page of the document from textures rendered on the PDF thread. There is no widget per page.
//!
//! - **Layout:** continuous vertical scrolling ([`PageLayout`]), fit-width by default. The zoom
//!   follows the view's size ([`Fit`]): its width (Ctrl+0), the whole page (for slides), or
//!   neither once zoomed (Ctrl+1 is 100%). Left and Right (h and l) scroll sideways.
//! - **Zoom:** Ctrl+wheel, pinch ([`gtk::GestureZoom`]) and the toolbar, anchored at the pointer.
//!   The textures on screen are stretched at once; the pages are re-rendered at
//!   `scale_q = round(zoom × surface scale × 8)` after [`RERENDER_DELAY`] without zoom changes.
//!   Pages that come into view meanwhile without a texture to stretch (zooming out) are rendered
//!   at once at that scale.
//! - **Render requests:** the visible pages from the centre outward, then the pages just before
//!   and after (as far as their bitmaps fit in the cache beside the visible ones), are sent
//!   through the request callback (the controller forwards them to the PDF thread as one
//!   latest-wins `Want`). Pages too large for one bitmap are tiled in full-width bands (one
//!   poppler pass each, so a few large bands cost far less than many small tiles; zoomed in
//!   past 4096 px, in tiles of about 1024²) and requested as the visible bands, then the band
//!   that scrolling down shows next, on the same page or the next one, if it fits in the cache.
//!   Nothing is requested while the view is not mapped (a hidden pane).
//! - **Textures** (`MemoryTexture` over the worker's pixel buffer, no copy) live in a byte-bounded
//!   LRU, where the textures the view needs are never evicted for others. While a page has no
//!   texture (or not all its visible tiles) of the current scale or generation, the best other
//!   ones are stretched in its place, so zooming and rebuilding never flash white pages.
//! - **Reload:** a new generation of the same PDF keeps the scroll [`Anchor`] (page and
//!   fraction), clamped when the page count changes; another PDF starts at the top. A rebuild with
//!   as many pages shows with the old page sizes until the PDF thread has read its own, and keeps
//!   the anchor when they differ ([`PdfView::set_page_sizes`]).
//! - **Dark mode:** the pages are requested dark ([`PageKey::dark`]) and the PDF thread colours
//!   the bitmaps (invert, then hue-rotate 180°, [`lemma_pdf::dark`]), so they are drawn as they
//!   are. (A colour matrix over the view would cost GTK's cairo renderer, the default, a per-pixel
//!   pass over the whole view on every scrolled frame: 12–30 ms, so scrolling missed frames.)
//!   After a toggle, the textures of the other mode stand in through a colour matrix until the
//!   pages are re-rendered, so the switch is immediate.
//! - **SyncTeX:** forward-search boxes are scrolled to a third of the view and flashed for
//!   [`FLASH_DURATION`]; Ctrl+click reports the page point under the pointer for inverse search,
//!   and [`PdfView::inverse_search_current`] the middle of what the view shows of the current
//!   page.
//! - **Search (M3):** the matches of the pages on screen are drawn over them, the current one
//!   emphasised ([`PdfView::set_search_matches`], [`PdfView::set_current_match`]).
//! - **Popovers (M3):** popovers parented to the view (the link preview) are positioned on every
//!   allocation and unparented when the view goes.
//! - **Moving to another window (M3):** [`PdfView::hold_anchor`] keeps the place through the
//!   allocations of the new window.

use super::geometry::{self, Rect};
use super::texture_cache::TextureCache;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use lemma_pdf::{
    Anchor, PageKey, PageLayout, RenderReq, Rendered, SearchMatch, page_pixels, scale_q, tile_grid,
    tile_rect,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Space between pages and around them, in logical pixels.
pub const PAGE_GAP: f64 = 12.0;
/// The view re-renders this long after the last zoom change; until then textures are stretched.
pub const RERENDER_DELAY: Duration = Duration::from_millis(120);
/// How long a SyncTeX highlight takes to fade.
pub const FLASH_DURATION: Duration = Duration::from_millis(600);
/// The default texture cache size (`pdf.cache_mib`).
pub const DEFAULT_CACHE_BYTES: usize = 48 << 20;
/// Zoom factor of one mouse-wheel notch.
const WHEEL_ZOOM_STEP: f64 = 1.1;
/// Zoom factor of one zoom-in/out command.
const ZOOM_STEP: f64 = 1.2;
/// Scroll step of the arrow keys, in logical pixels.
const SCROLL_STEP: f64 = 48.0;

/// How the zoom follows the view's size (the session remembers it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// The widest page fills the view's width.
    #[default]
    Width,
    /// The tallest page, with the gaps above and below it, fits the view's height (and no page
    /// is wider than the view).
    Page,
    /// The zoom stays as it was set when the view is resized. (A mode this version does not
    /// know, from a newer one's session, reads as this one.)
    #[serde(other)]
    None,
}

/// The zoom that fits `layout` to a view of `size` as `fit` says; `None` for [`Fit::None`] and
/// before the view has the size it needs.
fn fit_zoom(fit: Fit, layout: &PageLayout, (width, height): (f64, f64)) -> Option<f64> {
    let zoom = match fit {
        Fit::Width if width > 0.0 => layout.fit_width_zoom(width),
        Fit::Page if width > 0.0 && height > 0.0 => layout.fit_page_zoom(width, height),
        _ => return None,
    };
    Some(geometry::clamp_zoom(zoom))
}

/// When [`PdfView::flash_boxes`] scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// Always put the first box at a third of the view height.
    Always,
    /// Only scroll when the first box is not entirely visible (follow-cursor mode).
    IfHidden,
}

/// A rectangle on a page in bp, top-left origin: `(x, y, width, height)`.
pub type PageRect = (f64, f64, f64, f64);

type RequestFn = dyn Fn(Vec<RenderReq>);
type InverseFn = dyn Fn(u32, f64, f64);
type ChangedFn = dyn Fn(&PdfView);

/// The document shown: its generation and page layout.
struct Doc {
    generation: u64,
    layout: PageLayout,
}

/// A SyncTeX highlight being faded out.
struct Flash {
    /// 0-based page.
    page: u32,
    boxes: Vec<PageRect>,
    started: Instant,
}

/// Everything the view draws and requests (no GTK calls happen while it is borrowed).
struct Model {
    doc: Option<Doc>,
    zoom: f64,
    fit: Fit,
    /// The view size the zoom was last fitted to.
    fitted: (f64, f64),
    /// The scale textures are requested at; 0 until the first allocation after a new document.
    target_q: u16,
    device_scale: f64,
    cache: TextureCache,
    /// Requests sent and not delivered yet, in the order sent.
    outstanding: Vec<RenderReq>,
    /// Whole pages requested beyond the view by [`PdfView::prefetch`], until the next update.
    extra: Range<u32>,
    dark: bool,
    flash: Option<Flash>,
    /// Pointer position in widget coordinates while it is over the view.
    pointer: Option<(f64, f64)>,
    /// The zoom when the current pinch gesture began.
    gesture_start_zoom: f64,
    /// The longest `snapshot` so far (for the frame-time acceptance test).
    max_snapshot: Duration,
    /// The bitmaps of the shown generation that arrived from the PDF thread (tests count the
    /// renders a zoom costs).
    received: usize,
    /// The times the page sizes read after a reload laid the document out again (tests check
    /// that a rebuild showed before they came).
    resized: usize,
    /// Search matches per 0-based page (M3).
    search: HashMap<u32, Vec<SearchMatch>>,
    /// The emphasised match: page and index in its page's list.
    current_match: Option<(u32, usize)>,
    /// An anchor kept through relayouts until the instant given (see [`PdfView::hold_anchor`]).
    held: Option<(Anchor, Instant)>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            doc: None,
            zoom: 1.0,
            fit: Fit::Width,
            fitted: (0.0, 0.0),
            target_q: 0,
            device_scale: 1.0,
            cache: TextureCache::new(DEFAULT_CACHE_BYTES),
            outstanding: Vec::new(),
            extra: 0..0,
            dark: false,
            flash: None,
            pointer: None,
            gesture_start_zoom: 1.0,
            max_snapshot: Duration::ZERO,
            received: 0,
            resized: 0,
            search: HashMap::new(),
            current_match: None,
            held: None,
        }
    }
}

impl Model {
    /// The visible page range for a view of `height` scrolled to `scroll_y`.
    fn visible(&self, scroll_y: f64, height: f64) -> Range<u32> {
        match &self.doc {
            Some(doc) => doc.layout.visible(scroll_y, scroll_y + height, self.zoom),
            None => 0..0,
        }
    }

    /// True when `page` has its bitmap at the current generation and target scale (for a tiled
    /// page: every tile in `viewport`, in document coordinates).
    fn page_complete(&self, page: u32, view_width: f64, viewport: Rect) -> bool {
        let Some(doc) = &self.doc else {
            return false;
        };
        let key = |tile| PageKey {
            generation: doc.generation,
            page,
            scale_q: self.target_q,
            tile,
            dark: self.dark,
        };
        let Some(size) = doc.layout.page_size(page) else {
            return false;
        };
        if tile_grid(size, self.target_q).is_none() {
            return self.cache.contains(&key(None));
        }
        let rect = page_rect(&doc.layout, page, self.zoom, view_width);
        geometry::visible_tiles(size, self.target_q, rect, viewport)
            .into_iter()
            .all(|tile| self.cache.contains(&key(Some(tile))))
    }

    /// The scale to request `page` at (in a view of `view_width` showing `viewport`): the target
    /// scale, except while a zoom waits for its re-render ([`RERENDER_DELAY`]). Then a page with
    /// a texture of the shown generation and mode to stand in keeps that scale and waits for the
    /// re-render: the target scale, or that of a whole page or of tiles rendered earlier in the
    /// same zoom (a pinch, several Ctrl+wheel notches), so that a page in bands gets the ones it
    /// lacks at the scale of its first, and [`Model::fallback_tiles`] draws them all. A page
    /// without one (it came into view as the zoom went out) is rendered at once at the zoom's
    /// own scale, which is what the re-render will want, rather than at the old scale only to be
    /// rendered again 120 ms later. So is a page that is one bitmap at the zoom's scale while its
    /// tiles at the target scale do not cover it in view (the band rendered ahead below the view,
    /// or the bands in view before the zoom went out), unless a whole page of another scale
    /// stands in: one bitmap now costs less than the tiles it lacks at the old scale, which the
    /// re-render would replace with that bitmap.
    fn request_q(&self, page: u32, generation: u64, view_width: f64, viewport: Rect) -> u16 {
        let live = scale_q(self.zoom, self.device_scale);
        if live == self.target_q {
            return live;
        }
        let keys = self
            .cache
            .keys_of_page(page)
            .iter()
            .filter(|k| k.generation == generation && k.dark == self.dark);
        let whole_at_live = self
            .doc
            .as_ref()
            .and_then(|d| d.layout.page_size(page))
            .is_some_and(|size| tile_grid(size, live).is_none());
        if whole_at_live && !self.page_complete(page, view_width, viewport) {
            // Whole: at the scale of a whole page that stands in, else at the zoom's.
            return keys
                .filter(|k| k.tile.is_none())
                .map(|k| k.scale_q)
                .min_by_key(|q| q.abs_diff(live))
                .unwrap_or(live);
        }
        // Tiles of this zoom are of a scale between the target's and the zoom's. (Tiles left
        // from an earlier zoom can be of any scale, and the tiles a page lacks would then be
        // rendered at that scale, however large.)
        let zoomed = live.min(self.target_q)..=live.max(self.target_q);
        keys.filter(|k| k.tile.is_none() || zoomed.contains(&k.scale_q))
            // The target scale first, then a whole page (the one nearest the zoom's scale), then
            // the tiles that `fallback_tiles` draws, those nearest the target scale. (A band asked
            // for at one notch can arrive after the next notch asked for the page at its own
            // scale; growing the later group would leave it undrawn.)
            .min_by_key(|k| {
                let near = match k.tile {
                    None => k.scale_q.abs_diff(live),
                    Some(_) => k.scale_q.abs_diff(self.target_q),
                };
                (k.scale_q != self.target_q, k.tile.is_some(), near)
            })
            .map(|k| k.scale_q)
            .unwrap_or(live)
    }

    /// The bitmaps the current view needs, most urgent first: those of the visible pages (for a
    /// tiled page, its visible tiles), then the band of a tiled page that scrolling down shows
    /// next, then the whole pages just after and before them and the pages `extra`, as long as
    /// all of these bitmaps fit in the cache. Each page is keyed at [`Model::request_q`].
    fn working_set(&self, view: (f64, f64), scroll: (f64, f64), extra: Range<u32>) -> Vec<PageKey> {
        let Some(doc) = &self.doc else {
            return Vec::new();
        };
        if self.target_q == 0 {
            return Vec::new();
        }
        let (width, height) = view;
        let viewport = Rect::new(scroll.0, scroll.1, width, height);
        let visible = self.visible(scroll.1, height);
        let center = doc
            .layout
            .anchor_at(scroll.1 + height / 2.0, scroll.0, self.zoom)
            .page;
        let count = doc.layout.page_count();
        let mut pages = geometry::render_order(visible.clone(), center, count);
        let extra: Vec<u32> = extra.filter(|p| *p < count && !pages.contains(p)).collect();
        pages.extend(extra);
        let cap = self.cache.cap();
        let mut bytes = 0usize;
        let mut keys = Vec::new();
        let key = |page, scale_q, tile| PageKey {
            generation: doc.generation,
            page,
            scale_q,
            tile,
            dark: self.dark,
        };
        let add_page = |page: u32, bytes: &mut usize, keys: &mut Vec<PageKey>| {
            let Some(size) = doc.layout.page_size(page) else {
                return;
            };
            let scale_q = self.request_q(page, doc.generation, width, viewport);
            if tile_grid(size, scale_q).is_some() {
                // Tiled pages: the visible tiles of the visible pages.
                if visible.contains(&page) {
                    let rect = page_rect(&doc.layout, page, self.zoom, width);
                    for tile in geometry::visible_tiles(size, scale_q, rect, viewport) {
                        *bytes += bitmap_bytes(size, scale_q, Some(tile));
                        keys.push(key(page, scale_q, Some(tile)));
                    }
                }
            } else {
                // The visible pages always; the others only while everything fits, so that a
                // page ahead never pushes a page on screen out of the cache.
                let page_bytes = bitmap_bytes(size, scale_q, None);
                if visible.contains(&page) || *bytes + page_bytes <= cap {
                    *bytes += page_bytes;
                    keys.push(key(page, scale_q, None));
                }
            }
        };
        // The visible pages come first in `render_order`, then the pages beyond the view.
        let ahead = pages
            .iter()
            .position(|p| !visible.contains(p))
            .unwrap_or(pages.len());
        for &page in &pages[..ahead] {
            add_page(page, &mut bytes, &mut keys);
        }
        // Ahead of the view, the band below it first: on the last visible page, or the first of
        // the page after it. Like a page ahead, only while it fits.
        for (page, scale_q, tile) in self.next_band(doc, viewport, visible.clone()) {
            let Some(size) = doc.layout.page_size(page) else {
                continue;
            };
            let band_bytes = bitmap_bytes(size, scale_q, Some(tile));
            if bytes + band_bytes <= cap {
                bytes += band_bytes;
                keys.push(key(page, scale_q, Some(tile)));
            }
        }
        for &page in &pages[ahead..] {
            add_page(page, &mut bytes, &mut keys);
        }
        keys
    }

    /// The tiles of the band that scrolling down shows next, with their page and scale, when it
    /// is on a tiled page: the row below the view on the last visible page, or once that page's
    /// last row is in view, the first row of the page after it. Empty when that page is not
    /// tiled (it is asked for whole) or there is none, and while a zoom waits for its re-render,
    /// when the page keeps another scale than the zoom's (see [`Model::request_q`]): the
    /// re-render would render that band again.
    fn next_band(
        &self,
        doc: &Doc,
        viewport: Rect,
        visible: Range<u32>,
    ) -> Vec<(u32, u16, (u16, u16))> {
        if visible.is_empty() {
            return Vec::new();
        }
        let live = scale_q(self.zoom, self.device_scale);
        for page in [visible.end - 1, visible.end] {
            let Some(size) = doc.layout.page_size(page) else {
                continue;
            };
            // The viewport is as wide as the view.
            let scale_q = self.request_q(page, doc.generation, viewport.width, viewport);
            if tile_grid(size, scale_q).is_none() {
                // A whole page; the page after it may still be tiled (pages of other sizes).
                continue;
            }
            let rect = page_rect(&doc.layout, page, self.zoom, viewport.width);
            let band = geometry::tiles_below(size, scale_q, rect, viewport);
            if band.is_empty() {
                continue;
            }
            // The page keeps its scale until the re-render, which would render this band again.
            if scale_q != live {
                return Vec::new();
            }
            return band.into_iter().map(|tile| (page, scale_q, tile)).collect();
        }
        Vec::new()
    }

    /// The render requests for the current view, most urgent first, without cached bitmaps.
    fn wants(&self, view: (f64, f64), scroll: (f64, f64), extra: Range<u32>) -> Vec<RenderReq> {
        self.working_set(view, scroll, extra)
            .into_iter()
            .filter(|key| !self.cache.contains(key))
            .map(|key| RenderReq { key })
            .collect()
    }

    /// How well a texture of `key`'s generation, scale and mode stands in for the page wanted
    /// (smaller is better): the current generation before older ones, then the scale nearest
    /// the target, then the current mode (the other one needs a colour matrix), then the newest
    /// generation.
    fn stand_in_rank(&self, key: &PageKey, generation: u64) -> (bool, u16, bool, u64) {
        (
            key.generation != generation,
            key.scale_q.abs_diff(self.target_q),
            key.dark != self.dark,
            u64::MAX - key.generation,
        )
    }

    /// The best whole-page texture to stand in for `page` (see [`Model::stand_in_rank`]).
    fn fallback_key(&self, page: u32, generation: u64) -> Option<PageKey> {
        self.cache
            .keys_of_page(page)
            .iter()
            .filter(|k| k.tile.is_none())
            .min_by_key(|k| self.stand_in_rank(k, generation))
            .copied()
    }

    /// The cached tiles of `page` of the generation, scale and mode that stand in best for it
    /// (see [`Model::stand_in_rank`]), other than the current generation at the target scale in
    /// the current mode.
    fn fallback_tiles(&self, page: u32, generation: u64) -> Vec<PageKey> {
        let keys = self.cache.keys_of_page(page);
        let best = keys
            .iter()
            .filter(|k| k.tile.is_some() && !self.is_current(k, generation))
            .min_by_key(|k| self.stand_in_rank(k, generation));
        let Some(best) = best.copied() else {
            return Vec::new();
        };
        keys.iter()
            .filter(|k| {
                k.tile.is_some()
                    && k.generation == best.generation
                    && k.scale_q == best.scale_q
                    && k.dark == best.dark
            })
            .copied()
            .collect()
    }

    /// What `draw` puts on the visible `page` of `generation` (in a view of `view_width` showing
    /// `viewport`), bottom to top: whether paper goes first (a page not complete, with no whole
    /// page to stand in), then the textures: the stand-ins while the page is not complete, then
    /// its current tiles.
    fn layers(
        &self,
        page: u32,
        generation: u64,
        view_width: f64,
        viewport: Rect,
    ) -> (bool, Vec<PageKey>) {
        let size = self
            .doc
            .as_ref()
            .and_then(|d| d.layout.page_size(page))
            .unwrap_or((1.0, 1.0));
        let tiled = tile_grid(size, self.target_q).is_some();
        let mut paper = false;
        let mut layers = Vec::new();
        if !self.page_complete(page, view_width, viewport) {
            let whole = self.fallback_key(page, generation);
            let tiles = self.fallback_tiles(page, generation);
            let tiles_better = tiles.first().is_some_and(|t| {
                whole.is_none_or(|w| {
                    self.stand_in_rank(t, generation) < self.stand_in_rank(&w, generation)
                })
            });
            match whole {
                Some(key) => layers.push(key),
                None => paper = true,
            }
            if tiles_better {
                layers.extend(tiles);
            }
        } else if !tiled {
            layers.extend(self.fallback_key(page, generation));
        }
        if tiled {
            layers.extend(
                self.cache
                    .keys_of_page(page)
                    .iter()
                    .copied()
                    .filter(|k| k.tile.is_some() && self.is_current(k, generation)),
            );
        }
        (paper, layers)
    }

    /// True when `key` is of the current generation, scale and mode (what the view requests).
    fn is_current(&self, key: &PageKey, generation: u64) -> bool {
        key.generation == generation && key.scale_q == self.target_q && key.dark == self.dark
    }

    /// The colour of a page without any texture yet: white paper, or dark paper in dark mode.
    fn paper(&self) -> gdk::RGBA {
        if self.dark {
            let paper = lemma_pdf::dark::PAPER;
            gdk::RGBA::new(paper, paper, paper, 1.0)
        } else {
            gdk::RGBA::WHITE
        }
    }
}

/// The colours of search matches: every match, the current one, and the current one's edge
/// (GNOME's yellow 3 and orange 3).
fn search_colors() -> (gdk::RGBA, gdk::RGBA, gdk::RGBA) {
    (
        gdk::RGBA::new(0.965, 0.827, 0.176, 0.40),
        gdk::RGBA::new(1.0, 0.471, 0.0, 0.42),
        gdk::RGBA::new(0.902, 0.380, 0.0, 0.95),
    )
}

/// Bytes of the bitmap of `tile` (or the whole page) of a page of `size_bp` at `scale_q`.
fn bitmap_bytes(size_bp: (f64, f64), scale_q: u16, tile: Option<(u16, u16)>) -> usize {
    let (w, h) = match tile {
        None => page_pixels(size_bp, scale_q),
        Some(tile) => {
            let (_, _, w, h) = tile_rect(size_bp, scale_q, tile);
            (w, h)
        }
    };
    (w.max(0) as usize)
        .saturating_mul(h.max(0) as usize)
        .saturating_mul(4)
}

/// `page`'s rectangle in document coordinates.
fn page_rect(layout: &PageLayout, page: u32, zoom: f64, view_width: f64) -> Rect {
    let (x, y, w, h) = layout.page_rect(page, zoom, view_width);
    Rect::new(x, y, w, h)
}

fn graphene_rect(r: Rect) -> graphene::Rect {
    graphene::Rect::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32)
}

mod imp {
    use super::*;

    #[derive(glib::Properties)]
    #[properties(wrapper_type = super::PdfView)]
    pub struct PdfView {
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_hadjustment, nullable)]
        hadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vadjustment, nullable)]
        vadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        hscroll_policy: Cell<gtk::ScrollablePolicy>,
        #[property(override_interface = gtk::Scrollable, get, set)]
        vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub(super) model: RefCell<Model>,
        adjustment_handlers: RefCell<Vec<(gtk::Adjustment, glib::SignalHandlerId)>>,
        pub(super) request: RefCell<Option<Rc<RequestFn>>>,
        pub(super) inverse: RefCell<Option<Rc<InverseFn>>>,
        pub(super) changed: RefCell<Vec<Rc<ChangedFn>>>,
        update_scheduled: Cell<bool>,
        rerender_timer: RefCell<Option<glib::SourceId>>,
        flash_tick: RefCell<Option<gtk::TickCallbackId>>,
        surface_handler: RefCell<Option<(gdk::Surface, glib::SignalHandlerId)>>,
    }

    impl Default for PdfView {
        fn default() -> Self {
            Self {
                hadjustment: RefCell::new(None),
                vadjustment: RefCell::new(None),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                model: RefCell::new(Model::default()),
                adjustment_handlers: RefCell::new(Vec::new()),
                request: RefCell::new(None),
                inverse: RefCell::new(None),
                changed: RefCell::new(Vec::new()),
                update_scheduled: Cell::new(false),
                rerender_timer: RefCell::new(None),
                flash_tick: RefCell::new(None),
                surface_handler: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PdfView {
        const NAME: &'static str = "LemmaPdfView";
        type Type = super::PdfView;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("pdfview");
            klass.set_accessible_role(gtk::AccessibleRole::Document);
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for PdfView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.set_focusable(true);
            obj.add_css_class("lemma-pdf-view");
            obj.install_controllers();
        }

        fn dispose(&self) {
            // Popovers parented to the view (the link preview).
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
            if let Some(timer) = self.rerender_timer.take() {
                timer.remove();
            }
            if let Some(tick) = self.flash_tick.take() {
                tick.remove();
            }
            for (adjustment, handler) in self.adjustment_handlers.take() {
                adjustment.disconnect(handler);
            }
        }
    }

    impl WidgetImpl for PdfView {
        fn realize(&self) {
            self.parent_realize();
            let obj = self.obj();
            let Some(surface) = obj.native().and_then(|n| n.surface()) else {
                return;
            };
            tracing::info!("PDF view: surface scale {}", surface.scale());
            let weak = obj.downgrade();
            let handler = surface.connect_scale_notify(move |surface| {
                if let Some(view) = weak.upgrade() {
                    tracing::info!("PDF view: surface scale changed to {}", surface.scale());
                    view.imp().set_device_scale(surface.scale());
                }
            });
            self.set_device_scale(surface.scale());
            self.surface_handler.replace(Some((surface, handler)));
        }

        fn unrealize(&self) {
            if let Some((surface, handler)) = self.surface_handler.take() {
                surface.disconnect(handler);
            }
            self.parent_unrealize();
        }

        fn map(&self) {
            self.parent_map();
            // Nothing is requested while unmapped (a hidden pane); catch up now.
            self.schedule_update();
        }

        fn unmap(&self) {
            // A hidden pane needs no bitmaps: stop the PDF thread rendering for it.
            self.cancel_requests();
            self.parent_unmap();
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // A scrollable child: the scrolled window gives it whatever space there is.
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            self.relayout(f64::from(width), f64::from(height));
            // Popovers parented to the view follow it.
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>()
                    && popover.is_visible()
                {
                    popover.present();
                }
                child = widget.next_sibling();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let started = Instant::now();
            self.draw(snapshot);
            let mut model = self.model.borrow_mut();
            model.max_snapshot = model.max_snapshot.max(started.elapsed());
        }
    }

    impl ScrollableImpl for PdfView {}

    impl PdfView {
        fn set_hadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            self.set_adjustment(&self.hadjustment, adjustment);
        }

        fn set_vadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            self.set_adjustment(&self.vadjustment, adjustment);
        }

        fn set_adjustment(
            &self,
            slot: &RefCell<Option<gtk::Adjustment>>,
            adjustment: Option<gtk::Adjustment>,
        ) {
            if let Some(old) = slot.borrow().as_ref() {
                let mut handlers = self.adjustment_handlers.borrow_mut();
                if let Some(i) = handlers.iter().position(|(a, _)| a == old) {
                    let (a, handler) = handlers.remove(i);
                    a.disconnect(handler);
                }
            }
            if let Some(adjustment) = &adjustment {
                let weak = self.obj().downgrade();
                let handler = adjustment.connect_value_changed(move |_| {
                    if let Some(view) = weak.upgrade() {
                        view.queue_draw();
                        view.imp().schedule_update();
                        view.imp().emit_changed();
                    }
                });
                self.adjustment_handlers
                    .borrow_mut()
                    .push((adjustment.clone(), handler));
            }
            slot.replace(adjustment);
            let (x, y) = self.scroll();
            self.configure_adjustments(x, y);
        }

        pub(super) fn hadj(&self) -> Option<gtk::Adjustment> {
            self.hadjustment.borrow().clone()
        }

        pub(super) fn vadj(&self) -> Option<gtk::Adjustment> {
            self.vadjustment.borrow().clone()
        }

        /// The scroll offsets (document coordinates of the view's top-left corner).
        pub(super) fn scroll(&self) -> (f64, f64) {
            (
                self.hadj().map_or(0.0, |a| a.value()),
                self.vadj().map_or(0.0, |a| a.value()),
            )
        }

        /// The widget size in logical pixels.
        pub(super) fn view_size(&self) -> (f64, f64) {
            let obj = self.obj();
            (f64::from(obj.width()), f64::from(obj.height()))
        }

        /// Sets both adjustments for the current content size and scrolls to `(x, y)`
        /// (clamped). Must not be called while the model is borrowed: it emits `value-changed`.
        pub(super) fn configure_adjustments(&self, x: f64, y: f64) {
            let (width, height) = self.view_size();
            let (content_w, content_h) = {
                let model = self.model.borrow();
                model
                    .doc
                    .as_ref()
                    .map_or((0.0, 0.0), |d| d.layout.content_size(model.zoom))
            };
            // At fit width the content is as wide as the view up to rounding: no horizontal
            // range for that, or a touchpad scroll moves (and overshoots) sideways.
            let content_w = if content_w < width + 0.5 {
                width
            } else {
                content_w
            };
            if let Some(h) = self.hadj() {
                h.configure(x, 0.0, content_w, SCROLL_STEP, width * 0.9, width);
            }
            if let Some(v) = self.vadj() {
                v.configure(
                    y,
                    0.0,
                    content_h.max(height),
                    SCROLL_STEP,
                    height * 0.9,
                    height,
                );
            }
        }

        /// Fits the zoom to a new size (as the [`Fit`] mode says), keeping the scroll anchor, and
        /// configures the adjustments.
        pub(super) fn relayout(&self, width: f64, height: f64) {
            let (x, y) = self.scroll();
            let mut refit = false;
            let mut first_render = false;
            let (x, y, stale) = {
                let mut model = self.model.borrow_mut();
                let model = &mut *model;
                let mut target = (x, y);
                // A held anchor (a view moved to another window) wins over the scroll offsets,
                // which the allocations in between may have clamped.
                if model.held.is_some_and(|(_, until)| Instant::now() >= until) {
                    model.held = None;
                }
                let held = model.held.map(|(anchor, _)| anchor);
                let resized = match model.fit {
                    Fit::Width => width != model.fitted.0,
                    Fit::Page => (width, height) != model.fitted,
                    Fit::None => false,
                };
                if let Some(doc) = &model.doc
                    && resized
                    && let Some(zoom) = fit_zoom(model.fit, &doc.layout, (width, height))
                {
                    let anchor = held.unwrap_or_else(|| doc.layout.anchor_at(y, x, model.zoom));
                    refit = zoom != model.zoom;
                    model.zoom = zoom;
                    model.fitted = (width, height);
                    target = (anchor.x, doc.layout.y_for_anchor(anchor, zoom));
                } else if let (Some(doc), Some(anchor)) = (&model.doc, held) {
                    target = (anchor.x, doc.layout.y_for_anchor(anchor, model.zoom));
                }
                if model.doc.is_some() && model.target_q == 0 && width > 0.0 {
                    model.target_q = scale_q(model.zoom, model.device_scale);
                    first_render = true;
                }
                // The render scale lags the zoom (e.g. after a rebuild changed the fitted zoom).
                let stale = model.target_q != 0
                    && model.target_q != scale_q(model.zoom, model.device_scale);
                (target.0, target.1, stale)
            };
            self.configure_adjustments(x, y);
            let pending = self.rerender_timer.borrow().is_some();
            if !first_render && (refit || (stale && !pending)) {
                self.schedule_rerender();
            }
            if width > 0.0 && height > 0.0 {
                self.schedule_update();
            }
            if refit {
                self.emit_changed();
            }
        }

        fn set_device_scale(&self, scale: f64) {
            let changed = {
                let mut model = self.model.borrow_mut();
                let changed = scale > 0.0 && scale != model.device_scale;
                if changed {
                    model.device_scale = scale;
                    if model.target_q != 0 {
                        model.target_q = scale_q(model.zoom, scale);
                    }
                }
                changed
            };
            if changed {
                self.schedule_update();
                self.obj().queue_draw();
            }
        }

        /// Recomputes the render requests at the next idle (coalesces scroll events).
        pub(super) fn schedule_update(&self) {
            if self.update_scheduled.replace(true) {
                return;
            }
            let weak = self.obj().downgrade();
            glib::idle_add_local_once(move || {
                if let Some(view) = weak.upgrade() {
                    view.imp().update_scheduled.set(false);
                    view.imp().update_wants(0..0);
                }
            });
        }

        /// Sends the current render requests (plus whole pages `extra`) unless they equal the
        /// outstanding ones.
        pub(super) fn update_wants(&self, extra: Range<u32>) {
            let view = self.view_size();
            if view.0 <= 0.0 || view.1 <= 0.0 || !self.obj().is_mapped() {
                return;
            }
            let scroll = self.scroll();
            let wants = {
                let mut model = self.model.borrow_mut();
                model.extra = extra.clone();
                let wants = model.wants(view, scroll, extra);
                if wants == model.outstanding {
                    return;
                }
                model.outstanding = wants.clone();
                wants
            };
            let request = self.request.borrow().clone();
            if let Some(request) = request {
                request(wants);
            }
        }

        /// Withdraws the requests still outstanding (sends an empty request list).
        pub(super) fn cancel_requests(&self) {
            let had = {
                let mut model = self.model.borrow_mut();
                model.extra = 0..0;
                !std::mem::take(&mut model.outstanding).is_empty()
            };
            let request = self.request.borrow().clone();
            if had && let Some(request) = request {
                request(Vec::new());
            }
        }

        /// Re-renders at the current zoom after [`RERENDER_DELAY`] without further changes.
        pub(super) fn schedule_rerender(&self) {
            if let Some(timer) = self.rerender_timer.take() {
                timer.remove();
            }
            let weak = self.obj().downgrade();
            let timer = glib::timeout_add_local_once(RERENDER_DELAY, move || {
                if let Some(view) = weak.upgrade() {
                    let imp = view.imp();
                    imp.rerender_timer.take();
                    {
                        let mut model = imp.model.borrow_mut();
                        if model.target_q != 0 {
                            model.target_q = scale_q(model.zoom, model.device_scale);
                        }
                    }
                    imp.update_wants(0..0);
                    view.queue_draw();
                }
            });
            self.rerender_timer.replace(Some(timer));
        }

        pub(super) fn emit_changed(&self) {
            let callbacks = self.changed.borrow().clone();
            let obj = self.obj();
            for callback in callbacks {
                callback(&obj);
            }
        }

        /// Keeps redrawing while a highlight fades.
        pub(super) fn animate_flash(&self) {
            if self.flash_tick.borrow().is_some() {
                return;
            }
            let id = self.obj().add_tick_callback(|view, _clock| {
                view.queue_draw();
                let imp = view.imp();
                let mut model = imp.model.borrow_mut();
                let done = model
                    .flash
                    .as_ref()
                    .is_none_or(|f| f.started.elapsed() >= FLASH_DURATION);
                if done {
                    model.flash = None;
                    drop(model);
                    imp.flash_tick.take();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            self.flash_tick.replace(Some(id));
        }

        fn draw(&self, snapshot: &gtk::Snapshot) {
            let (width, height) = self.view_size();
            let (sx, sy) = self.scroll();
            let mut guard = self.model.borrow_mut();
            let model = &mut *guard;
            let Some(doc) = &model.doc else {
                return;
            };
            let scale = model.device_scale.max(1.0);
            let snap = |v: f64| (v * scale).round() / scale;
            let pages: Vec<(u32, Rect)> = model
                .visible(sy, height)
                .map(|page| {
                    let r = page_rect(&doc.layout, page, model.zoom, width);
                    (
                        page,
                        Rect::new(snap(r.x - sx), snap(r.y - sy), r.width, r.height),
                    )
                })
                .collect();

            let shadow = if model.dark {
                gdk::RGBA::new(0.0, 0.0, 0.0, 0.5)
            } else {
                gdk::RGBA::new(0.0, 0.0, 0.0, 0.22)
            };
            for (_, rect) in &pages {
                let outline = gsk::RoundedRect::from_rect(graphene_rect(*rect), 0.0);
                snapshot.append_outset_shadow(&outline, &shadow, 0.0, 1.0, 0.0, 4.0);
            }

            let generation = doc.generation;
            let viewport = Rect::new(sx, sy, width, height);
            let paper = model.paper();
            for &(page, rect) in &pages {
                let size = doc.layout.page_size(page).unwrap_or((1.0, 1.0));
                // Bottom to top: stand-ins while the page is incomplete, then the current tiles.
                let (paper_first, layers) = model.layers(page, generation, width, viewport);
                if paper_first {
                    snapshot.append_color(&paper, &graphene_rect(rect));
                }
                for key in layers {
                    let Some(texture) = model.cache.get(&key) else {
                        continue;
                    };
                    let r = match key.tile {
                        None => rect,
                        Some(tile) => {
                            let r = geometry::tile_view_rect(size, key.scale_q, tile, rect);
                            Rect::new(snap(r.x), snap(r.y), r.width, r.height)
                        }
                    };
                    // A texture of the other mode (just after a toggle) is recoloured; the
                    // others are drawn as they are.
                    let recolour = (key.dark != model.dark).then(|| {
                        if model.dark {
                            geometry::dark_color_transform()
                        } else {
                            geometry::light_color_transform()
                        }
                    });
                    if let Some((matrix, offset)) = recolour {
                        snapshot.push_color_matrix(
                            &graphene::Matrix::from_float(matrix),
                            &graphene::Vec4::from_float(offset),
                        );
                    }
                    append_texture(snapshot, &texture, r, scale);
                    if recolour.is_some() {
                        snapshot.pop();
                    }
                }
            }

            // Search matches, over the pages (and not inverted in dark mode).
            if !model.search.is_empty() {
                let z = model.zoom;
                let (fill, current_fill, current_edge) = search_colors();
                for &(page, rect) in &pages {
                    let Some(matches) = model.search.get(&page) else {
                        continue;
                    };
                    for (i, parts) in matches.iter().enumerate() {
                        let current = model.current_match == Some((page, i));
                        for &(x, y, w, h) in parts {
                            let r = Rect::new(
                                rect.x + x * z - 1.0,
                                rect.y + y * z - 1.0,
                                w * z + 2.0,
                                h * z + 2.0,
                            );
                            let rounded = gsk::RoundedRect::from_rect(graphene_rect(r), 2.0);
                            snapshot.push_rounded_clip(&rounded);
                            snapshot.append_color(
                                if current { &current_fill } else { &fill },
                                &graphene_rect(r),
                            );
                            snapshot.pop();
                            if current {
                                snapshot.append_border(&rounded, &[1.5; 4], &[current_edge; 4]);
                            }
                        }
                    }
                }
            }

            if let Some(flash) = &model.flash {
                let t = flash.started.elapsed().as_secs_f64() / FLASH_DURATION.as_secs_f64();
                if t < 1.0
                    && let Some((_, page)) = pages.iter().find(|(p, _)| *p == flash.page)
                {
                    let alpha = (1.0 - t * t) as f32;
                    let fill = gdk::RGBA::new(0.96, 0.83, 0.18, 0.45 * alpha);
                    let edge = gdk::RGBA::new(0.90, 0.65, 0.04, 0.9 * alpha);
                    for &(x, y, w, h) in &flash.boxes {
                        let z = model.zoom;
                        // Zero-area boxes (SyncTeX reports some) still get a visible mark.
                        let r = Rect::new(
                            page.x + x * z - 2.0,
                            page.y + y * z - 2.0,
                            (w * z).max(2.0) + 4.0,
                            (h * z).max(2.0) + 4.0,
                        );
                        let rounded = gsk::RoundedRect::from_rect(graphene_rect(r), 3.0);
                        snapshot.push_rounded_clip(&rounded);
                        snapshot.append_color(&fill, &graphene_rect(r));
                        snapshot.pop();
                        snapshot.append_border(&rounded, &[1.0; 4], &[edge; 4]);
                    }
                }
            }
        }
    }

    /// Draws `texture` stretched to `rect`, with mipmaps when it is shrunk noticeably.
    fn append_texture(snapshot: &gtk::Snapshot, texture: &gdk::Texture, rect: Rect, scale: f64) {
        let shrink = f64::from(texture.width()) > rect.width * scale * 1.25;
        let filter = if shrink {
            gsk::ScalingFilter::Trilinear
        } else {
            gsk::ScalingFilter::Linear
        };
        snapshot.append_scaled_texture(texture, filter, &graphene_rect(rect));
    }
}

glib::wrapper! {
    /// The continuous PDF preview (see the module documentation).
    pub struct PdfView(ObjectSubclass<imp::PdfView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl PdfView {
    /// An empty view whose texture cache holds at most `cache_bytes`.
    pub fn new(cache_bytes: usize) -> Self {
        let view: Self = glib::Object::new();
        view.imp().model.borrow_mut().cache = TextureCache::new(cache_bytes);
        view
    }

    /// Sets the callback that receives render requests (most urgent first). Each call replaces
    /// the previous requests, like `PdfCmd::Want`.
    pub fn set_request_handler(&self, f: impl Fn(Vec<RenderReq>) + 'static) {
        self.imp().request.replace(Some(Rc::new(f)));
    }

    /// Sets the callback for Ctrl+click: 0-based page and the point in bp (top-left origin).
    pub fn set_inverse_handler(&self, f: impl Fn(u32, f64, f64) + 'static) {
        self.imp().inverse.replace(Some(Rc::new(f)));
    }

    /// Calls `f` whenever the page shown, the zoom, the document or dark mode changes.
    pub fn connect_changed(&self, f: impl Fn(&PdfView) + 'static) {
        self.imp().changed.borrow_mut().push(Rc::new(f));
    }

    /// Shows another document than the one shown (if any), of `generation` with the given page
    /// sizes (bp): the view starts at the top, fitted to its width, and the previous document's
    /// textures are dropped.
    pub fn open_document(&self, generation: u64, page_sizes: Vec<(f64, f64)>) {
        {
            let mut model = self.imp().model.borrow_mut();
            model.doc = None;
            model.cache.retain(|_| false);
        }
        self.set_document(generation, page_sizes);
    }

    /// Shows the document of `generation` with the given page sizes (bp).
    ///
    /// When a document is already shown (a rebuilt PDF), the scroll anchor (page and fraction
    /// of it at the top of the view) is kept, clamped to the new page count, and the old
    /// textures stay on screen until the new ones arrive. Otherwise the view starts at the top,
    /// fitted to its width. (For another PDF, use [`PdfView::open_document`].)
    pub fn set_document(&self, generation: u64, page_sizes: Vec<(f64, f64)>) {
        self.lay_out(generation, page_sizes, true);
    }

    /// The document shown, of `generation`, has other page sizes than it is laid out with
    /// (`PdfEvent::PageSizes`: a reload showed with the sizes of the generation before, and the
    /// PDF thread has read its pages since). It is laid out again with them, keeping the scroll
    /// anchor as a rebuild does; the textures stay. Sizes of another generation, or the same
    /// ones, change nothing.
    pub fn set_page_sizes(&self, generation: u64, page_sizes: Vec<(f64, f64)>) {
        let changed = self
            .imp()
            .model
            .borrow()
            .doc
            .as_ref()
            .is_some_and(|d| d.generation == generation && d.layout.sizes() != page_sizes);
        if changed {
            self.imp().model.borrow_mut().resized += 1;
            self.lay_out(generation, page_sizes, false);
        }
    }

    /// Lays out the document of `generation` with `page_sizes`, keeping the scroll anchor of the
    /// document shown (see [`PdfView::set_document`]). A new generation also forgets the requests
    /// outstanding (the PDF thread drops those of older generations) and the SyncTeX highlight.
    fn lay_out(&self, generation: u64, page_sizes: Vec<(f64, f64)>, new_generation: bool) {
        let imp = self.imp();
        let size = imp.view_size();
        let (sx, sy) = imp.scroll();
        let layout = PageLayout::new(page_sizes, PAGE_GAP);
        let (x, y) = {
            let mut model = imp.model.borrow_mut();
            let model = &mut *model;
            let anchor = model
                .doc
                .as_ref()
                .map(|d| d.layout.anchor_at(sy, sx, model.zoom));
            if anchor.is_none() {
                model.fit = Fit::Width;
                model.target_q = 0;
            }
            if model.fit != Fit::None {
                match fit_zoom(model.fit, &layout, size) {
                    Some(zoom) => {
                        model.zoom = zoom;
                        model.fitted = size;
                    }
                    None => model.fitted = (0.0, 0.0),
                }
            }
            // Also for a rebuilt PDF: its widest page, and so the fitted zoom, may have changed.
            if size.0 > 0.0 {
                model.target_q = scale_q(model.zoom, model.device_scale);
            }
            let position = anchor.map_or((0.0, 0.0), |a| (a.x, layout.y_for_anchor(a, model.zoom)));
            model.doc = Some(Doc { generation, layout });
            if new_generation {
                // The PDF thread drops requests of older generations.
                model.outstanding.clear();
                model.flash = None;
            }
            position
        };
        imp.configure_adjustments(x, y);
        imp.schedule_update();
        self.queue_draw();
        imp.emit_changed();
    }

    /// Adds a bitmap from the PDF thread (`PdfEvent::Rendered`) without copying its pixels.
    /// Bitmaps of other generations than the one shown are dropped, and so are bitmaps that
    /// arrive while the view is not mapped (a hidden pane) or that the view no longer needs and
    /// that would push needed ones out of the cache.
    pub fn add_rendered(&self, rendered: Rendered) {
        let Rendered {
            key,
            width,
            height,
            stride,
            data,
        } = rendered;
        let expected = usize::try_from(stride)
            .ok()
            .zip(usize::try_from(height).ok())
            .map(|(s, h)| s * h);
        if width <= 0 || height <= 0 || expected.is_none_or(|n| data.len() < n) {
            tracing::warn!("dropping a malformed page bitmap for {key:?}");
            return;
        }
        let imp = self.imp();
        if !self.is_mapped() {
            imp.model.borrow_mut().outstanding.retain(|r| r.key != key);
            return;
        }
        let view = imp.view_size();
        let (width_px, height_px) = view;
        let scroll = imp.scroll();
        let (sx, sy) = scroll;
        let bytes = glib::Bytes::from_owned(data);
        let size = bytes.len();
        let texture = gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            stride as usize,
        );
        {
            let mut model = imp.model.borrow_mut();
            let Some(generation) = model.doc.as_ref().map(|d| d.generation) else {
                return;
            };
            if key.generation != generation {
                return;
            }
            model.outstanding.retain(|r| r.key != key);
            model.received += 1;
            // The bitmaps the view needs now are never evicted for this one.
            let needed: HashSet<PageKey> = model
                .working_set(view, scroll, model.extra.clone())
                .into_iter()
                .collect();
            model
                .cache
                .insert_keeping(key, texture.upcast(), size, |k| needed.contains(k));
            // Once every visible page of this generation is there in the current mode, older
            // generations and the other mode go.
            let viewport = Rect::new(sx, sy, width_px, height_px);
            let visible = model.visible(sy, height_px);
            if model.cache.len() > 0
                && visible
                    .clone()
                    .all(|p| model.page_complete(p, width_px, viewport))
            {
                let dark = model.dark;
                model
                    .cache
                    .retain(|k| k.generation == generation && k.dark == dark);
            }
        }
        self.queue_draw();
    }

    /// The generation shown, if any.
    pub fn generation(&self) -> Option<u64> {
        self.imp().model.borrow().doc.as_ref().map(|d| d.generation)
    }

    /// Number of pages (0 without a document).
    pub fn page_count(&self) -> u32 {
        self.imp()
            .model
            .borrow()
            .doc
            .as_ref()
            .map_or(0, |d| d.layout.page_count())
    }

    /// A copy of the page layout, if a document is shown.
    pub fn layout(&self) -> Option<PageLayout> {
        self.imp()
            .model
            .borrow()
            .doc
            .as_ref()
            .map(|d| d.layout.clone())
    }

    /// The 0-based page the view shows: the one at the top of the view when scrolled to the
    /// start, at the bottom when scrolled to the end, and in between proportionally (so the
    /// first and last pages are reported even when several pages fit in the view).
    pub fn current_page(&self) -> u32 {
        let imp = self.imp();
        let (sx, sy) = imp.scroll();
        let (_, height) = imp.view_size();
        let model = imp.model.borrow();
        model.doc.as_ref().map_or(0, |d| {
            let content = d.layout.content_size(model.zoom).1;
            d.layout
                .anchor_at(
                    sy + height * geometry::probe_fraction(sy, height, content),
                    sx,
                    model.zoom,
                )
                .page
        })
    }

    /// Scrolls so that the top of the 0-based `page` (with its gap) is at the top of the view.
    pub fn goto_page(&self, page: u32) {
        let imp = self.imp();
        let (width, _) = imp.view_size();
        let y = {
            let mut model = imp.model.borrow_mut();
            model.held = None;
            let Some(doc) = &model.doc else {
                return;
            };
            page_rect(&doc.layout, page, model.zoom, width).y - PAGE_GAP
        };
        if let Some(v) = imp.vadj() {
            v.set_value(y.max(0.0));
        }
    }

    /// The scroll offsets: document coordinates of the view's top-left corner.
    pub fn scroll_position(&self) -> (f64, f64) {
        self.imp().scroll()
    }

    /// Scrolls to document coordinates `(x, y)` (clamped to the scrollable range).
    pub fn set_scroll_position(&self, x: f64, y: f64) {
        let imp = self.imp();
        imp.model.borrow_mut().held = None;
        if let Some(h) = imp.hadj() {
            h.set_value(x);
        }
        if let Some(v) = imp.vadj() {
            v.set_value(y);
        }
    }

    /// The zoom (1.0 = 1 bp per logical pixel).
    pub fn zoom(&self) -> f64 {
        self.imp().model.borrow().zoom
    }

    /// True while the zoom follows the view width.
    pub fn is_fit_width(&self) -> bool {
        self.fit() == Fit::Width
    }

    /// How the zoom follows the view's size.
    pub fn fit(&self) -> Fit {
        self.imp().model.borrow().fit
    }

    /// Sets the zoom, keeping the document point at `anchor` (widget coordinates; the view
    /// centre when `None`) in place. Leaves the fit modes ([`Fit::None`]). The textures on
    /// screen are stretched at once and re-rendered after [`RERENDER_DELAY`].
    pub fn set_zoom(&self, zoom: f64, anchor: Option<(f64, f64)>) {
        let imp = self.imp();
        let (width, height) = imp.view_size();
        let pointer = anchor.unwrap_or((width / 2.0, height / 2.0));
        let scroll = imp.scroll();
        let zoom = geometry::clamp_zoom(zoom);
        let target = {
            let mut model = imp.model.borrow_mut();
            let model = &mut *model;
            model.fit = Fit::None;
            model.held = None;
            if (zoom - model.zoom).abs() < 1e-9 {
                None
            } else {
                let target = model.doc.as_ref().map(|d| {
                    geometry::anchored_scroll(&d.layout, model.zoom, zoom, width, scroll, pointer)
                });
                model.zoom = zoom;
                Some(target.unwrap_or(scroll))
            }
        };
        if let Some((x, y)) = target {
            imp.configure_adjustments(x, y);
            imp.schedule_rerender();
            // Pages that came into view are asked for now, also when the scroll offsets stayed.
            imp.schedule_update();
            self.queue_draw();
        }
        imp.emit_changed();
    }

    /// Multiplies the zoom by `factor` (see [`PdfView::set_zoom`]).
    pub fn zoom_by(&self, factor: f64, anchor: Option<(f64, f64)>) {
        self.set_zoom(self.zoom() * factor, anchor);
    }

    /// One zoom step in, at the view centre.
    pub fn zoom_in(&self) {
        self.zoom_by(ZOOM_STEP, None);
    }

    /// One zoom step out, at the view centre.
    pub fn zoom_out(&self) {
        self.zoom_by(1.0 / ZOOM_STEP, None);
    }

    /// Zooms to 100% (1 bp per logical pixel), at the view centre.
    pub fn reset_zoom(&self) {
        self.set_zoom(1.0, None);
    }

    /// Fits the widest page to the view width and keeps doing so when the view is resized.
    pub fn set_fit_width(&self) {
        self.set_fit(Fit::Width);
    }

    /// Fits the whole page to the view (the tallest page fills its height, unless the widest
    /// would then be wider than the view) and keeps doing so when the view is resized. The page
    /// shown (see [`PdfView::current_page`]) moves to the top, so it is on screen whole.
    pub fn set_fit_page(&self) {
        let page = self.current_page();
        self.set_fit(Fit::Page);
        if self.page_count() > 0 {
            self.goto_page(page);
        }
    }

    fn set_fit(&self, fit: Fit) {
        let imp = self.imp();
        {
            let mut model = imp.model.borrow_mut();
            model.fit = fit;
            model.fitted = (0.0, 0.0);
        }
        let (width, height) = imp.view_size();
        imp.relayout(width, height);
        imp.emit_changed();
    }

    /// Turns dark mode on or off. The pages are re-rendered in the new mode; until they arrive,
    /// the textures of the other mode are drawn recoloured, so the switch shows at once.
    pub fn set_dark(&self, dark: bool) {
        let imp = self.imp();
        let changed = std::mem::replace(&mut imp.model.borrow_mut().dark, dark) != dark;
        if changed {
            imp.schedule_update();
            self.queue_draw();
            imp.emit_changed();
        }
    }

    /// True while pages are drawn in dark mode.
    pub fn is_dark(&self) -> bool {
        self.imp().model.borrow().dark
    }

    /// Highlights SyncTeX `boxes` (bp, top-left origin) on the 0-based `page`, scrolling so that
    /// the first box is at a third of the view height (see [`Reveal`]); the highlight fades over
    /// [`FLASH_DURATION`].
    pub fn flash_boxes(&self, page: u32, boxes: &[PageRect], reveal: Reveal) {
        let Some(&(bx, by, bw, bh)) = boxes.first() else {
            return;
        };
        let imp = self.imp();
        let (width, height) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let target = {
            let mut model = imp.model.borrow_mut();
            let Some(doc) = &model.doc else {
                return;
            };
            if page >= doc.layout.page_count() {
                return;
            }
            let zoom = model.zoom;
            let (x, y) = doc.layout.to_view(page, bx, by, zoom, width);
            let target = Rect::new(x, y, bw * zoom, bh * zoom);
            let view = Rect::new(sx, sy, width, height);
            let scroll = match reveal {
                Reveal::IfHidden if view.contains(&target) => None,
                _ => {
                    let new_x = if target.x >= sx && target.x + target.width <= sx + width {
                        sx
                    } else {
                        target.x + target.width / 2.0 - width / 2.0
                    };
                    Some((new_x, target.y - height / 3.0))
                }
            };
            model.flash = Some(Flash {
                page,
                boxes: boxes.to_vec(),
                started: Instant::now(),
            });
            scroll
        };
        if let Some((x, y)) = target {
            self.set_scroll_position(x, y);
        }
        imp.animate_flash();
        self.queue_draw();
    }

    /// The current SyncTeX highlight: 0-based page and boxes in bp, while it is shown.
    pub fn highlight(&self) -> Option<(u32, Vec<PageRect>)> {
        let model = self.imp().model.borrow();
        model.flash.as_ref().map(|f| (f.page, f.boxes.clone()))
    }

    /// Inverse search at a point in widget coordinates (what Ctrl+click does): calls the
    /// inverse handler with the page point under it. Returns false when the point is not on a
    /// page.
    pub fn inverse_search_at(&self, x: f64, y: f64) -> bool {
        let imp = self.imp();
        let (width, _) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let hit = {
            let model = imp.model.borrow();
            model
                .doc
                .as_ref()
                .and_then(|d| d.layout.to_page(x + sx, y + sy, model.zoom, width))
        };
        let Some((page, x_bp, y_bp)) = hit else {
            return false;
        };
        let inverse = imp.inverse.borrow().clone();
        if let Some(inverse) = inverse {
            inverse(page, x_bp, y_bp);
        }
        true
    }

    /// Inverse search from the page the view shows (the toolbar's Show Page in Source): calls the
    /// inverse handler with [`PdfView::current_page_middle`]. Returns false when no document is
    /// shown.
    pub fn inverse_search_current(&self) -> bool {
        let Some((page, x_bp, y_bp)) = self.current_page_middle() else {
            return false;
        };
        let inverse = self.imp().inverse.borrow().clone();
        if let Some(inverse) = inverse {
            inverse(page, x_bp, y_bp);
        }
        true
    }

    /// The middle of the part of the current page ([`PdfView::current_page`], the toolbar's
    /// page number) that the view shows: 0-based page and point in bp.
    pub fn current_page_middle(&self) -> Option<(u32, f64, f64)> {
        let imp = self.imp();
        let page = self.current_page();
        let (width, height) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let model = imp.model.borrow();
        let rect = page_rect(&model.doc.as_ref()?.layout, page, model.zoom, width);
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return None;
        }
        // From the page's start along one axis; the page's own middle when none of it is shown.
        let middle = |start: f64, len: f64, scroll: f64, extent: f64| {
            let (a, b) = (start.max(scroll), (start + len).min(scroll + extent));
            if a < b {
                (a + b) / 2.0 - start
            } else {
                len / 2.0
            }
        };
        let x = middle(rect.x, rect.width, sx, width);
        let y = middle(rect.y, rect.height, sy, height);
        Some((page, x / model.zoom, y / model.zoom))
    }

    /// The 0-based pages intersecting the view.
    pub fn visible_pages(&self) -> Range<u32> {
        let imp = self.imp();
        let (_, height) = imp.view_size();
        let (_, sy) = imp.scroll();
        imp.model.borrow().visible(sy, height)
    }

    /// True when `page` is drawn from a bitmap of the current generation at the current render
    /// scale in the current mode (for a tiled page: every visible tile).
    pub fn is_page_rendered(&self, page: u32) -> bool {
        let imp = self.imp();
        let (width, height) = imp.view_size();
        let (sx, sy) = imp.scroll();
        imp.model
            .borrow()
            .page_complete(page, width, Rect::new(sx, sy, width, height))
    }

    /// The render scale requested for new bitmaps (`round(zoom × device scale × 8)`; 0 before
    /// the first allocation).
    pub fn render_scale_q(&self) -> u16 {
        self.imp().model.borrow().target_q
    }

    /// The display's scale factor (fractional on fractional-scaling outputs).
    pub fn device_scale(&self) -> f64 {
        self.imp().model.borrow().device_scale
    }

    /// Requests whole pages `pages` too (after the visible ones), e.g. to fill the cache ahead
    /// of scrolling. The next scroll or zoom replaces these requests.
    pub fn prefetch(&self, pages: Range<u32>) {
        self.imp().update_wants(pages);
    }

    /// Render requests sent and not answered yet.
    pub fn outstanding_requests(&self) -> usize {
        self.imp().model.borrow().outstanding.len()
    }

    /// The keys of the render requests sent and not answered yet, in the order sent.
    pub fn outstanding_keys(&self) -> Vec<PageKey> {
        let model = self.imp().model.borrow();
        model.outstanding.iter().map(|r| r.key).collect()
    }

    /// The bitmaps of the shown generation that have arrived from the PDF thread since the view
    /// was created.
    pub fn bitmaps_received(&self) -> usize {
        self.imp().model.borrow().received
    }

    /// How many times [`PdfView::set_page_sizes`] laid the document out again since the view
    /// was created.
    pub fn page_size_updates(&self) -> usize {
        self.imp().model.borrow().resized
    }

    /// The keys of the cached textures of the 0-based `page`, in no particular order.
    pub fn cached_keys(&self, page: u32) -> Vec<PageKey> {
        self.imp().model.borrow().cache.keys_of_page(page).to_vec()
    }

    /// The textures drawn for the 0-based `page` while it is visible, bottom to top: the ones
    /// that stand in while it is not complete, then its current tiles (tests check what a zoom
    /// shows while it waits for its re-render).
    pub fn drawn_keys(&self, page: u32) -> Vec<PageKey> {
        let imp = self.imp();
        let (width, height) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let model = imp.model.borrow();
        let Some(generation) = model.doc.as_ref().map(|d| d.generation) else {
            return Vec::new();
        };
        model
            .layers(page, generation, width, Rect::new(sx, sy, width, height))
            .1
    }

    /// Bytes of bitmap data in the texture cache.
    pub fn cache_used(&self) -> usize {
        self.imp().model.borrow().cache.used()
    }

    /// The texture cache's cap in bytes.
    pub fn cache_cap(&self) -> usize {
        self.imp().model.borrow().cache.cap()
    }

    /// Drops every texture except those drawn right now, and withdraws the render requests
    /// still outstanding (used when the PDF pane is hidden).
    pub fn trim_cache(&self) {
        let imp = self.imp();
        let (_, height) = imp.view_size();
        let (_, sy) = imp.scroll();
        {
            let mut model = imp.model.borrow_mut();
            let visible = model.visible(sy, height);
            let (generation, target, dark) = (
                model.doc.as_ref().map(|d| d.generation),
                model.target_q,
                model.dark,
            );
            model.cache.retain(|k| {
                Some(k.generation) == generation
                    && k.scale_q == target
                    && k.dark == dark
                    && visible.contains(&k.page)
            });
        }
        imp.cancel_requests();
    }

    // ----- M3 (WP3.4) -----

    /// Sets the search matches of the 0-based `page` (bp, top-left origin; one rectangle per
    /// line of a match), drawn while the page is on screen. An empty list removes them.
    pub fn set_search_matches(&self, page: u32, matches: Vec<SearchMatch>) {
        {
            let mut model = self.imp().model.borrow_mut();
            if matches.is_empty() {
                model.search.remove(&page);
            } else {
                model.search.insert(page, matches);
            }
        }
        self.queue_draw();
    }

    /// Removes every search match and the current one.
    pub fn clear_search_matches(&self) {
        {
            let mut model = self.imp().model.borrow_mut();
            model.search.clear();
            model.current_match = None;
        }
        self.queue_draw();
    }

    /// The search matches of `page`.
    pub fn search_matches(&self, page: u32) -> Vec<SearchMatch> {
        let model = self.imp().model.borrow();
        model.search.get(&page).cloned().unwrap_or_default()
    }

    /// The pages that have search matches, in order.
    pub fn pages_with_matches(&self) -> Vec<u32> {
        let mut pages: Vec<u32> = self.imp().model.borrow().search.keys().copied().collect();
        pages.sort_unstable();
        pages
    }

    /// Emphasises the match `index` of `page` (or none).
    pub fn set_current_match(&self, current: Option<(u32, usize)>) {
        self.imp().model.borrow_mut().current_match = current;
        self.queue_draw();
    }

    /// The emphasised search match.
    pub fn current_match(&self) -> Option<(u32, usize)> {
        self.imp().model.borrow().current_match
    }

    /// Scrolls so that `rect` (bp) of the 0-based `page` is on screen, at a third of the view
    /// height, unless it is entirely visible already. Returns whether it scrolled.
    pub fn reveal(&self, page: u32, rect: PageRect) -> bool {
        let imp = self.imp();
        let (width, height) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let target = {
            let model = imp.model.borrow();
            let Some(doc) = &model.doc else {
                return false;
            };
            if page >= doc.layout.page_count() {
                return false;
            }
            let zoom = model.zoom;
            let (x, y) = doc.layout.to_view(page, rect.0, rect.1, zoom, width);
            let target = Rect::new(x, y, rect.2 * zoom, rect.3 * zoom);
            if Rect::new(sx, sy, width, height).contains(&target) {
                return false;
            }
            let new_x = if target.x >= sx && target.x + target.width <= sx + width {
                sx
            } else {
                target.x + target.width / 2.0 - width / 2.0
            };
            (new_x, target.y - height / 3.0)
        };
        self.set_scroll_position(target.0, target.1);
        true
    }

    /// Scrolls so that the point `y_bp` of the 0-based `page` is `margin` logical pixels below
    /// the top of the view (a link's destination); the horizontal position is kept.
    pub fn scroll_to_point(&self, page: u32, y_bp: f64, margin: f64) {
        let imp = self.imp();
        let (width, _) = imp.view_size();
        let y = {
            let model = imp.model.borrow();
            let Some(doc) = &model.doc else {
                return;
            };
            if page >= doc.layout.page_count() {
                return;
            }
            doc.layout.to_view(page, 0.0, y_bp, model.zoom, width).1 - margin
        };
        let x = self.scroll_position().0;
        self.set_scroll_position(x, y.max(0.0));
    }

    /// The scroll anchor: page and fraction of it at the top of the view, and the horizontal
    /// offset (like [`anchor_of`], without copying the layout).
    pub fn anchor(&self) -> Option<Anchor> {
        let imp = self.imp();
        let (sx, sy) = imp.scroll();
        let model = imp.model.borrow();
        Some(model.doc.as_ref()?.layout.anchor_at(sy, sx, model.zoom))
    }

    /// Scrolls to `anchor` (clamped to the document).
    pub fn set_anchor(&self, anchor: Anchor) {
        let y = {
            let model = self.imp().model.borrow();
            let Some(doc) = &model.doc else {
                return;
            };
            doc.layout.y_for_anchor(anchor, model.zoom)
        };
        self.set_scroll_position(anchor.x, y);
    }

    /// Scrolls to `anchor` and keeps it at the top of the view through every relayout of the
    /// next `duration` (a view moved to another window is allocated several sizes before its
    /// window settles, and the sizes in between can clamp the scroll position). Scrolling with
    /// [`PdfView::set_scroll_position`] (or anything built on it) or zooming lets it go.
    pub fn hold_anchor(&self, anchor: Anchor, duration: Duration) {
        self.set_anchor(anchor);
        self.imp().model.borrow_mut().held = Some((anchor, Instant::now() + duration));
    }

    /// The document point (0-based page, bp) at `(x, y)` in widget coordinates, if it is on a
    /// page (like [`page_point_at`], without copying the layout: for pointer motion).
    pub fn page_point(&self, x: f64, y: f64) -> Option<(u32, f64, f64)> {
        let imp = self.imp();
        let (width, _) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let model = imp.model.borrow();
        model
            .doc
            .as_ref()?
            .layout
            .to_page(x + sx, y + sy, model.zoom, width)
    }

    /// Widget coordinates of the point `(x_bp, y_bp)` of the 0-based `page` (like
    /// [`view_point_of`], without copying the layout).
    pub fn widget_point(&self, page: u32, x_bp: f64, y_bp: f64) -> Option<(f64, f64)> {
        let imp = self.imp();
        let (width, _) = imp.view_size();
        let (sx, sy) = imp.scroll();
        let model = imp.model.borrow();
        let (x, y) = model
            .doc
            .as_ref()?
            .layout
            .to_view(page, x_bp, y_bp, model.zoom, width);
        Some((x - sx, y - sy))
    }

    /// The size in bp of the 0-based `page`.
    pub fn page_size(&self, page: u32) -> Option<(f64, f64)> {
        let model = self.imp().model.borrow();
        model.doc.as_ref()?.layout.page_size(page)
    }

    /// Stops keeping the anchor of [`PdfView::hold_anchor`].
    pub fn release_anchor(&self) {
        self.imp().model.borrow_mut().held = None;
    }

    /// The texture of the whole 0-based `page` of the shown generation at the current render
    /// scale, if it is cached (the link preview borrows it), and whether it is the dark page:
    /// the one of the current mode if there is one, else the other.
    pub fn page_texture(&self, page: u32) -> Option<(gdk::Texture, bool)> {
        let mut model = self.imp().model.borrow_mut();
        let generation = model.doc.as_ref()?.generation;
        let current = model.dark;
        [current, !current].into_iter().find_map(|dark| {
            let key = PageKey {
                generation,
                page,
                scale_q: model.target_q,
                tile: None,
                dark,
            };
            model.cache.get(&key).map(|texture| (texture, dark))
        })
    }

    /// Changes the texture cache's cap (the PDF strip's thumbnails take a share of the budget
    /// while it is shown); evicts the least recently used textures beyond it.
    pub fn set_cache_cap(&self, bytes: usize) {
        let imp = self.imp();
        imp.model.borrow_mut().cache.set_cap(bytes.max(1));
        imp.schedule_update();
        self.queue_draw();
    }

    /// The longest time one `snapshot` took so far.
    pub fn max_snapshot_time(&self) -> Duration {
        self.imp().model.borrow().max_snapshot
    }

    /// Resets [`PdfView::max_snapshot_time`].
    pub fn reset_snapshot_time(&self) {
        self.imp().model.borrow_mut().max_snapshot = Duration::ZERO;
    }

    fn install_controllers(&self) {
        let motion = gtk::EventControllerMotion::new();
        let weak = self.downgrade();
        motion.connect_enter(move |_, x, y| {
            if let Some(view) = weak.upgrade() {
                view.imp().model.borrow_mut().pointer = Some((x, y));
            }
        });
        let weak = self.downgrade();
        motion.connect_motion(move |_, x, y| {
            if let Some(view) = weak.upgrade() {
                view.imp().model.borrow_mut().pointer = Some((x, y));
            }
        });
        let weak = self.downgrade();
        motion.connect_leave(move |_| {
            if let Some(view) = weak.upgrade() {
                view.imp().model.borrow_mut().pointer = None;
            }
        });
        self.add_controller(motion);

        // Ctrl+wheel zooms at the pointer; plain scrolling goes on to the scrolled window.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = self.downgrade();
        scroll.connect_scroll(move |controller, _dx, dy| {
            let Some(view) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !controller
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }
            let factor = match controller.unit() {
                gdk::ScrollUnit::Wheel => WHEEL_ZOOM_STEP.powf(-dy),
                _ => (-dy * 0.01).exp(),
            };
            let pointer = view.imp().model.borrow().pointer;
            view.zoom_by(factor, pointer);
            glib::Propagation::Stop
        });
        self.add_controller(scroll);

        let pinch = gtk::GestureZoom::new();
        let weak = self.downgrade();
        pinch.connect_begin(move |_, _| {
            if let Some(view) = weak.upgrade() {
                let zoom = view.zoom();
                view.imp().model.borrow_mut().gesture_start_zoom = zoom;
            }
        });
        let weak = self.downgrade();
        pinch.connect_scale_changed(move |gesture, scale| {
            if let Some(view) = weak.upgrade() {
                let start = view.imp().model.borrow().gesture_start_zoom;
                view.set_zoom(start * scale, gesture.bounding_box_center());
            }
        });
        self.add_controller(pinch);

        let click = gtk::GestureClick::new();
        let weak = self.downgrade();
        click.connect_pressed(move |gesture, presses, x, y| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.grab_focus();
            let ctrl = gesture
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            if ctrl && presses == 1 && view.inverse_search_at(x, y) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        self.add_controller(click);

        let keys = gtk::ShortcutController::new();
        let add = |triggers: &str, run: fn(&PdfView)| {
            let action = gtk::CallbackAction::new(move |widget, _| {
                if let Some(view) = widget.downcast_ref::<PdfView>() {
                    run(view);
                }
                glib::Propagation::Stop
            });
            let trigger = gtk::ShortcutTrigger::parse_string(triggers);
            keys.add_shortcut(gtk::Shortcut::new(trigger, Some(action)));
        };
        add("<Control>plus|<Control>equal|<Control>KP_Add", |v| {
            v.zoom_in()
        });
        add("<Control>minus|<Control>KP_Subtract", |v| v.zoom_out());
        add("<Control>0|<Control>KP_0", |v| v.set_fit_width());
        add("<Control>1|<Control>KP_1", |v| v.reset_zoom());
        add("Down|j", |v| v.scroll_by(SCROLL_STEP));
        add("Up|k", |v| v.scroll_by(-SCROLL_STEP));
        // Sideways; taken also when there is nothing to scroll (at fit width), so that they never
        // go on to the window, where a key can move the focus out of the view.
        add("Right|l", |v| v.pan_by(SCROLL_STEP));
        add("Left|h", |v| v.pan_by(-SCROLL_STEP));
        add("Page_Down|space", |v| v.scroll_pages(1.0));
        add("Page_Up|<Shift>space", |v| v.scroll_pages(-1.0));
        add("Home", |v| {
            v.set_scroll_position(v.scroll_position().0, 0.0)
        });
        add("End", |v| {
            v.set_scroll_position(v.scroll_position().0, f64::MAX)
        });
        self.add_controller(keys);
    }

    fn scroll_by(&self, dy: f64) {
        let (x, y) = self.scroll_position();
        self.set_scroll_position(x, y + dy);
    }

    fn pan_by(&self, dx: f64) {
        let (x, y) = self.scroll_position();
        self.set_scroll_position(x + dx, y);
    }

    fn scroll_pages(&self, pages: f64) {
        let height = f64::from(self.height());
        self.scroll_by(pages * height * 0.9);
    }
}

impl Default for PdfView {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_BYTES)
    }
}

/// The document point (0-based page, bp) shown at widget coordinates `(x, y)` of `view`.
pub fn page_point_at(view: &PdfView, x: f64, y: f64) -> Option<(u32, f64, f64)> {
    let layout = view.layout()?;
    let (sx, sy) = view.scroll_position();
    layout.to_page(x + sx, y + sy, view.zoom(), f64::from(view.width()))
}

/// Widget coordinates of the point `(x_bp, y_bp)` of the 0-based `page` of `view`.
pub fn view_point_of(view: &PdfView, page: u32, x_bp: f64, y_bp: f64) -> Option<(f64, f64)> {
    let layout = view.layout()?;
    let (sx, sy) = view.scroll_position();
    let (x, y) = layout.to_view(page, x_bp, y_bp, view.zoom(), f64::from(view.width()));
    Some((x - sx, y - sy))
}

/// The scroll anchor of `view` (page and fraction at the top of the view), if it shows a
/// document.
pub fn anchor_of(view: &PdfView) -> Option<Anchor> {
    let layout = view.layout()?;
    let (sx, sy) = view.scroll_position();
    Some(layout.anchor_at(sy, sx, view.zoom()))
}
