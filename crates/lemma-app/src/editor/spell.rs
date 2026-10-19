//! Spell checking (WP2.9b; ARCHITECTURE §5 "Spell check", §7 lazy loading).
//!
//! # What it does
//!
//! The shell calls [`attach_after_first_frame`] once an editor's view has drawn its first frame,
//! so the spelling library and its dictionary load only after the window is on screen. While
//! checking is on, each editor gets:
//!
//! - a `libspelling::TextBufferAdapter` on its buffer with its own `libspelling::Checker` for
//!   `spell.language` (a language without an installed dictionary falls back to the default
//!   one, with a toast). Misspelled words get the adapter's wavy underline. Text tagged
//!   [`super::highlight::NO_SPELL_TAG`] by the scanner highlighter is skipped: commands, math,
//!   comments, verbatim, and the arguments of labels, references, citations, `\begin`/`\end`,
//!   packages and files; text arguments such as `\emph{…}` and `\caption{…}` are checked.
//!   libspelling rechecks a range when that tag is applied or removed. The highlighter tags only
//!   lines that have been on screen, so the rest is guarded as well (see "Text that is not
//!   prose" below);
//! - the adapter's `spelling` action group on the view, and its menu (suggestions, Add to
//!   Dictionary, Ignore, the per-document Check Spelling switch and Languages) as the **first**
//!   section of the editor's context menu. A right click moves the cursor to the clicked word
//!   (unless it is inside the selection) and fills in the suggestions; so do the Menu key and
//!   Shift+F10, for the word at the cursor (an observing key controller on the view, outside
//!   the key layer).
//!
//! `.bib` documents are not checked (keys, names and field names would all be flagged).
//!
//! # Words libspelling skips
//!
//! libspelling 0.4 never checks the last word of a range it checks (probed in WP2.9b), nor the
//! first word of the buffer. Its ranges end where a no-spell range starts, so with the scanner's
//! ranges every word directly before `}` (`\emph{word}`, `\section{Title}`), a command
//! (`word\footnote{…}`), `~` (`Lemma~\ref{…}`) or `$` would never be underlined; and after an
//! edit it clears and rechecks the edited words plus one on each side, so a misspelled word
//! right after an edit would lose its underline. `EdgeWords` checks exactly those words itself,
//! split as libspelling splits words (letters, digits and `’`; an ASCII `'` separates words),
//! with the same checker, and gives the misspelled ones the adapter's own underline tag. It looks
//! only where something changed: the words before the no-spell ranges of edited lines and of
//! lines whose tags changed (the highlighter tags lines as they come into view), the last word
//! of every range libspelling clears, the word left when the cursor moves away (like
//! libspelling, it does not underline the word being typed), and everything when checking
//! starts. It runs before the next frame, so libspelling's recheck of a range never makes such
//! an underline flicker.
//!
//! # Text that is not prose
//!
//! libspelling checks the whole buffer in the background, also lines the highlighter never
//! tagged. So in a LaTeX buffer every underline is checked against the scanner before it is
//! applied (a handler on the buffer's `apply-tag`, ahead of GTK's own): a word in a token that
//! is not text (`TokenKind::no_spell`) is never underlined, whether its line has been on screen
//! or not. That costs nothing per line, only a scan of the flagged word's line (one line's
//! tokens are kept for the next word), and it adds no tags, so underlines no longer pile up on
//! the commands of a long file. Checking starts once the scanner states exist (a large file
//! builds them on the jobs thread first). When a change moves lines in or out of math, a
//! comment or verbatim without editing them (an `\end{align}` deleted above them), the lines
//! that are not tagged and not in the highlighter's window are handed to libspelling again (the
//! no-spell tag applied and removed over them, which makes it recheck them); tagged lines get
//! the same from the highlighter's own retagging. While GTK's lines and the document's differ (a
//! lone `\r`), positions are not cheap to map and underlines are not filtered.
//!
//! # The user's word lists
//!
//! Every file in `spell.word_lists` (`~` expanded; none by default; read only, never written)
//! is read on a worker thread. Each line is split on blanks and hyphens (`Barr-Beck` → `Barr`,
//! `Beck`; `Mac Lane` → `Mac`, `Lane`), parts of one letter are dropped, and capitalised parts
//! also get their `'s` form (`Grothendieck's`), as a Neovim spell file built from such a list
//! does, and their `’s` form (libspelling reads `Grothendieck’s` as one word, `Grothendieck's`
//! as two); see [`list_words`].
//! The words are ignored through the checker (enchant's session list of that language, shared by
//! every document; enchant accepts an all-lowercase word in any capitalisation and a
//! capitalised one in capitals too, as Vim does). A document starts being checked once the
//! lists are loaded. Lists that change (or new lists) are read again on every config reload, when
//! another language is chosen and when another document starts being checked; whenever that
//! adds words, every document checked in that language (in every window) is checked again.
//! Words removed from a list stay accepted until Lemma restarts (enchant cannot forget a session
//! word).
//!
//! # Switching it on and off
//!
//! `win.toggle-spell` ("Check Spelling" in the main menu's view section, added with the first
//! editor) switches checking for the whole window; it starts as `spell.enabled`, and a config
//! reload that changes `spell.enabled` sets it again. Switching off removes every adapter (and
//! its underlines and menu section); nothing of libspelling is loaded while it is off. A reload
//! that changes `spell.language` re-creates the adapters for the new language; any other reload
//! reads the word lists that changed (or are new) and rechecks.
//!
//! [`detach`] removes everything from an editor when its tab closes (the buffer may live on as a
//! closed document's text); the window's `destroy` does the same for all of its editors.
//! Tests use [`adapter`], [`misspelled`], [`is_checking`], [`language`],
//! [`prepare_context_menu`] and [`menu_key_pressed`].

use super::highlight::{self, NO_SPELL_TAG};
use super::{Editor, TexView};
use crate::document::{Document, ObserverId};
use crate::hooks::HookId;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::gdk;
use gtk::prelude::*;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime};

/// The window action that switches spell checking.
pub const TOGGLE_ACTION: &str = "toggle-spell";
/// Its label in the main menu.
const TOGGLE_LABEL: &str = "Check Spelling";
/// Word lists larger than this are not read.
const MAX_LIST_BYTES: u64 = 16 << 20;

// ───────────── Word lists (pure) ─────────────

/// The words a word list makes acceptable, in order and without duplicates: each line split on
/// blanks and hyphens, parts of one character dropped, and the `'s` form added for parts that
/// start with an upper-case letter (and the `’s` form: libspelling reads `’` as part of a word,
/// while an ASCII `'` ends one).
pub fn list_words(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |word: String, out: &mut Vec<String>| {
        if seen.insert(word.clone()) {
            out.push(word);
        }
    };
    for part in text.split(|c: char| c.is_whitespace() || c == '-') {
        let mut chars = part.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if chars.next().is_none() {
            continue;
        }
        add(part.to_owned(), &mut out);
        if first.is_uppercase() {
            add(format!("{part}'s"), &mut out);
            add(format!("{part}’s"), &mut out);
        }
    }
    out
}

/// A list file as last read: its size and modification time.
type Stamp = (u64, Option<SystemTime>);

/// The stamp of `path`, `None` when it cannot be read.
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| (meta.len(), meta.modified().ok()))
}

/// Reads the lists whose stamp differs from `known` (worker thread): `(path, stamp, words)`.
fn read_lists(lists: Vec<(PathBuf, Option<Stamp>)>) -> Vec<(PathBuf, Stamp, Vec<String>)> {
    let mut out = Vec::new();
    for (path, known) in lists {
        let Some(now) = stamp(&path) else {
            tracing::debug!("spell: no word list at {}", path.display());
            continue;
        };
        if known == Some(now) {
            continue;
        }
        if now.0 > MAX_LIST_BYTES {
            tracing::warn!(
                "spell: {} is too large to read as a word list",
                path.display()
            );
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => out.push((path, now, list_words(&String::from_utf8_lossy(&bytes)))),
            Err(e) => tracing::warn!("spell: cannot read {}: {e}", path.display()),
        }
    }
    out
}

thread_local! {
    /// Per language: the lists whose words the dictionary of that language ignores, with the
    /// stamp they had then.
    static LOADED: RefCell<HashMap<String, HashMap<PathBuf, Stamp>>> =
        RefCell::new(HashMap::new());
}

/// Makes `checker` (of `language`) ignore the words of `lists` that it does not ignore yet
/// (reading changed lists on a worker thread). Resolves to the number of words added.
async fn load_lists(checker: libspelling::Checker, language: String, lists: Vec<PathBuf>) -> usize {
    let wanted: Vec<(PathBuf, Option<Stamp>)> = LOADED.with_borrow(|loaded| {
        let known = loaded.get(&language);
        lists
            .iter()
            .map(|p| (p.clone(), known.and_then(|k| k.get(p)).copied()))
            .collect()
    });
    let read = gio::spawn_blocking(move || read_lists(wanted))
        .await
        .unwrap_or_default();
    let mut added = 0;
    for (path, stamp, words) in read {
        for word in &words {
            checker.ignore_word(word);
        }
        added += words.len();
        LOADED.with_borrow_mut(|loaded| {
            loaded
                .entry(language.clone())
                .or_default()
                .insert(path, stamp);
        });
    }
    added
}

/// [`load_lists`], then, when that added words, [`recheck_language`].
async fn load_and_recheck(checker: libspelling::Checker, language: String, lists: Vec<PathBuf>) {
    if load_lists(checker, language.clone(), lists).await > 0 {
        recheck_language(&language);
    }
}

/// Checks again every document (of every window) that is checked in `language`: its words
/// changed.
fn recheck_language(language: &str) {
    let adapters: Vec<libspelling::TextBufferAdapter> = WINDOWS.with_borrow(|windows| {
        windows
            .iter()
            .flat_map(|w| w.live_editors())
            .filter_map(|entry| {
                let checking = entry.checking.borrow();
                let adapter = &checking.as_ref()?.adapter;
                (adapter.is_enabled() && adapter.language().as_deref() == Some(language))
                    .then(|| adapter.clone())
            })
            .collect()
    });
    for adapter in adapters {
        adapter.invalidate_all();
    }
}

// ───────────── Checkers ─────────────

/// The spelling provider (enchant), created on first use.
fn provider() -> libspelling::Provider {
    libspelling::Provider::default()
}

/// `wanted` when the provider has a dictionary for it, else the provider's default language
/// (with a message for the user); `None` when no dictionary is installed at all.
fn resolve_language(wanted: &str) -> Option<(String, Option<String>)> {
    let provider = provider();
    if !wanted.is_empty() && provider.supports_language(wanted) {
        return Some((wanted.to_owned(), None));
    }
    let fallback = provider
        .default_code()
        .map(|c| c.to_string())
        .filter(|c| provider.supports_language(c))?;
    let message =
        format!("Spell checking: no dictionary for “{wanted}” is installed; using {fallback}");
    Some((fallback, Some(message)))
}

/// Characters of a word as libspelling splits words: letters, digits and `’` (an ASCII `'` ends
/// a word there).
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '’'
}

/// The `spell.word_lists` of `state`'s configuration, `~` expanded.
fn configured_lists(state: &WindowState) -> Vec<PathBuf> {
    let home = state
        .paths
        .as_ref()
        .map(|p| p.home.clone())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    state
        .config
        .spell
        .word_lists
        .iter()
        .map(|p| match &home {
            Some(home) => lemma_config::expand_tilde(p, home),
            None => p.clone(),
        })
        .collect()
}

// ───────────── Per editor ─────────────

/// Spell checking of one editor while it is on.
struct Checking {
    adapter: libspelling::TextBufferAdapter,
    menu: gio::MenuModel,
    gesture: gtk::GestureClick,
    menu_key: gtk::EventControllerKey,
    language_handler: Option<glib::SignalHandlerId>,
    enabled_handler: Option<glib::SignalHandlerId>,
    edges: Option<Rc<EdgeWords>>,
    filter: Option<Rc<ProseFilter>>,
}

/// Checks the words libspelling skips (see the module docs, "Words libspelling skips").
struct EdgeWords {
    buffer: sourceview5::Buffer,
    adapter: glib::WeakRef<libspelling::TextBufferAdapter>,
    /// The adapter's underline tag.
    tag: gtk::TextTag,
    /// The highlighter's no-spell tag, once it exists.
    no_spell: RefCell<Option<gtk::TextTag>>,
    /// GTK lines `first..=last` to look at.
    dirty: Cell<Option<(i32, i32)>>,
    idle: RefCell<Option<glib::SourceId>>,
    /// True while this pass changes tags (its own signals are not news).
    busy: Cell<bool>,
    /// Where words end that must be checked: the ends of the ranges libspelling cleared.
    ends: RefCell<Vec<gtk::TextMark>>,
    /// The end of the word left alone because the cursor was in it.
    skipped: RefCell<Option<gtk::TextMark>>,
    handlers: RefCell<Vec<glib::SignalHandlerId>>,
}

impl EdgeWords {
    fn new(
        buffer: &sourceview5::Buffer,
        adapter: &libspelling::TextBufferAdapter,
    ) -> Option<Rc<Self>> {
        let tag = adapter.tag()?;
        let edges = Rc::new(Self {
            buffer: buffer.clone(),
            adapter: adapter.downgrade(),
            tag,
            no_spell: RefCell::new(None),
            dirty: Cell::new(None),
            idle: RefCell::new(None),
            busy: Cell::new(false),
            ends: RefCell::default(),
            skipped: RefCell::new(None),
            handlers: RefCell::default(),
        });
        let mut handlers = Vec::new();
        let weak = Rc::downgrade(&edges);
        handlers.push(buffer.connect_insert_text(move |_, at, text| {
            if let Some(edges) = weak.upgrade() {
                let lines = i32::try_from(text.matches('\n').count()).unwrap_or(i32::MAX);
                edges.mark(at.line(), at.line().saturating_add(lines));
            }
        }));
        let weak = Rc::downgrade(&edges);
        handlers.push(buffer.connect_delete_range(move |_, start, end| {
            if let Some(edges) = weak.upgrade() {
                edges.mark(start.line(), end.line());
            }
        }));
        let weak = Rc::downgrade(&edges);
        handlers.push(buffer.connect_apply_tag(move |_, tag, start, end| {
            if let Some(edges) = weak.upgrade() {
                edges.tag_changed(tag, start, end);
            }
        }));
        let weak = Rc::downgrade(&edges);
        handlers.push(buffer.connect_remove_tag(move |buffer, tag, start, end| {
            let Some(edges) = weak.upgrade() else {
                return;
            };
            if *tag == edges.tag && !edges.busy.get() {
                // libspelling clears a range before checking it again, and skips its last word.
                let mark = buffer.create_mark(None, end, true);
                edges.ends.borrow_mut().push(mark);
            }
            edges.tag_changed(tag, start, end);
        }));
        let weak = Rc::downgrade(&edges);
        handlers.push(buffer.connect_cursor_position_notify(move |buffer| {
            let Some(edges) = weak.upgrade() else {
                return;
            };
            let skipped = edges.skipped.borrow().clone();
            if let Some(mark) = skipped {
                let (start, end) = edges.word_around(&buffer.iter_at_mark(&mark));
                let cursor = buffer.cursor_position();
                if !(start.offset()..=end.offset()).contains(&cursor) {
                    edges.skipped.replace(None);
                    let line = start.line();
                    edges.ends.borrow_mut().push(mark);
                    edges.mark(line, line);
                }
            }
        }));
        edges.handlers.replace(handlers);
        Some(edges)
    }

    fn tag_changed(
        self: &Rc<Self>,
        tag: &gtk::TextTag,
        start: &gtk::TextIter,
        end: &gtk::TextIter,
    ) {
        if self.busy.get() {
            return;
        }
        let ours = *tag == self.tag || self.no_spell().is_some_and(|n| n == *tag);
        if ours {
            self.mark(start.line(), end.line());
        }
    }

    /// The highlighter's no-spell tag (looked up once it exists).
    fn no_spell(&self) -> Option<gtk::TextTag> {
        if self.no_spell.borrow().is_none() {
            let found = self.buffer.tag_table().lookup(NO_SPELL_TAG);
            self.no_spell.replace(found);
        }
        self.no_spell.borrow().clone()
    }

    /// Looks at lines `first..=last` before the next frame.
    fn mark(self: &Rc<Self>, first: i32, last: i32) {
        let (first, last) = match self.dirty.get() {
            Some((a, b)) => (a.min(first), b.max(last)),
            None => (first, last),
        };
        self.dirty.set(Some((first, last)));
        if self.idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(edges) = weak.upgrade() {
                edges.idle.take();
                edges.run();
            }
            glib::ControlFlow::Break
        });
        self.idle.replace(Some(id));
    }

    /// Looks at the whole buffer.
    fn mark_all(self: &Rc<Self>) {
        self.mark(0, self.buffer.line_count());
    }

    /// Checks the words right before the no-spell ranges that start on the dirty lines.
    fn run(&self) {
        let Some((first, last)) = self.dirty.take() else {
            return;
        };
        if !self.adapter.upgrade().is_some_and(|a| a.is_enabled()) {
            self.forget_ends();
            return;
        }
        let Some(checker) = self.adapter.upgrade().and_then(|a| a.checker()) else {
            self.forget_ends();
            return;
        };
        let no_spell = self.no_spell();
        let lines = self.buffer.line_count();
        let mut it = self
            .buffer
            .iter_at_line(first.clamp(0, lines - 1))
            .unwrap_or_else(|| self.buffer.start_iter());
        let end = if last + 1 < lines {
            self.buffer
                .iter_at_line(last + 1)
                .unwrap_or_else(|| self.buffer.end_iter())
        } else {
            self.buffer.end_iter()
        };
        let cursor = self.buffer.cursor_position();
        self.busy.set(true);
        if first <= 0 {
            self.check_first(no_spell.as_ref(), &checker, cursor);
        }
        for mark in self.ends.take() {
            if !mark.is_deleted() {
                let (start, end) = self.word_around(&self.buffer.iter_at_mark(&mark));
                self.buffer.delete_mark(&mark);
                self.check_range(&start, &end, &checker, cursor);
            }
        }
        let Some(no_spell) = no_spell else {
            self.busy.set(false);
            return;
        };
        while it < end {
            if !it.starts_tag(Some(&no_spell)) {
                if !it.forward_to_tag_toggle(Some(&no_spell)) {
                    break;
                }
                continue;
            }
            self.check_before(&it, &no_spell, &checker, cursor);
            if !it.forward_to_tag_toggle(Some(&no_spell)) {
                break;
            }
        }
        self.busy.set(false);
    }

    /// The word around `at` (touching it on either side): letters, digits and `’`, without
    /// no-spell text. Empty (`start == end`) when there is none.
    fn word_around(&self, at: &gtk::TextIter) -> (gtk::TextIter, gtk::TextIter) {
        let no_spell = self.no_spell();
        let in_word = |i: &gtk::TextIter| {
            word_char(i.char()) && !no_spell.as_ref().is_some_and(|t| i.has_tag(t))
        };
        let mut start = *at;
        loop {
            let mut prev = start;
            if !prev.backward_char() || !in_word(&prev) {
                break;
            }
            start = prev;
        }
        let mut end = *at;
        while !end.is_end() && in_word(&end) {
            end.forward_char();
        }
        (start, end)
    }

    /// Drops the pending range ends.
    fn forget_ends(&self) {
        for mark in self.ends.take() {
            if !mark.is_deleted() {
                self.buffer.delete_mark(&mark);
            }
        }
    }

    /// Checks the word that ends at `run` (where a no-spell range starts), if any.
    fn check_before(
        &self,
        run: &gtk::TextIter,
        no_spell: &gtk::TextTag,
        checker: &libspelling::Checker,
        cursor: i32,
    ) {
        let end = *run;
        let mut start = end;
        loop {
            let mut prev = start;
            if !prev.backward_char() || prev.has_tag(no_spell) || !word_char(prev.char()) {
                break;
            }
            start = prev;
        }
        self.check_range(&start, &end, checker, cursor);
    }

    /// Checks the first word of the buffer.
    fn check_first(
        &self,
        no_spell: Option<&gtk::TextTag>,
        checker: &libspelling::Checker,
        cursor: i32,
    ) {
        let start = self.buffer.start_iter();
        let mut end = start;
        while !end.is_end()
            && word_char(end.char())
            && !no_spell.is_some_and(|tag| end.has_tag(tag))
        {
            end.forward_char();
        }
        self.check_range(&start, &end, checker, cursor);
    }

    /// Underlines `start..end` (one word) when it is misspelled, and removes the underline when
    /// it is not. Words of one character or with digits are left alone, and so is the word at
    /// the cursor (until the cursor moves away).
    fn check_range(
        &self,
        start: &gtk::TextIter,
        end: &gtk::TextIter,
        checker: &libspelling::Checker,
        cursor: i32,
    ) {
        // A `’` only inside a word.
        let mut start = *start;
        let mut end = *end;
        while start < end && start.char() == '’' {
            start.forward_char();
        }
        while start < end && {
            let mut last = end;
            last.backward_char();
            last.char() == '’'
        } {
            end.backward_char();
        }
        if end.offset() - start.offset() < 2 {
            return;
        }
        if (start.offset()..=end.offset()).contains(&cursor) {
            // Being typed: left alone until the cursor moves away, as libspelling does.
            let mark = self.buffer.create_mark(None, &end, true);
            if let Some(old) = self.skipped.replace(Some(mark))
                && !old.is_deleted()
            {
                self.buffer.delete_mark(&old);
            }
            return;
        }
        let word = self.buffer.text(&start, &end, false);
        let misspelled =
            !word.chars().any(|c| c.is_numeric()) && !checker.check_word(word.as_str());
        let mut toggle = start;
        let fully = start.has_tag(&self.tag)
            && (!toggle.forward_to_tag_toggle(Some(&self.tag)) || toggle >= end);
        let partly = fully || {
            let mut t = start;
            t.forward_to_tag_toggle(Some(&self.tag)) && t < end
        };
        if misspelled && !fully {
            self.buffer.apply_tag(&self.tag, &start, &end);
        } else if !misspelled && partly {
            self.buffer.remove_tag(&self.tag, &start, &end);
        }
    }

    fn shutdown(&self) {
        for id in self.handlers.take() {
            self.buffer.disconnect(id);
        }
        if let Some(id) = self.idle.take() {
            id.remove();
        }
        self.forget_ends();
        if let Some(mark) = self.skipped.take()
            && !mark.is_deleted()
        {
            self.buffer.delete_mark(&mark);
        }
    }
}

/// Keeps the underline off text that is not prose (see the module docs, "Text that is not
/// prose").
struct ProseFilter {
    editor: Weak<Editor>,
    doc: Weak<Document>,
    buffer: sourceview5::Buffer,
    /// The adapter's underline tag.
    tag: gtk::TextTag,
    /// The tokens of the line last asked about: (document version, line, tokens).
    tokens: RefCell<(u64, u32, Vec<lemma_syntax::Token>)>,
    /// Lines whose scanner states changed since the last recheck (`start..end`, kept in step
    /// with later changes).
    pending: Cell<Option<(u32, u32)>>,
    idle: RefCell<Option<glib::SourceId>>,
    handler: RefCell<Option<glib::SignalHandlerId>>,
    observers: RefCell<Vec<ObserverId>>,
}

impl ProseFilter {
    /// The filter of `editor`'s buffer (LaTeX buffers only: others have no scanner tokens).
    fn new(editor: &Rc<Editor>, tag: gtk::TextTag) -> Option<Rc<Self>> {
        highlight::highlighter(editor)?;
        let doc = editor.document();
        let filter = Rc::new(Self {
            editor: Rc::downgrade(editor),
            doc: Rc::downgrade(doc),
            buffer: editor.buffer().clone(),
            tag,
            tokens: RefCell::new((u64::MAX, 0, Vec::new())),
            pending: Cell::new(None),
            idle: RefCell::new(None),
            handler: RefCell::new(None),
            observers: RefCell::default(),
        });
        let weak = Rc::downgrade(&filter);
        let handler = filter
            .buffer
            .connect_apply_tag(move |buffer, tag, start, end| {
                if let Some(filter) = weak.upgrade()
                    && *tag == filter.tag
                    && !filter.is_prose(start, end)
                {
                    // Before GTK's own handler applies it.
                    buffer.stop_signal_emission_by_name("apply-tag");
                }
            });
        filter.handler.replace(Some(handler));
        let mut observers = Vec::new();
        let weak = Rc::downgrade(&filter);
        observers.push(doc.connect_lines_changed(move |first, removed, added| {
            if let Some(filter) = weak.upgrade() {
                filter.lines_changed(first, removed, added);
            }
        }));
        let weak = Rc::downgrade(&filter);
        observers.push(doc.connect_retag(move |lines| {
            if let Some(filter) = weak.upgrade() {
                filter.retagged(lines);
            }
        }));
        filter.observers.replace(observers);
        Some(filter)
    }

    /// Whether `start..end` (a word) is prose for the scanner: no part of it is in a token that is
    /// not text. True when that cannot be told cheaply (no scanner states yet, or GTK's lines
    /// are not the document's).
    fn is_prose(&self, start: &gtk::TextIter, end: &gtk::TextIter) -> bool {
        let Some(doc) = self.doc.upgrade() else {
            return true;
        };
        let Ok(states) = doc.line_states.try_borrow() else {
            return true;
        };
        let Some(states) = states.as_ref() else {
            return true;
        };
        let count = doc.line_index().line_count();
        if u32::try_from(self.buffer.line_count()).ok() != Some(count) {
            return true;
        }
        let (first, last) = (start.line().max(0) as u32, end.line().max(0) as u32);
        for line in first..=last.min(count.saturating_sub(1)) {
            let from = if line == first {
                start.line_index().max(0) as u32
            } else {
                0
            };
            let to = if line == last {
                end.line_index().max(0) as u32
            } else {
                u32::MAX
            };
            let mut cached = self.tokens.borrow_mut();
            let (version, at, tokens) = &mut *cached;
            if *version != doc.version() || *at != line {
                let Some(text) = doc.line_text(line) else {
                    return true;
                };
                tokens.clear();
                states.tokens(line, &text, tokens);
                (*version, *at) = (doc.version(), line);
            }
            if tokens
                .iter()
                .any(|t| t.kind.no_spell() && t.start < to && from < t.end)
            {
                return false;
            }
        }
        true
    }

    /// After a change: the pending lines follow the text.
    fn lines_changed(&self, first: u32, removed: u32, added: u32) {
        let Some((start, end)) = self.pending.get() else {
            return;
        };
        let map = |line: u32, past: u32| {
            if line >= first + removed {
                line - removed + added
            } else if line > first {
                past
            } else {
                line
            }
        };
        self.pending
            .set(Some((map(start, first), map(end, first + added))));
    }

    /// The scanner states of `lines` changed: recheck them (see the module docs) before the
    /// next frame.
    fn retagged(self: &Rc<Self>, lines: Range<u32>) {
        let range = match self.pending.get() {
            Some((start, end)) => (start.min(lines.start), end.max(lines.end)),
            None => (lines.start, lines.end),
        };
        self.pending.set(Some(range));
        if self.idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
            if let Some(filter) = weak.upgrade() {
                filter.idle.take();
                filter.recheck();
            }
            glib::ControlFlow::Break
        });
        self.idle.replace(Some(id));
    }

    /// Hands the pending lines that the highlighter does not keep tagged to libspelling again.
    fn recheck(&self) {
        let Some((start, end)) = self.pending.take() else {
            return;
        };
        let (Some(editor), Some(doc)) = (self.editor.upgrade(), self.doc.upgrade()) else {
            return;
        };
        let Some(highlighter) = highlight::highlighter(&editor) else {
            return;
        };
        let Some(no_spell) = self.buffer.tag_table().lookup(NO_SPELL_TAG) else {
            return;
        };
        let count = doc.line_index().line_count();
        if u32::try_from(self.buffer.line_count()).ok() != Some(count) {
            return;
        }
        let window = highlighter.window().map_or(0..0, |w| w.tagged);
        let end = end.min(count);
        let mut line = start.min(end);
        while line < end {
            // A run of lines the highlighter leaves alone.
            let skip = |l: u32| window.contains(&l) || highlighter.is_current(l);
            if skip(line) {
                line += 1;
                continue;
            }
            let run = line;
            while line < end && !skip(line) {
                line += 1;
            }
            let Some(from) = self.buffer.iter_at_line(run as i32) else {
                break;
            };
            let to = self
                .buffer
                .iter_at_line(line as i32)
                .filter(|_| line < count)
                .unwrap_or_else(|| self.buffer.end_iter());
            // libspelling checks a range again when the no-spell tag changes on it.
            self.buffer.apply_tag(&no_spell, &from, &to);
            self.buffer.remove_tag(&no_spell, &from, &to);
        }
    }

    fn shutdown(&self) {
        if let Some(id) = self.handler.take() {
            self.buffer.disconnect(id);
        }
        if let Some(id) = self.idle.take() {
            id.remove();
        }
        if let Some(doc) = self.doc.upgrade() {
            for id in self.observers.take() {
                doc.disconnect(id);
            }
        }
    }
}

/// One editor of a window.
struct EditorSpell {
    editor: Weak<Editor>,
    view: glib::WeakRef<TexView>,
    checking: RefCell<Option<Checking>>,
    /// Bumped whenever checking is (re)created or removed, so a word-list load that finishes
    /// late does not switch on an adapter that was replaced meanwhile.
    generation: Cell<u64>,
}

/// The spell-checking state of one window.
struct WindowSpell {
    window: Weak<WindowState>,
    action: gio::SimpleAction,
    /// Whether checking is on (the action's state).
    enabled: Cell<bool>,
    /// The `[spell]` values last applied, to tell what a reload changed.
    applied: RefCell<lemma_config::SpellCfg>,
    /// Languages already reported as missing.
    reported: RefCell<HashSet<String>>,
    editors: RefCell<Vec<Rc<EditorSpell>>>,
    hook: Cell<Option<HookId>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<WindowSpell>>> = const { RefCell::new(Vec::new()) };
}

fn window_spell(state: &WindowState) -> Option<Rc<WindowSpell>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.window.strong_count() > 0);
        windows
            .iter()
            .find(|w| std::ptr::eq(w.window.as_ptr(), state))
            .cloned()
    })
}

/// The window's spell state, created (with its action, menu item and reload hook) on first use.
fn window_spell_for(state: &Rc<WindowState>) -> Rc<WindowSpell> {
    if let Some(ws) = window_spell(state) {
        return ws;
    }
    let enabled = state.config.spell.enabled;
    let action = gio::SimpleAction::new_stateful(TOGGLE_ACTION, None, &enabled.to_variant());
    let ws = Rc::new(WindowSpell {
        window: Rc::downgrade(state),
        action: action.clone(),
        enabled: Cell::new(enabled),
        applied: RefCell::new(state.config.spell.clone()),
        reported: RefCell::default(),
        editors: RefCell::default(),
        hook: Cell::new(None),
    });
    let weak = Rc::downgrade(&ws);
    action.connect_activate(move |_, _| {
        if let Some(ws) = weak.upgrade() {
            ws.set_enabled(!ws.enabled.get());
        }
    });
    let weak = Rc::downgrade(&ws);
    action.connect_change_state(move |_, value| {
        if let (Some(ws), Some(on)) = (weak.upgrade(), value.and_then(|v| v.get::<bool>())) {
            ws.set_enabled(on);
        }
    });
    state.window.add_action(&action);
    state
        .header
        .menu_view
        .append(Some(TOGGLE_LABEL), Some(&format!("win.{TOGGLE_ACTION}")));
    ws.hook
        .set(Some(state.connect_config_changed(|state, file| {
            if file == ConfigFile::Config
                && let Some(ws) = window_spell(state)
            {
                ws.config_changed(state);
            }
        })));
    let weak = Rc::downgrade(&ws);
    state.window.connect_destroy(move |_| {
        if let Some(ws) = weak.upgrade() {
            ws.shutdown();
        }
    });
    WINDOWS.with_borrow_mut(|windows| windows.push(ws.clone()));
    ws
}

/// Starts spell checking in `editor` (see the module docs).
pub fn attach_after_first_frame(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if is_bib(editor) {
        return;
    }
    let ws = window_spell_for(state);
    if ws.find(editor).is_some() {
        return;
    }
    let entry = Rc::new(EditorSpell {
        editor: Rc::downgrade(editor),
        view: editor.view().downgrade(),
        checking: RefCell::new(None),
        generation: Cell::new(0),
    });
    ws.editors.borrow_mut().push(entry.clone());
    if ws.enabled.get() {
        ws.start(state, &entry);
    }
}

/// Stops it (see the module docs). Safe when [`attach_after_first_frame`] never ran.
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(ws) = window_spell(state) else {
        return;
    };
    let removed = {
        let mut editors = ws.editors.borrow_mut();
        let at = editors
            .iter()
            .position(|e| std::ptr::eq(e.editor.as_ptr(), Rc::as_ptr(editor)));
        at.map(|i| editors.remove(i))
    };
    if let Some(entry) = removed {
        stop(&entry, Some(editor));
    }
}

/// Whether `editor` shows a BibTeX file.
fn is_bib(editor: &Editor) -> bool {
    let bib_language = editor
        .buffer()
        .language()
        .is_some_and(|l| l.id() == "bibtex");
    bib_language
        || editor
            .document()
            .path()
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("bib"))
}

impl WindowSpell {
    fn find(&self, editor: &Editor) -> Option<Rc<EditorSpell>> {
        self.editors
            .borrow()
            .iter()
            .find(|e| std::ptr::eq(e.editor.as_ptr(), editor))
            .cloned()
    }

    /// The editors that are still open.
    fn live_editors(&self) -> Vec<Rc<EditorSpell>> {
        self.editors
            .borrow_mut()
            .retain(|e| e.editor.strong_count() > 0);
        self.editors.borrow().clone()
    }

    /// Switches checking on or off in every editor of the window.
    fn set_enabled(&self, on: bool) {
        self.action.set_state(&on.to_variant());
        if self.enabled.replace(on) == on {
            return;
        }
        let Some(state) = self.window.upgrade() else {
            return;
        };
        for entry in self.live_editors() {
            if on {
                self.start(&state, &entry);
            } else {
                stop(&entry, None);
            }
        }
    }

    /// Applies what a config reload changed in `[spell]`.
    fn config_changed(&self, state: &Rc<WindowState>) {
        let new = state.config.spell.clone();
        let old = self.applied.replace(new.clone());
        if new.enabled != old.enabled {
            self.set_enabled(new.enabled);
        }
        if !self.enabled.get() {
            return;
        }
        if new.language != old.language {
            // New checkers for the new language.
            for entry in self.live_editors() {
                stop(&entry, None);
                self.start(state, &entry);
            }
        } else {
            // The lists may have changed on disk, or their paths: read what changed, once per
            // language in use.
            let lists = configured_lists(state);
            let mut languages = HashSet::new();
            for entry in self.live_editors() {
                let checker = {
                    let checking = entry.checking.borrow();
                    let Some(checking) = checking.as_ref() else {
                        continue;
                    };
                    checking.adapter.checker()
                };
                let Some(checker) = checker else {
                    continue;
                };
                let language = checker
                    .language()
                    .map(|l| l.to_string())
                    .unwrap_or_default();
                if languages.insert(language.clone()) {
                    glib::spawn_future_local(load_and_recheck(checker, language, lists.clone()));
                }
            }
        }
    }

    /// Creates the adapter of `entry` (see the module docs); it starts checking once the word
    /// lists are loaded.
    fn start(&self, state: &Rc<WindowState>, entry: &Rc<EditorSpell>) {
        let (Some(editor), Some(view)) = (entry.editor.upgrade(), entry.view.upgrade()) else {
            return;
        };
        if entry.checking.borrow().is_some() {
            return;
        }
        let Some((language, message)) = resolve_language(&state.config.spell.language) else {
            if self.reported.borrow_mut().insert(String::new()) {
                state.toast("Spell checking: no dictionary is installed");
            }
            return;
        };
        if let Some(message) = message
            && self
                .reported
                .borrow_mut()
                .insert(state.config.spell.language.clone())
        {
            tracing::warn!("{message}");
            state.toast(&message);
        }
        let checker = libspelling::Checker::new(Some(&provider()), Some(&language));
        let buffer = editor.buffer();
        let adapter = libspelling::TextBufferAdapter::new(buffer, &checker);
        adapter.set_enabled(false);
        let menu = adapter.menu_model();
        view.insert_action_group("spelling", Some(&adapter));
        editor.extra_menu().insert_section(0, None, &menu);

        // A right click: the clicked word's suggestions.
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&editor);
        gesture.connect_pressed(move |_, _, x, y| {
            if let Some(editor) = weak.upgrade() {
                let view = editor.view();
                let (bx, by) =
                    view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
                let at = view.iter_at_location(bx, by);
                prepare_context_menu(&editor, at.as_ref());
            }
        });
        view.add_controller(gesture.clone());
        // The Menu key and Shift+F10: the suggestions for the word at the cursor. An observing
        // controller on the view (it never stops a key, so its place among the view's
        // controllers does not matter, and the key layer stays free of it).
        let menu_key = gtk::EventControllerKey::new();
        menu_key.set_name(Some("lemma-spell-menu-key"));
        menu_key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&editor);
        menu_key.connect_key_pressed(move |_, keyval, _, state| {
            if let Some(editor) = weak.upgrade() {
                menu_key_pressed(&editor, keyval, state);
            }
            glib::Propagation::Proceed
        });
        view.add_controller(menu_key.clone());
        // Another language from the menu: load the word lists (as configured now) for it too.
        let lists = configured_lists(state);
        let language_handler = {
            let window = Rc::downgrade(state);
            adapter.connect_language_notify(move |adapter| {
                let (Some(checker), Some(language), Some(state)) =
                    (adapter.checker(), adapter.language(), window.upgrade())
                else {
                    return;
                };
                glib::spawn_future_local(load_and_recheck(
                    checker,
                    language.to_string(),
                    configured_lists(&state),
                ));
            })
        };
        // Before EdgeWords and libspelling apply anything.
        let filter = adapter.tag().and_then(|tag| ProseFilter::new(&editor, tag));
        let edges = EdgeWords::new(buffer, &adapter);
        // Checking switched on (also from the menu's own Check Spelling): look at everything.
        let enabled_handler = edges.as_ref().map(|edges| {
            let weak = Rc::downgrade(edges);
            adapter.connect_enabled_notify(move |adapter| {
                if let Some(edges) = weak.upgrade()
                    && adapter.is_enabled()
                {
                    edges.mark_all();
                }
            })
        });
        let generation = entry.generation.get() + 1;
        entry.generation.set(generation);
        entry.checking.replace(Some(Checking {
            adapter: adapter.clone(),
            menu,
            gesture,
            menu_key,
            language_handler: Some(language_handler),
            enabled_handler,
            edges,
            filter,
        }));
        let weak = Rc::downgrade(entry);
        let doc = Rc::downgrade(editor.document());
        glib::spawn_future_local(async move {
            if load_lists(checker, language.clone(), lists).await > 0 {
                // Words other documents of the language did not accept so far.
                recheck_language(&language);
            }
            let current = || {
                weak.upgrade().is_some_and(|entry| {
                    entry.generation.get() == generation && entry.checking.borrow().is_some()
                })
            };
            // The scanner states first (a large file builds them on the jobs thread): without
            // them, text that is not prose cannot be told.
            while current()
                && doc
                    .upgrade()
                    .is_some_and(|d| d.is_open() && !d.has_line_states())
            {
                glib::timeout_future(Duration::from_millis(50)).await;
            }
            // Still the same adapter, and checking still on.
            if current() {
                adapter.set_enabled(true);
            }
        });
    }

    /// Removes checking from every editor (the window is going away).
    fn shutdown(&self) {
        for entry in self.editors.take() {
            stop(&entry, None);
        }
        if let (Some(state), Some(id)) = (self.window.upgrade(), self.hook.take()) {
            state.disconnect(id);
        }
    }
}

/// Removes `entry`'s adapter, its underlines, menu section, actions and controllers.
fn stop(entry: &EditorSpell, editor: Option<&Rc<Editor>>) {
    entry.generation.set(entry.generation.get() + 1);
    let Some(checking) = entry.checking.take() else {
        return;
    };
    if let Some(id) = checking.language_handler {
        checking.adapter.disconnect(id);
    }
    if let Some(id) = checking.enabled_handler {
        checking.adapter.disconnect(id);
    }
    if let Some(edges) = &checking.edges {
        edges.shutdown();
    }
    if let Some(filter) = &checking.filter {
        filter.shutdown();
    }
    // Off first: the adapter removes its tags from the buffer.
    checking.adapter.set_enabled(false);
    let editor = editor.cloned().or_else(|| entry.editor.upgrade());
    if let Some(view) = entry.view.upgrade() {
        view.remove_controller(&checking.menu_key);
        view.remove_controller(&checking.gesture);
        view.insert_action_group("spelling", None::<&gio::ActionGroup>);
    }
    if let Some(editor) = editor {
        let menu = editor.extra_menu();
        let section = (0..menu.n_items())
            .find(|&i| menu.item_link(i, "section").as_ref() == Some(&checking.menu));
        if let Some(i) = section {
            menu.remove(i);
        }
    }
}

// ───────────── API for the context menu and tests ─────────────

fn checking_of<R>(editor: &Editor, f: impl FnOnce(&Checking) -> R) -> Option<R> {
    let entry = WINDOWS.with_borrow(|windows| windows.iter().find_map(|w| w.find(editor)))?;
    let checking = entry.checking.borrow();
    checking.as_ref().map(f)
}

/// Fills in the suggestions of the context menu for the word at `at` (moving the cursor there,
/// unless `at` is inside the selection), or for the word at the cursor.
pub fn prepare_context_menu(editor: &Editor, at: Option<&gtk::TextIter>) {
    let Some(adapter) = adapter(editor) else {
        return;
    };
    if let Some(at) = at {
        let buffer = editor.buffer();
        let inside = buffer
            .selection_bounds()
            .is_some_and(|(start, end)| at.in_range(&start, &end) || *at == end);
        if !inside {
            buffer.place_cursor(at);
        }
    }
    adapter.update_corrections();
}

/// What the Menu key and Shift+F10 do before GTK opens the context menu: fill in the suggestions
/// for the word at the cursor. True for those keys.
pub fn menu_key_pressed(editor: &Editor, keyval: gdk::Key, state: gdk::ModifierType) -> bool {
    let shift_f10 = keyval == gdk::Key::F10 && state.contains(gdk::ModifierType::SHIFT_MASK);
    let menu = keyval == gdk::Key::Menu || shift_f10;
    if menu {
        prepare_context_menu(editor, None);
    }
    menu
}

/// The spelling adapter of `editor` while checking is on.
pub fn adapter(editor: &Editor) -> Option<libspelling::TextBufferAdapter> {
    checking_of(editor, |c| c.adapter.clone())
}

/// Whether `editor`'s adapter exists and is checking (its word lists are loaded).
pub fn is_checking(editor: &Editor) -> bool {
    adapter(editor).is_some_and(|a| a.is_enabled())
}

/// The language `editor` is checked in.
pub fn language(editor: &Editor) -> Option<String> {
    adapter(editor)
        .and_then(|a| a.language())
        .map(|l| l.to_string())
}

/// The words of `editor` underlined as misspelled, in order.
pub fn misspelled(editor: &Editor) -> Vec<String> {
    let Some(tag) = adapter(editor).and_then(|a| a.tag()) else {
        return Vec::new();
    };
    let buffer = editor.buffer();
    let mut out = Vec::new();
    let mut iter = buffer.start_iter();
    loop {
        if iter.starts_tag(Some(&tag)) {
            let mut end = iter;
            end.forward_to_tag_toggle(Some(&tag));
            out.push(buffer.text(&iter, &end, true).to_string());
        }
        if !iter.forward_to_tag_toggle(Some(&tag)) {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_lists_split_like_the_neovim_spell_file() {
        let words =
            list_words("Barr-Beck\nMac Lane\ncolimit\nA-infinity\nétale\nČech\n\nx\r\nBarr\n");
        assert_eq!(
            words,
            [
                "Barr",
                "Barr's",
                "Barr’s",
                "Beck",
                "Beck's",
                "Beck’s",
                "Mac",
                "Mac's",
                "Mac’s",
                "Lane",
                "Lane's",
                "Lane’s",
                "colimit",
                "infinity",
                "étale",
                "Čech",
                "Čech's",
                "Čech’s"
            ]
        );
    }

    #[test]
    fn missing_lists_are_skipped() {
        let dir = std::env::temp_dir().join(format!("lemma-spell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("words.txt");
        std::fs::write(&list, "qqword\n").unwrap();
        let read = read_lists(vec![(dir.join("missing.txt"), None), (list.clone(), None)]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].2, ["qqword"]);
        // Unchanged since: not read again.
        let known = read[0].1;
        assert!(read_lists(vec![(list, Some(known))]).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
