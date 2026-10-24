//! The PDF pane's strip of thumbnails and bookmarks.
//!
//! The shell gives the PDF pane a collapsible left strip: `state.pdf_sidebar.split` (an
//! `adw::OverlaySplitView` whose content is the PDF view's scrolled window) and its slot
//! `state.pdf_sidebar.slot`, which [`install`] fills. `win.toggle-pdf-sidebar` (the toolbar's
//! sidebar button) shows and hides it; it starts as `[pdf] thumbnails` says, and follows that
//! key when config.toml changes.
//!
//! An inline switcher at the top picks one of two pages:
//!
//! - **Thumbnails:** a `gtk::ListView` of every page with its label (the PDF's page labels, so
//!   front matter reads `iv`). A thumbnail is rendered only while its row is bound (on screen or
//!   about to be), at its exact width (rounded up to [`WIDTH_STEP`] device pixels), by the PDF
//!   thread between page renders. The thumbnails have their own byte budget,
//!   [`thumbnail_budget`] (an eighth of `[pdf] cache_mib`), **inside** the shared cache: while
//!   the strip is shown the view's page cache gets the rest; when it is hidden the thumbnails are
//!   dropped and the view gets the whole cache back. The bound rows' thumbnails are never evicted
//!   for each other: while they need more than the budget (a HiDPI screen, a small
//!   `cache_mib`), the cache holds just them, as the view's cache does with the pages on screen,
//!   and a row that shows a current thumbnail is never rendered again. A rebuilt PDF keeps the
//!   old thumbnails on screen until the new ones arrive.
//! - **Bookmarks:** the PDF's outline (hyperref's bookmarks) as a tree (`gtk::TreeListModel`),
//!   with each entry's page label. It is read when the strip is first shown and after each
//!   rebuild; the entries that were expanded stay expanded (otherwise the PDF's own `/Count`
//!   decides).
//!
//! The row of the page shown (or of the section the top of the view is in) is selected and kept
//! in view while the PDF scrolls. Selecting a row (click or arrow keys) goes there; a bookmark
//! jump is recorded for Back like a link jump. Nothing is rendered while the strip is hidden.
//!
//! Hooks: [`install`] once per window (after `pdf_controller::install`) fills
//! `state.pdf_sidebar.slot`.

use super::links::JUMP_MARGIN;
use super::page_picture::PagePicture;
use super::pdf_controller::ThumbClient;
use super::pdf_view::PdfView;
use crate::build_ui::install_css_once;
use crate::window::WindowState;
use adw::prelude::*;
use gtk::gdk;
use lemma_pdf::{ByteLru, LinkDest, OutlineItem, PdfCmd, ThumbReq};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

/// Thumbnails are rendered at widths rounded up to this many device pixels, so resizing the
/// strip re-renders them only every few pixels.
pub const WIDTH_STEP: i32 = 32;

/// The strip's width per page: minimum (logical pixels) and fraction of the PDF pane (titles
/// need more room than thumbnails).
const STRIP_WIDTHS: [(Page, f64, f64); 2] = [
    (Page::Thumbnails, 120.0, 0.25),
    (Page::Bookmarks, 180.0, 0.34),
];

/// The thumbnails' share of a texture budget of `total` bytes (`[pdf] cache_mib`).
pub fn thumbnail_budget(total: usize) -> usize {
    total / 8
}

const CSS: &str = "
.lemma-pdf-strip { background-color: var(--sidebar-bg-color); }
.lemma-pdf-strip .lemma-pdf-strip-switcher { margin: 6px 8px; }
.lemma-pdf-strip listview { background: none; }
.lemma-pdf-thumbs > row { padding: 6px 12px 4px 12px; border-radius: 10px; margin: 0 4px; }
.lemma-pdf-thumbs > row:selected { background: none; }
.lemma-pdf-thumbs > row:hover { background-color: alpha(currentColor, 0.05); }
.lemma-pdf-thumbs pagepicture { box-shadow: 0 1px 3px alpha(black, 0.28); }
.lemma-pdf-thumbs > row:selected pagepicture {
  box-shadow: 0 0 0 2px var(--accent-color), 0 1px 3px alpha(black, 0.28);
}
.lemma-pdf-thumbs .lemma-thumb-label { margin-top: 5px; padding: 0 7px; border-radius: 999px; }
.lemma-pdf-thumbs > row:selected .lemma-thumb-label {
  background-color: var(--accent-bg-color); color: var(--accent-fg-color);
}
.lemma-pdf-bookmarks.navigation-sidebar > row { margin: 0 4px; padding: 0 6px; min-height: 30px; }
.lemma-pdf-bookmarks .lemma-bookmark-page { margin-left: 6px; }
";

/// The strip's pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Thumbnails,
    Bookmarks,
}

impl Page {
    fn name(self) -> &'static str {
        match self {
            Page::Thumbnails => "thumbnails",
            Page::Bookmarks => "bookmarks",
        }
    }
}

/// One cached thumbnail of a page.
struct Thumb {
    generation: u64,
    /// The width it was rendered at, in device pixels.
    width: u16,
    texture: gdk::Texture,
    /// The size of its bitmap.
    bytes: usize,
}

/// A bound thumbnail row.
struct Row {
    picture: PagePicture,
    /// The generation and width of the thumbnail the picture shows.
    shows: Option<(u64, u16)>,
}

impl Row {
    /// Shows `thumb` (or the blank page).
    fn show(&mut self, thumb: Option<&Thumb>) {
        self.picture.set_texture(thumb.map(|t| &t.texture));
        self.shows = thumb.map(|t| (t.generation, t.width));
    }
}

/// A thumbnail the user chose: the page, and the view's scroll position once it got there
/// (`None` while it is going there).
type Chosen = (u32, Option<(f64, f64)>);

/// A bookmark row as tests see it: depth, title and 0-based page.
pub type BookmarkRow = (u32, String, Option<u32>);

/// An outline entry in the bookmark tree.
struct Node {
    title: String,
    dest: LinkDest,
    open: bool,
    /// The titles from the root to this entry (identifies it across rebuilds).
    path: Vec<String>,
    children: Vec<Rc<Node>>,
}

/// The strip of one window's PDF pane (registered with the controller).
pub struct Strip {
    state: Weak<WindowState>,
    view: PdfView,
    split: adw::OverlaySplitView,
    stack: adw::ViewStack,
    /// The configured texture budget (the view's cache plus the thumbnails).
    total: usize,
    // thumbnails
    thumbs: gtk::ListView,
    thumbs_model: gtk::StringList,
    thumbs_selection: gtk::SingleSelection,
    /// The bound rows, by page.
    bound: RefCell<HashMap<u32, Row>>,
    /// The thumbnails, within [`thumbnail_budget`] unless the bound rows need more (see
    /// [`Strip::cache_thumbnail`]).
    cache: RefCell<ByteLru<u32, Thumb>>,
    /// The thumbnails that arrived from the PDF thread (tests check that a strip that has all
    /// it needs asks for no more).
    arrived: Cell<usize>,
    generation: Cell<u64>,
    sizes: RefCell<Vec<(f64, f64)>>,
    labels: RefCell<Vec<String>>,
    labels_generation: Cell<u64>,
    wants_scheduled: Cell<bool>,
    /// Set while the strip changes a selection itself (following the view).
    syncing: Cell<bool>,
    /// True while the strip is shown (and the view's cache gives the thumbnails their share).
    active: Cell<bool>,
    /// The thumbnail the user chose and the view's scroll position after going there: it stays
    /// selected until the view scrolls (the page the toolbar reports can be the next one when
    /// the view is near the end).
    chosen: Cell<Option<Chosen>>,
    // bookmarks
    bookmarks: gtk::ListView,
    bookmarks_stack: gtk::Stack,
    outline_root: gio::ListStore,
    tree: gtk::TreeListModel,
    bookmarks_selection: gtk::SingleSelection,
    outline_generation: Cell<u64>,
    outline_requested: Cell<u64>,
    /// The paths of the expanded entries, kept across rebuilds.
    expanded: RefCell<HashSet<Vec<String>>>,
    /// The page labels of the bound bookmark rows, with their pages.
    bookmark_pages: RefCell<Vec<(glib::WeakRef<gtk::Label>, u32)>>,
    /// The outline has been shown once (later loads keep the user's expansions).
    outline_seen: Cell<bool>,
    /// Some entry has children (rows then leave room for the expander arrow).
    nested: Cell<bool>,
}

impl Strip {
    fn send(&self, cmd: PdfCmd) {
        if let Some(state) = self.state.upgrade()
            && let Some(pdf) = state.pdf.get()
        {
            pdf.send(cmd);
        }
    }

    fn shown(&self) -> bool {
        self.split.shows_sidebar() && self.view.page_count() > 0
    }

    fn page(&self) -> Page {
        match self.stack.visible_child_name().as_deref() {
            Some("bookmarks") => Page::Bookmarks,
            _ => Page::Thumbnails,
        }
    }

    /// The label of the 0-based `page` (its page label, else its number).
    pub(super) fn page_label(&self, page: u32) -> String {
        self.labels
            .borrow()
            .get(page as usize)
            .cloned()
            .unwrap_or_else(|| (page + 1).to_string())
    }

    // ----- showing and hiding -----

    /// The strip was shown or hidden: the thumbnails take their share of the cache, or give it
    /// back with everything they hold.
    fn shown_changed(&self) {
        let shown = self.shown();
        if shown == self.active.get() {
            return;
        }
        self.active.set(shown);
        if shown {
            self.view
                .set_cache_cap(self.total - thumbnail_budget(self.total));
            self.refresh_page();
            self.follow_view();
        } else {
            self.view.set_cache_cap(self.total);
            self.cache.borrow_mut().retain(|_| false);
            for row in self.bound.borrow_mut().values_mut() {
                row.show(None);
            }
            self.set_wants(Vec::new());
        }
    }

    /// Gives the strip the width of its visible page.
    fn fit_width(&self) {
        let page = self.page();
        if let Some(&(_, min, fraction)) = STRIP_WIDTHS.iter().find(|(p, _, _)| *p == page) {
            self.split.set_min_sidebar_width(min);
            self.split.set_sidebar_width_fraction(fraction);
        }
    }

    /// Loads what the visible page needs (thumbnails, or the outline).
    fn refresh_page(&self) {
        if !self.active.get() {
            return;
        }
        let generation = self.generation.get();
        if self.labels_generation.get() != generation {
            self.labels_generation.set(generation);
            self.send(PdfCmd::PageLabels);
        }
        match self.page() {
            Page::Thumbnails => {
                self.restore_textures();
                self.schedule_wants();
            }
            Page::Bookmarks => {
                self.set_wants(Vec::new());
                if self.outline_requested.get() != generation {
                    self.outline_requested.set(generation);
                    if self.outline_generation.get() != generation && !self.outline_seen.get() {
                        self.bookmarks_stack.set_visible_child_name("loading");
                    }
                    self.send(PdfCmd::Outline);
                }
            }
        }
        self.follow_view();
    }

    // ----- the document -----

    /// A new generation (or, unless `rebuilt`, another PDF) is shown.
    pub(super) fn document_changed(&self, generation: u64, rebuilt: bool) {
        self.generation.set(generation);
        let sizes: Vec<(f64, f64)> = self.view.layout().map_or(Vec::new(), |layout| {
            (0..layout.page_count())
                .filter_map(|p| layout.page_size(p))
                .collect()
        });
        if !rebuilt {
            self.cache.borrow_mut().retain(|_| false);
            self.expanded.borrow_mut().clear();
            self.outline_seen.set(false);
            self.labels.borrow_mut().clear();
        }
        let n = sizes.len();
        self.cache.borrow_mut().retain(|page| (*page as usize) < n);
        self.sizes.replace(sizes);
        // Until the page labels come: the old ones where they still fit, else numbers.
        let labels: Vec<String> = (0..n as u32).map(|p| self.page_label(p)).collect();
        self.set_labels(labels);
        for (page, row) in self.bound.borrow().iter() {
            if let Some(size) = self.sizes.borrow().get(*page as usize) {
                row.picture.set_page_size(*size);
            }
        }
        self.labels_generation.set(0);
        self.outline_requested.set(0);
        if self.active.get() {
            self.refresh_page();
        }
    }

    /// Replaces the page labels shown (and the thumbnail rows, when the count changed).
    fn set_labels(&self, labels: Vec<String>) {
        if *self.labels.borrow() == labels && self.thumbs_model.n_items() as usize == labels.len() {
            return;
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.labels.replace(labels.clone());
        self.syncing.set(true);
        self.thumbs_model
            .splice(0, self.thumbs_model.n_items(), &refs);
        self.syncing.set(false);
        self.follow_view();
    }

    /// The page labels of `generation` (`PdfEvent::PageLabels`).
    pub(super) fn page_labels(&self, generation: u64, labels: Vec<String>) {
        if generation != self.generation.get() || labels.len() != self.sizes.borrow().len() {
            return;
        }
        self.set_labels(labels);
        // The bookmark rows show page labels too.
        self.refresh_bookmark_rows();
    }

    // ----- thumbnails -----

    fn bind_thumbnail(&self, page: u32, picture: &PagePicture, label: &gtk::Label) {
        if let Some(size) = self.sizes.borrow().get(page as usize) {
            picture.set_page_size(*size);
        }
        picture.set_dark(self.view.is_dark());
        label.set_text(&self.page_label(page));
        let mut row = Row {
            picture: picture.clone(),
            shows: None,
        };
        row.show(self.cache.borrow_mut().get(&page));
        picture.update_property(&[gtk::accessible::Property::Label(&format!(
            "Page {}",
            self.page_label(page)
        ))]);
        self.bound.borrow_mut().insert(page, row);
        self.schedule_wants();
    }

    fn unbind_thumbnail(&self, page: u32, picture: &PagePicture) {
        let mut bound = self.bound.borrow_mut();
        if bound.get(&page).is_some_and(|row| row.picture == *picture) {
            bound.remove(&page);
        }
        picture.set_texture(None);
        drop(bound);
        self.schedule_wants();
    }

    /// Gives the bound rows the textures the cache has (after the strip was hidden).
    fn restore_textures(&self) {
        let mut cache = self.cache.borrow_mut();
        for (page, row) in self.bound.borrow_mut().iter_mut() {
            if row.shows.is_none()
                && let Some(t) = cache.get(page)
            {
                row.show(Some(t));
            }
        }
    }

    /// Recomputes the thumbnail requests at the next idle.
    fn schedule_wants(&self) {
        if self.wants_scheduled.replace(true) {
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let weak = Rc::downgrade(&state);
        glib::idle_add_local_once(move || {
            if let Some(strip) = weak.upgrade().and_then(|s| s.pdf.get()?.strip().cloned()) {
                strip.wants_scheduled.set(false);
                strip.update_wants();
            }
        });
    }

    /// The thumbnails the bound rows lack (none while the thumbnails are not shown), nearest
    /// the page shown first. A row whose picture shows a thumbnail of the current generation,
    /// at least as wide as the row needs, lacks nothing.
    fn update_wants(&self) {
        if !self.active.get() || self.page() != Page::Thumbnails || !self.thumbs.is_mapped() {
            self.set_wants(Vec::new());
            return;
        }
        let generation = self.generation.get();
        let scale = self
            .thumbs
            .native()
            .and_then(|n| n.surface())
            .map_or(1.0, |s| s.scale());
        let current = self.view.current_page();
        let mut wants: Vec<ThumbReq> = self
            .bound
            .borrow()
            .iter()
            .filter_map(|(&page, row)| {
                let width = render_width(row.picture.width(), scale)?;
                let have = row
                    .shows
                    .is_some_and(|(g, w)| g == generation && w >= width);
                (!have).then_some(ThumbReq {
                    generation,
                    page,
                    width,
                })
            })
            .collect();
        wants.sort_by_key(|r| (r.page.abs_diff(current), r.page));
        self.set_wants(wants);
    }

    fn set_wants(&self, wants: Vec<ThumbReq>) {
        if let Some(state) = self.state.upgrade()
            && let Some(pdf) = state.pdf.get()
        {
            pdf.set_thumbnail_wants(ThumbClient::Strip, wants);
        }
    }

    /// A page rendered for a thumbnail request (`PdfEvent::Thumbnail`).
    pub(super) fn thumbnail(&self, req: ThumbReq, texture: &gdk::Texture, bytes: usize) {
        self.arrived.set(self.arrived.get() + 1);
        if req.generation != self.generation.get() || !self.active.get() {
            return;
        }
        // Only for a row that wants it (the link preview's pages come here too).
        let Some(picture) = self
            .bound
            .borrow()
            .get(&req.page)
            .map(|r| r.picture.clone())
        else {
            return;
        };
        let scale = self
            .thumbs
            .native()
            .and_then(|n| n.surface())
            .map_or(1.0, |s| s.scale());
        if render_width(picture.width(), scale) != Some(req.width) {
            return;
        }
        let thumb = Thumb {
            generation: req.generation,
            width: req.width,
            texture: texture.clone(),
            bytes,
        };
        if let Some(row) = self.bound.borrow_mut().get_mut(&req.page) {
            row.show(Some(&thumb));
        }
        self.cache_thumbnail(req.page, thumb);
        self.schedule_wants();
    }

    /// Caches `thumb`, the thumbnail of the bound row of `page`. The other bound rows'
    /// thumbnails are never evicted for it: while the bound rows need more than the budget, the
    /// cache grows to hold them (like the view's `TextureCache::insert_keeping`), so the rows do
    /// not take turns pushing each other out and asking again. Other thumbnails go first, the
    /// least recently used first, and the cache shrinks back to the budget as rows unbind.
    fn cache_thumbnail(&self, page: u32, thumb: Thumb) {
        let bytes = thumb.bytes;
        let mut cache = self.cache.borrow_mut();
        let mut needed = bytes;
        for other in self.bound.borrow().keys().filter(|p| **p != page) {
            // Also makes them the most recently used, so the others are evicted first.
            if let Some(t) = cache.get(other) {
                needed += t.bytes;
            }
        }
        cache.set_cap(thumbnail_budget(self.total).max(needed));
        cache.insert(page, thumb, bytes);
    }

    // ----- bookmarks -----

    /// The outline of `generation` (`PdfEvent::Outline`).
    pub(super) fn outline(&self, generation: u64, items: Vec<OutlineItem>) {
        if generation != self.generation.get() {
            return;
        }
        // Remember what is expanded now.
        if self.outline_seen.get() {
            let mut expanded = HashSet::new();
            for i in 0..self.tree.n_items() {
                if let Some(row) = self.tree.row(i)
                    && row.is_expanded()
                    && let Some(node) = row_node(&row)
                {
                    expanded.insert(node.path.clone());
                }
            }
            self.expanded.replace(expanded);
        }
        let first = !self.outline_seen.replace(true);
        self.outline_generation.set(generation);
        let nodes = build_nodes(&items, &[]);
        self.nested
            .set(nodes.iter().any(|n| !n.children.is_empty()));
        self.syncing.set(true);
        self.outline_root.remove_all();
        for node in &nodes {
            self.outline_root
                .append(&glib::BoxedAnyObject::new(node.clone()));
        }
        // Expand top-down (expanding inserts the children after the row).
        let expanded = self.expanded.borrow().clone();
        let mut i = 0;
        while i < self.tree.n_items() {
            if let Some(row) = self.tree.row(i)
                && let Some(node) = row_node(&row)
                && !node.children.is_empty()
            {
                let open = if first {
                    node.open
                } else {
                    expanded.contains(&node.path)
                };
                if open {
                    row.set_expanded(true);
                }
            }
            i += 1;
        }
        self.syncing.set(false);
        self.bookmarks_stack
            .set_visible_child_name(if nodes.is_empty() { "empty" } else { "list" });
        self.follow_view();
    }

    /// Updates the page labels of the bound bookmark rows (re-binding them would collapse
    /// the tree).
    fn refresh_bookmark_rows(&self) {
        let mut rows = self.bookmark_pages.borrow_mut();
        rows.retain(|(label, _)| label.upgrade().is_some());
        for (label, page) in rows.iter() {
            if let Some(label) = label.upgrade() {
                label.set_text(&self.page_label(*page));
            }
        }
    }

    // ----- following the view -----

    /// Selects the row of the page shown (and of the section the view is in) and keeps it in
    /// view.
    fn follow_view(&self) {
        if !self.active.get() || self.view.page_count() == 0 {
            return;
        }
        let mut page = self.view.current_page();
        match self.chosen.get() {
            Some((chosen, None)) => page = chosen,
            Some((chosen, Some(scroll))) if scroll == self.view.scroll_position() => page = chosen,
            Some(_) => self.chosen.set(None),
            None => {}
        }
        if page < self.thumbs_model.n_items() && self.thumbs_selection.selected() != page {
            self.syncing.set(true);
            self.thumbs_selection.select_item(page, true);
            self.syncing.set(false);
            if self.page() == Page::Thumbnails && self.thumbs.is_mapped() {
                self.thumbs
                    .scroll_to(page, gtk::ListScrollFlags::NONE, None);
            }
        }
        // Above the first entry (the title page), nothing is selected.
        let row = self
            .current_bookmark()
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        if self.bookmarks_selection.selected() != row {
            self.syncing.set(true);
            self.bookmarks_selection.set_selected(row);
            self.syncing.set(false);
            if row != gtk::INVALID_LIST_POSITION
                && self.page() == Page::Bookmarks
                && self.bookmarks.is_mapped()
            {
                self.bookmarks
                    .scroll_to(row, gtk::ListScrollFlags::NONE, None);
            }
        }
    }

    /// The last visible bookmark row whose destination is in the upper third of the view or
    /// above it (at least the margin a jump leaves).
    fn current_bookmark(&self) -> Option<u32> {
        let anchor = self.view.anchor()?;
        let (_, ph) = self.view.page_size(anchor.page)?;
        let zoom = self.view.zoom().max(0.01);
        let reach = (f64::from(self.view.height()) / 3.0).max(JUMP_MARGIN + 8.0);
        let here = (anchor.page, anchor.frac * ph + reach / zoom);
        let mut best = None;
        for i in 0..self.tree.n_items() {
            let Some(node) = self.tree.row(i).and_then(|r| row_node(&r)) else {
                continue;
            };
            let LinkDest::Page { page, y_bp } = node.dest else {
                continue;
            };
            let at = (page, y_bp.unwrap_or(0.0));
            if at.0 < here.0 || (at.0 == here.0 && at.1 <= here.1) {
                best = Some(i);
            }
        }
        best
    }

    /// The user selected thumbnail `page`.
    fn thumbnail_selected(&self, page: u32) {
        if self.syncing.get() || page >= self.view.page_count() {
            return;
        }
        self.chosen.set(Some((page, None)));
        if self.view.anchor().is_none_or(|a| a.page != page) {
            self.view.goto_page(page);
        }
        self.chosen
            .set(Some((page, Some(self.view.scroll_position()))));
        self.follow_view();
    }

    /// The user selected bookmark row `row`.
    fn bookmark_selected(&self, row: u32) {
        if self.syncing.get() {
            return;
        }
        let Some(node) = self.tree.row(row).and_then(|r| row_node(&r)) else {
            return;
        };
        let LinkDest::Page { page, y_bp } = node.dest else {
            return;
        };
        if let Some(state) = self.state.upgrade()
            && let Some(links) = state.pdf.get().and_then(|p| p.links().cloned())
        {
            links.jump(page, y_bp);
        } else {
            self.view.goto_page(page);
        }
    }
}

/// The width to render a thumbnail `width` logical pixels wide at, in device pixels (rounded up
/// to [`WIDTH_STEP`]); `None` before the row has a width.
fn render_width(width: i32, scale: f64) -> Option<u16> {
    if width <= 0 {
        return None;
    }
    let px = (f64::from(width) * scale.max(1.0)).ceil() as i32;
    let stepped = (px + WIDTH_STEP - 1) / WIDTH_STEP * WIDTH_STEP;
    u16::try_from(stepped.clamp(WIDTH_STEP, 2048)).ok()
}

fn build_nodes(items: &[OutlineItem], parent: &[String]) -> Vec<Rc<Node>> {
    items
        .iter()
        .map(|item| {
            let mut path = parent.to_vec();
            path.push(item.title.clone());
            let children = build_nodes(&item.children, &path);
            Rc::new(Node {
                title: item.title.clone(),
                dest: item.dest.clone(),
                open: item.open,
                path,
                children,
            })
        })
        .collect()
}

fn row_node(row: &gtk::TreeListRow) -> Option<Rc<Node>> {
    let item = row.item()?.downcast::<glib::BoxedAnyObject>().ok()?;
    let node = item.try_borrow::<Rc<Node>>().ok()?.clone();
    Some(node)
}

fn strip_of(state: &WindowState) -> Option<Rc<Strip>> {
    state.pdf.get()?.strip().cloned()
}

/// A small icon and caption (a status page is too big for the strip).
fn placeholder(icon: &str, text: &str) -> gtk::Box {
    let empty = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .valign(gtk::Align::Center)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build();
    empty.append(&gtk::Image::builder().icon_name(icon).pixel_size(32).build());
    empty.append(
        &gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["caption"])
            .build(),
    );
    empty
}

fn thumbnails_list(strip: &Weak<Strip>) -> (gtk::ListView, gtk::StringList, gtk::SingleSelection) {
    let model = gtk::StringList::new(&[]);
    let selection = gtk::SingleSelection::builder()
        .model(&model)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let picture = PagePicture::new();
        picture.set_halign(gtk::Align::Fill);
        let label = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .css_classes(["caption", "numeric", "lemma-thumb-label"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.append(&picture);
        row.append(&label);
        item.set_child(Some(&row));
    });
    let weak = strip.clone();
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(strip), Some((picture, label))) = (weak.upgrade(), row_widgets(item)) else {
            return;
        };
        let weak = Rc::downgrade(&strip);
        picture.connect_resized(move |_| {
            if let Some(strip) = weak.upgrade() {
                strip.schedule_wants();
            }
        });
        strip.bind_thumbnail(item.position(), &picture, &label);
    });
    let weak = strip.clone();
    factory.connect_unbind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(strip), Some((picture, _))) = (weak.upgrade(), row_widgets(item)) else {
            return;
        };
        strip.unbind_thumbnail(item.position(), &picture);
    });
    let list = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .css_classes(["lemma-pdf-thumbs"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Page Thumbnails")]);
    (list, model, selection)
}

/// The picture and label of a thumbnail row.
fn row_widgets(item: &gtk::ListItem) -> Option<(PagePicture, gtk::Label)> {
    let row = item.child()?;
    let picture = row.first_child()?.downcast::<PagePicture>().ok()?;
    let label = picture.next_sibling()?.downcast::<gtk::Label>().ok()?;
    Some((picture, label))
}

fn bookmarks_list(
    strip: &Weak<Strip>,
) -> (
    gtk::ListView,
    gio::ListStore,
    gtk::TreeListModel,
    gtk::SingleSelection,
) {
    let root = gio::ListStore::new::<glib::BoxedAnyObject>();
    let tree = gtk::TreeListModel::new(root.clone(), false, false, |item| {
        let node = item
            .downcast_ref::<glib::BoxedAnyObject>()?
            .try_borrow::<Rc<Node>>()
            .ok()?
            .clone();
        if node.children.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for child in &node.children {
            store.append(&glib::BoxedAnyObject::new(child.clone()));
        }
        Some(store.upcast())
    });
    let selection = gtk::SingleSelection::builder()
        .model(&tree)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        let page = gtk::Label::builder()
            .xalign(1.0)
            .css_classes(["dim-label", "caption", "numeric", "lemma-bookmark-page"])
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.append(&title);
        row.append(&page);
        let expander = gtk::TreeExpander::builder()
            .child(&row)
            .indent_for_icon(true)
            .build();
        item.set_child(Some(&expander));
    });
    let weak = strip.clone();
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        let row = item.item().and_downcast::<gtk::TreeListRow>();
        expander.set_list_row(row.as_ref());
        // Flat outlines need no room for expander arrows.
        expander.set_indent_for_icon(weak.upgrade().is_none_or(|s| s.nested.get()));
        let Some(node) = row.as_ref().and_then(row_node) else {
            return;
        };
        let Some(box_) = expander.child() else {
            return;
        };
        let (Some(title), Some(page)) = (
            box_.first_child().and_downcast::<gtk::Label>(),
            box_.last_child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        title.set_text(&node.title);
        title.set_tooltip_text(Some(&node.title));
        let label = match (&node.dest, weak.upgrade()) {
            (LinkDest::Page { page: p, .. }, Some(strip)) => {
                let mut rows = strip.bookmark_pages.borrow_mut();
                rows.retain(|(l, _)| l.upgrade().is_some_and(|l| l != page));
                rows.push((page.downgrade(), *p));
                strip.page_label(*p)
            }
            (LinkDest::Page { page, .. }, None) => (page + 1).to_string(),
            _ => String::new(),
        };
        page.set_text(&label);
    });
    let weak = strip.clone();
    factory.connect_unbind(move |_, item| {
        let Some(expander) = item
            .downcast_ref::<gtk::ListItem>()
            .and_then(|i| i.child())
            .and_downcast::<gtk::TreeExpander>()
        else {
            return;
        };
        expander.set_list_row(None);
        if let Some(strip) = weak.upgrade()
            && let Some(label) = expander.child().and_then(|b| b.last_child())
        {
            strip.bookmark_pages.borrow_mut().retain(|(l, _)| {
                l.upgrade()
                    .is_some_and(|l| l.upcast_ref::<gtk::Widget>() != &label)
            });
        }
    });
    let list = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .css_classes(["navigation-sidebar", "lemma-pdf-bookmarks"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Bookmarks")]);
    (list, root, tree, selection)
}

/// Called once per window after the M2 install hooks: fills `state.pdf_sidebar.slot`.
pub fn install(state: &Rc<WindowState>) {
    let Some(pdf) = state.pdf.get() else {
        return;
    };
    install_css_once("pdf-strip", CSS);
    let view = pdf.view().clone();
    let total = (state.config.pdf.cache_mib as usize).max(1) << 20;
    let strip = Rc::new_cyclic(|weak: &Weak<Strip>| {
        let (thumbs, thumbs_model, thumbs_selection) = thumbnails_list(weak);
        let (bookmarks, outline_root, tree, bookmarks_selection) = bookmarks_list(weak);
        let bookmarks_stack = gtk::Stack::new();
        bookmarks_stack.add_named(
            &gtk::ScrolledWindow::builder()
                .child(&bookmarks)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .build(),
            Some("list"),
        );
        bookmarks_stack.add_named(
            &placeholder("view-list-bullet-symbolic", "No Bookmarks"),
            Some("empty"),
        );
        bookmarks_stack.add_named(
            &adw::Spinner::builder()
                .width_request(24)
                .height_request(24)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build(),
            Some("loading"),
        );
        bookmarks_stack.set_visible_child_name("loading");
        let stack = adw::ViewStack::new();
        let page = stack.add_titled_with_icon(
            &gtk::ScrolledWindow::builder()
                .child(&thumbs)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .build(),
            Some(Page::Thumbnails.name()),
            "Thumbnails",
            "view-grid-symbolic",
        );
        page.set_use_underline(false);
        stack.add_titled_with_icon(
            &bookmarks_stack,
            Some(Page::Bookmarks.name()),
            "Bookmarks",
            "user-bookmarks-symbolic",
        );
        Strip {
            state: Rc::downgrade(state),
            view: view.clone(),
            split: state.pdf_sidebar.split.clone(),
            stack,
            total,
            thumbs,
            thumbs_model,
            thumbs_selection,
            bound: RefCell::new(HashMap::new()),
            cache: RefCell::new(ByteLru::new(thumbnail_budget(total))),
            arrived: Cell::new(0),
            generation: Cell::new(0),
            sizes: RefCell::new(Vec::new()),
            labels: RefCell::new(Vec::new()),
            labels_generation: Cell::new(0),
            wants_scheduled: Cell::new(false),
            syncing: Cell::new(false),
            active: Cell::new(false),
            chosen: Cell::new(None),
            bookmarks,
            bookmarks_stack,
            outline_root,
            tree,
            bookmarks_selection,
            outline_generation: Cell::new(0),
            outline_requested: Cell::new(0),
            expanded: RefCell::new(HashSet::new()),
            bookmark_pages: RefCell::new(Vec::new()),
            outline_seen: Cell::new(false),
            nested: Cell::new(false),
        }
    });
    let switcher = adw::InlineViewSwitcher::builder()
        .stack(&strip.stack)
        .display_mode(adw::InlineViewSwitcherDisplayMode::Icons)
        .homogeneous(true)
        .css_classes(["lemma-pdf-strip-switcher"])
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("lemma-pdf-strip");
    root.append(&switcher);
    root.append(&strip.stack);
    state.pdf_sidebar.slot.set_child(Some(&root));
    pdf.set_strip(strip.clone());

    let weak = Rc::downgrade(&strip);
    strip
        .thumbs_selection
        .connect_selected_notify(move |selection| {
            if let Some(strip) = weak.upgrade() {
                strip.thumbnail_selected(selection.selected());
            }
        });
    let weak = Rc::downgrade(&strip);
    strip
        .bookmarks_selection
        .connect_selected_notify(move |selection| {
            if let Some(strip) = weak.upgrade() {
                strip.bookmark_selected(selection.selected());
            }
        });
    // Enter (or a double click) on a row also gives the focus to the view.
    for list in [&strip.thumbs, &strip.bookmarks] {
        let view = view.clone();
        list.connect_activate(move |_, _| {
            view.grab_focus();
        });
    }
    let weak = Rc::downgrade(&strip);
    strip.stack.connect_visible_child_name_notify(move |_| {
        if let Some(strip) = weak.upgrade() {
            strip.fit_width();
            strip.refresh_page();
        }
    });
    strip.fit_width();
    let weak = Rc::downgrade(&strip);
    state
        .pdf_sidebar
        .split
        .connect_show_sidebar_notify(move |_| {
            if let Some(strip) = weak.upgrade() {
                strip.shown_changed();
            }
        });
    let weak = Rc::downgrade(&strip);
    view.connect_changed(move |view| {
        if let Some(strip) = weak.upgrade() {
            strip.shown_changed();
            strip.follow_view();
            let dark = view.is_dark();
            for row in strip.bound.borrow().values() {
                row.picture.set_dark(dark);
            }
        }
    });
    let weak = Rc::downgrade(&strip);
    strip.thumbs.connect_map(move |_| {
        if let Some(strip) = weak.upgrade() {
            strip.schedule_wants();
        }
    });
}

// ----- for tests and the demo -----

/// Shows `page` of the strip (the strip itself is shown with `win.toggle-pdf-sidebar`).
pub fn show_page(state: &WindowState, page: Page) {
    if let Some(strip) = strip_of(state) {
        strip.stack.set_visible_child_name(page.name());
    }
}

/// The page of the strip shown.
pub fn visible_page(state: &WindowState) -> Option<Page> {
    strip_of(state).map(|s| s.page())
}

/// Bytes of the cached thumbnails, and their budget (more are cached only while the bound rows
/// need more).
pub fn thumbnail_bytes(state: &WindowState) -> Option<(usize, usize)> {
    let strip = strip_of(state)?;
    let used = strip.cache.borrow().used();
    Some((used, thumbnail_budget(strip.total)))
}

/// The thumbnails that have arrived from the PDF thread since the window opened.
pub fn thumbnails_arrived(state: &WindowState) -> usize {
    strip_of(state).map_or(0, |s| s.arrived.get())
}

/// The pages whose thumbnail rows are bound and laid out, with whether each shows a thumbnail of
/// the current generation at its width, which the cache holds too.
pub fn thumbnail_rows(state: &WindowState) -> Vec<(u32, bool)> {
    let Some(strip) = strip_of(state) else {
        return Vec::new();
    };
    let generation = strip.generation.get();
    let scale = strip
        .thumbs
        .native()
        .and_then(|n| n.surface())
        .map_or(1.0, |s| s.scale());
    let mut cache = strip.cache.borrow_mut();
    let mut rows: Vec<(u32, bool)> = strip
        .bound
        .borrow()
        .iter()
        .filter(|(_, row)| row.picture.width() > 0)
        .map(|(&page, row)| {
            let ready = render_width(row.picture.width(), scale).is_some_and(|w| {
                let current = |(g, width): (u64, u16)| g == generation && width >= w;
                row.shows.is_some_and(current)
                    && cache
                        .get(&page)
                        .is_some_and(|t| current((t.generation, t.width)))
            }) && row.picture.texture().is_some();
            (page, ready)
        })
        .collect();
    rows.sort_unstable();
    rows
}

/// The thumbnail list (tests select rows and scroll it).
pub fn thumbnail_list(state: &WindowState) -> Option<(gtk::ListView, gtk::SingleSelection)> {
    strip_of(state).map(|s| (s.thumbs.clone(), s.thumbs_selection.clone()))
}

/// The label under each thumbnail.
pub fn thumbnail_labels(state: &WindowState) -> Vec<String> {
    strip_of(state).map_or(Vec::new(), |s| s.labels.borrow().clone())
}

/// The bookmark rows shown (expanded entries' children included), as `(depth, title, page)`
/// with 0-based pages, and the selected row.
pub fn bookmark_rows(state: &WindowState) -> (Vec<BookmarkRow>, Option<u32>) {
    let Some(strip) = strip_of(state) else {
        return (Vec::new(), None);
    };
    let rows = (0..strip.tree.n_items())
        .filter_map(|i| {
            let row = strip.tree.row(i)?;
            let node = row_node(&row)?;
            let page = match node.dest {
                LinkDest::Page { page, .. } => Some(page),
                _ => None,
            };
            Some((row.depth(), node.title.clone(), page))
        })
        .collect();
    let selected = strip.bookmarks_selection.selected();
    (
        rows,
        (selected != gtk::INVALID_LIST_POSITION).then_some(selected),
    )
}

/// Expands or collapses bookmark row `row`.
pub fn set_bookmark_expanded(state: &WindowState, row: u32, expanded: bool) {
    if let Some(strip) = strip_of(state)
        && let Some(r) = strip.tree.row(row)
    {
        r.set_expanded(expanded);
    }
}

/// Selects bookmark row `row`, as a click does.
pub fn select_bookmark(state: &WindowState, row: u32) {
    if let Some(strip) = strip_of(state) {
        strip.bookmarks_selection.select_item(row, true);
    }
}

/// True when the outline of the shown generation has been loaded.
pub fn bookmarks_loaded(state: &WindowState) -> bool {
    strip_of(state).is_some_and(|s| {
        s.outline_generation.get() == s.generation.get() && s.generation.get() != 0
    })
}

/// True when the strip's visible page is complete: every bound thumbnail is drawn from a
/// current thumbnail, or the bookmarks are loaded.
pub fn is_ready(state: &WindowState) -> bool {
    match visible_page(state) {
        Some(Page::Thumbnails) => {
            let rows = thumbnail_rows(state);
            !rows.is_empty() && rows.iter().all(|(_, ready)| *ready)
        }
        Some(Page::Bookmarks) => bookmarks_loaded(state),
        None => false,
    }
}

/// A line for the demo's report.
pub fn describe(state: &WindowState) -> String {
    match visible_page(state) {
        Some(Page::Thumbnails) => {
            let rows = thumbnail_rows(state);
            let (used, cap) = thumbnail_bytes(state).unwrap_or_default();
            format!(
                "thumbnails of pages {}–{} ({:.1} of {:.0} MiB)",
                rows.first().map_or(0, |r| r.0 + 1),
                rows.last().map_or(0, |r| r.0 + 1),
                used as f64 / f64::from(1 << 20),
                cap as f64 / f64::from(1 << 20)
            )
        }
        Some(Page::Bookmarks) => format!("{} bookmark rows", bookmark_rows(state).0.len()),
        None => "none".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_widths_round_up_to_the_step() {
        assert_eq!(render_width(0, 1.0), None);
        assert_eq!(render_width(1, 1.0), Some(32));
        assert_eq!(render_width(150, 1.0), Some(160));
        assert_eq!(render_width(160, 1.0), Some(160));
        assert_eq!(render_width(150, 2.0), Some(320));
        assert_eq!(render_width(150, 1.25), Some(192));
        assert_eq!(render_width(5000, 1.0), Some(2048));
    }

    #[test]
    fn the_budget_is_an_eighth() {
        assert_eq!(thumbnail_budget(48 << 20), 6 << 20);
    }
}
