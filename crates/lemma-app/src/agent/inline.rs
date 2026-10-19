//! Ctrl+K inline edit (WP2.5; QoL P1).
//!
//! # Contract
//!
//! [`install`] (once per window) adds the action `win.agent-inline-edit` with the default
//! accelerator Ctrl+K ([`crate::app::set_default_accels`]); it runs [`open`] on the active editor.
//!
//! [`open`] marks the region (the selection, or the paragraph at the cursor without one) with a
//! light tint and shows a popover below it with a one-line entry. Enter ([`submit`]) sends a
//! prompt with `PromptOrigin::Inline { path, range }` (UTF-8 byte offsets of the region at that
//! moment) whose text is the user's instruction followed by a note that tells the agent to
//! change **only** that region of that file, with `mcp__lemma__edit`; the region itself goes
//! along as a resource (`file://…#L<a>-L<b>`). The popover then shows progress (the tool the
//! agent is running, or that it waits for a permission answer in the panel) and closes when the
//! turn ends; its hunks are ordinary hunks. An error the agent reports shows at once, and the
//! popover stays open with it when the turn ends (an `Error` comes before the `TurnEnded` of a
//! prompt the agent answered with an error), ready for another try; an error alone does not end
//! the edit (it may be about something else: a refused model, another prompt), so Esc still
//! cancels the turn. A start of the agent that fails (its state becomes `Failed`) or a stop
//! ends it with the reason. Esc ([`cancel`]) cancels the turn, also while the prompt still waits
//! for the agent to start, or just closes the popover before anything was sent. The popover
//! follows the region when the view scrolls.

use super::{AgentController, AgentState, PromptBlock, PromptOrigin, panel};
use crate::editor::Editor;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_agent::AgentEvent;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// The tag that tints the region being edited.
pub const REGION_TAG: &str = "lemma-inline-region";
/// The CSS class of the inline-edit popover.
const POPOVER_CSS_CLASS: &str = "lemma-inline-edit";

/// One open inline edit.
struct Session {
    editor: Weak<Editor>,
    popover: gtk::Popover,
    entry: gtk::Entry,
    spinner: adw::Spinner,
    status: gtk::Label,
    start: gtk::TextMark,
    end: gtk::TextMark,
    /// The prompt was sent and its turn has not ended.
    running: Cell<bool>,
    /// The last error the agent reported while the turn ran.
    error: RefCell<Option<String>>,
    /// [`close`] ran.
    closed: Cell<bool>,
    /// Controller callbacks (events, state changes) and view handlers to disconnect.
    events: RefCell<Vec<super::SignalId>>,
    scroll: RefCell<Option<(glib::WeakRef<gtk::Adjustment>, glib::SignalHandlerId)>>,
    unrealize: RefCell<Option<glib::SignalHandlerId>>,
}

thread_local! {
    static SESSIONS: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Session>)>> =
        const { RefCell::new(Vec::new()) };
}

fn current(state: &WindowState) -> Option<Rc<Session>> {
    SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain(|(w, _)| w.upgrade().is_some());
        sessions
            .iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .map(|(_, s)| s.clone())
    })
}

fn set_session(state: &WindowState, session: Option<Rc<Session>>) {
    SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain(|(w, _)| w.upgrade().is_some_and(|w| w != state.window));
        if let Some(session) = session {
            sessions.push((state.window.downgrade(), session));
        }
    });
}

/// Adds `win.agent-inline-edit` (see the module docs).
pub fn install(state: &Rc<WindowState>) {
    let action = gio::SimpleAction::new("agent-inline-edit", None);
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, _| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        match state.active_editor() {
            Some(editor) => open(&state, &editor),
            None => state.toast(&format!("Open a document to edit it with {}", who(&state))),
        }
    });
    state.window.add_action(&action);
    crate::app::set_default_accels(state, &[("win.agent-inline-edit", &["<Control>k"])]);
}

/// The paragraph around `at`: from the line after the previous blank line to the end of the
/// line before the next one (both iters on `at`'s buffer). On a blank line, the empty range at
/// its start.
pub fn paragraph_bounds(at: &gtk::TextIter) -> (gtk::TextIter, gtk::TextIter) {
    let blank = |iter: &gtk::TextIter| {
        let mut end = *iter;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        let mut start = *iter;
        start.set_line_offset(0);
        start.slice(&end).trim().is_empty()
    };
    let mut start = *at;
    start.set_line_offset(0);
    if blank(&start) {
        return (start, start);
    }
    loop {
        let mut prev = start;
        if !prev.backward_line() || blank(&prev) {
            break;
        }
        start = prev;
    }
    let mut end = *at;
    end.set_line_offset(0);
    loop {
        let mut next = end;
        if !next.forward_line() || blank(&next) {
            break;
        }
        end = next;
    }
    if !end.ends_line() {
        end.forward_to_line_end();
    }
    (start, end)
}

/// Opens the inline-edit popover in `editor` (see the module docs).
pub fn open(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(controller) = state.agent_controller().cloned() else {
        return;
    };
    if panel::is_busy(&controller) {
        state.toast(&format!(
            "{} is still working on the last request; wait for it or stop it",
            who(state)
        ));
        return;
    }
    if let Some(old) = current(state) {
        if old.running.get() {
            state.toast("An inline edit is still running");
            return;
        }
        close(state, &old, Unparent::Now);
    }
    let buffer = editor.buffer();
    let (start, end) = buffer
        .selection_bounds()
        .unwrap_or_else(|| paragraph_bounds(&buffer.iter_at_mark(&buffer.get_insert())));
    let start_mark = buffer.create_mark(None, &start, true);
    let end_mark = buffer.create_mark(None, &end, false);
    let tag = region_tag(buffer);
    buffer.apply_tag(&tag, &start, &end);

    let (a, b) = line_span(&start, &end);
    let name = editor.document().display_name();
    let heading = gtk::Label::builder()
        .label(if a == b {
            format!("Edit line {a} of {name} with {}", who(state))
        } else {
            format!("Edit lines {a}–{b} of {name} with {}", who(state))
        })
        .xalign(0.0)
        .build();
    heading.add_css_class("caption-heading");
    heading.add_css_class("dim-label");
    let entry = gtk::Entry::builder()
        .placeholder_text("Describe the change, then press Enter")
        .width_chars(42)
        .activates_default(false)
        .build();
    let spinner = adw::Spinner::new();
    spinner.set_visible(false);
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(48)
        .build();
    status.add_css_class("dim-label");
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status_row.append(&spinner);
    status_row.append(&status);
    let hint = gtk::Label::builder()
        .label("Enter sends · Esc cancels")
        .xalign(0.0)
        .build();
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(4);
    content.set_margin_end(4);
    content.append(&heading);
    content.append(&entry);
    content.append(&status_row);
    content.append(&hint);
    let popover = gtk::Popover::builder()
        .child(&content)
        .position(gtk::PositionType::Bottom)
        .autohide(true)
        .has_arrow(true)
        .build();
    popover.add_css_class(POPOVER_CSS_CLASS);
    let view = editor.view();
    popover.set_parent(view);

    let session = Rc::new(Session {
        editor: Rc::downgrade(editor),
        popover: popover.clone(),
        entry: entry.clone(),
        spinner,
        status,
        start: start_mark,
        end: end_mark,
        running: Cell::new(false),
        error: RefCell::new(None),
        closed: Cell::new(false),
        events: RefCell::new(Vec::new()),
        scroll: RefCell::new(None),
        unrealize: RefCell::new(None),
    });
    set_session(state, Some(session.clone()));

    let weak = Rc::downgrade(state);
    entry.connect_activate(move |entry| {
        if let Some(state) = weak.upgrade() {
            submit(&state, &entry.text());
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(state);
    keys.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk::gdk::Key::Escape
            && let Some(state) = weak.upgrade()
        {
            cancel(&state);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    popover.add_controller(keys);
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    popover.connect_closed(move |_| {
        // Closed by a click elsewhere: a running turn goes on (the panel shows it); otherwise
        // the edit is over.
        if let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade())
            && !session.running.get()
        {
            close(&state, &session, Unparent::Later);
        }
    });
    let adjustment = view.vadjustment();
    if let Some(adjustment) = &adjustment {
        let ses = Rc::downgrade(&session);
        let handler = adjustment.connect_value_changed(move |_| {
            if let Some(session) = ses.upgrade() {
                place(&session);
            }
        });
        session
            .scroll
            .replace(Some((adjustment.downgrade(), handler)));
    }
    // The view going (its tab closed) takes the popover with it.
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    let handler = view.connect_unrealize(move |_| {
        if let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) {
            close(&state, &session, Unparent::TabGoes);
        }
    });
    session.unrealize.replace(Some(handler));

    place(&session);
    popover.popup();
    entry.grab_focus();
}

/// Points the popover at the region's last line.
fn place(session: &Session) {
    let Some(editor) = session.editor.upgrade() else {
        return;
    };
    let view = editor.view();
    let buffer = editor.buffer();
    let start = buffer.iter_at_mark(&session.start);
    let end = buffer.iter_at_mark(&session.end);
    let first = view.iter_location(&start);
    let last = view.iter_location(&end);
    let (x, y) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, first.x(), last.y());
    let height = last.height().max(1);
    let visible = view.visible_rect();
    let width = view.width();
    let x = x.clamp(0, (width - 1).max(0));
    // Keep the arrow inside the view while the region is scrolled away.
    let y = y.clamp(0, (visible.height() - height).max(0));
    session
        .popover
        .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x, y, 1, height)));
}

fn region_tag(buffer: &sourceview5::Buffer) -> gtk::TextTag {
    let table = buffer.tag_table();
    if let Some(tag) = table.lookup(REGION_TAG) {
        return tag;
    }
    let tag = gtk::TextTag::builder()
        .name(REGION_TAG)
        .background_rgba(&gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 0.13))
        .paragraph_background_rgba(&gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 0.07))
        .build();
    table.add(&tag);
    tag
}

/// 1-based first and last line of `[start, end)` (a region ending at a line start does not
/// include that line).
fn line_span(start: &gtk::TextIter, end: &gtk::TextIter) -> (i32, i32) {
    let a = start.line() + 1;
    let mut b = end.line() + 1;
    if end.starts_line() && end.offset() > start.offset() {
        b -= 1;
    }
    (a, b.max(a))
}

/// The popover of the open inline edit, if any.
pub fn popover(state: &WindowState) -> Option<gtk::Popover> {
    current(state).map(|s| s.popover.clone())
}

/// The entry of the open inline edit, if any.
pub fn entry(state: &WindowState) -> Option<gtk::Entry> {
    current(state).map(|s| s.entry.clone())
}

/// The progress text of the open inline edit, if any.
pub fn status(state: &WindowState) -> Option<String> {
    current(state).map(|s| s.status.text().to_string())
}

/// Sends `instruction` for the open inline edit, as Enter does (see the module docs).
pub fn submit(state: &Rc<WindowState>, instruction: &str) {
    let Some(session) = current(state) else {
        return;
    };
    let instruction = instruction.trim();
    if instruction.is_empty() || session.running.get() {
        return;
    }
    let (Some(editor), Some(controller)) =
        (session.editor.upgrade(), state.agent_controller().cloned())
    else {
        return;
    };
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    let start = buffer.iter_at_mark(&session.start);
    let end = buffer.iter_at_mark(&session.end);
    let range = doc.byte_offset(&start)..doc.byte_offset(&end);
    let region = buffer.text(&start, &end, false).to_string();
    let (a, b) = line_span(&start, &end);
    let file = crate::review_ui::relative_name(state, doc.path());
    let text = inline_prompt(instruction, &file, a, b, region.is_empty());
    let uri = glib::filename_to_uri(doc.path(), None)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", doc.path().display()));
    let blocks = vec![
        PromptBlock::Text(text),
        PromptBlock::Resource {
            uri: format!("{uri}#L{a}-L{b}"),
            mime: panel::mime_for(doc.path()).into(),
            text: region,
        },
    ];
    let origin = PromptOrigin::Inline {
        path: doc.path().to_path_buf(),
        range,
    };
    // Follow the turn before sending: the stub bridge answers at once.
    follow_turn(state, &session, &controller);
    session.error.replace(None);
    session.running.set(true);
    // Read-only rather than insensitive: an insensitive entry loses the focus without a
    // focus-out, and GtkText then warns from its cursor blink ("did not receive a focus-out
    // event"); the entry also keeps the focus, so Esc still reaches the popover.
    session.entry.set_editable(false);
    session.spinner.set_visible(true);
    session
        .status
        .set_text(&format!("{} is editing…", who(state)));
    if !panel::send_prompt(state, blocks, origin) {
        let busy = format!("{} is busy; try again when it has finished", who(state));
        finish(state, &session, Some(&busy));
    }
}

/// What messages call the window's agent ("Claude", "Pi").
fn who(state: &WindowState) -> &'static str {
    super::bridge::agent_label(state.config.agent.backend)
}

/// The prompt text of an inline edit of lines `a`–`b` of `file`.
pub fn inline_prompt(instruction: &str, file: &str, a: i32, b: i32, empty: bool) -> String {
    let lines = if a == b {
        format!("line {a}")
    } else {
        format!("lines {a}–{b}")
    };
    let region = if empty {
        format!("the empty region at {lines} of {file}: insert the new text there")
    } else {
        format!("{lines} of {file}, the region attached below as it is now")
    };
    format!(
        "{instruction}\n\n(Inline edit of {region}. Change only this region: make the change \
         with mcp__lemma__edit on {file}, taking old_string from the region, and do not edit \
         anything outside it or any other file. Reply with one short sentence.)"
    )
}

fn follow_turn(state: &Rc<WindowState>, session: &Rc<Session>, controller: &Rc<AgentController>) {
    for id in session.events.take() {
        controller.disconnect(id);
    }
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(session));
    let events = controller.connect_event(move |event| {
        let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) else {
            return;
        };
        if !session.running.get() {
            return;
        }
        // An `Error` is not this turn's end: it may be about anything else (another prompt
        // that was refused, a model the agent did not accept). It shows now, and stays when
        // the turn ends. A failed start shows in the agent's state.
        match event {
            AgentEvent::ToolCall { title, kind, .. } => {
                let (name, _) = panel::tool_display(title, kind);
                session.status.set_text(&format!("{name}…"));
            }
            AgentEvent::Permission { .. } => {
                session
                    .status
                    .set_text("Waiting for your answer in the agent panel");
            }
            AgentEvent::Error(message) => {
                let message = super::markdown::without_nul(message).into_owned();
                session.status.set_text(&message);
                session.error.replace(Some(message));
            }
            AgentEvent::TurnEnded(_) => {
                let error = session.error.take();
                finish(&state, &session, error.as_deref());
            }
            AgentEvent::Exited(_) => {
                finish(&state, &session, Some(&format!("{} exited", who(&state))));
            }
            _ => {}
        }
    });
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(session));
    let states = controller.connect_state_changed(move |agent| {
        let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) else {
            return;
        };
        if !session.running.get() {
            return;
        }
        match agent {
            AgentState::Failed(message) => finish(&state, &session, Some(message)),
            AgentState::Stopped => {
                let stopped = format!("{} was stopped", who(&state));
                finish(&state, &session, Some(&stopped));
            }
            _ => {}
        }
    });
    session.events.replace(vec![events, states]);
}

/// Ends the turn's part of the session: closes the popover, or (with `error`) shows the error
/// and lets the user try again.
fn finish(state: &Rc<WindowState>, session: &Rc<Session>, error: Option<&str>) {
    session.running.set(false);
    session.spinner.set_visible(false);
    if let Some(controller) = state.agent_controller() {
        for id in session.events.take() {
            controller.disconnect(id);
        }
    }
    match error {
        Some(message) => {
            session
                .status
                .set_text(&super::markdown::without_nul(message));
            session.entry.set_editable(true);
            if !session.popover.is_visible() {
                close(state, session, Unparent::Now);
            }
        }
        None => close(state, session, Unparent::Now),
    }
}

/// Esc: cancels the turn (also a prompt that still waits for the agent to start), then
/// closes.
pub fn cancel(state: &Rc<WindowState>) {
    let Some(session) = current(state) else {
        return;
    };
    if session.running.get()
        && let Some(controller) = state.agent_controller()
        && panel::is_busy(controller)
        && panel::get(state).is_none_or(|p| p.command(&panel::PanelCommand::Cancel))
    {
        controller.cancel();
    }
    session.running.set(false);
    // Esc arrives inside the popover's key handling: unparent it afterwards.
    close(state, &session, Unparent::Later);
}

/// When [`close`] unparents the popover.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unparent {
    /// Right away (the view may be about to go).
    Now,
    /// From an idle callback (inside the popover's own signal or event handling).
    Later,
    /// Right away, because the view leaves the window (its tab closes): no focus for the view,
    /// and a shown popover is not popped down (see [`close`]).
    TabGoes,
}

/// Removes the session: popover, tint, marks and handlers. Runs once per session.
fn close(state: &WindowState, session: &Rc<Session>, unparent: Unparent) {
    if session.closed.replace(true) {
        return;
    }
    if current(state).is_some_and(|s| Rc::ptr_eq(&s, session)) {
        set_session(state, None);
    }
    if let Some(controller) = state.agent_controller() {
        for id in session.events.take() {
            controller.disconnect(id);
        }
    }
    if let Some((adjustment, handler)) = session.scroll.take()
        && let Some(adjustment) = adjustment.upgrade()
    {
        adjustment.disconnect(handler);
    }
    let editor = session.editor.upgrade();
    if let Some(editor) = &editor {
        if let Some(handler) = session.unrealize.take() {
            editor.view().disconnect(handler);
        }
        let buffer = editor.buffer();
        if !session.start.is_deleted() && !session.end.is_deleted() {
            let (start, end) = (
                buffer.iter_at_mark(&session.start),
                buffer.iter_at_mark(&session.end),
            );
            if let Some(tag) = buffer.tag_table().lookup(REGION_TAG) {
                buffer.remove_tag(&tag, &start, &end);
            }
            buffer.delete_mark(&session.start);
            buffer.delete_mark(&session.end);
        }
    }
    let popover = session.popover.clone();
    // Take the focus out of the popover before it goes: GtkText warns ("did not receive a
    // focus-out event") when it is unrealized while it still has the focus, as when the window
    // closes with the popover open.
    if let Some(root) = popover.root()
        && root.focus().is_some_and(|f| f.is_ancestor(&popover))
    {
        let view_took_it = unparent != Unparent::TabGoes
            && editor
                .as_ref()
                .is_some_and(|e| e.view().is_mapped() && e.view().grab_focus());
        if !view_took_it {
            root.set_focus(None::<&gtk::Widget>);
        }
    }
    // Popping a popover down while its view leaves the window makes GTK queue a draw on the
    // view later, when it is gone (a Gtk-CRITICAL when the window closes): unparenting it is
    // enough then.
    if unparent != Unparent::TabGoes && popover.is_visible() {
        popover.popdown();
    }
    match unparent {
        Unparent::Now => popover.unparent(),
        Unparent::TabGoes => {
            popover.unparent();
            // Earlier sessions' popovers whose deferred unparent has not run yet go too: a
            // GtkTextView disposed with a popover still parented loops forever.
            if let Some(editor) = &editor {
                let mut child = editor.view().first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    if widget.is::<gtk::Popover>() && widget.has_css_class(POPOVER_CSS_CLASS) {
                        widget.unparent();
                    }
                }
            }
        }
        Unparent::Later => {
            glib::idle_add_local_once(move || {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
        }
    }
    if let Some(editor) = editor
        && unparent != Unparent::TabGoes
        && editor.view().is_mapped()
    {
        editor.view().grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text() {
        let text = inline_prompt("Make it shorter", "main.tex", 12, 14, false);
        assert!(text.starts_with("Make it shorter\n\n(Inline edit of lines 12–14 of main.tex"));
        assert!(text.contains("mcp__lemma__edit"));
        assert!(text.contains("do not edit anything outside it"));
        assert!(text.contains("region"));
        let text = inline_prompt("Add a remark", "a.tex", 3, 3, true);
        assert!(text.contains("the empty region at line 3 of a.tex: insert"));
    }
}
