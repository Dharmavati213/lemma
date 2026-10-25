//! GtkSourceView snippets built from `lemma-snippets` chunks (WP1.8, ARCHITECTURE §10.5), and
//! the snippet driver (WP2.9a, §5 "Snippets").
//!
//! Snippets are assembled chunk by chunk and never parsed: GtkSourceView 5.20's
//! `Snippet::new_parsed` makes no tab stop from a bare `$1` and drops the dollar of `\$`, which
//! breaks bodies such as `mk` (`$$1$$0`) (ARCHITECTURE §0 item 2).
//!
//! [`expand`] replaces the typed trigger with the snippet as a single undo step.
//!
//! # The driver
//!
//! A [`Driver`] runs the snippets of one view (`editor::completion` creates it and feeds it the
//! key presses of the key layer):
//!
//! - **Autosnippets** ([`Driver::autosnippet`]) are decided on the key press of a typed
//!   character, before the character reaches Vim or the view: the line as it will be and the
//!   scanner context after the character go to `SnippetSet::auto_match`. On a match the driver
//!   types the character itself and expands the snippet as an undo step of its own, so one undo
//!   gives back the literal trigger. See [`typed_char`] for
//!   why the key press, and nothing that changes the buffer, is the signal: pastes, IM commits,
//!   undo/redo, agent, review and reload edits never arrive as a key press of a character.
//! - **Tab** ([`Driver::tab`]): an exact manual trigger expands; else, inside an active snippet,
//!   GtkSourceView moves to the next stop; else the cursor steps out past the next `}`, `)`, `]`
//!   or (inside inline math) `$` on the line, escaped ones (`\}`, `\$`) not counted, and never
//!   past a `$` that opens a formula; else the view indents. **Shift+Tab** moves back inside an
//!   active snippet and otherwise does what the view does (unindent).
//! - **Expansions inside an active snippet** ([`Driver::expand_chunks`]): GtkSourceView 5.20
//!   loses track of an outer snippet's text while a nested one runs (typing in the outer's next
//!   stop then replaced the nested text, and Tab after the nested one wrapped around), and ends
//!   its session when a deletion reaches the end of the focused stop. So a body with stops and
//!   the outer snippet's remaining stops become one snippet (`ff` in `ff`: Tab visits the inner
//!   numerator and denominator, then the outer denominator), and a body without stops (`->` →
//!   `\to`) is typed in place, keeping the session.
//! - Line breaks of expansions are the document's (`\r\n` in a CRLF file).
//! - Every snippet session ends before an edit that is not typing ([`Document::apply_edits`]:
//!   agent, review and reload edits), and before undo and redo (WP1.8).
//! - In Vim insert mode, where GtkSourceView keeps one user action open for the whole insert,
//!   an expansion closes it first and opens it again afterwards, so `u` after `<Esc>` also gives
//!   back the trigger.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::{Rc, Weak};

use crate::document::{DocRef, Document, ObserverId};
use crate::editor::TexView;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use lemma_snippets::{Chunk, SnippetMatch, SnippetSet, with_indent};
use lemma_syntax::Context;
use sourceview5::prelude::*;

/// The focus position of chunks the cursor never stops in.
const NO_FOCUS: i32 = -1;

/// Builds a snippet chunk by chunk (never parsed):
///
/// - `Text` → `set_text` + `set_text_set(true)`, focus −1;
/// - `Stop { n, default }` → focus `n` (GtkSourceView visits 1, 2, … and ends at 0), plus
///   `set_text` + `set_text_set(true)` when there is a default, which is then pre-selected;
/// - `Mirror(n)` → `set_spec("$n")`, focus −1, so it repeats whatever is typed into stop `n`.
///
/// Literal text is set verbatim, so `$` and `\` need no escaping.
pub fn build_gsv_snippet(chunks: &[Chunk]) -> sourceview5::Snippet {
    let snippet = sourceview5::Snippet::new(None, None);
    for chunk in chunks {
        snippet.add_chunk(&gsv_chunk(chunk));
    }
    snippet
}

fn gsv_chunk(chunk: &Chunk) -> sourceview5::SnippetChunk {
    let out = sourceview5::SnippetChunk::new();
    match chunk {
        Chunk::Text(text) => {
            set_literal(&out, text);
            out.set_focus_position(NO_FOCUS);
        }
        Chunk::Stop { n, default } => {
            out.set_focus_position(i32::from(*n));
            if !default.is_empty() {
                set_literal(&out, default);
            }
        }
        Chunk::Mirror(n) => {
            out.set_spec(&format!("${n}"));
            out.set_focus_position(NO_FOCUS);
        }
    }
    out
}

fn set_literal(chunk: &sourceview5::SnippetChunk, text: &str) {
    chunk.set_text(text);
    chunk.set_text_set(true);
}

/// Why [`expand`] or [`expand_chunks`] left the buffer alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpandError {
    /// The view is not editable.
    NotEditable,
    /// The text before the cursor does not end with this trigger: the match is stale, or its
    /// `trigger_len` does not fit its trigger.
    TriggerNotBeforeCursor(String),
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEditable => f.write_str("the view is not editable"),
            Self::TriggerNotBeforeCursor(trigger) => {
                write!(
                    f,
                    "the text before the cursor does not end with {trigger:?}"
                )
            }
        }
    }
}

impl std::error::Error for ExpandError {}

/// The chunks to expand for `m`: its `alt_chunks` when the snippet stands for a macro
/// ([`SnippetDef::required_macro`](lemma_snippets::SnippetDef::required_macro)) that
/// `macro_defined` reports as undefined, else its `chunks`.
///
/// `macro_defined` gets the macro name without the backslash (`"N"` for `\N`). It is called at
/// most once, and only for snippets that have an alternative body.
pub fn chunks_for<'a>(
    m: &SnippetMatch<'a>,
    macro_defined: impl FnOnce(&str) -> bool,
) -> &'a [Chunk] {
    match (m.def.required_macro(), m.alt_chunks) {
        (Some(name), Some(alt)) if !macro_defined(name) => alt,
        _ => m.chunks,
    }
}

/// Replaces the trigger of `m` before the cursor with its snippet, as one undo step.
///
/// The body is chosen with [`chunks_for`] and `macro_defined`; the deleted text is the first
/// `m.trigger_len` bytes of the trigger (all of it for an autosnippet, the typed prefix for a
/// manual match). See [`expand_chunks`] for the rest. Returns the pushed snippet.
pub fn expand(
    view: &impl IsA<sourceview5::View>,
    m: &SnippetMatch<'_>,
    macro_defined: impl FnOnce(&str) -> bool,
) -> Result<sourceview5::Snippet, ExpandError> {
    let typed = m
        .def
        .trigger
        .get(..m.trigger_len)
        .ok_or_else(|| ExpandError::TriggerNotBeforeCursor(m.def.trigger.clone()))?;
    expand_chunks(view, chunks_for(m, macro_defined), typed)
}

/// Replaces `typed`, the text right before the cursor, with a snippet made of `chunks`.
///
/// - Continuation lines are indented with the leading whitespace (spaces and tabs) of the
///   cursor's line, because GtkSourceView does not indent them ([`with_indent`]), and their line
///   breaks are the document's (the cursor line's own, else the first line's: `\r\n` in a CRLF
///   file).
/// - Deleting `typed` and pushing the snippet happen inside one user action, so a single undo
///   brings back the literal trigger.
/// - Pushing selects the first tab stop (its default, if any) or, for a snippet without numbered
///   stops, puts the cursor at `$0`.
///
/// Nothing changes when the view is not editable or the text before the cursor does not end with
/// `typed`. Returns the pushed snippet.
pub fn expand_chunks(
    view: &impl IsA<sourceview5::View>,
    chunks: &[Chunk],
    typed: &str,
) -> Result<sourceview5::Snippet, ExpandError> {
    let view = view.as_ref();
    let chunks = prepare_chunks(view, chunks, typed)?;
    let buffer = view.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    Ok(push_prepared(view, &chunks, char_count(typed), &cursor).0)
}

/// `chunks` ready to replace `typed` before the cursor: indented like the cursor's line, with
/// the document's line breaks (see [`expand_chunks`]).
fn prepare_chunks(
    view: &sourceview5::View,
    chunks: &[Chunk],
    typed: &str,
) -> Result<Vec<Chunk>, ExpandError> {
    if !view.is_editable() {
        return Err(ExpandError::NotEditable);
    }
    let buffer = view.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let mut line_start = cursor;
    line_start.set_line_offset(0);
    // `slice` keeps U+FFFC for embedded objects, so it has one char per buffer position.
    let before_cursor = buffer.slice(&line_start, &cursor, true);
    let before_trigger = before_cursor
        .strip_suffix(typed)
        .ok_or_else(|| ExpandError::TriggerNotBeforeCursor(typed.to_owned()))?;
    let indented = with_indent(chunks, leading_whitespace(before_trigger));
    Ok(with_line_break(&indented, line_break_at(&buffer, &cursor)))
}

/// Deletes the `typed` chars before `cursor` and pushes `chunks` (prepared) there, in one user
/// action. Returns the snippet and marks around its text (left and right gravity).
fn push_prepared(
    view: &sourceview5::View,
    chunks: &[Chunk],
    typed: i32,
    cursor: &gtk::TextIter,
) -> (sourceview5::Snippet, gtk::TextMark, gtk::TextMark) {
    let mut start = *cursor;
    start.backward_chars(typed);
    push_replacing(view, chunks, start, *cursor)
}

/// Replaces `start..end` with a snippet of `chunks` (prepared) in one user action; see
/// [`push_prepared`].
fn push_replacing(
    view: &sourceview5::View,
    chunks: &[Chunk],
    mut start: gtk::TextIter,
    mut end: gtk::TextIter,
) -> (sourceview5::Snippet, gtk::TextMark, gtk::TextMark) {
    let buffer = view.buffer();
    let snippet = build_gsv_snippet(chunks);
    guard_history(view);
    buffer.begin_user_action();
    if start != end {
        buffer.delete(&mut start, &mut end);
    }
    let first = buffer.create_mark(None, &start, true);
    let last = buffer.create_mark(None, &start, false);
    view.push_snippet(&snippet, Some(&mut start));
    buffer.end_user_action();
    view.scroll_mark_onscreen(&buffer.get_insert());
    (snippet, first, last)
}

/// `chunks` with the `\n` of their text and defaults replaced by `line_break`.
fn with_line_break(chunks: &[Chunk], line_break: &str) -> Vec<Chunk> {
    if line_break == "\n" {
        return chunks.to_vec();
    }
    let convert = |s: &str| s.replace('\n', line_break);
    chunks
        .iter()
        .map(|chunk| match chunk {
            Chunk::Text(t) => Chunk::Text(convert(t)),
            Chunk::Stop { n, default } => Chunk::Stop {
                n: *n,
                default: convert(default),
            },
            Chunk::Mirror(n) => Chunk::Mirror(*n),
        })
        .collect()
}

/// The line break of the document at `at`: its line's own, else the first line's, else `\n`.
fn line_break_at(buffer: &gtk::TextBuffer, at: &gtk::TextIter) -> &'static str {
    let break_after = |line_end: gtk::TextIter| {
        let mut next = line_end;
        if !next.forward_line() && next == line_end {
            return None;
        }
        let delimiter = buffer.slice(&line_end, &next, true);
        Some(match delimiter.as_str() {
            "\r\n" => "\r\n",
            "\r" => "\r",
            _ => "\n",
        })
    };
    let mut line_end = *at;
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    break_after(line_end)
        .or_else(|| {
            let mut first = buffer.start_iter();
            if !first.ends_line() {
                first.forward_to_line_end();
            }
            break_after(first)
        })
        .unwrap_or("\n")
}

/// Ends every active snippet session of `view`: Tab no longer jumps between its stops and its
/// mirrors no longer follow their stop. The text is left as it is.
///
/// Undo and redo do this automatically for views that [`expand`] has used. Call it before
/// changing text inside an active snippet in other ways than typing (agent edits, review
/// actions, reloads): GtkSourceView answers such changes by rewriting mirrors, and those
/// rewrites bypass the undo history.
///
/// GtkSourceView 5.20 has no direct API for this; changing `enable-snippets` pops all of the
/// view's snippets, so the property is toggled and restored.
pub fn end_snippet_sessions(view: &impl IsA<sourceview5::View>) {
    let view = view.as_ref();
    let enabled = view.enables_snippets();
    view.set_enable_snippets(!enabled);
    view.set_enable_snippets(enabled);
}

/// A view and the buffer whose undo and redo [`guard_history`] has wired for it.
type Guarded = (
    glib::WeakRef<sourceview5::View>,
    glib::WeakRef<gtk::TextBuffer>,
);

thread_local! {
    static GUARDED: RefCell<Vec<Guarded>> = const { RefCell::new(Vec::new()) };
}

/// Makes `view` end its snippet sessions right before every undo and redo of its buffer.
///
/// An active session survives undo in GtkSourceView 5.20 and rewrites mirror chunks while the
/// history replays, outside the history: after typing into `\begin{$1}…\end{$1}`, one undo
/// left the mirror's text behind, and the next one deleted the wrong characters. Without a
/// session, undo and redo replay exactly what was recorded, mirror updates included.
fn guard_history(view: &sourceview5::View) {
    let buffer = view.buffer();
    let first_time = GUARDED.with_borrow_mut(|guarded| {
        guarded.retain(|(v, b)| v.upgrade().is_some() && b.upgrade().is_some());
        let known = guarded.iter().any(|(v, b)| {
            v.upgrade().as_ref() == Some(view) && b.upgrade().as_ref() == Some(&buffer)
        });
        if !known {
            guarded.push((view.downgrade(), buffer.downgrade()));
        }
        !known
    });
    if !first_time {
        return;
    }
    // `undo` and `redo` run their default handler last, so these run before the replay.
    let weak = view.downgrade();
    buffer.connect_undo(move |_| {
        if let Some(view) = weak.upgrade() {
            end_snippet_sessions(&view);
        }
    });
    let weak = view.downgrade();
    buffer.connect_redo(move |_| {
        if let Some(view) = weak.upgrade() {
            end_snippet_sessions(&view);
        }
    });
}

/// The leading spaces and tabs of `line`.
fn leading_whitespace(line: &str) -> &str {
    let rest = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - rest.len()]
}

/// The number of chars in `s`, as GTK counts iterator positions.
fn char_count(s: &str) -> i32 {
    i32::try_from(s.chars().count()).unwrap_or(i32::MAX)
}

// ───────────── The snippet driver ─────────────

/// Modifiers that make a key press a command rather than a typed character. Shift is part of
/// the character, and so is AltGr (GTK 4 reports its level-3 keyval, `{` on many layouts, with no
/// modifier bit); Caps Lock changes nothing.
const COMMAND_MODIFIERS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SUPER_MASK)
    .union(gdk::ModifierType::HYPER_MASK)
    .union(gdk::ModifierType::META_MASK);

/// The character a key press types, or `None` for keys that type nothing themselves: named
/// keys (Return, Tab, arrows, …), presses with Ctrl, Alt, Super, Hyper or Meta, dead keys and
/// the Compose key (the input method composes those; the composed character is committed by
/// the IM, not typed by this key).
pub fn typed_char(keyval: gdk::Key, state: gdk::ModifierType) -> Option<char> {
    if state.intersects(COMMAND_MODIFIERS) || is_compose_key(keyval) {
        return None;
    }
    keyval.to_unicode().filter(|c| !c.is_control())
}

/// Dead keys (`dead_acute`, …) and `Multi_key`.
fn is_compose_key(keyval: gdk::Key) -> bool {
    use glib::translate::IntoGlib;
    let raw = keyval.into_glib();
    // GDK_KEY_dead_grave ..= GDK_KEY_dead_greek (and the dead keys GDK added after it).
    (0xfe50..=0xfe93).contains(&raw) || keyval == gdk::Key::Multi_key
}

/// The best manual snippet whose trigger is exactly the text before the cursor (the longest
/// trigger, then the highest priority, then the earliest entry), in context `ctx`. Its
/// `trigger_len` covers the whole trigger.
pub fn exact_manual_match<'a>(
    set: &'a SnippetSet,
    line_before: &str,
    ctx: &Context,
) -> Option<SnippetMatch<'a>> {
    let mut best: Option<SnippetMatch<'a>> = None;
    for compiled in set.iter().filter(|c| !c.def.auto) {
        let trigger = compiled.def.trigger.as_str();
        if !line_before.ends_with(trigger) {
            continue;
        }
        // `manual` applies the position and context rules to the text before the trigger.
        let found = set
            .manual(trigger, line_before, ctx)
            .into_iter()
            .find(|m| std::ptr::eq(m.def, &compiled.def));
        let Some(m) = found else {
            continue;
        };
        let better = best.as_ref().is_none_or(|b| {
            (m.def.trigger.len(), m.def.priority) > (b.def.trigger.len(), b.def.priority)
        });
        if better {
            best = Some(m);
        }
    }
    best
}

/// The number of characters to move forward to step out past the next `}`, `)`, `]` or `$` in
/// `after` (the rest of the line after the cursor, inside inline math); `None` when there is
/// none. Characters escaped by a backslash (`\$`, `\}`) are no closers.
pub fn step_out_chars(after: &str) -> Option<i32> {
    step_out_chars_in(after, true)
}

/// [`step_out_chars`], where `in_inline_math` tells whether the cursor is inside inline math:
/// only there is a `$` the closing one. Elsewhere a `$` opens a formula, so nothing after it is
/// stepped out of (Tab in text before `$f$` must not jump into the formula).
pub fn step_out_chars_in(after: &str, in_inline_math: bool) -> Option<i32> {
    let mut escaped = false;
    for (i, c) in after.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '}' | ')' | ']' => return i32::try_from(after[..=i].chars().count()).ok(),
            '$' if in_inline_math => return i32::try_from(after[..=i].chars().count()).ok(),
            '$' => return None,
            _ => {}
        }
    }
    None
}

/// The line of `offset` split at it, in `LineIndex` lines (`\n` lines; a `\r` before the `\n`
/// is dropped from the rest): `(line, before, after)`.
fn split_line(doc: &Document, offset: usize) -> Option<(u32, String, String)> {
    let (line, col) = doc.line_index().position(offset);
    let mut text = doc.line_text(line)?;
    if text.ends_with('\r') {
        text.pop();
    }
    let col = (col as usize).min(text.len());
    if !text.is_char_boundary(col) {
        return None;
    }
    let after = text.split_off(col);
    Some((line, text, after))
}

/// What [`Driver::tab`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabAction {
    /// An exact manual trigger before the cursor was expanded.
    Expanded,
    /// A snippet is active: GtkSourceView moves to its next (or previous) stop.
    SnippetStop,
    /// The cursor stepped out past the next closing `}`, `)`, `]` or `$` on the line.
    SteppedOut,
    /// Nothing applied: the key goes on to Vim and the view (indent, or unindent for Shift+Tab).
    PassThrough,
}

impl TabAction {
    /// Whether the key is consumed (it never reaches Vim or the view).
    pub fn consumes_key(self) -> bool {
        matches!(self, Self::Expanded | Self::SteppedOut)
    }
}

/// The snippet driver of one view (see the module docs).
pub struct Driver {
    view: glib::WeakRef<TexView>,
    buffer: sourceview5::Buffer,
    doc: Weak<Document>,
    /// True between the outermost `begin-user-action` and `end-user-action` (Vim's insert mode
    /// keeps one open for the whole insert).
    user_action_open: Rc<Cell<bool>>,
    /// True while the input method shows preedit text (a compose sequence or an IM's
    /// conversion): the next key belongs to the IM.
    preedit: Rc<Cell<bool>>,
    /// Snippets this driver pushed; they are active while `Snippet::buffer` is set.
    pushed: Rc<RefCell<Vec<Pushed>>>,
    buffer_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    view_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    doc_observers: RefCell<Vec<ObserverId>>,
}

impl Driver {
    /// Starts driving the snippets of `view`, which shows `doc`.
    ///
    /// Turns on GtkSourceView's `enable-snippets` (it moves between stops on Tab and Shift+Tab)
    /// and empties the search path of its own snippet manager, so that Tab never expands one of
    /// GtkSourceView's bundled snippets: the snippets are Lemma's.
    pub fn new(view: &TexView, doc: &DocRef) -> Rc<Self> {
        disable_gsv_snippet_files();
        let buffer = view
            .buffer()
            .downcast::<sourceview5::Buffer>()
            .expect("a TexView shows a sourceview5::Buffer");
        view.set_enable_snippets(true);
        let driver = Rc::new(Self {
            view: view.downgrade(),
            buffer: buffer.clone(),
            doc: Rc::downgrade(doc),
            user_action_open: Rc::new(Cell::new(false)),
            preedit: Rc::new(Cell::new(false)),
            pushed: Rc::default(),
            buffer_handlers: RefCell::default(),
            view_handlers: RefCell::default(),
            doc_observers: RefCell::default(),
        });
        let open = driver.user_action_open.clone();
        let begin = buffer.connect_begin_user_action(move |_| open.set(true));
        let open = driver.user_action_open.clone();
        let end = buffer.connect_end_user_action(move |_| open.set(false));
        driver.buffer_handlers.replace(vec![begin, end]);
        let preedit = driver.preedit.clone();
        let handler = view.connect_preedit_changed(move |_, text| preedit.set(!text.is_empty()));
        driver.view_handlers.replace(vec![handler]);
        // Non-typing edits end the sessions first: GtkSourceView would rewrite mirrors outside
        // the undo history (STATUS "Snippets").
        let (weak_view, pushed) = (view.downgrade(), driver.pushed.clone());
        let observer = doc.connect_before_apply(move |_, _, _| {
            if any_active(&pushed)
                && let Some(view) = weak_view.upgrade()
            {
                end_snippet_sessions(&view);
                pushed.borrow_mut().clear();
            }
        });
        driver.doc_observers.replace(vec![observer]);
        driver
    }

    /// Disconnects everything [`Driver::new`] connected and ends the view's snippet sessions.
    /// The buffer can outlive the tab (a closed document's text).
    pub fn shutdown(&self) {
        for id in self.buffer_handlers.take() {
            self.buffer.disconnect(id);
        }
        if let Some(view) = self.view.upgrade() {
            for id in self.view_handlers.take() {
                view.disconnect(id);
            }
            if self.is_active() {
                end_snippet_sessions(&view);
            }
        }
        if let Some(doc) = self.doc.upgrade() {
            for id in self.doc_observers.take() {
                doc.disconnect(id);
            }
        }
        self.pushed.borrow_mut().clear();
    }

    /// True while a snippet this driver pushed is active (Tab moves between its stops).
    pub fn is_active(&self) -> bool {
        any_active(&self.pushed)
    }

    /// Ends the view's snippet sessions (see [`end_snippet_sessions`]) when one is active.
    pub fn end_sessions(&self) {
        if self.is_active()
            && let Some(view) = self.view.upgrade()
        {
            end_snippet_sessions(&view);
        }
        self.pushed.borrow_mut().clear();
    }

    /// True while the input method shows preedit text.
    pub fn preedit_active(&self) -> bool {
        self.preedit.get()
    }

    /// The autosnippet that typing `c` at the cursor would expand: the line up to the cursor
    /// plus `c` and the scanner context right after `c` go to [`SnippetSet::auto_match`].
    /// `None` when there is a selection, the view is not editable or in overwrite mode, an IM
    /// preedit is in progress, or the document has no scanner states yet (a large document
    /// still being scanned: its contexts are unknown).
    pub fn autosnippet<'a>(&self, set: &'a SnippetSet, c: char) -> Option<SnippetMatch<'a>> {
        let view = self.view.upgrade()?;
        let doc = self.doc.upgrade()?;
        if !view.is_editable()
            || view.overwrites()
            || self.preedit.get()
            || self.buffer.has_selection()
        {
            return None;
        }
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let offset = doc.byte_offset(&cursor);
        let (line, mut before, after) = split_line(&doc, offset)?;
        before.push(c);
        let ctx = {
            let states = doc.line_states.borrow();
            let states = states.as_ref()?;
            let typed_line = format!("{before}{after}");
            states.context_at(line, &typed_line, u32::try_from(before.len()).ok()?)
        };
        set.auto_match(&before, &ctx)
    }

    /// Types `c` at the cursor (as the text view would) and expands `m`, whose trigger now ends
    /// before the cursor, as an undo step of its own. `macro_defined` decides the
    /// `if_undefined` body ([`chunks_for`]). Returns false when nothing was expanded (the
    /// character is typed all the same).
    pub fn type_and_expand(
        &self,
        c: char,
        m: &SnippetMatch<'_>,
        macro_defined: impl FnOnce(&str) -> bool,
    ) -> bool {
        self.type_char(c) && self.expand_match(m, macro_defined)
    }

    /// Types `c` at the cursor and replaces `typed`, which then ends before the cursor, with the
    /// text `with`, as an undo step of its own: a typing replacement (`...` → `\dots`, see
    /// `typing::replacement`), which works like an autosnippet without stops. Returns false when
    /// the view is gone and nothing was typed; once `c` is typed the key is handled, replaced or
    /// not.
    pub fn type_and_replace(&self, c: char, typed: &str, with: &str) -> bool {
        if !self.type_char(c) {
            return false;
        }
        // As a parsed body would be: the text, then the exit.
        let chunks = [
            Chunk::Text(with.to_owned()),
            Chunk::Stop {
                n: 0,
                default: String::new(),
            },
        ];
        if let Err(e) = self.expand_chunks(&chunks, typed) {
            tracing::debug!("replacement not made: {e}");
        }
        true
    }

    /// Types `c` at the cursor as the text view would; false when the view is gone.
    fn type_char(&self, c: char) -> bool {
        let Some(view) = self.view.upgrade() else {
            return false;
        };
        let mut utf8 = [0u8; 4];
        let text = c.encode_utf8(&mut utf8);
        if self.user_action_open.get() {
            // Part of Vim's insert, as a typed character is.
            self.buffer
                .insert_interactive_at_cursor(text, view.is_editable());
        } else {
            self.buffer.begin_user_action();
            self.buffer
                .insert_interactive_at_cursor(text, view.is_editable());
            self.buffer.end_user_action();
        }
        true
    }

    /// Expands the match `m` (see [`expand`]) through [`Driver::expand_chunks`]; false when
    /// nothing was expanded.
    fn expand_match(&self, m: &SnippetMatch<'_>, macro_defined: impl FnOnce(&str) -> bool) -> bool {
        let Some(typed) = m.def.trigger.get(..m.trigger_len) else {
            return false;
        };
        match self.expand_chunks(chunks_for(m, macro_defined), typed) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("snippet not expanded: {e}");
                false
            }
        }
    }

    /// Tab (`back` = false) or Shift+Tab (`back` = true) in insert mode; see [`TabAction`].
    /// Manual snippets come from `set`; `macro_defined` decides `if_undefined` bodies.
    pub fn tab(
        &self,
        back: bool,
        set: &SnippetSet,
        macro_defined: impl FnOnce(&str) -> bool,
    ) -> TabAction {
        let (Some(view), Some(doc)) = (self.view.upgrade(), self.doc.upgrade()) else {
            return TabAction::PassThrough;
        };
        let editable = view.is_editable() && !self.preedit.get();
        let selection = self.buffer.has_selection();
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let split = split_line(&doc, doc.byte_offset(&cursor));
        let ctx = split.as_ref().map(|(line, before, after)| {
            let states = doc.line_states.borrow();
            states.as_ref().map_or(Context::Text, |s| {
                s.context_at(*line, &format!("{before}{after}"), before.len() as u32)
            })
        });
        // 1. An exact manual trigger (even while the completion popup is open).
        if !back
            && editable
            && !selection
            && let (Some((_, before, _)), Some(ctx)) = (&split, &ctx)
            && let Some(m) = exact_manual_match(set, before, ctx)
            && self.expand_match(&m, macro_defined)
        {
            return TabAction::Expanded;
        }
        // 2. The next (previous) stop of the active snippet: GtkSourceView's own Tab handling.
        if self.is_active() {
            return TabAction::SnippetStop;
        }
        // 3. Out past the next closing delimiter on the line.
        let in_inline_math = matches!(ctx, Some(Context::Math { display: false, .. }));
        if !back
            && !selection
            && let Some((_, before, after)) = &split
            && let Some(n) = {
                // A backslash right before the cursor escapes the first character after it.
                let escaped = before.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1;
                match after.chars().next() {
                    Some(first) if escaped => {
                        step_out_chars_in(&after[first.len_utf8()..], in_inline_math).map(|n| n + 1)
                    }
                    _ => step_out_chars_in(after, in_inline_math),
                }
            }
        {
            let mut target = cursor;
            target.forward_chars(n);
            self.buffer.place_cursor(&target);
            view.scroll_mark_onscreen(&self.buffer.get_insert());
            return TabAction::SteppedOut;
        }
        // 4. Indentation (or unindentation) by Vim and the view.
        TabAction::PassThrough
    }

    /// [`expand_chunks`] as an undo step of its own, tracking the snippet (autosnippets, manual
    /// snippets, completion items), with two differences:
    ///
    /// - A body without numbered stops (`\to`, `\item $0`) is inserted as text, with the cursor
    ///   at its `$0`: an active snippet keeps its session (Tab still reaches its next stop).
    /// - Inside an active snippet, the expansion and the active snippet's remaining stops become
    ///   one snippet ([`flattened`]): Tab visits the new stops, then the remaining ones. Nested
    ///   GtkSourceView sessions lose track of the outer snippet's text (typing in its next stop
    ///   replaced the nested expansion). When that is not possible the active session ends first.
    pub fn expand_chunks(&self, chunks: &[Chunk], typed: &str) -> Result<(), ExpandError> {
        let view = self.view.upgrade().ok_or(ExpandError::NotEditable)?;
        let mut prepared = prepare_chunks(view.upcast_ref(), chunks, typed)?;
        let typed_chars = char_count(typed);
        self.as_undo_step(|| {
            let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
            if !has_numbered_stops(&prepared) {
                // GtkSourceView 5.20 ends a session when a deletion touches the start of its
                // focused stop: a trigger there continues in a new snippet (below) whose first
                // stop is the exit.
                let at_stop_start = {
                    let pushed = self.pushed.borrow();
                    pushed
                        .iter()
                        .rev()
                        .find(|p| p.is_active())
                        .is_some_and(|outer| {
                            focused_chunk(&self.buffer, outer, cursor.offset(), typed_chars)
                                .is_some_and(|f| f.start == cursor.offset() - typed_chars)
                        })
                };
                if !at_stop_start {
                    insert_plain(&view, &prepared, typed_chars, &cursor);
                    return;
                }
                for chunk in &mut prepared {
                    if let Chunk::Stop { n: n @ 0, .. } = chunk {
                        *n = 1;
                    }
                }
            }
            let flat = {
                let pushed = self.pushed.borrow();
                pushed
                    .iter()
                    .rev()
                    .find(|p| p.is_active())
                    .and_then(|outer| {
                        flattened(&self.buffer, outer, &prepared, cursor.offset(), typed_chars)
                    })
            };
            if self.is_active() {
                end_snippet_sessions(&view);
            }
            self.pushed.borrow_mut().clear();
            let (chunks, start, end) = match flat {
                Some((chunks, end)) => {
                    let mut start = cursor;
                    start.backward_chars(typed_chars);
                    (chunks, start, self.buffer.iter_at_offset(end))
                }
                None => {
                    let mut start = cursor;
                    start.backward_chars(typed_chars);
                    (prepared, start, cursor)
                }
            };
            let (snippet, first, last) = push_replacing(view.upcast_ref(), &chunks, start, end);
            self.remember(snippet, chunks, first, last);
        });
        Ok(())
    }

    /// Runs `f` (which makes its own user action) so that it is an undo step of its own: the
    /// user action Vim's insert mode keeps open is closed first and opened again afterwards
    /// (as `Document::apply_edits` does for agent edits).
    pub fn as_undo_step<R>(&self, f: impl FnOnce() -> R) -> R {
        let split = self.user_action_open.get();
        if split {
            self.buffer.end_user_action();
        }
        let result = f();
        if split {
            self.buffer.begin_user_action();
        }
        result
    }

    /// Remembers a pushed snippet while it is active (`chunks`: what it was built from; `start`,
    /// `end`: marks around its text).
    fn remember(
        &self,
        snippet: sourceview5::Snippet,
        chunks: Vec<Chunk>,
        start: gtk::TextMark,
        end: gtk::TextMark,
    ) {
        let record = Pushed {
            snippet: snippet.downgrade(),
            chunks,
            start,
            end,
        };
        let mut pushed = self.pushed.borrow_mut();
        pushed.retain(Pushed::is_active);
        if record.is_active() {
            pushed.push(record);
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A snippet the driver pushed, with marks around its text: GtkSourceView does not say where its
/// chunks are, only their texts.
struct Pushed {
    snippet: glib::WeakRef<sourceview5::Snippet>,
    /// The chunks it was built from (prepared: indented, with the document's line breaks).
    chunks: Vec<Chunk>,
    /// Left gravity, at its first character.
    start: gtk::TextMark,
    /// Right gravity, after its last character.
    end: gtk::TextMark,
}

impl Pushed {
    /// True while the snippet is active (GtkSourceView clears a snippet's buffer when its session
    /// ends: after its last stop, when the cursor leaves it, or when sessions are ended).
    fn is_active(&self) -> bool {
        self.snippet.upgrade().is_some_and(|s| s.buffer().is_some())
    }
}

impl Drop for Pushed {
    fn drop(&mut self) {
        for mark in [&self.start, &self.end] {
            if !mark.is_deleted()
                && let Some(buffer) = mark.buffer()
            {
                buffer.delete_mark(mark);
            }
        }
    }
}

/// True while one of `pushed` is still active.
fn any_active(pushed: &RefCell<Vec<Pushed>>) -> bool {
    pushed.borrow().iter().any(Pushed::is_active)
}

/// Whether `chunks` have a tab stop other than `$0`.
fn has_numbered_stops(chunks: &[Chunk]) -> bool {
    chunks
        .iter()
        .any(|c| matches!(c, Chunk::Stop { n, .. } if *n > 0) || matches!(c, Chunk::Mirror(_)))
}

/// Replaces the `typed` chars before `cursor` with the text of `chunks` (which have no numbered
/// stops) in one user action, as typing would, and puts the cursor at `$0`.
///
/// The text is inserted after the trigger before the trigger is deleted: GtkSourceView 5.20 ends
/// an active session when a deletion reaches the end of its focused stop (a deletion inside it
/// keeps the session).
fn insert_plain(view: &TexView, chunks: &[Chunk], typed: i32, cursor: &gtk::TextIter) {
    let buffer = view.buffer();
    let (mut before, mut after) = (String::new(), String::new());
    let mut seen_exit = false;
    for chunk in chunks {
        let text = match chunk {
            Chunk::Text(t) => t.as_str(),
            Chunk::Stop { default, .. } => {
                seen_exit = true;
                default.as_str()
            }
            Chunk::Mirror(_) => "",
        };
        if seen_exit {
            after.push_str(text);
        } else {
            before.push_str(text);
        }
    }
    let at = cursor.offset();
    buffer.begin_user_action();
    let mut insert = *cursor;
    buffer.insert(&mut insert, &before);
    let exit = buffer.create_mark(None, &insert, true);
    buffer.insert(&mut insert, &after);
    if typed > 0 {
        let mut start = buffer.iter_at_offset(at - typed);
        let mut end = buffer.iter_at_offset(at);
        buffer.delete(&mut start, &mut end);
    }
    buffer.end_user_action();
    buffer.place_cursor(&buffer.iter_at_mark(&exit));
    buffer.delete_mark(&exit);
    view.scroll_mark_onscreen(&buffer.get_insert());
}

/// The focused stop of the active snippet `outer`, holding the `typed` chars before the cursor
/// (at char offset `cursor`).
struct Focused {
    /// GtkSourceView's text of each of `outer`'s chunks.
    texts: Vec<String>,
    /// The focus position.
    focus: i32,
    /// The index of the focused chunk.
    index: usize,
    /// Char offsets of the focused chunk's start and of `outer`'s end.
    start: i32,
    end: i32,
}

/// See [`Focused`]. `None` when `outer`'s chunk texts (as GtkSourceView keeps them) are not the
/// text between its marks, or the trigger is not inside its focused stop.
fn focused_chunk(
    buffer: &sourceview5::Buffer,
    outer: &Pushed,
    cursor: i32,
    typed: i32,
) -> Option<Focused> {
    let snippet = outer.snippet.upgrade()?;
    let n = usize::try_from(snippet.n_chunks()).ok()?;
    if n != outer.chunks.len() || outer.start.is_deleted() || outer.end.is_deleted() {
        return None;
    }
    let texts: Vec<String> = (0..snippet.n_chunks())
        .map(|i| snippet.nth_chunk(i).text().to_string())
        .collect();
    let start = buffer.iter_at_mark(&outer.start);
    let end = buffer.iter_at_mark(&outer.end);
    if buffer.slice(&start, &end, true) != texts.concat() {
        return None;
    }
    let focus = snippet.focus_position();
    let mut at = start.offset();
    for (index, text) in texts.iter().enumerate() {
        let len = char_count(text);
        if stop_number(&outer.chunks[index]) == Some(focus)
            && at <= cursor - typed
            && cursor <= at + len
        {
            return Some(Focused {
                texts,
                focus,
                index,
                start: at,
                end: end.offset(),
            });
        }
        at += len;
    }
    None
}

/// The number of a [`Chunk::Stop`].
fn stop_number(chunk: &Chunk) -> Option<i32> {
    match chunk {
        Chunk::Stop { n, .. } => Some(i32::from(*n)),
        _ => None,
    }
}

/// The snippet that replaces the `typed` chars before the cursor (at char offset `cursor`) inside
/// the active snippet `outer`: `inner` (prepared), then the rest of `outer` after the cursor, in
/// which the stops still to visit stay stops (renumbered after `inner`'s) and `outer`'s `$0`
/// stays the exit. Returns the chunks and the char offset where `outer`'s text ends (the text
/// they replace starts at the trigger).
///
/// `None` when `outer`'s chunk texts (as GtkSourceView keeps them) are not the text between its
/// marks, the cursor is not in its focused stop, or a stop still to visit lies before it.
fn flattened(
    buffer: &sourceview5::Buffer,
    outer: &Pushed,
    inner: &[Chunk],
    cursor: i32,
    typed: i32,
) -> Option<(Vec<Chunk>, i32)> {
    let Focused {
        texts,
        focus,
        index: f,
        start,
        end,
    } = focused_chunk(buffer, outer, cursor, typed)?;
    let cursor_in_chunk = cursor - start;
    let stop_of = stop_number;
    if focus <= 0
        || outer.chunks[..f]
            .iter()
            .any(|c| stop_of(c).is_some_and(|n| n > focus))
    {
        return None;
    }
    let inner_max = inner.iter().filter_map(stop_of).max().unwrap_or(0);
    let later: std::collections::BTreeSet<i32> = outer.chunks[f + 1..]
        .iter()
        .filter_map(stop_of)
        .filter(|n| *n > focus)
        .collect();
    let renumber = |n: i32| -> Option<u8> {
        let index = later.iter().position(|m| *m == n)?;
        u8::try_from(inner_max + 1 + i32::try_from(index).ok()?).ok()
    };
    let outer_exit = outer.chunks[f + 1..].iter().any(|c| stop_of(c) == Some(0));
    let mut out: Vec<Chunk> = Vec::new();
    let push_text = |out: &mut Vec<Chunk>, text: &str| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(Chunk::Text(t)) => t.push_str(text),
            _ => out.push(Chunk::Text(text.to_owned())),
        }
    };
    for chunk in inner {
        match chunk {
            // The outer snippet's exit is the exit.
            Chunk::Stop { n: 0, .. } if outer_exit => {}
            Chunk::Text(t) => push_text(&mut out, t),
            other => out.push(other.clone()),
        }
    }
    let rest: String = texts[f]
        .chars()
        .skip(usize::try_from(cursor_in_chunk).ok()?)
        .collect();
    push_text(&mut out, &rest);
    for (chunk, text) in outer.chunks.iter().zip(&texts).skip(f + 1) {
        match chunk {
            Chunk::Stop { n: 0, .. } => out.push(Chunk::Stop {
                n: 0,
                default: text.clone(),
            }),
            Chunk::Stop { n, .. } if i32::from(*n) > focus => out.push(Chunk::Stop {
                n: renumber(i32::from(*n))?,
                default: text.clone(),
            }),
            Chunk::Mirror(n) if i32::from(*n) > focus => {
                out.push(Chunk::Mirror(renumber(i32::from(*n))?));
            }
            // Text, and stops and mirrors already visited.
            _ => push_text(&mut out, text),
        }
    }
    Some((out, end))
}

/// Empties the search path of GtkSourceView's snippet manager, once: with `enable-snippets` on,
/// Tab would otherwise expand GtkSourceView's own bundled snippets (license headers and the
/// like) on systems that ship them.
fn disable_gsv_snippet_files() {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if !DONE.replace(true) {
        sourceview5::SnippetManager::default().set_search_path(&[] as &[&str]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_snippets::SnippetSet;

    #[test]
    fn leading_whitespace_is_spaces_and_tabs() {
        assert_eq!(leading_whitespace("    thm"), "    ");
        assert_eq!(leading_whitespace("\t  x ff"), "\t  ");
        assert_eq!(leading_whitespace("x  "), "");
        assert_eq!(leading_whitespace("   "), "   ");
        assert_eq!(leading_whitespace(""), "");
        assert_eq!(leading_whitespace("\u{a0}x"), "");
    }

    fn whole_trigger_match<'a>(set: &'a SnippetSet, trigger: &str) -> SnippetMatch<'a> {
        let c = set
            .iter()
            .find(|c| c.def.trigger == trigger)
            .unwrap_or_else(|| panic!("no default {trigger:?}"));
        SnippetMatch {
            def: &c.def,
            chunks: &c.chunks,
            trigger_len: c.def.trigger.len(),
            alt_chunks: c.alt.as_deref(),
        }
    }

    #[test]
    fn chunks_for_picks_the_alternative_only_for_undefined_macros() {
        let set = SnippetSet::defaults();
        let m = whole_trigger_match(&set, "NN");
        let alt = m.alt_chunks.expect("NN has an alternative body");

        let mut asked = Vec::new();
        let chosen = chunks_for(&m, |name| {
            asked.push(name.to_owned());
            false
        });
        assert_eq!(chosen, alt);
        assert_eq!(asked, ["N"]);

        assert_eq!(chunks_for(&m, |_| true), m.chunks);
    }

    #[test]
    fn typed_characters() {
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        assert_eq!(typed_char(gdk::Key::f, none), Some('f'));
        assert_eq!(typed_char(gdk::Key::F, shift), Some('F'));
        assert_eq!(typed_char(gdk::Key::underscore, shift), Some('_'));
        assert_eq!(typed_char(gdk::Key::backslash, none), Some('\\'));
        assert_eq!(typed_char(gdk::Key::KP_1, none), Some('1'));
        // Caps Lock changes nothing.
        assert_eq!(
            typed_char(gdk::Key::A, gdk::ModifierType::LOCK_MASK),
            Some('A')
        );
        // Commands, named keys and compose keys type nothing themselves.
        assert_eq!(
            typed_char(gdk::Key::v, gdk::ModifierType::CONTROL_MASK),
            None
        );
        assert_eq!(typed_char(gdk::Key::f, gdk::ModifierType::ALT_MASK), None);
        assert_eq!(typed_char(gdk::Key::f, gdk::ModifierType::SUPER_MASK), None);
        for key in [
            gdk::Key::Return,
            gdk::Key::Tab,
            gdk::Key::BackSpace,
            gdk::Key::Escape,
            gdk::Key::Left,
            gdk::Key::dead_acute,
            gdk::Key::dead_circumflex,
            gdk::Key::Multi_key,
            gdk::Key::Shift_L,
        ] {
            assert_eq!(typed_char(key, none), None, "{key:?}");
        }
    }

    #[test]
    fn stepping_out() {
        assert_eq!(step_out_chars("}{b}$"), Some(1));
        assert_eq!(step_out_chars(" + y)^2"), Some(5));
        assert_eq!(step_out_chars("x$ and"), Some(2));
        assert_eq!(step_out_chars("é]"), Some(2));
        assert_eq!(step_out_chars("no closer"), None);
        assert_eq!(step_out_chars(""), None);
        // Escaped characters are no closers.
        assert_eq!(step_out_chars(r"50\$ off}"), Some(9));
        assert_eq!(step_out_chars(r"a\} b}"), Some(6));
        assert_eq!(step_out_chars(r"\\}"), Some(3));
        // Outside inline math a `$` opens a formula: nothing after it.
        assert_eq!(step_out_chars_in("$f$ is}", false), None);
        assert_eq!(step_out_chars_in("x} $f$", false), Some(2));
        assert_eq!(step_out_chars_in("$f$ is}", true), Some(1));
    }

    #[test]
    fn exact_manual_triggers() {
        let set = SnippetSet::defaults();
        let text = Context::Text;
        let m = exact_manual_match(&set, "  thm", &text).expect("thm at the line start");
        assert_eq!(m.def.trigger, "thm");
        assert_eq!(m.trigger_len, 3);
        assert!(exact_manual_match(&set, "x thm", &text).is_none());
        assert!(
            exact_manual_match(&set, "  th", &text).is_none(),
            "exact only"
        );
        // The longest trigger wins: `eqs`, not `eq` + `s`.
        assert_eq!(
            exact_manual_match(&set, "eqs", &text).map(|m| m.def.trigger.as_str()),
            Some("eqs")
        );
        // Section snippets are not line-begin snippets.
        assert_eq!(
            exact_manual_match(&set, "see sec", &text).map(|m| m.def.trigger.as_str()),
            Some("sec")
        );
        assert!(
            exact_manual_match(&set, "insec", &text).is_none(),
            "word trigger"
        );
        // Autosnippets are not manual.
        let math = Context::Math {
            display: false,
            env: None,
        };
        assert!(exact_manual_match(&set, "x ff", &math).is_none());
        assert!(exact_manual_match(&set, "  thm", &Context::Comment).is_none());
    }

    #[test]
    fn chunks_for_never_asks_without_an_alternative() {
        let set = SnippetSet::defaults();
        let m = whole_trigger_match(&set, "ff");
        let chosen = chunks_for(&m, |name| panic!("asked about {name:?}"));
        assert_eq!(chosen, m.chunks);
    }
}
