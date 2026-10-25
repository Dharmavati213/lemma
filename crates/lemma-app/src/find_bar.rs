//! Find and replace in the current file: a search bar between the tab bar and the editor.
//!
//! # Finding
//!
//! `win.find` (Ctrl+F) opens the bar with the focus in its entry, prefilled with the active
//! editor's selection when that is one line (escaped while Regular Expression is on; the match
//! the open bar selected, and Ctrl+F in the bar, keep the query). The entry searches as you
//! type: every match in the file is highlighted, the first match at or after the start of the
//! selection is selected and scrolled into view once the search has counted them, and the label
//! says "3 of 17" while the selection is a match, else "17 matches" or "No matches". Match Case
//! (Alt+C), Whole Word (Alt+W) and Regular Expression (Alt+R; GLib's, which is PCRE) are
//! toggles, as on the Search page. **Enter** / **Shift+Enter** in the entry, the arrow buttons,
//! and **F3** / **Shift+F3** (`win.find-next`, `win.find-prev`) go to the next and previous
//! match, wrapping around; F3 opens a closed bar again on the same query, leaving the focus in
//! the editor. **Escape** anywhere in the bar (or its close button) closes it, removes the
//! highlights and gives the focus back to the editor. The query and the toggles stay for the
//! next time.
//!
//! The search is GtkSourceView's (`SearchContext` with the window's `SearchSettings`), which
//! scans the buffer in idle steps and follows its edits, so the highlights and the count keep up
//! with typing in the editor while the bar is shown. A new query waits for that scan before it
//! selects its first match ([`FindBar::scanned`]): asking the context at once would scan the
//! rest of the file in one go when nothing matches (about 115 ms on 60 000 lines, at every pause
//! in typing the query). For the same reason a step (F3, Enter, …) before that looks for its
//! match in the background, and the steps and Replaces that follow it wait for it
//! ([`FindBar::step`]). GTK keeps one primary selection at a time, so selecting the entry's text
//! takes the editor's selection away: the match the bar selected last still counts as selected
//! while the cursor stays at its start and the text is unchanged ([`Found`]).
//!
//! The rows wrap (the toggles, then the replace buttons, onto lines of their own) rather than
//! make the editor wider than its minimum.
//!
//! # Replacing
//!
//! `win.find-replace` (Ctrl+H) opens the bar with its replace row (Ctrl+H in the bar hides the
//! row again, as on the Search page); the focus goes to the replace entry when there is a query.
//! **Replace** (Enter in the replace entry) replaces the selected match and selects the next one;
//! when the selection is no match it only goes to the next one. **Replace All** (Ctrl+Enter in the
//! replace entry) replaces every match in the file as one undo step and says how many in a toast.
//! With a regular expression the replacement takes `$1`, `${name}` and `$$` as on the Search page
//! (`lemma_search::Template`, written in GLib's syntax for GtkSourceView), and backslashes are
//! never escapes (`\textbf{$1}` stays LaTeX). Both are ordinary edits of the buffer
//! (`Origin::User`): a review treats them as it treats typing. Live environment rename stays out
//! of them (`envtools::without_live_rename`), so only the matches change: replacing the name in
//! `\begin{theorem}` does not rename its `\end`.
//!
//! # Lifetime
//!
//! One bar per window, in `state.find_slot`. While it is shown, one `SearchContext` searches the
//! active editor's buffer: switching tabs moves the search to the new tab, and closing the bar,
//! the tab or the window drops the context (its highlights go with it) and disconnects its
//! handlers. No context exists while the bar is closed.
//!
//! The shell enables the four actions only while an editor or the bar has the keyboard focus and
//! Vim is off (`WindowState::focus_moved`): Ctrl+F is also `win.pdf-search` while the PDF has the
//! focus, and with Vim on Ctrl+F and Ctrl+H are Vim's own keys (Vim searches with `/`).
//!
//! Hooks: [`install`] once per window; [`detach`] when a tab closes; [`show`], [`show_replace`]
//! and [`step`] are the actions. Tests use [`bar`] and the methods of [`FindBar`].

use crate::build_ui::install_css_once;
use crate::editor::{Editor, envtools};
use crate::search_panel;
use crate::window::WindowState;
use adw::prelude::*;
use gtk::gdk;
use lemma_search::Template;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const CSS: &str = "
.lemma-find-bar .lemma-find-count { font-feature-settings: 'tnum'; margin: 0 4px; }
.lemma-find-bar .lemma-find-replace { margin-top: 6px; }
.lemma-find-bar button.lemma-find-option { min-width: 30px; min-height: 26px; padding: 0 5px; }
.lemma-find-bar button.lemma-find-option label { font-weight: 700; font-size: 0.9em; }
";

/// The entries' smallest width in characters (the rows wrap before they get narrower).
const ENTRY_MIN_CHARS: i32 = 6;
/// The entries' natural width in characters; they grow with the editor.
const ENTRY_CHARS: i32 = 28;

/// The find bar of one window.
pub struct FindBar {
    state: Weak<WindowState>,
    bar: gtk::SearchBar,
    entry: gtk::SearchEntry,
    count: gtk::Label,
    previous: gtk::Button,
    next: gtk::Button,
    case: gtk::ToggleButton,
    word: gtk::ToggleButton,
    regex: gtk::ToggleButton,
    replace_toggle: gtk::ToggleButton,
    replace_revealer: gtk::Revealer,
    replace: gtk::Entry,
    replace_one: gtk::Button,
    replace_all: gtk::Button,
    /// The query and the toggles, shared by the searches of every tab.
    settings: sourceview5::SearchSettings,
    /// The search in the active editor while the bar is shown.
    target: RefCell<Option<Target>>,
    /// The replace toggle is set from code: the focus stays.
    quiet_toggle: Cell<bool>,
}

/// The search of one editor's buffer.
struct Target {
    editor: Weak<Editor>,
    context: sourceview5::SearchContext,
    /// Updates the count when the selection moves (the context's own handlers go with it).
    mark_set: Option<(glib::WeakRef<sourceview5::Buffer>, glib::SignalHandlerId)>,
    /// The match the bar selected last (see [`Found`]).
    found: Cell<Option<Found>>,
    /// Where a new query's first match is looked for once the search has counted
    /// ([`FindBar::scanned`]).
    pending: Cell<Option<Start>>,
    /// The search has counted the matches of the query since it was set: a step finds its
    /// match at once from then on (see [`FindBar::step`]).
    counted: Cell<bool>,
    /// The step that waits for the search to scan as far as its match (see [`FindBar::step`]).
    stepping: RefCell<Option<Stepping>>,
}

/// A step looking for its match in the background (see [`FindBar::step`]).
struct Stepping {
    /// Cancelled when the search ends or the query changes; GtkSourceView cancels it too when
    /// the buffer changes.
    cancellable: gio::Cancellable,
    /// What was asked for meanwhile, in order; it follows the step.
    queued: Vec<Queued>,
}

/// A step or Replace asked for while a step looks for its match in the background.
#[derive(Clone, Copy, Debug)]
enum Queued {
    /// `true`: forward.
    Step(bool),
    /// Replaces the match the steps before it selected (see [`FindBar::replace_one`]).
    Replace,
}

/// A buffer character offset and the document version it was taken in.
#[derive(Clone, Copy, Debug)]
struct Start {
    version: u64,
    offset: i32,
}

/// A match the bar selected (buffer character offsets) and the document version it was found
/// in. Selecting the entry's text takes the editor's selection away (GTK keeps one primary
/// selection at a time), so while the text is unchanged and the cursor still at its start it
/// counts as selected: the next step goes past it, Replace replaces it, the label counts it.
#[derive(Clone, Copy, Debug)]
struct Found {
    version: u64,
    start: i32,
    end: i32,
}

impl Target {
    fn new(find: &Rc<FindBar>, editor: &Rc<Editor>) -> Self {
        let buffer = editor.buffer();
        let context = sourceview5::SearchContext::new(buffer, Some(&find.settings));
        context.set_highlight(true);
        let weak = Rc::downgrade(find);
        context.connect_occurrences_count_notify(move |_| {
            if let Some(find) = weak.upgrade() {
                find.scanned();
            }
        });
        let weak = Rc::downgrade(find);
        context.connect_regex_error_notify(move |_| {
            if let Some(find) = weak.upgrade() {
                find.refresh_count();
            }
        });
        let weak = Rc::downgrade(find);
        let mark_set = buffer.connect_mark_set(move |buffer, _, mark| {
            if (*mark == buffer.get_insert() || *mark == buffer.selection_bound())
                && let Some(find) = weak.upgrade()
            {
                find.refresh_count();
            }
        });
        Self {
            editor: Rc::downgrade(editor),
            context,
            mark_set: Some((buffer.downgrade(), mark_set)),
            found: Cell::new(None),
            pending: Cell::new(None),
            counted: Cell::new(false),
            stepping: RefCell::new(None),
        }
    }

    /// Forgets the step looking for its match in the background, and the steps queued behind
    /// it.
    fn cancel_stepping(&self) {
        if let Some(stepping) = self.stepping.take() {
            stepping.cancellable.cancel();
        }
    }

    /// Puts `next` behind the step looking for its match in the background; false when none
    /// does.
    fn queue(&self, next: Queued) -> bool {
        let mut stepping = self.stepping.borrow_mut();
        if let Some(stepping) = stepping.as_mut() {
            stepping.queued.push(next);
        }
        stepping.is_some()
    }

    /// The match a step or Replace starts from: the selection, else the [`Found`] match while it
    /// still counts.
    fn match_bounds(&self, editor: &Editor) -> Option<(gtk::TextIter, gtk::TextIter)> {
        let buffer = editor.buffer();
        if let Some(bounds) = buffer.selection_bounds() {
            return Some(bounds);
        }
        let found = self
            .found
            .get()
            .filter(|f| f.version == editor.document().version())?;
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        (cursor.offset() == found.start).then(|| (cursor, buffer.iter_at_offset(found.end)))
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Some((buffer, id)) = self.mark_set.take()
            && let Some(buffer) = buffer.upgrade()
        {
            buffer.disconnect(id);
        }
        self.cancel_stepping();
        // The highlights go now, whoever still holds the context.
        self.context.set_highlight(false);
    }
}

thread_local! {
    static BARS: RefCell<Vec<Rc<FindBar>>> = const { RefCell::new(Vec::new()) };
}

/// The find bar of `state`'s window.
pub fn bar(state: &WindowState) -> Option<Rc<FindBar>> {
    BARS.with_borrow_mut(|bars| {
        bars.retain(|b| b.state.strong_count() > 0);
        bars.iter()
            .find(|b| std::ptr::eq(b.state.as_ptr(), state))
            .cloned()
    })
}

/// Called once per window after the M2 install hooks: puts the (hidden) bar into
/// `state.find_slot`.
pub fn install(state: &Rc<WindowState>) {
    install_css_once("find-bar", CSS);
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Find in File")
        .width_chars(ENTRY_MIN_CHARS)
        .max_width_chars(ENTRY_CHARS)
        .hexpand(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Find in file")]);
    let count = gtk::Label::builder()
        .css_classes(["dim-label", "numeric", "caption", "lemma-find-count"])
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
    let previous = nav_button("go-up-symbolic", "Previous Match (Shift+F3)");
    let next = nav_button("go-down-symbolic", "Next Match (F3)");
    let option = |markup: &str, tooltip: &str| {
        let label = gtk::Label::builder().use_markup(true).label(markup).build();
        gtk::ToggleButton::builder()
            .child(&label)
            .tooltip_text(tooltip)
            .css_classes(["flat", "lemma-find-option"])
            .valign(gtk::Align::Center)
            .build()
    };
    let case = option("Aa", "Match Case (Alt+C)");
    let word = option("<u>ab</u>", "Match Whole Word (Alt+W)");
    let regex = option(".*", "Use Regular Expression (Alt+R)");
    let replace_toggle = gtk::ToggleButton::builder()
        .icon_name("edit-find-replace-symbolic")
        .tooltip_text("Replace (Ctrl+H)")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let find_row = wrap_row(&[
        &group(&[
            entry.upcast_ref(),
            count.upcast_ref(),
            previous.upcast_ref(),
            next.upcast_ref(),
        ]),
        &group(&[
            case.upcast_ref(),
            word.upcast_ref(),
            regex.upcast_ref(),
            replace_toggle.upcast_ref(),
        ]),
    ]);

    let replace = gtk::Entry::builder()
        .placeholder_text("Replace")
        .width_chars(ENTRY_MIN_CHARS)
        .max_width_chars(ENTRY_CHARS)
        .hexpand(true)
        .build();
    replace.update_property(&[gtk::accessible::Property::Label("Replace with")]);
    let replace_one = gtk::Button::builder()
        .label("Replace")
        .tooltip_text("Replace the match and go to the next one (Enter)")
        .sensitive(false)
        .build();
    let replace_all = gtk::Button::builder()
        .label("Replace All")
        .tooltip_text("Replace every match in the file (Ctrl+Enter)")
        .sensitive(false)
        .build();
    let replace_row = wrap_row(&[
        replace.upcast_ref(),
        &group(&[replace_one.upcast_ref(), replace_all.upcast_ref()]),
    ]);
    replace_row.add_css_class("lemma-find-replace");
    let replace_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .child(&replace_row)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&find_row);
    content.append(&replace_revealer);
    // Not `connect_entry`: the bar would empty the entry whenever it closes.
    let bar = gtk::SearchBar::builder()
        .child(&content)
        .show_close_button(true)
        .css_classes(["lemma-find-bar"])
        .build();
    state.find_slot.set_child(Some(&bar));

    let settings = sourceview5::SearchSettings::builder()
        .wrap_around(true)
        .build();
    for (toggle, property) in [
        (&case, "case-sensitive"),
        (&word, "at-word-boundaries"),
        (&regex, "regex-enabled"),
    ] {
        toggle
            .bind_property("active", &settings, property)
            .bidirectional()
            .sync_create()
            .build();
    }
    let find = Rc::new(FindBar {
        state: Rc::downgrade(state),
        bar,
        entry,
        count,
        previous,
        next,
        case,
        word,
        regex,
        replace_toggle,
        replace_revealer,
        replace,
        replace_one,
        replace_all,
        settings,
        target: RefCell::new(None),
        quiet_toggle: Cell::new(false),
    });
    find.connect_signals(state, &content);
    BARS.with_borrow_mut(|bars| bars.push(find.clone()));
    let weak = Rc::downgrade(&find);
    state.window.connect_destroy(move |_| {
        if let Some(find) = weak.upgrade() {
            let target = find.target.take();
            drop(target);
            BARS.with_borrow_mut(|bars| bars.retain(|b| !Rc::ptr_eq(b, &find)));
        }
    });
}

/// Called when `editor`'s tab closes: its search ends, and the bar moves on to the tab shown
/// next (or closes with the last one).
pub fn detach(state: &WindowState, editor: &Rc<Editor>) {
    let Some(find) = bar(state) else {
        return;
    };
    let ours = find
        .target
        .borrow()
        .as_ref()
        .is_some_and(|t| std::ptr::eq(t.editor.as_ptr(), Rc::as_ptr(editor)));
    if ours {
        let target = find.target.take();
        drop(target);
        find.retarget();
    }
}

/// `win.find` (Ctrl+F): opens the bar with the focus in its entry.
pub fn show(state: &Rc<WindowState>) {
    open(state, false);
}

/// `win.find-replace` (Ctrl+H): opens the bar with its replace row; in the bar, hides that row
/// again.
pub fn show_replace(state: &Rc<WindowState>) {
    open(state, true);
}

/// `win.find-next` (F3, `forward`) and `win.find-prev` (Shift+F3): selects the next or previous
/// match, wrapping around. A closed bar opens again; without a query the focus goes to its
/// entry.
pub fn step(state: &Rc<WindowState>, forward: bool) {
    let Some(find) = bar(state) else {
        return;
    };
    if state.active_editor().is_none() {
        return;
    }
    find.bar.set_search_mode(true);
    if find.settings.search_text().is_none() {
        find.focus_entry();
        return;
    }
    find.step(forward);
}

fn open(state: &Rc<WindowState>, replace: bool) {
    let Some(find) = bar(state) else {
        return;
    };
    if state.active_editor().is_none() {
        return;
    }
    let in_bar = gtk::prelude::GtkWindowExt::focus(&state.window)
        .is_some_and(|focus| focus.is_ancestor(&find.bar));
    if replace && in_bar && find.replace_toggle.is_active() {
        find.replace_toggle.set_active(false);
        find.focus_entry();
        return;
    }
    // In the bar, and on the match the bar selected, the query stays: it may be the regular
    // expression that found the match.
    let prefill = (!in_bar && !find.selection_is_found())
        .then(|| search_panel::selection_text(state))
        .flatten();
    if let Some(selection) = prefill {
        let text = if find.regex.is_active() {
            glib::Regex::escape_string(&selection).to_string()
        } else {
            selection
        };
        find.entry.set_text(&text);
    }
    find.bar.set_search_mode(true);
    find.search(false);
    if replace {
        find.quiet_toggle.set(true);
        find.replace_toggle.set_active(true);
        find.quiet_toggle.set(false);
    }
    if replace && !find.entry.text().is_empty() {
        find.replace.grab_focus();
    } else {
        find.focus_entry();
    }
}

impl FindBar {
    fn connect_signals(self: &Rc<Self>, state: &Rc<WindowState>, content: &gtk::Box) {
        let weak = Rc::downgrade(self);
        self.entry.connect_search_changed(move |_| {
            if let Some(find) = weak.upgrade() {
                find.search(false);
            }
        });
        // Enter: the query typed (at once, without the entry's delay), else the next match.
        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |entry| {
            if let Some(find) = weak.upgrade() {
                let fresh = find.settings.search_text().as_deref() == Some(entry.text().as_str());
                if fresh {
                    find.step(true);
                } else {
                    find.search(false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_stop_search(move |_| {
            if let Some(find) = weak.upgrade() {
                find.bar.set_search_mode(false);
            }
        });
        // Shift+Enter: the previous match (the entry's own Enter has no Shift variant).
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            if is_enter(key)
                && mods.contains(gdk::ModifierType::SHIFT_MASK)
                && let Some(find) = weak.upgrade()
            {
                find.step(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.entry.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.previous.connect_clicked(move |_| {
            if let Some(find) = weak.upgrade() {
                find.step(false);
            }
        });
        let weak = Rc::downgrade(self);
        self.next.connect_clicked(move |_| {
            if let Some(find) = weak.upgrade() {
                find.step(true);
            }
        });
        for toggle in [&self.case, &self.word, &self.regex] {
            let weak = Rc::downgrade(self);
            toggle.connect_toggled(move |_| {
                if let Some(find) = weak.upgrade() {
                    find.search(true);
                }
            });
        }

        let weak = Rc::downgrade(self);
        self.replace_toggle.connect_toggled(move |toggle| {
            if let Some(find) = weak.upgrade() {
                find.replace_revealer.set_reveal_child(toggle.is_active());
                if find.quiet_toggle.get() {
                    return;
                }
                if toggle.is_active() {
                    find.replace.grab_focus();
                } else if find
                    .replace
                    .state_flags()
                    .contains(gtk::StateFlags::FOCUS_WITHIN)
                {
                    find.focus_entry();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.replace.connect_activate(move |_| {
            if let Some(find) = weak.upgrade() {
                find.replace_one();
            }
        });
        // Ctrl+Enter in the replace entry replaces every match.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            if is_enter(key)
                && mods.contains(gdk::ModifierType::CONTROL_MASK)
                && let Some(find) = weak.upgrade()
            {
                find.replace_all();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.replace.add_controller(keys);
        let weak = Rc::downgrade(self);
        self.replace_one.connect_clicked(move |_| {
            if let Some(find) = weak.upgrade() {
                find.replace_one();
            }
        });
        let weak = Rc::downgrade(self);
        self.replace_all.connect_clicked(move |_| {
            if let Some(find) = weak.upgrade() {
                find.replace_all();
            }
        });

        let weak = Rc::downgrade(self);
        self.bar.connect_search_mode_enabled_notify(move |bar| {
            if let Some(find) = weak.upgrade() {
                if bar.is_search_mode() {
                    find.retarget();
                } else {
                    find.closed();
                }
            }
        });
        let weak = Rc::downgrade(self);
        state.tab_view.connect_selected_page_notify(move |_| {
            if let Some(find) = weak.upgrade() {
                find.retarget();
            }
        });

        // Keys anywhere in the bar: Escape closes it, and the toggles' keys are the Search page's.
        let controller = gtk::ShortcutController::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        let close = gtk::CallbackAction::new(move |_, _| {
            if let Some(find) = weak.upgrade() {
                find.bar.set_search_mode(false);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(close),
        ));
        for (trigger, toggle) in [
            ("<Alt>c", &self.case),
            ("<Alt>w", &self.word),
            ("<Alt>r", &self.regex),
        ] {
            let toggle = toggle.downgrade();
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(toggle) = toggle.upgrade() {
                    toggle.set_active(!toggle.is_active());
                }
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(action),
            ));
        }
        content.add_controller(controller);
    }

    /// The editor searched and its search, while the bar is shown.
    fn current(&self) -> Option<(Rc<Editor>, sourceview5::SearchContext)> {
        let target = self.target.borrow();
        let target = target.as_ref()?;
        Some((target.editor.upgrade()?, target.context.clone()))
    }

    /// See [`Target::match_bounds`].
    fn match_bounds(&self, editor: &Editor) -> Option<(gtk::TextIter, gtk::TextIter)> {
        self.target.borrow().as_ref()?.match_bounds(editor)
    }

    /// Selects the match `start..end` of `editor` (see [`select`]) and remembers it
    /// ([`Found`]).
    fn select_match(&self, editor: &Editor, start: &gtk::TextIter, end: &gtk::TextIter) {
        if let Some(target) = self.target.borrow().as_ref() {
            target.found.set(Some(Found {
                version: editor.document().version(),
                start: start.offset(),
                end: end.offset(),
            }));
        }
        select(editor, start, end);
    }

    /// Puts the search on the active editor's buffer while the bar is shown, and ends it
    /// otherwise; a shown bar closes when no editor is left.
    fn retarget(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let shown = self.bar.is_search_mode();
        let editor = state.active_editor().filter(|_| shown);
        if shown && editor.is_none() {
            self.bar.set_search_mode(false);
            return;
        }
        let same = match (self.target.borrow().as_ref(), &editor) {
            (Some(t), Some(e)) => std::ptr::eq(t.editor.as_ptr(), Rc::as_ptr(e)),
            (None, None) => true,
            _ => false,
        };
        if !same {
            let old = self.target.take();
            drop(old);
            if let Some(editor) = &editor {
                let target = Target::new(self, editor);
                self.target.replace(Some(target));
            }
        }
        self.refresh_count();
    }

    /// The bar closed: the search ends, and the focus goes back to the editor unless the user
    /// moved it elsewhere.
    fn closed(&self) {
        let target = self.target.take();
        drop(target);
        self.refresh_count();
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let focus = gtk::prelude::GtkWindowExt::focus(&state.window);
        let lost = focus.is_none_or(|f| {
            f.is_ancestor(&self.bar) || &f == state.window.upcast_ref::<gtk::Widget>()
        });
        if lost && let Some(editor) = state.active_editor() {
            editor.view().grab_focus();
        }
    }

    fn focus_entry(&self) {
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Searches for the entry's text unless that is the search shown (`force`: an option
    /// changed). The first match at or after the start of the selection is selected once the
    /// search has counted ([`FindBar::scanned`]).
    fn search(&self, force: bool) {
        let text = self.entry.text();
        let query = (!text.is_empty()).then_some(text.as_str());
        if !force && self.settings.search_text().as_deref() == query {
            return;
        }
        if let Some(target) = self.target.borrow().as_ref() {
            // The search counts again, and a step looking for a match of the old one is moot.
            target.counted.set(false);
            target.cancel_stepping();
        }
        self.settings.set_search_text(query);
        self.count.set_text("");
        if let Some((editor, _)) = self.current()
            && let Some(target) = self.target.borrow().as_ref()
        {
            let buffer = editor.buffer();
            let from = buffer
                .selection_bounds()
                .map_or_else(|| buffer.iter_at_mark(&buffer.get_insert()), |(s, _)| s);
            target.pending.set(query.map(|_| Start {
                version: editor.document().version(),
                offset: from.offset(),
            }));
        }
        self.refresh_count();
    }

    /// The search counted its matches (it has scanned the whole buffer, so finding one is
    /// quick now): a new query's first match is selected, then the label updated.
    fn scanned(&self) {
        let pending = self.target.borrow().as_ref().and_then(|t| {
            let counted = t.context.occurrences_count() >= 0;
            if counted {
                t.counted.set(true);
            }
            counted.then(|| t.pending.take()).flatten()
        });
        if let Some(from) = pending
            && let Some((editor, context)) = self.current()
        {
            let buffer = editor.buffer();
            // Edited meanwhile: from the cursor.
            let from = if from.version == editor.document().version() {
                buffer.iter_at_offset(from.offset)
            } else {
                buffer.iter_at_mark(&buffer.get_insert())
            };
            if let Some((start, end, _)) = context.forward(&from) {
                // Selecting in the editor takes the entry's selection away (one primary
                // selection): a query selected for typing over stays so.
                let typing_over = self.entry.selection_bounds();
                self.select_match(&editor, &start, &end);
                if let Some((a, b)) = typing_over {
                    self.entry.select_region(a, b);
                }
            }
        }
        self.refresh_count();
    }

    /// Forgets the first match a new query waits for: the user moved on.
    fn cancel_pending(&self) {
        if let Some(target) = self.target.borrow().as_ref() {
            target.pending.set(None);
        }
    }

    /// The editor's selection is the match the bar selected last ([`Found`]), not one the user
    /// made.
    fn selection_is_found(&self) -> bool {
        let target = self.target.borrow();
        let Some(target) = target.as_ref() else {
            return false;
        };
        let Some(editor) = target.editor.upgrade() else {
            return false;
        };
        let found = target
            .found
            .get()
            .filter(|f| f.version == editor.document().version());
        let selection = editor.buffer().selection_bounds();
        found.zip(selection).is_some_and(|(found, (start, end))| {
            (start.offset(), end.offset()) == (found.start, found.end)
        })
    }

    /// Selects the next (or previous) match after (before) the selection, wrapping around; rings
    /// the bell when there is none.
    ///
    /// Until the search has counted the matches of a new query (it scans the buffer in idle
    /// steps), GtkSourceView would scan the text up to the match on the spot: all of it when the
    /// only match is near the end or there is none (up to 47 ms on the 60 000 lines of
    /// `large.tex`, 8 ms on 10 000). So the match is looked for in the background then
    /// (`forward_async`, which waits for the idle scan to get there), and the steps and Replaces
    /// asked for meanwhile follow it, in order. An edit of the buffer, a new query or the end of
    /// the search drops them.
    fn step(self: &Rc<Self>, forward: bool) {
        let Some((editor, context)) = self.current() else {
            return;
        };
        if self.settings.search_text().is_none() {
            return;
        }
        self.cancel_pending();
        let target = self.target.borrow();
        let Some(target) = target.as_ref() else {
            return;
        };
        if target.queue(Queued::Step(forward)) {
            return;
        }
        let (start, end) = target.match_bounds(&editor).unwrap_or_else(|| {
            let buffer = editor.buffer();
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (cursor, cursor)
        });
        // (An invalid expression finds nothing, at once.)
        if target.counted.get() || context.regex_error().is_some() {
            self.show_step(&editor, find_at_once(&context, &start, &end, forward));
            return;
        }
        let cancellable = gio::Cancellable::new();
        target.stepping.replace(Some(Stepping {
            cancellable: cancellable.clone(),
            queued: Vec::new(),
        }));
        let weak = Rc::downgrade(self);
        let ours = cancellable.clone();
        let version = editor.document().version();
        let from = if forward { end } else { start };
        search_async(&context, &from, forward, &cancellable, move |searched| {
            // GtkSourceView calls back from inside its scan, and goes on with the search's state
            // when this returns: the match is taken (and the next search started) from the main
            // loop.
            let found = searched.map(|found| found.map(|(s, e)| (s.offset(), e.offset())));
            glib::idle_add_local_once(move || {
                if let Some(find) = weak.upgrade() {
                    find.stepped(&ours, version, found);
                }
            });
        });
    }

    /// A step's background search ended (see [`FindBar::step`]) with the character offsets of
    /// its match, `version` being the document's version when it began: the match is selected
    /// (or the bell rings), then the steps and Replaces asked for meanwhile are taken.
    /// Cancelled, or with the text changed since, it takes them with it. (It never finds an
    /// empty match at the selection, which `find_at_once` steps past: GtkSourceView matches no
    /// empty text.)
    fn stepped(
        self: &Rc<Self>,
        cancellable: &gio::Cancellable,
        version: u64,
        found: Result<Option<(i32, i32)>, glib::Error>,
    ) {
        let Some((editor, _)) = self.current() else {
            return;
        };
        let queued = {
            let target = self.target.borrow();
            let Some(target) = target.as_ref() else {
                return;
            };
            let mut stepping = target.stepping.borrow_mut();
            if stepping
                .as_ref()
                .is_none_or(|s| s.cancellable != *cancellable)
            {
                return;
            }
            let found = match found {
                Ok(found) if editor.document().version() == version => found,
                _ => {
                    stepping.take();
                    return;
                }
            };
            let buffer = editor.buffer();
            let queued = stepping.take().map(|s| s.queued).unwrap_or_default();
            drop(stepping);
            let found = found.map(|(s, e)| (buffer.iter_at_offset(s), buffer.iter_at_offset(e)));
            self.show_step(&editor, found);
            queued
        };
        for next in queued {
            match next {
                Queued::Step(forward) => self.step(forward),
                Queued::Replace => self.replace_one(),
            }
        }
    }

    /// Selects a step's match, or rings the bell when it found none.
    fn show_step(&self, editor: &Editor, found: Option<(gtk::TextIter, gtk::TextIter)>) {
        match found {
            Some((start, end)) => {
                self.select_match(editor, &start, &end);
                self.refresh_count();
            }
            None => self.entry.error_bell(),
        }
    }

    /// What Replace inserts: the replace entry's text, with a regular expression as a template
    /// in GLib's syntax (see the module docs). `None` while the expression is invalid.
    fn replacement(&self) -> Option<String> {
        let text = self.replace.text();
        if !self.settings.is_regex_enabled() {
            return Some(text.to_string());
        }
        let pattern = self.settings.search_text()?;
        let regex = glib::Regex::new(
            &pattern,
            glib::RegexCompileFlags::MULTILINE,
            glib::RegexMatchFlags::empty(),
        )
        .ok()
        .flatten()?;
        let groups = usize::try_from(regex.capture_count()).map_or(1, |n| n + 1);
        let template = Template::for_groups(&text, groups, |name| {
            usize::try_from(regex.string_number(name)).ok()
        });
        Some(template.to_glib())
    }

    /// Replaces the selected match (see [`Target::match_bounds`]), then selects the next one.
    /// While a step looks for its match in the background, it waits for it.
    fn replace_one(self: &Rc<Self>) {
        let Some((editor, context)) = self.current() else {
            return;
        };
        if self
            .target
            .borrow()
            .as_ref()
            .is_some_and(|t| t.queue(Queued::Replace))
        {
            return;
        }
        let Some(replacement) = self.replacement() else {
            self.entry.error_bell();
            return;
        };
        if let Some((mut start, mut end)) = self.match_bounds(&editor) {
            let replaced = envtools::without_live_rename(|| {
                replace_match(&context, &mut start, &mut end, &replacement)
            });
            match replaced {
                Ok(true) => editor.buffer().place_cursor(&end),
                Ok(false) => {}
                Err(e) => {
                    self.report(&e);
                    return;
                }
            }
        }
        self.step(true);
    }

    /// Replaces every match in the file, as one undo step.
    fn replace_all(&self) {
        let Some((_, context)) = self.current() else {
            return;
        };
        let Some(replacement) = self.replacement() else {
            self.entry.error_bell();
            return;
        };
        self.cancel_pending();
        match envtools::without_live_rename(|| replace_every_match(&context, &replacement)) {
            Ok(0) => self.entry.error_bell(),
            Ok(n) => {
                if let Some(state) = self.state.upgrade() {
                    let n = usize::try_from(n).unwrap_or(usize::MAX);
                    state.toast(&format!(
                        "Replaced {}",
                        search_panel::count(n, "match", "matches")
                    ));
                }
            }
            Err(e) => self.report(&e),
        }
        self.refresh_count();
    }

    fn report(&self, error: &glib::Error) {
        if let Some(state) = self.state.upgrade() {
            state.toast(&format!("Could not replace: {}", error.message()));
        }
    }

    /// Updates the "3 of 17" label, the entry's error style and the buttons.
    fn refresh_count(&self) {
        let current = self.current();
        let query = self.settings.search_text();
        let mut error = None;
        let mut total = -1;
        let label = match (&current, &query) {
            (Some((editor, context)), Some(_)) => match context.regex_error() {
                Some(e) => {
                    error = Some(e.message().to_owned());
                    Some("Invalid expression".to_owned())
                }
                None => {
                    total = context.occurrences_count();
                    let position = self
                        .match_bounds(editor)
                        .map_or(-1, |(s, e)| context.occurrence_position(&s, &e));
                    count_label(total, position)
                }
            },
            _ => Some(String::new()),
        };
        if let Some(label) = label
            && self.count.text() != label
        {
            self.count.set_text(&label);
        }
        self.count.set_visible(!self.count.text().is_empty());
        if error.is_some() || total == 0 {
            self.entry.add_css_class("error");
        } else {
            self.entry.remove_css_class("error");
        }
        self.entry.set_tooltip_text(error.as_deref());
        let can_find = current.is_some() && query.is_some() && error.is_none() && total != 0;
        for button in [
            &self.previous,
            &self.next,
            &self.replace_one,
            &self.replace_all,
        ] {
            button.set_sensitive(can_find);
        }
    }

    // ----- for tests -----

    /// The bar itself.
    pub fn search_bar(&self) -> &gtk::SearchBar {
        &self.bar
    }

    pub fn entry(&self) -> &gtk::SearchEntry {
        &self.entry
    }

    pub fn replace_entry(&self) -> &gtk::Entry {
        &self.replace
    }

    /// Types `text` into the entry and searches at once (without the entry's delay); the first
    /// match is selected once the search has counted ([`Status::total`]).
    pub fn search_now(&self, text: &str) {
        self.entry.set_text(text);
        self.search(false);
    }

    /// Sets Match Case, Whole Word and Regular Expression (searches again when they change).
    pub fn set_options(&self, case_sensitive: bool, whole_word: bool, regex: bool) {
        self.case.set_active(case_sensitive);
        self.word.set_active(whole_word);
        self.regex.set_active(regex);
    }

    /// Match Case, Whole Word and Regular Expression.
    pub fn options(&self) -> (bool, bool, bool) {
        (
            self.settings.is_case_sensitive(),
            self.settings.is_at_word_boundaries(),
            self.settings.is_regex_enabled(),
        )
    }

    /// Shows the replace row with `text` in its entry (`None` hides the row).
    pub fn set_replacement(&self, text: Option<&str>) {
        self.quiet_toggle.set(true);
        self.replace_toggle.set_active(text.is_some());
        self.quiet_toggle.set(false);
        self.replace.set_text(text.unwrap_or(""));
    }

    /// Replace, as the button does.
    pub fn replace(self: &Rc<Self>) {
        self.replace_one();
    }

    /// Replace All, as the button does.
    pub fn replace_every(&self) {
        self.replace_all();
    }

    /// The search's buffer and context while the bar is shown.
    pub fn search_context(&self) -> Option<sourceview5::SearchContext> {
        self.current().map(|(_, context)| context)
    }

    /// See [`Status`].
    pub fn status(&self) -> Status {
        let current = self.current();
        let count = |n: i32| u32::try_from(n).ok();
        Status {
            open: self.bar.is_search_mode(),
            query: self.settings.search_text().map(|q| q.to_string()),
            total: current
                .as_ref()
                .and_then(|(_, c)| count(c.occurrences_count())),
            current: current.as_ref().and_then(|(editor, c)| {
                let (s, e) = self.match_bounds(editor)?;
                count(c.occurrence_position(&s, &e)).filter(|n| *n > 0)
            }),
            label: if self.count.is_visible() {
                self.count.text().to_string()
            } else {
                String::new()
            },
            replace_shown: self.replace_revealer.reveals_child(),
            error: self.entry.has_css_class("error"),
        }
    }
}

/// What the bar shows (for tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub open: bool,
    pub query: Option<String>,
    /// Matches in the file; `None` while they are counted or the bar is closed.
    pub total: Option<u32>,
    /// The 1-based ordinal of the match selected; `None` when the selection is no match (see
    /// [`Found`]).
    pub current: Option<u32>,
    /// "3 of 17", "17 matches", "No matches".
    pub label: String,
    pub replace_shown: bool,
    /// The entry is marked as failed (no matches, or an invalid expression).
    pub error: bool,
}

/// The label for `total` matches (`-1`: still counting) with the selection being match
/// `position` (1-based; `0`: the selection is no match, `-1`: not known yet, as
/// `SearchContext::occurrence_position` says). `None` while counting: the label keeps what it
/// shows, so typing in the editor does not make it flicker.
pub fn count_label(total: i32, position: i32) -> Option<String> {
    let total = usize::try_from(total).ok()?;
    let label = match usize::try_from(position) {
        _ if total == 0 => "No matches".to_owned(),
        Ok(p) if p > 0 => format!(
            "{} of {}",
            search_panel::digits(p),
            search_panel::digits(total)
        ),
        _ => search_panel::count(total, "match", "matches"),
    };
    Some(label)
}

/// A row of the bar: `children` on one line, wrapping onto more lines when the editor is too
/// narrow for one, so the bar never makes the editor wider than its minimum.
fn wrap_row(children: &[&gtk::Widget]) -> adw::WrapBox {
    let row = adw::WrapBox::builder()
        .child_spacing(6)
        .line_spacing(4)
        .justify(adw::JustifyMode::Fill)
        .justify_last_line(true)
        .wrap_policy(adw::WrapPolicy::Minimum)
        .build();
    for child in children {
        row.append(*child);
    }
    row
}

/// Widgets that stay on one line.
fn group(children: &[&gtk::Widget]) -> gtk::Widget {
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    for child in children {
        group.append(*child);
    }
    group.upcast()
}

fn is_enter(key: gdk::Key) -> bool {
    matches!(
        key,
        gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter
    )
}

/// Selects `start..end` in `editor` and scrolls it into view: a line out of sight to the middle
/// of the view (keeping the horizontal position where it can), one on screen only as far as
/// needed.
fn select(editor: &Editor, start: &gtk::TextIter, end: &gtk::TextIter) {
    let buffer = editor.buffer();
    buffer.select_range(start, end);
    let view = editor.view();
    let visible = view.visible_rect();
    let at = view.iter_location(start);
    let on_screen = at.y() >= visible.y() && at.y() + at.height() <= visible.y() + visible.height();
    let insert = buffer.get_insert();
    if on_screen || visible.height() <= 0 {
        view.scroll_mark_onscreen(&insert);
    } else {
        let x = f64::from(at.x() - visible.x()) / f64::from(visible.width().max(1));
        view.scroll_to_mark(&insert, 0.0, true, x.clamp(0.0, 1.0), 0.5);
    }
}

/// The next match after `end` (`forward`) or the previous one before `start`, wrapping around,
/// looked for at once. An empty match (`^`, `\b`) at `start..end` is found again, so the search
/// looks on from the next character.
fn find_at_once(
    context: &sourceview5::SearchContext,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
    forward: bool,
) -> Option<(gtk::TextIter, gtk::TextIter)> {
    let search = |from: &gtk::TextIter| {
        let found = if forward {
            context.forward(from)
        } else {
            context.backward(from)
        };
        found.map(|(s, e, _)| (s, e))
    };
    let found = search(if forward { end } else { start });
    if found
        .as_ref()
        .is_some_and(|(s, e)| s == e && s == start && e == end)
    {
        let mut from = if forward { *end } else { *start };
        if forward {
            from.forward_char();
        } else {
            from.backward_char();
        }
        return search(&from);
    }
    found
}

/// What a background search gives its callback: the match (`None`: there is none), or the error
/// it was cancelled with.
type Searched = Result<Option<(gtk::TextIter, gtk::TextIter)>, glib::Error>;

/// `gtk_source_search_context_forward_async` (`backward_async`): looks for the next (previous)
/// match from `from`, wrapping around, as the search's idle scan gets there, and calls `done`
/// with it. GtkSourceView cancels it, and `done` gets the error, when the buffer changes, the
/// search's settings change or another one starts. (sourceview5's binding reads the match's
/// iters also when there is none.)
fn search_async(
    context: &sourceview5::SearchContext,
    from: &gtk::TextIter,
    forward: bool,
    cancellable: &gio::Cancellable,
    done: impl FnOnce(Searched) + 'static,
) {
    use glib::translate::{ToGlibPtr, ToGlibPtrMut, Uninitialized, from_glib_full};
    type Done = (bool, Box<dyn FnOnce(Searched)>);
    unsafe extern "C" fn finished(
        source: *mut glib::gobject_ffi::GObject,
        result: *mut gio::ffi::GAsyncResult,
        data: glib::ffi::gpointer,
    ) {
        // SAFETY: `data` is the box `search_async` leaked for this call, taken back once.
        let (forward, done) = *unsafe { Box::from_raw(data.cast::<Done>()) };
        let finish = if forward {
            sourceview5::ffi::gtk_source_search_context_forward_finish
        } else {
            sourceview5::ffi::gtk_source_search_context_backward_finish
        };
        let mut error = std::ptr::null_mut();
        // SAFETY: the search context and its result, two iters to fill in (left as they are
        // without a match, and not read then), no wrap-around flag (it is optional) and an
        // empty error location.
        let (found, start, end) = unsafe {
            let mut start = gtk::TextIter::uninitialized();
            let mut end = gtk::TextIter::uninitialized();
            let found = finish(
                source.cast(),
                result,
                start.to_glib_none_mut().0,
                end.to_glib_none_mut().0,
                std::ptr::null_mut(),
                &mut error,
            );
            (found, start, end)
        };
        let searched = if error.is_null() {
            Ok((found != glib::ffi::GFALSE).then_some((start, end)))
        } else {
            // SAFETY: `finish` set `error`, which the caller owns.
            Err(unsafe { from_glib_full(error) })
        };
        done(searched);
    }
    let start = if forward {
        sourceview5::ffi::gtk_source_search_context_forward_async
    } else {
        sourceview5::ffi::gtk_source_search_context_backward_async
    };
    let data: Box<Done> = Box::new((forward, Box::new(done)));
    // SAFETY: the context and an iter of its buffer, a cancellable (with one, GtkSourceView
    // always calls back), and the callback with its data, which it takes back.
    unsafe {
        start(
            context.to_glib_none().0,
            from.to_glib_none().0,
            cancellable.to_glib_none().0,
            Some(finished),
            Box::into_raw(data).cast(),
        );
    }
}

/// `gtk_source_search_context_replace`: replaces the match `start..end`, after which the iters
/// bound the replacement; `Ok(false)` when `start..end` is no match. (sourceview5's binding
/// asserts that a false return comes with an error, which it does not then.)
fn replace_match(
    context: &sourceview5::SearchContext,
    start: &mut gtk::TextIter,
    end: &mut gtk::TextIter,
    replacement: &str,
) -> Result<bool, glib::Error> {
    use glib::translate::{ToGlibPtr, ToGlibPtrMut, from_glib_full};
    let mut error = std::ptr::null_mut();
    // SAFETY: the context and two iters of its buffer, a NUL-terminated replacement (length
    // -1) and an empty error location, all alive for the call.
    let replaced = unsafe {
        sourceview5::ffi::gtk_source_search_context_replace(
            context.to_glib_none().0,
            start.to_glib_none_mut().0,
            end.to_glib_none_mut().0,
            replacement.to_glib_none().0,
            -1,
            &mut error,
        )
    };
    if !error.is_null() {
        // SAFETY: the call set `error`, which the caller owns.
        return Err(unsafe { from_glib_full(error) });
    }
    Ok(replaced != glib::ffi::GFALSE)
}

/// `gtk_source_search_context_replace_all`: replaces every match in one user action (one undo
/// step) and returns how many. (sourceview5's binding takes that count for a success flag and
/// panics when nothing was replaced.)
fn replace_every_match(
    context: &sourceview5::SearchContext,
    replacement: &str,
) -> Result<u32, glib::Error> {
    use glib::translate::{ToGlibPtr, from_glib_full};
    let mut error = std::ptr::null_mut();
    // SAFETY: as in `replace_match`.
    let replaced = unsafe {
        sourceview5::ffi::gtk_source_search_context_replace_all(
            context.to_glib_none().0,
            replacement.to_glib_none().0,
            -1,
            &mut error,
        )
    };
    if !error.is_null() {
        // SAFETY: the call set `error`, which the caller owns.
        return Err(unsafe { from_glib_full(error) });
    }
    Ok(replaced)
}

#[cfg(test)]
mod tests {
    use super::count_label;

    #[test]
    fn count_labels() {
        assert_eq!(count_label(-1, -1), None, "still counting");
        assert_eq!(count_label(-1, 3), None);
        assert_eq!(count_label(0, -1).as_deref(), Some("No matches"));
        assert_eq!(count_label(17, 3).as_deref(), Some("3 of 17"));
        assert_eq!(count_label(17, -1).as_deref(), Some("17 matches"));
        assert_eq!(count_label(17, 0).as_deref(), Some("17 matches"));
        assert_eq!(count_label(1, -1).as_deref(), Some("1 match"));
        assert_eq!(
            count_label(20_000, 1_234).as_deref(),
            Some("1\u{202f}234 of 20\u{202f}000")
        );
    }
}
