//! Inline review rendering (WP1.2, ARCHITECTURE §4.3): the anchor tags that reserve space, the
//! cached layouts of deleted lines, painting, and the pool of 8 `ReviewBar` overlays.
//!
//! The buffer always holds the agent's proposal. For every hunk of the document's review:
//!
//! - its current (added) lines carry the shared `lemma-review-added` tag, a green paragraph
//!   background, and its changed words `lemma-review-added-word`, a stronger green;
//! - its **anchor line** (the first current line; for a pure deletion the line after it) carries
//!   an anchor tag whose `pixels-above-lines` reserves a header band plus the height of the
//!   deleted text. A deletion at the end of the file, where no line follows, reserves the space
//!   with `pixels-below-lines` on the last line with text instead. Anchors that reserve the same
//!   space share one tag ([`SpaceTag`]): one-line hunks need a few heights, so thousands of
//!   hunks make tens of tags, where a tag per hunk slowed every tag operation of the buffer;
//! - [`paint`] (called from `TexView::snapshot_layer(AboveText)`, in buffer coordinates) fills
//!   the reserved space: an opaque base, the band, then the deleted text in red with its deleted
//!   words darker, laid out like the view's text (font, tabs, wrapping to the text width);
//! - one of [`BAR_POOL`] action bars (Accept Alt+Y · Reject Alt+N · i/n, with the shortcuts
//!   actually bound) is placed in the band with `TextView::add_overlay`, so it never covers text.
//!
//! The plan (which hunk reserves what where) is rebuilt after every recompute. Tags change only
//! where the plan or the text did: each tag's ranges from the last apply are followed through
//! the edits since ([`Edits`]), and only the bytes that should be tagged and are not, or the
//! other way round, are tagged or untagged. Reading mode (Alt+D) shrinks each band to its header
//! and hides the deleted text.

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
/// Separate places of the text [`Edits`] follows; past them the next apply reads the tags'
/// ranges from the text (a replace_all, a Reject all).
const MAX_EDIT_SPANS: usize = 128;

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
    /// Index of its anchor in the plan (blocks sharing an anchor stack there).
    pub group: usize,
}

/// One anchor of the plan: where blocks reserve space, how much in all, and which blocks.
#[derive(Clone, Debug, PartialEq)]
struct Group {
    anchor: Anchor,
    space: i32,
    /// Indices of its blocks in [`Plan::blocks`].
    blocks: Range<usize>,
}

impl Group {
    /// The anchor line (`None` for [`Anchor::Top`]).
    fn line(&self) -> Option<u32> {
        match self.anchor {
            Anchor::Above(line) | Anchor::Below(line) => Some(line),
            Anchor::Top => None,
        }
    }
}

/// Everything derived from the hunks that the tags and the painting use.
#[derive(Clone, Debug, Default, PartialEq)]
struct Plan {
    blocks: Vec<Block>,
    /// The anchors, in text order (index = `Block::group`).
    groups: Vec<Group>,
    /// Line ranges with the added-line background.
    added: Vec<Range<u32>>,
    /// Changed words: line and byte range within the line.
    words: Vec<(u32, Range<u32>)>,
    header: i32,
    /// Width the deleted text wraps at (`None`: no wrapping).
    wrap: Option<i32>,
}

/// An anchor tag: it reserves `space` pixels above the lines it covers, or below them
/// (`below`). Anchors that reserve the same space share it, so its properties never change.
struct SpaceTag {
    tag: gtk::TextTag,
    below: bool,
    space: i32,
    /// The ranges the last [`ViewReview::apply`] gave it (see [`ViewReview::added_ranges`]).
    ranges: Vec<Range<usize>>,
}

/// The changes to the text since the last [`ViewReview::apply`]: the spans of the current text
/// they made (spans that meet are one), each with the length it had before them.
///
/// GTK moves a tag's ranges with the text, and text inserted where a range starts or ends goes
/// outside it, so a range no span meets or touches is where it was, shifted by the spans before
/// it (see [`follow`]); only the others are read from the text again.
#[derive(Debug, Default)]
struct Edits {
    spans: Vec<Changed>,
    /// More than [`MAX_EDIT_SPANS`] places changed and the spans were dropped: the tags' ranges
    /// are read from the whole text.
    lost: bool,
}

/// One span of [`Edits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Changed {
    /// Its byte offset and length in the current text.
    start: usize,
    len: usize,
    /// Its length before the changes.
    old_len: usize,
}

impl Changed {
    fn end(&self) -> usize {
        self.start + self.len
    }
}

impl Edits {
    /// Nothing changed since the last apply.
    fn is_empty(&self) -> bool {
        self.spans.is_empty() && !self.lost
    }

    /// Records the change of the bytes `range` (offsets of the text before it) to `inserted`
    /// bytes.
    fn record(&mut self, range: Range<usize>, inserted: usize) {
        if self.lost || (range.is_empty() && inserted == 0) {
            return;
        }
        // The spans the change meets or touches become one with it.
        let first = self.spans.partition_point(|s| s.end() < range.start);
        let met = self.spans[first..]
            .iter()
            .take_while(|s| s.start <= range.end)
            .count();
        let (mut start, mut end, mut grown) = (range.start, range.end, 0);
        for s in &self.spans[first..first + met] {
            start = start.min(s.start);
            end = end.max(s.end());
            grown += signed(s.len) - signed(s.old_len);
        }
        let delta = signed(inserted) - signed(range.len());
        let merged = Changed {
            start,
            len: shifted(end - start, delta),
            old_len: shifted(end - start, -grown),
        };
        self.spans.splice(first..first + met, [merged]);
        for s in &mut self.spans[first + 1..] {
            s.start = shifted(s.start, delta);
        }
        if self.spans.len() > MAX_EDIT_SPANS {
            self.spans = Vec::new();
            self.lost = true;
        }
    }
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
    /// The byte ranges the last [`ViewReview::apply`] gave `added` and `word`, in the text as it
    /// was then (sorted, apart): with [`Self::edits`] they tell where the tags are now.
    added_ranges: RefCell<Vec<Range<usize>>>,
    word_ranges: RefCell<Vec<Range<usize>>>,
    /// The anchor tags, in priority order (the last is the highest).
    anchors: RefCell<Vec<SpaceTag>>,
    /// The changes to the text since the last apply.
    edits: RefCell<Edits>,
    plan: RefCell<Plan>,
    /// The block of each hunk of the plan, by id, for frames drawn after the text changed (see
    /// [`ViewReview::groups_on`]); made the first time one needs it.
    block_of: RefCell<Option<HashMap<HunkId, usize>>>,
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
    /// the text it copies); the next apply reads their ranges from the whole text. Otherwise
    /// GTK's edits move the ranges with the text and never add one, so the ranges of the last
    /// apply and the edits since tell where they are, which keeps a sync with thousands of
    /// hunks linear and its tag changes to what changed: each search or change of a tag costs
    /// GTK time, and a tag changed over text makes GTK draw or lay out that text again.
    tags_unsure: Cell<bool>,
    /// Document version the plan was last checked against.
    plan_version: Cell<Option<u64>>,
    /// The view's own top margin, restored when no hunk needs the space above line 1.
    base_top_margin: i32,
    decide: DecideFn,
    observers: RefCell<Vec<ObserverId>>,
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
            added_ranges: RefCell::new(Vec::new()),
            word_ranges: RefCell::new(Vec::new()),
            anchors: RefCell::new(Vec::new()),
            edits: RefCell::new(Edits::default()),
            plan: RefCell::new(Plan::default()),
            block_of: RefCell::new(None),
            phantoms: RefCell::new(HashMap::new()),
            phantom_of: RefCell::new(HashMap::new()),
            bar_height: Cell::new(None),
            bars: (0..BAR_POOL).map(|i| ReviewBar::new(weak, i)).collect(),
            reading: Cell::new(false),
            cursor_hunk: Cell::new(None),
            relayout_queued: Cell::new(false),
            applying: Cell::new(false),
            tags_unsure: Cell::new(true),
            plan_version: Cell::new(None),
            base_top_margin: view.top_margin(),
            decide,
            observers: RefCell::new(Vec::new()),
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
        let hunks = doc.connect_hunks_changed(move || {
            if let Some(review) = weak.upgrade() {
                review.sync();
            }
        });
        let weak = Rc::downgrade(self);
        let edits = doc.connect_edit(move |_, edit, _| {
            if let Some(review) = weak.upgrade() {
                review
                    .edits
                    .borrow_mut()
                    .record(edit.range.clone(), edit.text.len());
            }
        });
        self.observers.replace(vec![hunks, edits]);
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
        let observers = self.observers.take();
        if let Some(doc) = self.doc.upgrade() {
            for id in observers {
                doc.disconnect(id);
            }
        }
        for (object, id) in self.handlers.borrow_mut().drain(..) {
            object.disconnect(id);
        }
        remove_tag_everywhere(&self.buffer, &self.added);
        remove_tag_everywhere(&self.buffer, &self.word);
        // The highest priority first: the table renumbers only the tags above the one it loses.
        for anchor in self.anchors.take().into_iter().rev() {
            remove_spacing_tag(&self.buffer, &anchor.tag);
        }
        self.tags_unsure.set(true);
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

    /// The bands near the visible area as the next frame draws them: each block's hunk and the
    /// top of its band (buffer coordinates), in text order (tests).
    pub fn band_tops(&self) -> Vec<(HunkId, i32)> {
        let (Some(view), Some(doc)) = (self.view.upgrade(), self.doc.upgrade()) else {
            return Vec::new();
        };
        let plan = self.plan.borrow();
        let vis = view.visible_rect();
        self.tops(&view, &doc, &plan, vis.y(), vis.y() + vis.height())
            .into_iter()
            .flat_map(|(g, top)| {
                plan.blocks[plan.groups[g].blocks.clone()]
                    .iter()
                    .map(move |b| (b.id, top + b.offset))
            })
            .collect()
    }

    /// The hunks whose bands the gutter marks next to line `line` of the current text (in
    /// `LineIndex` numbering), in order (tests).
    pub fn hunks_at_line(&self, line: u32) -> Vec<HunkId> {
        let Some(doc) = self.doc.upgrade() else {
            return Vec::new();
        };
        let plan = self.plan.borrow();
        self.groups_on(&doc, &plan, line..line + 1)
            .into_iter()
            .flat_map(|(g, _)| {
                plan.blocks[plan.groups[g].blocks.clone()]
                    .iter()
                    .map(|b| b.id)
            })
            .collect()
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
        // The tags are as the plan wants them unless it or the text changed since the last apply.
        let changed = self.tags_unsure.get()
            || !self.edits.borrow().is_empty()
            || *self.plan.borrow() != plan;
        if changed {
            self.apply(&view, &doc, &plan);
            self.plan.replace(plan);
            self.block_of.replace(None);
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
                Some(group) if group.anchor == anchor => {
                    let offset = group.space;
                    group.space += space;
                    group.blocks.end = i + 1;
                    (plan.groups.len() - 1, offset)
                }
                _ => {
                    plan.groups.push(Group {
                        anchor,
                        space,
                        blocks: i..i + 1,
                    });
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

    /// Re-tags the buffer for `plan`, where it differs from what the tags cover now: the ranges
    /// of the last apply, followed through the edits since ([`Edits`]), or read from the text
    /// when that cannot tell ([`Self::tags_unsure`], too many edits). A decision thus changes
    /// the tags of the decided hunk's lines only, and typing those of the lines typed on.
    ///
    /// Anchors share a tag per space they reserve ([`SpaceTag`]); a space no anchor reserves any
    /// more takes its tag out of the tag table, so its cost does not outlive its hunks.
    fn apply(&self, view: &TexView, doc: &Document, plan: &Plan) {
        self.applying.set(true);
        let index = doc.line_index();
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
        // The anchor lines by the space they reserve, above or below them.
        let mut top = 0;
        let mut lines: Vec<((bool, i32), Range<usize>)> = Vec::new();
        for group in &plan.groups {
            match group.anchor {
                Anchor::Above(line) | Anchor::Below(line) if line < line_count => {
                    let below = matches!(group.anchor, Anchor::Below(_));
                    let range = index.line_start(line)..line_end(&index, line + 1);
                    lines.push(((below, group.space), range));
                }
                Anchor::Top => top += group.space,
                _ => {}
            }
        }
        drop(index);
        // Stable: each space's lines stay in text order.
        lines.sort_by_key(|&(space, _)| space);
        let edits = std::mem::take(&mut *self.edits.borrow_mut());
        let edits = (!self.tags_unsure.get() && !edits.lost).then_some(&edits.spans[..]);
        // Backgrounds: they change no sizes.
        let mut exact = self.retag(
            doc,
            &self.added,
            false,
            &mut self.added_ranges.borrow_mut(),
            added,
            edits,
        );
        exact &= self.retag(
            doc,
            &self.word,
            false,
            &mut self.word_ranges.borrow_mut(),
            words,
            edits,
        );
        let mut anchors = self.anchors.borrow_mut();
        let table = self.buffer.tag_table();
        for chunk in lines.chunk_by(|a, b| a.0 == b.0) {
            let (below, space) = chunk[0].0;
            if !anchors.iter().any(|a| a.below == below && a.space == space) {
                // Highest priority: nothing else may override the reserved space.
                let tag = gtk::TextTag::new(None);
                if below {
                    tag.set_pixels_below_lines(space);
                } else {
                    tag.set_pixels_above_lines(space);
                }
                table.add(&tag);
                tag.set_priority(table.size() - 1);
                anchors.push(SpaceTag {
                    tag,
                    below,
                    space,
                    ranges: Vec::new(),
                });
            }
        }
        for anchor in anchors.iter_mut() {
            let first = lines.partition_point(|&(space, _)| space < (anchor.below, anchor.space));
            let want = lines[first..]
                .iter()
                .take_while(|&&(space, _)| space == (anchor.below, anchor.space))
                .map(|(_, range)| range.clone());
            let want = joined(want);
            exact &= self.retag(doc, &anchor.tag, true, &mut anchor.ranges, want, edits);
        }
        // Spaces no anchor reserves now: the highest priority first, as the table renumbers
        // the tags above the one it loses.
        for i in (0..anchors.len()).rev() {
            if anchors[i].ranges.is_empty() {
                remove_spacing_tag(&self.buffer, &anchors.remove(i).tag);
            }
        }
        // A tag added to the table since outranks ours: they go back to the top.
        let size = table.size();
        let ours = i32::try_from(anchors.len()).unwrap_or(i32::MAX);
        if anchors.iter().any(|a| a.tag.priority() < size - ours) {
            for anchor in anchors.iter() {
                anchor.tag.set_priority(size - 1);
            }
        }
        drop(anchors);
        view.set_top_margin(self.base_top_margin + top);
        self.applying.set(false);
        // A range that could not be tagged (no iter at its offsets) is not where its record
        // says: the next apply reads the tags from the text.
        self.tags_unsure.set(!exact);
    }

    /// Makes `tag` cover exactly `want` (sorted, apart), and records it in `applied`, which holds
    /// the ranges the last apply gave the tag. With `edits` (the spans of [`Edits`]) it covers
    /// those ranges moved with the text, except near an edit, where its ranges are read from
    /// the text; without, all its ranges are read. Only the difference is tagged or untagged:
    /// GTK draws or lays out again whatever text a tag changes over. False when a range could
    /// not be changed.
    ///
    /// A tag that changes no sizes (`sizes` false) is taken off in one go across text it should
    /// not cover: GTK then only draws that text again, and Accept all takes the added lines'
    /// background off thousands of hunks at once. One that does would have that text laid out
    /// again.
    fn retag(
        &self,
        doc: &Document,
        tag: &gtk::TextTag,
        sizes: bool,
        applied: &mut Vec<Range<usize>>,
        want: Vec<Range<usize>>,
        edits: Option<&[Changed]>,
    ) -> bool {
        let have = match edits {
            Some(spans) => {
                let (mut have, unsure) = follow(applied, spans);
                if !unsure.is_empty() {
                    for region in unsure {
                        have.extend(tag_ranges(doc, tag, region));
                    }
                    have = joined(have.into_iter());
                }
                have
            }
            None => tag_ranges(doc, tag, 0..doc.line_index().text_len()),
        };
        let mut exact = true;
        let at = |range: &Range<usize>| {
            Some((doc.iter_at_byte(range.start)?, doc.iter_at_byte(range.end)?))
        };
        let mut untag = difference(&have, &want);
        if !sizes {
            untag = joined_outside(untag, &want);
        }
        for range in untag {
            match at(&range) {
                Some((start, end)) => self.buffer.remove_tag(tag, &start, &end),
                None => exact = false,
            }
        }
        for range in difference(&want, &have) {
            match at(&range) {
                Some((start, end)) => self.buffer.apply_tag(tag, &start, &end),
                None => exact = false,
            }
        }
        *applied = want;
        exact
    }

    /// True for the review's own tags.
    fn is_own_tag(&self, tag: &gtk::TextTag) -> bool {
        // Anchor tags are the ones that reserve space.
        *tag == self.added
            || *tag == self.word
            || ((tag.is_pixels_above_lines_set() || tag.is_pixels_below_lines_set())
                && self.anchors.borrow().iter().any(|a| a.tag == *tag))
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

    /// The groups of `plan` anchored on `lines` of the current text, in text order, each with
    /// its line ([`Anchor::Top`] left out). While the plan matches the text these are its lines.
    /// After an edit (typing, until the recompute 50 ms later) they come from the review's
    /// hunks, which follow every edit, and the line must start with the group's tag: that is
    /// where its space is reserved.
    fn groups_on(&self, doc: &Document, plan: &Plan, lines: Range<u32>) -> Vec<(usize, u32)> {
        if self.plan_version.get() == Some(doc.version()) {
            let first = plan
                .groups
                .partition_point(|g| g.line().is_none_or(|l| l < lines.start));
            return plan.groups[first..]
                .iter()
                .map_while(|g| g.line().filter(|l| *l < lines.end))
                .enumerate()
                .map(|(k, line)| (first + k, line))
                .collect();
        }
        let review = doc.review.borrow();
        let Some(review) = review.as_ref() else {
            return Vec::new();
        };
        let mut block_of = self.block_of.borrow_mut();
        let block_of = block_of.get_or_insert_with(|| {
            plan.blocks
                .iter()
                .enumerate()
                .map(|(i, b)| (b.id, i))
                .collect()
        });
        let index = doc.line_index();
        let hunks = review.hunks();
        // A hunk's anchor is on one of its lines, the line after a pure deletion, or the line
        // before a deletion at the end.
        let first =
            hunks.partition_point(|h| h.cur_lines.end.max(h.cur_lines.start + 1) <= lines.start);
        let mut out: Vec<(usize, u32)> = Vec::new();
        for hunk in hunks[first..]
            .iter()
            .take_while(|h| h.cur_lines.start <= lines.end)
        {
            let Some(&block) = block_of.get(&hunk.id) else {
                continue;
            };
            let group = plan.blocks[block].group;
            if out.last().is_some_and(|&(g, _)| g == group) {
                continue;
            }
            let below = matches!(plan.groups[group].anchor, Anchor::Below(_));
            if let Some(line) = self.moved_anchor(doc, &index, &plan.groups[group], hunk)
                && lines.contains(&line)
                // A line reserves one space above it and one below. When an edit joined two
                // anchor lines whose groups share a tag, the line is the first group's.
                && !out.iter().any(|&(g, l)| {
                    l == line && matches!(plan.groups[g].anchor, Anchor::Below(_)) == below
                })
            {
                out.push((group, line));
            }
        }
        out
    }

    /// The line of the current text that starts with `group`'s tag, found from `hunk`, the hunk
    /// of one of its blocks as the review has it now: the hunk's anchor line, or a later line of
    /// the hunk when text typed at its start went in front of the tag (GTK leaves text inserted
    /// where a range starts outside it). `None` when neither starts with the tag.
    ///
    /// The line is where the group's kind of anchor puts it (see [`anchor_of`]), whatever the
    /// hunk would get now: typing after the last line gives a deletion at the end a line of its
    /// own, but its tag stays below the line before until the recompute.
    fn moved_anchor(
        &self,
        doc: &Document,
        index: &LineIndex,
        group: &Group,
        hunk: &Hunk,
    ) -> Option<u32> {
        let line = hunk.cur_lines.start.min(index.line_count() - 1);
        let (below, line) = match group.anchor {
            Anchor::Above(_) => (false, line),
            Anchor::Below(_) => (true, line.checked_sub(1)?),
            Anchor::Top => return None,
        };
        let anchors = self.anchors.borrow();
        let tag = &anchors
            .iter()
            .find(|a| a.below == below && a.space == group.space)?
            .tag;
        let mut iter = doc.iter_at_byte(index.line_start(line))?;
        if iter.has_tag(tag) {
            return Some(line);
        }
        if below || !iter.forward_to_tag_toggle(Some(tag)) || !iter.starts_tag(Some(tag)) {
            return None;
        }
        let offset = doc.byte_offset(&iter);
        let moved = index.line_of(offset);
        (moved < hunk.cur_lines.end && index.line_start(moved) == offset).then_some(moved)
    }

    /// Top of the space `group` reserves when its anchor is on `line` (buffer coordinates).
    fn group_top(&self, view: &TexView, doc: &Document, group: &Group, line: u32) -> Option<i32> {
        let offset = doc.line_index().line_start(line);
        let (y, height) = view.line_yrange(&doc.iter_at_byte(offset)?);
        Some(match group.anchor {
            Anchor::Below(_) => y + height - group.space,
            _ => y,
        })
    }

    /// The groups whose space may lie in the buffer-y range `y0..y1` (those anchored on the
    /// lines there, and one above the first line), with the top of their space, in text order.
    fn tops(
        &self,
        view: &TexView,
        doc: &Document,
        plan: &Plan,
        y0: i32,
        y1: i32,
    ) -> Vec<(usize, i32)> {
        let mut tops = Vec::new();
        if let Some(group) = plan.groups.first()
            && group.anchor == Anchor::Top
        {
            let (y, _) = view.line_yrange(&self.buffer.start_iter());
            tops.push((0, y - group.space));
        }
        let line_at = |y: i32| {
            let (iter, _) = view.line_at_y(y);
            doc.line_index().line_of(doc.byte_offset(&iter))
        };
        let lines = line_at(y0).saturating_sub(1)..line_at(y1) + 2;
        for (g, line) in self.groups_on(doc, plan, lines) {
            if let Some(top) = self.group_top(view, doc, &plan.groups[g], line) {
                tops.push((g, top));
            }
        }
        tops
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
        let tops = self.tops(view, &doc, &plan, vis.y(), vis.y() + vis.height());
        // As wide as the added lines' paragraph background (the text area without its right
        // margin).
        let (x, width) = (
            vis.x() as f32,
            (vis.width() - view.right_margin()).max(0) as f32,
        );
        let text_x = text_origin(view) as f32;
        let header = plan.header;
        let blocks = tops.iter().flat_map(|&(g, top)| {
            plan.blocks[plan.groups[g].blocks.clone()]
                .iter()
                .map(move |block| (block, top))
        });
        for (block, top) in blocks {
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
            && let Some(&(_, top)) = tops.iter().find(|(g, _)| *g == block.group)
        {
            let y0 = top + block.offset;
            // The hunk's lines now: the plan's until the text changes, then the review's.
            let lines = if self.plan_version.get() == Some(doc.version()) {
                block.lines.clone()
            } else {
                review
                    .hunk(id)
                    .map_or(block.lines.clone(), |h| h.cur_lines.clone())
            };
            let y1 = match lines.end.checked_sub(1) {
                Some(last) if !lines.is_empty() => {
                    let index = doc.line_index();
                    let last = last.min(index.line_count().saturating_sub(1));
                    let offset = index.line_start(last);
                    drop(index);
                    doc.iter_at_byte(offset).map_or(y0 + header, |iter| {
                        let (y, h) = view.line_yrange(&iter);
                        y + h - self.below_space_on(&doc, &plan, last)
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

    /// Space reserved below `line` of the current text by deletions at the end of the file.
    fn below_space_on(&self, doc: &Document, plan: &Plan, line: u32) -> i32 {
        self.groups_on(doc, plan, line..line + 1)
            .into_iter()
            .map(|(g, _)| &plan.groups[g])
            .filter(|group| matches!(group.anchor, Anchor::Below(_)))
            .map(|group| group.space)
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
        let Some(doc) = self.doc.upgrade().filter(|_| !plan.blocks.is_empty()) else {
            return marks;
        };
        marks.touched = if self.plan_version.get() == Some(doc.version()) {
            let first = plan.blocks.partition_point(|b| b.lines.end <= line);
            plan.blocks[first..]
                .iter()
                .take_while(|b| b.lines.start <= line)
                .any(|b| b.touched && b.lines.contains(&line))
        } else {
            // The plan's lines are stale until the recompute; the review's follow the text.
            doc.review.borrow().as_ref().is_some_and(|review| {
                review
                    .hunk_at_line(line)
                    .is_some_and(|h| h.user_touched && h.cur_lines.contains(&line))
            })
        };
        for (g, _) in self.groups_on(&doc, &plan, line..line + 1) {
            let group = &plan.groups[g];
            let below = matches!(group.anchor, Anchor::Below(_));
            if below {
                marks.below_total = group.space;
            }
            for block in &plan.blocks[group.blocks.clone()] {
                marks.blocks.push(GutterBlock {
                    below,
                    offset: block.offset,
                    header: plan.header,
                    deleted: block.deleted,
                    has_old: block.has_old,
                    touched: block.touched,
                });
            }
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
            let tops = self.tops(
                &view,
                &doc,
                &plan,
                vis.y() - margin,
                vis.y() + vis.height() + margin,
            );
            let center = vis.y() + vis.height() / 2;
            for (g, top) in tops {
                for i in plan.groups[g].blocks.clone() {
                    let y = top + plan.blocks[i].offset;
                    if y + plan.header < vis.y() - margin || y > vis.y() + vis.height() + margin {
                        continue;
                    }
                    let visible = y + plan.header > vis.y() && y < vis.y() + vis.height();
                    let distance = if visible { 0 } else { (y - center).abs() };
                    wanted.push((distance, i, y));
                }
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

/// Where the changes `spans` (of [`Edits`]) left `ranges` (sorted, apart; offsets of the text
/// before them): the ranges no span meets or touches, moved with the text, and the parts of the
/// current text where the others may be now (sorted, apart). A range that a span meets keeps
/// the ends outside the span, where the text did not change, and may reach anywhere in it.
fn follow(ranges: &[Range<usize>], spans: &[Changed]) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    // Each span's place in the text before the changes, and the shift of the text after it.
    let mut old = Vec::with_capacity(spans.len());
    let mut shift = 0;
    for span in spans {
        let start = shifted(span.start, -shift);
        shift += signed(span.len) - signed(span.old_len);
        old.push((start..start + span.old_len, shift));
    }
    let (mut kept, mut unsure): (Vec<Range<usize>>, Vec<Range<usize>>) = (Vec::new(), Vec::new());
    // The first span that does not end before the range, and the shift of the text before it.
    let (mut i, mut before) = (0, 0);
    for range in ranges {
        while i < old.len() && old[i].0.end < range.start {
            before = old[i].1;
            i += 1;
        }
        let met = old[i..]
            .iter()
            .take_while(|(span, _)| span.start <= range.end)
            .count();
        if met == 0 {
            kept.push(shifted(range.start, before)..shifted(range.end, before));
            continue;
        }
        let (first, last) = (i, i + met - 1);
        let start = if range.start < old[first].0.start {
            shifted(range.start, before)
        } else {
            spans[first].start
        };
        let end = if range.end > old[last].0.end {
            shifted(range.end, old[last].1)
        } else {
            spans[last].end()
        };
        match unsure.last_mut() {
            // Nothing is left of a range deleted with its text.
            _ if start >= end => {}
            Some(region) if start <= region.end => region.end = region.end.max(end),
            _ => unsure.push(start..end),
        }
    }
    (kept, unsure)
}

/// The bytes of `a` that are not in `b` (both sorted, apart), as sorted ranges.
fn difference(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut j = 0;
    for range in a {
        while j < b.len() && b[j].end <= range.start {
            j += 1;
        }
        let mut start = range.start;
        for cut in b[j..].iter().take_while(|cut| cut.start < range.end) {
            if cut.start > start {
                out.push(start..cut.start);
            }
            start = start.max(cut.end);
        }
        if start < range.end {
            out.push(start..range.end);
        }
    }
    out
}

/// `cuts` (sorted, apart) with those that no range of `keep` (sorted, apart from them) lies
/// between made one, from the first one's start to the last one's end.
fn joined_outside(cuts: Vec<Range<usize>>, keep: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::with_capacity(cuts.len());
    let mut j = 0;
    for cut in cuts {
        if let Some(last) = out.last_mut() {
            while j < keep.len() && keep[j].end <= last.end {
                j += 1;
            }
            if keep.get(j).is_none_or(|k| k.start >= cut.start) {
                last.end = cut.end;
                continue;
            }
        }
        out.push(cut);
    }
    out
}

/// The ranges of `tag` that start in `region` of the text, or reach into it from before (from
/// where they start), as byte offsets in text order.
fn tag_ranges(doc: &Document, tag: &gtk::TextTag, region: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let Some(mut iter) = doc.iter_at_byte(region.start) else {
        return out;
    };
    if iter.has_tag(tag) && !iter.starts_tag(Some(tag)) {
        iter.backward_to_tag_toggle(Some(tag));
    }
    // At a toggle off, or where the tag is not, the next toggle turns it on.
    while iter.starts_tag(Some(tag)) || iter.forward_to_tag_toggle(Some(tag)) {
        let start = doc.byte_offset(&iter);
        if start >= region.end && start > region.start {
            break;
        }
        iter.forward_to_tag_toggle(Some(tag));
        out.push(start..doc.byte_offset(&iter));
        if iter.is_end() {
            break;
        }
    }
    out
}

fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

/// `n` moved by `by` bytes.
fn shifted(n: usize, by: isize) -> usize {
    n.saturating_add_signed(by)
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
#[allow(clippy::single_range_in_vec_init)] // tagged ranges are lists
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

    fn span(start: usize, len: usize, old_len: usize) -> Changed {
        Changed {
            start,
            len,
            old_len,
        }
    }

    #[test]
    fn edits_merge_spans_that_meet() {
        let mut edits = Edits::default();
        assert!(edits.is_empty());
        // Typing "ab" at 10, then deleting 3 bytes at 29.
        edits.record(10..10, 1);
        edits.record(11..11, 1);
        edits.record(29..32, 0);
        assert_eq!(edits.spans, [span(10, 2, 0), span(29, 0, 3)]);
        // A change before both moves them; one that touches a span joins it, with the
        // unchanged bytes between them.
        edits.record(0..2, 5);
        assert_eq!(edits.spans, [span(0, 5, 2), span(13, 2, 0), span(32, 0, 3)]);
        edits.record(15..20, 1);
        assert_eq!(edits.spans, [span(0, 5, 2), span(13, 3, 5), span(28, 0, 3)]);
        // One across all of them makes a single span of the text they cover: the three spans'
        // 2 + 5 + 3 bytes before the changes and the 8 + 12 + 1 unchanged ones.
        edits.record(3..29, 0);
        assert_eq!(edits.spans, [span(0, 3, 2 + 5 + 3 + 8 + 12 + 1)]);
        assert!(!edits.is_empty());
        // No change at all is not one.
        let mut none = Edits::default();
        none.record(4..4, 0);
        assert!(none.is_empty());
    }

    #[test]
    fn edits_in_too_many_places_are_lost() {
        let mut edits = Edits::default();
        for i in 0..=MAX_EDIT_SPANS {
            edits.record(10 * i..10 * i, 1);
        }
        assert!(edits.lost && edits.spans.is_empty() && !edits.is_empty());
        edits.record(0..0, 1);
        assert!(edits.lost && edits.spans.is_empty());
    }

    /// Applies `changes` (range, text) to `text` and to [`Edits`], and checks [`follow`] against
    /// where GTK puts `ranges`: each range keeps the bytes it covered that remain, plus text
    /// inserted strictly inside it (text inserted at either end goes outside).
    fn check_follow(text: &str, ranges: &[Range<usize>], changes: &[(Range<usize>, &str)]) {
        // Mark each byte with whether it is tagged: a byte inserted inside a range is.
        let mut bytes: Vec<(u8, bool)> = text
            .bytes()
            .enumerate()
            .map(|(i, b)| (b, ranges.iter().any(|r| r.contains(&i))))
            .collect();
        let mut edits = Edits::default();
        for (range, insert) in changes {
            edits.record(range.clone(), insert.len());
            bytes.drain(range.clone());
            let a = range.start;
            let inside = a > 0 && a < bytes.len() && bytes[a - 1].1 && bytes[a].1;
            for (k, b) in insert.bytes().enumerate() {
                bytes.insert(a + k, (b, inside));
            }
        }
        let mut actual: Vec<Range<usize>> = Vec::new();
        for (i, &(_, tagged)) in bytes.iter().enumerate() {
            match actual.last_mut() {
                Some(r) if tagged && i == r.end => r.end += 1,
                _ if tagged => actual.push(i..i + 1),
                _ => {}
            }
        }
        let (kept, unsure) = follow(ranges, &edits.spans);
        // Every kept range is a range of the tag, and every range is kept or in a region.
        for r in &kept {
            assert!(
                actual.contains(r),
                "{r:?} is kept but the tag has {actual:?}"
            );
        }
        for r in &actual {
            let within = |u: &Range<usize>| u.start <= r.start && r.end <= u.end;
            assert!(
                kept.contains(r) || unsure.iter().any(within),
                "{r:?} is neither kept nor in {unsure:?}"
            );
        }
    }

    #[test]
    fn follow_moves_ranges_no_edit_meets() {
        let text = "0123456789abcdefghijklmnopqrstuvwxyz";
        let ranges = [2..5, 10..14, 20..22, 30..34];
        // Typing between ranges, at a range's start and at its end, and inside one.
        check_follow(text, &ranges, &[(7..7, "xy")]);
        check_follow(text, &ranges, &[(12..12, "in")]);
        check_follow(text, &ranges, &[(10..10, "at")]);
        check_follow(text, &ranges, &[(14..14, "end")]);
        // Deleting the gap between two ranges joins them; deleting a range removes it.
        check_follow(text, &ranges, &[(14..20, "")]);
        check_follow(text, &ranges, &[(9..15, "")]);
        // A line break inside a range, and edits in several places.
        check_follow(text, &ranges, &[(3..3, "\n"), (25..27, "ww"), (0..1, "")]);
        let (kept, unsure) = follow(&ranges, &[span(7, 2, 0)]);
        assert_eq!(kept, [2..5, 12..16, 22..24, 32..36]);
        assert!(unsure.is_empty());
        let (kept, unsure) = follow(&ranges, &[span(12, 4, 2)]);
        assert_eq!(kept, [2..5, 22..24, 32..36]);
        assert_eq!(unsure, [10..16]);
        // A range that ends where an edit starts is in doubt too (the edit may extend it).
        let (kept, unsure) = follow(&ranges, &[span(5, 1, 1)]);
        assert_eq!((kept, unsure), (vec![10..14, 20..22, 30..34], vec![2..6]));
    }

    #[test]
    fn follow_agrees_with_random_edits() {
        // xorshift64: the same cases on every run.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let text = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        for _ in 0..3000 {
            let mut ranges = Vec::new();
            let mut at = next(4);
            while at + 1 < text.len() && ranges.len() < 6 {
                let end = (at + 1 + next(6)).min(text.len());
                ranges.push(at..end);
                at = end + 1 + next(8);
            }
            let mut len = text.len();
            let mut changes = Vec::new();
            for _ in 0..1 + next(6) {
                let start = next(len + 1);
                let end = (start + next(4)).min(len);
                let insert = ["", "x", "yz", "\n"][next(4)];
                len = len - (end - start) + insert.len();
                changes.push((start..end, insert));
            }
            check_follow(text, &ranges, &changes);
        }
    }

    #[test]
    fn difference_of_ranges() {
        assert_eq!(difference(&[0..10], &[2..3, 5..7]), [0..2, 3..5, 7..10]);
        assert_eq!(difference(&[0..10, 20..30], &[5..25]), [0..5, 25..30]);
        assert_eq!(difference(&[0..10], &[0..10]), Vec::<Range<usize>>::new());
        assert_eq!(difference(&[0..4, 6..8], &[]), [0..4, 6..8]);
        assert_eq!(difference(&[], &[1..2]), Vec::<Range<usize>>::new());
        assert_eq!(difference(&[3..6], &[0..1, 8..9]), [3..6]);
    }

    #[test]
    fn cuts_join_across_what_is_not_kept() {
        let cuts = vec![0..2, 4..5, 8..10, 12..13];
        assert_eq!(joined_outside(cuts.clone(), &[]), [0..13]);
        assert_eq!(joined_outside(cuts.clone(), &[6..7]), [0..5, 8..13]);
        assert_eq!(
            joined_outside(cuts.clone(), &[2..3, 10..12]),
            [0..2, 4..10, 12..13]
        );
        // Kept ranges before the first cut or after the last one keep nothing apart.
        assert_eq!(joined_outside(cuts, &[20..22]), [0..13]);
        assert_eq!(joined_outside(vec![5..6], &[0..1]), [5..6]);
    }
}
