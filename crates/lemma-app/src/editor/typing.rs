//! Typing helpers (WP2.9b; ARCHITECTURE §5 "Pairing").
//!
//! [`attach`] (after `completion::attach`) registers one key-layer handler per editor at
//! [`keys::PRIORITY_TYPING`], below the snippet driver (an autosnippet such as `lr(` wins over
//! pairing). The helpers act on **key presses** in Vim insert mode and in plain editing only, so
//! they never fire on pastes, IM commits, undo/redo, or agent, review and reload edits (none of
//! those is a key press of the character; see `snippets::typed_char`). They are
//! also off in comments and verbatim text, with a selection, in overwrite mode, during an IM
//! preedit, while a large document's scanner states are still being built, and whenever a
//! `[typing]` key switches them off (read at use time, so config reloads apply at once).
//!
//! | Key | `[typing]` key | What it does |
//! |---|---|---|
//! | `{` `[` `(` | `autopair` | inserts the closing character too; not after a `\` (`\{` in text, `\[`, `\(`), not before a letter, digit or `\`, not when the paragraph already has an unmatched closer of that kind |
//! | `{` after `\` in math | `autopair` | `\{` → `\{\}`; `\left\{` → `\left\{\right\}` |
//! | `(` `[` `\|` after `\left` in math | `autopair` | `\left(` → `\left(\right)`, and the same for `[` and `\|` |
//! | `}` `]` `)` | `autopair` | types over the same closing character when this helper inserted it, or when the paragraph has no unmatched opener of that kind (else the closer is new); in math also over `\right)`; `\}` types over a paired `\}`; in math, `)` typed before a `]` this helper inserted (and `]` before its `)`) replaces it, for half-open intervals `[0,1)` |
//! | `$` | `smart_dollar` | in text: `$$` with the cursor between (not after `\`, not before a letter, digit, `\` or `$`, not right after a `$`); in math: types over a closing `$` |
//! | Backspace | `autopair` / `smart_dollar` | deletes an empty pair (`{}`, `[]`, `()`, `\{\}`, `\left(\right)`, `$$`) as one step: the opening character typed and its partner, when that partner is one this helper inserted or the pair is balanced in its paragraph (an opener typed without its partner takes only itself) |
//! | `}` completing `\begin{x}` | `auto_end` | inserts `\end{x}` on the next line when a forward scan finds the environment unclosed; the cursor stays after `\begin{x}`, for its arguments |
//! | Enter after `\begin{x}…` | `auto_end` | an indented body line (the `document` body is not indented), and `\end{x}` below it when the environment is unclosed |
//! | Enter in itemize, enumerate, description | `item_on_enter` | a new line with `\item ` (at the indentation of the item above); right after `\begin{itemize}` too |
//! | Enter on an empty `\item` line | `item_on_enter` | removes the line and leaves the list: the cursor goes to a new line after `\end{itemize}` when only blank lines are left in the list, else the item line is just emptied |
//! | `.` completing `...` | `replacements` (off by default) | in text, the three dots become `\dots`; not in math or arguments, not after a backslash (`\.` is the dot accent), not after a fourth dot |
//!
//! Shift+Enter always inserts a plain line break. While the completion popup is open, Enter
//! belongs to it (it may accept a proposal): the helper lets the key through and, if the view
//! then inserted a plain line break, replaces it with its own result (in the same Vim insert;
//! a separate undo step in plain editing).
//!
//! Every helper edits the buffer directly, as typing does (never `Document::apply_edits`, which
//! would end an active snippet session), in one user action, with `Origin::User`: one undo step
//! in plain editing, part of the insert in Vim. Pairs typed in a snippet's stop keep the
//! snippet (Tab still moves on); an edit that is not typing at the cursor (Backspace on a pair,
//! leaving a list, `\end` after a typed-over `}`) ends an active snippet session first, because
//! GtkSourceView 5.20 rewrites mirror chunks during such edits. As in Vim with `<Left>` in insert
//! mode, Vim's `.` after an insert repeats only what was typed after the last helper moved the
//! cursor (probed on real keys: `a{b<Esc>.` adds `b`).
//!
//! A replacement works like an autosnippet instead (`completion::type_and_replace`, through the
//! snippet driver): the typed `.` goes in first, then the replacement as an undo step of its
//! own, so one undo gives back `...` (in Vim, `u` after `<Esc>`), and an active snippet keeps
//! its session.
//!
//! **Counting brackets.** A group or a formula can span lines but not paragraphs (`\par` ends
//! inline math and the arguments of ordinary commands), so whether a closer is new, or an opener
//! already has its closer, is decided by the bracket count of the cursor's paragraph (blank lines
//! end it; at most [`MAX_PARAGRAPH_LINES`] lines each way), comments and escaped characters
//! skipped. A closer this helper inserted together with its opener (tracked by a text mark) is
//! always typed over and deleted with its opener.
//!
//! **Cost.** A key reads only the lines it needs: the cursor's, and its paragraph's when a rule
//! counts brackets. Enter and `\end` look at the environment commands of the other lines through
//! `EnvIndex`, which keeps them parsed (and which lines have none) in step with every change,
//! so these keys cost about the same in a 500-line and in a 60k-line document.
//!
//! The rules themselves are pure functions ([`on_char`], [`replacement`], [`on_backspace`],
//! [`on_enter`], [`on_enter_lines`]) over the text ([`TextLines`]), tested in this module;
//! [`detach`] removes the handler when the tab closes.

use super::{Editor, TexView, VimMode, completion, keys, snippets};
use crate::document::Document;
use crate::window::WindowState;
use gtk::gdk;
use gtk::prelude::*;
use lemma_config::TypingCfg;
use lemma_core::LineIndex;
use lemma_syntax::Context;
use sourceview5::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::ops::{ControlFlow, Range};
use std::rc::{Rc, Weak};

/// How many lines that may hold `\begin`, `\end` or `\item` Enter reads, at most, looking back for
/// the environment around the cursor (lines without them are skipped unread).
const MAX_SCAN_BACK_LINES: usize = 3000;
/// How far (in lines, each way) the bracket counts look for the end of the cursor's paragraph.
pub const MAX_PARAGRAPH_LINES: u32 = 100;
/// Closers this helper remembers per editor (typed over and deleted with their opener; the
/// interval rule).
const MAX_TRACKED_CLOSERS: usize = 16;
/// Lines of unknown content (see [`EnvIndex`]) above which the next scan classifies every line
/// from one copy of the text instead of line by line.
const ENV_FILL_AT: usize = 256;

// ───────────── The rules (pure) ─────────────

/// The kind of text at the cursor, as the helpers see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Text, or text inside math (`\text{…}`).
    Text,
    /// Math.
    Math,
    /// The argument of a reference, citation, label, file, package, environment or key-value
    /// command.
    Arg,
    /// A comment or verbatim text: no helper applies.
    Off,
}

impl Mode {
    /// The mode for `ctx`; inside a command name, `at_command(start)` gives the context at its
    /// backslash (`start` is the backslash's byte in the line), asked again while that is a
    /// command name too.
    pub fn of(ctx: &Context, at_command: impl Fn(u32) -> Context) -> Self {
        let mut ctx = ctx.clone();
        let mut last = u32::MAX;
        // Right after `\left`, the backslash of `\left\` is itself inside a command name.
        while let Context::CommandName { start } = ctx {
            if start >= last {
                break;
            }
            last = start;
            ctx = at_command(start);
        }
        Self::plain(&ctx)
    }

    /// The mode for `ctx`, a command name counting as text.
    fn plain(ctx: &Context) -> Self {
        match ctx {
            Context::Text | Context::TextInMath | Context::CommandName { .. } => Mode::Text,
            Context::Math { .. } => Mode::Math,
            Context::Arg { .. } => Mode::Arg,
            Context::Comment | Context::Verbatim => Mode::Off,
        }
    }
}

/// A helper's result: edits of the text before the key, then the cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Non-overlapping replacements in ascending order, in byte offsets of the text before the
    /// key (relative to the line start for [`on_char`] and [`on_backspace`]).
    pub edits: Vec<(Range<usize>, String)>,
    /// The cursor afterwards, in the same coordinates but in the text after the edits.
    pub cursor: usize,
    /// True when the character right after the new cursor is a closer this plan inserted.
    pub closer: bool,
}

impl Plan {
    fn insert(at: usize, text: impl Into<String>, cursor: usize) -> Self {
        Self {
            edits: vec![(at..at, text.into())],
            cursor,
            closer: false,
        }
    }

    fn moved(cursor: usize) -> Self {
        Self {
            edits: Vec::new(),
            cursor,
            closer: false,
        }
    }

    /// Applies the edits to `text` (for tests): the new text.
    pub fn apply_to(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (range, with) in self.edits.iter().rev() {
            out.replace_range(range.clone(), with);
        }
        out
    }
}

/// The cursor's line as the character rules see it.
#[derive(Clone, Copy, Debug)]
pub struct Typed<'a> {
    /// The line before the cursor.
    pub before: &'a str,
    /// The line after the cursor, without its line break.
    pub after: &'a str,
    /// The mode at the cursor.
    pub mode: Mode,
    /// The character after the cursor is a closer that this helper inserted (and that has not
    /// been typed over since).
    pub next_auto: bool,
    /// The other lines of the cursor's paragraph, for the bracket counts.
    pub paragraph: &'a Paragraph<'a>,
}

/// The lines of the cursor's paragraph other than its own (see the module docs, "Counting
/// brackets"), read only when a rule counts brackets.
pub struct Paragraph<'a> {
    read: Box<dyn Fn() -> String + 'a>,
    lines: OnceCell<String>,
}

impl<'a> Paragraph<'a> {
    /// The lines `read` returns (`\n`-separated), read on first use.
    pub fn new(read: impl Fn() -> String + 'a) -> Self {
        Self {
            read: Box::new(read),
            lines: OnceCell::new(),
        }
    }

    /// No other lines: the paragraph is the cursor's line.
    pub fn none() -> Self {
        Self::new(String::new)
    }

    /// The other lines of the paragraph around line `line` of `lines`: up to the blank lines
    /// before and after it, at most [`MAX_PARAGRAPH_LINES`] each way.
    pub fn around(lines: &'a dyn TextLines, line: u32) -> Self {
        Self::new(move || paragraph_lines(lines, line))
    }

    fn text(&self) -> &str {
        self.lines.get_or_init(|| (self.read)())
    }

    /// `count` of the cursor's line (`before` + `after`) plus `count` of each other line.
    fn count(&self, at: &Typed<'_>, count: impl Fn(&str) -> i32) -> i32 {
        let line = format!("{}{}", at.before, at.after);
        count(&line) + self.text().split('\n').map(count).sum::<i32>()
    }
}

impl std::fmt::Debug for Paragraph<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paragraph")
            .field("lines", &self.lines.get())
            .finish_non_exhaustive()
    }
}

/// The lines of the paragraph around line `line` (not that line itself), `\n`-separated.
fn paragraph_lines(lines: &dyn TextLines, line: u32) -> String {
    let blank = |text: &str| text.trim().is_empty();
    let mut out = String::new();
    let mut l = line;
    for _ in 0..MAX_PARAGRAPH_LINES {
        let Some(prev) = l.checked_sub(1) else {
            break;
        };
        l = prev;
        match lines.line(l) {
            Some(text) if !blank(&text) => {
                out.push_str(&text);
                out.push('\n');
            }
            _ => break,
        }
    }
    let count = lines.line_count();
    let mut l = line;
    for _ in 0..MAX_PARAGRAPH_LINES {
        l += 1;
        if l >= count {
            break;
        }
        match lines.line(l) {
            Some(text) if !blank(&text) => {
                out.push_str(&text);
                out.push('\n');
            }
            _ => break,
        }
    }
    out
}

/// What typing `c` does on the line `at` (offsets relative to the line start; the cursor is at
/// `at.before.len()`). `unclosed(name, from)` tells whether the environment `name`, whose
/// `\begin{name}` ends at byte `from` of the line (in the text before the key), is unclosed
/// further on; `nl` is the line break to insert. `None` lets the key through.
pub fn on_char(
    c: char,
    at: &Typed<'_>,
    cfg: &TypingCfg,
    unclosed: &mut dyn FnMut(&str, usize) -> bool,
    nl: &str,
) -> Option<Plan> {
    if at.mode == Mode::Off {
        return None;
    }
    let cursor = at.before.len();
    match c {
        '{' | '[' | '(' | '|' => cfg.autopair.then(|| opener(c, at)).flatten(),
        '}' | ']' | ')' => {
            let typed = if cfg.autopair { closer(c, at) } else { None };
            if c == '}' && cfg.auto_end {
                // The `}` as typed (or typed over), then `\end{x}` when it completes `\begin{x}`.
                let plan = typed
                    .clone()
                    .unwrap_or_else(|| Plan::insert(cursor, "}", cursor + 1));
                if let Some(plan) = auto_end(plan, at, unclosed, nl) {
                    return Some(plan);
                }
            }
            typed
        }
        '$' => cfg.smart_dollar.then(|| dollar(at)).flatten(),
        _ => None,
    }
}

/// The typing replacement (`[typing] replacements`) that typing `c` on the line `at` completes:
/// the trigger as it then ends before the cursor (`c` included) and its replacement. Only
/// `...` → `\dots`, in text: never in math, arguments, comments or verbatim text, after a
/// backslash (`\.` is the dot accent) or after a fourth dot (one undo gives back `...`, and
/// typing on must not replace again).
pub fn replacement(
    c: char,
    at: &Typed<'_>,
    cfg: &TypingCfg,
) -> Option<(&'static str, &'static str)> {
    if !cfg.replacements || c != '.' || at.mode != Mode::Text {
        return None;
    }
    let head = at.before.strip_suffix("..")?;
    if head.ends_with('.') || ends_with_unescaped_backslash(head) {
        return None;
    }
    Some(("...", "\\dots"))
}

/// `{`, `[`, `(` and `|` (only after `\left`).
fn opener(c: char, at: &Typed<'_>) -> Option<Plan> {
    let cursor = at.before.len();
    let math = at.mode == Mode::Math;
    let close = match c {
        '{' => '}',
        '[' => ']',
        '(' => ')',
        _ => '|',
    };
    let blocked_by_next = at
        .after
        .chars()
        .next()
        .is_some_and(|n| n.is_alphanumeric() || n == '\\');
    // `\left(` → `\right)`, `\left[`, `\left|`, and `\left\{` (below).
    if math && c != '{' && ends_with_command(at.before, "left") {
        if blocked_by_next {
            return None;
        }
        return Some(Plan::insert(
            cursor,
            format!("{c}\\right{close}"),
            cursor + 1,
        ));
    }
    if c == '|' {
        // `|` types over the `\right|` of a pair.
        return (math && at.after.starts_with("\\right|"))
            .then(|| Plan::moved(cursor + "\\right|".len()));
    }
    if ends_with_unescaped_backslash(at.before) {
        // `\{` pairs in math only; `\[` and `\(` start math and are left alone.
        if c != '{' || !math || blocked_by_next {
            return None;
        }
        let left = at
            .before
            .strip_suffix('\\')
            .is_some_and(|b| ends_with_command(b, "left"));
        let closing = if left { "\\right\\}" } else { "\\}" };
        return Some(Plan::insert(cursor, format!("{{{closing}"), cursor + 1));
    }
    if blocked_by_next {
        return None;
    }
    if at.paragraph.count(at, |l| balance(l, c, close)) < 0 {
        // The paragraph already has a closer waiting for this opener.
        return None;
    }
    Some(Plan {
        edits: vec![(cursor..cursor, format!("{c}{close}"))],
        cursor: cursor + 1,
        closer: true,
    })
}

/// `}`, `]` and `)`: typing over the next closer.
fn closer(c: char, at: &Typed<'_>) -> Option<Plan> {
    let cursor = at.before.len();
    let math = at.mode == Mode::Math;
    let open = match c {
        '}' => '{',
        ']' => '[',
        _ => '(',
    };
    if ends_with_unescaped_backslash(at.before) {
        // `\}` typed before the `\}` of a `\{` pair: drop the typed `\` and type over the pair's.
        if c == '}' && math && at.after.starts_with("\\}") {
            return Some(Plan {
                edits: vec![(cursor - 1..cursor, String::new())],
                cursor: cursor - 1 + 2,
                closer: false,
            });
        }
        return None;
    }
    // `\left( x \right)`: `)` types over `\right)`.
    if math && c != '}' {
        let right = format!("\\right{c}");
        if at.after.starts_with(&right) {
            return Some(Plan::moved(cursor + right.len()));
        }
    }
    let next = at.after.chars().next()?;
    if next == c {
        // A closer this helper inserted is typed over; another one only when no opener of the
        // paragraph is still unclosed (else the typed closer is that opener's).
        if !at.next_auto && at.paragraph.count(at, |l| balance(l, open, c)) > 0 {
            return None;
        }
        return Some(Plan::moved(cursor + 1));
    }
    // A half-open interval: `[0,1)` or `(0,1]`, typed over the paired closer.
    if math && at.next_auto && matches!((c, next), (')', ']') | (']', ')')) {
        return Some(Plan {
            edits: vec![(cursor..cursor + 1, c.to_string())],
            cursor: cursor + 1,
            closer: false,
        });
    }
    None
}

/// `$`: pairs in text, types over a closing `$` in math.
fn dollar(at: &Typed<'_>) -> Option<Plan> {
    let cursor = at.before.len();
    if ends_with_unescaped_backslash(at.before) {
        return None;
    }
    let next = at.after.chars().next();
    match at.mode {
        Mode::Math => (next == Some('$')).then(|| Plan::moved(cursor + 1)),
        Mode::Text => {
            let blocked = next.is_some_and(|n| n.is_alphanumeric() || n == '\\' || n == '$')
                || at.before.ends_with('$');
            (!blocked).then(|| Plan {
                edits: vec![(cursor..cursor, "$$".into())],
                cursor: cursor + 1,
                closer: false,
            })
        }
        Mode::Arg | Mode::Off => None,
    }
}

/// Adds `\end{x}` to `plan` (the `}` key's own result) when, with it, the text before the cursor
/// ends with `\begin{x}` and the environment is unclosed.
fn auto_end(
    mut plan: Plan,
    at: &Typed<'_>,
    unclosed: &mut dyn FnMut(&str, usize) -> bool,
    nl: &str,
) -> Option<Plan> {
    let cursor = at.before.len();
    // The line before the new cursor: `before` + the `}` (inserted or typed over).
    let typed_over = plan.edits.is_empty();
    if plan.cursor != cursor + 1 {
        return None;
    }
    let mut new_before = at.before.to_owned();
    new_before.push('}');
    let (_, name) = begin_at_end(&new_before)?;
    // The scan starts after the `}` in the text before the key (which holds the `\begin{x}`
    // only when the `}` was typed over).
    let from = if typed_over { cursor + 1 } else { cursor };
    if !unclosed(name, from) {
        return None;
    }
    let indent = leading_ws(at.before);
    let line_end = cursor + at.after.len();
    plan.edits
        .push((line_end..line_end, format!("{nl}{indent}\\end{{{name}}}")));
    Some(plan)
}

/// Backspace: deletes an empty pair whose partner this helper inserted, or which is balanced in
/// its paragraph (an opener typed without a partner, before someone else's closer, is deleted
/// alone). `None` lets the key through.
pub fn on_backspace(at: &Typed<'_>, cfg: &TypingCfg) -> Option<Plan> {
    if at.mode == Mode::Off {
        return None;
    }
    let (before, after) = (at.before, at.after);
    let cursor = before.len();
    let delete = |back: usize, ahead: usize| Plan {
        edits: vec![(cursor - back..cursor + ahead, String::new())],
        cursor: cursor - back,
        closer: false,
    };
    // Whether the pair at the cursor is balanced in the paragraph by `count` (openers minus
    // closers): then the closer after the cursor is the opener's.
    let paired = |count: fn(&str) -> i32| at.paragraph.count(at, count) <= 0;
    if cfg.autopair {
        let last = before.chars().next_back()?;
        let head = &before[..before.len() - last.len_utf8()];
        match last {
            '(' | '[' | '|' if ends_with_command(head, "left") => {
                let right = format!("\\right{}", matching(last));
                if after.starts_with(&right) {
                    return paired(left_right).then(|| delete(1, right.len()));
                }
            }
            '{' if ends_with_unescaped_backslash(head) => {
                let left = head
                    .strip_suffix('\\')
                    .is_some_and(|h| ends_with_command(h, "left"));
                let right = if left { "\\right\\}" } else { "\\}" };
                if after.starts_with(right) {
                    let count: fn(&str) -> i32 = if left { left_right } else { escaped_braces };
                    return paired(count).then(|| delete(1, right.len()));
                }
                return None;
            }
            '{' | '[' | '('
                if !ends_with_unescaped_backslash(head) && after.starts_with(matching(last)) =>
            {
                let count: fn(&str) -> i32 = match last {
                    '{' => |l| balance(l, '{', '}'),
                    '[' => |l| balance(l, '[', ']'),
                    _ => |l| balance(l, '(', ')'),
                };
                return (at.next_auto || paired(count)).then(|| delete(1, 1));
            }
            _ => {}
        }
    }
    if cfg.smart_dollar
        && before.ends_with('$')
        && !ends_with_unescaped_backslash(&before[..before.len() - 1])
        && after.starts_with('$')
    {
        // An odd number of `$` in the paragraph: the one before the cursor has no partner.
        return (at.paragraph.count(at, dollars) % 2 == 0).then(|| delete(1, 1));
    }
    None
}

/// The closer of an opener.
fn matching(open: char) -> char {
    match open {
        '{' => '}',
        '[' => ']',
        '(' => ')',
        other => other,
    }
}

/// The text as the Enter and `\end` rules read it: line by line, so that their work does not
/// grow with the document (see the module docs, "Cost").
pub trait TextLines {
    /// The number of lines (`\n` lines, as in `LineIndex`).
    fn line_count(&self) -> u32;
    /// The byte offset of line `i`'s start; the text's length for `i >= line_count()`.
    fn line_start(&self, i: u32) -> usize;
    /// Line `i` without its `\n` (a `\r` before it stays); `None` past the last line.
    fn line(&self, i: u32) -> Option<Cow<'_, str>>;
    /// Calls `f(line, indent, commands)` for each line of `lines` that holds environment
    /// commands (`rev`: the last line first), until `f` breaks; `indent` is the line's leading
    /// blanks. Scans skip the other lines unread.
    fn for_env_lines(&self, lines: Range<u32>, rev: bool, f: &mut EnvVisitor<'_>) {
        let mut visit = |i: u32| {
            let Some(text) = self.line(i) else {
                return ControlFlow::Continue(());
            };
            if !has_env_word(&text) {
                return ControlFlow::Continue(());
            }
            let toks = env_toks(&text, &mut |name| Rc::from(name));
            if toks.is_empty() {
                return ControlFlow::Continue(());
            }
            f(i, leading_ws(&text), &toks)
        };
        if rev {
            for i in lines.rev() {
                if visit(i).is_break() {
                    return;
                }
            }
        } else {
            for i in lines {
                if visit(i).is_break() {
                    return;
                }
            }
        }
    }
}

/// What [`TextLines::for_env_lines`] calls for each line: `(line, indent, commands)`, breaking to
/// stop the scan.
pub type EnvVisitor<'a> = dyn FnMut(u32, &str, &[EnvTok]) -> ControlFlow<()> + 'a;

/// The kind of an environment command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvKind {
    Begin,
    End,
    Item,
}

/// An environment command on a line: `\begin{name}`, `\end{name}` or `\item`, outside comments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvTok {
    /// The byte of its backslash in the line.
    pub at: usize,
    pub kind: EnvKind,
    /// The environment's name (empty for `\item`).
    pub name: Rc<str>,
    /// Only blanks come before it on the line.
    pub first: bool,
}

/// The environment commands of `line`, their names made by `name` (which may share them).
fn env_toks(line: &str, name: &mut dyn FnMut(&str) -> Rc<str>) -> Vec<EnvTok> {
    env_tokens(line)
        .into_iter()
        .map(|(at, tok)| {
            let (kind, n) = match tok {
                Tok::Begin(n) => (EnvKind::Begin, n),
                Tok::End(n) => (EnvKind::End, n),
                Tok::Item => (EnvKind::Item, ""),
            };
            EnvTok {
                at,
                kind,
                name: name(n),
                first: line[..at].trim().is_empty(),
            }
        })
        .collect()
}

/// [`TextLines`] of a string.
pub struct StrText<'a> {
    text: &'a str,
    index: LineIndex,
}

impl<'a> StrText<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            index: LineIndex::new(text),
        }
    }

    /// Line and byte column of byte `offset`.
    pub fn position(&self, offset: usize) -> (u32, usize) {
        let (line, col) = self.index.position(offset.min(self.text.len()));
        (line, col as usize)
    }
}

impl TextLines for StrText<'_> {
    fn line_count(&self) -> u32 {
        self.index.line_count()
    }

    fn line_start(&self, i: u32) -> usize {
        if i < self.index.line_count() {
            self.index.line_start(i)
        } else {
            self.text.len()
        }
    }

    fn line(&self, i: u32) -> Option<Cow<'_, str>> {
        (i < self.index.line_count()).then(|| Cow::Borrowed(&self.text[self.index.line_range(i)]))
    }
}

/// Whether `line` contains `\begin`, `\end` or `\item` (anywhere, even in a comment).
fn has_env_word(line: &str) -> bool {
    line.contains('\\')
        && (line.contains("\\begin") || line.contains("\\end") || line.contains("\\item"))
}

/// What Enter does at byte `cursor` of `text` (the whole document); offsets are `text`'s.
/// `mode_at(offset)` is the mode at any offset, `unit` one indentation level. `None` lets the
/// key through (the view inserts a line break and indents). See [`on_enter_lines`].
pub fn on_enter(
    text: &str,
    cursor: usize,
    mode_at: &dyn Fn(usize) -> Mode,
    cfg: &TypingCfg,
    unit: &str,
) -> Option<Plan> {
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let lines = StrText::new(text);
    let (line, col) = lines.position(cursor);
    on_enter_lines(&lines, line, col, mode_at, cfg, unit)
}

/// What Enter does at byte `col` of line `line` of `lines`; offsets (of the plan and of
/// `mode_at`) are in the whole text. `mode_at(offset)` is the mode at any offset, `unit` one
/// indentation level. `None` lets the key through (the view inserts a line break and indents).
pub fn on_enter_lines(
    lines: &dyn TextLines,
    line: u32,
    col: usize,
    mode_at: &dyn Fn(usize) -> Mode,
    cfg: &TypingCfg,
    unit: &str,
) -> Option<Plan> {
    if !(cfg.auto_end || cfg.item_on_enter) {
        return None;
    }
    let at = LineAt::new(lines, line, col)?;
    let cursor = at.start + col;
    if mode_at(cursor) == Mode::Off {
        return None;
    }
    let before = &at.text[..col];
    let after = &at.text[col..at.len];
    let nl = at.newline(lines);
    let indent = leading_ws(before);

    // An empty `\item`: leave the list.
    if cfg.item_on_enter && before.trim() == "\\item" && after.trim().is_empty() {
        let env = enclosing_env(lines, line, 0)?;
        if !is_list(&env.name) {
            return None;
        }
        return Some(leave_list(lines, &at, &env, nl));
    }

    // Right after `\begin{x}` and its arguments.
    if after.trim().is_empty()
        && let Some((backslash, name)) = begin_with_args(before)
        && mode_at(at.start + backslash) != Mode::Off
    {
        let item = if cfg.item_on_enter && is_list(name) {
            "\\item "
        } else {
            ""
        };
        let body_indent = if cfg.auto_end && name != "document" {
            format!("{indent}{unit}")
        } else if item.is_empty() {
            indent.to_owned()
        } else {
            format!("{indent}{unit}")
        };
        let replace = cursor..at.start + at.len;
        if cfg.auto_end && env_unclosed_at(lines, line, col, name, true) {
            let body = format!("{nl}{body_indent}{item}");
            let end = format!("{nl}{indent}\\end{{{name}}}");
            return Some(Plan {
                cursor: cursor + body.len(),
                edits: vec![(replace, format!("{body}{end}"))],
                closer: false,
            });
        }
        if body_indent == indent && item.is_empty() {
            // Just a line break: the view does that.
            return None;
        }
        let body = format!("{nl}{body_indent}{item}");
        return Some(Plan {
            cursor: cursor + body.len(),
            edits: vec![(replace, body)],
            closer: false,
        });
    }

    // Continue a list with `\item`.
    if cfg.item_on_enter
        && mode_at(cursor) == Mode::Text
        && !before.trim().is_empty()
        && !after.trim_start().starts_with("\\item")
        && let Some(env) = enclosing_env(lines, line, col)
        && is_list(&env.name)
    {
        let item_indent = env
            .item_indent
            .clone()
            .unwrap_or_else(|| format!("{}{unit}", env.indent));
        let trailing = before.len() - before.trim_end_matches([' ', '\t']).len();
        let leading = after.len() - after.trim_start_matches([' ', '\t']).len();
        let insert = format!("{nl}{item_indent}\\item ");
        let start = cursor - trailing;
        return Some(Plan {
            cursor: start + insert.len(),
            edits: vec![(start..cursor + leading, insert)],
            closer: false,
        });
    }
    None
}

/// Enter on an empty `\item` line `at` of the list `env`.
fn leave_list(lines: &dyn TextLines, at: &LineAt<'_>, env: &Enclosing, nl: &str) -> Plan {
    let next = at.number + 1;
    let next_start = lines.line_start(next);
    let line_end = at.start + at.len;
    if let Some(end_line) = env_end_line(lines, next, &env.name)
        && (next..end_line).all(|l| lines.line(l).is_some_and(|t| t.trim().is_empty()))
        && let Some(end) = LineAt::new(lines, end_line, 0)
    {
        // The list ends here: drop the item line and continue after `\end{…}`.
        let end_indent = leading_ws(&end.text[..end.len]);
        let insert = format!("{nl}{end_indent}");
        let removed = next_start - at.start;
        let end_end = end.start + end.len;
        return Plan {
            cursor: end_end - removed + insert.len(),
            edits: vec![
                (at.start..next_start, String::new()),
                (end_end..end_end, insert),
            ],
            closer: false,
        };
    }
    // More items follow: just empty the line (at the list's indentation).
    Plan {
        cursor: at.start + env.indent.len(),
        edits: vec![(at.start..line_end, env.indent.clone())],
        closer: false,
    }
}

/// A line of a text.
struct LineAt<'a> {
    number: u32,
    /// Byte offset of its start.
    start: usize,
    /// The line without its `\n` (a `\r` before it stays).
    text: Cow<'a, str>,
    /// Its length without the `\r` of a `\r\n` (kept when the column is past it).
    len: usize,
}

impl<'a> LineAt<'a> {
    /// Line `number` of `lines`, for a position at byte `col` of it; `None` past the end or
    /// inside a character.
    fn new(lines: &'a dyn TextLines, number: u32, col: usize) -> Option<Self> {
        let text = lines.line(number)?;
        if col > text.len() || !text.is_char_boundary(col) {
            return None;
        }
        let mut len = text.len();
        if len > col && text.ends_with('\r') {
            len -= 1;
        }
        Some(Self {
            number,
            start: lines.line_start(number),
            text,
            len,
        })
    }

    /// The line break to insert: this line's, else the text's first, else `\n`.
    fn newline(&self, lines: &dyn TextLines) -> &'static str {
        let crlf = |text: &str| if text.ends_with('\r') { "\r\n" } else { "\n" };
        if self.number + 1 < lines.line_count() {
            return crlf(&self.text);
        }
        if lines.line_count() > 1 {
            return lines.line(0).map_or("\n", |first| crlf(&first));
        }
        "\n"
    }
}

/// Lists whose lines Enter continues with `\item`.
pub fn is_list(env: &str) -> bool {
    matches!(
        env.trim_end_matches('*'),
        "itemize" | "enumerate" | "description"
    )
}

/// The leading spaces and tabs of `s`.
fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start_matches([' ', '\t']).len()]
}

/// Whether `s` ends with a backslash that is not itself escaped.
fn ends_with_unescaped_backslash(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Whether `s` ends with the control word `\name` (its backslash not escaped).
fn ends_with_command(s: &str, name: &str) -> bool {
    s.strip_suffix(name)
        .is_some_and(ends_with_unescaped_backslash)
}

/// The line without its comment (from the first `%` that is not escaped).
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(off) = bytes[i..].iter().position(|&b| b == b'%') {
        let at = i + off;
        if !ends_with_unescaped_backslash(&line[..at]) {
            return &line[..at];
        }
        i = at + 1;
    }
    line
}

/// Unescaped `open` minus unescaped `close` on `line` (outside its comment). `\{`, `\[`, … are
/// not counted.
fn balance(line: &str, open: char, close: char) -> i32 {
    let line = strip_comment(line);
    let mut depth = 0;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
        }
    }
    depth
}

/// The unescaped `$` on `line` (outside its comment).
fn dollars(line: &str) -> i32 {
    let line = strip_comment(line).as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b'\\' => i += 1,
            b'$' => count += 1,
            _ => {}
        }
        i += 1;
    }
    count
}

/// `\{` minus `\}` on `line` (outside its comment).
fn escaped_braces(line: &str) -> i32 {
    let line = strip_comment(line).as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + 1 < line.len() {
        if line[i] == b'\\' {
            match line[i + 1] {
                b'{' => count += 1,
                b'}' => count -= 1,
                _ => {}
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    count
}

/// `\left` minus `\right` on `line` (outside its comment).
fn left_right(line: &str) -> i32 {
    let line = strip_comment(line);
    let mut count = 0;
    let mut i = 0;
    let bytes = line.as_bytes();
    while let Some(off) = line[i..].find('\\') {
        let at = i + off;
        let word_len = bytes[at + 1..]
            .iter()
            .take_while(|b| b.is_ascii_alphabetic())
            .count();
        match &line[at + 1..at + 1 + word_len] {
            "left" => count += 1,
            "right" => count -= 1,
            _ => {}
        }
        // After the control word, or after the escaped character (`\\`, `\{`, …).
        let skip = if word_len > 0 {
            word_len
        } else {
            line[at + 1..].chars().next().map_or(0, char::len_utf8)
        };
        i = at + 1 + skip;
        if i >= line.len() {
            break;
        }
    }
    count
}

/// Bytes allowed in environment names.
fn env_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'*' | b'@' | b':' | b'.' | b'_' | b'-')
}

/// When `s` ends with `\begin{name}` (an unescaped backslash): the backslash's byte and `name`.
fn begin_at_end(s: &str) -> Option<(usize, &str)> {
    let head = s.strip_suffix('}')?;
    let open = head.rfind("\\begin{")?;
    let name = &head[open + "\\begin{".len()..];
    let valid = !name.is_empty() && name.bytes().all(env_name_char);
    (valid && !ends_with_unescaped_backslash(&s[..open])).then_some((open, name))
}

/// When `before` (trailing blanks ignored) is `…\begin{name}` followed by complete arguments
/// (`[…]`, `{…}`) only: the backslash's byte and `name`.
fn begin_with_args(before: &str) -> Option<(usize, &str)> {
    let s = strip_comment(before);
    if s.len() != before.len() {
        return None;
    }
    let s = s.trim_end();
    let mut search = s.len();
    // The last `\begin{` whose rest is arguments.
    while let Some(open) = s[..search].rfind("\\begin{") {
        search = open;
        if ends_with_unescaped_backslash(&s[..open]) {
            continue;
        }
        let rest = &s[open + "\\begin{".len()..];
        let Some(close) = rest.find('}') else {
            continue;
        };
        let name = &rest[..close];
        if name.is_empty() || !name.bytes().all(env_name_char) {
            continue;
        }
        return only_arguments(&rest[close + 1..]).then_some((open, name));
    }
    None
}

/// Whether `s` is a sequence of complete `[…]` and `{…}` groups and blanks.
fn only_arguments(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' => i += 1,
            b'[' | b'{' => match group_end(b, i) {
                Some(end) => i = end,
                None => return false,
            },
            _ => return false,
        }
    }
    true
}

/// The byte after the group that opens at `start` (`{…}` with nested braces, or `[…]`, which
/// ends at the first `]` outside braces); `None` when it does not close.
fn group_end(b: &[u8], start: usize) -> Option<usize> {
    let bracket = b[start] == b'[';
    let mut braces = 0i32;
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => braces += 1,
            b'}' if !bracket && braces == 0 => return Some(i + 1),
            b'}' => braces -= 1,
            b']' if bracket && braces == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// An environment token on a line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok<'a> {
    Begin(&'a str),
    End(&'a str),
    Item,
}

/// The `\begin{…}`, `\end{…}` and `\item` tokens of `line` (outside its comment), in order,
/// with their byte offsets.
fn env_tokens(line: &str) -> Vec<(usize, Tok<'_>)> {
    let line = strip_comment(line);
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = line[i..].find('\\') {
        let at = i + off;
        i = at + 1;
        if ends_with_unescaped_backslash(&line[..at]) {
            // `\\begin` is a line break followed by text.
            continue;
        }
        let rest = &line[at + 1..];
        let word_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let word = &rest[..word_len];
        match word {
            "begin" | "end" => {
                let after = &rest[word_len..];
                let after_trim = after.trim_start_matches([' ', '\t']);
                if let Some(arg) = after_trim.strip_prefix('{')
                    && let Some(close) = arg.find('}')
                {
                    let name = &arg[..close];
                    if !name.is_empty() && name.bytes().all(env_name_char) {
                        out.push((
                            at,
                            if word == "begin" {
                                Tok::Begin(name)
                            } else {
                                Tok::End(name)
                            },
                        ));
                    }
                }
            }
            "item" => out.push((at, Tok::Item)),
            _ => {}
        }
        i = at + 1 + word_len;
    }
    out
}

/// The environment around a position.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Enclosing {
    name: String,
    /// The leading blanks of its `\begin` line.
    indent: String,
    /// The leading blanks of the nearest `\item` line above the position at this level.
    item_indent: Option<String>,
}

/// The innermost environment around byte `col` of line `line` (a backward scan reading at most
/// [`MAX_SCAN_BACK_LINES`] lines with environment commands, skipping comments).
fn enclosing_env(lines: &dyn TextLines, line: u32, col: usize) -> Option<Enclosing> {
    let mut stack: Vec<Rc<str>> = Vec::new();
    let mut item_indent: Option<String> = None;
    let mut found: Option<Enclosing> = None;
    let mut visit = |indent: &str, toks: &[EnvTok]| {
        for tok in toks.iter().rev() {
            match tok.kind {
                EnvKind::End => stack.push(tok.name.clone()),
                // A `\begin` without its `\end` in between encloses the position.
                EnvKind::Begin => match stack.iter().rposition(|n| *n == tok.name) {
                    Some(i) => stack.truncate(i),
                    None => {
                        found = Some(Enclosing {
                            name: tok.name.to_string(),
                            indent: indent.to_owned(),
                            item_indent: item_indent.take(),
                        });
                        return ControlFlow::Break(());
                    }
                },
                EnvKind::Item => {
                    if stack.is_empty() && item_indent.is_none() && tok.first {
                        item_indent = Some(indent.to_owned());
                    }
                }
            }
        }
        ControlFlow::Continue(())
    };
    // The cursor's line up to the cursor, then the lines above with environment commands.
    let text = lines.line(line)?;
    let part = text.get(..col.min(text.len()))?;
    let toks = env_toks(part, &mut |name| Rc::from(name));
    if visit(leading_ws(part), &toks).is_continue() {
        let mut read = 1;
        lines.for_env_lines(0..line, true, &mut |_, indent, toks| {
            read += 1;
            match visit(indent, toks) {
                ControlFlow::Continue(()) if read >= MAX_SCAN_BACK_LINES => ControlFlow::Break(()),
                flow => flow,
            }
        });
    }
    found
}

/// Whether the environment `name`, whose `\begin{name}` ends at byte `from` (`ours_in_text`:
/// that `\begin` is already part of `text`; else it is being completed), is unclosed. See
/// [`env_unclosed_at`].
pub fn env_unclosed(text: &str, from: usize, name: &str, ours_in_text: bool) -> bool {
    let lines = StrText::new(text);
    let (line, col) = lines.position(from);
    env_unclosed_at(&lines, line, col, name, ours_in_text)
}

/// Whether the environment `name`, whose `\begin{name}` ends at byte `col` of line `line`
/// (`ours_in_text`: that `\begin` is already part of the text; else it is being completed), is
/// unclosed:
///
/// - the document (outside comments) has fewer `\end{name}` than `\begin{name}`, ours counted,
///   so a nested environment of the same name is unclosed too;
/// - and a forward scan does not first meet an `\end{other}` that the whole document has too many
///   of: then `\begin{name}` was just renamed from `other`, and its `\end` is already there.
pub fn env_unclosed_at(
    lines: &dyn TextLines,
    line: u32,
    col: usize,
    name: &str,
    ours_in_text: bool,
) -> bool {
    let (begins, ends) = count_env(lines, name);
    if ends >= begins + usize::from(!ours_in_text) {
        return false;
    }
    let mut stack: Vec<Rc<str>> = Vec::new();
    // The first `\end` that closes something around ours (or ours, renamed).
    let mut outer: Option<Rc<str>> = None;
    let mut visit = |toks: &[EnvTok]| {
        for tok in toks {
            match tok.kind {
                EnvKind::Begin => stack.push(tok.name.clone()),
                EnvKind::End => {
                    if let Some(i) = stack.iter().rposition(|s| *s == tok.name) {
                        stack.truncate(i);
                    } else if *tok.name != *name {
                        outer = Some(tok.name.clone());
                        return ControlFlow::Break(());
                    }
                }
                EnvKind::Item => {}
            }
        }
        ControlFlow::Continue(())
    };
    // The rest of the line, then the lines below with environment commands.
    if let Some(text) = lines.line(line) {
        let rest = text.get(col.min(text.len())..).unwrap_or_default();
        if visit(&env_toks(rest, &mut |n| Rc::from(n))).is_continue() {
            lines.for_env_lines(line + 1..lines.line_count(), false, &mut |_, _, toks| {
                visit(toks)
            });
        }
    }
    match outer {
        Some(other) => {
            let (b, e) = count_env(lines, &other);
            e <= b
        }
        None => true,
    }
}

/// How many `\begin{name}` and `\end{name}` the text has outside comments.
fn count_env(lines: &dyn TextLines, name: &str) -> (usize, usize) {
    let (mut begins, mut ends) = (0usize, 0usize);
    lines.for_env_lines(0..lines.line_count(), false, &mut |_, _, toks| {
        for tok in toks {
            if *tok.name == *name {
                match tok.kind {
                    EnvKind::Begin => begins += 1,
                    EnvKind::End => ends += 1,
                    EnvKind::Item => {}
                }
            }
        }
        ControlFlow::Continue(())
    });
    (begins, ends)
}

/// The line holding the `\end{name}` that closes the environment containing the start of line
/// `from` (a forward scan).
fn env_end_line(lines: &dyn TextLines, from: u32, name: &str) -> Option<u32> {
    let mut stack: Vec<Rc<str>> = Vec::new();
    let mut found = None;
    lines.for_env_lines(from..lines.line_count(), false, &mut |line, _, toks| {
        for tok in toks {
            match tok.kind {
                EnvKind::Begin => stack.push(tok.name.clone()),
                EnvKind::End => {
                    if let Some(i) = stack.iter().rposition(|s| *s == tok.name) {
                        stack.truncate(i);
                    } else {
                        found = (*tok.name == *name).then_some(line);
                        return ControlFlow::Break(());
                    }
                }
                EnvKind::Item => {}
            }
        }
        ControlFlow::Continue(())
    });
    found
}

// ───────────── Per editor ─────────────

/// Modifiers that make a key a command (Shift is part of a character; AltGr and the locks are
/// not modifiers here).
const COMMAND_MODIFIERS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SUPER_MASK)
    .union(gdk::ModifierType::HYPER_MASK)
    .union(gdk::ModifierType::META_MASK);

/// The keys the helpers act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelperKey {
    /// Backspace (with or without Shift).
    Backspace,
    /// Enter without Shift.
    Enter,
    /// A typed `{ [ ( | } ] ) $`, or `.` (replacements).
    Char(char),
}

impl HelperKey {
    fn of(event: &keys::KeyEvent) -> Option<Self> {
        let commands = event.state.intersects(COMMAND_MODIFIERS);
        match event.keyval {
            gdk::Key::BackSpace if !commands => Some(Self::Backspace),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter
                if !commands && !event.state.contains(gdk::ModifierType::SHIFT_MASK) =>
            {
                Some(Self::Enter)
            }
            _ => match snippets::typed_char(event.keyval, event.state) {
                Some(c @ ('{' | '[' | '(' | '|' | '}' | ']' | ')' | '$' | '.')) => {
                    Some(Self::Char(c))
                }
                _ => None,
            },
        }
    }
}

/// A plain line break the view is about to insert while the completion popup is open; see the
/// module docs.
struct PendingEnter {
    version: u64,
    line: u32,
    offset: usize,
    before: String,
    after: String,
    plan: Plan,
}

/// The typing helpers of one editor.
struct Attached {
    state: Weak<WindowState>,
    editor: Weak<Editor>,
    view: glib::WeakRef<TexView>,
    doc: Weak<Document>,
    buffer: sourceview5::Buffer,
    handler: Cell<Option<keys::HandlerId>>,
    preedit: Rc<Cell<bool>>,
    view_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    /// Marks right before closers this helper inserted (right gravity: typing before the closer
    /// keeps the mark next to it).
    closers: RefCell<Vec<gtk::TextMark>>,
    pending_enter: RefCell<Option<PendingEnter>>,
    /// Which lines may hold environment commands.
    envs: Rc<EnvIndex>,
    /// The document observer that keeps `envs` up to date.
    observer: Cell<Option<crate::document::ObserverId>>,
}

thread_local! {
    static EDITORS: RefCell<Vec<Rc<Attached>>> = const { RefCell::new(Vec::new()) };
}

fn attached_of(editor: &Editor) -> Option<Rc<Attached>> {
    EDITORS.with_borrow(|editors| {
        editors
            .iter()
            .find(|a| std::ptr::eq(a.editor.as_ptr(), editor))
            .cloned()
    })
}

/// Starts the typing helpers in `editor` (see the module docs). Nothing happens for a document
/// that is not LaTeX (`completion::is_latex`: no `$` pairing in Markdown or a shell script).
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if attached_of(editor).is_some() || !completion::is_latex(editor.document().path()) {
        return;
    }
    let view = editor.view();
    let attached = Rc::new(Attached {
        state: Rc::downgrade(state),
        editor: Rc::downgrade(editor),
        view: view.downgrade(),
        doc: Rc::downgrade(editor.document()),
        buffer: editor.buffer().clone(),
        handler: Cell::new(None),
        preedit: Rc::new(Cell::new(false)),
        view_handlers: RefCell::default(),
        closers: RefCell::default(),
        pending_enter: RefCell::new(None),
        envs: Rc::new(EnvIndex::new(editor.document().line_index().line_count())),
        observer: Cell::new(None),
    });
    let envs = attached.envs.clone();
    let doc = Rc::downgrade(editor.document());
    attached
        .observer
        .set(Some(editor.document().connect_lines_changed(
            move |first, removed, added| {
                envs.changed(first, removed, added);
                EnvIndex::schedule_fill(&envs, &doc);
            },
        )));
    let preedit = attached.preedit.clone();
    let handler = view.connect_preedit_changed(move |_, text| preedit.set(!text.is_empty()));
    attached.view_handlers.replace(vec![handler]);
    let weak = Rc::downgrade(&attached);
    attached.handler.set(keys::register(
        view,
        keys::PRIORITY_TYPING,
        move |event, mode| match weak.upgrade() {
            Some(attached) => attached.on_key(event, mode),
            None => glib::Propagation::Proceed,
        },
    ));
    EDITORS.with_borrow_mut(|editors| {
        editors.retain(|a| a.editor.strong_count() > 0);
        editors.push(attached);
    });
}

/// Stops them (see the module docs). Safe when [`attach`] never ran for `editor`.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let removed = EDITORS.with_borrow_mut(|editors| {
        let at = editors
            .iter()
            .position(|a| std::ptr::eq(a.editor.as_ptr(), Rc::as_ptr(editor)));
        at.map(|i| editors.remove(i))
    });
    if let Some(attached) = removed {
        attached.shutdown();
    }
}

impl Attached {
    fn shutdown(&self) {
        if let Some(view) = self.view.upgrade() {
            if let Some(id) = self.handler.take() {
                keys::unregister(&view, id);
            }
            for id in self.view_handlers.take() {
                view.disconnect(id);
            }
        }
        for mark in self.closers.take() {
            if !mark.is_deleted() {
                self.buffer.delete_mark(&mark);
            }
        }
        self.pending_enter.replace(None);
        if let (Some(id), Some(doc)) = (self.observer.take(), self.doc.upgrade()) {
            doc.disconnect(id);
        }
    }

    /// The key-layer handler (priority [`keys::PRIORITY_TYPING`]).
    fn on_key(self: &Rc<Self>, event: &keys::KeyEvent, mode: VimMode) -> glib::Propagation {
        let handled = mode == VimMode::Insert && self.handle(event);
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }

    /// Runs the helper for `event`; true when it consumed the key.
    fn handle(self: &Rc<Self>, event: &keys::KeyEvent) -> bool {
        let Some(key) = HelperKey::of(event) else {
            return false;
        };
        let (Some(state), Some(editor), Some(view), Some(doc)) = (
            self.state.upgrade(),
            self.editor.upgrade(),
            self.view.upgrade(),
            self.doc.upgrade(),
        ) else {
            return false;
        };
        if !view.is_editable()
            || view.overwrites()
            || self.preedit.get()
            || self.buffer.has_selection()
            || !doc.has_line_states()
        {
            return false;
        }
        let cfg = state.config.typing.clone();
        // `.` only ever completes a replacement.
        if key == HelperKey::Char('.') && !cfg.replacements {
            return false;
        }
        match key {
            HelperKey::Backspace => self.backspace(&state, &editor, &doc, &cfg),
            HelperKey::Enter => self.enter(&editor, &view, &doc, &cfg),
            HelperKey::Char(c) => self.typed(c, &state, &editor, &doc, &cfg),
        }
    }

    /// The cursor's line split at the cursor, its start and the mode there.
    fn cursor_line(&self, doc: &Document) -> Option<CursorLine> {
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let offset = doc.byte_offset(&cursor);
        let (line, col) = doc.line_index().position(offset);
        let start = doc.line_index().line_start(line);
        let mut text = doc.line_text(line)?;
        let crlf = text.ends_with('\r');
        if crlf {
            text.pop();
        }
        let col = (col as usize).min(text.len());
        if !text.is_char_boundary(col) {
            return None;
        }
        let ctx = doc.context_at_byte(offset);
        let mode = Mode::of(&ctx, |at| doc.context_at_byte(start + at as usize));
        Some(CursorLine {
            line,
            start,
            offset,
            text,
            col,
            mode,
            crlf,
        })
    }

    /// The document's lines as the rules read them.
    fn lines<'a>(&'a self, doc: &'a Document) -> DocLines<'a> {
        DocLines {
            doc,
            buffer: &self.buffer,
            envs: &self.envs,
            copy: OnceCell::new(),
        }
    }

    fn typed(
        &self,
        c: char,
        state: &Rc<WindowState>,
        editor: &Rc<Editor>,
        doc: &Document,
        cfg: &TypingCfg,
    ) -> bool {
        let Some(at) = self.cursor_line(doc) else {
            return false;
        };
        let next_auto = self.next_is_auto_closer();
        let lines = self.lines(doc);
        let paragraph = Paragraph::around(&lines, at.line);
        let typed = Typed {
            before: &at.text[..at.col],
            after: &at.text[at.col..],
            mode: at.mode,
            next_auto,
            paragraph: &paragraph,
        };
        if let Some((trigger, with)) = replacement(c, &typed, cfg) {
            return completion::type_and_replace(state, editor, c, trigger, with);
        }
        let nl = if at.crlf { "\r\n" } else { "\n" };
        let mut unclosed = |name: &str, from: usize| {
            // The scan starts after the `}` only when it was typed over: then the `\begin{name}`
            // is already in the text.
            let ours = from > typed.before.len();
            env_unclosed_at(&lines, at.line, from, name, ours)
        };
        let Some(plan) = on_char(c, &typed, cfg, &mut unclosed, nl) else {
            return false;
        };
        let moved_only = plan.edits.is_empty();
        let replaced_closer = next_auto
            && plan
                .edits
                .iter()
                .any(|(r, _)| r.start == at.col && r.end > r.start);
        if moved_only || replaced_closer {
            self.forget_closer_at_cursor();
        }
        if !self.apply(doc, at.start, at.col, &plan) {
            return false;
        }
        if moved_only {
            // The cursor left what the popup was completing.
            completion::hide(state, editor);
        }
        true
    }

    fn backspace(
        &self,
        state: &Rc<WindowState>,
        editor: &Rc<Editor>,
        doc: &Document,
        cfg: &TypingCfg,
    ) -> bool {
        let Some(at) = self.cursor_line(doc) else {
            return false;
        };
        let lines = self.lines(doc);
        let paragraph = Paragraph::around(&lines, at.line);
        let typed = Typed {
            before: &at.text[..at.col],
            after: &at.text[at.col..],
            mode: at.mode,
            next_auto: self.next_is_auto_closer(),
            paragraph: &paragraph,
        };
        let Some(plan) = on_backspace(&typed, cfg) else {
            return false;
        };
        self.forget_closer_at_cursor();
        let applied = self.apply(doc, at.start, at.col, &plan);
        if applied {
            completion::hide(state, editor);
        }
        applied
    }

    fn enter(
        self: &Rc<Self>,
        editor: &Editor,
        view: &TexView,
        doc: &Document,
        cfg: &TypingCfg,
    ) -> bool {
        let Some(at) = self.cursor_line(doc) else {
            return false;
        };
        if at.mode == Mode::Off {
            return false;
        }
        let unit = indent_unit(view);
        let mode_at = |offset: usize| {
            let line = doc.line_index().line_of(offset);
            let start = doc.line_index().line_start(line);
            let ctx = doc.context_at_byte(offset);
            Mode::of(&ctx, |at| doc.context_at_byte(start + at as usize))
        };
        let plan = on_enter_lines(&self.lines(doc), at.line, at.col, &mode_at, cfg, &unit);
        let Some(plan) = plan else {
            return false;
        };
        if completion::popup_visible(editor) {
            // The popup may accept a proposal with this Enter: see what the view did first.
            self.pending_enter.replace(Some(PendingEnter {
                version: doc.version(),
                line: at.line,
                offset: at.offset,
                before: at.text[..at.col].to_owned(),
                after: at.text[at.col..].to_owned(),
                plan,
            }));
            let weak = Rc::downgrade(self);
            glib::idle_add_local_full(glib::Priority::HIGH, move || {
                if let Some(attached) = weak.upgrade() {
                    attached.finish_enter();
                }
                glib::ControlFlow::Break
            });
            return false;
        }
        self.apply(doc, 0, at.offset, &plan)
    }

    /// After an Enter the popup let through: when the view inserted a plain line break (and
    /// indentation), replaces it with the planned result.
    fn finish_enter(&self) {
        let Some(pending) = self.pending_enter.take() else {
            return;
        };
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        if doc.version() == pending.version {
            return;
        }
        let strip = |mut s: String| {
            if s.ends_with('\r') {
                s.pop();
            }
            s
        };
        let (Some(old), Some(new)) = (doc.line_text(pending.line), doc.line_text(pending.line + 1))
        else {
            return;
        };
        let (old, new) = (strip(old), strip(new));
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let offset = doc.byte_offset(&cursor);
        let new_start = doc.line_index().line_start(pending.line + 1);
        // `before` stayed, and the new line is blanks (the indentation) and then `after`.
        let typed_break = old == pending.before
            && new.ends_with(&pending.after)
            && new[..new.len() - pending.after.len()].trim().is_empty()
            && offset == new_start + new.len() - pending.after.len()
            && doc.line_index().line_start(pending.line) + old.len() == pending.offset
            && offset >= pending.offset;
        if !typed_break {
            return;
        }
        self.end_snippet_sessions();
        let Some(mut start) = doc.iter_at_byte(pending.offset) else {
            return;
        };
        let mut end = cursor;
        // Take the view's line break back, then apply the plan to the text as it was.
        self.buffer.begin_user_action();
        self.buffer.delete(&mut start, &mut end);
        self.apply(&doc, 0, pending.offset, &pending.plan);
        self.buffer.end_user_action();
    }

    /// Applies `plan` (offsets relative to `base`; the cursor was at `cursor`) as one user action
    /// and places the cursor. False (nothing applied) when an offset does not fit the text.
    ///
    /// Edits that are not typing at the cursor (an insertion there, or a deletion of what is
    /// right before it) end an active snippet session first: GtkSourceView 5.20 rewrites mirror
    /// chunks during other edits (see `snippets::end_snippet_sessions`).
    fn apply(&self, doc: &Document, base: usize, cursor: usize, plan: &Plan) -> bool {
        let fits = |offset: usize| doc.iter_at_byte(base + offset).is_some();
        if !plan
            .edits
            .iter()
            .all(|(range, _)| fits(range.start) && fits(range.end))
        {
            return false;
        }
        let typing = plan
            .edits
            .iter()
            .all(|(range, text)| range.end == cursor && (range.start == cursor || text.is_empty()));
        if !typing {
            self.end_snippet_sessions();
        }
        let buffer = &self.buffer;
        buffer.begin_user_action();
        for (range, text) in plan.edits.iter().rev() {
            let Some(mut start) = doc.iter_at_byte(base + range.start) else {
                continue;
            };
            if range.end > range.start
                && let Some(mut end) = doc.iter_at_byte(base + range.end)
            {
                buffer.delete(&mut start, &mut end);
            }
            if !text.is_empty() {
                buffer.insert(&mut start, text);
            }
        }
        buffer.end_user_action();
        if let Some(cursor) = doc.iter_at_byte(base + plan.cursor) {
            buffer.place_cursor(&cursor);
            if plan.closer {
                self.track_closer(&cursor);
            }
        }
        if let Some(view) = self.view.upgrade() {
            view.scroll_mark_onscreen(&buffer.get_insert());
        }
        true
    }

    /// Ends the view's snippet sessions while one is active.
    fn end_snippet_sessions(&self) {
        if let (Some(state), Some(editor)) = (self.state.upgrade(), self.editor.upgrade())
            && completion::snippet_active(&state, &editor)
        {
            snippets::end_snippet_sessions(editor.view());
        }
    }

    /// Remembers that the character after `at` is a closer this helper inserted.
    fn track_closer(&self, at: &gtk::TextIter) {
        let mut closers = self.closers.borrow_mut();
        closers.retain(|m| {
            let keep = !m.is_deleted() && closer_after(&self.buffer, m);
            if !keep && !m.is_deleted() {
                self.buffer.delete_mark(m);
            }
            keep
        });
        if closers.len() >= MAX_TRACKED_CLOSERS {
            let old = closers.remove(0);
            self.buffer.delete_mark(&old);
        }
        closers.push(self.buffer.create_mark(None, at, false));
    }

    /// Whether the character after the cursor is a closer this helper inserted.
    fn next_is_auto_closer(&self) -> bool {
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        self.closers.borrow().iter().any(|m| {
            !m.is_deleted()
                && self.buffer.iter_at_mark(m) == cursor
                && closer_after(&self.buffer, m)
        })
    }

    /// Forgets the tracked closer right after the cursor (typed over, replaced or deleted).
    fn forget_closer_at_cursor(&self) {
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        self.closers.borrow_mut().retain(|m| {
            let here = !m.is_deleted() && self.buffer.iter_at_mark(m) == cursor;
            if here {
                self.buffer.delete_mark(m);
            }
            !here && !m.is_deleted()
        });
    }
}

/// Whether the character after `mark` is `)`, `]` or `}`.
fn closer_after(buffer: &sourceview5::Buffer, mark: &gtk::TextMark) -> bool {
    matches!(buffer.iter_at_mark(mark).char(), ')' | ']' | '}')
}

/// [`EnvIndex`] marks.
const ENV_NONE: u8 = 0;
const ENV_SOME: u8 = 1;
const ENV_UNKNOWN: u8 = 2;

/// The environment commands of one line, as [`EnvIndex`] keeps them.
struct EnvLine {
    /// The line's leading blanks.
    indent: Box<str>,
    toks: Box<[EnvTok]>,
}

/// The environment commands of a document's lines, kept in step with every change: one mark
/// per line (none, some, or unknown since a change) and the commands of the lines that have
/// some, so that scans neither read nor parse lines. Changed lines are read when a scan meets
/// them; after a change of many lines, every line is classified from one copy of the text.
struct EnvIndex {
    marks: RefCell<Vec<u8>>,
    /// How many marks are [`ENV_UNKNOWN`].
    unknown: Cell<usize>,
    /// The commands of the lines marked [`ENV_SOME`], by line, ascending.
    lines: RefCell<Vec<(u32, EnvLine)>>,
    /// The environment names seen, shared by the commands.
    names: RefCell<HashSet<Rc<str>>>,
    /// A [`Self::schedule_fill`] idle is pending.
    fill_scheduled: Cell<bool>,
}

impl EnvIndex {
    fn new(lines: u32) -> Self {
        Self {
            marks: RefCell::new(vec![ENV_UNKNOWN; lines as usize]),
            unknown: Cell::new(lines as usize),
            lines: RefCell::default(),
            names: RefCell::default(),
            fill_scheduled: Cell::new(false),
        }
    }

    /// After a change of many lines (loading, a paste, a reload, a whole-text replacement),
    /// classifies them from a high-priority idle callback (before GTK's layout and the next
    /// frame), so that the next Enter or `}` does not pay for copying and reading the whole text
    /// in its keystroke (1–2 ms on a 5k-line document, which `perf_typing_keys` measured).
    fn schedule_fill(envs: &Rc<Self>, doc: &Weak<Document>) {
        if envs.unknown.get() <= ENV_FILL_AT || envs.fill_scheduled.replace(true) {
            return;
        }
        let (envs, doc) = (envs.clone(), doc.clone());
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            envs.fill_scheduled.set(false);
            if let Some(doc) = doc.upgrade() {
                let count = doc.line_index().line_count();
                if envs.needs_fill(count) {
                    doc.with_text(|text| envs.fill(text, &doc.line_index()));
                }
            }
            glib::ControlFlow::Break
        });
    }

    /// After a change: lines `[first, first + removed)` became `added` lines.
    fn changed(&self, first: u32, removed: u32, added: u32) {
        let mut marks = self.marks.borrow_mut();
        let (start, end) = (first as usize, (first + removed) as usize);
        if end > marks.len() {
            // Out of step (cannot happen): the next use starts over.
            marks.clear();
            self.lines.borrow_mut().clear();
            self.unknown.set(0);
            return;
        }
        let gone = marks[start..end]
            .iter()
            .filter(|&&m| m == ENV_UNKNOWN)
            .count();
        marks.splice(start..end, std::iter::repeat_n(ENV_UNKNOWN, added as usize));
        self.unknown.set(self.unknown.get() - gone + added as usize);
        let mut lines = self.lines.borrow_mut();
        let lo = lines.partition_point(|(l, _)| *l < first);
        let hi = lines.partition_point(|(l, _)| *l < first + removed);
        lines.drain(lo..hi);
        for (l, _) in &mut lines[lo..] {
            *l = *l - removed + added;
        }
    }

    /// Makes the marks describe `count` lines; true when so many are unknown that [`Self::fill`]
    /// should classify them all at once.
    fn needs_fill(&self, count: u32) -> bool {
        let mut marks = self.marks.borrow_mut();
        if marks.len() != count as usize {
            *marks = vec![ENV_UNKNOWN; count as usize];
            self.lines.borrow_mut().clear();
            self.unknown.set(count as usize);
        }
        self.unknown.get() > ENV_FILL_AT
    }

    /// Classifies every line of `text` (cut by `index`).
    fn fill(&self, text: &str, index: &LineIndex) {
        if self.marks.borrow().len() != index.line_count() as usize {
            return;
        }
        let mut candidates: Vec<u32> = ["\\begin", "\\end", "\\item"]
            .iter()
            .flat_map(|word| text.match_indices(word).map(|(at, _)| index.line_of(at)))
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        self.names.borrow_mut().clear();
        let mut lines = Vec::new();
        let mut marks = self.marks.borrow_mut();
        marks.fill(ENV_NONE);
        for line in candidates {
            if let Some(env) = self.read(&text[index.line_range(line)]) {
                marks[line as usize] = ENV_SOME;
                lines.push((line, env));
            }
        }
        self.lines.replace(lines);
        self.unknown.set(0);
    }

    /// The commands of a line's `text`, if it has any.
    fn read(&self, text: &str) -> Option<EnvLine> {
        if !has_env_word(text) {
            return None;
        }
        let mut names = self.names.borrow_mut();
        let toks = env_toks(text, &mut |name| match names.get(name) {
            Some(shared) => shared.clone(),
            None => {
                let shared: Rc<str> = Rc::from(name);
                names.insert(shared.clone());
                shared
            }
        });
        (!toks.is_empty()).then(|| EnvLine {
            indent: leading_ws(text).into(),
            toks: toks.into_boxed_slice(),
        })
    }

    /// The unknown lines of `lines`.
    fn unknown_in(&self, lines: Range<u32>) -> Vec<u32> {
        if self.unknown.get() == 0 {
            return Vec::new();
        }
        let marks = self.marks.borrow();
        let end = (lines.end as usize).min(marks.len());
        let start = (lines.start as usize).min(end);
        (start..end)
            .filter(|&i| marks[i] == ENV_UNKNOWN)
            .map(|i| i as u32)
            .collect()
    }

    /// Records what line `line` (unknown until now) holds, read from its `text` (`None`: gone).
    fn learn(&self, line: u32, text: Option<&str>) {
        let env = text.and_then(|t| self.read(t));
        let mut marks = self.marks.borrow_mut();
        let Some(mark) = marks.get_mut(line as usize) else {
            return;
        };
        if *mark != ENV_UNKNOWN {
            return;
        }
        self.unknown.set(self.unknown.get().saturating_sub(1));
        match env {
            Some(env) => {
                *mark = ENV_SOME;
                let mut lines = self.lines.borrow_mut();
                let at = lines.partition_point(|(l, _)| *l < line);
                lines.insert(at, (line, env));
            }
            None => *mark = ENV_NONE,
        }
    }

    /// Calls `f` for the known lines of `lines` with commands (see [`TextLines::for_env_lines`]).
    fn visit(&self, lines: Range<u32>, rev: bool, f: &mut EnvVisitor<'_>) {
        let known = self.lines.borrow();
        let lo = known.partition_point(|(l, _)| *l < lines.start);
        let hi = known.partition_point(|(l, _)| *l < lines.end);
        let mut call = |(line, env): &(u32, EnvLine)| f(*line, &env.indent, &env.toks);
        if rev {
            for entry in known[lo..hi].iter().rev() {
                if call(entry).is_break() {
                    return;
                }
            }
        } else {
            for entry in &known[lo..hi] {
                if call(entry).is_break() {
                    return;
                }
            }
        }
    }
}

/// [`TextLines`] of an open document for one key: the buffer's lines (from one copy of the text
/// while GTK's lines are not the document's, see `Document::byte_offset`), with [`EnvIndex`].
struct DocLines<'a> {
    doc: &'a Document,
    buffer: &'a sourceview5::Buffer,
    envs: &'a EnvIndex,
    copy: OnceCell<glib::GString>,
}

impl DocLines<'_> {
    /// Whether GTK's lines are the document's.
    fn gtk_lines(&self) -> bool {
        u32::try_from(self.buffer.line_count())
            .is_ok_and(|n| n == self.doc.line_index().line_count())
    }

    fn text(&self) -> &glib::GString {
        self.copy
            .get_or_init(|| crate::document::buffer_text(self.buffer))
    }
}

impl TextLines for DocLines<'_> {
    fn line_count(&self) -> u32 {
        self.doc.line_index().line_count()
    }

    fn line_start(&self, i: u32) -> usize {
        let index = self.doc.line_index();
        if i < index.line_count() {
            index.line_start(i)
        } else {
            index.text_len()
        }
    }

    fn line(&self, i: u32) -> Option<Cow<'_, str>> {
        if self.gtk_lines() {
            return self.doc.line_text(i).map(Cow::Owned);
        }
        let range = {
            let index = self.doc.line_index();
            (i < index.line_count()).then(|| index.line_range(i))?
        };
        self.text().get(range).map(Cow::Borrowed)
    }

    fn for_env_lines(&self, lines: Range<u32>, rev: bool, f: &mut EnvVisitor<'_>) {
        let count = self.line_count();
        if self.envs.needs_fill(count) {
            self.envs.fill(self.text(), &self.doc.line_index());
        }
        let lines = lines.start.min(count)..lines.end.min(count);
        // Lines changed since they were read.
        for line in self.envs.unknown_in(lines.clone()) {
            self.envs.learn(line, self.line(line).as_deref());
        }
        self.envs.visit(lines, rev, f);
    }
}

/// The cursor's line as [`Attached`] reads it.
struct CursorLine {
    line: u32,
    /// Byte offset of the line start.
    start: usize,
    /// Byte offset of the cursor.
    offset: usize,
    /// The line without its line break.
    text: String,
    /// The cursor's byte in `text`.
    col: usize,
    mode: Mode,
    crlf: bool,
}

/// One level of indentation in `view`: its indent width in spaces, or a tab.
fn indent_unit(view: &TexView) -> String {
    if !view.is_insert_spaces_instead_of_tabs() {
        return "\t".into();
    }
    let width = match view.indent_width() {
        w if w > 0 => w,
        _ => i32::try_from(view.tab_width()).unwrap_or(2),
    };
    " ".repeat(usize::try_from(width).unwrap_or(2).clamp(1, 16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_core::LineIndex;
    use lemma_syntax::{LineStates, ScanConfig, StrLines};
    use std::sync::Arc;

    fn cfg() -> TypingCfg {
        TypingCfg::default()
    }

    /// The modes of `text` from the scanner (as the editor computes them).
    fn modes(text: &str) -> impl Fn(usize) -> Mode + '_ {
        let index = LineIndex::new(text);
        let mut states = LineStates::new(Arc::new(ScanConfig::default()));
        states.rebuild(&StrLines {
            text,
            index: &index,
        });
        move |offset: usize| {
            let (line, col) = index.position(offset);
            let range = index.line_range(line);
            let line_text = &text[range.clone()];
            let ctx = states.context_at(line, line_text, col);
            Mode::of(&ctx, |at| states.context_at(line, line_text, at))
        }
    }

    /// Types `c` at `|` in `doc` (one `|` marks the cursor; its line is the line). Returns the
    /// text with `|` at the new cursor, or `None` when the key goes through.
    fn type_char(doc: &str, c: char) -> Option<String> {
        type_char_with(doc, c, &cfg(), false)
    }

    fn type_char_with(doc: &str, c: char, cfg: &TypingCfg, next_auto: bool) -> Option<String> {
        let cursor = doc.find('|').expect("a cursor");
        let text = doc.replacen('|', "", 1);
        let mode_at = modes(&text);
        let lines = StrText::new(&text);
        let (line, col) = lines.position(cursor);
        let at_line = LineAt::new(&lines, line, col).unwrap();
        let paragraph = Paragraph::around(&lines, line);
        let at = Typed {
            before: &at_line.text[..col],
            after: &at_line.text[col..at_line.len],
            mode: mode_at(cursor),
            next_auto,
            paragraph: &paragraph,
        };
        let start = at_line.start;
        let mut unclosed = |name: &str, from: usize| {
            env_unclosed(&text, start + from, name, from > at.before.len())
        };
        let plan = on_char(c, &at, cfg, &mut unclosed, "\n")?;
        Some(render(&text, start, &plan))
    }

    fn backspace(doc: &str) -> Option<String> {
        backspace_with(doc, false)
    }

    fn backspace_with(doc: &str, next_auto: bool) -> Option<String> {
        let cursor = doc.find('|').expect("a cursor");
        let text = doc.replacen('|', "", 1);
        let mode_at = modes(&text);
        let lines = StrText::new(&text);
        let (line, col) = lines.position(cursor);
        let at_line = LineAt::new(&lines, line, col).unwrap();
        let paragraph = Paragraph::around(&lines, line);
        let at = Typed {
            before: &at_line.text[..col],
            after: &at_line.text[col..at_line.len],
            mode: mode_at(cursor),
            next_auto,
            paragraph: &paragraph,
        };
        let plan = on_backspace(&at, &cfg())?;
        Some(render(&text, at_line.start, &plan))
    }

    fn enter(doc: &str) -> Option<String> {
        enter_with(doc, &cfg())
    }

    fn enter_with(doc: &str, cfg: &TypingCfg) -> Option<String> {
        let cursor = doc.find('|').expect("a cursor");
        let text = doc.replacen('|', "", 1);
        let mode_at = modes(&text);
        let plan = on_enter(&text, cursor, &mode_at, cfg, "  ")?;
        Some(render(&text, 0, &plan))
    }

    /// The text after `plan` (offsets relative to `base`) with `|` at the cursor.
    fn render(text: &str, base: usize, plan: &Plan) -> String {
        let shifted = Plan {
            edits: plan
                .edits
                .iter()
                .map(|(r, t)| (base + r.start..base + r.end, t.clone()))
                .collect(),
            cursor: base + plan.cursor,
            closer: plan.closer,
        };
        let mut out = shifted.apply_to(text);
        out.insert(shifted.cursor, '|');
        out
    }

    #[test]
    fn brackets_pair_in_text_and_math() {
        assert_eq!(type_char("a |", '{').as_deref(), Some("a {|}"));
        assert_eq!(type_char("a |", '[').as_deref(), Some("a [|]"));
        assert_eq!(type_char("a |", '(').as_deref(), Some("a (|)"));
        assert_eq!(type_char("$x |$", '(').as_deref(), Some("$x (|)$"));
        assert_eq!(type_char("\\ref|", '{').as_deref(), Some("\\ref{|}"));
        assert_eq!(
            type_char("\\frac{a}|", '{').as_deref(),
            Some("\\frac{a}{|}")
        );
        // Before a word or a command: no pair.
        assert_eq!(type_char("a |word", '('), None);
        assert_eq!(type_char("$|\\alpha$", '{'), None);
        // The line has a closer waiting: no pair.
        assert_eq!(type_char("a | b)", '('), None);
        // In a comment and in verbatim: nothing.
        assert_eq!(type_char("% a |", '{'), None);
        assert_eq!(type_char("\\verb!x |", '{'), None);
        // Switched off.
        let off = TypingCfg {
            autopair: false,
            ..cfg()
        };
        assert_eq!(type_char_with("a |", '{', &off, false), None);
    }

    #[test]
    fn escaped_openers() {
        // `\{` pairs to `\}` in math only; `\[` and `\(` are left alone.
        assert_eq!(type_char("$\\|$", '{').as_deref(), Some("$\\{|\\}$"));
        assert_eq!(type_char("text \\|", '{'), None);
        assert_eq!(type_char("text \\|", '['), None);
        assert_eq!(type_char("text \\|", '('), None);
        // An escaped backslash before it: `\\{` is a line break and a group.
        assert_eq!(type_char("a \\\\|", '{').as_deref(), Some("a \\\\{|}"));
    }

    #[test]
    fn left_right_pairs() {
        assert_eq!(
            type_char("$\\left|$", '(').as_deref(),
            Some("$\\left(|\\right)$")
        );
        assert_eq!(
            type_char("$\\left|$", '[').as_deref(),
            Some("$\\left[|\\right]$")
        );
        assert_eq!(
            type_char("$\\left|$", '|').as_deref(),
            Some("$\\left||\\right|$")
        );
        assert_eq!(
            type_char("$\\left\\|$", '{').as_deref(),
            Some("$\\left\\{|\\right\\}$")
        );
        // Not in text, and not after a longer command.
        assert_eq!(type_char("\\left|", '|'), None);
        assert_eq!(
            type_char("$\\upleft|$", '(').as_deref(),
            Some("$\\upleft(|)$")
        );
        // `)` types over `\right)`.
        assert_eq!(
            type_char("$\\left(x|\\right)$", ')').as_deref(),
            Some("$\\left(x\\right)|$")
        );
    }

    #[test]
    fn closers_type_over() {
        assert_eq!(type_char("a {x|}", '}').as_deref(), Some("a {x}|"));
        assert_eq!(type_char("a (x|)", ')').as_deref(), Some("a (x)|"));
        assert_eq!(type_char("a [x|]", ']').as_deref(), Some("a [x]|"));
        assert_eq!(
            type_char("$\\frac{a|}{b}$", '}').as_deref(),
            Some("$\\frac{a}|{b}$")
        );
        // An unclosed opener on the line: the closer is new.
        assert_eq!(type_char("{{x|}", '}'), None);
        // Not the same closer: nothing.
        assert_eq!(type_char("a (x|]", ')'), None);
        // `\}` typed before a paired `\}`.
        assert_eq!(
            type_char("$\\{x\\|\\}$", '}').as_deref(),
            Some("$\\{x\\}|$")
        );
    }

    #[test]
    fn brackets_are_counted_over_the_paragraph() {
        // The footnote's `}` is on the next line: the `}` before the cursor is the citation's.
        assert_eq!(
            type_char("\\footnote{note \\cite{a|}\n}\n", '}').as_deref(),
            Some("\\footnote{note \\cite{a}|\n}\n")
        );
        // The footnote opened on the line above: its `}` is no closer for the citation.
        assert_eq!(
            type_char("\\footnote{As shown\nin \\cite{X|}\nNext.\n", '}'),
            None
        );
        // A blank line ends the paragraph; brackets in comments do not count.
        assert_eq!(
            type_char("\\footnote{a\n\nb {x|}\n", '}').as_deref(),
            Some("\\footnote{a\n\nb {x}|\n")
        );
        assert_eq!(
            type_char("a {x|} % {\n", '}').as_deref(),
            Some("a {x}| % {\n")
        );
        // An opener whose closer waits on another line of the paragraph is not paired.
        assert_eq!(type_char("a | b\nc)\n", '('), None);
        assert_eq!(
            type_char("a |\n\nc)\n", '(').as_deref(),
            Some("a (|)\n\nc)\n")
        );
        assert_eq!(
            type_char("\\section{A long title\non |}\n", '{').as_deref(),
            Some("\\section{A long title\non {|}}\n")
        );
        // A closer this helper inserted is typed over whatever the count.
        assert_eq!(
            type_char_with("{{x|}", '}', &cfg(), true).as_deref(),
            Some("{{x}|")
        );
        assert_eq!(
            type_char_with("$$\\bigl( a + f(y|)\n b \\bigr)$$", ')', &cfg(), true).as_deref(),
            Some("$$\\bigl( a + f(y)|\n b \\bigr)$$")
        );
    }

    #[test]
    fn backspace_keeps_closers_it_did_not_insert() {
        // A `$` typed before a formula's `$` (not paired): one `$` too many in the paragraph.
        assert_eq!(backspace("Let $|$x$ be."), None);
        // A `{` typed (not paired) before a multi-line group's `}`.
        assert_eq!(backspace("\\footnote{As shown\nin {|}\nNext."), None);
        // Unless this helper inserted that closer.
        assert_eq!(
            backspace_with("\\footnote{As shown\nin {|}}\nNext.", true).as_deref(),
            Some("\\footnote{As shown\nin |}\nNext.")
        );
        // A balanced empty pair (a snippet's): deleted.
        assert_eq!(backspace("$\\frac{|}{}$").as_deref(), Some("$\\frac|{}$"));
        assert_eq!(backspace("a $|$\nb $x$").as_deref(), Some("a |\nb $x$"));
        // `\left(` and `\{` typed before another pair's partner.
        assert_eq!(backspace("$\\left( a \\left(|\\right)$"), None);
        assert_eq!(backspace("$\\{ a \\{|\\}$"), None);
    }

    #[test]
    fn counts() {
        assert_eq!(dollars("a $x$ \\$ $$ % $"), 4);
        assert_eq!(escaped_braces("\\{ \\{ \\} \\\\{ % \\{"), 1);
        assert_eq!(
            left_right("\\left( \\right) \\left[ \\leftarrow \\\\left \\é"),
            1
        );
        assert_eq!(left_right("\\right"), -1);
        let text = StrText::new("a\nb\n\nc\nd\ne\n\nf");
        assert_eq!(paragraph_lines(&text, 4), "c\ne\n");
        assert_eq!(paragraph_lines(&text, 7), "");
        assert_eq!(paragraph_lines(&text, 0), "b\n");
    }

    #[test]
    fn env_index_follows_changes() {
        // The commands of the lines in `envs` known for `lines`, as (line, kinds and names).
        fn known(envs: &EnvIndex, lines: Range<u32>) -> Vec<(u32, String)> {
            let mut out = Vec::new();
            envs.visit(lines, false, &mut |line, indent, toks| {
                let toks: Vec<String> = toks
                    .iter()
                    .map(|t| format!("{:?}{}{}", t.kind, t.name, if t.first { "^" } else { "" }))
                    .collect();
                out.push((line, format!("{indent:?}{}", toks.join(","))));
                ControlFlow::Continue(())
            });
            out
        }
        let text = "a\n\\begin{x}\nb\n  \\item c % \\end{y}\nd \\end{x}";
        let index = LineIndex::new(text);
        let envs = EnvIndex::new(index.line_count());
        assert!(!envs.needs_fill(index.line_count()), "few unknown lines");
        envs.fill(text, &index);
        assert_eq!(envs.unknown.get(), 0);
        assert_eq!(
            known(&envs, 0..5),
            [
                (1, "\"\"Beginx^".to_owned()),
                (3, "\"  \"Item^".to_owned()),
                (4, "\"\"Endx".to_owned())
            ]
        );
        // Line 2 became three lines: they are unknown, and the lines below moved.
        envs.changed(2, 1, 3);
        assert_eq!(envs.marks.borrow().len(), 7);
        assert_eq!(envs.unknown_in(0..7), [2, 3, 4]);
        assert_eq!(
            known(&envs, 0..7)
                .iter()
                .map(|(l, _)| *l)
                .collect::<Vec<_>>(),
            [1, 5, 6]
        );
        envs.learn(3, Some("\\end{z} \\begin{z}"));
        envs.learn(2, Some("plain"));
        envs.learn(4, None);
        assert_eq!(envs.unknown_in(0..7), Vec::<u32>::new());
        assert_eq!(known(&envs, 2..4), [(3, "\"\"Endz^,Beginz".to_owned())]);
        // Lines removed: their commands go, the rest moves up.
        envs.changed(0, 4, 1);
        assert_eq!(
            known(&envs, 0..4)
                .iter()
                .map(|(l, _)| *l)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        // Backwards, stopping early.
        let mut seen = Vec::new();
        envs.visit(0..4, true, &mut |line, _, _| {
            seen.push(line);
            ControlFlow::Break(())
        });
        assert_eq!(seen, [3]);
        // Many unknown lines: classified all at once.
        let big = EnvIndex::new(1000);
        assert!(big.needs_fill(1000));
        let lines = "x\n".repeat(999) + "\\end{a}";
        big.fill(&lines, &LineIndex::new(&lines));
        assert!(!big.needs_fill(1000));
        assert_eq!(known(&big, 0..1000).len(), 1);
        // The names are shared.
        let shared = EnvIndex::new(2);
        let two = "\\begin{proof}\n\\end{proof}";
        shared.fill(two, &LineIndex::new(two));
        let mut names = Vec::new();
        shared.visit(0..2, false, &mut |_, _, toks| {
            names.push(toks[0].name.clone());
            ControlFlow::Continue(())
        });
        assert!(Rc::ptr_eq(&names[0], &names[1]));
    }

    #[test]
    fn scans_skip_lines_without_environments() {
        // A text whose lines claim to have no environment commands except those listed: the
        // scans read only those (and the cursor's line).
        struct Sparse<'a> {
            text: StrText<'a>,
            envs: Vec<u32>,
            read: RefCell<Vec<u32>>,
        }
        impl TextLines for Sparse<'_> {
            fn line_count(&self) -> u32 {
                self.text.line_count()
            }
            fn line_start(&self, i: u32) -> usize {
                self.text.line_start(i)
            }
            fn line(&self, i: u32) -> Option<Cow<'_, str>> {
                self.read.borrow_mut().push(i);
                self.text.line(i)
            }
            fn for_env_lines(&self, lines: Range<u32>, rev: bool, f: &mut EnvVisitor<'_>) {
                let mut listed: Vec<u32> = self
                    .envs
                    .iter()
                    .copied()
                    .filter(|l| lines.contains(l))
                    .collect();
                if rev {
                    listed.reverse();
                }
                for l in listed {
                    let text = self.line(l).unwrap();
                    let toks = env_toks(&text, &mut |n| Rc::from(n));
                    if f(l, leading_ws(&text), &toks).is_break() {
                        return;
                    }
                }
            }
        }
        let mut doc = String::from("\\begin{document}\n");
        for i in 0..5000 {
            doc.push_str(&format!("Line {i}.\n"));
        }
        doc.push_str("\\end{document}\n");
        let sparse = Sparse {
            text: StrText::new(&doc),
            envs: vec![0, 5001],
            read: RefCell::default(),
        };
        let no_modes = |_: usize| Mode::Text;
        assert_eq!(
            on_enter_lines(&sparse, 2500, 3, &no_modes, &cfg(), "  "),
            None
        );
        assert!(env_unclosed_at(&sparse, 2500, 0, "proof", false));
        assert!(!env_unclosed_at(&sparse, 2500, 0, "document", true));
        let read = sparse.read.borrow();
        assert!(
            read.iter().all(|&l| [0, 2500, 5001].contains(&l)),
            "read lines {read:?}"
        );
    }

    #[test]
    fn half_open_intervals() {
        assert_eq!(
            type_char_with("$[0,1|]$", ')', &cfg(), true).as_deref(),
            Some("$[0,1)|$")
        );
        assert_eq!(
            type_char_with("$(0,1|)$", ']', &cfg(), true).as_deref(),
            Some("$(0,1]|$")
        );
        // Only for a closer the helper inserted, and only in math.
        assert_eq!(type_char_with("$[0,1|]$", ')', &cfg(), false), None);
        assert_eq!(type_char_with("[0,1|]", ')', &cfg(), true), None);
    }

    #[test]
    fn smart_dollar() {
        assert_eq!(type_char("a |", '$').as_deref(), Some("a $|$"));
        assert_eq!(type_char("a $x|$ b", '$').as_deref(), Some("a $x$| b"));
        assert_eq!(type_char("a $x|", '$'), None);
        // Never after `\`, never before a word, a command or a `$`.
        assert_eq!(type_char("costs \\|", '$'), None);
        assert_eq!(type_char("a |x", '$'), None);
        assert_eq!(type_char("a |\\alpha", '$'), None);
        // In a comment: nothing.
        assert_eq!(type_char("% a |", '$'), None);
        // In `\text{…}` inside math, `$` opens math again.
        assert_eq!(
            type_char("$x \\text{if |}$", '$').as_deref(),
            Some("$x \\text{if $|$}$")
        );
        let off = TypingCfg {
            smart_dollar: false,
            ..cfg()
        };
        assert_eq!(type_char_with("a |", '$', &off, false), None);
    }

    /// The replacement that typing `c` at `|` in `doc` completes, with `cfg`.
    fn replaced(doc: &str, c: char, cfg: &TypingCfg) -> Option<(&'static str, &'static str)> {
        let cursor = doc.find('|').expect("a cursor");
        let text = doc.replacen('|', "", 1);
        let mode_at = modes(&text);
        let lines = StrText::new(&text);
        let (line, col) = lines.position(cursor);
        let at_line = LineAt::new(&lines, line, col).unwrap();
        let paragraph = Paragraph::none();
        let at = Typed {
            before: &at_line.text[..col],
            after: &at_line.text[col..at_line.len],
            mode: mode_at(cursor),
            next_auto: false,
            paragraph: &paragraph,
        };
        replacement(c, &at, cfg)
    }

    #[test]
    fn replacements() {
        let on = TypingCfg {
            replacements: true,
            ..cfg()
        };
        let dots = Some(("...", "\\dots"));
        assert_eq!(replaced("Wait..|", '.', &on), dots);
        assert_eq!(replaced("..|", '.', &on), dots);
        assert_eq!(
            replaced("a\\\\..|", '.', &on),
            dots,
            "after a line break `\\\\`"
        );
        assert_eq!(replaced("$\\text{and so on..|}$", '.', &on), dots);
        // Off by default, and only for the third dot.
        assert_eq!(replaced("Wait..|", '.', &cfg()), None);
        assert_eq!(replaced("Wait.|", '.', &on), None);
        assert_eq!(replaced("Wait..|", ',', &on), None);
        // Never in math, arguments, comments or verbatim, after `\.` or after a fourth dot.
        assert_eq!(replaced("$x_1, ..|$", '.', &on), None);
        assert_eq!(replaced("\\input{..|}", '.', &on), None);
        assert_eq!(replaced("% and so on..|", '.', &on), None);
        assert_eq!(
            replaced("\\begin{verbatim}\n..|\n\\end{verbatim}", '.', &on),
            None
        );
        assert_eq!(replaced("\\..|", '.', &on), None);
        assert_eq!(replaced("Wait...|", '.', &on), None);
    }

    #[test]
    fn backspace_deletes_empty_pairs() {
        assert_eq!(backspace("a {|}").as_deref(), Some("a |"));
        assert_eq!(backspace("a (|)").as_deref(), Some("a |"));
        assert_eq!(backspace("a [|]").as_deref(), Some("a |"));
        assert_eq!(backspace("a $|$").as_deref(), Some("a |"));
        assert_eq!(backspace("$\\{|\\}$").as_deref(), Some("$\\|$"));
        assert_eq!(
            backspace("$\\left(|\\right)$").as_deref(),
            Some("$\\left|$")
        );
        assert_eq!(
            backspace("$\\left\\{|\\right\\}$").as_deref(),
            Some("$\\left\\|$")
        );
        // Not a pair: Backspace as usual.
        assert_eq!(backspace("a {x|}"), None);
        assert_eq!(backspace("a \\{|}"), None);
        assert_eq!(backspace("a \\$|$"), None);
        assert_eq!(backspace("% {|}"), None);
    }

    #[test]
    fn end_after_begin() {
        // `}` completing `\begin{x}`: `\end{x}` below; the cursor stays for the arguments.
        assert_eq!(
            type_char("  \\begin{proof|\n", '}').as_deref(),
            Some("  \\begin{proof}|\n  \\end{proof}\n")
        );
        // Typed over a paired `}`.
        assert_eq!(
            type_char("\\begin{theorem|}\n", '}').as_deref(),
            Some("\\begin{theorem}|\n\\end{theorem}\n")
        );
        // Closed further on: nothing more.
        assert_eq!(
            type_char("\\begin{proof|}\nx\n\\end{proof}\n", '}').as_deref(),
            Some("\\begin{proof}|\nx\n\\end{proof}\n")
        );
        // A nested one of the same name is still unclosed.
        assert_eq!(
            type_char("\\begin{itemize}\n\\begin{itemize|\n\\end{itemize}\n", '}').as_deref(),
            Some("\\begin{itemize}\n\\begin{itemize}|\n\\end{itemize}\n\\end{itemize}\n")
        );
        // A renamed `\begin` (its old `\end` is left over): nothing added (the key goes on).
        assert_eq!(
            type_char("\\begin{enumerate|\n\\item a\n\\end{itemize}\n", '}'),
            None
        );
        // Inside another environment: unclosed until that one ends.
        assert_eq!(
            type_char("\\begin{proof}\n\\begin{align|\n\\end{proof}\n", '}').as_deref(),
            Some("\\begin{proof}\n\\begin{align}|\n\\end{align}\n\\end{proof}\n")
        );
        // Not a `\begin` (escaped, in a comment), or switched off.
        assert_eq!(type_char("\\\\begin{x|\n", '}'), None);
        assert_eq!(type_char("% \\begin{x|\n", '}'), None);
        let off = TypingCfg {
            auto_end: false,
            ..cfg()
        };
        assert_eq!(type_char_with("\\begin{x|\n", '}', &off, false), None);
    }

    #[test]
    fn enter_after_begin() {
        assert_eq!(
            enter("\\begin{theorem}[Main]|\n").as_deref(),
            Some("\\begin{theorem}[Main]\n  |\n\\end{theorem}\n")
        );
        assert_eq!(
            enter("  \\begin{tabular}{cc}|\n").as_deref(),
            Some("  \\begin{tabular}{cc}\n    |\n  \\end{tabular}\n")
        );
        // Closed: only the indented body line.
        assert_eq!(
            enter("\\begin{proof}|\n\\end{proof}\n").as_deref(),
            Some("\\begin{proof}\n  |\n\\end{proof}\n")
        );
        // Lists start with `\item`.
        assert_eq!(
            enter("\\begin{itemize}|\n").as_deref(),
            Some("\\begin{itemize}\n  \\item |\n\\end{itemize}\n")
        );
        // The document body is not indented.
        assert_eq!(
            enter("\\begin{document}|\n").as_deref(),
            Some("\\begin{document}\n|\n\\end{document}\n")
        );
        assert_eq!(enter("\\begin{document}|\n\\end{document}"), None);
        // Something else after the arguments: an ordinary line break.
        assert_eq!(enter("\\begin{proof} text|\n\\end{proof}\n"), None);
        assert_eq!(enter("% \\begin{proof}|\n"), None);
        // A multi-byte character before the first line break of the text.
        assert_eq!(
            enter("é\n\\begin{proof}|").as_deref(),
            Some("é\n\\begin{proof}\n  |\n\\end{proof}")
        );
        // CRLF documents get CRLF line breaks.
        assert_eq!(
            enter("\\begin{proof}|\r\n").as_deref(),
            Some("\\begin{proof}\r\n  |\r\n\\end{proof}\r\n")
        );
    }

    #[test]
    fn enter_continues_lists() {
        let doc = "\\begin{itemize}\n  \\item first|\n\\end{itemize}\n";
        assert_eq!(
            enter(doc).as_deref(),
            Some("\\begin{itemize}\n  \\item first\n  \\item |\n\\end{itemize}\n")
        );
        // In the middle of an item: the rest moves to the new item.
        let doc = "\\begin{enumerate}\n  \\item one| two\n\\end{enumerate}\n";
        assert_eq!(
            enter(doc).as_deref(),
            Some("\\begin{enumerate}\n  \\item one\n  \\item |two\n\\end{enumerate}\n")
        );
        // A continuation line of an item: the new item aligns with the one above.
        let doc = "\\begin{itemize}\n  \\item first\n    more|\n\\end{itemize}\n";
        assert_eq!(
            enter(doc).as_deref(),
            Some("\\begin{itemize}\n  \\item first\n    more\n  \\item |\n\\end{itemize}\n")
        );
        // A nested environment inside the item is skipped.
        let doc = "\\begin{description}\n  \\item[A] x\n  \\begin{proof}\n  \\end{proof}\n  done|\n\\end{description}\n";
        assert!(enter(doc).unwrap().contains("done\n  \\item |\n"));
        // Not in a list, in math, in a comment, before an `\item`, or switched off.
        assert_eq!(enter("\\begin{proof}\n  text|\n\\end{proof}\n"), None);
        assert_eq!(
            enter("\\begin{itemize}\n  \\item $x|$\n\\end{itemize}\n"),
            None
        );
        assert_eq!(
            enter("\\begin{itemize}\n  \\item x % note|\n\\end{itemize}\n"),
            None
        );
        assert_eq!(
            enter("\\begin{itemize}\n  |\\item x\n\\end{itemize}\n"),
            None
        );
        assert_eq!(
            enter("\\begin{itemize}\n\\end{itemize}\n\\begin{proof}\nx|\n\\end{proof}"),
            None
        );
        let off = TypingCfg {
            item_on_enter: false,
            ..cfg()
        };
        assert_eq!(
            enter_with("\\begin{itemize}\n  \\item a|\n\\end{itemize}\n", &off),
            None
        );
    }

    #[test]
    fn enter_on_an_empty_item_leaves_the_list() {
        let doc = "\\begin{itemize}\n  \\item a\n  \\item |\n\\end{itemize}\nNext";
        assert_eq!(
            enter(doc).as_deref(),
            Some("\\begin{itemize}\n  \\item a\n\\end{itemize}\n|\nNext")
        );
        // Nested: out of the inner list only, at its `\end`'s indentation.
        let doc = "\\begin{itemize}\n  \\item a\n  \\begin{enumerate}\n    \\item b\n    \\item |\n  \\end{enumerate}\n\\end{itemize}\n";
        assert_eq!(
            enter(doc).as_deref(),
            Some(
                "\\begin{itemize}\n  \\item a\n  \\begin{enumerate}\n    \\item b\n  \\end{enumerate}\n  |\n\\end{itemize}\n"
            )
        );
        // More items follow: the line is emptied.
        let doc = "\\begin{itemize}\n  \\item |\n  \\item b\n\\end{itemize}\n";
        assert_eq!(
            enter(doc).as_deref(),
            Some("\\begin{itemize}\n|\n  \\item b\n\\end{itemize}\n")
        );
        // Not in a list: an ordinary line break.
        assert_eq!(enter("\\item |\n"), None);
    }

    #[test]
    fn scans() {
        assert!(env_unclosed("\n", 0, "x", false));
        assert!(!env_unclosed("a\n\\end{x}", 0, "x", false));
        assert!(env_unclosed("a\n% \\end{x}\n", 0, "x", false));
        assert!(env_unclosed("\\begin{x}\\end{x}", 0, "x", false));
        assert!(!env_unclosed("\\begin{x}\\end{x}", 9, "x", true));
        let proof = "\\begin{proof}\nX\n\\end{proof}\n";
        assert!(env_unclosed(proof, 15, "align", false));
        // Only an `\end{proof}`: `\begin{align}` was renamed from `proof`.
        assert!(!env_unclosed("X\n\\end{proof}\n", 1, "align", false));
        let text = StrText::new("\\begin{a}\n\\begin{b}\\end{b}\nx");
        assert_eq!(enclosing_env(&text, 2, 1).map(|e| e.name), Some("a".into()));
        assert_eq!(enclosing_env(&StrText::new("x"), 0, 1), None);
        assert_eq!(
            env_tokens("\\item \\begin {x} \\\\begin{y} \\itemsep % \\end{z}"),
            vec![(0, Tok::Item), (6, Tok::Begin("x"))]
        );
        assert_eq!(begin_with_args("  \\begin{a}[t]{c}  "), Some((2, "a")));
        assert_eq!(begin_with_args("\\begin{a} b"), None);
        assert_eq!(begin_with_args("\\begin{a}[x={1}]"), Some((0, "a")));
        assert_eq!(begin_with_args("\\begin{a}{"), None);
        assert_eq!(strip_comment("a\\%b%c"), "a\\%b");
        assert_eq!(balance("{a}\\{ }", '{', '}'), -1);
    }
}
