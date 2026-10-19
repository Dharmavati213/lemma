//! Auto-save.
//!
//! `[editor] autosave` (read live, so a change applies at once):
//!
//! - `"off"` (the default): nothing happens by itself;
//! - `"focus"`: the edited documents are saved when the window loses the keyboard focus;
//! - `"idle"`: they are saved [`IDLE_DELAY`] (2 s) after the last change.
//!
//! "Edited" means the open documents with unsaved changes; a change is the user's (typing,
//! undo and redo, accepting or rejecting a hunk), never an agent's or a reload's. Saves go
//! through the normal save path ([`WindowState::save_document`]): a file another program
//! changed is not overwritten (the Overwrite toast offers it), and a build follows when
//! `[build] on_save` is set. While an agent turn runs nothing is saved; a save that came due is
//! made when the turn ends (two seconds later for `"idle"`, at once for `"focus"` if the window
//! is still in the background).
//!
//! Hooks: [`install`] once per window; [`attach`] / [`detach`] per editor tab. Tests:
//! [`set_idle_delay`], [`is_scheduled`], [`is_deferred`], [`focus_lost`], [`save_now`].

use crate::document::{DocRef, ObserverId};
use crate::editor::Editor;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_config::Autosave;
use lemma_core::Origin;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// `"idle"` saves this long after the last change.
pub const IDLE_DELAY: Duration = Duration::from_secs(2);

/// One window's auto-saver.
struct Saver {
    state: Weak<WindowState>,
    timer: RefCell<Option<glib::SourceId>>,
    delay: Cell<Duration>,
    /// A save came due while an agent turn ran.
    deferred: Cell<bool>,
    /// Each attached editor's document and edit observer.
    observers: RefCell<Vec<(Weak<Editor>, DocRef, ObserverId)>>,
}

thread_local! {
    static SAVERS: RefCell<Vec<Rc<Saver>>> = const { RefCell::new(Vec::new()) };
}

fn saver(state: &WindowState) -> Option<Rc<Saver>> {
    SAVERS.with_borrow_mut(|list| {
        list.retain(|s| s.state.strong_count() > 0);
        list.iter()
            .find(|s| std::ptr::eq(s.state.as_ptr(), state))
            .cloned()
    })
}

fn mode(state: &WindowState) -> Autosave {
    state.config.editor.autosave
}

/// Watches the window's focus and the agent's turns.
pub fn install(state: &Rc<WindowState>) {
    let saver = Rc::new(Saver {
        state: Rc::downgrade(state),
        timer: RefCell::new(None),
        delay: Cell::new(IDLE_DELAY),
        deferred: Cell::new(false),
        observers: RefCell::new(Vec::new()),
    });
    SAVERS.with_borrow_mut(|list| list.push(saver.clone()));
    let weak = Rc::downgrade(state);
    state.window.connect_is_active_notify(move |window| {
        if !window.is_active()
            && let Some(state) = weak.upgrade()
        {
            focus_lost(&state);
        }
    });
    if let Some(agent) = state.agent_controller() {
        let weak = Rc::downgrade(&saver);
        agent.connect_state_changed(move |agent_state| {
            if let Some(saver) = weak.upgrade()
                && !matches!(agent_state, crate::agent::AgentState::Running { .. })
                && saver.deferred.replace(false)
            {
                saver.turn_ended();
            }
        });
    }
}

/// Restarts the idle timer on every change the user makes to `editor`'s document.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(saver) = saver(state) else {
        return;
    };
    let doc = editor.document().clone();
    let weak = Rc::downgrade(&saver);
    let id = doc.connect_edit(move |_, _, origin| {
        if matches!(origin, Origin::User | Origin::History | Origin::Review)
            && let Some(saver) = weak.upgrade()
        {
            saver.changed();
        }
    });
    saver
        .observers
        .borrow_mut()
        .push((Rc::downgrade(editor), doc, id));
}

/// Undoes [`attach`].
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(saver) = saver(state) else {
        return;
    };
    saver.observers.borrow_mut().retain(|(e, doc, id)| {
        let this = e.as_ptr() == Rc::as_ptr(editor);
        if this {
            doc.disconnect(*id);
        }
        !this && e.strong_count() > 0
    });
}

/// The window lost the focus: `"focus"` saves now.
pub fn focus_lost(state: &Rc<WindowState>) {
    if mode(state) == Autosave::Focus {
        save_now(state);
    }
}

/// Saves every open document with unsaved changes, unless auto-save is off or an agent turn
/// runs (then the save is made when the turn ends). Returns how many saves started.
pub fn save_now(state: &Rc<WindowState>) -> usize {
    if mode(state) == Autosave::Off {
        return 0;
    }
    if state.agent_controller().is_some_and(|a| a.is_running()) {
        if let Some(saver) = saver(state) {
            saver.deferred.set(true);
        }
        return 0;
    }
    let mut started = 0;
    for editor in state.editors() {
        let doc = editor.document();
        if doc.is_dirty() {
            state.save_document(doc);
            started += 1;
        }
    }
    started
}

/// Sets the idle delay (tests).
pub fn set_idle_delay(state: &WindowState, delay: Duration) {
    if let Some(saver) = saver(state) {
        saver.delay.set(delay);
    }
}

/// True while an idle save is scheduled.
pub fn is_scheduled(state: &WindowState) -> bool {
    saver(state).is_some_and(|s| s.timer.borrow().is_some())
}

/// True while a save waits for the agent's turn to end.
pub fn is_deferred(state: &WindowState) -> bool {
    saver(state).is_some_and(|s| s.deferred.get())
}

impl Saver {
    /// The user changed a document.
    fn changed(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if mode(&state) == Autosave::Idle {
            self.schedule();
        }
    }

    /// (Re)starts the idle timer.
    fn schedule(self: &Rc<Self>) {
        if let Some(old) = self.timer.take() {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(self.delay.get(), move || {
            let Some(saver) = weak.upgrade() else {
                return;
            };
            saver.timer.take();
            if let Some(state) = saver.state.upgrade()
                && mode(&state) == Autosave::Idle
            {
                save_now(&state);
            }
        });
        self.timer.replace(Some(id));
    }

    /// The agent's turn ended with a save due.
    fn turn_ended(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        match mode(&state) {
            Autosave::Idle => self.schedule(),
            Autosave::Focus if !state.window.is_active() => {
                save_now(&state);
            }
            _ => {}
        }
    }
}
