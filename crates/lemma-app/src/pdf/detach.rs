//! The PDF preview in a window of its own.
//!
//! [`set_detached`] moves the content of `state.pdf_pane` (the PDF view with its toolbar,
//! search bar and strip) into an `adw::Window` of its own (for a second monitor) and back. The
//! controller, the view and its texture cache stay the same objects, so SyncTeX (both ways),
//! search, reloads after builds (with their scroll anchor) and dark mode keep working; the
//! scroll position survives the move ([`PdfView::hold_anchor`] across the new window's first
//! allocations).
//!
//! - While detached, the main window's PDF pane is hidden; showing it (the header's PDF button,
//!   or a jump to a place in the PDF) brings the PDF window forward instead. Re-attaching shows
//!   the pane again.
//! - The PDF window has a header bar with the PDF's name and an "Attach to Main Window" button.
//!   It shares the PDF actions of the main window (zoom, dark pages, the strip, opening the PDF
//!   in another application, building with F5, back from link jumps with Alt+Left; not the
//!   editing ones such as Ctrl+W) and has its own Ctrl+F for the search bar. Its size is
//!   remembered for the next time in the session.
//! - Closing the PDF window re-attaches the view (and sets `win.pdf-detach` back to false);
//!   closing the main window closes it too.
//!
//! Hooks: [`install`] once per window; `win.pdf-detach` is a stateful (boolean) action the shell
//! creates ("PDF in Separate Window" in the main menu): activating it calls [`set_detached`]
//! with the opposite of its state and sets the state to what that returns.
//!
//! [`PdfView::hold_anchor`]: super::pdf_view::PdfView::hold_anchor

use crate::window::WindowState;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long the view keeps its anchor while its new window settles.
pub const ANCHOR_HOLD: Duration = Duration::from_millis(1500);
/// The PDF window's first size.
const DEFAULT_SIZE: (i32, i32) = (720, 900);

/// The main window's actions the PDF window shares (its toolbar buttons and accelerators).
const SHARED_ACTIONS: &[&str] = &[
    "pdf-zoom-in",
    "pdf-zoom-out",
    "pdf-fit-width",
    "pdf-fit-page",
    "pdf-zoom-reset",
    "pdf-open-external",
    "pdf-show-in-folder",
    "pdf-invert",
    "pdf-detach",
    "pdf-search",
    "toggle-pdf-sidebar",
    "sync-forward",
    "sync-inverse",
    "goto-back",
    "build",
    "stop-build",
    "save",
];

/// The detach state of one window.
struct Detach {
    window: RefCell<Option<adw::Window>>,
    /// The window's last size.
    size: Cell<(i32, i32)>,
    /// Set while re-attaching (the window's close handler must not re-attach again).
    attaching: Cell<bool>,
    /// The handler that keeps the pane hidden while detached.
    pane_handler: RefCell<Option<glib::SignalHandlerId>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<(Weak<WindowState>, Rc<Detach>)>> = const { RefCell::new(Vec::new()) };
}

fn detach_of(state: &WindowState) -> Option<Rc<Detach>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|(w, _)| w.strong_count() > 0);
        windows
            .iter()
            .find(|(w, _)| std::ptr::eq(w.as_ptr(), state))
            .map(|(_, d)| d.clone())
    })
}

/// Called once per window after the M2 install hooks.
pub fn install(state: &Rc<WindowState>) {
    let detach = Rc::new(Detach {
        window: RefCell::new(None),
        size: Cell::new(DEFAULT_SIZE),
        attaching: Cell::new(false),
        pane_handler: RefCell::new(None),
    });
    WINDOWS.with_borrow_mut(|w| w.push((Rc::downgrade(state), detach.clone())));
    // Closing the main window closes the PDF window too (the application would otherwise keep
    // running for it). `gtk_window_destroy` unrealizes the window at once, while its "destroy"
    // signal waits for the last reference to go.
    let weak = Rc::downgrade(&detach);
    state.window.connect_unrealize(move |_| {
        if let Some(detach) = weak.upgrade()
            && let Some(window) = detach.window.take()
        {
            detach.attaching.set(true);
            window.destroy();
        }
    });
}

/// Detaches (`true`) or re-attaches (`false`) the PDF preview; returns whether it is detached
/// afterwards.
pub fn set_detached(state: &Rc<WindowState>, detached: bool) -> bool {
    let Some(detach) = detach_of(state) else {
        return false;
    };
    if detached == detach.window.borrow().is_some() {
        if detached {
            present(state);
        }
        return detached;
    }
    if detached {
        detach_now(state, &detach);
    } else {
        attach_now(state, &detach);
    }
    is_detached(state)
}

/// True while the PDF preview is in its own window.
pub fn is_detached(state: &WindowState) -> bool {
    detach_of(state).is_some_and(|d| d.window.borrow().is_some())
}

/// The PDF window while detached.
pub fn window(state: &WindowState) -> Option<adw::Window> {
    detach_of(state)?.window.borrow().clone()
}

/// Brings the PDF window forward (while detached).
pub fn present(state: &WindowState) {
    if let Some(window) = window(state) {
        window.present();
    }
}

fn detach_now(state: &Rc<WindowState>, detach: &Rc<Detach>) {
    let Some(content) = state.pdf_pane.child() else {
        return;
    };
    let view = state.pdf.get().map(|p| p.view().clone());
    let anchor = view.as_ref().and_then(|v| v.anchor());
    // The keyboard focus leaves the content while it is still in the window (removing the
    // focus widget with it makes the window's paned lose track of its focus).
    let focus_inside = gtk::prelude::GtkWindowExt::focus(&state.window)
        .is_some_and(|f| f == content || f.is_ancestor(&content));
    if focus_inside {
        match state.active_editor() {
            Some(editor) => {
                editor.view().grab_focus();
            }
            None => gtk::prelude::GtkWindowExt::set_focus(&state.window, None::<&gtk::Widget>),
        }
    }
    // Out of the main window: the pane stays hidden while the PDF has its own window.
    state.pdf_pane.set_child(None::<&gtk::Widget>);
    state.pdf_pane.set_visible(false);

    let title = adw::WindowTitle::new("PDF Preview", "");
    if let Some(pdf) = state.pdf.get().and_then(|p| p.pdf()) {
        title.set_title(
            &pdf.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "PDF Preview".into()),
        );
        if let Some(dir) = pdf.parent() {
            title.set_subtitle(&tilde(dir));
        }
    }
    let attach = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("view-dual-symbolic")
                .label("_Attach")
                .use_underline(true)
                .build(),
        )
        .tooltip_text("Show the PDF in the Main Window Again")
        .action_name("win.pdf-detach")
        .build();
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    header.pack_start(&attach);
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));
    let (width, height) = detach.size.get();
    let window = adw::Window::builder()
        .application(&state.app)
        .title("PDF Preview — Lemma")
        .default_width(width)
        .default_height(height)
        .content(&toolbar_view)
        .build();
    window.add_css_class("lemma-pdf-window");

    // The PDF actions of the main window, and Ctrl+F.
    let actions = gio::SimpleActionGroup::new();
    for name in SHARED_ACTIONS {
        if let Some(action) = state.window.lookup_action(name) {
            actions.add_action(&action);
        }
    }
    window.insert_action_group("win", Some(&actions));
    let keys = gtk::ShortcutController::new();
    let weak = Rc::downgrade(state);
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>f"),
        Some(gtk::CallbackAction::new(move |_, _| {
            if let Some(state) = weak.upgrade() {
                super::search::show(&state);
            }
            glib::Propagation::Stop
        })),
    ));
    window.add_controller(keys);

    // Closing it re-attaches the view.
    let weak = Rc::downgrade(state);
    window.connect_close_request(move |_| {
        if let Some(state) = weak.upgrade()
            && let Some(detach) = detach_of(&state)
            && !detach.attaching.get()
        {
            attach_now(&state, &detach);
            if let Some(action) = state
                .window
                .lookup_action("pdf-detach")
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_state(&false.into());
            }
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(detach);
    window.connect_default_width_notify(move |w| remember_size(&weak, w));
    let weak = Rc::downgrade(detach);
    window.connect_default_height_notify(move |w| remember_size(&weak, w));
    // A title for the PDF shown (it may change while detached).
    if let Some(view) = &view {
        let weak = Rc::downgrade(state);
        let title = title.clone();
        view.connect_changed(move |_| {
            if let Some(state) = weak.upgrade()
                && let Some(pdf) = state.pdf.get().and_then(|p| p.pdf())
                && let Some(name) = pdf.file_name()
            {
                let name = name.to_string_lossy();
                if title.title().as_str() != name {
                    title.set_title(&name);
                }
            }
        });
    }

    // While detached, showing the pane brings the PDF window forward instead.
    let weak = Rc::downgrade(state);
    let handler = state.pdf_pane.connect_visible_notify(move |pane| {
        if !pane.is_visible() {
            return;
        }
        let weak = weak.clone();
        glib::idle_add_local_once(move || {
            if let Some(state) = weak.upgrade()
                && is_detached(&state)
            {
                state.pdf_pane.set_visible(false);
                present(&state);
            }
        });
    });
    detach.pane_handler.replace(Some(handler));
    detach.window.replace(Some(window.clone()));
    window.present();
    if let Some(view) = &view {
        view.grab_focus();
        if let Some(anchor) = anchor {
            view.hold_anchor(anchor, ANCHOR_HOLD);
        }
    }
    tracing::info!("the PDF preview is in its own window");
}

/// `dir` with the home directory shown as `~`.
fn tilde(dir: &std::path::Path) -> String {
    let home = glib::home_dir();
    match dir.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

fn remember_size(detach: &Weak<Detach>, window: &adw::Window) {
    if let Some(detach) = detach.upgrade() {
        let (w, h) = (window.default_width(), window.default_height());
        if w > 0 && h > 0 {
            detach.size.set((w, h));
        }
    }
}

fn attach_now(state: &Rc<WindowState>, detach: &Rc<Detach>) {
    let Some(window) = detach.window.take() else {
        return;
    };
    if let Some(handler) = detach.pane_handler.take() {
        state.pdf_pane.disconnect(handler);
    }
    let view = state.pdf.get().map(|p| p.view().clone());
    let anchor = view.as_ref().and_then(|v| v.anchor());
    let content = window
        .content()
        .and_downcast::<adw::ToolbarView>()
        .and_then(|t| t.content());
    gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
    if let Some(toolbar_view) = window.content().and_downcast::<adw::ToolbarView>() {
        toolbar_view.set_content(None::<&gtk::Widget>);
    }
    if let Some(content) = &content {
        state.pdf_pane.set_child(Some(content));
    }
    state.pdf_pane.set_visible(true);
    detach.attaching.set(true);
    window.destroy();
    detach.attaching.set(false);
    if let (Some(view), Some(anchor)) = (view, anchor) {
        view.hold_anchor(anchor, ANCHOR_HOLD);
    }
    tracing::info!("the PDF preview is back in the main window");
}
