//! The hover registry (M3, WP3.0): one popover per editor view, filled by **providers** that
//! several modules register — references, citations and macros (WP3.2, [`PRIORITY_REFS`]) and
//! the rendered math preview (WP3.3, [`PRIORITY_MATH`]).
//!
//! # Providers
//!
//! ```ignore
//! hover::register(view, priority, |ctx: &HoverCtx| -> Option<HoverContent> { … });
//! ```
//!
//! A provider looks at the place the pointer rests on (or the cursor, for the keyboard) and
//! answers with something to show, or `None`. Providers run **highest priority first** (equal
//! priorities in registration order) and the first answer wins; only that one is shown. They
//! run on the main thread, at most once per hover, and must be quick (index lookups, not file
//! reads): slow work goes into [`HoverContent::Pending`].
//!
//! [`HoverCtx`] dereferences to a [`TextPoint`]: the document, the byte offset, the scanner
//! [`Context`](lemma_syntax::Context) and the token there, the line, and helpers for the name
//! under the point ([`TextPoint::item`], [`TextPoint::control_word`]). A provider whose
//! content describes more than the default range ([`TextPoint::anchor`]: the name, control word
//! or token) says so with [`HoverCtx::set_range`] — the math preview gives the whole formula,
//! so moving the pointer inside it keeps the popover instead of asking again.
//!
//! [`HoverContent`] is Pango markup (shown in a wrapping label; invalid markup is shown as
//! plain text), any widget, or a future that resolves to one of those later (`None`: nothing
//! to show after all). A pending content shows nothing at first, then a spinner if it takes
//! longer than [`SPINNER_DELAY`]; a newer hover drops the older future (latest wins).
//!
//! # The popover
//!
//! - **Pointer:** it appears when the pointer has rested on a character for `[preview]
//!   delay_ms` (300 ms) and a provider answers, pointing at the range; it stays while the
//!   pointer is inside the range or on the popover, and goes [`LEAVE_GRACE`] after the pointer
//!   left both (so the pointer can move onto it). `[editor] hover = false` turns pointer
//!   hovers off.
//! - **Keyboard:** `K` in Vim normal mode (a `[vim.normal]` binding of `win.show-hover`) and
//!   Alt+K (`win.show-hover`'s accelerator) show it at the cursor at once ([`show_at_cursor`];
//!   the character before the cursor is tried too, so the end of a name typed in insert mode
//!   works). Nothing to show: the view's error bell.
//! - It hides on typing (any key press in the view that is not a modifier, and any change of
//!   the text), scrolling, a click, and when the view loses the focus (keyboard hovers) or is
//!   unmapped. After typing or scrolling the pointer has to move again before the next hover.
//! - It **never takes the focus**: it does not autohide, nothing in it can be focused, and the
//!   view keeps the keyboard.
//!
//! The popover (CSS class `lemma-hover`) is created on the first hover of a view; until then a
//! view costs one motion controller, one click gesture and one key-layer handler.
//!
//! # Window hooks
//!
//! The window calls [`attach`] for every editor tab before the other M3 per-editor hooks (so
//! they can register providers in theirs), and [`detach`] after them when the tab closes. The
//! providers of WP3.2 are registered in [`attach_providers`] at the end of this file.

use super::{Editor, TexView, TextPoint, Trigger, keys};
use crate::window::WindowState;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::ops::{Deref, Range};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Priority of WP3.2's providers (references, citations, user macros).
pub const PRIORITY_REFS: i32 = 200;
/// Priority of WP3.3's math preview (below the references: `\eqref` inside math shows the
/// reference).
pub const PRIORITY_MATH: i32 = 100;
/// Priority of the key-layer handler that hides the popover on typing (it only looks).
pub const PRIORITY_HIDE_KEYS: i32 = 1000;
/// How long a pointer hover stays after the pointer left its range and the popover.
pub const LEAVE_GRACE: Duration = Duration::from_millis(250);
/// How long a pending content may take before the popover shows a spinner.
pub const SPINNER_DELAY: Duration = Duration::from_millis(300);
/// The shortest hover delay `[preview] delay_ms` can set.
pub const MIN_DELAY: Duration = Duration::from_millis(50);
/// Largest size of the popover's content before it scrolls, in pixels.
pub const MAX_CONTENT_WIDTH: i32 = 640;
pub const MAX_CONTENT_HEIGHT: i32 = 420;
/// The CSS class of the popover.
pub const CSS_CLASS: &str = "lemma-hover";

/// A future that resolves to the content to show (`None`: nothing after all).
pub type PendingContent = Pin<Box<dyn Future<Output = Option<HoverContent>>>>;

/// What a provider shows.
pub enum HoverContent {
    /// Pango markup, shown in a wrapping label (at most 72 characters wide).
    Markup(String),
    /// Any widget (a rendered formula, a crop of the PDF, …); it is shown as given, scrolled
    /// beyond [`MAX_CONTENT_WIDTH`] × [`MAX_CONTENT_HEIGHT`]. Nothing in it should take the
    /// focus.
    Widget(gtk::Widget),
    /// Content that takes time (see the module docs).
    Pending(PendingContent),
}

impl HoverContent {
    /// Plain text (escaped into markup).
    pub fn text(text: &str) -> Self {
        Self::Markup(glib::markup_escape_text(text).to_string())
    }

    /// Content computed by `future` (on the main loop; spawn blocking work on a thread inside
    /// it).
    pub fn pending(future: impl Future<Output = Option<HoverContent>> + 'static) -> Self {
        Self::Pending(Box::pin(future))
    }
}

impl std::fmt::Debug for HoverContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Markup(m) => f.debug_tuple("Markup").field(m).finish(),
            Self::Widget(w) => f.debug_tuple("Widget").field(&w.type_().name()).finish(),
            Self::Pending(_) => f.write_str("Pending(..)"),
        }
    }
}

/// What a provider gets: the place ([`TextPoint`], through `Deref`) and what asked.
pub struct HoverCtx {
    pub at: TextPoint,
    pub trigger: Trigger,
    range: RefCell<Option<Range<usize>>>,
}

impl HoverCtx {
    pub fn new(at: TextPoint, trigger: Trigger) -> Self {
        Self {
            at,
            trigger,
            range: RefCell::new(None),
        }
    }

    /// Declares the document byte range the provider's content describes (it must contain the
    /// point): the popover points at it and a pointer hover stays while the pointer is inside.
    /// Only the answering provider's range counts.
    pub fn set_range(&self, range: Range<usize>) {
        self.range.replace(Some(range));
    }

    /// The range set with [`HoverCtx::set_range`], else [`TextPoint::anchor`].
    pub fn range(&self) -> Range<usize> {
        self.range
            .borrow()
            .clone()
            .filter(|r| r.start <= self.at.offset && self.at.offset <= r.end)
            .unwrap_or_else(|| self.at.anchor())
    }
}

impl Deref for HoverCtx {
    type Target = TextPoint;

    fn deref(&self) -> &TextPoint {
        &self.at
    }
}

/// A provider (see the module docs).
pub type Provider = dyn Fn(&HoverCtx) -> Option<HoverContent>;

/// Identifies a provider for [`unregister`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProviderId(u64);

/// What the popover shows (or is about to show).
#[derive(Clone, Debug)]
struct Shown {
    range: Range<usize>,
    trigger: Trigger,
    /// The byte the hover was asked for (the popover points at its display line).
    at: usize,
}

/// The hover state of one view.
struct ViewHover {
    view: glib::WeakRef<TexView>,
    editor: Weak<Editor>,
    state: Weak<WindowState>,
    providers: RefCell<Vec<(i32, ProviderId, Rc<Provider>)>>,
    next_id: Cell<u64>,
    /// Created on the first hover.
    popover: RefCell<Option<Popover>>,
    shown: RefCell<Option<Shown>>,
    /// Incremented by every new hover and every hide: a pending content of an older
    /// generation is dropped.
    generation: Cell<u64>,
    task: RefCell<Option<glib::JoinHandle<()>>>,
    spinner_timer: RefCell<Option<glib::SourceId>>,
    dwell_timer: RefCell<Option<glib::SourceId>>,
    leave_timer: RefCell<Option<glib::SourceId>>,
    /// The pointer over the view (widget coordinates), and when it last moved.
    pointer: Cell<Option<(f64, f64)>>,
    last_motion: Cell<Option<Instant>>,
    /// The last key press, text change, scroll or click.
    last_interrupt: Cell<Option<Instant>>,
    in_popover: Cell<bool>,
    controllers: RefCell<Vec<gtk::EventController>>,
    signals: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    key_handler: Cell<Option<keys::HandlerId>>,
}

struct Popover {
    popover: gtk::Popover,
    scroller: gtk::ScrolledWindow,
}

thread_local! {
    static VIEWS: RefCell<Vec<Rc<ViewHover>>> = const { RefCell::new(Vec::new()) };
}

fn lookup(view: &TexView) -> Option<Rc<ViewHover>> {
    VIEWS.with_borrow(|views| {
        views
            .iter()
            .find(|v| v.view.upgrade().as_ref() == Some(view))
            .cloned()
    })
}

// ----- registry -----

/// Adds `provider` to `view`'s hovers with `priority` (higher runs first). `None` when the view
/// has no hover registry (the window gives every editor tab one, see [`attach`]).
pub fn register(
    view: &TexView,
    priority: i32,
    provider: impl Fn(&HoverCtx) -> Option<HoverContent> + 'static,
) -> Option<ProviderId> {
    let vh = lookup(view)?;
    let id = ProviderId(vh.next_id.get());
    vh.next_id.set(id.0 + 1);
    let mut providers = vh.providers.borrow_mut();
    let at = providers
        .iter()
        .position(|(p, _, _)| *p < priority)
        .unwrap_or(providers.len());
    providers.insert(at, (priority, id, Rc::new(provider)));
    Some(id)
}

/// Removes a provider added by [`register`].
pub fn unregister(view: &TexView, id: ProviderId) {
    if let Some(vh) = lookup(view) {
        vh.providers.borrow_mut().retain(|(_, i, _)| *i != id);
    }
}

/// Number of providers registered on `view`.
pub fn provider_count(view: &TexView) -> usize {
    lookup(view).map_or(0, |vh| vh.providers.borrow().len())
}

/// Asks `view`'s providers about `ctx`, highest priority first; the first answer.
pub fn query(view: &TexView, ctx: &HoverCtx) -> Option<HoverContent> {
    let vh = lookup(view)?;
    query_providers(&vh, ctx)
}

fn query_providers(vh: &ViewHover, ctx: &HoverCtx) -> Option<HoverContent> {
    // A snapshot: a provider may register or unregister others.
    let providers: Vec<Rc<Provider>> = vh
        .providers
        .borrow()
        .iter()
        .map(|(_, _, p)| p.clone())
        .collect();
    providers.iter().find_map(|p| p(ctx))
}

// ----- showing and hiding -----

/// Shows the hover for the document byte `offset` of `editor` now (no delay), as `trigger`
/// asked; returns false when no provider answers (a popover already shown stays). Nothing is
/// shown while that place is scrolled out of the view.
pub fn show_at(editor: &Editor, offset: usize, trigger: Trigger) -> bool {
    lookup(editor.view()).is_some_and(|vh| request(&vh, offset, trigger))
}

/// `win.show-hover` (`K` in Vim normal mode, Alt+K): the hover at the active editor's cursor,
/// else at the character before it; the error bell when there is nothing to show.
pub fn show_at_cursor(state: &Rc<WindowState>) -> bool {
    let Some(editor) = state.active_editor() else {
        return false;
    };
    let Some(vh) = lookup(editor.view()) else {
        return false;
    };
    // The popover points at the cursor, so it has to be on screen.
    let buffer = editor.buffer();
    editor.view().scroll_mark_onscreen(&buffer.get_insert());
    let cursor = editor.cursor_offset();
    if request(&vh, cursor, Trigger::Keyboard) {
        return true;
    }
    if let Some(before) = char_before(&editor, cursor)
        && request(&vh, before, Trigger::Keyboard)
    {
        return true;
    }
    editor.view().error_bell();
    false
}

/// The offset of the character before `offset` on its line.
pub(crate) fn char_before(editor: &Editor, offset: usize) -> Option<usize> {
    let at = TextPoint::new(editor.document(), offset)?;
    let c = at.line_text.get(..at.column)?.chars().next_back()?;
    Some(offset - c.len_utf8())
}

/// Hides `view`'s popover (and drops a pending content).
pub fn hide(view: &TexView) {
    if let Some(vh) = lookup(view) {
        hide_now(&vh);
    }
}

/// The popover of `view`, once it was shown (tests).
pub fn popover(view: &TexView) -> Option<gtk::Popover> {
    let vh = lookup(view)?;
    vh.popover.borrow().as_ref().map(|p| p.popover.clone())
}

/// True while `view`'s popover is shown or about to be (its content is pending).
pub fn is_shown(view: &TexView) -> bool {
    lookup(view).is_some_and(|vh| vh.shown.borrow().is_some())
}

/// The document byte range the shown hover is about, and what asked for it.
pub fn shown_range(view: &TexView) -> Option<(Range<usize>, Trigger)> {
    let vh = lookup(view)?;
    let shown = vh.shown.borrow();
    shown.as_ref().map(|s| (s.range.clone(), s.trigger))
}

/// The widget the popover shows (the label of a markup content, a provider's widget, or the
/// spinner), while it is shown (tests).
pub fn shown_widget(view: &TexView) -> Option<gtk::Widget> {
    let vh = lookup(view)?;
    let popover = vh.popover.borrow();
    let popover = popover.as_ref()?;
    if !popover.popover.is_visible() {
        return None;
    }
    scrolled_child(&popover.scroller)
}

/// The widget shown in `scroller` (GtkScrolledWindow wraps a widget that is not scrollable in
/// a viewport of its own).
fn scrolled_child(scroller: &gtk::ScrolledWindow) -> Option<gtk::Widget> {
    let child = scroller.child()?;
    match child.downcast::<gtk::Viewport>() {
        Ok(viewport) => viewport.child(),
        Err(child) => Some(child),
    }
}

/// Starts (or replaces) the hover for `offset`; false when no provider answers (what is shown
/// stays: a pointer hover goes with its leave grace).
fn request(vh: &Rc<ViewHover>, offset: usize, trigger: Trigger) -> bool {
    let Some(editor) = vh.editor.upgrade() else {
        return false;
    };
    let Some(at) = TextPoint::new(editor.document(), offset) else {
        return false;
    };
    let ctx = HoverCtx::new(at, trigger);
    let Some(content) = query_providers(vh, &ctx) else {
        return false;
    };
    // Nothing older may land now.
    cancel_pending(vh);
    vh.shown.replace(Some(Shown {
        range: ctx.range(),
        trigger,
        at: offset,
    }));
    match content {
        HoverContent::Pending(future) => {
            let generation = vh.generation.get();
            let weak = Rc::downgrade(vh);
            let timer = glib::timeout_add_local_once(SPINNER_DELAY, move || {
                if let Some(vh) = weak.upgrade() {
                    vh.spinner_timer.take();
                    if vh.generation.get() == generation {
                        present(&vh, spinner());
                    }
                }
            });
            vh.spinner_timer.replace(Some(timer));
            let weak = Rc::downgrade(vh);
            let task = glib::spawn_future_local(async move {
                let mut content = future.await;
                while let Some(HoverContent::Pending(next)) = content {
                    content = next.await;
                }
                let Some(vh) = weak.upgrade() else {
                    return;
                };
                if vh.generation.get() != generation {
                    return;
                }
                if let Some(timer) = vh.spinner_timer.take() {
                    timer.remove();
                }
                vh.task.take();
                match content {
                    Some(HoverContent::Markup(markup)) => present(&vh, markup_label(&markup)),
                    Some(HoverContent::Widget(widget)) => present(&vh, widget),
                    Some(HoverContent::Pending(_)) | None => hide_now(&vh),
                }
            });
            vh.task.replace(Some(task));
        }
        HoverContent::Markup(markup) => present(vh, markup_label(&markup)),
        HoverContent::Widget(widget) => present(vh, widget),
    }
    true
}

/// Shows `child` in the popover, pointing at the shown range.
fn present(vh: &Rc<ViewHover>, child: gtk::Widget) {
    let (Some(editor), Some(view)) = (vh.editor.upgrade(), vh.view.upgrade()) else {
        return;
    };
    let Some(shown) = vh.shown.borrow().clone() else {
        return;
    };
    // A popover must point into the view (GTK cannot place one pointing outside its window):
    // nothing is shown for a range scrolled out of sight.
    let Some(rect) = editor
        .range_rect(shown.range.clone(), shown.at)
        .and_then(|rect| clamp_to_view(rect, view.width(), view.height()))
    else {
        hide_now(vh);
        return;
    };
    let popover = ensure_popover(vh, &view);
    // A provider may answer with the widget it gave last time (still shown here); a widget
    // that is somewhere else cannot be shown.
    if scrolled_child(&popover.scroller).as_ref() != Some(&child) {
        if child.parent().is_some() {
            tracing::warn!(
                "a hover provider gave a {} that is shown elsewhere",
                child.type_().name()
            );
            hide_now(vh);
            return;
        }
        popover.scroller.set_child(Some(&child));
        fit_scroller(&popover.scroller, &child);
    }
    popover.popover.set_pointing_to(Some(&rect));
    if !popover.popover.is_visible() {
        popover.popover.popup();
    }
}

/// Scrolls only in the directions where `child` is larger than [`MAX_CONTENT_WIDTH`] ×
/// [`MAX_CONTENT_HEIGHT`]. A scroller that may scroll vertically is at least as tall as its
/// scrollbar's minimum (about 58 px), which left blank space under one or two lines of text or
/// a small formula.
fn fit_scroller(scroller: &gtk::ScrolledWindow, child: &gtk::Widget) {
    let (_, width, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
    let (_, height, _, _) = child.measure(gtk::Orientation::Vertical, width.min(MAX_CONTENT_WIDTH));
    let policy = |fits: bool| {
        if fits {
            gtk::PolicyType::Never
        } else {
            gtk::PolicyType::Automatic
        }
    };
    scroller.set_policy(
        policy(width <= MAX_CONTENT_WIDTH),
        policy(height <= MAX_CONTENT_HEIGHT),
    );
}

/// `rect` cut to the view's `width` × `height`; `None` when nothing of it is inside.
fn clamp_to_view(
    rect: gtk::gdk::Rectangle,
    width: i32,
    height: i32,
) -> Option<gtk::gdk::Rectangle> {
    let (x0, y0) = (rect.x().max(0), rect.y().max(0));
    let (x1, y1) = (
        (rect.x() + rect.width()).min(width),
        (rect.y() + rect.height()).min(height),
    );
    (x1 > x0 && y1 > y0).then(|| gtk::gdk::Rectangle::new(x0, y0, x1 - x0, y1 - y0))
}

fn ensure_popover(vh: &Rc<ViewHover>, view: &TexView) -> Popover {
    if let Some(p) = vh.popover.borrow().as_ref() {
        return Popover {
            popover: p.popover.clone(),
            scroller: p.scroller.clone(),
        };
    }
    let scroller = gtk::ScrolledWindow::builder()
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_width(MAX_CONTENT_WIDTH)
        .max_content_height(MAX_CONTENT_HEIGHT)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .can_focus(false)
        .build();
    let popover = gtk::Popover::builder()
        .child(&scroller)
        .autohide(false)
        .has_arrow(true)
        .position(gtk::PositionType::Top)
        .can_focus(false)
        .focusable(false)
        .build();
    popover.add_css_class(CSS_CLASS);
    popover.set_parent(view);
    // The pointer may move onto the popover (to read a long one) without it going away.
    let motion = gtk::EventControllerMotion::new();
    let weak = Rc::downgrade(vh);
    motion.connect_enter(move |_, _, _| {
        if let Some(vh) = weak.upgrade() {
            vh.in_popover.set(true);
            stop_timer(&vh.leave_timer);
        }
    });
    let weak = Rc::downgrade(vh);
    motion.connect_leave(move |_| {
        if let Some(vh) = weak.upgrade() {
            vh.in_popover.set(false);
            start_leave_grace(&vh);
        }
    });
    popover.add_controller(motion);
    vh.popover.replace(Some(Popover {
        popover: popover.clone(),
        scroller: scroller.clone(),
    }));
    Popover { popover, scroller }
}

/// The label of a markup content; markup Pango cannot parse is shown as plain text.
fn markup_label(markup: &str) -> gtk::Widget {
    let valid = pango::parse_markup(markup, '\0').is_ok();
    if !valid {
        tracing::warn!("a hover provider gave invalid markup: {markup:?}");
    }
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(72)
        .xalign(0.0)
        .yalign(0.0)
        .selectable(false)
        .can_focus(false)
        .build();
    if valid {
        label.set_markup(markup);
    } else {
        label.set_text(markup);
    }
    label.add_css_class("lemma-hover-text");
    label.upcast()
}

fn spinner() -> gtk::Widget {
    adw::Spinner::builder()
        .width_request(24)
        .height_request(24)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build()
        .upcast()
}

/// Drops a pending content (and its spinner timer) without hiding what is shown.
fn cancel_pending(vh: &ViewHover) {
    vh.generation.set(vh.generation.get() + 1);
    if let Some(task) = vh.task.take() {
        task.abort();
    }
    stop_timer(&vh.spinner_timer);
}

fn hide_now(vh: &ViewHover) {
    cancel_pending(vh);
    stop_timer(&vh.leave_timer);
    vh.shown.replace(None);
    vh.in_popover.set(false);
    let popover = vh.popover.borrow().as_ref().map(|p| p.popover.clone());
    if let Some(popover) = popover
        && popover.is_visible()
    {
        popover.popdown();
    }
}

fn stop_timer(slot: &RefCell<Option<glib::SourceId>>) {
    if let Some(timer) = slot.take() {
        timer.remove();
    }
}

// ----- the pointer -----

/// The hover delay (`[preview] delay_ms`).
fn delay(vh: &ViewHover) -> Duration {
    let ms = vh
        .state
        .upgrade()
        .map_or(300, |state| state.config.preview.delay_ms);
    Duration::from_millis(u64::from(ms)).max(MIN_DELAY)
}

fn pointer_hovers_enabled(vh: &ViewHover) -> bool {
    vh.state
        .upgrade()
        .is_some_and(|state| state.config.editor.hover)
}

/// What the view's motion controller does when the pointer moves to `(x, y)` (widget
/// coordinates); tests call it (GTK cannot inject pointer events).
pub fn pointer_moved(view: &TexView, x: f64, y: f64) {
    if let Some(vh) = lookup(view) {
        on_motion(&vh, x, y);
    }
}

/// What the view's motion controller does when the pointer leaves the view (tests).
pub fn pointer_left(view: &TexView) {
    if let Some(vh) = lookup(view) {
        on_leave(&vh);
    }
}

fn on_motion(vh: &Rc<ViewHover>, x: f64, y: f64) {
    if vh.pointer.get() == Some((x, y)) {
        return;
    }
    vh.pointer.set(Some((x, y)));
    vh.last_motion.set(Some(Instant::now()));
    let shown = vh.shown.borrow().clone();
    if let Some(shown) = shown
        && shown.trigger == Trigger::Pointer
    {
        let editor = vh.editor.upgrade();
        let inside = editor
            .and_then(|e| e.offset_at_point(x, y))
            .is_some_and(|o| shown.range.contains(&o));
        if inside {
            stop_timer(&vh.leave_timer);
            return;
        }
        start_leave_grace(vh);
    }
    if !pointer_hovers_enabled(vh) {
        return;
    }
    if vh.dwell_timer.borrow().is_none() {
        schedule_dwell(vh, delay(vh));
    }
}

fn on_leave(vh: &Rc<ViewHover>) {
    vh.pointer.set(None);
    stop_timer(&vh.dwell_timer);
    let pointer_hover = vh
        .shown
        .borrow()
        .as_ref()
        .is_some_and(|s| s.trigger == Trigger::Pointer);
    if pointer_hover {
        start_leave_grace(vh);
    }
}

/// Runs [`dwelled`] once the pointer has rested for the hover delay: one timer, re-armed for
/// the rest of the delay while the pointer keeps moving (no timer churn per motion event).
fn schedule_dwell(vh: &Rc<ViewHover>, after: Duration) {
    let weak = Rc::downgrade(vh);
    let timer = glib::timeout_add_local_once(after, move || {
        let Some(vh) = weak.upgrade() else {
            return;
        };
        vh.dwell_timer.take();
        let wanted = delay(&vh);
        let rested = vh.last_motion.get().map_or(wanted, |t| t.elapsed());
        if rested < wanted {
            schedule_dwell(&vh, wanted - rested);
        } else {
            dwelled(&vh);
        }
    });
    vh.dwell_timer.replace(Some(timer));
}

/// The pointer rested: hover what is under it.
fn dwelled(vh: &Rc<ViewHover>) {
    // Typing, scrolling or a click since the pointer stopped: wait for it to move again.
    if let (Some(interrupt), Some(motion)) = (vh.last_interrupt.get(), vh.last_motion.get())
        && interrupt >= motion
    {
        return;
    }
    let (Some((x, y)), Some(editor), Some(view)) =
        (vh.pointer.get(), vh.editor.upgrade(), vh.view.upgrade())
    else {
        return;
    };
    if !view.is_mapped() || !pointer_hovers_enabled(vh) {
        return;
    }
    let Some(offset) = editor.offset_at_point(x, y) else {
        return;
    };
    let same = vh
        .shown
        .borrow()
        .as_ref()
        .is_some_and(|s| s.trigger == Trigger::Pointer && s.range.contains(&offset));
    if !same {
        request(vh, offset, Trigger::Pointer);
    }
}

fn start_leave_grace(vh: &Rc<ViewHover>) {
    if vh.leave_timer.borrow().is_some() {
        return;
    }
    let weak = Rc::downgrade(vh);
    let timer = glib::timeout_add_local_once(LEAVE_GRACE, move || {
        let Some(vh) = weak.upgrade() else {
            return;
        };
        vh.leave_timer.take();
        if vh.in_popover.get() {
            return;
        }
        let shown = vh.shown.borrow().clone();
        let Some(shown) = shown.filter(|s| s.trigger == Trigger::Pointer) else {
            return;
        };
        let inside = match (vh.pointer.get(), vh.editor.upgrade()) {
            (Some((x, y)), Some(editor)) => editor
                .offset_at_point(x, y)
                .is_some_and(|o| shown.range.contains(&o)),
            _ => false,
        };
        if !inside {
            hide_now(&vh);
        }
    });
    vh.leave_timer.replace(Some(timer));
}

/// Typing, a text change, scrolling or a click: the popover goes, and the pointer has to move
/// before the next hover.
fn interrupt(vh: &ViewHover) {
    vh.last_interrupt.set(Some(Instant::now()));
    stop_timer(&vh.dwell_timer);
    if vh.shown.borrow().is_some() {
        hide_now(vh);
    }
}

// ----- window hooks -----

/// Gives `editor`'s view its hover registry and popover behaviour (see the module docs), then
/// registers WP3.2's providers ([`attach_providers`]). Called by the window for every editor
/// tab, before the other M3 per-editor hooks.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    if lookup(view).is_some() {
        return;
    }
    let vh = Rc::new(ViewHover {
        view: view.downgrade(),
        editor: Rc::downgrade(editor),
        state: Rc::downgrade(state),
        providers: RefCell::new(Vec::new()),
        next_id: Cell::new(1),
        popover: RefCell::new(None),
        shown: RefCell::new(None),
        generation: Cell::new(0),
        task: RefCell::new(None),
        spinner_timer: RefCell::new(None),
        dwell_timer: RefCell::new(None),
        leave_timer: RefCell::new(None),
        pointer: Cell::new(None),
        last_motion: Cell::new(None),
        last_interrupt: Cell::new(None),
        in_popover: Cell::new(false),
        controllers: RefCell::new(Vec::new()),
        signals: RefCell::new(Vec::new()),
        key_handler: Cell::new(None),
    });
    VIEWS.with_borrow_mut(|views| {
        views.retain(|v| v.view.upgrade().is_some());
        views.push(vh.clone());
    });

    let motion = gtk::EventControllerMotion::new();
    motion.set_name(Some("lemma-hover-motion"));
    let weak = Rc::downgrade(&vh);
    motion.connect_motion(move |controller, x, y| {
        let Some(vh) = weak.upgrade() else {
            return;
        };
        // No hovers while a button is held (selecting text).
        let buttons = gtk::gdk::ModifierType::BUTTON1_MASK
            | gtk::gdk::ModifierType::BUTTON2_MASK
            | gtk::gdk::ModifierType::BUTTON3_MASK;
        if controller.current_event_state().intersects(buttons) {
            vh.pointer.set(Some((x, y)));
            vh.last_motion.set(Some(Instant::now()));
            interrupt(&vh);
            return;
        }
        on_motion(&vh, x, y);
    });
    let weak = Rc::downgrade(&vh);
    motion.connect_leave(move |_| {
        if let Some(vh) = weak.upgrade() {
            on_leave(&vh);
        }
    });
    view.add_controller(motion.clone());

    // Capture phase, never claiming: the view's own gestures still get every click.
    let click = gtk::GestureClick::new();
    click.set_name(Some("lemma-hover-click"));
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&vh);
    click.connect_pressed(move |_, _, _, _| {
        if let Some(vh) = weak.upgrade() {
            interrupt(&vh);
        }
    });
    view.add_controller(click.clone());
    vh.controllers
        .replace(vec![motion.upcast(), click.upcast()]);

    let weak = Rc::downgrade(&vh);
    let handler = keys::register(view, PRIORITY_HIDE_KEYS, move |event, _| {
        if let Some(vh) = weak.upgrade()
            && keys::key_name(event.keyval, event.state).is_some()
        {
            interrupt(&vh);
        }
        glib::Propagation::Proceed
    });
    vh.key_handler.set(handler);

    let mut signals: Vec<(glib::Object, glib::SignalHandlerId)> = Vec::new();
    let weak = Rc::downgrade(&vh);
    let buffer = editor.buffer();
    signals.push((
        buffer.clone().upcast(),
        buffer.connect_changed(move |_| {
            if let Some(vh) = weak.upgrade() {
                interrupt(&vh);
            }
        }),
    ));
    for adjustment in [view.vadjustment(), view.hadjustment()]
        .into_iter()
        .flatten()
    {
        let weak = Rc::downgrade(&vh);
        let id = adjustment.connect_value_changed(move |_| {
            if let Some(vh) = weak.upgrade() {
                interrupt(&vh);
            }
        });
        signals.push((adjustment.upcast(), id));
    }
    let weak = Rc::downgrade(&vh);
    signals.push((
        view.clone().upcast(),
        view.connect_has_focus_notify(move |view| {
            let Some(vh) = weak.upgrade() else {
                return;
            };
            let keyboard = vh
                .shown
                .borrow()
                .as_ref()
                .is_some_and(|s| s.trigger == Trigger::Keyboard);
            if keyboard && !view.has_focus() {
                hide_now(&vh);
            }
        }),
    ));
    let weak = Rc::downgrade(&vh);
    signals.push((
        view.clone().upcast(),
        view.connect_unmap(move |_| {
            if let Some(vh) = weak.upgrade() {
                stop_timer(&vh.dwell_timer);
                hide_now(&vh);
            }
        }),
    ));
    vh.signals.replace(signals);

    attach_providers(state, editor);
}

/// Removes everything [`attach`] added to `editor`'s view (the popover included). Called by the
/// window when the tab closes, after the other M3 per-editor hooks.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    let Some(vh) = lookup(view) else {
        return;
    };
    VIEWS.with_borrow_mut(|views| views.retain(|v| !Rc::ptr_eq(v, &vh)));
    hide_now(&vh);
    stop_timer(&vh.dwell_timer);
    vh.providers.borrow_mut().clear();
    for controller in vh.controllers.take() {
        view.remove_controller(&controller);
    }
    for (object, id) in vh.signals.take() {
        object.disconnect(id);
    }
    if let Some(id) = vh.key_handler.take() {
        keys::unregister(view, id);
    }
    if let Some(popover) = vh.popover.take() {
        popover.popover.unparent();
    }
}

// ----- providers (WP3.2) -----

/// Registers WP3.2's hovers on `editor`'s view (called at the end of [`attach`]):
///
/// - a reference (`\ref`, `\eqref`, `\cref`, … including ranges): its number and page from the
///   `.aux` file plus the three source lines of the labelled environment — and, when the target
///   has a SyncTeX box, a 2× crop of it from the PDF (through the existing poppler worker), else
///   the text;
/// - a citation key: authors, year and title; a key that cannot be found says so;
/// - a user macro: its definition.
///
/// Use [`PRIORITY_REFS`]. The contents are `labels::resolve::hover_content`.
fn attach_providers(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let weak = Rc::downgrade(state);
    let id = register(editor.view(), PRIORITY_REFS, move |ctx| {
        let state = weak.upgrade()?;
        crate::labels::resolve::hover_content(&state, ctx)
    });
    crate::labels::remember_tools(editor.view(), id, None);
}

#[cfg(test)]
mod tests {
    use super::clamp_to_view;
    use gtk::gdk::Rectangle;

    #[test]
    fn rectangles_are_cut_to_the_view() {
        let r = |x, y, w, h| Rectangle::new(x, y, w, h);
        assert_eq!(
            clamp_to_view(r(10, 20, 30, 18), 800, 600),
            Some(r(10, 20, 30, 18))
        );
        assert_eq!(
            clamp_to_view(r(-5, 590, 30, 18), 800, 600),
            Some(r(0, 590, 25, 10))
        );
        assert_eq!(
            clamp_to_view(r(198, -449, 35, 18), 800, 600),
            None,
            "scrolled away"
        );
        assert_eq!(clamp_to_view(r(10, 600, 30, 18), 800, 600), None, "below");
        assert_eq!(
            clamp_to_view(r(800, 10, 30, 18), 800, 600),
            None,
            "right of it"
        );
    }
}
