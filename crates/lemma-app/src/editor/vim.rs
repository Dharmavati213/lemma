//! Vim emulation (ARCHITECTURE §5): GtkSourceView's `VimIMContext`, fed by a capture-phase key
//! controller on the view so it sees every key before the view's own bindings.
//!
//! Application accelerators (Ctrl+S, F5, …) still win: GTK handles them in the window's capture
//! phase, before the view's controllers run. Between the two, a guard on the view's parent stops
//! the few keys Vim must not see (see [`Vim::blocks_key`]).

use super::TexView;
use gtk::gdk;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Vim emulation attached to one view.
pub struct Vim {
    im: sourceview5::VimIMContext,
    keys: gtk::EventControllerKey,
    /// Stops the keys of [`Vim::blocks_key`]. It sits on the view's parent, whose capture phase
    /// runs before the view's.
    guard: gtk::EventControllerKey,
    guard_widget: glib::WeakRef<gtk::Widget>,
    view: glib::WeakRef<TexView>,
    /// Whether Vim's last change was a join (for `.`).
    joins: Rc<JoinGuard>,
    buffer: glib::WeakRef<gtk::TextBuffer>,
    changed: Option<glib::SignalHandlerId>,
}

impl Vim {
    /// Starts Vim emulation on `view` (normal mode).
    pub fn attach(view: &TexView) -> Self {
        let im = sourceview5::VimIMContext::new();
        let keys = gtk::EventControllerKey::new();
        keys.set_im_context(Some(&im));
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        view.add_controller(keys.clone());
        im.set_client_widget(Some(view));
        let joins = Rc::new(JoinGuard::default());
        let guard = gtk::EventControllerKey::new();
        guard.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak_im, weak_view, guard_joins) = (im.downgrade(), view.downgrade(), joins.clone());
        guard.connect_key_pressed(move |_, keyval, _, state| {
            match (weak_im.upgrade(), weak_view.upgrade()) {
                (Some(im), Some(view)) if key_blocked(&guard_joins, &im, &view, keyval, state) => {
                    view.error_bell();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        // Without a parent, a controller added later still runs first (GTK prepends them).
        let guard_widget = view
            .parent()
            .unwrap_or_else(|| view.clone().upcast::<gtk::Widget>());
        guard_widget.add_controller(guard.clone());
        // A change made while a key is dispatched is that key's (see `JoinGuard`).
        let buffer = view.buffer();
        let recorder = joins.clone();
        let changed = buffer.connect_changed(move |_| recorder.buffer_changed());
        Self {
            im,
            keys,
            guard,
            guard_widget: guard_widget.downgrade(),
            view: view.downgrade(),
            joins,
            buffer: buffer.downgrade(),
            changed: Some(changed),
        }
    }

    /// Stops Vim emulation; the view goes back to plain editing.
    pub fn detach(mut self) {
        self.im.reset();
        self.keys.set_im_context(None::<&gtk::IMContext>);
        self.im.set_client_widget(None::<&gtk::Widget>);
        if let Some(widget) = self.guard_widget.upgrade() {
            widget.remove_controller(&self.guard);
        }
        if let Some(view) = self.view.upgrade() {
            view.remove_controller(&self.keys);
            // Normal mode turns on overwrite (block cursor); plain editing must not keep it.
            view.set_overwrite(false);
        }
        if let (Some(buffer), Some(id)) = (self.buffer.upgrade(), self.changed.take()) {
            buffer.disconnect(id);
        }
        self.joins.last_join.set(None);
    }

    /// True when Lemma's key layer stops the key press `keyval` (with modifiers `state`) before
    /// Vim sees it, as the guard controller does for real key presses. Code that gives a key
    /// press to Vim itself must ask exactly so, once per key press, right before: the answer
    /// also records what the key is (a join, `.`, an undo, …) for the next ones.
    ///
    /// GtkSourceView 5.20's `gtk_source_buffer_join_lines` (Vim's `J`, `gJ` and `:join`) never
    /// returns when it has to join across the whitespace at the end of the buffer (everything
    /// after the buffer's last non-blank character): `J` on a last line of blanks, or on a last
    /// line with trailing blanks, or a counted join (`2J`, `3J`) or a repeated one (`.` after
    /// `J`) whose lines reach that whitespace would freeze the app. GtkSourceView's count is one
    /// more than Vim's (`2J` joins three lines). Such a key is not given to Vim (the bell rings,
    /// and a pending count is cancelled); see [`join_range_would_hang`].
    pub fn blocks_key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
        self.view
            .upgrade()
            .is_some_and(|view| key_blocked(&self.joins, &self.im, &view, keyval, state))
    }

    /// The input-method context, for connecting `write`, `execute-command` and the
    /// `command-bar-text`/`command-text` notifications.
    pub fn im(&self) -> &sourceview5::VimIMContext {
        &self.im
    }

    /// The current mode and pending command.
    pub fn status(&self) -> VimStatus {
        VimStatus::from_texts(&self.im.command_bar_text(), &self.im.command_text())
    }
}

/// Gives Vim a press and release of `keyval` with modifiers `state`, built from the display's
/// keymap as a keyboard would produce them (`gtk_im_context_filter_key`). Returns whether Vim
/// consumed the press; in insert mode it lets plain characters through for the text view to
/// type. Used to replay keys the key layer held back, and in tests.
pub fn feed_key(
    im: &sourceview5::VimIMContext,
    view: &TexView,
    keyval: gdk::Key,
    state: gdk::ModifierType,
) -> bool {
    let Some(surface) = view.native().and_then(|n| n.surface()) else {
        return false;
    };
    let display = surface.display();
    let Some(keyboard) = display.default_seat().and_then(|s| s.keyboard()) else {
        return false;
    };
    let Some(key) = display
        .map_keyval(keyval)
        .and_then(|keys| keys.into_iter().min_by_key(gdk::KeymapKey::level))
    else {
        return false;
    };
    let state = if key.level() > 0 {
        state | gdk::ModifierType::SHIFT_MASK
    } else {
        state
    };
    // GDK_CURRENT_TIME: Vim does not look at event times.
    let time = 0;
    let consumed = im.filter_key(
        true,
        &surface,
        &keyboard,
        time,
        key.keycode(),
        state,
        key.group(),
    );
    im.filter_key(
        false,
        &surface,
        &keyboard,
        time,
        key.keycode(),
        state,
        key.group(),
    );
    consumed
}

/// Cancels what Vim has pending in normal mode (a count such as `3`, or `d`), as Escape does,
/// without leaving the mode. Nothing happens when nothing is pending.
///
/// (`VimIMContext::reset` would clear a count too, but leaves `command-text` showing it until
/// the next key; verified on real input.)
pub fn cancel_pending(im: &sourceview5::VimIMContext, view: &TexView) {
    let status = VimStatus::from_texts(&im.command_bar_text(), &im.command_text());
    if status.mode == VimMode::Normal && !status.command.is_empty() {
        feed_key(im, view, gdk::Key::Escape, gdk::ModifierType::empty());
    }
}

/// What the key press being dispatched is, for [`JoinGuard`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum KeyRole {
    /// No key press is being dispatched: changes come from elsewhere (agent, review, reload,
    /// accelerators such as Ctrl+Z) and do not change Vim's last change.
    #[default]
    Idle,
    /// A join of GtkSourceView's count `n`: the cursor's line and the `n` lines after it.
    Join(u32),
    /// `.` with its count, if any: it repeats the last change.
    Repeat(Option<u32>),
    /// `u` or Ctrl-R: Vim's last change stays what it was.
    Undo,
    /// Any other key: a change it makes is Vim's new last change.
    Other,
}

/// Remembers whether Vim's last change was a join, so that `.` is checked like the join it
/// repeats. The guard records each key press's [`KeyRole`] (until the end of its dispatch); a
/// buffer change during the dispatch is that key's change.
#[derive(Default)]
struct JoinGuard {
    role: Cell<KeyRole>,
    /// GtkSourceView's count of Vim's last change when that was a join.
    last_join: Cell<Option<u32>>,
    /// Vim's pending command plus the last key it was given in normal mode (`2r`): GtkSourceView
    /// shows nothing pending after `r`, `y`, `gq`, `g?` (see `keys::waits_invisibly`).
    typed: RefCell<String>,
    /// An idle callback that ends the key's dispatch is queued.
    reset_queued: Rc<Cell<bool>>,
}

impl JoinGuard {
    /// Records `role` for the key press being dispatched; the role ends with the dispatch (a
    /// high-priority idle callback runs before the next event).
    fn set_role(self: &Rc<Self>, role: KeyRole) {
        self.role.set(role);
        if self.reset_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(guard) = weak.upgrade() {
                guard.reset_queued.set(false);
                guard.role.set(KeyRole::Idle);
            }
            glib::ControlFlow::Break
        });
    }

    /// The buffer changed: if a key made the change, it may be Vim's new last change.
    fn buffer_changed(&self) {
        match self.role.get() {
            KeyRole::Join(n) => self.last_join.set(Some(n)),
            KeyRole::Repeat(count) => {
                if let Some(n) = self.last_join.get() {
                    self.last_join.set(Some(count.unwrap_or(n)));
                }
            }
            KeyRole::Undo | KeyRole::Idle => {}
            KeyRole::Other => self.last_join.set(None),
        }
    }
}

/// See [`Vim::blocks_key`]: classifies the key press (and records it in `joins`), and answers
/// whether it is a join, or a `.` repeating one, that would hang.
fn key_blocked(
    joins: &Rc<JoinGuard>,
    im: &sourceview5::VimIMContext,
    view: &TexView,
    keyval: gdk::Key,
    state: gdk::ModifierType,
) -> bool {
    let (role, blocked) = classify_key(joins, im, view, keyval, state);
    joins.set_role(if blocked { KeyRole::Other } else { role });
    if blocked {
        // Vim never sees the key: a count typed before it must not stay pending.
        cancel_pending(im, view);
    }
    blocked
}

/// The [`KeyRole`] of a key press, and whether it must be blocked.
fn classify_key(
    joins: &JoinGuard,
    im: &sourceview5::VimIMContext,
    view: &TexView,
    keyval: gdk::Key,
    state: gdk::ModifierType,
) -> (KeyRole, bool) {
    let modifiers = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::META_MASK;
    let status = VimStatus::from_texts(&im.command_bar_text(), &im.command_text());
    let normal = status.mode == VimMode::Normal;
    let visual = matches!(
        status.mode,
        VimMode::Visual | VimMode::VisualLine | VimMode::VisualBlock
    );
    if state.intersects(modifiers) {
        let redo = normal
            && state.contains(gdk::ModifierType::CONTROL_MASK)
            && matches!(keyval, gdk::Key::r | gdk::Key::R);
        return (if redo { KeyRole::Undo } else { KeyRole::Other }, false);
    }
    if !(normal || visual) {
        return (KeyRole::Other, false);
    }
    let digits = status
        .command
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let count = status.command[..digits]
        .parse::<u32>()
        .ok()
        .map(|n| n.clamp(1, 99_999));
    let pending = &status.command[digits..];
    // After `f`, `t`, `r`, `m`, `"`, … the key is that command's argument, not a command.
    let argument = pending == "\""
        || pending.ends_with(['f', 'F', 't', 'T', 'r', 'm', '\'', '`', '@', 'q'])
        || (pending.is_empty() && super::keys::waits_invisibly(&joins.typed.borrow()));
    if normal {
        let mut typed = joins.typed.borrow_mut();
        typed.clear();
        typed.push_str(&status.command);
        if let Some(c) = keyval.to_unicode().filter(|c| !c.is_control()) {
            typed.push(c);
        }
    }
    if argument {
        return (KeyRole::Other, false);
    }
    let buffer = view.buffer();
    let cursor_line = || buffer.iter_at_mark(&buffer.get_insert()).line();
    match keyval {
        gdk::Key::J if visual => {
            let Some((start, end)) = buffer.selection_bounds() else {
                let line = cursor_line();
                return (
                    KeyRole::Join(1),
                    join_range_would_hang(&buffer, line, line + 1),
                );
            };
            let first = start.line();
            let mut last = end.line();
            if end.starts_line() && last > first {
                last -= 1;
            }
            // As many lines as selected (one: the next too), plus GtkSourceView's one more.
            let n = u32::try_from(last - first).unwrap_or(0).max(1) + 1;
            (
                KeyRole::Join(n),
                join_range_would_hang(&buffer, first, first.saturating_add_unsigned(n)),
            )
        }
        gdk::Key::J => {
            let n = count.unwrap_or(1);
            let line = cursor_line();
            (
                KeyRole::Join(n),
                join_range_would_hang(&buffer, line, line.saturating_add_unsigned(n)),
            )
        }
        gdk::Key::period if normal && pending.is_empty() => match joins.last_join.get() {
            Some(last) => {
                let n = count.map_or(last, |c| c.max(last));
                let line = cursor_line();
                (
                    KeyRole::Repeat(count),
                    join_range_would_hang(&buffer, line, line.saturating_add_unsigned(n)),
                )
            }
            None => (KeyRole::Repeat(count), false),
        },
        gdk::Key::u if normal && pending.is_empty() => (KeyRole::Undo, false),
        _ => (KeyRole::Other, false),
    }
}

/// True when GtkSourceView's `gtk_source_buffer_join_lines` would never return for a join of the
/// lines `first..=last` (0-based; `last` may lie past the end of the buffer): the join starts on
/// a line after whose last non-blank character the rest of the buffer is whitespace
/// ([`join_would_hang`]), or it reaches past the line of the buffer's last non-blank character
/// while whitespace follows that character. Joins that end before never hang.
pub fn join_range_would_hang(buffer: &impl IsA<gtk::TextBuffer>, first: i32, last: i32) -> bool {
    if join_would_hang(buffer, first) {
        return true;
    }
    let buffer = buffer.as_ref();
    let end = buffer.end_iter();
    let mut tail = end;
    while tail.backward_char() {
        if !tail.char().is_whitespace() {
            // `tail` is on the last non-blank character.
            let mut after = tail;
            after.forward_char();
            return after != end && last > tail.line();
        }
    }
    // Nothing but whitespace: every join reaches it.
    end.offset() > 0
}

/// True when GtkSourceView's `gtk_source_buffer_join_lines` would never return for a join that
/// starts at `line` and reaches the end of the buffer: the text after `line`'s last non-blank
/// character (its start if there is none) up to the end of the buffer is all whitespace, and
/// not empty. (Joins that end earlier never hang; refusing them too is harmless, as they would
/// only remove whitespace.) See [`join_range_would_hang`] for joins of several lines.
pub fn join_would_hang(buffer: &impl IsA<gtk::TextBuffer>, line: i32) -> bool {
    let end = buffer.end_iter();
    // Just after the last non-blank character of the buffer (its start if there is none).
    let mut tail = end;
    while tail.backward_char() {
        if !tail.char().is_whitespace() {
            tail.forward_char();
            break;
        }
    }
    // Just after the last non-blank character of `line` (its start if there is none).
    let Some(line_start) = buffer.iter_at_line(line) else {
        return false;
    };
    let mut first = line_start;
    if !first.ends_line() {
        first.forward_to_line_end();
    }
    while first.offset() > line_start.offset() {
        let mut previous = first;
        previous.backward_char();
        if !previous.char().is_whitespace() {
            break;
        }
        first = previous;
    }
    first.offset() >= tail.offset() && first.offset() < end.offset()
}

/// True for the ex command `:j[oin][!]` without a range or arguments.
pub fn is_join_command(command: &str) -> bool {
    let name = command.trim().trim_start_matches(':').trim();
    let name = name.strip_suffix('!').unwrap_or(name);
    matches!(name, "j" | "jo" | "joi" | "join")
}

/// An address of an ex range, as far as [`join_command_lines`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Address {
    /// A 0-based line.
    Line(i32),
    /// An address it cannot read (a mark, a pattern): the safe guess is the last line.
    Unknown,
}

/// Reads one address (`12`, `.`, `$`, `'<`, `'>`, `+2`, `.-1`, …) at the start of `s`; `None`
/// when `s` starts with none. Returns the address and the rest.
fn parse_address(
    s: &str,
    cursor: i32,
    last_line: i32,
    selection: Option<(i32, i32)>,
) -> Option<(Address, &str)> {
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let (mut address, mut rest) = match s.as_bytes().first()? {
        b'.' => (Address::Line(cursor), &s[1..]),
        b'$' => (Address::Line(last_line), &s[1..]),
        b'0'..=b'9' => {
            let n = digits(s);
            let line = s[..n].parse::<i64>().unwrap_or(i64::MAX).saturating_sub(1);
            let line = i32::try_from(line.max(0)).unwrap_or(i32::MAX);
            (Address::Line(line), &s[n..])
        }
        b'\'' => {
            let mark = s[1..].chars().next()?;
            let line = match (mark, selection) {
                ('<', Some((first, _))) => Address::Line(first),
                ('>', Some((_, last))) => Address::Line(last),
                _ => Address::Unknown,
            };
            (line, &s[1 + mark.len_utf8()..])
        }
        // A pattern (`/x/`, `?x?`) or `\/`, `\?`, `\&`.
        &delimiter @ (b'/' | b'?') => {
            let b = s.as_bytes();
            let mut i = 1;
            while i < b.len() && b[i] != delimiter {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            (Address::Unknown, s.get(i + 1..).unwrap_or(""))
        }
        b'\\' => (Address::Unknown, s.get(2..).unwrap_or("")),
        b'+' | b'-' => (Address::Line(cursor), s),
        _ => return None,
    };
    // Offsets: `+2`, `-`, `++`.
    while let Some(sign) = rest.chars().next().filter(|c| matches!(c, '+' | '-')) {
        let n = digits(&rest[1..]);
        let by = if n == 0 {
            1
        } else {
            rest[1..=n].parse::<i32>().unwrap_or(i32::MAX)
        };
        if let Address::Line(line) = &mut address {
            *line = if sign == '+' {
                line.saturating_add(by)
            } else {
                line.saturating_sub(by)
            };
        }
        rest = &rest[1 + n..];
    }
    Some((address, rest))
}

/// The lines GtkSourceView joins for the ex command `command` when it is
/// `:[range]j[oin][!] [count]`, as `(first, last)` (0-based, `last` included, and one more than
/// Vim joins, as GtkSourceView's count is one more); `None` for any other command. `cursor` is
/// the cursor's line, `last_line` the buffer's last line and `selection` the first and last
/// selected lines (for `'<` and `'>`). An address it cannot read counts as the last line.
pub fn join_command_lines(
    command: &str,
    cursor: i32,
    last_line: i32,
    selection: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    let s = command.trim().trim_start_matches(':').trim_start();
    let resolve = |a: Address| match a {
        Address::Line(line) => line.clamp(0, last_line.max(0)),
        Address::Unknown => last_line,
    };
    let (range, rest) = if let Some(rest) = s.strip_prefix('%') {
        (Some((0, last_line)), rest)
    } else {
        match parse_address(s, cursor, last_line, selection) {
            Some((a, rest)) => match rest
                .strip_prefix([',', ';'])
                .and_then(|r| parse_address(r, cursor, last_line, selection))
            {
                Some((b, rest)) => (Some((resolve(a), resolve(b))), rest),
                None => {
                    let a = resolve(a);
                    (Some((a, a)), rest.strip_prefix([',', ';']).unwrap_or(rest))
                }
            },
            None => (None, s),
        }
    };
    let rest = rest.trim_start();
    let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !matches!(&rest[..name_len], "j" | "jo" | "joi" | "join") {
        return None;
    }
    let args = rest[name_len..]
        .strip_prefix('!')
        .unwrap_or(&rest[name_len..]);
    let args = args.trim();
    let count = if args.is_empty() {
        None
    } else if args.bytes().all(|b| b.is_ascii_digit()) {
        Some(args.parse::<i32>().unwrap_or(i32::MAX))
    } else {
        // Not a join command Lemma can read: assume the worst.
        Some(i32::MAX)
    };
    let (first, last) = match range {
        None => (cursor, cursor),
        Some((a, b)) => (a.min(b), a.max(b)),
    };
    let (first, last) = match count {
        // `:j N` joins N lines from the range's last line on.
        Some(n) => (last, last.saturating_add(n.max(2) - 1)),
        None if first == last => (first, first.saturating_add(1)),
        None => (first, last),
    };
    Some((first, last.saturating_add(1)))
}

/// A Vim mode as shown in the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    VisualBlock,
    /// Typing an ex command or a search in the command bar.
    CommandLine,
}

impl VimMode {
    /// The status-bar label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Replace => "REPLACE",
            Self::Visual => "VISUAL",
            Self::VisualLine => "V-LINE",
            Self::VisualBlock => "V-BLOCK",
            Self::CommandLine => "COMMAND",
        }
    }
}

/// What the status bar shows for Vim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimStatus {
    pub mode: VimMode,
    /// The command line being typed (`:w`, `/foo`) or the pending normal-mode keys (`2d`).
    pub command: String,
}

impl VimStatus {
    /// Interprets VimIMContext's `command-bar-text` (`-- INSERT --`, `:w`, or empty in normal
    /// mode) and `command-text` (pending keys).
    pub fn from_texts(command_bar: &str, command: &str) -> Self {
        let bar = command_bar.trim();
        let mode = match bar
            .strip_prefix("--")
            .and_then(|s| s.strip_suffix("--"))
            .map(str::trim)
        {
            Some("INSERT") => VimMode::Insert,
            Some("REPLACE") => VimMode::Replace,
            Some("VISUAL") => VimMode::Visual,
            Some("VISUAL LINE") => VimMode::VisualLine,
            Some("VISUAL BLOCK") => VimMode::VisualBlock,
            Some(_) => VimMode::Normal,
            None if bar.starts_with([':', '/', '?']) => VimMode::CommandLine,
            None => VimMode::Normal,
        };
        let command = if mode == VimMode::CommandLine {
            bar.to_owned()
        } else {
            command.trim().to_owned()
        };
        Self { mode, command }
    }
}

/// An ex command about files and tabs. GtkSourceView offers every ex command to
/// `execute-command` before running its own; of these it implements only `:w` (it emits `write`)
/// and `:e` (it emits `edit`), not their `!` forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCommand {
    /// `:w [path]`, `:wq [path]`, `:x`; `force` (`:w!`, `:wq!`) also writes a file another
    /// program changed, and a `path` that exists. A `path` other than the file's own gets a copy
    /// of the text; the document stays the file it was.
    Write {
        path: Option<String>,
        quit: bool,
        force: bool,
    },
    /// `:sav[eas] path`: Save As; `force` (`:sav!`) writes over a file that exists.
    SaveAs { path: Option<String>, force: bool },
    /// `:e [path]`; `force` (`:e!`) reloads the file even with unsaved changes, discarding them.
    Edit { path: Option<String>, force: bool },
    /// `:wa`, `:wqa`, `:xa`.
    WriteAll { quit: bool },
    /// `:q`; `force` (`:q!`) discards unsaved changes.
    Quit { force: bool },
    /// `:qa`; `force` (`:qa!`) discards unsaved changes.
    QuitAll { force: bool },
}

/// Parses the write and quit families of ex commands (with or without the leading `:`).
pub fn parse_file_command(command: &str) -> Option<FileCommand> {
    let command = command.trim().trim_start_matches(':').trim_start();
    let (name, rest) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let (name, force) = match name.strip_suffix('!') {
        Some(name) => (name, true),
        None => (name, false),
    };
    let path = Some(rest.trim())
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    Some(match name {
        "w" | "write" => FileCommand::Write {
            path,
            quit: false,
            force,
        },
        "wq" | "x" | "xit" | "exit" => FileCommand::Write {
            path,
            quit: true,
            force,
        },
        "e" | "edit" => FileCommand::Edit { path, force },
        // `:sa` is Vim's `:sargument`.
        "sav" | "save" | "savea" | "saveas" => FileCommand::SaveAs { path, force },
        _ if path.is_some() => return None,
        "wa" | "wall" => FileCommand::WriteAll { quit: false },
        "wqa" | "wqall" | "xa" | "xall" => FileCommand::WriteAll { quit: true },
        "q" | "quit" | "clo" | "close" => FileCommand::Quit { force },
        "qa" | "qall" | "quita" | "quitall" => FileCommand::QuitAll { force },
        _ => return None,
    })
}

/// Why the file name of `:w name` or `:sav name` is not one to write, if so: Vim would pipe the
/// text to a command (`:w !sudo tee %`), append it to a file (`:w >> notes.tex`) or expand `%`
/// and `#` (the current and the alternate file), none of which is supported. GtkSourceView
/// hands such names over as they were typed, and writing them would make a file of that name.
pub fn unsupported_file_name(name: &str) -> Option<&'static str> {
    if name.starts_with('!') {
        Some("Writing to a command (:w !cmd) is not supported")
    } else if name.starts_with(">>") {
        Some("Appending to a file (:w >> file) is not supported")
    } else if name.contains(['%', '#']) {
        Some("File names with % or # (the current or the alternate file) are not supported")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_command_bar() {
        let s = |bar: &str, cmd: &str| VimStatus::from_texts(bar, cmd);
        assert_eq!(s("", "").mode, VimMode::Normal);
        assert_eq!(
            s("", "2d"),
            VimStatus {
                mode: VimMode::Normal,
                command: "2d".into()
            }
        );
        assert_eq!(s("-- INSERT --", "").mode, VimMode::Insert);
        assert_eq!(s("-- REPLACE --", "").mode, VimMode::Replace);
        assert_eq!(s("-- VISUAL --", "").mode, VimMode::Visual);
        assert_eq!(s("-- VISUAL LINE --", "").mode, VimMode::VisualLine);
        assert_eq!(s("-- VISUAL BLOCK --", "").mode, VimMode::VisualBlock);
        assert_eq!(
            s(":w", ""),
            VimStatus {
                mode: VimMode::CommandLine,
                command: ":w".into()
            }
        );
        assert_eq!(s("/needle", "").mode, VimMode::CommandLine);
        assert_eq!(VimMode::VisualLine.label(), "V-LINE");
    }

    #[test]
    fn join_commands() {
        for command in [":j", "join", ":join!", " :jo "] {
            assert!(is_join_command(command), "{command}");
        }
        for command in [":jumps", ":j 3", ":%j", ":w"] {
            assert!(!is_join_command(command), "{command}");
        }
    }

    #[test]
    fn join_command_ranges() {
        // Cursor on line 4 (0-based) of a buffer whose last line is 9; GtkSourceView joins one
        // line more than Vim.
        let lines = |c: &str| join_command_lines(c, 4, 9, Some((2, 3)));
        assert_eq!(lines(":j"), Some((4, 6)));
        assert_eq!(lines("join!"), Some((4, 6)));
        assert_eq!(lines(":j 3"), Some((4, 7)));
        assert_eq!(lines(":j3"), Some((4, 7)));
        assert_eq!(lines(":3j"), Some((2, 4)));
        assert_eq!(lines(":2,5j"), Some((1, 5)));
        assert_eq!(lines(":5,2j"), Some((1, 5)));
        assert_eq!(lines(":%j"), Some((0, 10)));
        assert_eq!(lines(":.,$j"), Some((4, 10)));
        assert_eq!(lines(":.,+2j"), Some((4, 7)));
        assert_eq!(lines(":'<,'>j"), Some((2, 4)));
        assert_eq!(
            lines(":'a,'bj"),
            Some((9, 11)),
            "unknown marks: the last line"
        );
        assert_eq!(lines(":/x/j"), Some((9, 11)), "a pattern: the last line");
        assert_eq!(lines(r":/a\/b/,$j"), Some((9, 11)));
        assert_eq!(lines(":$j"), Some((9, 11)));
        assert_eq!(lines(":j x"), Some((4, i32::MAX)));
        for other in [":jumps", ":w", ":3", ":s/a/b/", ":Agent join"] {
            assert_eq!(lines(other), None, "{other}");
        }
    }

    #[test]
    fn file_commands() {
        let w = |path: Option<&str>, quit| FileCommand::Write {
            path: path.map(Into::into),
            quit,
            force: false,
        };
        let w_bang = |path: Option<&str>, quit| FileCommand::Write {
            path: path.map(Into::into),
            quit,
            force: true,
        };
        let e = |path: Option<&str>, force| FileCommand::Edit {
            path: path.map(Into::into),
            force,
        };
        let parse = parse_file_command;
        assert_eq!(parse(":w"), Some(w(None, false)));
        assert_eq!(parse("w"), Some(w(None, false)));
        assert_eq!(parse(":write!"), Some(w_bang(None, false)));
        assert_eq!(parse(":w! a.tex"), Some(w_bang(Some("a.tex"), false)));
        assert_eq!(parse(":w other.tex"), Some(w(Some("other.tex"), false)));
        assert_eq!(parse(":wq"), Some(w(None, true)));
        assert_eq!(parse(":x!"), Some(w_bang(None, true)));
        assert_eq!(parse(":x"), Some(w(None, true)));
        assert_eq!(parse(":wa"), Some(FileCommand::WriteAll { quit: false }));
        assert_eq!(parse(":xa"), Some(FileCommand::WriteAll { quit: true }));
        assert_eq!(parse(":q"), Some(FileCommand::Quit { force: false }));
        assert_eq!(parse(":q!"), Some(FileCommand::Quit { force: true }));
        assert_eq!(parse(":qa!"), Some(FileCommand::QuitAll { force: true }));
        assert_eq!(
            parse(":quitall"),
            Some(FileCommand::QuitAll { force: false })
        );
        assert_eq!(parse(":wall x"), None);
        assert_eq!(parse(":q x"), None);
        assert_eq!(parse(":wn"), None);
        assert_eq!(parse(":s/a/b/"), None);
        assert_eq!(parse(":e main.tex"), Some(e(Some("main.tex"), false)));
        assert_eq!(parse(":e"), Some(e(None, false)));
        assert_eq!(parse(":e!"), Some(e(None, true)));
        assert_eq!(parse(":edit! main.tex"), Some(e(Some("main.tex"), true)));
        assert_eq!(parse(":wq b.tex"), Some(w(Some("b.tex"), true)));
        let sav = |path: Option<&str>, force| FileCommand::SaveAs {
            path: path.map(Into::into),
            force,
        };
        assert_eq!(parse(":sav b.tex"), Some(sav(Some("b.tex"), false)));
        assert_eq!(
            parse(":saveas! ch/b.tex"),
            Some(sav(Some("ch/b.tex"), true))
        );
        assert_eq!(parse(":savea b.tex"), Some(sav(Some("b.tex"), false)));
        assert_eq!(parse(":sav"), Some(sav(None, false)));
        assert_eq!(parse(":sa b.tex"), None, ":sargument");
        assert_eq!(parse(":"), None);
        // Names Vim would not write as they are.
        for name in [
            "!sudo tee %",
            ">> notes.tex",
            ">>notes.tex",
            "%.bak",
            "#",
            "a#b.tex",
        ] {
            assert!(unsupported_file_name(name).is_some(), "{name}");
        }
        for name in ["b.tex", "ch/b.tex", "a!b.tex", "a>b.tex"] {
            assert_eq!(unsupported_file_name(name), None, "{name}");
        }
    }
}
