//! The key layer (ARCHITECTURE §4.3, §9.3): Lemma's own key handlers, run **before**
//! GtkSourceView's Vim emulation sees a key, plus Lemma's Vim normal-mode bindings and ex
//! commands (WP2.10).
//!
//! # How it works
//!
//! Every editor gets one [`KeyLayer`] ([`install`], called by `Editor::new`): a capture-phase
//! `gtk::EventControllerKey` on the editor's scrolled window, the view's parent. Capture runs
//! from the window down to the focus widget, so the layer sees a key press before the view's
//! own capture-phase controllers — the Vim controller (`editor::vim`) among them, whenever Vim is
//! switched on. Application accelerators (Ctrl+S, F5, Ctrl+K, F10, the review chords) are handled
//! by the window before any of this. (On the view itself the layer could not be ordered before
//! Vim: GTK runs a widget's controllers newest first, and Vim's controller is added whenever Vim
//! is switched on.)
//!
//! **Verified on real input** (WP2.10, `tests/gtk_keys.rs`, which drives a private Broadway
//! daemon over its browser protocol, so the key events take GDK's real path): in Vim normal mode
//! the layer sees `x` while the character is still in the buffer and Vim deletes it afterwards;
//! a key a handler stops never reaches Vim or the view; `]h` in insert mode reaches Vim (and is
//! typed); Tab in insert mode reaches a [`PRIORITY_SNIPPETS`] handler before Vim indents; the
//! review chords, Ctrl+K and F10 run in normal, insert and visual mode and never reach the layer.
//! On the scrolled window itself controllers run newest first, so Vim's `J` guard (added when
//! Vim is switched on, see [`super::vim::Vim::blocks_key`]) runs just before the layer; it only
//! stops a `J` that would hang GtkSourceView.
//!
//! **Ctrl+W and Ctrl+O** are application accelerators (close the tab, open a file), and Vim's
//! own keys too (insert mode: delete the word before the cursor, run one normal-mode command).
//! While Vim is on and an editor has the focus they are Vim's in every mode: a capture-phase
//! controller on the window ([`install_window`]), which runs before the application's
//! accelerators, gives them to that editor's layer and Vim. The menu and header buttons, and the
//! keys while the focus is elsewhere or Vim is off, still close the tab and open a file.
//!
//! Handlers are registered per view with [`register`]`(view, priority, f)`; `f(event, mode)` runs
//! for every key press, highest priority first (equal priorities in registration order), until
//! one returns `Propagation::Stop`, which also stops the key from reaching Vim and the view.
//! `mode` is the Vim mode, and [`VimMode::Insert`] while Vim is off (plain editing types like
//! insert mode); `event.vim` tells the two apart. While a handler runs, [`current_event`] gives
//! the real `gdk::Event` (none for [`dispatch`]).
//!
//! | Priority | Registered by |
//! |---|---|
//! | [`PRIORITY_SNIPPETS`] | WP2.9a: Tab / Shift+Tab (insert mode) |
//! | [`PRIORITY_TYPING`] | WP2.9b: pairing, Backspace, Enter helpers if done on key press |
//! | [`PRIORITY_VIM_BINDINGS`] | WP2.10: `]h`, `[h`, `ga`, `gr`, `<Space>a` (normal mode, `[vim.normal]`) |
//!
//! [`dispatch`] runs the handlers of a view as the controller does; tests call it to emulate a
//! key press (GTK has no public way to inject key events).
//!
//! # Vim normal-mode bindings
//!
//! [`attach`] registers one handler per editor at [`PRIORITY_VIM_BINDINGS`] while Vim is on (it
//! follows `win.toggle-vim`). The bindings are `state.config.vim.normal_bindings()` (key
//! sequence in Vim notation → detailed action name, [`parse_keys`]), re-read on config reloads.
//! They work like Vim mappings:
//!
//! - they apply only in normal mode, and only while Vim has nothing pending except a count, so
//!   `f]`, `d]…`, `"g` and `mg` keep their Vim meaning; GtkSourceView does not show every
//!   pending command, so the key after `r`, `y`, `gq` and `g?` goes to Vim too (`r]`,
//!   `y<Space>`, see `waits_invisibly`);
//! - a key that starts a binding is held back from Vim (the status bar shows it); the next key
//!   either completes the binding, extends it, or ends it — then the held keys are given to Vim
//!   in order, followed by the new key (so `gg`, `gj`, `]]` and `<Space>` still work);
//! - after [`SEQUENCE_TIMEOUT`] (Vim's `timeoutlen`) without a next key, a complete binding
//!   runs, otherwise the held keys go to Vim;
//! - a count typed before a binding (`3]h`, `2ga`) is taken from Vim's pending command and
//!   cleared there. It repeats the review actions (`3]h` = three times `]h`, across files;
//!   `2ga` accepts two hunks as two Alt+Y presses would); other actions run once.
//!
//! The actions run from a high-priority idle callback right after the key: before the next key
//! event, but outside the key's dispatch (an action may switch or close tabs).
//!
//! # Ex commands
//!
//! [`ex_command`] (after `review_ui::ex_command`, which handles the plain `:Accept`, `:Hnext`, …):
//! `:Agent <prompt>` sends a prompt (`:Agent <quick action>` runs that template, `:Agent` alone
//! focuses the agent pane), `:AgentStop` cancels the running turn (`:AgentStop!` stops the agent
//! process), and `:Accept N`, `:Reject N`, `:Hnext N`, `:Hprev N` (or `:3Hnext`) take counts.
//! See [`parse_ex_command`].

use super::{Editor, TexView, VimMode, vim};
use crate::agent::{self, PromptBlock, PromptOrigin};
use crate::document::DocRef;
use crate::review_ui::{self, Decision};
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::gdk;
use gtk::prelude::*;
use lemma_review::HunkId;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Priority of the snippet driver's Tab handling (runs first).
pub const PRIORITY_SNIPPETS: i32 = 300;
/// Priority of the typing helpers.
pub const PRIORITY_TYPING: i32 = 200;
/// Priority of the Vim normal-mode bindings.
pub const PRIORITY_VIM_BINDINGS: i32 = 100;

/// How long a held key waits for the rest of a binding (Vim's default `timeoutlen`).
pub const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(1000);

/// Largest count a binding or ex command accepts (bigger counts are clamped).
const MAX_COUNT: u32 = 9999;

/// The inline-edit action and its default accelerator, which [`install_window`] sets when
/// nothing else did (WP2.5's `inline::install` owns the action).
const INLINE_EDIT_ACCEL: (&str, &[&str]) = ("win.agent-inline-edit", &["<Control>k"]);

/// A key press as the layer reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub keyval: gdk::Key,
    pub keycode: u32,
    pub state: gdk::ModifierType,
    /// True while Vim emulation is on in this editor.
    pub vim: bool,
}

/// A registered handler.
pub type KeyHandler = dyn Fn(&KeyEvent, VimMode) -> glib::Propagation;

/// Identifies a handler for [`unregister`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HandlerId(u64);

/// The key layer of one view (see the module docs).
pub struct KeyLayer {
    handlers: RefCell<Vec<(i32, HandlerId, Rc<KeyHandler>)>>,
    next: Cell<u64>,
    mode: Box<dyn Fn() -> Option<VimMode>>,
    controller: gtk::EventControllerKey,
    host: glib::WeakRef<gtk::Widget>,
    /// The real event while the controller runs the handlers.
    current: RefCell<Option<gdk::Event>>,
}

impl KeyLayer {
    /// The capture-phase controller (on the view's parent).
    pub fn controller(&self) -> &gtk::EventControllerKey {
        &self.controller
    }

    /// Number of registered handlers.
    pub fn len(&self) -> usize {
        self.handlers.borrow().len()
    }

    /// True when no handler is registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.borrow().is_empty()
    }

    /// [`KeyLayer::run`] with the real event available to [`current_event`].
    fn run_with(&self, event: &KeyEvent, real: Option<gdk::Event>) -> glib::Propagation {
        let previous = self.current.replace(real);
        let result = self.run(event);
        self.current.replace(previous);
        result
    }

    fn run(&self, event: &KeyEvent) -> glib::Propagation {
        let mode = (self.mode)().unwrap_or(VimMode::Insert);
        let handlers: Vec<Rc<KeyHandler>> = self
            .handlers
            .borrow()
            .iter()
            .map(|(_, _, f)| f.clone())
            .collect();
        for handler in handlers {
            if handler(event, mode) == glib::Propagation::Stop {
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    }
}

impl Drop for KeyLayer {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            host.remove_controller(&self.controller);
        }
    }
}

/// Gives `view` its key layer: a capture-phase key controller on `host` (the view's scrolled
/// window). `mode` reports the Vim mode, `None` while Vim is off. Called once by `Editor::new`.
pub fn install(
    view: &TexView,
    host: &impl IsA<gtk::Widget>,
    mode: impl Fn() -> Option<VimMode> + 'static,
) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_name(Some("lemma-key-layer"));
    let weak = view.downgrade();
    controller.connect_key_pressed(move |controller, keyval, keycode, state| {
        let Some(view) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        // Only keys for the view itself, not for a widget inside it (the review bars' buttons).
        if !view.is_focus() {
            return glib::Propagation::Proceed;
        }
        let Some(layer) = view.key_layer() else {
            return glib::Propagation::Proceed;
        };
        let event = KeyEvent {
            keyval,
            keycode,
            state,
            vim: (layer.mode)().is_some(),
        };
        let previous = layer.current.replace(controller.current_event());
        let result = layer.run(&event);
        layer.current.replace(previous);
        result
    });
    host.add_controller(controller.clone());
    view.set_key_layer(Some(Rc::new(KeyLayer {
        handlers: RefCell::new(Vec::new()),
        next: Cell::new(0),
        mode: Box::new(mode),
        controller,
        host: host.as_ref().downgrade(),
        current: RefCell::new(None),
    })));
}

/// Adds `f` to `view`'s key layer with `priority` (higher runs first). Returns `None` when the
/// view has no key layer (it was not created by `Editor::new`).
pub fn register(
    view: &TexView,
    priority: i32,
    f: impl Fn(&KeyEvent, VimMode) -> glib::Propagation + 'static,
) -> Option<HandlerId> {
    let layer = view.key_layer()?;
    let id = HandlerId(layer.next.get());
    layer.next.set(id.0 + 1);
    let mut handlers = layer.handlers.borrow_mut();
    // After every handler of the same or a higher priority.
    let at = handlers
        .iter()
        .position(|(p, _, _)| *p < priority)
        .unwrap_or(handlers.len());
    handlers.insert(at, (priority, id, Rc::new(f)));
    Some(id)
}

/// Removes a handler added by [`register`].
pub fn unregister(view: &TexView, id: HandlerId) {
    if let Some(layer) = view.key_layer() {
        layer.handlers.borrow_mut().retain(|(_, i, _)| *i != id);
    }
}

/// Runs `view`'s handlers for `event` as a real key press would (see the module docs).
pub fn dispatch(view: &TexView, event: &KeyEvent) -> glib::Propagation {
    view.key_layer()
        .map_or(glib::Propagation::Proceed, |layer| layer.run(event))
}

/// The real key-press event the layer of `view` is handling, while one of its handlers runs
/// (`None` otherwise, and for [`dispatch`]). A handler that holds a key back can give it to Vim
/// later with `VimIMContext::filter_keypress`.
pub fn current_event(view: &TexView) -> Option<gdk::Event> {
    view.key_layer()?.current.borrow().clone()
}

/// The key event the layer would report for `keyval` with `state` in `editor` (keycode 0).
pub fn event_for(editor: &Editor, keyval: gdk::Key, state: gdk::ModifierType) -> KeyEvent {
    KeyEvent {
        keyval,
        keycode: 0,
        state,
        vim: editor.vim().is_some(),
    }
}

// ----- key names (Vim notation) -----

/// Named keys of Vim's notation, in their canonical spelling, with the GDK keys that produce them.
const NAMED_KEYS: &[(&str, &[gdk::Key])] = &[
    ("Space", &[gdk::Key::space, gdk::Key::KP_Space]),
    (
        "Tab",
        &[gdk::Key::Tab, gdk::Key::ISO_Left_Tab, gdk::Key::KP_Tab],
    ),
    (
        "CR",
        &[gdk::Key::Return, gdk::Key::KP_Enter, gdk::Key::ISO_Enter],
    ),
    ("Esc", &[gdk::Key::Escape]),
    ("BS", &[gdk::Key::BackSpace]),
    ("Del", &[gdk::Key::Delete, gdk::Key::KP_Delete]),
    ("Insert", &[gdk::Key::Insert, gdk::Key::KP_Insert]),
    ("Up", &[gdk::Key::Up, gdk::Key::KP_Up]),
    ("Down", &[gdk::Key::Down, gdk::Key::KP_Down]),
    ("Left", &[gdk::Key::Left, gdk::Key::KP_Left]),
    ("Right", &[gdk::Key::Right, gdk::Key::KP_Right]),
    ("Home", &[gdk::Key::Home, gdk::Key::KP_Home]),
    ("End", &[gdk::Key::End, gdk::Key::KP_End]),
    ("PageUp", &[gdk::Key::Page_Up, gdk::Key::KP_Page_Up]),
    ("PageDown", &[gdk::Key::Page_Down, gdk::Key::KP_Page_Down]),
    ("F1", &[gdk::Key::F1]),
    ("F2", &[gdk::Key::F2]),
    ("F3", &[gdk::Key::F3]),
    ("F4", &[gdk::Key::F4]),
    ("F5", &[gdk::Key::F5]),
    ("F6", &[gdk::Key::F6]),
    ("F7", &[gdk::Key::F7]),
    ("F8", &[gdk::Key::F8]),
    ("F9", &[gdk::Key::F9]),
    ("F10", &[gdk::Key::F10]),
    ("F11", &[gdk::Key::F11]),
    ("F12", &[gdk::Key::F12]),
];

/// Other spellings Vim accepts for named keys (lower case) → canonical name. Characters that
/// need a name in mappings map to themselves (`<lt>` is `<`).
const KEY_ALIASES: &[(&str, &str)] = &[
    ("enter", "CR"),
    ("return", "CR"),
    ("escape", "Esc"),
    ("backspace", "BS"),
    ("delete", "Del"),
    ("ins", "Insert"),
    ("pageup", "PageUp"),
    ("pagedown", "PageDown"),
];

/// Keys that only modify others.
const MODIFIER_KEYS: &[gdk::Key] = &[
    gdk::Key::Shift_L,
    gdk::Key::Shift_R,
    gdk::Key::Control_L,
    gdk::Key::Control_R,
    gdk::Key::Alt_L,
    gdk::Key::Alt_R,
    gdk::Key::Meta_L,
    gdk::Key::Meta_R,
    gdk::Key::Super_L,
    gdk::Key::Super_R,
    gdk::Key::Hyper_L,
    gdk::Key::Hyper_R,
    gdk::Key::Caps_Lock,
    gdk::Key::Shift_Lock,
    gdk::Key::Num_Lock,
    gdk::Key::ISO_Level3_Shift,
    gdk::Key::ISO_Level5_Shift,
    gdk::Key::Mode_switch,
];

/// Characters with a name in Vim's notation (`<lt>`, `<Bslash>`, `<Bar>`).
const CHAR_NAMES: &[(char, &str)] = &[('<', "lt"), ('\\', "Bslash"), ('|', "Bar")];

/// The canonical name of a key in Vim notation, as [`parse_keys`] and [`key_name`] produce it:
/// a plain character (`]`, `h`, `A`), or `<…>` with the modifiers `C-` (Control), `M-` (Alt),
/// `S-` (Shift, only for named keys) in that order: `<Space>`, `<C-k>`, `<M-x>`, `<S-Tab>`,
/// `<lt>`.
fn render(ctrl: bool, alt: bool, shift: bool, name: &str) -> String {
    let plain_char = name.chars().count() == 1;
    if !ctrl && !alt && !shift && plain_char {
        return name.to_owned();
    }
    let mut out = String::from("<");
    if ctrl {
        out.push_str("C-");
    }
    if alt {
        out.push_str("M-");
    }
    if shift {
        out.push_str("S-");
    }
    out.push_str(name);
    out.push('>');
    out
}

/// The name of character `c` with `ctrl`/`alt` (Shift is part of the character).
fn char_key(c: char, ctrl: bool, alt: bool) -> String {
    if c == ' ' {
        return render(ctrl, alt, false, "Space");
    }
    // <C-A> is <C-a> in Vim: a terminal cannot tell them apart.
    let c = if ctrl { c.to_ascii_lowercase() } else { c };
    match CHAR_NAMES.iter().find(|(ch, _)| *ch == c) {
        Some((_, name)) => format!(
            "<{}{}{name}>",
            if ctrl { "C-" } else { "" },
            if alt { "M-" } else { "" }
        ),
        None => render(ctrl, alt, false, &c.to_string()),
    }
}

/// The name (Vim notation, see [`parse_keys`]) of the key press `keyval` with modifiers `state`;
/// `None` for modifier keys and keys without a name (they never take part in a binding).
/// Caps Lock, Num Lock and AltGr do not count as modifiers; Shift is part of a character
/// (`A`, `}`), and only named keys spell it (`<S-Tab>`).
pub fn key_name(keyval: gdk::Key, state: gdk::ModifierType) -> Option<String> {
    if MODIFIER_KEYS.contains(&keyval) {
        return None;
    }
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gdk::ModifierType::ALT_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
    if let Some((name, _)) = NAMED_KEYS.iter().find(|(_, keys)| keys.contains(&keyval)) {
        let shift = shift || keyval == gdk::Key::ISO_Left_Tab;
        return Some(render(ctrl, alt, shift, name));
    }
    let c = keyval.to_unicode().filter(|c| !c.is_control())?;
    Some(char_key(c, ctrl, alt))
}

/// Parses one `<…>` key name (without the brackets) into its canonical form; `None` when it is
/// not a key name.
fn parse_special(inner: &str) -> Result<String, String> {
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut rest = inner;
    // Modifier prefixes: C- M- A- S- (any case), but a trailing "-" is the key itself.
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0].to_ascii_lowercase() {
            b'c' => ctrl = true,
            b'm' | b'a' => alt = true,
            b's' => shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let lower = rest.to_ascii_lowercase();
    if let Some((name, _)) = NAMED_KEYS
        .iter()
        .find(|(n, _)| n.to_ascii_lowercase() == lower)
    {
        return Ok(render(ctrl, alt, shift, name));
    }
    if let Some((_, name)) = KEY_ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return Ok(render(ctrl, alt, shift, name));
    }
    if let Some((c, _)) = CHAR_NAMES
        .iter()
        .find(|(_, n)| n.to_ascii_lowercase() == lower)
    {
        return Ok(char_key(*c, ctrl, alt));
    }
    let mut chars = rest.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && (ctrl || alt)
        && !shift
    {
        return Ok(char_key(c, ctrl, alt));
    }
    if lower == "leader" || lower == "localleader" {
        return Err(format!(
            "<{inner}> is not supported: Lemma has no leader key, write the key itself (e.g. <Space>)"
        ));
    }
    Err(format!("unknown key <{inner}>"))
}

/// Parses a key sequence in Vim notation (`]h`, `<Space>a`, `<C-k>x`, `<lt>`) into key names, as
/// [`key_name`] reports them for key presses. Names in `<…>` are case-insensitive (`<space>`).
pub fn parse_keys(notation: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    let mut rest = notation;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
            && !rest[1..end].contains('<')
        {
            keys.push(parse_special(&rest[1..end])?);
            rest = &rest[end + 1..];
            continue;
        }
        if c.is_control() {
            return Err(format!("control character {c:?} in {notation:?}"));
        }
        keys.push(char_key(c, false, false));
        rest = &rest[c.len_utf8()..];
    }
    if keys.is_empty() {
        return Err("empty key sequence".into());
    }
    Ok(keys)
}

// ----- bindings -----

/// Lemma's Vim normal-mode bindings: key sequences → detailed action names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bindings {
    entries: Vec<(Vec<String>, String)>,
}

/// What a key sequence is for [`Bindings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// No binding starts with it.
    None,
    /// It starts a longer binding.
    Prefix,
    /// It is a binding; `longer` when it also starts a longer one.
    Exact { action: &'a str, longer: bool },
}

impl Bindings {
    /// The bindings of `normal` (`VimCfg::normal_bindings()`), and one message per entry that
    /// was skipped (bad key notation or action name).
    pub fn new(normal: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for (keys, action) in normal {
            if gio::Action::parse_detailed_name(action).is_err() || !action.contains('.') {
                errors.push(format!(
                    "[vim.normal] {keys:?}: {action:?} is not an action name like \"win.review-next\""
                ));
                continue;
            }
            match parse_keys(keys) {
                Ok(seq) => entries.push((seq, action.clone())),
                Err(e) => errors.push(format!("[vim.normal] {keys:?}: {e}")),
            }
        }
        (Self { entries }, errors)
    }

    /// True when there is no binding.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Classifies the key sequence `keys`.
    pub fn lookup(&self, keys: &[String]) -> Lookup<'_> {
        let mut exact = None;
        let mut longer = false;
        for (seq, action) in &self.entries {
            if seq.as_slice() == keys {
                exact = Some(action.as_str());
            } else if seq.len() > keys.len() && seq.starts_with(keys) {
                longer = true;
            }
        }
        match (exact, longer) {
            (Some(action), longer) => Lookup::Exact { action, longer },
            (None, true) => Lookup::Prefix,
            (None, false) => Lookup::None,
        }
    }
}

// ----- per-window and per-editor state -----

/// A key held back while it may start a binding.
struct Held {
    name: String,
    keyval: gdk::Key,
    state: gdk::ModifierType,
    /// The real event, to give to Vim as it was; `None` for [`dispatch`]ed keys.
    event: Option<gdk::Event>,
}

/// The binding being typed in one editor.
#[derive(Default)]
struct Sequence {
    held: RefCell<Vec<Held>>,
    /// The count Vim had pending when the first key was held.
    count: Cell<Option<u32>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Vim's pending command plus the last key it was given in normal mode (`"ay`, `2r`): see
    /// [`waits_invisibly`].
    typed: RefCell<String>,
}

impl Sequence {
    fn names(&self) -> Vec<String> {
        self.held.borrow().iter().map(|h| h.name.clone()).collect()
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
    }

    /// Forgets the held keys (they never reach Vim).
    fn clear(&self) -> Vec<Held> {
        self.stop_timer();
        self.count.set(None);
        self.held.take()
    }

    /// Records that Vim, with `pending` pending, was given the key `name`.
    fn note(&self, pending: &str, name: &str) {
        let mut typed = self.typed.borrow_mut();
        typed.clear();
        typed.push_str(pending);
        typed.push_str(name);
    }
}

/// True when GtkSourceView's Vim, after the keys `typed` (its visible pending command plus the
/// last key), waits for another key although its `command-text` is empty. In GtkSourceView 5.20
/// (probed key by key) that is `r` (replace a character), `y` (yank) and the `gq`/`g?`
/// operators, with an optional count and register. Their next key is their argument, never the
/// start of a binding (`r]`, `y<Space>`), nor a join or a repeat (`rJ`, `r.`; see `vim`).
pub(crate) fn waits_invisibly(typed: &str) -> bool {
    let digits = |s: &str| {
        s.trim_start_matches(|c: char| c.is_ascii_digit())
            .to_owned()
    };
    let mut rest = digits(typed);
    if let Some(register) = rest.strip_prefix('"') {
        let mut chars = register.chars();
        chars.next();
        rest = digits(chars.as_str());
    }
    matches!(rest.as_str(), "r" | "y" | "gq" | "g?")
}

/// One editor of a window.
struct Attached {
    editor: Weak<Editor>,
    seq: Rc<Sequence>,
    handler: Cell<Option<HandlerId>>,
}

/// WP2.10's state of one window.
struct WindowKeys {
    window: Weak<WindowState>,
    bindings: RefCell<Rc<Bindings>>,
    /// The `[vim.normal]` problems last reported, so a reload reports only new ones.
    errors: RefCell<Vec<String>>,
    editors: RefCell<Vec<Attached>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<WindowKeys>>> = const { RefCell::new(Vec::new()) };
}

fn window_keys(state: &WindowState) -> Option<Rc<WindowKeys>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.window.strong_count() > 0);
        windows
            .iter()
            .find(|w| std::ptr::eq(w.window.as_ptr(), state))
            .cloned()
    })
}

impl WindowKeys {
    fn bindings(&self) -> Rc<Bindings> {
        self.bindings.borrow().clone()
    }

    /// Reads the bindings from the configuration; reports problems not reported before.
    fn load_bindings(&self, state: &WindowState) {
        let (bindings, errors) = Bindings::new(&state.config.vim.normal_bindings());
        for error in &errors {
            if !self.errors.borrow().contains(error) {
                tracing::warn!("{error}");
                state.toast(error);
            }
        }
        self.errors.replace(errors);
        self.bindings.replace(Rc::new(bindings));
    }

    fn attached(&self, editor: &Editor) -> Option<Rc<Sequence>> {
        self.editors
            .borrow()
            .iter()
            .find(|a| std::ptr::eq(a.editor.as_ptr(), editor))
            .map(|a| a.seq.clone())
    }

    /// Registers the binding handler in every editor while Vim is on, and removes it while Vim
    /// is off (held keys are dropped then).
    fn sync(self: &Rc<Self>, state: &Rc<WindowState>) {
        self.editors
            .borrow_mut()
            .retain(|a| a.editor.strong_count() > 0);
        let attached: Vec<(Rc<Editor>, Rc<Sequence>, Option<HandlerId>)> = self
            .editors
            .borrow()
            .iter()
            .filter_map(|a| Some((a.editor.upgrade()?, a.seq.clone(), a.handler.get())))
            .collect();
        for (editor, seq, handler) in attached {
            let wanted = state.vim_enabled();
            let id = match (wanted, handler) {
                (true, None) => register_bindings(state, self, &editor, &seq),
                (false, Some(id)) => {
                    unregister(editor.view(), id);
                    seq.clear();
                    seq.typed.borrow_mut().clear();
                    None
                }
                (_, handler) => handler,
            };
            if let Some(a) = self
                .editors
                .borrow()
                .iter()
                .find(|a| std::ptr::eq(a.editor.as_ptr(), Rc::as_ptr(&editor)))
            {
                a.handler.set(id);
            }
        }
    }
}

fn register_bindings(
    state: &Rc<WindowState>,
    keys: &Rc<WindowKeys>,
    editor: &Rc<Editor>,
    seq: &Rc<Sequence>,
) -> Option<HandlerId> {
    let (state, keys, ed, seq) = (
        Rc::downgrade(state),
        Rc::downgrade(keys),
        Rc::downgrade(editor),
        seq.clone(),
    );
    register(
        editor.view(),
        PRIORITY_VIM_BINDINGS,
        move |event: &KeyEvent, _mode: VimMode| match (
            state.upgrade(),
            keys.upgrade(),
            ed.upgrade(),
        ) {
            (Some(state), Some(keys), Some(editor)) => on_key(&state, &keys, &editor, &seq, event),
            _ => glib::Propagation::Proceed,
        },
    )
}

// ----- the binding handler -----

/// The binding handler of one editor (see the module docs).
fn on_key(
    state: &Rc<WindowState>,
    keys: &Rc<WindowKeys>,
    editor: &Rc<Editor>,
    seq: &Rc<Sequence>,
    event: &KeyEvent,
) -> glib::Propagation {
    if !event.vim {
        return glib::Propagation::Proceed;
    }
    // Modifier keys (Shift for `]H`, …) neither extend nor end a binding.
    let Some(name) = key_name(event.keyval, event.state) else {
        return glib::Propagation::Proceed;
    };
    let bindings = keys.bindings();
    let held = Held {
        name,
        keyval: event.keyval,
        state: event.state,
        event: current_event(editor.view()),
    };
    if !seq.held.borrow().is_empty() {
        let mut names = seq.names();
        names.push(held.name.clone());
        match bindings.lookup(&names) {
            Lookup::Exact {
                action,
                longer: false,
            } => {
                let count = seq.count.get();
                seq.clear();
                seq.typed.borrow_mut().clear();
                run_binding(state, editor, action, count);
                return glib::Propagation::Stop;
            }
            Lookup::Exact { longer: true, .. } | Lookup::Prefix => {
                seq.held.borrow_mut().push(held);
                start_timer(state, keys, editor, seq);
                show_pending(state, editor, seq);
                return glib::Propagation::Stop;
            }
            // The held keys were not a binding after all: Vim gets them, then this key as if
            // it were the first. The guard judged this key before they moved the cursor.
            Lookup::None => {
                flush(state, editor, seq);
                if editor.vim_blocks_key(event.keyval, event.state) {
                    editor.view().error_bell();
                    return glib::Propagation::Stop;
                }
            }
        }
    }
    let Some(status) = editor.vim_status() else {
        return glib::Propagation::Proceed;
    };
    if status.mode != VimMode::Normal {
        seq.typed.borrow_mut().clear();
        return glib::Propagation::Proceed;
    }
    // The argument of a command whose pending state Vim does not show (`r]`).
    if status.command.is_empty() && waits_invisibly(&seq.typed.borrow()) {
        seq.typed.borrow_mut().clear();
        return glib::Propagation::Proceed;
    }
    // A pending operator, register, mark, `f`/`F` target, … takes this key as its argument.
    let count = match status.command.as_str() {
        "" => None,
        digits if digits.bytes().all(|b| b.is_ascii_digit()) => Some(
            digits
                .parse::<u32>()
                .unwrap_or(MAX_COUNT)
                .clamp(1, MAX_COUNT),
        ),
        pending => {
            seq.note(pending, &held.name);
            return glib::Propagation::Proceed;
        }
    };
    match bindings.lookup(std::slice::from_ref(&held.name)) {
        Lookup::None => {
            seq.note(&status.command, &held.name);
            glib::Propagation::Proceed
        }
        Lookup::Exact {
            action,
            longer: false,
        } => {
            seq.typed.borrow_mut().clear();
            run_binding(state, editor, action, count);
            glib::Propagation::Stop
        }
        Lookup::Exact { longer: true, .. } | Lookup::Prefix => {
            seq.count.set(count);
            seq.held.borrow_mut().push(held);
            start_timer(state, keys, editor, seq);
            show_pending(state, editor, seq);
            glib::Propagation::Stop
        }
    }
}

/// (Re)starts the timeout of the held keys: then a complete binding runs, or Vim gets the keys.
fn start_timer(
    state: &Rc<WindowState>,
    keys: &Rc<WindowKeys>,
    editor: &Rc<Editor>,
    seq: &Rc<Sequence>,
) {
    seq.stop_timer();
    let (st, ks, ed, sq) = (
        Rc::downgrade(state),
        Rc::downgrade(keys),
        Rc::downgrade(editor),
        Rc::downgrade(seq),
    );
    let id = glib::timeout_add_local_once(SEQUENCE_TIMEOUT, move || {
        let (Some(state), Some(keys), Some(editor), Some(seq)) =
            (st.upgrade(), ks.upgrade(), ed.upgrade(), sq.upgrade())
        else {
            return;
        };
        // This source is done: forget its id so nobody removes it again.
        seq.timer.take();
        let bindings = keys.bindings();
        let names = seq.names();
        if let Lookup::Exact { action, .. } = bindings.lookup(&names) {
            let count = seq.count.get();
            seq.clear();
            seq.typed.borrow_mut().clear();
            run_binding(&state, &editor, action, count);
        } else {
            flush(&state, &editor, &seq);
        }
    });
    seq.timer.replace(Some(id));
}

/// Gives the held keys to Vim, in order, as if they had not been held.
fn flush(state: &Rc<WindowState>, editor: &Rc<Editor>, seq: &Sequence) {
    let held = seq.clear();
    for key in held {
        let Some(im) = editor.vim() else {
            break;
        };
        let pending = editor.vim_status().map(|s| s.command).unwrap_or_default();
        seq.note(&pending, &key.name);
        // The guard saw the key before it was held; ask again now that Vim gets it.
        if editor.vim_blocks_key(key.keyval, key.state) {
            editor.view().error_bell();
            continue;
        }
        let consumed = match &key.event {
            Some(event) => im.filter_keypress(event),
            None => vim::feed_key(&im, editor.view(), key.keyval, key.state),
        };
        // An insert-mode character that Vim lets through is typed by the text view.
        if !consumed
            && editor
                .vim_status()
                .is_some_and(|s| matches!(s.mode, VimMode::Insert | VimMode::Replace))
            && let Some(c) = key.keyval.to_unicode().filter(|c| !c.is_control())
            && !key.state.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            )
        {
            let view = editor.view();
            view.buffer()
                .insert_interactive_at_cursor(&c.to_string(), view.is_editable());
        }
    }
    show_pending(state, editor, seq);
}

/// Shows Vim's pending command plus the held keys in the status bar (the active editor only).
fn show_pending(state: &WindowState, editor: &Rc<Editor>, seq: &Sequence) {
    show_command(state, editor, &seq.names());
}

/// Shows Vim's pending command followed by `held` in the status bar, if `editor` is active.
fn show_command(state: &WindowState, editor: &Rc<Editor>, held: &[String]) {
    let active = state
        .active_editor()
        .is_some_and(|a| Rc::ptr_eq(&a, editor));
    if !active {
        return;
    }
    let mut text = editor.vim_status().map(|s| s.command).unwrap_or_default();
    text.extend(held.iter().map(String::as_str));
    state.status.vim_command.set_text(&text);
}

/// Runs the binding `action` with Vim's `count`: clears the count in Vim, then performs the
/// action from a high-priority idle callback (before the next key, outside this key's dispatch).
fn run_binding(state: &Rc<WindowState>, editor: &Rc<Editor>, action: &str, count: Option<u32>) {
    if count.is_some()
        && let Some(im) = editor.vim()
    {
        vim::cancel_pending(&im, editor.view());
    }
    // No longer the held keys.
    show_command(state, editor, &[]);
    let (state, editor, action) = (
        Rc::downgrade(state),
        Rc::downgrade(editor),
        action.to_owned(),
    );
    let count = count.unwrap_or(1);
    glib::idle_add_local_full(glib::Priority::HIGH, move || {
        if let (Some(state), Some(editor)) = (state.upgrade(), editor.upgrade()) {
            perform(&state, &editor, &action, count);
        }
        glib::ControlFlow::Break
    });
}

/// Performs a binding's action: a review action `count` times (see the module docs), any other
/// action once.
fn perform(state: &Rc<WindowState>, editor: &Rc<Editor>, action: &str, count: u32) {
    match action {
        "win.review-next" if count > 1 => step_hunks(state, editor, true, count),
        "win.review-prev" if count > 1 => step_hunks(state, editor, false, count),
        "win.review-accept" if count > 1 => decide_hunks(state, editor, Decision::Accept, count),
        "win.review-reject" if count > 1 => decide_hunks(state, editor, Decision::Reject, count),
        _ => {
            if let Err(e) = activate(editor.view(), action) {
                state.toast(&e);
            }
        }
    }
}

/// Activates the detailed action `detailed` (`"win.review-next"`, `"win.foo::bar"`) from
/// `widget`, so window, application and widget actions are all found.
pub fn activate(widget: &impl IsA<gtk::Widget>, detailed: &str) -> Result<(), String> {
    let (name, target) = gio::Action::parse_detailed_name(detailed)
        .map_err(|e| format!("“{detailed}” is not an action name: {}", e.message()))?;
    widget
        .activate_action(&name, target.as_ref())
        .map_err(|_| format!("There is no action “{detailed}”"))
}

// ----- counted review actions -----

/// The line of `editor`'s cursor, 0-based, in `LineIndex` numbering.
fn cursor_line(editor: &Editor) -> u32 {
    let buffer = editor.buffer();
    let iter = buffer.iter_at_mark(&buffer.get_insert());
    let doc = editor.document();
    let offset = doc.byte_offset(&iter);
    doc.line_index().line_of(offset)
}

/// The position `n` steps from `cursor` in `positions` (sorted ascending), forward or backward,
/// wrapping around: the first step goes to the first position after (before) `cursor`. `None`
/// when there are no positions.
pub fn nth_position<T: Ord>(positions: &[T], cursor: &T, forward: bool, n: usize) -> Option<usize> {
    let len = positions.len();
    if len == 0 || n == 0 {
        return None;
    }
    let steps = (n - 1) % len;
    Some(if forward {
        let first = positions.iter().position(|p| p > cursor).unwrap_or(0);
        (first + steps) % len
    } else {
        let first = positions
            .iter()
            .rposition(|p| p < cursor)
            .unwrap_or(len - 1);
        (first + len - steps) % len
    })
}

/// `]h`/`[h` with a count: moves `n` hunks forward or backward from `editor`'s cursor in review
/// order (files by path, hunks by line, wrapping around), as `n` presses of Alt+] would.
pub fn step_hunks(state: &Rc<WindowState>, editor: &Rc<Editor>, forward: bool, n: u32) {
    let mut hunks: Vec<(DocRef, u32)> = Vec::new();
    for doc in review_ui::reviewed_documents(&state.workspace) {
        let starts: Vec<u32> = doc
            .review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.cur_lines.start).collect())
            .unwrap_or_default();
        hunks.extend(starts.into_iter().map(|s| (doc.clone(), s)));
    }
    if hunks.is_empty() {
        state.toast("No changes to review");
        return;
    }
    let positions: Vec<(&std::path::Path, u32)> = hunks
        .iter()
        .map(|(doc, line)| (doc.path(), *line))
        .collect();
    let cursor = (editor.document().path(), cursor_line(editor));
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    if let Some(i) = nth_position(&positions, &cursor, forward, n) {
        let (doc, line) = &hunks[i];
        review_ui::show_line(state, doc, *line);
    }
}

/// The hunks `n` presses of Alt+Y (Alt+N) would decide, starting with `first` in `doc`, followed
/// by the hunk the cursor would end on: the rest of `doc` from `first` on, then the start of
/// `doc` (decisions wrap within a file), then the other files in path order after `doc`
/// (wrapping), each from its first hunk.
fn decision_order(state: &WindowState, doc: &DocRef, first: HunkId) -> Vec<(DocRef, HunkId)> {
    let ids = |d: &DocRef| -> Vec<HunkId> {
        d.review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.id).collect())
            .unwrap_or_default()
    };
    let own = ids(doc);
    let at = own.iter().position(|id| *id == first).unwrap_or(0);
    let mut order: Vec<(DocRef, HunkId)> = own[at..]
        .iter()
        .chain(&own[..at])
        .map(|id| (doc.clone(), *id))
        .collect();
    let docs: Vec<DocRef> = review_ui::reviewed_documents(&state.workspace)
        .into_iter()
        .filter(|d| !Rc::ptr_eq(d, doc))
        .collect();
    let split = docs.partition_point(|d| d.path() < doc.path());
    for other in docs[split..].iter().chain(&docs[..split]) {
        order.extend(ids(other).into_iter().map(|id| (other.clone(), id)));
    }
    order
}

/// `ga`/`gr` with a count, `:Accept N`, `:Reject N`: decides the hunk at `editor`'s cursor and
/// the next ones, `n` in all, in the order `n` presses of Alt+Y (Alt+N) would, each as its own
/// decision (Alt+U reverts the last one); then moves to the hunk after them.
pub fn decide_hunks(state: &Rc<WindowState>, editor: &Rc<Editor>, decision: Decision, n: u32) {
    let doc = editor.document();
    let Some(first) = review_ui::hunk_at_cursor(editor) else {
        state.toast("No change at the cursor");
        return;
    };
    let order = decision_order(state, doc, first);
    let n = usize::try_from(n).unwrap_or(usize::MAX).min(order.len());
    let next = order.get(n).cloned();
    for (doc, id) in &order[..n] {
        if let Err(e) = review_ui::decide(state, doc, *id, decision) {
            state.toast(&e);
            break;
        }
    }
    if let Some((doc, id)) = next {
        let line = doc
            .review
            .borrow()
            .as_ref()
            .and_then(|r| r.hunk(id))
            .map(|h| h.cur_lines.start);
        if let Some(line) = line {
            review_ui::show_line(state, &doc, line);
        }
    }
}

// ----- ex commands -----

/// An ex command of WP2.10 (see [`parse_ex_command`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExCommand {
    /// `:Agent [text]`: a prompt, a quick action's name, or (empty) focus the agent pane.
    Agent(String),
    /// `:AgentStop` cancels the running turn; `:AgentStop!` (`force`) stops the agent process.
    AgentStop { force: bool },
    /// `:Accept [N]` / `:Reject [N]` (also `:NAccept`).
    Decide { decision: Decision, count: u32 },
    /// `:Hnext [N]` / `:Hprev [N]` (also `:NHnext`).
    Hunk { forward: bool, count: u32 },
}

/// Parses an ex command line (with or without the leading `:`). `None` when it is not one of
/// WP2.10's commands; `Some(Err(message))` when it is, but malformed (`:Accept x`, `:Hnext 0`).
///
/// | Command | Meaning |
/// |---|---|
/// | `:Agent <prompt>` | send `<prompt>` to the agent (shows the agent pane) |
/// | `:Agent <quick action>` | run that prompt template (by name or slug, e.g. `proofread`) |
/// | `:Agent` | focus the agent pane's prompt box |
/// | `:AgentStop` / `:AgentStop!` | cancel the running turn / stop the agent process |
/// | `:Accept N`, `:Reject N`, `:NAccept`, `:NReject` | decide `N` hunks from the cursor on |
/// | `:Hnext N`, `:Hprev N`, `:NHnext`, `:NHprev` | move `N` hunks |
pub fn parse_ex_command(command: &str) -> Option<Result<ExCommand, String>> {
    let line = command.trim().trim_start_matches(':').trim_start();
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let (prefix, line) = line.split_at(digits);
    let name_len = line.bytes().take_while(u8::is_ascii_alphabetic).count();
    let (name, rest) = line.split_at(name_len);
    let (force, rest) = match rest.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    // A command name ends at a blank (`:Agent!x` and `:Hnext2` are other commands).
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let args = rest.trim();
    let count = |what: &str| -> Result<u32, String> {
        let given = match (prefix, args) {
            ("", "") => return Ok(1),
            (p, "") => p,
            ("", a) => a,
            _ => return Err(format!(":{what}: give the count once")),
        };
        match given.parse::<u64>() {
            Ok(0) => Err(format!(":{what}: the count must be positive")),
            Ok(n) => Ok(u32::try_from(n).unwrap_or(MAX_COUNT).min(MAX_COUNT)),
            Err(_) => Err(format!(":{what}: {given:?} is not a count")),
        }
    };
    let counted = |what: &str| -> Result<(), String> {
        if force {
            Err(format!(":{what} does not take a !"))
        } else {
            Ok(())
        }
    };
    let parsed = match name {
        "Agent" if prefix.is_empty() && !force => Ok(ExCommand::Agent(args.to_owned())),
        "AgentStop" if prefix.is_empty() => {
            if args.is_empty() {
                Ok(ExCommand::AgentStop { force })
            } else {
                Err(":AgentStop takes no arguments".to_owned())
            }
        }
        "Accept" | "Reject" => counted(name).and_then(|()| {
            Ok(ExCommand::Decide {
                decision: if name == "Accept" {
                    Decision::Accept
                } else {
                    Decision::Reject
                },
                count: count(name)?,
            })
        }),
        "Hnext" | "Hprev" => counted(name).and_then(|()| {
            Ok(ExCommand::Hunk {
                forward: name == "Hnext",
                count: count(name)?,
            })
        }),
        _ => return None,
    };
    Some(parsed)
}

/// Window-level set-up of WP2.10, once per window: the per-window binding table (re-read on
/// config reloads), the binding handlers following `win.toggle-vim`, and the default Ctrl+K of
/// `win.agent-inline-edit` when nothing set one (WP2.5 owns that action).
pub fn install_window(state: &Rc<WindowState>) {
    let keys = Rc::new(WindowKeys {
        window: Rc::downgrade(state),
        bindings: RefCell::new(Rc::new(Bindings::default())),
        errors: RefCell::new(Vec::new()),
        editors: RefCell::new(Vec::new()),
    });
    keys.load_bindings(state);
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.window.strong_count() > 0);
        windows.push(keys);
    });
    install_vim_accelerator_keys(state);
    state.connect_config_changed(|state, file| {
        if file != ConfigFile::Config {
            return;
        }
        let Some(keys) = window_keys(state) else {
            return;
        };
        let editors: Vec<(Rc<Editor>, Rc<Sequence>)> = keys
            .editors
            .borrow()
            .iter()
            .filter_map(|a| Some((a.editor.upgrade()?, a.seq.clone())))
            .collect();
        // Keys held under the old bindings go to Vim as typed.
        for (editor, seq) in editors {
            flush(state, &editor, &seq);
        }
        keys.load_bindings(state);
        set_inline_edit_accel(state);
    });
    if let Some(action) = state.window.lookup_action("toggle-vim") {
        let weak = Rc::downgrade(state);
        action.connect_notify_local(Some("state"), move |_, _| {
            if let Some(state) = weak.upgrade()
                && let Some(keys) = window_keys(&state)
            {
                keys.sync(&state);
            }
        });
    }
    set_inline_edit_accel(state);
}

/// Keys that are application accelerators and Vim keys alike: with Vim on, in an editor that has
/// the focus, they are Vim's (see the module docs).
fn is_vim_accelerator_key(keyval: gdk::Key, state: gdk::ModifierType) -> bool {
    let modifiers = state
        & (gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK);
    modifiers == gdk::ModifierType::CONTROL_MASK
        && matches!(
            keyval,
            gdk::Key::w | gdk::Key::W | gdk::Key::o | gdk::Key::O
        )
}

/// The capture-phase controller on the window that gives Ctrl+W and Ctrl+O to the focused
/// editor's layer and Vim while Vim is on. Added after the window was given to the application,
/// so it runs before the accelerators' controller (GTK runs a widget's controllers newest
/// first).
fn install_vim_accelerator_keys(state: &Rc<WindowState>) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_name(Some("lemma-vim-accelerator-keys"));
    let weak = Rc::downgrade(state);
    controller.connect_key_pressed(move |controller, keyval, keycode, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if !is_vim_accelerator_key(keyval, modifiers) {
            return glib::Propagation::Proceed;
        }
        let Some(editor) = state
            .editors()
            .into_iter()
            .find(|e| e.vim().is_some() && e.view().has_focus())
        else {
            return glib::Propagation::Proceed;
        };
        deliver_to_editor(
            &editor,
            controller.current_event(),
            keyval,
            keycode,
            modifiers,
        );
        glib::Propagation::Stop
    });
    state.window.add_controller(controller);
}

/// Delivers a key press to `editor` as GTK would below the window: its key layer, then (unless
/// a handler stopped it or Vim's guard refuses it) Vim.
fn deliver_to_editor(
    editor: &Editor,
    real: Option<gdk::Event>,
    keyval: gdk::Key,
    keycode: u32,
    state: gdk::ModifierType,
) {
    let event = KeyEvent {
        keyval,
        keycode,
        state,
        vim: editor.vim().is_some(),
    };
    let layer = editor.view().key_layer();
    let result = layer.map_or(glib::Propagation::Proceed, |layer| {
        layer.run_with(&event, real.clone())
    });
    if result == glib::Propagation::Stop || editor.vim_blocks_key(keyval, state) {
        return;
    }
    let Some(im) = editor.vim() else {
        return;
    };
    let consumed = match &real {
        Some(event) => im.filter_keypress(event),
        None => vim::feed_key(&im, editor.view(), keyval, state),
    };
    // GtkSourceView 5.20's Vim has no insert-mode Ctrl-W: Vim's "delete the word before the
    // cursor" is done here. (Its insert-mode Ctrl-O does nothing either.)
    let insert = editor
        .vim_status()
        .is_some_and(|s| matches!(s.mode, VimMode::Insert | VimMode::Replace));
    if !consumed && insert && matches!(keyval, gdk::Key::w | gdk::Key::W) {
        delete_word_before_cursor(editor.view());
    }
}

/// Vim's insert-mode Ctrl-W: deletes the blanks before the cursor and the word before them (a
/// run of letters, digits and `_`, or a run of other characters: `\frac` takes two), or the
/// line break at the start of a line.
pub fn delete_word_before_cursor(view: &TexView) {
    let buffer = view.buffer();
    let mut end = buffer.iter_at_mark(&buffer.get_insert());
    let mut start = end;
    let before = |at: &gtk::TextIter| {
        let mut p = *at;
        (!at.starts_line() && p.backward_char()).then(|| p.char())
    };
    if start.starts_line() {
        start.backward_char();
    } else {
        while before(&start).is_some_and(|c| c == ' ' || c == '\t') {
            start.backward_char();
        }
        let word = |c: char| c.is_alphanumeric() || c == '_';
        if let Some(first) = before(&start) {
            let kind = word(first);
            while before(&start).is_some_and(|c| !c.is_whitespace() && word(c) == kind) {
                start.backward_char();
            }
        }
    }
    if start != end {
        buffer.begin_user_action();
        buffer.delete_interactive(&mut start, &mut end, view.is_editable());
        buffer.end_user_action();
    }
}

/// Ctrl+K for the inline edit (a binding of every mode), unless the action already has an
/// accelerator or `[keys]` rebinds it.
fn set_inline_edit_accel(state: &WindowState) {
    let (action, accels) = INLINE_EDIT_ACCEL;
    if state.app.accels_for_action(action).is_empty() {
        crate::app::set_default_accels(state, &[(action, accels)]);
    }
}

/// Called after an editor tab is created: gives it the Vim normal-mode bindings (registered in
/// its key layer while Vim is on).
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(keys) = window_keys(state) else {
        return;
    };
    if keys.attached(editor).is_some() {
        return;
    }
    keys.editors.borrow_mut().push(Attached {
        editor: Rc::downgrade(editor),
        seq: Rc::default(),
        handler: Cell::new(None),
    });
    keys.sync(state);
}

/// Called after an editor tab is closed: removes its binding handler and drops held keys.
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(keys) = window_keys(state) else {
        return;
    };
    let removed = {
        let mut editors = keys.editors.borrow_mut();
        let at = editors
            .iter()
            .position(|a| std::ptr::eq(a.editor.as_ptr(), Rc::as_ptr(editor)));
        at.map(|i| editors.remove(i))
    };
    if let Some(attached) = removed {
        attached.seq.clear();
        if let Some(id) = attached.handler.get() {
            unregister(editor.view(), id);
        }
    }
}

/// Ex commands of WP2.10 ([`parse_ex_command`]). Runs after `review_ui::ex_command` and before
/// the shell's `:w`/`:q` family; returns `true` when it handled `command`. Like every ex command
/// handler it runs inside Vim's key handling, so the command itself runs from a high-priority
/// idle callback (before the next key).
pub fn ex_command(state: &Rc<WindowState>, editor: &Rc<Editor>, command: &str) -> bool {
    // A `:join` GtkSourceView would never return from (see `vim::join_range_would_hang`).
    if join_command_hangs(editor, command) {
        editor.view().error_bell();
        return true;
    }
    let command = match parse_ex_command(command) {
        None => return false,
        Some(Err(message)) => {
            state.toast(&message);
            return true;
        }
        Some(Ok(command)) => command,
    };
    let (state, editor) = (Rc::downgrade(state), Rc::downgrade(editor));
    let mut command = Some(command);
    glib::idle_add_local_full(glib::Priority::HIGH, move || {
        if let (Some(state), Some(editor), Some(command)) =
            (state.upgrade(), editor.upgrade(), command.take())
        {
            run_ex(&state, &editor, command);
        }
        glib::ControlFlow::Break
    });
    true
}

/// Whether `command` is a `:[range]j[oin] [count]` whose lines reach the whitespace at the end
/// of the buffer, where GtkSourceView's join never returns.
fn join_command_hangs(editor: &Editor, command: &str) -> bool {
    let buffer = editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert()).line();
    let last_line = buffer.end_iter().line();
    let selection = buffer.selection_bounds().map(|(start, end)| {
        let last = if end.starts_line() && end.line() > start.line() {
            end.line() - 1
        } else {
            end.line()
        };
        (start.line(), last)
    });
    vim::join_command_lines(command, cursor, last_line, selection)
        .is_some_and(|(first, last)| vim::join_range_would_hang(buffer, first, last))
}

fn run_ex(state: &Rc<WindowState>, editor: &Rc<Editor>, command: ExCommand) {
    match command {
        ExCommand::Agent(text) if text.is_empty() => agent::panel::focus_prompt(state),
        ExCommand::Agent(text) => {
            if lemma_config::find_prompt(&state.prompts(), &text).is_some() {
                if let Err(e) = agent::quick::run(state, &text) {
                    state.toast(&e);
                }
                return;
            }
            if state.agent_controller().is_none() {
                state.toast("This window has no agent");
                return;
            }
            // As the panel's own prompt box sends it: refused with a toast while a turn runs.
            if agent::panel::send_prompt(state, vec![PromptBlock::Text(text)], PromptOrigin::Panel)
            {
                state.agent.split.set_show_sidebar(true);
            }
        }
        ExCommand::AgentStop { force } => {
            let Some(controller) = state.agent_controller() else {
                return;
            };
            if force {
                // As the pane's "Stop agent (free memory)": the pane shows the stopped card and
                // does not start the agent again when it is shown.
                if let Some(panel) = agent::panel::get(state) {
                    panel.actions().activate_action("shutdown", None);
                }
                if controller.state() != agent::AgentState::Stopped {
                    controller.stop();
                }
            } else if controller.is_running() {
                controller.cancel();
            } else {
                state.toast("No agent turn is running");
            }
        }
        ExCommand::Decide { decision, count } => decide_hunks(state, editor, decision, count),
        ExCommand::Hunk { forward, count } => step_hunks(state, editor, forward, count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(notation: &str) -> Vec<String> {
        parse_keys(notation).unwrap_or_else(|e| panic!("{notation:?}: {e}"))
    }

    #[test]
    fn key_notation_is_parsed_and_normalized() {
        assert_eq!(keys("]h"), ["]", "h"]);
        assert_eq!(keys("<Space>a"), ["<Space>", "a"]);
        assert_eq!(keys("<space>A"), ["<Space>", "A"]);
        assert_eq!(keys("<C-k>"), ["<C-k>"]);
        assert_eq!(keys("<c-K>"), ["<C-k>"], "<C-K> is <C-k>");
        assert_eq!(keys("<A-x><M-X>"), ["<M-x>", "<M-X>"]);
        assert_eq!(keys("<C-M-a>"), ["<C-M-a>"]);
        assert_eq!(keys("<S-Tab>"), ["<S-Tab>"]);
        assert_eq!(keys("<Enter><return><CR>"), ["<CR>", "<CR>", "<CR>"]);
        assert_eq!(keys("<lt>"), ["<lt>"]);
        assert_eq!(keys("<"), ["<lt>"], "a lone < is the character");
        assert_eq!(keys("a<b"), ["a", "<lt>", "b"]);
        assert_eq!(keys("<Bslash><Bar>"), ["<Bslash>", "<Bar>"]);
        assert_eq!(keys("\\|"), ["<Bslash>", "<Bar>"]);
        assert_eq!(keys("<F5>"), ["<F5>"]);
        assert_eq!(keys("<C-->"), ["<C-->"]);
        assert_eq!(keys("é"), ["é"]);
        assert_eq!(keys(" a"), ["<Space>", "a"], "a literal space is <Space>");
        assert!(parse_keys("").is_err());
        assert!(
            parse_keys("<Foo>")
                .unwrap_err()
                .contains("unknown key <Foo>")
        );
        assert!(parse_keys("<Leader>a").unwrap_err().contains("no leader"));
        assert!(parse_keys("a\tb").is_err());
    }

    #[test]
    fn key_presses_are_named_like_the_notation() {
        use gdk::ModifierType as M;
        let none = M::empty();
        assert_eq!(key_name(gdk::Key::bracketright, none).as_deref(), Some("]"));
        assert_eq!(key_name(gdk::Key::h, none).as_deref(), Some("h"));
        assert_eq!(key_name(gdk::Key::H, M::SHIFT_MASK).as_deref(), Some("H"));
        assert_eq!(
            key_name(gdk::Key::braceright, M::SHIFT_MASK).as_deref(),
            Some("}")
        );
        assert_eq!(key_name(gdk::Key::space, none).as_deref(), Some("<Space>"));
        assert_eq!(
            key_name(gdk::Key::k, M::CONTROL_MASK).as_deref(),
            Some("<C-k>")
        );
        assert_eq!(
            key_name(gdk::Key::K, M::CONTROL_MASK | M::SHIFT_MASK).as_deref(),
            Some("<C-k>")
        );
        assert_eq!(key_name(gdk::Key::x, M::ALT_MASK).as_deref(), Some("<M-x>"));
        assert_eq!(
            key_name(gdk::Key::ISO_Left_Tab, M::SHIFT_MASK).as_deref(),
            Some("<S-Tab>")
        );
        assert_eq!(key_name(gdk::Key::Return, none).as_deref(), Some("<CR>"));
        assert_eq!(
            key_name(gdk::Key::less, M::SHIFT_MASK).as_deref(),
            Some("<lt>")
        );
        // Caps Lock, Num Lock (Mod2) and AltGr (Mod5) are not modifiers of the key.
        let locks = M::LOCK_MASK | M::from_bits_retain(1 << 4) | M::from_bits_retain(1 << 7);
        assert_eq!(
            key_name(gdk::Key::bracketright, locks).as_deref(),
            Some("]")
        );
        assert_eq!(key_name(gdk::Key::Shift_L, none), None);
        assert_eq!(key_name(gdk::Key::Control_R, M::CONTROL_MASK), None);
        // Every name a key press gets parses back to itself.
        for (key, state) in [
            (gdk::Key::space, none),
            (gdk::Key::k, M::CONTROL_MASK),
            (gdk::Key::ISO_Left_Tab, M::SHIFT_MASK),
            (gdk::Key::backslash, M::ALT_MASK),
            (gdk::Key::F10, M::CONTROL_MASK),
            (gdk::Key::less, M::CONTROL_MASK),
        ] {
            let name = key_name(key, state).unwrap();
            assert_eq!(keys(&name), std::slice::from_ref(&name), "{name}");
        }
    }

    #[test]
    fn bindings_classify_sequences() {
        let config: BTreeMap<String, String> = [
            ("]h", "win.review-next"),
            ("[h", "win.review-prev"),
            ("ga", "win.review-accept"),
            ("g", "win.only-g"),
            ("gab", "win.longer"),
            ("<Space>a", "win.agent-focus"),
            ("x y", "not an action"),
            ("<Bogus>", "win.x"),
        ]
        .into_iter()
        .map(|(k, a)| (k.to_owned(), a.to_owned()))
        .collect();
        let (bindings, errors) = Bindings::new(&config);
        assert_eq!(errors.len(), 2, "{errors:?}");
        let s = |v: &[&str]| v.iter().map(|k| (*k).to_owned()).collect::<Vec<_>>();
        assert_eq!(bindings.lookup(&s(&["]"])), Lookup::Prefix);
        assert_eq!(
            bindings.lookup(&s(&["]", "h"])),
            Lookup::Exact {
                action: "win.review-next",
                longer: false
            }
        );
        assert_eq!(bindings.lookup(&s(&["]", "x"])), Lookup::None);
        assert_eq!(bindings.lookup(&s(&["h"])), Lookup::None);
        assert_eq!(
            bindings.lookup(&s(&["g"])),
            Lookup::Exact {
                action: "win.only-g",
                longer: true
            }
        );
        assert_eq!(
            bindings.lookup(&s(&["g", "a"])),
            Lookup::Exact {
                action: "win.review-accept",
                longer: true
            }
        );
        assert_eq!(bindings.lookup(&s(&["<Space>"])), Lookup::Prefix);
        let defaults = lemma_config::VimCfg::default().normal_bindings();
        let (defaults, errors) = Bindings::new(&defaults);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            defaults.lookup(&s(&["<Space>", "a"])),
            Lookup::Exact {
                action: "win.agent-focus",
                longer: false
            }
        );
    }

    #[test]
    fn invisible_pending_commands() {
        for typed in ["r", "2r", "\"ar", "2\"a3r", "y", "\"+y", "12y", "gq", "g?"] {
            assert!(waits_invisibly(typed), "{typed:?}");
        }
        for typed in [
            "", "x", "fr", "dr", "yy", "]", "g", "gg", "rr", "<Space>", "\"r",
        ] {
            assert!(!waits_invisibly(typed), "{typed:?}");
        }
    }

    #[test]
    fn nth_position_wraps_like_repeated_steps() {
        let p = [10, 20, 30];
        assert_eq!(nth_position(&p, &15, true, 1), Some(1));
        assert_eq!(nth_position(&p, &20, true, 1), Some(2), "strictly after");
        assert_eq!(nth_position(&p, &15, true, 2), Some(2));
        assert_eq!(nth_position(&p, &15, true, 3), Some(0), "wraps");
        assert_eq!(nth_position(&p, &35, true, 1), Some(0));
        assert_eq!(nth_position(&p, &15, false, 1), Some(0));
        assert_eq!(
            nth_position(&p, &10, false, 1),
            Some(2),
            "strictly before, wraps"
        );
        assert_eq!(nth_position(&p, &25, false, 2), Some(0));
        assert_eq!(nth_position(&p, &25, false, 4), Some(1));
        assert_eq!(nth_position(&p, &25, true, 7), Some(2), "7 steps = 1 step");
        assert_eq!(nth_position::<i32>(&[], &0, true, 1), None);
        // n steps equal n single steps.
        for cursor in 0..40 {
            for forward in [true, false] {
                let mut at = cursor;
                for n in 1..8 {
                    let i = nth_position(&p, &at, forward, 1).unwrap();
                    at = p[i];
                    assert_eq!(nth_position(&p, &cursor, forward, n), Some(i));
                }
            }
        }
    }

    #[test]
    fn ex_commands_parse() {
        let ok = |c: &str| parse_ex_command(c).unwrap().unwrap();
        let err = |c: &str| parse_ex_command(c).unwrap().unwrap_err();
        assert_eq!(
            ok(":Agent fix the proof"),
            ExCommand::Agent("fix the proof".into())
        );
        assert_eq!(ok("Agent   x  "), ExCommand::Agent("x".into()));
        assert_eq!(ok(":Agent"), ExCommand::Agent(String::new()));
        assert_eq!(ok(":AgentStop"), ExCommand::AgentStop { force: false });
        assert_eq!(ok(":AgentStop!"), ExCommand::AgentStop { force: true });
        assert!(err(":AgentStop now").contains("no arguments"));
        assert_eq!(
            ok(":Accept 3"),
            ExCommand::Decide {
                decision: Decision::Accept,
                count: 3
            }
        );
        assert_eq!(
            ok(":2Reject"),
            ExCommand::Decide {
                decision: Decision::Reject,
                count: 2
            }
        );
        assert_eq!(
            ok(":Accept"),
            ExCommand::Decide {
                decision: Decision::Accept,
                count: 1
            }
        );
        assert_eq!(
            ok(":Hnext 2"),
            ExCommand::Hunk {
                forward: true,
                count: 2
            }
        );
        assert_eq!(
            ok(":4Hprev"),
            ExCommand::Hunk {
                forward: false,
                count: 4
            }
        );
        assert_eq!(
            ok(":Hnext 123456789"),
            ExCommand::Hunk {
                forward: true,
                count: MAX_COUNT
            }
        );
        assert!(err(":Hnext 0").contains("positive"));
        assert!(err(":Accept x").contains("not a count"));
        assert!(err(":2Accept 3").contains("once"));
        assert!(err(":Accept! 2").contains("does not take a !"));
        for other in [
            ":w",
            ":Agentx",
            ":Hnext2",
            ":AcceptAll",
            ":2Agent hi",
            ":Agent!",
            ":hnext",
            ":s/a/b/",
            ":",
            "",
        ] {
            assert_eq!(parse_ex_command(other), None, "{other:?}");
        }
    }
}
