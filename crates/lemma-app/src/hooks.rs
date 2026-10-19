//! Callback lists for the shell's `connect_*` methods (window, agent controller, saver).
//!
//! Callbacks run on a snapshot of the list, so a callback may connect or disconnect others
//! (including itself) while it runs. Ids are unique per thread, so one `disconnect(id)` can serve
//! several lists.
//!
//! [`not_available`] is what the M3 stubs (WP3.0) do until their work package fills them.

use crate::window::WindowState;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// What an action whose module is still a stub does: a toast saying `feature` (a sentence-case
/// noun phrase, "The command palette") is not available yet, and a log line naming the work
/// package that fills it.
pub fn not_available(state: &WindowState, feature: &str, work_package: &str) {
    tracing::info!("{feature}: a stub until {work_package}");
    state.toast(&format!("{feature} is not available yet"));
}

/// What a scripted demo whose module is still a stub returns.
pub fn demo_not_available(demo: &str, work_package: &str) -> Result<(), String> {
    Err(format!(
        "--demo {demo} is not available yet ({work_package} fills it)"
    ))
}

/// Identifies a connected callback; pass it to the matching `disconnect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HookId(u64);

impl HookId {
    fn next() -> Self {
        thread_local! {
            static NEXT: Cell<u64> = const { Cell::new(1) };
        }
        NEXT.with(|n| {
            let id = n.get();
            n.set(id + 1);
            Self(id)
        })
    }
}

/// A list of callbacks of type `F` (e.g. `dyn Fn(&Event)`).
pub struct Hooks<F: ?Sized>(RefCell<Vec<(HookId, Rc<F>)>>);

impl<F: ?Sized> Default for Hooks<F> {
    fn default() -> Self {
        Self(RefCell::new(Vec::new()))
    }
}

impl<F: ?Sized> Hooks<F> {
    /// Adds `f`; returns its id.
    pub fn add(&self, f: Rc<F>) -> HookId {
        let id = HookId::next();
        self.0.borrow_mut().push((id, f));
        id
    }

    /// Removes the callback `id`; returns false when this list does not have it.
    pub fn remove(&self, id: HookId) -> bool {
        let mut list = self.0.borrow_mut();
        let before = list.len();
        list.retain(|(i, _)| *i != id);
        list.len() != before
    }

    /// The callbacks, in the order they were added.
    pub fn snapshot(&self) -> Vec<Rc<F>> {
        self.0.borrow().iter().map(|(_, f)| f.clone()).collect()
    }

    /// True when no callback is connected.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_and_snapshot() {
        let hooks: Hooks<dyn Fn(u32) -> u32> = Hooks::default();
        assert!(hooks.is_empty());
        let a = hooks.add(Rc::new(|x| x + 1));
        let b = hooks.add(Rc::new(|x| x * 10));
        assert_ne!(a, b);
        let results: Vec<u32> = hooks.snapshot().iter().map(|f| f(2)).collect();
        assert_eq!(results, [3, 20]);
        assert!(hooks.remove(a));
        assert!(!hooks.remove(a));
        let results: Vec<u32> = hooks.snapshot().iter().map(|f| f(2)).collect();
        assert_eq!(results, [20]);
        let other: Hooks<dyn Fn()> = Hooks::default();
        assert!(
            !other.remove(b),
            "ids are not shared between lists by accident"
        );
    }
}
