//! Inline review rendering (WP1.2, ARCHITECTURE §4.3): per-hunk anchor tags, the cached layouts
//! of deleted lines, painting, and the pool of 8 `ReviewBar` overlays.
//!
//! The buffer always holds the agent's proposal. For every hunk of the document's review:
//!
//! - its current (added) lines carry the shared `lemma-review-added` tag, a green paragraph
//!   background, and its changed words `lemma-review-added-word`, a stronger green;
//! - its **anchor line** (the first current line; for a pure deletion the line after it) carries
//!   a pooled anchor tag whose `pixels-above-lines` reserves a header band plus the height of the
//!   deleted text. A deletion at the end of the file, where no line follows, reserves the space
//!   with `pixels-below-lines` on the last line with text instead;
//! - [`paint`] (called from `TexView::snapshot_layer(AboveText)`, in buffer coordinates) fills
//!   the reserved space: an opaque base, the band, then the deleted text in red with its deleted
//!   words darker, laid out like the view's text (font, tabs, wrapping to the text width);
//! - one of [`BAR_POOL`] action bars (Accept Alt+Y · Reject Alt+N · i/n, with the shortcuts
//!   actually bound) is placed in the band with `TextView::add_overlay`, so it never covers text.
//!
//! The plan (which hunk reserves what where) is rebuilt after every recompute; tags are only
//! touched when it changes. Reading mode (Alt+D) shrinks each band to its header and hides the
//! deleted text.

use crate::document::{DocRef, Document, ObserverId};
use crate::editor::{TexView, highlight, remove_spacing_tag, remove_tag_everywhere};
use crate::review_ui::{self, Decision};
use gtk::prelude::*;
use gtk::{gdk, graphene, gsk};
use lemma_core::LineIndex;
use lemma_review::{Hunk, HunkId, Side};
use lemma_syntax::Context;
use sourceview5::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::{Rc, Weak};

/// Action bars per view: hunks near the visible area get one each.
pub const BAR_POOL: usize = 8;
/// Header band height as a share of the line height (it grows to fit the bar).
const HEADER_SCALE: f64 = 0.9;
/// Space between a bar and the right edge of the band.
const BAR_RIGHT_MARGIN: i32 = 6;
/// Deleted text longer than this many lines is drawn shortened: its first [`PHANTOM_HEAD`] and
/// last [`PHANTOM_TAIL`] lines around one "⋯ n more deleted lines ⋯" line.
const PHANTOM_MAX_LINES: usize = 500;
const PHANTOM_HEAD: usize = 300;
const PHANTOM_TAIL: usize = 100;
/// Lines per Pango layout of deleted text: Pango's cost grows faster than linearly with the size
/// of one layout, and only the chunks in view are drawn.
const PHANTOM_CHUNK_LINES: usize = 50;
const ADDED_TAG: &str = "lemma-review-added";
const WORD_TAG: &str = "lemma-review-added-word";

/// Called with a hunk and a decision when a bar button is clicked.
pub type DecideFn = Box<dyn Fn(HunkId, Decision)>;

/// Where a hunk's band and deleted text are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Above this line (`pixels-above-lines`).
    Above(u32),
    /// Below this line, the last line with text (`pixels-below-lines`): a deletion at the end.
    Below(u32),
    /// Above the first line of an empty document (the view's top margin grows).
    Top,
}

/// What one hunk reserves and where.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: HunkId,
    /// 0-based position of the hunk in its file, and the number of hunks in the file.
    pub index: usize,
    pub count: usize,
    pub anchor: Anchor,
    /// Current lines of the hunk (empty for a pure deletion).
    pub lines: Range<u32>,
    /// Height of the drawn deleted text (0 for a pure insertion and in reading mode).
    pub deleted: i32,
    /// The hunk has deleted text (drawn or hidden by reading mode).
    pub has_old: bool,
    /// Number of deleted (baseline) lines.
    pub old_lines: u32,
    pub touched: bool,
    /// Offset of this block in the space reserved at its anchor (blocks sharing an anchor stack).
    pub offset: i32,
    /// Index of the anchor tag (blocks sharing an anchor share it).
    pub group: usize,
}

/// Everything derived from the hunks that the tags and the painting use.
#[derive(Clone, Debug, Default, PartialEq)]
struct Plan {
    blocks: Vec<Block>,
    /// Space reserved per anchor tag (index = `Block::group`), with its anchor.
    groups: Vec<(Anchor, i32)>,
    /// Line ranges with the added-line background.
    added: Vec<Range<u32>>,
    /// Changed words: line and byte range within the line.
    words: Vec<(u32, Range<u32>)>,
    /// The blocks drawn at a line (anchored [`Anchor::Above`] or [`Anchor::Below`] it): the
    /// line and the block's index, by line and then in block order, for the gutter.
    anchored: Vec<(u32, usize)>,
    header: i32,
    /// Width the deleted text wraps at (`None`: no wrapping).
    wrap: Option<i32>,
}

/// The cached layouts of a hunk's deleted text.
///
/// Cached by content (the deleted text, its deleted words, the wrap width, the scheme and the
/// scanner context its syntax colours start in), not by [`HunkId`]: typing inside a hunk changes
/// its id but not its deleted text, and must not lay the deleted text out again.
struct Phantom {
    old_text: String,
    /// Deleted words: line of `old_text` and byte range within it.
    words: Vec<(u32, Range<u32>)>,
    wrap: Option<i32>,
    dark: bool,
    /// Where the syntax colours start (`highlight::phantom_context`); `None` without scanner
    /// highlighting.
    context: Option<Context>,
    /// Consecutive layouts of at most [`PHANTOM_CHUNK_LINES`] lines: y offset, height, layout.
    chunks: Vec<(i32, i32, pango::Layout)>,
    height: i32,
}

impl Phantom {
    fn matches(
        &self,
        hunk: &Hunk,
        words: &[(u32, Range<u32>)],
        wrap: Option<i32>,
        dark: bool,
        context: &Option<Context>,
    ) -> bool {
        self.wrap == wrap
            && self.dark == dark
            && self.old_text.len() == hunk.old_text.len()
            && self.words == words
            && self.context == *context
            && self.old_text == hunk.old_text
    }
}

/// One action bar: Accept, Reject, position.
struct ReviewBar {
    root: gtk::Box,
    accept: gtk::Button,
    reject: gtk::Button,
    accept_key: gtk::Label,
    reject_key: gtk::Label,
    position: gtk::Label,
    hunk: Cell<Option<HunkId>>,
    /// Where the bar was placed (buffer coordinates).
    at: Cell<Option<(i32, i32)>>,
}

/// Review rendering state of one view (stored in the [`TexView`] while attached).
pub struct ViewReview {
    view: glib::WeakRef<TexView>,
    doc: Weak<Document>,
    buffer: sourceview5::Buffer,
    added: gtk::TextTag,
    word: gtk::TextTag,
    anchors: RefCell<Vec<gtk::TextTag>>,
    plan: RefCell<Plan>,
    /// Deleted-text layouts of the current plan's hunks (see [`Phantom`]), by [`phantom_key`].
    phantoms: RefCell<HashMap<u64, Vec<Rc<Phantom>>>>,
    /// The phantom of each hunk of the current plan.
    phantom_of: RefCell<HashMap<HunkId, Rc<Phantom>>>,
    /// Height of an action bar, measured for this line height and (un)rooted view.
    bar_height: Cell<Option<(i32, bool, i32)>>,
    bars: Vec<ReviewBar>,
    reading: Cell<bool>,
    cursor_hunk: Cell<Option<HunkId>>,
    relayout_queued: Cell<bool>,
    /// [`ViewReview::apply`] is tagging the buffer.
    applying: Cell<bool>,
    /// The review's tags may cover more than the ranges [`ViewReview::apply`] gave them: set
    /// until the first apply, and when something else applies one (a paste copies the tags of
    /// the text it copies); the next sync tags everything again. Otherwise GTK's edits move the
    /// ranges with the text and never add one, so the ends of a range tell whether it is where
    /// the plan wants it, which keeps a sync with thousands of hunks linear: each search or
    /// change of a tag costs GTK time that grows with the number of tags.
    tags_unsure: Cell<bool>,
    /// How many ranges the added-lines and changed-words tags got from the last apply.
    shared_ranges: Cell<(usize, usize)>,
    /// Document version the plan was last checked against.
    plan_version: Cell<Option<u64>>,
    /// The view's own top margin, restored when no hunk needs the space above line 1.
    base_top_margin: i32,
    decide: DecideFn,
    observer: Cell<Option<ObserverId>>,
    handlers: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
}

impl ViewReview {
    /// Attaches review rendering to `view`, which shows `buffer`, the buffer of the open document
    /// `doc`, and stores the state in the view. `decide` runs when a bar button is clicked.
    pub fn attach(
        view: &TexView,
        doc: &DocRef,
        buffer: &sourceview5::Buffer,
        decide: DecideFn,
    ) -> Rc<Self> {
        let table = buffer.tag_table();
        let tag = |name: &str| {
            table.lookup(name).unwrap_or_else(|| {
                let tag = gtk::TextTag::new(Some(name));
                table.add(&tag);
                tag
            })
        };
        let (added, word) = (tag(ADDED_TAG), tag(WORD_TAG));
        let review = Rc::new_cyclic(|weak: &Weak<Self>| Self {
            view: view.downgrade(),
            doc: Rc::downgrade(doc),
            buffer: buffer.clone(),
            added,
            word,
            anchors: RefCell::new(Vec::new()),
            plan: RefCell::new(Plan::default()),
            phantoms: RefCell::new(HashMap::new()),
            phantom_of: RefCell::new(HashMap::new()),
            bar_height: Cell::new(None),
            bars: (0..BAR_POOL).map(|i| ReviewBar::new(weak, i)).collect(),
            reading: Cell::new(false),
            cursor_hunk: Cell::new(None),
            relayout_queued: Cell::new(false),
            applying: Cell::new(false),
            tags_unsure: Cell::new(true),
            shared_ranges: Cell::new((0, 0)),
            plan_version: Cell::new(None),
            base_top_margin: view.top_margin(),
            decide,
            observer: Cell::new(None),
            handlers: RefCell::new(Vec::new()),
        });
        for bar in &review.bars {
            view.add_overlay(&bar.root, 0, 0);
        }
        review.connect(view, doc);
        view.set_review_state(Some(review.clone()));
        review.update_tag_colors();
        review.sync();
        review
    }

    fn connect(self: &Rc<Self>, view: &TexView, doc: &DocRef) {
        let weak = Rc::downgrade(self);
        self.observer.set(Some(doc.connect_hunks_changed(move || {
            if let Some(review) = weak.upgrade() {
                review.sync();
            }
        })));
        let mut handlers = self.handlers.borrow_mut();
        let adjustments = [view.vadjustment(), view.hadjustment()];
        for adjustment in adjustments.into_iter().flatten() {
            let weak = Rc::downgrade(self);
            let id = adjustment.connect_value_changed(move |_| {
                if let Some(review) = weak.upgrade() {
                    review.queue_relayout();
                }
            });
            handlers.push((adjustment.clone().upcast(), id));
            let weak = Rc::downgrade(self);
            let id = adjustment.connect_changed(move |_| {
                if let Some(review) = weak.upgrade() {
                    review.queue_relayout();
                }
            });
            handlers.push((adjustment.upcast(), id));
        }
        let weak = Rc::downgrade(self);
        let id = self.buffer.connect_cursor_position_notify(move |_| {
            if let Some(review) = weak.upgrade() {
                review.update_cursor_hunk();
            }
        });
        handlers.push((self.buffer.clone().upcast(), id));
        let weak = Rc::downgrade(self);
        let id = self.buffer.connect_apply_tag(move |_, tag, _, _| {
            if let Some(review) = weak.upgrade()
                && !review.applying.get()
                && review.is_own_tag(tag)
            {
                review.tags_unsure.set(true);
            }
        });
        handlers.push((self.buffer.clone().upcast(), id));
        let weak = Rc::downgrade(self);
        let id = self.buffer.connect_style_scheme_notify(move |_| {
            if let Some(review) = weak.upgrade() {
                review.update_tag_colors();
                review.phantoms.borrow_mut().clear();
                review.phantom_of.borrow_mut().clear();
                review.sync();
            }
        });
        handlers.push((self.buffer.clone().upcast(), id));
    }

    /// Removes the tags, bars and signal handlers; the view no longer shows the review.
    pub fn detach(&self) {
        if let (Some(doc), Some(id)) = (self.doc.upgrade(), self.observer.take()) {
            doc.disconnect(id);
        }
        for (object, id) in self.handlers.borrow_mut().drain(..) {
            object.disconnect(id);
        }
        remove_tag_everywhere(&self.buffer, &self.added);
        remove_tag_everywhere(&self.buffer, &self.word);
        for tag in self.anchors.borrow().iter() {
            remove_tag_everywhere(&self.buffer, tag);
        }
        if let Some(view) = self.view.upgrade() {
            // A closed tab's view was disposed first, which already removed its children.
            let parent = Some(view.clone().upcast::<gtk::Widget>());
            for bar in self.bars.iter().filter(|b| b.root.parent() == parent) {
                view.remove(&bar.root);
            }
            view.set_top_margin(self.base_top_margin);
            view.set_review_state(None);
        }
    }

    /// The layouts the deleted text of hunk `id` is drawn with, in the current plan (tests).
    pub fn phantom_layouts(&self, id: HunkId) -> Vec<pango::Layout> {
        self.phantom_of
            .borrow()
            .get(&id)
            .map(|p| p.chunks.iter().map(|(_, _, l)| l.clone()).collect())
            .unwrap_or_default()
    }

    /// Reading mode: bands shrink to their header and deleted text is hidden.
    pub fn set_reading(&self, on: bool) {
        if self.reading.replace(on) != on {
            self.sync();
        }
    }

    /// True in reading mode.
    pub fn reading(&self) -> bool {
        self.reading.get()
    }

    /// The blocks of the current plan, in text order (for tests and the gutter).
    pub fn blocks(&self) -> Vec<Block> {
        self.plan.borrow().blocks.clone()
    }

    /// Height of the header band in pixels.
    pub fn header_height(&self) -> i32 {
        self.plan.borrow().header
    }

    /// The bars currently shown: their hunk and their rectangle in buffer coordinates, as
    /// placed with `move_overlay` (the widget allocation follows on the next frame).
    pub fn visible_bars(&self) -> Vec<(HunkId, gdk::Rectangle)> {
        self.bars
            .iter()
            .filter(|bar| bar.root.is_visible())
            .filter_map(|bar| {
                let id = bar.hunk.get()?;
                let (x, y) = bar.at.get()?;
                let (w, h) = bar.size();
                Some((id, gdk::Rectangle::new(x, y, w, h)))
            })
            .collect()
    }

    /// Every bar widget of the pool (shown or not).
    pub fn bar_widgets(&self) -> Vec<gtk::Widget> {
        self.bars.iter().map(|b| b.root.clone().upcast()).collect()
    }

    /// Shows these shortcuts (as `gtk::accelerator_get_label` writes them, e.g. "Alt+Y") on the
    /// bars' Accept and Reject buttons; `None` when the action has no shortcut.
    pub fn set_shortcuts(&self, accept: Option<&str>, reject: Option<&str>) {
        for bar in &self.bars {
            bar.set_shortcuts(accept, reject);
        }
        // The bars may have changed size.
        self.bar_height.set(None);
        self.sync();
    }

    /// True when the plan shows nothing (no hunks): the gutter then has no marks to look up.
    pub fn is_empty(&self) -> bool {
        self.plan.borrow().blocks.is_empty()
    }

    /// The hunk under the cursor (outlined), if any.
    pub fn cursor_hunk(&self) -> Option<HunkId> {
        self.cursor_hunk.get()
    }

    /// Rebuilds the plan from the document's hunks and updates tags, bars and drawing.
    pub fn sync(&self) {
        let (Some(view), Some(doc)) = (self.view.upgrade(), self.doc.upgrade()) else {
            return;
        };
        let plan = {
            let review = doc.review.borrow();
            match review.as_ref() {
                Some(review) if !review.hunks().is_empty() => {
                    self.plan_for(&view, &doc, review.hunks())
                }
                _ => {
                    self.phantom_of.borrow_mut().clear();
                    Plan::default()
                }
            }
        };
        // Keep only the layouts this plan uses.
        {
            let used: HashSet<*const Phantom> =
                self.phantom_of.borrow().values().map(Rc::as_ptr).collect();
            self.phantoms.borrow_mut().retain(|_, layouts| {
                layouts.retain(|p| used.contains(&Rc::as_ptr(p)));
                !layouts.is_empty()
            });
        }
        let changed = self.tags_unsure.get()
            || *self.plan.borrow() != plan
            || !self.tags_in_place(&doc, &plan);
        if changed {
            self.apply(&view, &doc, &plan);
            self.plan.replace(plan);
        }
        self.plan_version.set(Some(doc.version()));
        self.update_cursor_hunk();
        view.queue_draw();
        sourceview5::prelude::ViewExt::gutter(&view, gtk::TextWindowType::Left).queue_draw();
        self.queue_relayout();
    }

    fn plan_for(&self, view: &TexView, doc: &Document, hunks: &[Hunk]) -> Plan {
        let header = self.header_height_for(view);
        let wrap = wrap_width(view);
        let dark = Palette::new(Some(view), &self.buffer).dark;
        let reading = self.reading.get();
        let index = doc.line_index();
        let mut plan = Plan {
            header,
            wrap,
            ..Plan::default()
        };
        let mut phantom_of = HashMap::new();
        for (i, hunk) in hunks.iter().enumerate() {
            let anchor = anchor_of(hunk, &index);
            let has_old = !hunk.old_text.is_empty();
            let deleted = if reading || !has_old {
                0
            } else {
                let phantom = self.phantom(view, hunk, wrap, dark);
                let height = phantom.height;
                phantom_of.insert(hunk.id, phantom);
                height
            };
            let space = header + deleted;
            let (group, offset) = match plan.groups.last_mut() {
                Some((a, total)) if *a == anchor => {
                    let offset = *total;
                    *total += space;
                    (plan.groups.len() - 1, offset)
                }
                _ => {
                    plan.groups.push((anchor, space));
                    (plan.groups.len() - 1, 0)
                }
            };
            plan.blocks.push(Block {
                id: hunk.id,
                index: i,
                count: hunks.len(),
                anchor,
                lines: hunk.cur_lines.clone(),
                deleted,
                has_old,
                old_lines: hunk.base_lines.len() as u32,
                touched: hunk.user_touched,
                offset,
                group,
            });
            if !hunk.cur_lines.is_empty() {
                plan.added.push(hunk.cur_lines.clone());
            }
            plan.words.extend(
                hunk.words
                    .iter()
                    .filter(|w| w.side == Side::New)
                    .map(|w| (hunk.cur_lines.start + w.line, w.bytes.clone())),
            );
        }
        plan.anchored = plan
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, block)| match block.anchor {
                Anchor::Above(line) | Anchor::Below(line) => Some((line, i)),
                Anchor::Top => None,
            })
            .collect();
        plan.anchored.sort_by_key(|&(line, _)| line);
        self.phantom_of.replace(phantom_of);
        plan
    }

    /// The header band height: about 0.9 of a line, and at least the bar's height.
    ///
    /// The bar's height is measured whether or not a bar is shown (GTK measures a hidden widget
    /// as 0), so the band has the same height on every plan.
    fn header_height_for(&self, view: &TexView) -> i32 {
        let line = line_height(view);
        let rooted = view.root().is_some();
        let bar = match self.bar_height.get() {
            Some((l, r, h)) if l == line && r == rooted => h,
            _ => {
                let h = self.bars.first().map_or(0, ReviewBar::measured_height);
                self.bar_height.set(Some((line, rooted, h)));
                h
            }
        };
        ((f64::from(line) * HEADER_SCALE).round() as i32).max(bar + 4)
    }

    /// True when every anchor tag covers exactly its planned line (GTK moves tags with the text,
    /// but an edit can split or extend a tagged line).
    fn tags_in_place(&self, doc: &Document, plan: &Plan) -> bool {
        let anchors = self.anchors.borrow();
        anchors.len() == plan.groups.len()
            && plan
                .groups
                .iter()
                .zip(anchors.iter())
                .all(|((anchor, _), tag)| self.anchor_in_place(doc, tag, *anchor))
    }

    /// True when `tag` covers exactly the line of `anchor` (nothing for [`Anchor::Top`]).
    fn anchor_in_place(&self, doc: &Document, tag: &gtk::TextTag, anchor: Anchor) -> bool {
        let line = match anchor {
            Anchor::Above(l) | Anchor::Below(l) => l,
            Anchor::Top => return tag_start(&self.buffer, tag).is_none(),
        };
        let index = doc.line_index();
        if line >= index.line_count() {
            return false;
        }
        let (want_start, want_end) = (index.line_start(line), line_end(&index, line + 1));
        drop(index);
        if !self.tags_unsure.get() {
            // Its one range starts and ends there (see `tags_unsure`).
            return ranges_in_place(doc, tag, std::slice::from_ref(&(want_start..want_end)));
        }
        let Some(start) = tag_start(&self.buffer, tag) else {
            return false;
        };
        let mut end = start;
        end.forward_to_tag_toggle(Some(tag));
        let mut rest = end;
        doc.iter_at_byte(want_start).as_ref() == Some(&start)
            && doc.iter_at_byte(want_end).as_ref() == Some(&end)
            && !rest.forward_to_tag_toggle(Some(tag))
    }

    /// Re-tags the buffer for `plan`. The shared tags and each anchor tag are left alone where
    /// GTK moved them with the text to where the plan wants them; otherwise they are removed
    /// only where they are and applied again. Surplus anchor tags leave the tag table, so their
    /// cost does not outlive their hunks.
    fn apply(&self, view: &TexView, doc: &Document, plan: &Plan) {
        let buffer = &self.buffer;
        self.applying.set(true);
        self.resize_anchor_pool(plan.groups.len());
        let anchors = self.anchors.borrow();
        let index = doc.line_index();
        let at = |offset: usize| doc.iter_at_byte(offset);
        // The added lines and the changed words, as GTK keeps them: ranges that touch are one.
        let added = joined(
            plan.added
                .iter()
                .map(|lines| index.line_start(lines.start)..line_end(&index, lines.end)),
        );
        let line_count = index.line_count();
        let words = joined(
            plan.words
                .iter()
                .filter(|(line, _)| *line < line_count)
                .map(|(line, bytes)| {
                    let base = index.line_start(*line);
                    base + bytes.start as usize..base + bytes.end as usize
                }),
        );
        // Usually only a few hunks changed: the others' ranges moved with the text.
        let kept = !self.tags_unsure.get()
            && self.shared_ranges.get() == (added.len(), words.len())
            && ranges_in_place(doc, &self.added, &added)
            && ranges_in_place(doc, &self.word, &words);
        if !kept {
            remove_tag_everywhere(buffer, &self.added);
            remove_tag_everywhere(buffer, &self.word);
            let tag_all = |tag: &gtk::TextTag, ranges: &[Range<usize>]| {
                let mut tagged = 0;
                for range in ranges {
                    if let (Some(a), Some(b)) = (at(range.start), at(range.end)) {
                        buffer.apply_tag(tag, &a, &b);
                        tagged += 1;
                    }
                }
                tagged
            };
            self.shared_ranges
                .set((tag_all(&self.added, &added), tag_all(&self.word, &words)));
        }
        let range = |line: u32| (index.line_start(line), line_end(&index, line + 1));
        let mut top = 0;
        for ((anchor, space), tag) in plan.groups.iter().zip(anchors.iter()) {
            let (above, below) = match *anchor {
                Anchor::Above(_) => (*space, 0),
                Anchor::Below(_) => (0, *space),
                Anchor::Top => {
                    top += space;
                    (0, 0)
                }
            };
            // Only real changes: setting a tag property re-lays out the text it covers.
            if tag.pixels_above_lines() != above {
                tag.set_pixels_above_lines(above);
            }
            if tag.pixels_below_lines() != below {
                tag.set_pixels_below_lines(below);
            }
            if self.anchor_in_place(doc, tag, *anchor) {
                continue;
            }
            remove_tag_everywhere(buffer, tag);
            if let Anchor::Above(line) | Anchor::Below(line) = *anchor
                && line < line_count
            {
                let (start, end) = range(line);
                if let (Some(a), Some(b)) = (at(start), at(end)) {
                    buffer.apply_tag(tag, &a, &b);
                }
            }
        }
        view.set_top_margin(self.base_top_margin + top);
        self.applying.set(false);
        self.tags_unsure.set(false);
    }

    /// True for the review's own tags.
    fn is_own_tag(&self, tag: &gtk::TextTag) -> bool {
        // Anchor tags are the ones that reserve space.
        *tag == self.added
            || *tag == self.word
            || ((tag.is_pixels_above_lines_set() || tag.is_pixels_below_lines_set())
                && self.anchors.borrow().contains(tag))
    }

    /// Makes the pool of anchor tags exactly `n` long: new tags get the highest priority, surplus
    /// ones leave the text and the tag table (without laying out the whole text again).
    fn resize_anchor_pool(&self, n: usize) {
        let mut anchors = self.anchors.borrow_mut();
        let table = self.buffer.tag_table();
        let keep = n.min(anchors.len());
        for tag in anchors.drain(keep..) {
            remove_spacing_tag(&self.buffer, &tag);
        }
        while anchors.len() < n {
            let tag = gtk::TextTag::new(None);
            table.add(&tag);
            // Highest priority: nothing else may override the reserved space.
            tag.set_priority(table.size() - 1);
            anchors.push(tag);
        }
    }

    fn update_tag_colors(&self) {
        let palette = Palette::new(self.view.upgrade().as_ref(), &self.buffer);
        self.added
            .set_paragraph_background_rgba(Some(&palette.added));
        self.word.set_background_rgba(Some(&palette.added_word));
    }

    /// The layouts of `hunk`'s deleted text, from the cache when an identical deleted text was
    /// laid out already.
    fn phantom(&self, view: &TexView, hunk: &Hunk, wrap: Option<i32>, dark: bool) -> Rc<Phantom> {
        let words = old_words(hunk);
        // Syntax colours (WP2.7): the deleted text starts where the hunk's current lines do.
        let context = highlight::phantom_context(view, hunk.cur_lines.start);
        let key = phantom_key(&hunk.old_text, wrap, dark);
        if let Some(p) = self.phantoms.borrow().get(&key).and_then(|layouts| {
            layouts
                .iter()
                .find(|p| p.matches(hunk, &words, wrap, dark, &context))
        }) {
            return p.clone();
        }
        let syntax = context
            .as_ref()
            .map(|c| highlight::phantom_syntax(view, &hunk.old_text, c));
        let chunks = phantom_chunks(view, &hunk.old_text, &words, wrap, dark, syntax.as_ref());
        let height = chunks.last().map_or(0, |(y, h, _)| y + h);
        let phantom = Rc::new(Phantom {
            old_text: hunk.old_text.clone(),
            words,
            wrap,
            dark,
            context,
            chunks,
            height,
        });
        self.phantoms
            .borrow_mut()
            .entry(key)
            .or_default()
            .push(phantom.clone());
        phantom
    }

    fn update_cursor_hunk(&self) {
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        let iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let line = doc.line_index().line_of(doc.byte_offset(&iter));
        let id = review_ui::hunk_for_line(&doc, line);
        if self.cursor_hunk.replace(id) != id
            && let Some(view) = self.view.upgrade()
        {
            view.queue_draw();
        }
    }

    /// Top of the space reserved by anchor tag `group` (buffer coordinates), or `None` when the
    /// tag is not where the plan put it (a change since the last recompute).
    fn group_top(&self, view: &TexView, group: usize, anchor: Anchor, space: i32) -> Option<i32> {
        if anchor == Anchor::Top {
            let (y, _) = view.line_yrange(&self.buffer.start_iter());
            return Some(y - space);
        }
        let anchors = self.anchors.borrow();
        let start = tag_start(&self.buffer, anchors.get(group)?)?;
        if !start.starts_line() {
            return None;
        }
        let (y, height) = view.line_yrange(&start);
        Some(match anchor {
            Anchor::Below(_) => y + height - space,
            _ => y,
        })
    }

    /// [`Self::group_top`] of the groups whose space may lie in the buffer-y range `y0..y1`:
    /// those anchored on the lines there while the plan matches the text, else all of them (the
    /// plan's line numbers are stale until the next recompute). Other groups get `None`.
    fn group_tops(
        &self,
        view: &TexView,
        doc: &Document,
        plan: &Plan,
        y0: i32,
        y1: i32,
    ) -> Vec<Option<i32>> {
        let lines = (self.plan_version.get() == Some(doc.version())).then(|| {
            let line_at = |y: i32| {
                let (iter, _) = view.line_at_y(y);
                doc.line_index().line_of(doc.byte_offset(&iter))
            };
            line_at(y0).saturating_sub(1)..line_at(y1) + 2
        });
        plan.groups
            .iter()
            .enumerate()
            .map(|(i, &(anchor, space))| {
                let near = match (anchor, &lines) {
                    (Anchor::Above(l) | Anchor::Below(l), Some(lines)) => lines.contains(&l),
                    _ => true,
                };
                near.then(|| self.group_top(view, i, anchor, space))
                    .flatten()
            })
            .collect()
    }

    fn paint(&self, view: &TexView, snapshot: &gtk::Snapshot) {
        let plan = self.plan.borrow();
        if plan.blocks.is_empty() {
            return;
        }
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        let review = doc.review.borrow();
        let Some(review) = review.as_ref() else {
            return;
        };
        let vis = view.visible_rect();
        let palette = Palette::new(Some(view), &self.buffer);
        let tops = self.group_tops(view, &doc, &plan, vis.y(), vis.y() + vis.height());
        // As wide as the added lines' paragraph background (the text area without its right
        // margin).
        let (x, width) = (
            vis.x() as f32,
            (vis.width() - view.right_margin()).max(0) as f32,
        );
        let text_x = text_origin(view) as f32;
        let header = plan.header;
        for block in &plan.blocks {
            let Some(top) = tops[block.group] else {
                continue;
            };
            let band_y = top + block.offset;
            if band_y > vis.y() + vis.height() || band_y + header + block.deleted < vis.y() {
                continue;
            }
            let band = rect(x, band_y as f32, width, header as f32);
            snapshot.append_color(&palette.base, &band);
            snapshot.append_color(&palette.band, &band);
            snapshot.append_color(
                &palette.rule,
                &rect(x, (band_y + header) as f32 - 1.0, width, 1.0),
            );
            if block.has_old && self.reading.get() {
                // Reading mode hides the deleted lines; the band says how many.
                let label = hidden_label(view, block.old_lines);
                let (_, label_h) = label.pixel_size();
                snapshot.save();
                snapshot.translate(&graphene::Point::new(
                    text_x,
                    (band_y + (header - label_h) / 2) as f32,
                ));
                snapshot.append_layout(&label, &palette.deleted_fg);
                snapshot.restore();
            }
            if block.deleted == 0 {
                continue;
            }
            let phantom = match self.phantom_of.borrow().get(&block.id) {
                Some(p) if p.dark == palette.dark => p.clone(),
                _ => match review.hunk(block.id) {
                    Some(hunk) => self.phantom(view, hunk, plan.wrap, palette.dark),
                    None => continue,
                },
            };
            let area = rect(x, (band_y + header) as f32, width, block.deleted as f32);
            snapshot.append_color(&palette.base, &area);
            snapshot.append_color(&palette.deleted_bg, &area);
            snapshot.push_clip(&area);
            let (vis_top, vis_bottom) = (vis.y(), vis.y() + vis.height());
            for (y, height, layout) in &phantom.chunks {
                let top = band_y + header + y;
                if top > vis_bottom || top + height < vis_top {
                    continue;
                }
                snapshot.save();
                snapshot.translate(&graphene::Point::new(text_x, top as f32));
                snapshot.append_layout(layout, &palette.deleted_fg);
                snapshot.restore();
            }
            snapshot.pop();
        }
        if let Some(id) = self.cursor_hunk.get()
            && let Some(block) = plan.blocks.iter().find(|b| b.id == id)
            && let Some(top) = tops[block.group]
        {
            let y0 = top + block.offset;
            let y1 = match block.lines.end.checked_sub(1) {
                Some(last) if !block.lines.is_empty() => {
                    let index = doc.line_index();
                    let last = last.min(index.line_count().saturating_sub(1));
                    let offset = index.line_start(last);
                    drop(index);
                    doc.iter_at_byte(offset).map_or(y0 + header, |iter| {
                        let (y, h) = view.line_yrange(&iter);
                        y + h - self.below_space_on(&plan, last)
                    })
                }
                _ => y0 + header + block.deleted,
            };
            let outline = gsk::RoundedRect::from_rect(
                rect(
                    x + 1.0,
                    y0 as f32 + 0.5,
                    width - 3.0,
                    (y1 - y0) as f32 - 1.0,
                ),
                5.0,
            );
            snapshot.append_border(&outline, &[1.5; 4], &[palette.outline; 4]);
        }
    }

    /// Space reserved below `line` by deletions at the end of the file.
    fn below_space_on(&self, plan: &Plan, line: u32) -> i32 {
        plan.groups
            .iter()
            .filter(|(a, _)| *a == Anchor::Below(line))
            .map(|(_, space)| *space)
            .sum()
    }

    /// What the gutter draws next to the line starting at `iter` (`line` in `LineIndex`
    /// numbering).
    pub(crate) fn gutter_marks(&self, iter: &gtk::TextIter, line: u32) -> GutterMarks {
        let plan = self.plan.borrow();
        let mut marks = GutterMarks {
            added: iter.has_tag(&self.added),
            ..GutterMarks::default()
        };
        if plan.blocks.is_empty() {
            return marks;
        }
        let first = plan.blocks.partition_point(|b| b.lines.end <= line);
        marks.touched = plan.blocks[first..]
            .iter()
            .take_while(|b| b.lines.start <= line)
            .any(|b| b.touched && b.lines.contains(&line));
        let anchors = self.anchors.borrow();
        let at = plan.anchored.partition_point(|&(l, _)| l < line);
        for &(_, i) in plan.anchored[at..].iter().take_while(|&&(l, _)| l == line) {
            let block = &plan.blocks[i];
            let below = matches!(block.anchor, Anchor::Below(_));
            if !anchors.get(block.group).is_some_and(|t| iter.has_tag(t)) {
                continue;
            }
            if below {
                marks.below_total = plan.groups[block.group].1;
            }
            marks.blocks.push(GutterBlock {
                below,
                offset: block.offset,
                header: plan.header,
                deleted: block.deleted,
                has_old: block.has_old,
                touched: block.touched,
            });
        }
        marks
    }

    fn queue_relayout(&self) {
        if self.relayout_queued.replace(true) {
            return;
        }
        let weak = self.view.clone();
        glib::idle_add_local_once(move || {
            if let Some(view) = weak.upgrade()
                && let Some(review) = view.review_state()
            {
                review.relayout();
            }
        });
    }

    /// Places the bars in the bands of the hunks nearest the visible area (run from an idle
    /// callback, when the layout is valid).
    fn relayout(&self) {
        self.relayout_queued.set(false);
        let (Some(view), Some(doc)) = (self.view.upgrade(), self.doc.upgrade()) else {
            return;
        };
        if self.plan.borrow().wrap != wrap_width(&view) && !self.plan.borrow().blocks.is_empty() {
            // The text width changed: deleted text wraps differently.
            self.sync();
            return;
        }
        let plan = self.plan.borrow();
        let vis = view.visible_rect();
        let mut wanted: Vec<(i32, usize, i32)> = Vec::new();
        if vis.height() > 0 {
            let margin = vis.height();
            let tops = self.group_tops(
                &view,
                &doc,
                &plan,
                vis.y() - margin,
                vis.y() + vis.height() + margin,
            );
            let center = vis.y() + vis.height() / 2;
            for (i, block) in plan.blocks.iter().enumerate() {
                let Some(top) = tops[block.group] else {
                    continue;
                };
                let y = top + block.offset;
                if y + plan.header < vis.y() - margin || y > vis.y() + vis.height() + margin {
                    continue;
                }
                let visible = y + plan.header > vis.y() && y < vis.y() + vis.height();
                let distance = if visible { 0 } else { (y - center).abs() };
                wanted.push((distance, i, y));
            }
        }
        wanted.sort_by_key(|&(distance, _, y)| (distance, y));
        wanted.truncate(BAR_POOL);
        // Bars keep the hunk they show when it is still wanted (no flicker while scrolling).
        let mut assigned: Vec<Option<(usize, i32)>> = vec![None; BAR_POOL];
        let mut rest = Vec::new();
        for &(_, i, y) in &wanted {
            let id = plan.blocks[i].id;
            match self.bars.iter().position(|b| b.hunk.get() == Some(id)) {
                Some(bar) if assigned[bar].is_none() => assigned[bar] = Some((i, y)),
                _ => rest.push((i, y)),
            }
        }
        for item in rest {
            if let Some(slot) = assigned.iter_mut().find(|s| s.is_none()) {
                *slot = Some(item);
            }
        }
        for (bar, slot) in self.bars.iter().zip(assigned) {
            let Some((i, y)) = slot else {
                bar.hunk.set(None);
                bar.at.set(None);
                bar.root.set_visible(false);
                continue;
            };
            let block = &plan.blocks[i];
            bar.show(block);
            let (w, h) = bar.size();
            let right = vis.x() + vis.width() - view.right_margin();
            let x = (right - w - BAR_RIGHT_MARGIN).max(vis.x());
            let y = y + (plan.header - h) / 2;
            bar.at.set(Some((x, y)));
            view.move_overlay(&bar.root, x, y);
        }
    }
}

/// What the gutter draws for one line (see [`ViewReview::gutter_marks`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct GutterMarks {
    /// The line is part of a hunk's current text.
    pub added: bool,
    /// That hunk was edited by the user.
    pub touched: bool,
    /// Reserved space at this line.
    pub blocks: Vec<GutterBlock>,
    /// Total space reserved below the line's text.
    pub below_total: i32,
}

/// One hunk's reserved space at a line (see [`GutterMarks`]).
#[derive(Clone, Debug)]
pub(crate) struct GutterBlock {
    /// Below the line's text (a deletion at the end of the file) rather than above it.
    pub below: bool,
    pub offset: i32,
    pub header: i32,
    pub deleted: i32,
    pub has_old: bool,
    pub touched: bool,
}

impl ReviewBar {
    fn new(review: &Weak<ViewReview>, index: usize) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .css_classes(["lemma-review-bar"])
            .visible(false)
            .focusable(false)
            .build();
        let (accept, accept_key) = bar_button("Accept", "accept");
        let (reject, reject_key) = bar_button("Reject", "reject");
        let position = gtk::Label::builder()
            .css_classes(["position", "numeric"])
            .build();
        for (button, decision) in [(&accept, Decision::Accept), (&reject, Decision::Reject)] {
            let review = review.clone();
            button.connect_clicked(move |_| {
                if let Some(review) = review.upgrade()
                    && let Some(id) = review.bars[index].hunk.get()
                {
                    (review.decide)(id, decision);
                }
            });
        }
        root.append(&accept);
        root.append(&reject);
        root.append(&position);
        let bar = Self {
            root,
            accept,
            reject,
            accept_key,
            reject_key,
            position,
            hunk: Cell::new(None),
            at: Cell::new(None),
        };
        bar.set_shortcuts(
            review_ui::default_shortcut_label("win.review-accept").as_deref(),
            review_ui::default_shortcut_label("win.review-reject").as_deref(),
        );
        bar
    }

    /// Shows the shortcuts of Accept and Reject (`None`: the action has no shortcut).
    fn set_shortcuts(&self, accept: Option<&str>, reject: Option<&str>) {
        for (button, key, shortcut, what) in [
            (&self.accept, &self.accept_key, accept, "Accept"),
            (&self.reject, &self.reject_key, reject, "Reject"),
        ] {
            key.set_label(shortcut.unwrap_or(""));
            key.set_visible(shortcut.is_some());
            let tooltip = match shortcut {
                Some(s) => format!("{what} this change ({s})"),
                None => format!("{what} this change"),
            };
            button.set_tooltip_text(Some(&tooltip));
        }
    }

    fn show(&self, block: &Block) {
        self.hunk.set(Some(block.id));
        self.position
            .set_text(&format!("{}/{}", block.index + 1, block.count));
        self.root.set_visible(true);
    }

    /// Natural width and height.
    fn size(&self) -> (i32, i32) {
        let (_, width, _, _) = self.root.measure(gtk::Orientation::Horizontal, -1);
        let (_, height, _, _) = self.root.measure(gtk::Orientation::Vertical, width);
        (width, height)
    }

    /// Natural height, also while the bar is hidden (GTK measures hidden widgets as 0, so the
    /// bar is shown for the measurement).
    fn measured_height(&self) -> i32 {
        let hidden = !self.root.is_visible();
        if hidden {
            self.root.set_visible(true);
        }
        let (_, height) = self.size();
        if hidden {
            self.root.set_visible(false);
        }
        height
    }
}

/// A bar button and the label showing its shortcut.
fn bar_button(label: &str, class: &str) -> (gtk::Button, gtk::Label) {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    content.append(&gtk::Label::new(Some(label)));
    let key = gtk::Label::builder().css_classes(["key"]).build();
    content.append(&key);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes([class])
        .focus_on_click(false)
        .focusable(false)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    (button, key)
}

/// Paints the review overlay of `view`'s document (phantom deleted lines, the outline of the hunk
/// under the cursor) above the text. Called from `TexView::snapshot_layer(AboveText)`; the
/// snapshot is in buffer coordinates. `view.document()` gives the document and its review.
pub fn paint(view: &TexView, snapshot: &gtk::Snapshot) {
    if let Some(review) = view.review_state() {
        review.paint(view, snapshot);
    }
}

/// The anchor of `hunk` in the current text (see [`Anchor`]).
fn anchor_of(hunk: &Hunk, index: &LineIndex) -> Anchor {
    let last = index.line_count() - 1;
    let line = hunk.cur_lines.start.min(last);
    let has_text = line < last || !index.line_range(line).is_empty();
    if has_text {
        Anchor::Above(line)
    } else if line > 0 {
        Anchor::Below(line - 1)
    } else {
        Anchor::Top
    }
}

/// The key of deleted text `old_text` laid out at `wrap` in a light or dark style in
/// [`ViewReview::phantoms`]: equal texts share it.
fn phantom_key(old_text: &str, wrap: Option<i32>, dark: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (old_text, wrap, dark).hash(&mut hasher);
    hasher.finish()
}

/// `ranges` with those that touch or overlap made one, in text order; empty ones left out.
fn joined(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let mut sorted: Vec<Range<usize>> = ranges.filter(|r| r.start < r.end).collect();
    sorted.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

/// True when ranges of `tag` start and end where each of `ranges` (sorted, apart) does. With as
/// many ranges as `tag` has, those are exactly its ranges: their starts and ends are all
/// toggles of the tag.
fn ranges_in_place(doc: &Document, tag: &gtk::TextTag, ranges: &[Range<usize>]) -> bool {
    ranges.iter().all(|range| {
        doc.iter_at_byte(range.start)
            .is_some_and(|start| start.starts_tag(Some(tag)))
            && doc
                .iter_at_byte(range.end)
                .is_some_and(|end| end.ends_tag(Some(tag)))
    })
}

/// The first position where `tag` applies.
fn tag_start(buffer: &sourceview5::Buffer, tag: &gtk::TextTag) -> Option<gtk::TextIter> {
    let mut iter = buffer.start_iter();
    (iter.starts_tag(Some(tag)) || iter.forward_to_tag_toggle(Some(tag))).then_some(iter)
}

/// Byte offset where the line before `line` ends: the start of `line`, or the end of the text
/// past the last line.
fn line_end(index: &LineIndex, line: u32) -> usize {
    if line < index.line_count() {
        index.line_start(line)
    } else {
        index.text_len()
    }
}

/// Height of one line of the view's font.
fn line_height(view: &TexView) -> i32 {
    view.create_pango_layout(Some("Xg")).pixel_size().1.max(1)
}

/// x of the start of a line's text (left margin plus CSS padding), in buffer coordinates.
fn text_origin(view: &TexView) -> i32 {
    view.iter_location(&view.buffer().start_iter()).x()
}

/// Width the view wraps text at, or `None` without wrapping (or before the first allocation).
fn wrap_width(view: &TexView) -> Option<i32> {
    if view.wrap_mode() == gtk::WrapMode::None {
        return None;
    }
    let left = text_origin(view);
    let right = view.right_margin() + (left - view.left_margin()).max(0);
    let width = view.visible_rect().width() - left - right - 1;
    (width > 20).then_some(width)
}

/// The deleted words of `hunk`: line of `old_text` and byte range within it.
fn old_words(hunk: &Hunk) -> Vec<(u32, Range<u32>)> {
    hunk.words
        .iter()
        .filter(|w| w.side == Side::Old)
        .map(|w| (w.line, w.bytes.clone()))
        .collect()
}

/// The lines of deleted text as drawn: without their line ends (`"\n"` or `"\r\n"`, which
/// Pango would show as an extra empty line), each with its line number in `old_text`; `None` for
/// the line that stands for the lines left out of a long text.
fn phantom_lines(old_text: &str) -> Vec<(Option<usize>, Cow<'_, str>)> {
    let lines: Vec<&str> = old_text
        .split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l);
            l.strip_suffix('\r').unwrap_or(l)
        })
        .collect();
    if lines.len() <= PHANTOM_MAX_LINES {
        return lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| (Some(i), Cow::Borrowed(l)))
            .collect();
    }
    let left_out = lines.len() - PHANTOM_HEAD - PHANTOM_TAIL;
    let skip = lines.len() - PHANTOM_TAIL;
    let mut out: Vec<(Option<usize>, Cow<'_, str>)> = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        if i == PHANTOM_HEAD {
            out.push((
                None,
                Cow::Owned(format!("⋯ {left_out} more deleted lines ⋯")),
            ));
        }
        if i < PHANTOM_HEAD || i >= skip {
            out.push((Some(i), Cow::Borrowed(line)));
        }
    }
    out
}

/// The layouts of deleted text (see [`Phantom::chunks`]): the view's font and tab stops, wrapped
/// like the view, deleted words with a darker background, and the scanner's syntax colours when
/// `syntax` is given.
fn phantom_chunks(
    view: &TexView,
    old_text: &str,
    words: &[(u32, Range<u32>)],
    wrap: Option<i32>,
    dark: bool,
    syntax: Option<&highlight::TextSyntax>,
) -> Vec<(i32, i32, pango::Layout)> {
    let lines = phantom_lines(old_text);
    let tabs = view.tabs();
    let (r, g, b, a) = if dark {
        (0xff, 0x5a, 0x4f, 0.42)
    } else {
        (0xe0, 0x1b, 0x24, 0.30)
    };
    let mut chunks = Vec::with_capacity(lines.len().div_ceil(PHANTOM_CHUNK_LINES));
    let mut y = 0;
    for chunk in lines.chunks(PHANTOM_CHUNK_LINES) {
        let mut text = String::new();
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrInt::new_insert_hyphens(false));
        for (k, (number, line)) in chunk.iter().enumerate() {
            if k > 0 {
                text.push('\n');
            }
            let base = text.len();
            text.push_str(line);
            let Some(number) = number else {
                let mut style = pango::AttrInt::new_style(pango::Style::Italic);
                style.set_start_index(base as u32);
                style.set_end_index(text.len() as u32);
                attrs.insert(style);
                continue;
            };
            if let Some(syntax) = syntax {
                syntax.insert_attrs(&attrs, *number as u32, line, base as u32);
            }
            for (_, bytes) in words.iter().filter(|(l, _)| *l as usize == *number) {
                let start = base + (bytes.start as usize).min(line.len());
                let end = base + (bytes.end as usize).min(line.len());
                if start >= end {
                    continue;
                }
                let mut background = pango::AttrColor::new_background(r * 257, g * 257, b * 257);
                background.set_start_index(start as u32);
                background.set_end_index(end as u32);
                attrs.insert(background);
                let mut alpha = pango::AttrInt::new_background_alpha((a * 65535.0) as u16);
                alpha.set_start_index(start as u32);
                alpha.set_end_index(end as u32);
                attrs.insert(alpha);
            }
        }
        let layout = view.create_pango_layout(Some(&text));
        if let Some(tabs) = &tabs {
            layout.set_tabs(Some(tabs));
        }
        if let Some(width) = wrap {
            layout.set_width(width * pango::SCALE);
            layout.set_wrap(pango::WrapMode::WordChar);
        }
        layout.set_attributes(Some(&attrs));
        let height = layout.pixel_size().1;
        chunks.push((y, height, layout));
        y += height;
    }
    chunks
}

/// "−3 lines" in a smaller italic version of the view's font.
fn hidden_label(view: &TexView, lines: u32) -> pango::Layout {
    let text = match lines {
        1 => "−1 line hidden".to_owned(),
        n => format!("−{n} lines hidden"),
    };
    let layout = view.create_pango_layout(Some(&text));
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrFloat::new_scale(0.8));
    attrs.insert(pango::AttrInt::new_style(pango::Style::Italic));
    layout.set_attributes(Some(&attrs));
    layout
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> graphene::Rect {
    graphene::Rect::new(x, y, width, height)
}

/// Review colours for the view's style scheme.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Palette {
    /// The scheme's text background (opaque).
    pub base: gdk::RGBA,
    pub dark: bool,
    pub band: gdk::RGBA,
    pub rule: gdk::RGBA,
    pub deleted_bg: gdk::RGBA,
    pub deleted_fg: gdk::RGBA,
    pub deleted_mark: gdk::RGBA,
    pub added: gdk::RGBA,
    pub added_word: gdk::RGBA,
    pub added_mark: gdk::RGBA,
    pub touched: gdk::RGBA,
    pub outline: gdk::RGBA,
}

impl Palette {
    pub(crate) fn new(view: Option<&TexView>, buffer: &sourceview5::Buffer) -> Self {
        let text = buffer.style_scheme().and_then(|s| s.style("text"));
        let color = |value: Option<glib::GString>| value.and_then(|v| gdk::RGBA::parse(&v).ok());
        let base = text
            .as_ref()
            .filter(|s| s.is_background_set())
            .and_then(|s| color(s.background()));
        let dark = base.map_or_else(
            || adw::StyleManager::default().is_dark(),
            |b| luminance(&b) < 0.5,
        );
        let base = base.unwrap_or(if dark {
            gdk::RGBA::new(0.12, 0.12, 0.12, 1.0)
        } else {
            gdk::RGBA::WHITE
        });
        let fg = text
            .as_ref()
            .filter(|s| s.is_foreground_set())
            .and_then(|s| color(s.foreground()))
            .or_else(|| view.map(|v| v.color()))
            .unwrap_or(if dark {
                gdk::RGBA::WHITE
            } else {
                gdk::RGBA::BLACK
            });
        let rgba = |r: u8, g: u8, b: u8, a: f32| {
            gdk::RGBA::new(
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                a,
            )
        };
        let with_alpha = |c: gdk::RGBA, a: f32| gdk::RGBA::new(c.red(), c.green(), c.blue(), a);
        let accent = adw::StyleManager::default().accent_color_rgba();
        if dark {
            Self {
                base,
                dark,
                band: with_alpha(fg, 0.05),
                rule: with_alpha(fg, 0.08),
                deleted_bg: rgba(0xf6, 0x61, 0x51, 0.17),
                deleted_fg: rgba(0xff, 0x9e, 0x94, 1.0),
                deleted_mark: rgba(0xf6, 0x61, 0x51, 1.0),
                added: rgba(0x57, 0xe3, 0x89, 0.14),
                added_word: rgba(0x57, 0xe3, 0x89, 0.30),
                added_mark: rgba(0x57, 0xe3, 0x89, 1.0),
                touched: rgba(0xf8, 0xc8, 0x3c, 1.0),
                outline: accent,
            }
        } else {
            Self {
                base,
                dark,
                band: with_alpha(fg, 0.04),
                rule: with_alpha(fg, 0.08),
                deleted_bg: rgba(0xe0, 0x1b, 0x24, 0.12),
                deleted_fg: rgba(0xa5, 0x1d, 0x2d, 1.0),
                deleted_mark: rgba(0xe0, 0x1b, 0x24, 1.0),
                added: rgba(0x2e, 0xc2, 0x7e, 0.18),
                added_word: rgba(0x2e, 0xc2, 0x7e, 0.35),
                added_mark: rgba(0x26, 0xa2, 0x69, 1.0),
                touched: rgba(0xe5, 0xa5, 0x0a, 1.0),
                outline: accent,
            }
        }
    }
}

fn luminance(c: &gdk::RGBA) -> f32 {
    0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue()
}

/// Styles of the action bars and the header summary; installed once per display.
pub fn install_css(display: &gdk::Display) {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(REVIEW_CSS);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

const REVIEW_CSS: &str = "
.lemma-review-bar { font-family: initial; font-size: 0.8rem; }
.lemma-review-bar > button { min-height: 0; min-width: 0; padding: 1px 8px; margin: 0;
                             border-radius: 5px; font-weight: 600; box-shadow: none; }
.lemma-review-bar > button.accept { background-color: alpha(@success_color, 0.16);
                                    color: @success_color; }
.lemma-review-bar > button.accept:hover { background-color: alpha(@success_color, 0.28); }
.lemma-review-bar > button.reject { background-color: alpha(@error_color, 0.13);
                                    color: @error_color; }
.lemma-review-bar > button.reject:hover { background-color: alpha(@error_color, 0.25); }
.lemma-review-bar .key { font-weight: normal; opacity: 0.75; }
.lemma-review-bar .position { opacity: 0.6; margin: 0 2px 0 4px; }
";

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[(Option<usize>, Cow<'_, str>)]) -> Vec<String> {
        lines.iter().map(|(_, l)| l.to_string()).collect()
    }

    #[test]
    fn phantom_lines_drop_line_ends() {
        let lines = phantom_lines("a\r\nb\nc");
        assert_eq!(texts(&lines), ["a", "b", "c"]);
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2)]
        );
        // A lone '\r' inside a line is content.
        assert_eq!(texts(&phantom_lines("a\rb\n")), ["a\rb"]);
        assert_eq!(texts(&phantom_lines("\n\n")), ["", ""]);
    }

    #[test]
    fn long_phantoms_are_shortened() {
        let old: String = (0..PHANTOM_MAX_LINES + 100)
            .map(|i| format!("l{i}\n"))
            .collect();
        let lines = phantom_lines(&old);
        assert_eq!(lines.len(), PHANTOM_HEAD + 1 + PHANTOM_TAIL);
        assert_eq!(
            lines[PHANTOM_HEAD - 1],
            (Some(PHANTOM_HEAD - 1), "l299".into())
        );
        let (number, marker) = &lines[PHANTOM_HEAD];
        assert_eq!(*number, None);
        assert_eq!(marker, "⋯ 200 more deleted lines ⋯");
        let last = PHANTOM_MAX_LINES + 99;
        assert_eq!(
            lines.last().unwrap(),
            &(Some(last), format!("l{last}").into())
        );
        // At the limit nothing is left out.
        let old: String = (0..PHANTOM_MAX_LINES).map(|i| format!("l{i}\n")).collect();
        assert_eq!(phantom_lines(&old).len(), PHANTOM_MAX_LINES);
    }
}
