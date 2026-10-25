//! PDF render + SyncTeX worker thread (owns the !Send poppler::Document), page layout math, byte-bounded LRU.
//!
//! The GTK main thread never calls poppler. It sends [`PdfCmd`]s through a [`PdfHandle`] and receives
//! [`PdfEvent`]s on an `async_channel`. Page indices are 0-based everywhere in this crate except in
//! [`SyncBox::page`], which keeps SyncTeX's 1-based numbering. PDF coordinates are big points (bp) with
//! the origin at the page's top-left corner and y growing downward.

pub mod dark;
mod layout;
mod links;
mod lru;
mod outline;
mod render;
mod rotation;
mod search;
mod worker;

pub use layout::{Anchor, PageLayout};
pub use lemma_synctex::{SyncBox, SyncTarget};
pub use lru::ByteLru;
pub use render::{
    TILE_SIZE, TILE_THRESHOLD_BYTES, page_pixels, scale_of, scale_q, tile_grid, tile_rect,
};
pub use worker::{PdfHandle, spawn};

use std::path::PathBuf;

/// Identifies one bitmap: a page (or a tile of it) of one document generation at one scale, light
/// or dark.
///
/// zoom * device_scale quantized to 1/8 steps: scale_q = round(zoom * scale * 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageKey {
    /// The generation of the [`PdfCmd::Open`] the page belongs to.
    pub generation: u64,
    /// 0-based page index.
    pub page: u32,
    /// Render scale in device pixels per bp, times 8 (see [`scale_q`]).
    pub scale_q: u16,
    /// `(column, row)` of a [`TILE_SIZE`]² tile, used when a page bitmap would exceed
    /// [`TILE_THRESHOLD_BYTES`] (see [`tile_grid`] and [`tile_rect`]); `None` for the whole page.
    pub tile: Option<(u16, u16)>,
    /// True for the page in dark mode: the worker colours the bitmap with [`dark::darken_bgra`],
    /// so the view draws it without a colour matrix.
    pub dark: bool,
}

/// A request to render one page or tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderReq {
    pub key: PageKey,
}

/// A hyperlink on a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// `(x, y, width, height)` of the clickable area in bp, top-left origin.
    pub area: (f64, f64, f64, f64), /*bp, top-left origin*/
    pub dest: LinkDest,
}

/// Where a link points.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkDest {
    /// A page of the same document: 0-based `page`, and the target's y in bp from the top of that page
    /// when the destination names one (`None` = the top of the page).
    Page { page: u32, y_bp: Option<f64> },
    /// An external URI.
    Uri(String),
    /// Any other action (named actions, other files, launch, JavaScript, …).
    Other,
}

/// One entry of the document outline (the PDF's bookmarks, e.g. hyperref's sections).
#[derive(Clone, Debug, PartialEq)]
pub struct OutlineItem {
    /// The title, with runs of white space (line breaks included) collapsed to one space.
    pub title: String,
    /// Where the entry points.
    pub dest: LinkDest,
    /// True when the PDF asks for the entry's children to be shown (`/Count` > 0).
    pub open: bool,
    pub children: Vec<OutlineItem>,
}

/// How [`PdfCmd::Search`] matches. The default is case-insensitive, ignores diacritics when the
/// query has none (poppler's rule), and matches inside words and across line breaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_words: bool,
}

/// One search match: one rectangle `(x, y, width, height)` in bp (top-left origin) for each
/// line the match covers, in reading order.
pub type SearchMatch = Vec<(f64, f64, f64, f64)>;

/// A request for one page rendered at a width in device pixels (thumbnails and link previews),
/// outside the view's cache of [`PageKey`] bitmaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThumbReq {
    /// The generation of the [`PdfCmd::Open`] the page belongs to.
    pub generation: u64,
    /// 0-based page index.
    pub page: u32,
    /// The bitmap's width in device pixels (at least 1; the height follows the page's aspect).
    pub width: u16,
    /// True for the page in dark mode, coloured like a dark [`PageKey`] bitmap, so the strip
    /// draws it without a colour matrix.
    pub dark: bool,
}

/// One page rendered for a [`ThumbReq`].
pub struct Thumbnail {
    pub req: ThumbReq,
    /// Bitmap width in device pixels (`req.width`).
    pub width: i32,
    /// Bitmap height in device pixels.
    pub height: i32,
    /// Bytes per row; `data.len() == stride * height`.
    pub stride: i32,
    /// BGRA premultiplied, as in [`Rendered::data`].
    pub data: cairo::ImageSurfaceDataOwned,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thumbnail")
            .field("req", &self.req)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// One rendered bitmap.
pub struct Rendered {
    pub key: PageKey,
    /// Bitmap width in device pixels.
    pub width: i32,
    /// Bitmap height in device pixels.
    pub height: i32,
    /// Bytes per row; `data.len() == stride * height`.
    pub stride: i32,
    /// BGRA premultiplied (cairo ARgb32 == gdk::MemoryFormat::B8g8r8a8Premultiplied). Send; wrap with glib::Bytes::from_owned.
    pub data: cairo::ImageSurfaceDataOwned,
}

impl std::fmt::Debug for Rendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendered")
            .field("key", &self.key)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Events from the PDF thread to the main thread.
#[derive(Debug)]
pub enum PdfEvent {
    /// The document of `generation` is now current; page sizes in bp.
    Opened {
        generation: u64,
        page_sizes: Vec<(f64, f64)>, /*bp*/
    },
    /// The file could not be read or parsed. The previously opened document (if any) stays current, so
    /// requests for its generation are still served.
    OpenFailed {
        generation: u64,
        error: String,
    },
    Rendered(Rendered),
    /// Answer to [`PdfCmd::Forward`]: the boxes of the source line in the current document (empty when
    /// there is no document, no SyncTeX data or no match). Note that [`SyncBox::page`] is 1-based.
    /// Boxes are in the space of the page as displayed (the `page_sizes` of [`PdfEvent::Opened`]), also on
    /// pages turned by `/Rotate`.
    /// `generation` is that of the current document, or 0 when none is open.
    Forward {
        generation: u64,
        boxes: Vec<SyncBox>,
    },
    /// Answer to [`PdfCmd::Inverse`].
    Inverse {
        target: Option<SyncTarget>,
    },
    /// Answer to [`PdfCmd::Links`]; empty when the page does not exist. `generation` is that of the
    /// current document, or 0 when none is open.
    Links {
        generation: u64,
        page: u32,
        links: Vec<Link>,
    },
    /// The matches of [`PdfCmd::Search`] `id` on one 0-based `page` of the document of
    /// `generation`, in reading order. Only pages with matches are reported, in the order they
    /// were searched (from the search's `start` page on, wrapping around).
    SearchPage {
        id: u64,
        generation: u64,
        page: u32,
        matches: Vec<SearchMatch>,
    },
    /// Every page of the document of `generation` has been searched for [`PdfCmd::Search`]
    /// `id` (`generation` 0: there was no document). Not sent for a search that was replaced or
    /// cancelled, or whose document was replaced meanwhile.
    SearchDone {
        id: u64,
        generation: u64,
    },
    /// Answer to one request of [`PdfCmd::WantThumbnails`].
    Thumbnail(Thumbnail),
    /// Answer to [`PdfCmd::Outline`]: the outline of the current document (empty when it has
    /// none, or when no document is open: `generation` 0).
    Outline {
        generation: u64,
        items: Vec<OutlineItem>,
    },
    /// Answer to [`PdfCmd::PageLabels`]: one label per page (the PDF's page labels, e.g. `iv`
    /// for front matter; the 1-based page number where the PDF has none).
    PageLabels {
        generation: u64,
        labels: Vec<String>,
    },
}

/// Commands to the PDF thread, processed in order.
#[derive(Debug)]
pub enum PdfCmd {
    /// Worker reads the file itself (std::fs::read -> Document::from_bytes), so latexmk may overwrite it later.
    ///
    /// Answered with [`PdfEvent::Opened`] or [`PdfEvent::OpenFailed`]. Once a newer generation is open,
    /// queued requests for older generations are dropped. The SyncTeX file next to the PDF
    /// (`<stem>.synctex.gz` or `<stem>.synctex`) is parsed on the first Forward or Inverse and reparsed
    /// whenever it changes.
    Open { generation: u64, pdf: PathBuf },
    /// Replaces the whole pending render queue (latest wins). Requests for older generations are dropped.
    ///
    /// Requests are rendered in the given order, one [`PdfEvent::Rendered`] each; a page being rendered
    /// when the Want arrives still completes. Only requests for the current document's generation are
    /// rendered: older generations, the generation of a failed Open and non-existent pages are skipped.
    /// A whole-page request for a page that must be tiled (see [`tile_grid`]) is answered with its
    /// tiles in row-major order instead, if there are at most 256 of them.
    Want(Vec<RenderReq>),
    /// Forward search for a 1-based source `line` of `file`; answered with [`PdfEvent::Forward`].
    Forward { file: PathBuf, line: u32 },
    /// Inverse search at a point of the 0-based `page` (bp, top-left origin, on the page as displayed);
    /// answered with [`PdfEvent::Inverse`].
    Inverse { page: u32, x_bp: f64, y_bp: f64 },
    /// The links of the 0-based `page`; answered with [`PdfEvent::Links`].
    Links { page: u32 },
    /// Searches every page of the current document for `query`, from the 0-based page `start`
    /// on, wrapping around; answered with a [`PdfEvent::SearchPage`] per page with matches and
    /// a final [`PdfEvent::SearchDone`]. Pages are searched one at a time between renders, so
    /// the view stays responsive. A new `Search` replaces the one in progress (latest wins).
    Search {
        id: u64,
        query: String,
        options: SearchOptions,
        start: u32,
    },
    /// Stops the search in progress (no [`PdfEvent::SearchDone`] follows).
    CancelSearch,
    /// Replaces the pending thumbnail requests (latest wins). Each is answered with a
    /// [`PdfEvent::Thumbnail`]; they share the thread with [`PdfCmd::Want`] and searches in
    /// turn, and requests for other generations or non-existent pages are skipped.
    WantThumbnails(Vec<ThumbReq>),
    /// The outline (bookmarks) of the current document; answered with [`PdfEvent::Outline`].
    Outline,
    /// The page labels of the current document; answered with [`PdfEvent::PageLabels`].
    PageLabels,
    /// Stops the thread; pending requests are dropped. See also [`PdfHandle::shutdown_and_join`].
    Shutdown,
}
