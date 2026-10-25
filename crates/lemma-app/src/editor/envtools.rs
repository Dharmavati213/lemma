//! Environment tools (M5 WP5.3, from M4): live rename, Change Environment, Toggle Star and
//! Delete Environment. No Vim bindings.
//!
//! - **Live rename:** editing the name in `\begin{…}` or `\end{…}` renames its partner, in the
//!   same undo step. The partner is found with the M3 pairs code ([`pairs::pair_at`]: the
//!   environment cache, never a scan of the document per keystroke) **before** the first edit
//!   of the name, while both names still match; from then on the two names are a *link* whose
//!   ranges follow every edit, until an edit lands outside the name (or is not the user's:
//!   undo, agent, review, reload). The partner is renamed when the user action ends (GTK groups
//!   that edit with the typing; Vim's insert mode keeps its action open, so there it happens
//!   from an idle inside the action). A name with braces, a backslash, `%`, `#`, `$` or white
//!   space is not copied and ends the link; an empty one is (the user is typing a new name).
//! - **Change Environment…** (`win.change-environment`, Ctrl+Shift+E, palette): a popover at
//!   the innermost environment around the cursor ([`pairs::enclosing_env`]) asks for the new
//!   name and renames both ends.
//! - **Toggle Star** (`win.toggle-environment-star`, Ctrl+* = Ctrl+Shift+8 on US layouts):
//!   `align` ↔ `align*` at both ends.
//! - **Delete Environment** (`win.delete-environment`, palette): removes `\begin{…}` (with its
//!   arguments when they are alone on the line) and `\end{…}`, keeps the body and dedents it by
//!   the body's extra indentation.
//!
//! Every command is one undo step (`Document::apply_edits`, origin `User`). The text
//! transformations are pure functions ([`rename_edits`], [`toggle_star`], [`delete_edits`],
//! [`name_problem`]) with table tests; `tests/gtk_envtools.rs` drives the actions, the popover,
//! live rename and undo in a window.
//!
//! Hooks: [`install`] once per window (actions, accelerators, descriptions); [`attach`] /
//! [`detach`] per editor tab (live rename; after `pairs::attach`). Test API: [`linked`],
//! [`prompt_entry`], [`change_environment_in`], [`toggle_star_in`], [`delete_environment_in`].

use super::Editor;
use super::pairs::{self, EnvSpan, PairKind, Partner, Side};
use crate::app::{self, ShortcutGroup};
use crate::document::{Document, ObserverId};
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};

/// The actions (without `win.`), their labels and default accelerators.
const ACTIONS: &[(&str, &str, &[&str])] = &[
    (
        "change-environment",
        "Change Environment…",
        &["<Control><Shift>e"],
    ),
    (
        "toggle-environment-star",
        "Toggle Environment Star",
        &["<Control>asterisk"],
    ),
    ("delete-environment", "Delete Environment", &[]),
];

const NO_ENV: &str = "Put the cursor inside an environment";

// ───────────── Pure text functions ─────────────

/// The byte range of the name inside a `\begin{name}` or `\end{name}` delimiter's text.
pub fn name_range(token: &str) -> Option<Range<usize>> {
    let open = token.find('{')? + 1;
    let close = token.rfind('}')?;
    (open <= close).then_some(open..close)
}

/// Why `name` cannot be an environment name (`None`: it can): empty, or with braces, a
/// backslash, `%`, `#` or white space.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("Type an environment name")
    } else if name
        .chars()
        .any(|c| matches!(c, '{' | '}' | '\\' | '%' | '#' | '$') || c.is_whitespace())
    {
        Some("An environment name cannot contain braces, \\, %, #, $ or spaces")
    } else {
        None
    }
}

/// The name with its star toggled: `align` → `align*`, `align*` → `align`.
pub fn toggle_star(name: &str) -> String {
    match name.strip_suffix('*') {
        Some(plain) => plain.to_owned(),
        None => format!("{name}*"),
    }
}

/// The edits that rename the environment `span` of `text` to `new` (descending).
pub fn rename_edits(text: &str, span: &EnvSpan, new: &str) -> Option<Vec<TextEdit>> {
    let at = |tok: &Range<usize>| -> Option<Range<usize>> {
        let r = name_range(text.get(tok.clone())?)?;
        Some(tok.start + r.start..tok.start + r.end)
    };
    let (begin, end) = (at(&span.begin)?, at(&span.end)?);
    Some(vec![
        TextEdit {
            range: end,
            text: new.to_owned(),
        },
        TextEdit {
            range: begin,
            text: new.to_owned(),
        },
    ])
}

/// The bounds of the line holding byte `at`: (start, end without the `\n`).
pub(crate) fn line_bounds(text: &str, at: usize) -> Range<usize> {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    start..end
}

/// The leading white space of `line`.
pub(crate) fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// The length of the bracket and brace groups (`[pos]{cols}`) at the start of `s`, white space
/// between them included; `brackets_only` skips only `[…]` groups.
pub(crate) fn arguments_len(s: &str, brackets_only: bool) -> usize {
    let b = s.as_bytes();
    let mut end = 0;
    let mut i = 0;
    loop {
        while i < b.len() && matches!(b[i], b' ' | b'\t') {
            i += 1;
        }
        let close = match b.get(i) {
            Some(b'[') => b']',
            Some(b'{') if !brackets_only => b'}',
            _ => return end,
        };
        let open = b[i];
        let mut depth = 0i32;
        let mut j = i;
        let mut done = false;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 1,
                b'\n' => break,
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        done = true;
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if !done {
            return end;
        }
        i = j + 1;
        end = i;
    }
}

/// The edits that delete the environment `span` of `text` but keep its body (descending):
///
/// - `\begin{x}` alone on its line (with its arguments, white space and nothing else) goes
///   with its line; else the delimiter and its `[…]` arguments go (and the white space before
///   it when nothing follows it);
/// - `\end{x}` likewise;
/// - the lines in between lose the body's extra indentation: the least indentation beyond the
///   `\begin` line's of their non-blank lines.
pub fn delete_edits(text: &str, span: &EnvSpan) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    // `\end`.
    let end_line = line_bounds(text, span.end.start);
    let before_end = &text[end_line.start..span.end.start];
    let after_end = &text[span.end.end..end_line.end];
    if before_end.trim().is_empty() && after_end.trim().is_empty() {
        edits.push(TextEdit::delete(whole_line(text, end_line.clone())));
    } else if after_end.trim().is_empty() {
        let from = end_line.start + before_end.trim_end().len();
        edits.push(TextEdit::delete(from..end_line.end));
    } else {
        edits.push(TextEdit::delete(span.end.clone()));
    }
    // The body.
    let begin_line = line_bounds(text, span.begin.start);
    let mut dedents = Vec::new();
    if end_line.start > begin_line.end {
        let body = begin_line.end + 1..end_line.start;
        let base = indent_of(&text[begin_line.clone()]);
        let lines: Vec<(usize, &str)> = text[body.clone()]
            .split_inclusive('\n')
            .scan(body.start, |at, l| {
                let start = *at;
                *at += l.len();
                Some((start, l.trim_end_matches('\n')))
            })
            .collect();
        let extra = lines
            .iter()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(_, l)| l.strip_prefix(base).map_or(0, |rest| indent_of(rest).len()))
            .min()
            .unwrap_or(0);
        if extra > 0 {
            for (start, l) in lines.iter().rev() {
                if !l.trim().is_empty() {
                    let from = start + base.len();
                    dedents.push(TextEdit::delete(from..from + extra));
                }
            }
        }
    }
    edits.extend(dedents);
    // `\begin`.
    let before_begin = &text[begin_line.start..span.begin.start];
    let rest = &text[span.begin.end..begin_line.end];
    let args = arguments_len(rest, false);
    if before_begin.trim().is_empty() && rest[args..].trim().is_empty() {
        edits.push(TextEdit::delete(whole_line(text, begin_line)));
    } else {
        let args = arguments_len(rest, true);
        let mut to = span.begin.end + args;
        let after = &text[to..begin_line.end];
        if before_begin.trim().is_empty() {
            to += after.len() - after.trim_start().len();
        }
        let from = if after.trim().is_empty() {
            begin_line.start + before_begin.trim_end().len()
        } else {
            span.begin.start
        };
        edits.push(TextEdit::delete(from..to));
    }
    // Overlapping edits (begin and end on one line) are merged.
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut out: Vec<TextEdit> = Vec::new();
    for e in edits {
        match out.last_mut() {
            Some(last) if e.range.end > last.range.start => {
                last.range = last.range.start.min(e.range.start)..last.range.end.max(e.range.end);
            }
            _ => out.push(e),
        }
    }
    out
}

/// A line's range with its `\n` (or, for the last line, the `\n` before it).
fn whole_line(text: &str, line: Range<usize>) -> Range<usize> {
    if line.end < text.len() {
        line.start..line.end + 1
    } else if line.start > 0 {
        line.start - 1..line.end
    } else {
        line
    }
}

// ───────────── Commands ─────────────

/// Adds the actions, their default accelerators and their palette descriptions.
pub fn install(state: &Rc<WindowState>) {
    for &(name, label, _) in ACTIONS {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        let name = name.to_owned();
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                run(&state, &name);
            }
        });
        state.window.add_action(&action);
        app::describe_action(
            &format!("win.{}", action.name()),
            label,
            ShortcutGroup::Editing,
            None,
        );
    }
    let accels: Vec<(String, &[&str])> = ACTIONS
        .iter()
        .filter(|(_, _, keys)| !keys.is_empty())
        .map(|&(name, _, keys)| (format!("win.{name}"), keys))
        .collect();
    let accels: Vec<(&str, &[&str])> = accels.iter().map(|(n, k)| (n.as_str(), *k)).collect();
    app::set_default_accels(state, &accels);
}

fn run(state: &Rc<WindowState>, action: &str) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    let result = match action {
        "change-environment" => {
            open_prompt(state, &editor);
            Ok(())
        }
        "toggle-environment-star" => toggle_star_in(&editor),
        "delete-environment" => delete_environment_in(&editor),
        _ => Ok(()),
    };
    if let Err(message) = result {
        state.toast(&message);
    }
}

/// The innermost environment around `editor`'s cursor.
fn current_env(editor: &Editor) -> Result<EnvSpan, String> {
    pairs::enclosing_env(editor, editor.cursor_offset()).ok_or_else(|| NO_ENV.to_owned())
}

/// Applies `edits` to `editor`'s document as one undo step (live rename stays out of it).
fn apply(editor: &Editor, edits: &[TextEdit]) -> Result<(), String> {
    SUSPENDED.with(|s| s.set(s.get() + 1));
    let result = editor.document().apply_edits(edits, Origin::User);
    SUSPENDED.with(|s| s.set(s.get() - 1));
    result.map_err(|e| format!("Could not change the text: {e}"))
}

/// Renames the innermost environment around `editor`'s cursor to `new` (both ends).
pub fn change_environment_in(editor: &Editor, new: &str) -> Result<(), String> {
    if let Some(problem) = name_problem(new) {
        return Err(problem.to_owned());
    }
    let span = current_env(editor)?;
    if span.name == new {
        return Ok(());
    }
    let edits = editor
        .document()
        .with_text(|text| rename_edits(text, &span, new))
        .ok_or_else(|| NO_ENV.to_owned())?;
    apply(editor, &edits)
}

/// `win.toggle-environment-star` in `editor`.
pub fn toggle_star_in(editor: &Editor) -> Result<(), String> {
    let span = current_env(editor)?;
    let new = toggle_star(&span.name);
    let edits = editor
        .document()
        .with_text(|text| rename_edits(text, &span, &new))
        .ok_or_else(|| NO_ENV.to_owned())?;
    apply(editor, &edits)
}

/// `win.delete-environment` in `editor`.
pub fn delete_environment_in(editor: &Editor) -> Result<(), String> {
    let span = current_env(editor)?;
    let edits = editor
        .document()
        .with_text(|text| delete_edits(text, &span));
    apply(editor, &edits)
}

// ───────────── The Change Environment popover ─────────────

/// The open Change Environment popover.
struct Prompt {
    editor: Weak<Editor>,
    popover: gtk::Popover,
    entry: gtk::Entry,
}

thread_local! {
    static PROMPT: RefCell<Option<Prompt>> = const { RefCell::new(None) };
    /// Non-zero while this module (or align, format) applies its own edits.
    static SUSPENDED: Cell<u32> = const { Cell::new(0) };
}

/// Runs `f` without live rename (for other modules' edits of environment names).
pub fn without_live_rename<R>(f: impl FnOnce() -> R) -> R {
    SUSPENDED.with(|s| s.set(s.get() + 1));
    let r = f();
    SUSPENDED.with(|s| s.set(s.get() - 1));
    r
}

/// The entry of the open Change Environment popover (tests).
pub fn prompt_entry() -> Option<gtk::Entry> {
    PROMPT.with_borrow(|p| p.as_ref().map(|p| p.entry.clone()))
}

fn close_prompt() {
    if let Some(p) = PROMPT.take() {
        p.popover.popdown();
        p.popover.unparent();
    }
}

fn open_prompt(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    close_prompt();
    let span = match current_env(editor) {
        Ok(span) => span,
        Err(message) => {
            state.toast(&message);
            return;
        }
    };
    let heading = gtk::Label::builder()
        .label("Change Environment")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let entry = gtk::Entry::builder()
        .text(&span.name)
        .width_chars(24)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New environment name")]);
    let hint = gtk::Label::builder()
        .label("Enter renames \\begin and \\end · Esc cancels")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&heading);
    content.append(&entry);
    content.append(&hint);
    let popover = gtk::Popover::builder()
        .child(&content)
        .position(gtk::PositionType::Bottom)
        .autohide(true)
        .build();
    popover.add_css_class("lemma-change-env");
    let view = editor.view();
    view.adopt_popover(&popover);
    if let Some(rect) = editor.range_rect(span.begin.clone(), span.begin.start) {
        let x = rect.x().clamp(0, (view.width() - 1).max(0));
        let y = rect.y().clamp(0, (view.height() - 1).max(0));
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            x,
            y,
            rect.width().max(1),
            rect.height().max(1),
        )));
    }
    let (weak_state, weak_editor) = (Rc::downgrade(state), Rc::downgrade(editor));
    entry.connect_activate(move |entry| {
        let (Some(state), Some(editor)) = (weak_state.upgrade(), weak_editor.upgrade()) else {
            return;
        };
        let new = entry.text();
        match change_environment_in(&editor, new.trim()) {
            Ok(()) => {
                close_prompt();
                editor.view().grab_focus();
            }
            Err(message) => state.toast(&message),
        }
    });
    let weak_view = view.downgrade();
    popover.connect_closed(move |popover| {
        let popover = popover.clone();
        let weak_view = weak_view.clone();
        // Unparented after the close finishes; refocus the editor.
        glib::idle_add_local_once(move || {
            PROMPT.with_borrow_mut(|p| {
                if p.as_ref().is_some_and(|p| p.popover == popover) {
                    *p = None;
                }
            });
            if popover.parent().is_some() {
                popover.unparent();
            }
            if let Some(view) = weak_view.upgrade() {
                view.grab_focus();
            }
        });
    });
    PROMPT.set(Some(Prompt {
        editor: Rc::downgrade(editor),
        popover: popover.clone(),
        entry: entry.clone(),
    }));
    popover.popup();
    entry.grab_focus();
    entry.select_region(0, -1);
}

// ───────────── Live rename ─────────────

/// Two environment names that follow each other (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The name being edited (document bytes).
    pub name: Range<usize>,
    /// The partner's name (document bytes).
    pub partner: Range<usize>,
    /// The partner has not been renamed after the latest edit yet.
    pub dirty: bool,
}

/// One editor's live rename.
struct Live {
    editor: Weak<Editor>,
    link: RefCell<Option<Link>>,
    /// While this module renames the partner.
    syncing: Cell<bool>,
    /// Between the buffer's outermost `begin-user-action` and `end-user-action`.
    in_action: Cell<bool>,
    /// Inside a `Document::apply_edits` (until the main loop's next iteration).
    applying: Cell<bool>,
    idle: RefCell<Option<glib::SourceId>>,
    observers: RefCell<Vec<ObserverId>>,
    signals: RefCell<Vec<glib::SignalHandlerId>>,
}

thread_local! {
    static LIVE: RefCell<Vec<Rc<Live>>> = const { RefCell::new(Vec::new()) };
}

fn live_of(editor: &Editor) -> Option<Rc<Live>> {
    LIVE.with_borrow(|all| {
        all.iter()
            .find(|l| {
                l.editor
                    .upgrade()
                    .is_some_and(|e| std::ptr::eq(&*e, editor))
            })
            .cloned()
    })
}

/// The current link of `editor` (tests).
pub fn linked(editor: &Editor) -> Option<Link> {
    live_of(editor).and_then(|l| l.link.borrow().clone())
}

/// Starts live rename in `editor` (LaTeX documents only).
pub fn attach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let doc = editor.document();
    if !doc
        .path()
        .extension()
        .is_some_and(|e| e == "tex" || e == "sty" || e == "cls" || e == "ltx" || e == "dtx")
    {
        return;
    }
    let live = Rc::new(Live {
        editor: Rc::downgrade(editor),
        link: RefCell::new(None),
        syncing: Cell::new(false),
        in_action: Cell::new(false),
        applying: Cell::new(false),
        idle: RefCell::new(None),
        observers: RefCell::new(Vec::new()),
        signals: RefCell::new(Vec::new()),
    });
    let weak = Rc::downgrade(&live);
    let before = doc.connect_edit(move |doc, edit, origin| {
        if let Some(live) = weak.upgrade() {
            before_edit(&live, doc, edit, origin);
        }
    });
    let weak = Rc::downgrade(&live);
    let after = doc.connect_changed(move |_, _, _| {
        if let Some(live) = weak.upgrade()
            && live.link.borrow().as_ref().is_some_and(|l| l.dirty)
            && !live.syncing.get()
        {
            queue_sync(&live);
        }
    });
    // Edits made through `apply_edits` (agent, review, snippets, other commands) are not typing.
    let weak = Rc::downgrade(&live);
    let apply = doc.connect_before_apply(move |_, _, _| {
        let Some(live) = weak.upgrade() else {
            return;
        };
        live.applying.set(true);
        let weak = Rc::downgrade(&live);
        glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(live) = weak.upgrade() {
                live.applying.set(false);
            }
            glib::ControlFlow::Break
        });
    });
    live.observers.replace(vec![before, after, apply]);
    let buffer = editor.buffer();
    let weak = Rc::downgrade(&live);
    let begin = buffer.connect_begin_user_action(move |_| {
        if let Some(live) = weak.upgrade() {
            live.in_action.set(true);
        }
    });
    let weak = Rc::downgrade(&live);
    let end = buffer.connect_end_user_action(move |_| {
        if let Some(live) = weak.upgrade() {
            live.in_action.set(false);
            sync(&live);
        }
    });
    live.signals.replace(vec![begin, end]);
    LIVE.with_borrow_mut(|all| all.push(live));
}

/// Stops live rename in `editor` and closes its Change Environment popover.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if PROMPT.with_borrow(|p| {
        p.as_ref()
            .is_some_and(|p| p.editor.upgrade().is_none_or(|e| Rc::ptr_eq(&e, editor)))
    }) {
        close_prompt();
    }
    let Some(live) = live_of(editor) else {
        return;
    };
    LIVE.with_borrow_mut(|all| all.retain(|l| !Rc::ptr_eq(l, &live)));
    for id in live.observers.take() {
        editor.document().disconnect(id);
    }
    for id in live.signals.take() {
        editor.buffer().disconnect(id);
    }
    if let Some(id) = live.idle.take() {
        id.remove();
    }
}

/// `inner` lies within `outer` (ends included).
fn within(inner: &Range<usize>, outer: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// `r` after an edit of `edit.range` (before or after it, not overlapping it) to `len` bytes.
fn shift(r: &Range<usize>, edit: &Range<usize>, len: usize) -> Range<usize> {
    if r.start >= edit.end {
        r.start - edit.len() + len..r.end - edit.len() + len
    } else {
        r.clone()
    }
}

/// The document text of `range` when it lies on one line (no copy of the whole text).
fn line_slice(doc: &Document, range: &Range<usize>) -> Option<String> {
    let line = doc.line_index().line_of(range.start);
    let start = doc.line_index().line_start(line);
    let text = doc.line_text(line)?;
    text.get(range.start - start..range.end - start)
        .map(str::to_owned)
}

/// Before each change (pre-edit offsets): keeps the link in step, ends it, or starts one.
fn before_edit(live: &Rc<Live>, doc: &Document, edit: &TextEdit, origin: Origin) {
    let len = edit.text.len();
    if live.syncing.get() {
        if let Some(link) = live.link.borrow_mut().as_mut() {
            link.name = shift(&link.name, &edit.range, len);
            link.partner = edit.range.start..edit.range.start + len;
        }
        return;
    }
    // Typing: the user's edits inside a user action (GtkTextView wraps each one in one, Vim's
    // insert mode keeps one open), not made through `apply_edits`.
    let user = origin == Origin::User
        && SUSPENDED.with(Cell::get) == 0
        && live.in_action.get()
        && !live.applying.get();
    let safe = !edit
        .text
        .chars()
        .any(|c| matches!(c, '{' | '}' | '\\' | '%' | '#' | '$') || c.is_whitespace());
    {
        let mut link = live.link.borrow_mut();
        if let Some(l) = link.as_mut() {
            if user && safe && within(&edit.range, &l.name) {
                l.name.end = l.name.end - edit.range.len() + len;
                l.partner = shift(&l.partner, &edit.range, len);
                l.dirty = true;
                return;
            }
            *link = None;
        }
    }
    if !user || !safe {
        return;
    }
    let Some(editor) = live.editor.upgrade() else {
        return;
    };
    let Some(pair) = pairs::pair_at(&editor, edit.range.start, Side::On) else {
        return;
    };
    let Partner::Found(partner) = pair.partner else {
        return;
    };
    if pair.kind != PairKind::Env {
        return;
    }
    let (Some(token), Some(other)) = (line_slice(doc, &pair.token), line_slice(doc, &partner))
    else {
        return;
    };
    let (Some(name), Some(other_name)) = (name_range(&token), name_range(&other)) else {
        return;
    };
    let name = pair.token.start + name.start..pair.token.start + name.end;
    let other_name = partner.start + other_name.start..partner.start + other_name.end;
    if !within(&edit.range, &name) {
        return;
    }
    live.link.replace(Some(Link {
        name: name.start..name.end - edit.range.len() + len,
        partner: shift(&other_name, &edit.range, len),
        dirty: true,
    }));
}

/// Renames the partner from an idle when the user action stays open (Vim's insert mode).
fn queue_sync(live: &Rc<Live>) {
    if live.idle.borrow().is_some() {
        return;
    }
    let weak = Rc::downgrade(live);
    let id = glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
        if let Some(live) = weak.upgrade() {
            live.idle.take();
            sync(&live);
        }
        glib::ControlFlow::Break
    });
    live.idle.replace(Some(id));
}

/// Gives the partner the edited name, if the link is dirty and the name is valid.
fn sync(live: &Rc<Live>) {
    if live.syncing.get() {
        return;
    }
    let Some(link) = live.link.borrow().clone().filter(|l| l.dirty) else {
        return;
    };
    let Some(editor) = live.editor.upgrade() else {
        return;
    };
    let doc = editor.document();
    let (Some(name), Some(partner)) = (line_slice(doc, &link.name), line_slice(doc, &link.partner))
    else {
        live.link.replace(None);
        return;
    };
    if let Some(l) = live.link.borrow_mut().as_mut() {
        l.dirty = false;
    }
    // An empty name is copied too (the user is typing a new one).
    if name == partner || (!name.is_empty() && name_problem(&name).is_some()) {
        return;
    }
    // The partner must still be a delimiter's name: `{` before it and `}` after it.
    let around = link
        .partner
        .start
        .checked_sub(1)
        .map(|s| s..link.partner.end + 1);
    let still = around
        .and_then(|r| line_slice(doc, &r))
        .is_some_and(|s| s.starts_with('{') && s.ends_with('}'));
    if !still {
        live.link.replace(None);
        return;
    }
    live.syncing.set(true);
    let result = doc.apply_edits(
        &[TextEdit {
            range: link.partner.clone(),
            text: name,
        }],
        Origin::User,
    );
    live.syncing.set(false);
    if result.is_err() {
        live.link.replace(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, name: &str, nth: usize) -> EnvSpan {
        let begin_tok = format!("\\begin{{{name}}}");
        let end_tok = format!("\\end{{{name}}}");
        let b = text.match_indices(&begin_tok).nth(nth).unwrap().0;
        let e = text[b..].find(&end_tok).unwrap() + b;
        EnvSpan {
            name: name.to_owned(),
            begin: b..b + begin_tok.len(),
            end: e..e + end_tok.len(),
        }
    }

    fn apply_all(text: &str, edits: &[TextEdit]) -> String {
        assert!(
            edits.windows(2).all(|w| w[1].range.end <= w[0].range.start),
            "descending: {edits:?}"
        );
        let mut s = text.to_owned();
        for e in edits {
            s.replace_range(e.range.clone(), &e.text);
        }
        s
    }

    #[test]
    fn names() {
        assert_eq!(name_range("\\begin{align*}"), Some(7..13));
        assert_eq!(name_range("\\end{x}"), Some(5..6));
        assert_eq!(name_range("\\begin{}"), Some(7..7));
        assert_eq!(name_range("\\begin"), None);
        assert_eq!(toggle_star("align"), "align*");
        assert_eq!(toggle_star("align*"), "align");
        assert_eq!(toggle_star("equation"), "equation*");
        for (name, ok) in [
            ("align", true),
            ("my-env:2*", true),
            ("", false),
            ("a b", false),
            ("a}", false),
            ("a\\b", false),
            ("a%", false),
        ] {
            assert_eq!(name_problem(name).is_none(), ok, "{name:?}");
        }
    }

    #[test]
    fn rename_and_toggle_star_tables() {
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "\\begin{align}\n  a &= b\n\\end{align}\n",
                "align",
                "gather",
                "\\begin{gather}\n  a &= b\n\\end{gather}\n",
            ),
            (
                "x \\begin{equation} y \\end{equation} z",
                "equation",
                "equation*",
                "x \\begin{equation*} y \\end{equation*} z",
            ),
            (
                "\\begin{theorem}[Main]\\label{t}\nText.\n\\end{theorem}",
                "theorem",
                "lemma",
                "\\begin{lemma}[Main]\\label{t}\nText.\n\\end{lemma}",
            ),
        ];
        for &(text, name, new, want) in cases {
            let edits = rename_edits(text, &span(text, name, 0), new).unwrap();
            assert_eq!(apply_all(text, &edits), want);
        }
        let text = "\\begin{align*}\nx\n\\end{align*}";
        let s = span(text, "align*", 0);
        let edits = rename_edits(text, &s, &toggle_star(&s.name)).unwrap();
        assert_eq!(apply_all(text, &edits), "\\begin{align}\nx\n\\end{align}");
    }

    #[test]
    fn delete_tables() {
        let cases: &[(&str, &str, &str)] = &[
            // Alone on their lines: lines go, the body is dedented by its extra indentation.
            (
                "a\n\\begin{center}\n  Text\n    more\n\n  end\n\\end{center}\nb\n",
                "center",
                "a\nText\n  more\n\nend\nb\n",
            ),
            // Nested and indented: the base indentation stays.
            (
                "\\begin{document}\n  \\begin{proof}\n    Easy.\n  \\end{proof}\n\\end{document}\n",
                "proof",
                "\\begin{document}\n  Easy.\n\\end{document}\n",
            ),
            // Arguments alone on the `\begin` line go too.
            (
                "\\begin{minipage}[t]{0.5\\textwidth}\n  Body\n\\end{minipage}\n",
                "minipage",
                "Body\n",
            ),
            // Inline: only the delimiters (and `[…]` arguments) go.
            (
                "x \\begin{bfseries}[y] bold \\end{bfseries} z",
                "bfseries",
                "x  bold  z",
            ),
            // Text after `\begin` on its line stays; no dedent of a body that is not indented.
            (
                "\\begin{itemize} \\item a\n\\item b\n\\end{itemize}\n",
                "itemize",
                "\\item a\n\\item b\n",
            ),
            // `\end` after text: the white space before it goes.
            (
                "\\begin{equation}\n  x = 1 \\end{equation}\n",
                "equation",
                "  x = 1\n",
            ),
            // The last line of the text.
            ("\\begin{x}\nA\n\\end{x}", "x", "A"),
            // Tabs.
            ("\\begin{x}\n\tA\n\t\tB\n\\end{x}\n", "x", "A\n\tB\n"),
        ];
        for &(text, name, want) in cases {
            let edits = delete_edits(text, &span(text, name, 0));
            assert_eq!(apply_all(text, &edits), want, "{text:?}");
        }
    }

    #[test]
    fn argument_lengths() {
        assert_eq!(arguments_len("[t]{0.5\\textwidth} x", false), 18);
        assert_eq!(arguments_len("[t]{0.5\\textwidth} x", true), 3);
        assert_eq!(arguments_len(" {a{b}c}", false), 8);
        assert_eq!(arguments_len("{unclosed", false), 0);
        assert_eq!(arguments_len("x", false), 0);
    }
}
