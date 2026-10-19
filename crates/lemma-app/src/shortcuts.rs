//! The keyboard shortcuts window.
//!
//! `win.shortcuts` (Ctrl+?) shows an `adw::ShortcutsDialog` (libadwaita 1.9's replacement for
//! `gtk::ShortcutsWindow`, which GTK 4.22 deprecates; it has its own search) generated from the
//! same registry as the command palette ([`sections`]), one section per
//! `app::ShortcutGroup`, in `ShortcutGroup::ALL` order (General, Editing, Navigation, Review,
//! Agent, Build and PDF, Vim):
//!
//! - every command of the palette (`palette::commands`: the described actions, the quick
//!   actions of `prompts.toml` and any other registered action) that has an accelerator right
//!   now, read from the accel registry (so `[keys]` overrides show and removed bindings are
//!   gone), with all its alternatives;
//! - the keys that are not accelerators (`app::KEY_LAYER_SHORTCUTS`: Tab, Ctrl+Space, `%`, the
//!   permission card keys, …), after the accelerators of their group; a mouse gesture
//!   (Ctrl+Click) is listed with its modifier as the keycap and the click spelled out in the
//!   subtitle;
//! - in the Vim group, first the `[vim.normal]` bindings (`VimCfg::normal_bindings`, so
//!   remapped and removed ones show as configured), as key sequences (`g` then `d`), titled
//!   with the label of the action they run.
//!
//! The dialog is built when it opens, so it always shows the current bindings; opening it
//! again while it is open does nothing. It opens with its search entry focused (type to
//! filter); Escape with nothing typed closes it at once (libadwaita would first only stop the
//! search).
//!
//! Hooks: [`show`] is the action. Tests use [`sections`], [`key_accelerator`] and [`dialog`].

use crate::app::{self, ShortcutGroup};
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// One line of the shortcuts window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutEntry {
    pub title: String,
    pub subtitle: String,
    /// The keys, in `adw::ShortcutLabel` syntax: alternatives separated by spaces, sequences
    /// by `+` (`<Control>s`, `Tab <Shift>Tab`, `g+d`).
    pub accel: String,
}

/// Converts a key as `app::KEY_LAYER_SHORTCUTS` writes it ("Tab / Shift+Tab", "Ctrl+Space",
/// "%", "y / Shift+Y / n") into `adw::ShortcutLabel` syntax ("Tab <Shift>Tab", "<Control>space",
/// "percent", "y <Shift>y n"). A mouse click ("Ctrl+Click") gives its modifier's key
/// ("Control_L") and `true`; `None` when a key is unknown.
pub fn key_accelerator(label: &str) -> Option<(String, bool)> {
    let mut alternatives = Vec::new();
    let mut click = false;
    // "Ctrl+Click in the PDF": the keys are the first word.
    let keys = label
        .split_whitespace()
        .next()
        .filter(|_| label.contains("Click"));
    let keys = keys.unwrap_or(label);
    for alternative in keys.split(" / ") {
        let parts: Vec<&str> = alternative.split('+').collect();
        let (key, mods) = parts.split_last()?;
        let mut accel = String::new();
        for m in mods {
            accel.push_str(match *m {
                "Ctrl" | "Control" => "<Control>",
                "Shift" => "<Shift>",
                "Alt" => "<Alt>",
                "Super" => "<Super>",
                _ => return None,
            });
        }
        if *key == "Click" {
            // A click with a modifier: the modifier's own key.
            let first = mods.first()?;
            click = true;
            alternatives.push(
                match *first {
                    "Ctrl" | "Control" => "Control_L",
                    "Shift" => "Shift_L",
                    "Alt" => "Alt_L",
                    _ => return None,
                }
                .to_owned(),
            );
            continue;
        }
        let name = match *key {
            "Space" => "space".to_owned(),
            "Tab" | "Return" | "Escape" | "BackSpace" | "Delete" => (*key).to_owned(),
            k if k.chars().count() == 1 => {
                let c = k.chars().next()?;
                let lower = c.to_lowercase().next()?;
                let mut accel_key = crate::palette::vim_accelerator(&lower.to_string())?;
                // "Ctrl+O" is Ctrl and o; a capital alone ("Y") is typed with Shift.
                if c.is_uppercase() && mods.is_empty() {
                    accel_key = format!("<Shift>{accel_key}");
                }
                accel_key
            }
            k if k.starts_with('F') && k[1..].parse::<u8>().is_ok() => k.to_owned(),
            _ => return None,
        };
        accel.push_str(&name);
        alternatives.push(accel);
    }
    Some((alternatives.join(" "), click))
}

/// The contents of the shortcuts window: every group of `ShortcutGroup::ALL`, in order (see the
/// module docs).
pub fn sections(state: &WindowState) -> Vec<(ShortcutGroup, Vec<ShortcutEntry>)> {
    let mut sections: Vec<(ShortcutGroup, Vec<ShortcutEntry>)> = ShortcutGroup::ALL
        .iter()
        .map(|g| (*g, Vec::new()))
        .collect();
    let mut push = |group: ShortcutGroup, entry: ShortcutEntry| {
        if let Some((_, list)) = sections.iter_mut().find(|(g, _)| *g == group) {
            list.push(entry);
        }
    };
    let commands = crate::palette::commands(state);
    for c in &commands {
        if c.accels.is_empty() {
            continue;
        }
        push(
            c.group,
            ShortcutEntry {
                title: c.label.clone(),
                subtitle: if c.quick {
                    "Agent quick action".to_owned()
                } else {
                    String::new()
                },
                accel: c.accels.join(" "),
            },
        );
    }
    // The Vim bindings, in the order of the actions they run.
    let position = |action: &str| {
        commands
            .iter()
            .position(|c| c.name == action)
            .unwrap_or(usize::MAX)
    };
    let mut bindings: Vec<(String, String)> =
        state.config.vim.normal_bindings().into_iter().collect();
    bindings.sort_by_key(|(keys, action)| (position(action), keys.clone()));
    for (keys, action) in bindings {
        let Some(accel) = crate::palette::vim_accelerator(&keys) else {
            continue;
        };
        let title = commands
            .iter()
            .find(|c| c.name == action)
            .map(|c| c.label.clone())
            .or_else(|| app::action_info(&action).map(|d| d.label))
            .unwrap_or_else(|| action.clone());
        push(
            ShortcutGroup::Vim,
            ShortcutEntry {
                title,
                subtitle: "Normal mode".to_owned(),
                accel,
            },
        );
    }
    for (keys, what, group) in app::KEY_LAYER_SHORTCUTS {
        let Some((accel, click)) = key_accelerator(keys) else {
            tracing::warn!("shortcuts: cannot show {keys:?}");
            continue;
        };
        let mut chars = what.chars();
        let title = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect::<String>())
            .unwrap_or_default();
        push(
            *group,
            ShortcutEntry {
                title,
                subtitle: if click {
                    (*keys).to_owned()
                } else {
                    String::new()
                },
                accel,
            },
        );
    }
    sections
}

/// The text in the dialog's search entry.
fn search_text(dialog: &adw::ShortcutsDialog) -> String {
    let mut stack = vec![dialog.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if let Some(entry) = widget.downcast_ref::<gtk::SearchEntry>() {
            return entry.text().to_string();
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    String::new()
}

thread_local! {
    /// The open shortcuts dialog of each window.
    static OPEN: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, glib::WeakRef<adw::ShortcutsDialog>)>> =
        const { RefCell::new(Vec::new()) };
}

/// The open shortcuts dialog of `state`'s window (until it closes).
pub fn dialog(state: &WindowState) -> Option<adw::ShortcutsDialog> {
    OPEN.with_borrow_mut(|open| {
        open.retain(|(w, d)| w.upgrade().is_some() && d.upgrade().is_some());
        open.iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .and_then(|(_, d)| d.upgrade())
    })
}

/// `win.shortcuts` (Ctrl+?).
pub fn show(state: &Rc<WindowState>) {
    if dialog(state).is_some() {
        return;
    }
    let dialog = adw::ShortcutsDialog::new();
    for (group, entries) in sections(state) {
        if entries.is_empty() {
            continue;
        }
        let section = adw::ShortcutsSection::new(Some(group.title()));
        for entry in entries {
            let item = adw::ShortcutsItem::new(&entry.title, &entry.accel);
            if !entry.subtitle.is_empty() {
                item.set_subtitle(&entry.subtitle);
            }
            section.add(item);
        }
        dialog.add(section);
    }
    // The dialog starts with its search entry focused, where Escape only stops the search:
    // with nothing typed, Escape closes the dialog at once.
    let escape = gtk::EventControllerKey::new();
    escape.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = dialog.downgrade();
    escape.connect_key_pressed(move |_, key, _, mods| {
        let Some(dialog) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let plain = (mods & gtk::accelerator_get_default_mod_mask()).is_empty();
        if key == gtk::gdk::Key::Escape && plain && search_text(&dialog).is_empty() {
            dialog.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    dialog.add_controller(escape);
    OPEN.with_borrow_mut(|open| open.push((state.window.downgrade(), dialog.downgrade())));
    dialog.connect_closed(|dialog| {
        OPEN.with_borrow_mut(|open| {
            open.retain(|(_, d)| d.upgrade().is_some_and(|d| d != *dialog));
        });
    });
    dialog.present(Some(&state.window));
}
