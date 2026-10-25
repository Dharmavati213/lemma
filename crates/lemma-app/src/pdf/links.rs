//! Links in the PDF preview.
//!
//! The links of the pages on screen come from `PdfCmd::Links` (once per page and generation).
//!
//! - **Pointer:** over a link the pointer is a hand. Resting on an internal link for
//!   `[preview] delay_ms` shows a sioyek-style popover with the destination: about a quarter of
//!   the target page around the target, at the view's zoom (at most [`MAX_PREVIEW_ZOOM`]),
//!   with "Page N" below it. It borrows the page's texture from the view when it is cached and
//!   otherwise has the page rendered at exactly its width, in the view's mode (a thumbnail
//!   request, freed when the popover goes). The popover goes when the pointer leaves the link
//!   (after [`LEAVE_GRACE`]), on scrolling, a click, a key or when the view is hidden. A web link
//!   shows its address as a tooltip.
//! - **Click:** an internal link scrolls its destination to near the top of the view
//!   ([`JUMP_MARGIN`]) and records where the view was. A web link asks first ("Open Link?")
//!   and then opens with `gtk::UriLauncher`; runs that quit by themselves refuse (tests route it
//!   with [`set_uri_hook`]). Ctrl+click stays inverse search.
//! - **Back:** Alt+Left (`win.goto-back`, through `editor::goto::add_back_handler`) returns from
//!   link jumps while the PDF has the focus, also in a detached PDF window; so do Backspace in
//!   the view and the mouse's back button (forward: the mouse's forward button).
//!
//! Hooks: [`install`] once per window (after `pdf_controller::install`).

use super::page_picture::PagePicture;
use super::pdf_controller::ThumbClient;
use super::pdf_view::PdfView;
use crate::build_ui::install_css_once;
use crate::editor::goto;
use crate::window::WindowState;
use adw::prelude::*;
use gtk::gdk;
use lemma_pdf::{Anchor, Link, LinkDest, PdfCmd, ThumbReq};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How far below the top of the view a link's destination lands, in logical pixels.
pub const JUMP_MARGIN: f64 = 24.0;
/// The preview is never drawn larger than this zoom (it renders a whole page at it).
pub const MAX_PREVIEW_ZOOM: f64 = 2.0;
/// The largest preview, in logical pixels.
pub const MAX_PREVIEW_SIZE: (f64, f64) = (640.0, 300.0);
/// How long the preview stays after the pointer left the link (and the popover).
pub const LEAVE_GRACE: Duration = Duration::from_millis(250);
/// Most entries of the link history.
pub const MAX_HISTORY: usize = 100;
/// The pointer may move this far (logical pixels) between press and release of a link click.
const CLICK_SLOP: f64 = 4.0;

const CSS: &str = "
popover.lemma-pdf-link-preview > contents { padding: 6px; }
popover.lemma-pdf-link-preview pagepicture { border-radius: 4px; }
popover.lemma-pdf-link-preview .lemma-pdf-link-caption { margin-top: 4px; }
";

/// Opens a web address instead of `gtk::UriLauncher` (see [`set_uri_hook`]); returns whether it
/// did.
pub type UriHook = dyn Fn(&str) -> bool;

/// A link under the pointer: page, index in that page's links, and the link.
#[derive(Clone, Debug, PartialEq)]
struct Hit {
    page: u32,
    index: usize,
    link: Link,
}

/// The links of one window's PDF view (registered with the controller).
pub struct Links {
    state: Weak<WindowState>,
    view: PdfView,
    generation: Cell<u64>,
    links: RefCell<HashMap<u32, Vec<Link>>>,
    requested: RefCell<HashSet<u32>>,
    hovered: RefCell<Option<Hit>>,
    hover_timer: RefCell<Option<glib::SourceId>>,
    leave_timer: RefCell<Option<glib::SourceId>>,
    in_popover: Cell<bool>,
    preview: RefCell<Option<Preview>>,
    /// Where the view was before each link jump (most recent last), and after each Back.
    back: RefCell<Vec<Anchor>>,
    forward: RefCell<Vec<Anchor>>,
    /// The button and point of the press a release may complete into a click.
    press: Cell<Option<(u32, f64, f64)>>,
    uri_hook: RefCell<Option<Box<UriHook>>>,
    uri_dialog: RefCell<Option<adw::AlertDialog>>,
}

/// The preview popover (created on the first hover).
struct Preview {
    popover: gtk::Popover,
    picture: PagePicture,
    caption: gtk::Label,
    /// The link shown.
    shown: RefCell<Option<Hit>>,
    /// The thumbnail asked for, until it arrives.
    want: Cell<Option<ThumbReq>>,
    /// The view's scroll position when it was shown (scrolling hides it).
    scroll: Cell<(f64, f64)>,
}

impl Links {
    fn state(&self) -> Option<Rc<WindowState>> {
        self.state.upgrade()
    }

    fn send(&self, cmd: PdfCmd) {
        if let Some(state) = self.state()
            && let Some(pdf) = state.pdf.get()
        {
            pdf.send(cmd);
        }
    }

    /// A new generation (or another PDF): its links are fetched anew.
    pub(super) fn document_changed(&self, generation: u64) {
        self.generation.set(generation);
        self.links.borrow_mut().clear();
        self.requested.borrow_mut().clear();
        self.hovered.replace(None);
        self.view.set_cursor(None);
        self.hide_preview();
        self.request_visible();
    }

    /// The links of `page` (`PdfEvent::Links`).
    pub(super) fn links_arrived(&self, generation: u64, page: u32, links: Vec<Link>) {
        if generation != self.generation.get() {
            return;
        }
        self.links.borrow_mut().insert(page, links);
    }

    /// Asks for the links of the pages on screen not asked for yet.
    fn request_visible(&self) {
        if self.view.page_count() == 0 || self.generation.get() == 0 {
            return;
        }
        let pages: Vec<u32> = {
            let mut requested = self.requested.borrow_mut();
            self.view
                .visible_pages()
                .filter(|p| requested.insert(*p))
                .collect()
        };
        for page in pages {
            self.send(PdfCmd::Links { page });
        }
    }

    /// The link at a point of the view (widget coordinates).
    fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        let (page, x_bp, y_bp) = self.view.page_point(x, y)?;
        let links = self.links.borrow();
        let Some(page_links) = links.get(&page) else {
            drop(links);
            // Not fetched yet (the page just came on screen).
            if self.requested.borrow_mut().insert(page) {
                self.send(PdfCmd::Links { page });
            }
            return None;
        };
        page_links
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| {
                let (lx, ly, lw, lh) = l.area;
                x_bp >= lx && x_bp <= lx + lw && y_bp >= ly && y_bp <= ly + lh
            })
            .map(|(index, link)| Hit {
                page,
                index,
                link: link.clone(),
            })
    }

    /// The rectangle of `hit`'s link in widget coordinates.
    fn hit_rect(&self, hit: &Hit) -> Option<gdk::Rectangle> {
        let (x, y, w, h) = hit.link.area;
        let (x0, y0) = self.view.widget_point(hit.page, x, y)?;
        let (x1, y1) = self.view.widget_point(hit.page, x + w, y + h)?;
        Some(gdk::Rectangle::new(
            x0.floor() as i32,
            y0.floor() as i32,
            ((x1 - x0).ceil() as i32).max(1),
            ((y1 - y0).ceil() as i32).max(1),
        ))
    }

    // ----- the pointer -----

    fn pointer_moved(self: &Rc<Self>, x: f64, y: f64) {
        let hit = self.hit(x, y);
        let same = *self.hovered.borrow() == hit;
        if same {
            return;
        }
        stop(&self.hover_timer);
        self.view
            .set_cursor_from_name(hit.as_ref().map(|_| "pointer"));
        let shown_same = self
            .preview
            .borrow()
            .as_ref()
            .is_some_and(|p| p.shown.borrow().as_ref() == hit.as_ref());
        if shown_same {
            stop(&self.leave_timer);
        } else if self.preview_visible() {
            self.start_leave_grace();
        }
        if let Some(h) = &hit
            && matches!(h.link.dest, LinkDest::Page { .. })
            && !shown_same
        {
            let delay = self
                .state()
                .map_or(300, |s| u64::from(s.config.preview.delay_ms));
            let weak = Rc::downgrade(self);
            let target = h.clone();
            let timer = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
                if let Some(links) = weak.upgrade() {
                    links.hover_timer.take();
                    if links.hovered.borrow().as_ref() == Some(&target) {
                        links.show_preview(&target);
                    }
                }
            });
            self.hover_timer.replace(Some(timer));
        }
        self.hovered.replace(hit);
    }

    fn pointer_left(self: &Rc<Self>) {
        stop(&self.hover_timer);
        self.hovered.replace(None);
        self.view.set_cursor(None);
        if self.preview_visible() {
            self.start_leave_grace();
        }
    }

    fn start_leave_grace(self: &Rc<Self>) {
        if self.leave_timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(LEAVE_GRACE, move || {
            if let Some(links) = weak.upgrade() {
                links.leave_timer.take();
                let on_link = links.hovered.borrow().is_some()
                    && links
                        .preview
                        .borrow()
                        .as_ref()
                        .is_some_and(|p| *p.shown.borrow() == *links.hovered.borrow());
                if !links.in_popover.get() && !on_link {
                    links.hide_preview();
                }
            }
        });
        self.leave_timer.replace(Some(timer));
    }

    fn preview_visible(&self) -> bool {
        self.preview
            .borrow()
            .as_ref()
            .is_some_and(|p| p.popover.is_visible())
    }

    // ----- the preview -----

    fn ensure_preview(self: &Rc<Self>) {
        if self.preview.borrow().is_some() {
            return;
        }
        let picture = PagePicture::new();
        let caption = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["caption", "dim-label", "lemma-pdf-link-caption"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&picture);
        content.append(&caption);
        let popover = gtk::Popover::builder()
            .child(&content)
            .autohide(false)
            .has_arrow(true)
            .position(gtk::PositionType::Bottom)
            .can_focus(false)
            .focusable(false)
            .css_classes(["lemma-pdf-link-preview"])
            .build();
        popover.set_parent(&self.view);
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(links) = weak.upgrade() {
                links.in_popover.set(true);
                stop(&links.leave_timer);
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(links) = weak.upgrade() {
                links.in_popover.set(false);
                links.start_leave_grace();
            }
        });
        popover.add_controller(motion);
        // A click on the preview follows the link.
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |gesture, _, _, _| {
            if let Some(links) = weak.upgrade() {
                let shown = links
                    .preview
                    .borrow()
                    .as_ref()
                    .and_then(|p| p.shown.borrow().clone());
                if let Some(hit) = shown {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    links.follow(&hit.link);
                }
            }
        });
        popover.add_controller(click);
        self.preview.replace(Some(Preview {
            popover,
            picture,
            caption,
            shown: RefCell::new(None),
            want: Cell::new(None),
            scroll: Cell::new((0.0, 0.0)),
        }));
    }

    /// Shows the destination of `hit` (an internal link) in the popover.
    fn show_preview(self: &Rc<Self>, hit: &Hit) {
        let LinkDest::Page { page, y_bp } = hit.link.dest else {
            return;
        };
        let Some((pw, ph)) = self.view.page_size(page) else {
            return;
        };
        let Some(rect) = self.hit_rect(hit) else {
            return;
        };
        self.ensure_preview();
        let (region, width) = preview_region(
            (pw, ph),
            y_bp,
            self.view.zoom(),
            f64::from(self.view.width()),
        );
        let preview = self.preview.borrow();
        let Some(preview) = preview.as_ref() else {
            return;
        };
        let picture = &preview.picture;
        picture.set_page_size((pw, ph));
        picture.set_region(Some(region));
        picture.set_fixed_width(Some(width.round() as i32));
        picture.set_dark(self.view.is_dark());
        let label = self
            .state()
            .and_then(|s| {
                s.pdf
                    .get()
                    .and_then(|p| p.strip().map(|s| s.page_label(page)))
            })
            .unwrap_or_else(|| (page + 1).to_string());
        preview.caption.set_text(&format!("Page {label}"));
        match self.view.page_texture(page) {
            Some((texture, dark)) => {
                picture.set_page_texture(Some(&texture), dark);
                preview.want.set(None);
                self.want_thumbnail(None);
            }
            None => {
                picture.set_texture(None);
                let scale = self
                    .view
                    .native()
                    .and_then(|n| n.surface())
                    .map_or(1.0, |s| s.scale());
                let page_px = (pw * width / region.2 * scale).ceil().clamp(1.0, 4096.0) as u16;
                let req = ThumbReq {
                    generation: self.generation.get(),
                    page,
                    width: page_px,
                    dark: self.view.is_dark(),
                };
                preview.want.set(Some(req));
                self.want_thumbnail(Some(req));
            }
        }
        preview.shown.replace(Some(hit.clone()));
        preview.scroll.set(self.view.scroll_position());
        preview.popover.set_pointing_to(Some(&rect));
        preview.popover.popup();
    }

    fn want_thumbnail(&self, req: Option<ThumbReq>) {
        if let Some(state) = self.state()
            && let Some(pdf) = state.pdf.get()
        {
            pdf.set_thumbnail_wants(ThumbClient::Preview, req.into_iter().collect());
        }
    }

    /// A page rendered for a thumbnail request (`PdfEvent::Thumbnail`).
    pub(super) fn thumbnail(&self, req: ThumbReq, texture: &gdk::Texture) {
        let delivered = self.preview.borrow().as_ref().is_some_and(|preview| {
            let wanted = preview.want.get() == Some(req);
            if wanted {
                preview.want.set(None);
                preview.picture.set_page_texture(Some(texture), req.dark);
            }
            wanted
        });
        if delivered {
            self.want_thumbnail(None);
        }
    }

    fn hide_preview(&self) {
        stop(&self.leave_timer);
        stop(&self.hover_timer);
        let had_want = {
            let preview = self.preview.borrow();
            let Some(preview) = preview.as_ref() else {
                return;
            };
            if preview.popover.is_visible() {
                preview.popover.popdown();
            }
            preview.shown.replace(None);
            preview.picture.set_texture(None);
            preview.want.take().is_some()
        };
        self.in_popover.set(false);
        if had_want {
            self.want_thumbnail(None);
        }
    }

    // ----- following links -----

    /// Follows `link`: jumps to an internal destination, asks before opening a web address.
    fn follow(self: &Rc<Self>, link: &Link) {
        self.hide_preview();
        match &link.dest {
            LinkDest::Page { page, y_bp } => self.jump(*page, *y_bp),
            LinkDest::Uri(uri) => self.confirm_uri(uri),
            LinkDest::Other => {
                if let Some(state) = self.state() {
                    state.toast("This kind of link is not supported");
                }
            }
        }
    }

    /// Scrolls to `y_bp` of the 0-based `page` (its top when `None`), recording where the view
    /// was for Back.
    pub(super) fn jump(&self, page: u32, y_bp: Option<f64>) {
        if page >= self.view.page_count() {
            return;
        }
        if let Some(anchor) = self.view.anchor() {
            let mut back = self.back.borrow_mut();
            back.push(anchor);
            if back.len() > MAX_HISTORY {
                back.remove(0);
            }
            self.forward.borrow_mut().clear();
        }
        match y_bp {
            Some(y) => self.view.scroll_to_point(page, y, JUMP_MARGIN),
            None => self.view.goto_page(page),
        }
    }

    /// Returns to where the view was before the last link jump.
    fn go_back(&self) -> bool {
        let Some(anchor) = self.back.borrow_mut().pop() else {
            return false;
        };
        if let Some(here) = self.view.anchor() {
            self.forward.borrow_mut().push(here);
        }
        self.hide_preview();
        self.view.set_anchor(anchor);
        true
    }

    fn go_forward(&self) -> bool {
        let Some(anchor) = self.forward.borrow_mut().pop() else {
            return false;
        };
        if let Some(here) = self.view.anchor() {
            self.back.borrow_mut().push(here);
        }
        self.hide_preview();
        self.view.set_anchor(anchor);
        true
    }

    /// True when the keyboard focus is in the PDF pane's content (in whichever window holds it)
    /// and that window is the one in use.
    fn pdf_has_focus(&self, state: &WindowState) -> bool {
        let Some(root) = self.view.root() else {
            return false;
        };
        let Some(window) = root.downcast_ref::<gtk::Window>() else {
            return false;
        };
        let main = window == state.window.upcast_ref::<gtk::Window>();
        if !main && !window.is_active() {
            return false;
        }
        let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else {
            return false;
        };
        let content = state
            .pdf
            .get()
            .map(|p| p.content().clone().upcast::<gtk::Widget>());
        focus == *self.view.upcast_ref::<gtk::Widget>()
            || content.is_some_and(|c| focus.is_ancestor(&c))
    }

    /// Asks before opening `uri`; opens it with the system's handler on "Open".
    fn confirm_uri(self: &Rc<Self>, uri: &str) {
        let Some(state) = self.state() else {
            return;
        };
        if state.scripted && self.uri_hook.borrow().is_none() {
            state.toast("Links are not opened in runs that quit by themselves");
            return;
        }
        let dialog = adw::AlertDialog::builder()
            .heading("Open Link?")
            .body(uri)
            .close_response("cancel")
            .default_response("open")
            .build();
        dialog.add_responses(&[("cancel", "_Cancel"), ("open", "_Open Link")]);
        dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
        let weak = Rc::downgrade(self);
        let target = uri.to_string();
        dialog.connect_response(None, move |dialog, response| {
            let Some(links) = weak.upgrade() else {
                return;
            };
            if links.uri_dialog.borrow().as_ref() == Some(dialog) {
                links.uri_dialog.replace(None);
            }
            if response == "open" {
                links.open_uri(&target);
            }
        });
        self.uri_dialog.replace(Some(dialog.clone()));
        let parent: gtk::Widget = match self.view.root() {
            Some(root) => root.upcast(),
            None => state.window.clone().upcast(),
        };
        dialog.present(Some(&parent));
    }

    fn open_uri(&self, uri: &str) {
        if let Some(hook) = self.uri_hook.borrow().as_ref() {
            hook(uri);
            return;
        }
        let Some(state) = self.state() else {
            return;
        };
        let parent = self
            .view
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok());
        let weak = Rc::downgrade(&state);
        gtk::UriLauncher::new(uri).launch(
            parent.as_ref(),
            None::<&gio::Cancellable>,
            move |result| {
                if let (Err(e), Some(state)) = (result, weak.upgrade())
                    && !e.matches(gtk::DialogError::Dismissed)
                {
                    state.toast(&format!("Could not open the link: {e}"));
                }
            },
        );
    }
}

/// The region of a page of `size` (bp) the preview shows for a destination at `y_bp`, and the
/// preview's width in logical pixels: the page's width at the view's `zoom` (at most
/// [`MAX_PREVIEW_ZOOM`]), within [`MAX_PREVIEW_SIZE`] and 90 % of the view's width, and about a
/// quarter of the page's height with the destination a quarter of the way down.
pub fn preview_region(
    size: (f64, f64),
    y_bp: Option<f64>,
    zoom: f64,
    view_width: f64,
) -> ((f64, f64, f64, f64), f64) {
    let (pw, ph) = (size.0.max(1.0), size.1.max(1.0));
    let zoom = zoom.clamp(0.1, MAX_PREVIEW_ZOOM);
    let max_width = MAX_PREVIEW_SIZE.0.min((view_width * 0.9).max(160.0));
    let width = (pw * zoom).min(max_width);
    let height = (ph * zoom / 4.0).min(MAX_PREVIEW_SIZE.1);
    let (rw, rh) = (width / zoom, (height / zoom).min(ph));
    let rx = ((pw - rw) / 2.0).max(0.0);
    let ry = match y_bp {
        Some(y) => (y - rh / 4.0).clamp(0.0, (ph - rh).max(0.0)),
        None => 0.0,
    };
    ((rx, ry, rw, rh), width)
}

fn stop(timer: &RefCell<Option<glib::SourceId>>) {
    if let Some(id) = timer.take() {
        id.remove();
    }
}

fn links_of(state: &WindowState) -> Option<Rc<Links>> {
    state.pdf.get()?.links().cloned()
}

/// Called once per window after the M2 install hooks (after `pdf_controller::install`).
pub fn install(state: &Rc<WindowState>) {
    let Some(pdf) = state.pdf.get() else {
        return;
    };
    install_css_once("pdf-links", CSS);
    let view = pdf.view().clone();
    let links = Rc::new(Links {
        state: Rc::downgrade(state),
        view: view.clone(),
        generation: Cell::new(0),
        links: RefCell::new(HashMap::new()),
        requested: RefCell::new(HashSet::new()),
        hovered: RefCell::new(None),
        hover_timer: RefCell::new(None),
        leave_timer: RefCell::new(None),
        in_popover: Cell::new(false),
        preview: RefCell::new(None),
        back: RefCell::new(Vec::new()),
        forward: RefCell::new(Vec::new()),
        press: Cell::new(None),
        uri_hook: RefCell::new(None),
        uri_dialog: RefCell::new(None),
    });
    pdf.set_links(links.clone());

    let motion = gtk::EventControllerMotion::new();
    let weak = Rc::downgrade(&links);
    motion.connect_motion(move |_, x, y| {
        if let Some(links) = weak.upgrade() {
            links.pointer_moved(x, y);
        }
    });
    let weak = Rc::downgrade(&links);
    motion.connect_leave(move |_| {
        if let Some(links) = weak.upgrade() {
            links.pointer_left();
        }
    });
    view.add_controller(motion);

    // Clicks: any button, so the mouse's back and forward buttons work too.
    let click = gtk::GestureClick::builder().button(0).build();
    let weak = Rc::downgrade(&links);
    click.connect_pressed(move |gesture, presses, x, y| {
        let Some(links) = weak.upgrade() else {
            return;
        };
        let button = gesture.current_button();
        links.hide_preview();
        match button {
            8 => {
                links.go_back();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
            9 => {
                links.go_forward();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
            _ => {
                let modifiers = gesture.current_event_state()
                    & (gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK);
                let plain = button == 1 && presses == 1 && modifiers.is_empty();
                links.press.set(plain.then_some((button, x, y)));
            }
        }
    });
    let weak = Rc::downgrade(&links);
    click.connect_released(move |gesture, _, x, y| {
        let Some(links) = weak.upgrade() else {
            return;
        };
        let Some((_, px, py)) = links.press.take() else {
            return;
        };
        if (x - px).hypot(y - py) > CLICK_SLOP {
            return;
        }
        if let Some(hit) = links.hit(x, y) {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            links.follow(&hit.link);
        }
    });
    view.add_controller(click);

    // Keys hide the preview; Backspace goes back.
    let keys = gtk::EventControllerKey::new();
    let weak = Rc::downgrade(&links);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(links) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        links.hide_preview();
        if key == gdk::Key::BackSpace && modifiers.is_empty() && links.go_back() {
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    view.add_controller(keys);

    // Web links show their address.
    view.set_has_tooltip(true);
    let weak = Rc::downgrade(&links);
    view.connect_query_tooltip(move |_, x, y, keyboard, tooltip| {
        let Some(links) = weak.upgrade() else {
            return false;
        };
        if keyboard {
            return false;
        }
        let Some(hit) = links.hit(f64::from(x), f64::from(y)) else {
            return false;
        };
        let LinkDest::Uri(uri) = &hit.link.dest else {
            return false;
        };
        tooltip.set_text(Some(uri));
        if let Some(rect) = links.hit_rect(&hit) {
            tooltip.set_tip_area(&rect);
        }
        true
    });

    let weak = Rc::downgrade(&links);
    view.connect_changed(move |view| {
        if let Some(links) = weak.upgrade() {
            links.request_visible();
            // Scrolling or zooming moves the link away from the preview.
            let moved = links.preview.borrow().as_ref().is_some_and(|p| {
                p.popover.is_visible() && p.scroll.get() != view.scroll_position()
            });
            if moved {
                links.hide_preview();
            }
            if let Some(p) = links.preview.borrow().as_ref() {
                p.picture.set_dark(view.is_dark());
            }
        }
    });
    let weak = Rc::downgrade(&links);
    view.connect_unmap(move |_| {
        if let Some(links) = weak.upgrade() {
            links.hide_preview();
        }
    });

    let weak = Rc::downgrade(&links);
    goto::add_back_handler(state, move |state| {
        weak.upgrade()
            .filter(|links| links.pdf_has_focus(state))
            .is_some_and(|links| links.go_back())
    });
}

// ----- for tests -----

/// Routes opening web links to `hook` (with the address; it returns whether it "opened" it)
/// instead of `gtk::UriLauncher`; the confirmation is still asked. `None` restores the
/// launcher.
pub fn set_uri_hook(state: &WindowState, hook: Option<Box<UriHook>>) {
    if let Some(links) = links_of(state) {
        links.uri_hook.replace(hook);
    }
}

/// The "Open Link?" dialog while it is shown.
pub fn uri_dialog(state: &WindowState) -> Option<adw::AlertDialog> {
    links_of(state)?.uri_dialog.borrow().clone()
}

/// Answers the "Open Link?" dialog with `response` (`"open"` or `"cancel"`), as its buttons do.
pub fn answer_uri_dialog(state: &WindowState, response: &str) -> bool {
    let Some(dialog) = uri_dialog(state) else {
        return false;
    };
    dialog.emit_by_name::<()>("response", &[&response]);
    dialog.force_close();
    true
}

/// The links of `page` fetched so far (`None`: not fetched yet).
pub fn page_links(state: &WindowState, page: u32) -> Option<Vec<Link>> {
    links_of(state)?.links.borrow().get(&page).cloned()
}

/// What the pointer at `(x, y)` (view coordinates) does: moves over the view.
pub fn pointer_moved(state: &WindowState, x: f64, y: f64) {
    if let Some(links) = links_of(state) {
        links.pointer_moved(x, y);
    }
}

/// The pointer left the view.
pub fn pointer_left(state: &WindowState) {
    if let Some(links) = links_of(state) {
        links.pointer_left();
    }
}

/// A plain left click at `(x, y)` (view coordinates): follows the link there; false when there
/// is none.
pub fn click(state: &WindowState, x: f64, y: f64) -> bool {
    let Some(links) = links_of(state) else {
        return false;
    };
    match links.hit(x, y) {
        Some(hit) => {
            links.follow(&hit.link);
            true
        }
        None => false,
    }
}

/// Back from the last link jump (what Alt+Left does while the PDF has the focus).
pub fn back(state: &WindowState) -> bool {
    links_of(state).is_some_and(|l| l.go_back())
}

/// The number of link jumps Back can undo.
pub fn history_len(state: &WindowState) -> usize {
    links_of(state).map_or(0, |l| l.back.borrow().len())
}

/// The preview popover, its picture and the link (page and index) it shows, while shown.
pub fn preview(state: &WindowState) -> Option<(gtk::Popover, PagePicture, u32, usize)> {
    let links = links_of(state)?;
    let preview = links.preview.borrow();
    let preview = preview.as_ref()?;
    let shown = preview.shown.borrow().clone()?;
    preview.popover.is_visible().then(|| {
        (
            preview.popover.clone(),
            preview.picture.clone(),
            shown.page,
            shown.index,
        )
    })
}

/// True when the PDF has the keyboard focus in the window in use (Back then goes to the link
/// history).
pub fn pdf_has_focus(state: &WindowState) -> bool {
    links_of(state).is_some_and(|l| l.pdf_has_focus(state))
}
