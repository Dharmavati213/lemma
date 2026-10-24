//! Search in the PDF.
//!
//! A search bar below the PDF toolbar (Ctrl+F while the PDF has the focus, `/` in the view, or
//! the toolbar's search button): the query, the match count ("3 of 17"), previous and next,
//! and a menu with Match Case and Whole Words. Matching is case-insensitive by default, ignores
//! diacritics when the query has none, and spans line breaks (poppler's `find_text`).
//!
//! - The PDF thread searches page by page from the page shown on, wrapping around
//!   (`PdfCmd::Search`), between page renders; each new query replaces the search in progress
//!   (latest wins) and closing the bar cancels it.
//! - Every match on the pages on screen is highlighted; the current one is emphasised and
//!   scrolled into view (to a third of the view height when it was hidden). The first match
//!   at or below the top of the view becomes current as soon as it is found.
//! - **Enter** / **Shift+Enter** (also Ctrl+G / Ctrl+Shift+G) in the entry, and **n** / **N**
//!   in the view while the bar is shown, go to the next and previous match, wrapping around.
//!   **Escape** (in the entry or the view) closes the bar, removes the highlights and gives the
//!   focus back to the view.
//! - **Vim style:** opened with `/` from the view (as in zathura or less), Enter runs the search
//!   and gives the focus back to the view, where `n` / `N` go on; opened with Ctrl+F or the
//!   button, Enter goes to the next match and the focus stays in the entry (GNOME style).
//! - A rebuilt PDF is searched again with the same query, without moving the view: the match
//!   that was current stays current while the new PDF still has it, else the first match at or
//!   below the top of the view becomes current once every page has been searched. Only the
//!   searches the user starts (and Enter, n, N) scroll to their match, also when the PDF is
//!   rebuilt before they found one.
//!
//! Hooks: [`install`] once per window (after `pdf_controller::install`); [`show`] is
//! `win.pdf-search`. The shell enables that action only while the keyboard focus is inside
//! `state.pdf_pane`; a detached PDF window (`pdf::detach`) has its own Ctrl+F. [`run_demo`] is
//! `--demo pdf-search`.

use super::pdf_controller::PdfController;
use super::pdf_view::{PageRect, PdfView};
use super::{detach, sidebar};
use crate::build_ui::install_css_once;
use crate::window::WindowState;
use adw::prelude::*;
use gtk::gdk;
use lemma_pdf::{PdfCmd, SearchMatch, SearchOptions};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

const CSS: &str = "
.lemma-pdf-search-bar .lemma-pdf-search-count { font-feature-settings: 'tnum'; }
";

/// The search of one window's PDF pane (registered with the controller).
pub struct Search {
    state: Weak<WindowState>,
    view: PdfView,
    bar: gtk::SearchBar,
    entry: gtk::SearchEntry,
    count: gtk::Label,
    spinner: adw::Spinner,
    previous: gtk::Button,
    next: gtk::Button,
    case_sensitive: gtk::CheckButton,
    whole_words: gtk::CheckButton,
    /// The search shown (or in progress).
    query: RefCell<Option<Query>>,
    last_id: Cell<u64>,
    /// The number of matches of each page with matches (their rectangles are in the view).
    counts: RefCell<BTreeMap<u32, usize>>,
    /// The page the search started on and the top of the view on it (bp), for choosing the
    /// first current match.
    origin: Cell<(u32, f64)>,
    /// Opened with `/` from the view: Enter returns to the view (see the module docs).
    vim_style: Cell<bool>,
}

/// One search sent to the PDF thread.
#[derive(Clone, Debug, PartialEq)]
struct Query {
    id: u64,
    text: String,
    options: SearchOptions,
    /// Every page has been searched.
    done: bool,
    /// Run again for a new generation, not by the user: it never scrolls the view.
    rerun: bool,
    /// For a re-run on a rebuilt PDF, the match (page, index) that was current before: it is
    /// made current again when the new generation has it.
    kept: Option<(u32, usize)>,
}

impl Search {
    fn send(&self, cmd: PdfCmd) {
        if let Some(state) = self.state.upgrade()
            && let Some(pdf) = state.pdf.get()
        {
            pdf.send(cmd);
        }
    }

    fn options(&self) -> SearchOptions {
        SearchOptions {
            case_sensitive: self.case_sensitive.is_active(),
            whole_words: self.whole_words.is_active(),
        }
    }

    /// Searches for `text` (cancels the search shown when it is blank).
    fn start(&self, text: &str) {
        self.run(text, false, None);
    }

    /// Searches for `text` as [`Search::start`] does; `rerun` and `kept` as in [`Query`].
    fn run(&self, text: &str, rerun: bool, kept: Option<(u32, usize)>) {
        let options = self.options();
        if self
            .query
            .borrow()
            .as_ref()
            .is_some_and(|q| q.text == text && q.options == options)
        {
            return;
        }
        self.reset();
        if text.trim().is_empty() || self.view.page_count() == 0 {
            if !text.trim().is_empty() {
                // No document yet: searched when one opens.
                self.query.replace(Some(Query {
                    id: 0,
                    text: text.to_string(),
                    options,
                    done: true,
                    rerun: false,
                    kept: None,
                }));
            }
            self.refresh_count();
            return;
        }
        let id = self.last_id.get() + 1;
        self.last_id.set(id);
        let (start, top) = self.view_top();
        self.origin.set((start, top));
        self.query.replace(Some(Query {
            id,
            text: text.to_string(),
            options,
            done: false,
            rerun,
            kept,
        }));
        self.send(PdfCmd::Search {
            id,
            query: text.to_string(),
            options,
            start,
        });
        self.refresh_count();
    }

    /// The page the view shows and the top of the view on it (bp; 0 when the view's top is
    /// on another page).
    fn view_top(&self) -> (u32, f64) {
        let page = self.view.current_page();
        let top = match self.view.anchor() {
            Some(a) if a.page == page => self
                .view
                .layout()
                .and_then(|l| l.page_size(page))
                .map_or(0.0, |(_, h)| a.frac.max(0.0) * h),
            _ => 0.0,
        };
        (page, top)
    }

    /// The first match at or below the top of the view, else the first on a later page,
    /// wrapping around.
    fn first_from_view(&self) -> Option<(u32, usize)> {
        let (page, top) = self.view_top();
        let below = self
            .view
            .search_matches(page)
            .iter()
            .position(|m| m.first().is_some_and(|r| r.1 + r.3 > top));
        if let Some(index) = below {
            return Some((page, index));
        }
        let counts = self.counts.borrow();
        let next = counts
            .range(page + 1..)
            .next()
            .or_else(|| counts.iter().next());
        next.map(|(&p, _)| (p, 0))
    }

    /// Forgets the search shown: cancels it and removes the highlights.
    fn reset(&self) {
        if self.query.take().is_some_and(|q| !q.done) {
            self.send(PdfCmd::CancelSearch);
        }
        self.counts.borrow_mut().clear();
        self.view.clear_search_matches();
    }

    /// Matches of the search `id` on `page` (`PdfEvent::SearchPage`).
    pub(super) fn page_results(
        &self,
        id: u64,
        generation: u64,
        page: u32,
        matches: Vec<SearchMatch>,
    ) {
        let Some((rerun, kept)) = self
            .query
            .borrow()
            .as_ref()
            .filter(|q| q.id == id)
            .map(|q| (q.rerun, q.kept))
        else {
            return;
        };
        if Some(generation) != self.view.generation() || matches.is_empty() {
            return;
        }
        let n = matches.len();
        let first = {
            let (start, top) = self.origin.get();
            if page == start {
                matches
                    .iter()
                    .position(|m| m.first().is_some_and(|r| r.1 + r.3 > top))
                    .unwrap_or(0)
            } else {
                0
            }
        };
        self.view.set_search_matches(page, matches);
        self.counts.borrow_mut().insert(page, n);
        if self.view.current_match().is_some() {
            self.refresh_count();
        } else if !rerun {
            self.select(page, first);
        } else {
            // A re-run: the match that was current, in place, if this page still has it.
            if kept.is_some_and(|(p, index)| p == page && index < n) {
                self.view.set_current_match(kept);
            }
            self.refresh_count();
        }
    }

    /// Every page has been searched for `id` (`PdfEvent::SearchDone`).
    pub(super) fn search_done(&self, id: u64, _generation: u64) {
        let rerun = match self.query.borrow_mut().as_mut().filter(|q| q.id == id) {
            Some(q) => {
                q.done = true;
                q.rerun
            }
            None => false,
        };
        // A re-run whose match is gone: the first from the view on, in place.
        if rerun && self.view.current_match().is_none() {
            self.view.set_current_match(self.first_from_view());
        }
        self.refresh_count();
    }

    /// A new generation (`rebuilt`) or another PDF is shown: the query is searched again,
    /// without scrolling (see the module docs).
    pub(super) fn document_changed(&self, _generation: u64, rebuilt: bool) {
        let query = self.query.take();
        if let Some(query) = query {
            let current = self.view.current_match();
            // A search the user started that has not found a match yet still goes to it.
            let rerun = query.rerun || query.done || current.is_some();
            // A re-run still waiting for its match goes on waiting for it.
            let waiting = query.kept.filter(|_| query.rerun && !query.done);
            let kept = current.or(waiting).filter(|_| rebuilt);
            self.counts.borrow_mut().clear();
            self.view.clear_search_matches();
            self.run(&query.text, rerun, kept);
        }
    }

    /// Makes match `index` of `page` current and scrolls it into view.
    fn select(&self, page: u32, index: usize) {
        self.view.set_current_match(Some((page, index)));
        if let Some(rect) = match_bounds(&self.view, page, index) {
            self.view.reveal(page, rect);
        }
        self.refresh_count();
    }

    /// Goes to the next (or previous) match, wrapping around; the first at or after the page
    /// shown when there is no current one.
    fn step(&self, forward: bool) -> bool {
        let target = {
            let counts = self.counts.borrow();
            if counts.is_empty() {
                None
            } else {
                match self.view.current_match() {
                    Some((page, index)) => neighbour(&counts, page, index, forward),
                    None => {
                        let page = self.view.current_page();
                        let after = if forward {
                            counts.range(page..).next().or_else(|| counts.iter().next())
                        } else {
                            counts
                                .range(..=page)
                                .next_back()
                                .or_else(|| counts.iter().next_back())
                        };
                        after.map(|(&p, &n)| (p, if forward { 0 } else { n - 1 }))
                    }
                }
            }
        };
        match target {
            Some((page, index)) => {
                self.select(page, index);
                true
            }
            None => {
                self.entry.error_bell();
                false
            }
        }
    }

    /// Updates the "3 of 17" label, the spinner, the entry's error style and the buttons.
    fn refresh_count(&self) {
        let query = self.query.borrow().clone();
        let counts = self.counts.borrow();
        let total: usize = counts.values().sum();
        let searching = query.as_ref().is_some_and(|q| !q.done);
        let ordinal = self
            .view
            .current_match()
            .map(|(page, index)| counts.range(..page).map(|(_, n)| n).sum::<usize>() + index + 1);
        let text = match (&query, total) {
            (None, _) => String::new(),
            (Some(_), 0) if searching => String::new(),
            (Some(_), 0) => "No matches".to_string(),
            (Some(_), _) => match ordinal {
                Some(i) => format!("{i} of {total}"),
                None => format!("{total} matches"),
            },
        };
        self.count.set_text(&text);
        self.count.set_visible(!text.is_empty());
        self.spinner.set_visible(searching);
        let none = query.as_ref().is_some_and(|q| q.done) && total == 0;
        if none {
            self.entry.add_css_class("error");
        } else {
            self.entry.remove_css_class("error");
        }
        self.previous.set_sensitive(total > 0);
        self.next.set_sensitive(total > 0);
    }

    /// Opens the bar (if needed) and focuses the entry with its text selected; `vim_style`:
    /// opened with `/` (see the module docs).
    fn open(&self, vim_style: bool) {
        self.vim_style.set(vim_style);
        self.bar.set_search_mode(true);
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Closing the bar ends the search.
    fn closed(&self) {
        self.reset();
        self.refresh_count();
        if self.view.is_mapped() {
            self.view.grab_focus();
        }
    }
}

/// The match after (or before) match `index` of `page`, wrapping around.
fn neighbour(
    counts: &BTreeMap<u32, usize>,
    page: u32,
    index: usize,
    forward: bool,
) -> Option<(u32, usize)> {
    let here = counts.get(&page).copied().unwrap_or(0);
    if forward {
        if index + 1 < here {
            return Some((page, index + 1));
        }
        let next = counts
            .range(page + 1..)
            .next()
            .or_else(|| counts.iter().next())?;
        Some((*next.0, 0))
    } else {
        if index > 0 && index <= here {
            return Some((page, index - 1));
        }
        let previous = counts
            .range(..page)
            .next_back()
            .or_else(|| counts.iter().next_back())?;
        Some((*previous.0, previous.1 - 1))
    }
}

/// The bounding box (bp) of match `index` of `page`.
fn match_bounds(view: &PdfView, page: u32, index: usize) -> Option<PageRect> {
    let matches = view.search_matches(page);
    let parts = matches.get(index)?;
    let x0 = parts.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
    let y0 = parts.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let x1 = parts
        .iter()
        .map(|r| r.0 + r.2)
        .fold(f64::NEG_INFINITY, f64::max);
    let y1 = parts
        .iter()
        .map(|r| r.1 + r.3)
        .fold(f64::NEG_INFINITY, f64::max);
    x0.is_finite().then_some((x0, y0, x1 - x0, y1 - y0))
}

fn search_of(state: &WindowState) -> Option<Rc<Search>> {
    state.pdf.get()?.search().cloned()
}

/// Called once per window after the M2 install hooks: builds the search bar below the PDF
/// toolbar and registers it with the controller.
pub fn install(state: &Rc<WindowState>) {
    let Some(pdf) = state.pdf.get() else {
        return;
    };
    install_css_once("pdf-search", CSS);
    let view = pdf.view().clone();
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search in PDF")
        .hexpand(true)
        .width_chars(8)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Search in PDF")]);
    let count = gtk::Label::builder()
        .css_classes(["dim-label", "numeric", "caption", "lemma-pdf-search-count"])
        .visible(false)
        .build();
    let spinner = adw::Spinner::builder()
        .width_request(16)
        .height_request(16)
        .visible(false)
        .build();
    let nav_button = |icon: &str, tooltip: &str| {
        gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .sensitive(false)
            .build()
    };
    let previous = nav_button("go-up-symbolic", "Previous Match (Shift+Enter)");
    let next = nav_button("go-down-symbolic", "Next Match (Enter)");
    let case_sensitive = gtk::CheckButton::with_mnemonic("Match _Case");
    let whole_words = gtk::CheckButton::with_mnemonic("_Whole Words");
    let options_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    options_box.append(&case_sensitive);
    options_box.append(&whole_words);
    let options = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Search Options")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .popover(&gtk::Popover::builder().child(&options_box).build())
        .build();
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    row.append(&entry);
    row.append(&spinner);
    row.append(&count);
    row.append(&previous);
    row.append(&next);
    row.append(&options);
    let bar = gtk::SearchBar::builder()
        .child(&row)
        .show_close_button(true)
        .css_classes(["lemma-pdf-search-bar"])
        .build();
    bar.connect_entry(&entry);
    pdf.content().insert_child_after(&bar, Some(pdf.toolbar()));
    pdf.search_button()
        .bind_property("active", &bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();

    let search = Rc::new(Search {
        state: Rc::downgrade(state),
        view: view.clone(),
        bar: bar.clone(),
        entry: entry.clone(),
        count,
        spinner,
        previous: previous.clone(),
        next: next.clone(),
        case_sensitive: case_sensitive.clone(),
        whole_words: whole_words.clone(),
        query: RefCell::new(None),
        last_id: Cell::new(0),
        counts: RefCell::new(BTreeMap::new()),
        origin: Cell::new((0, 0.0)),
        vim_style: Cell::new(false),
    });
    pdf.set_search(search.clone());
    connect(&search, &entry, &bar, &previous, &next);
    for check in [&case_sensitive, &whole_words] {
        let weak = Rc::downgrade(&search);
        check.connect_toggled(move |_| {
            if let Some(s) = weak.upgrade() {
                let text = s.entry.text().to_string();
                s.start(&text);
            }
        });
    }
    install_view_keys(state, &view);
}

fn connect(
    search: &Rc<Search>,
    entry: &gtk::SearchEntry,
    bar: &gtk::SearchBar,
    previous: &gtk::Button,
    next: &gtk::Button,
) {
    let weak = Rc::downgrade(search);
    entry.connect_search_changed(move |entry| {
        if let Some(s) = weak.upgrade() {
            s.start(&entry.text());
        }
    });
    // Enter: the query typed (at once, without the entry's delay), else the next match.
    let weak = Rc::downgrade(search);
    entry.connect_activate(move |entry| {
        if let Some(s) = weak.upgrade() {
            let text = entry.text().to_string();
            let options = s.options();
            let fresh = s
                .query
                .borrow()
                .as_ref()
                .is_some_and(|q| q.text == text && q.options == options);
            if !fresh {
                s.start(&text);
            } else if !s.vim_style.get() {
                s.step(true);
            }
            if s.vim_style.get() && !text.trim().is_empty() {
                s.view.grab_focus();
            }
        }
    });
    let weak = Rc::downgrade(search);
    entry.connect_next_match(move |_| {
        if let Some(s) = weak.upgrade() {
            s.step(true);
        }
    });
    let weak = Rc::downgrade(search);
    entry.connect_previous_match(move |_| {
        if let Some(s) = weak.upgrade() {
            s.step(false);
        }
    });
    let weak = Rc::downgrade(search);
    entry.connect_stop_search(move |_| {
        if let Some(s) = weak.upgrade() {
            s.bar.set_search_mode(false);
        }
    });
    // Shift+Enter: the previous match (the entry's own Enter has no Shift variant).
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(search);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let enter = matches!(
            key,
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter
        );
        if enter
            && modifiers.contains(gdk::ModifierType::SHIFT_MASK)
            && let Some(s) = weak.upgrade()
        {
            s.step(false);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    entry.add_controller(keys);
    let weak = Rc::downgrade(search);
    previous.connect_clicked(move |_| {
        if let Some(s) = weak.upgrade() {
            s.step(false);
        }
    });
    let weak = Rc::downgrade(search);
    next.connect_clicked(move |_| {
        if let Some(s) = weak.upgrade() {
            s.step(true);
        }
    });
    let weak = Rc::downgrade(search);
    bar.connect_search_mode_enabled_notify(move |bar| {
        if let Some(s) = weak.upgrade() {
            if bar.is_search_mode() {
                // The bar gives its entry the focus as it opens.
                let text = s.entry.text().to_string();
                s.start(&text);
            } else {
                s.closed();
            }
        }
    });
}

/// What a key of the view does; true when it handled the key.
type KeyFn = Rc<dyn Fn(&Rc<WindowState>) -> bool>;

/// `/` opens the search from the view; `n` / `N` go to the next and previous match and Escape
/// closes the search while the bar is shown (Vim's keys; otherwise they do nothing in the view).
fn install_view_keys(state: &Rc<WindowState>, view: &PdfView) {
    let keys = gtk::ShortcutController::new();
    let weak = Rc::downgrade(state);
    let add = |trigger: &str, run: KeyFn| {
        let weak = weak.clone();
        let action = gtk::CallbackAction::new(move |_, _| match weak.upgrade() {
            Some(state) if run(&state) => glib::Propagation::Stop,
            _ => glib::Propagation::Proceed,
        });
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(action),
        ));
    };
    add(
        "slash|KP_Divide",
        Rc::new(|state| {
            open_search(state, true);
            true
        }),
    );
    add(
        "Escape",
        Rc::new(|state| {
            let open = search_of(state).filter(|s| s.bar.is_search_mode());
            if let Some(s) = &open {
                s.bar.set_search_mode(false);
            }
            open.is_some()
        }),
    );
    let step = |forward: bool| -> KeyFn {
        Rc::new(move |state| {
            search_of(state)
                .filter(|s| s.bar.is_search_mode() && !s.counts.borrow().is_empty())
                .is_some_and(|s| s.step(forward))
        })
    };
    add("n", step(true));
    add("<Shift>n|N", step(false));
    view.add_controller(keys);
}

/// `win.pdf-search` (Ctrl+F while the PDF has the focus): opens the search bar.
pub fn show(state: &Rc<WindowState>) {
    open_search(state, false);
}

/// Opens the search bar with the focus in its entry (`vim_style`: opened with `/`).
fn open_search(state: &Rc<WindowState>, vim_style: bool) {
    let Some(search) = search_of(state) else {
        return;
    };
    if state.pdf.get().is_some_and(|p| p.pdf().is_none()) {
        state.toast("There is no PDF to search yet; build with F5");
        return;
    }
    if detach::is_detached(state) {
        detach::present(state);
    } else {
        state.pdf_pane.set_visible(true);
    }
    // Once the event that asked is handled (the bar takes the focus when it opens): an action
    // run from the command palette or a menu runs while the focus is being given back to the
    // window, and the action's other handlers still see the focus where the user left it.
    let search = Rc::downgrade(&search);
    glib::idle_add_local_once(move || {
        if let Some(search) = search.upgrade() {
            search.open(vim_style);
        }
    });
}

// ----- for tests and the demo -----

/// Searches for `text` at once (opens the bar, sets the entry, starts without its delay).
pub fn search_now(state: &Rc<WindowState>, text: &str) {
    let Some(search) = search_of(state) else {
        return;
    };
    search.bar.set_search_mode(true);
    search.entry.set_text(text);
    search.start(text);
}

/// Goes to the next (`true`) or previous match, as Enter and Shift+Enter do.
pub fn step(state: &WindowState, forward: bool) -> bool {
    search_of(state).is_some_and(|s| s.step(forward))
}

/// Sets Match Case and Whole Words (searches again when they change).
pub fn set_options(state: &WindowState, options: SearchOptions) {
    if let Some(s) = search_of(state) {
        s.case_sensitive.set_active(options.case_sensitive);
        s.whole_words.set_active(options.whole_words);
    }
}

/// Closes the search bar (as Escape does).
pub fn close(state: &WindowState) {
    if let Some(s) = search_of(state) {
        s.bar.set_search_mode(false);
    }
}

/// The search's state: whether the bar is shown, the query, whether every page has been
/// searched, the total number of matches and the 1-based ordinal of the current one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub open: bool,
    pub query: Option<String>,
    pub done: bool,
    pub total: usize,
    pub current: Option<usize>,
    /// The label shown ("3 of 17", "No matches").
    pub label: String,
}

/// See [`Status`].
pub fn status(state: &WindowState) -> Option<Status> {
    let s = search_of(state)?;
    let query = s.query.borrow().clone();
    let counts = s.counts.borrow();
    Some(Status {
        open: s.bar.is_search_mode(),
        query: query.as_ref().map(|q| q.text.clone()),
        done: query.as_ref().is_some_and(|q| q.done),
        total: counts.values().sum(),
        current: s
            .view
            .current_match()
            .map(|(page, index)| counts.range(..page).map(|(_, n)| n).sum::<usize>() + index + 1),
        label: if s.count.is_visible() {
            s.count.text().to_string()
        } else {
            String::new()
        },
    })
}

/// The search entry (tests type into it and check the focus).
pub fn entry(state: &WindowState) -> Option<gtk::SearchEntry> {
    search_of(state).map(|s| s.entry.clone())
}

// ----- the demo -----

/// `LEMMA_DEMO_PDF_QUERY`: what `--demo pdf-search` searches for (default [`DEMO_QUERY`]; empty:
/// no search).
pub const QUERY_ENV: &str = "LEMMA_DEMO_PDF_QUERY";
/// `LEMMA_DEMO_PDF_STRIP`: `thumbnails` or `bookmarks` shows the PDF strip on that page.
pub const STRIP_ENV: &str = "LEMMA_DEMO_PDF_STRIP";
/// The demo's default query (several matches on the first page of `pdf50`).
pub const DEMO_QUERY: &str = "nulla";
const DEMO_TIMEOUT: Duration = Duration::from_secs(15);

/// `--demo pdf-search` (run with FILE = a built document, e.g. `target/fixtures/pdf50.tex`):
/// shows the PDF of its last build, searches for [`DEMO_QUERY`] and resolves when every page
/// has been searched and the current match is drawn. Knobs: [`QUERY_ENV`], [`STRIP_ENV`] (the
/// strip's page; the demo then also waits for the thumbnails on screen or the bookmarks).
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    use crate::testmode::{wait_for_frames, wait_until};
    let build = state.build.get().ok_or("there is no build controller")?;
    let controller: &PdfController = state.pdf.get().ok_or("there is no PDF preview")?;
    let pdf = build
        .show_existing_pdf()
        .await
        .map_err(|e| format!("no PDF to show ({e}); build the document first"))?;
    let view = controller.view().clone();
    wait_until(DEMO_TIMEOUT, "the PDF to open", || {
        controller.pdf().as_deref() == Some(pdf.as_path())
            && view.page_count() > 0
            && view.render_scale_q() > 0
    })
    .await?;
    wait_for_frames(&state.window, 1).await;

    let strip = std::env::var(STRIP_ENV).ok().filter(|s| !s.is_empty());
    if let Some(page) = &strip {
        let page = match page.as_str() {
            "thumbnails" => sidebar::Page::Thumbnails,
            "bookmarks" => sidebar::Page::Bookmarks,
            other => return Err(format!("{STRIP_ENV}={other}: use thumbnails or bookmarks")),
        };
        state.pdf_sidebar.split.set_show_sidebar(true);
        sidebar::show_page(&state, page);
    }
    let query = std::env::var(QUERY_ENV).unwrap_or_else(|_| DEMO_QUERY.to_string());
    let mut summary = format!(
        "demo pdf-search: {} ({} pages)",
        pdf.display(),
        view.page_count()
    );
    if !query.is_empty() {
        search_now(&state, &query);
        wait_until(DEMO_TIMEOUT, "the search to finish", || {
            status(&state).is_some_and(|s| s.done)
        })
        .await?;
        let s = status(&state).ok_or("no search")?;
        if s.total == 0 {
            return Err(format!("{query:?} was not found"));
        }
        wait_until(DEMO_TIMEOUT, "the pages to render", || {
            let visible = view.visible_pages();
            !visible.is_empty() && visible.clone().all(|p| view.is_page_rendered(p))
        })
        .await?;
        let (page, _) = view.current_match().ok_or("no current match")?;
        summary += &format!(
            "; {query:?}: {} matches on {} pages, {} on page {}",
            s.total,
            view.pages_with_matches().len(),
            s.label,
            page + 1
        );
    }
    if strip.is_some() {
        wait_until(DEMO_TIMEOUT, "the strip to fill", || {
            sidebar::is_ready(&state)
        })
        .await?;
        summary += &format!("; strip: {}", sidebar::describe(&state));
    }
    // Two frames so the highlights and thumbnails are drawn.
    wait_for_frames(&state.window, 2).await;
    println!("{summary}");
    Ok(())
}
