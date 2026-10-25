//! Editor tabs: a [`TexView`] on an open [`Document`](crate::document::Document), optional Vim
//! emulation, and the look shared by all editors (font, style scheme).
//!
//! Every editor has a key layer ([`keys`], run before Vim) and a context-menu model
//! ([`Editor::extra_menu`]) that the window and the M2 modules add sections to. The per-editor
//! M2 hooks ([`highlight`], [`completion`], [`typing`], [`spell`], [`keys::attach`]) are called by
//! the window (`WindowState::show_document`).
//!
//! M3 adds two registries several modules share, [`hover`] (popovers when the pointer rests on
//! something, or at the cursor with `K` / Alt+K) and [`goto`] (Ctrl+click, `gd`, F12, and the
//! back-jump list), and the per-editor modules [`annotations`] (inline equation numbers),
//! [`math_preview`] and [`pairs`] (delimiter pairs). Both registries describe a place with a
//! [`TextPoint`].

pub mod align;
pub mod annotations;
pub mod completion;
pub mod editing;
pub mod envtools;
pub mod goto;
pub mod highlight;
pub mod hover;
pub mod keys;
pub mod math_preview;
pub mod pairs;
pub mod review_gutter;
pub mod review_overlay;
pub mod snippets;
pub mod spell;
pub mod tex_view;
pub mod typing;
pub mod vim;

pub use tex_view::TexView;
pub use vim::{Vim, VimMode, VimStatus};

use crate::document::DocRef;
use gtk::prelude::*;
use lemma_config::EditorCfg;
use lemma_syntax::{Context, Token, TokenKind};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

/// One editor tab.
pub struct Editor {
    doc: DocRef,
    buffer: sourceview5::Buffer,
    view: TexView,
    scroller: gtk::ScrolledWindow,
    vim: RefCell<Option<Vim>>,
    /// The view's context-menu model (`TextView::set_extra_menu`).
    extra_menu: gio::Menu,
    /// Set by the window once the tab is gone and every hook has detached
    /// ([`Editor::tab_closed`]).
    closed: Cell<bool>,
}

impl Editor {
    /// An editor for `doc`; `None` when `doc` is closed (has no buffer). It comes with its key
    /// layer ([`keys::install`]) and an empty [`Editor::extra_menu`].
    pub fn new(doc: &DocRef, cfg: &EditorCfg) -> Option<Rc<Self>> {
        let buffer = doc.buffer()?;
        let view = TexView::new(doc, cfg)?;
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hexpand(true)
            .vexpand(true)
            .build();
        review_gutter::attach(&view);
        let extra_menu = gio::Menu::new();
        view.set_extra_menu(Some(&extra_menu));
        let editor = Rc::new_cyclic(|weak: &std::rc::Weak<Editor>| {
            let weak = weak.clone();
            keys::install(&view, &scroller, move || {
                weak.upgrade()
                    .and_then(|e| e.vim_status())
                    .map(|status| status.mode)
            });
            Self {
                doc: doc.clone(),
                buffer,
                view,
                scroller,
                vim: RefCell::new(None),
                extra_menu,
                closed: Cell::new(false),
            }
        });
        Some(editor)
    }

    /// The view's context-menu model: sections added here appear below GTK's own items (Cut,
    /// Copy, …). The window appends its `editor_menu` section (quick actions) when the tab is
    /// created; spell checking inserts its suggestions as section 0.
    pub fn extra_menu(&self) -> &gio::Menu {
        &self.extra_menu
    }

    /// Runs `f` once, right after the view has drawn its first frame (soon after the next
    /// frame if it is already on screen; when it is first mapped otherwise). Used to load spell
    /// checking lazily.
    pub fn after_first_frame(self: &Rc<Self>, f: impl FnOnce(&Rc<Editor>) + 'static) {
        let weak = Rc::downgrade(self);
        let f = Cell::new(Some(f));
        let run: Rc<dyn Fn()> = Rc::new(move || {
            if let (Some(editor), Some(f)) = (weak.upgrade(), f.take()) {
                f(&editor);
            }
        });
        if self.view.is_mapped() {
            run_after_paint(self.view.upcast_ref(), run);
            return;
        }
        let slot: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::default();
        let handler = slot.clone();
        slot.set(Some(self.view.connect_map(move |view| {
            if let Some(id) = handler.take() {
                view.disconnect(id);
            }
            run_after_paint(view.upcast_ref(), run.clone());
        })));
    }

    /// The document shown.
    pub fn document(&self) -> &DocRef {
        &self.doc
    }

    /// The text view.
    pub fn view(&self) -> &TexView {
        &self.view
    }

    /// The buffer shown (the document's buffer).
    pub fn buffer(&self) -> &sourceview5::Buffer {
        &self.buffer
    }

    /// The tab's root widget (a scrolled window around the view).
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    /// Turns Vim emulation on. Returns the new input-method context so the caller can connect
    /// its signals, or `None` if Vim was already on.
    pub fn enable_vim(&self) -> Option<sourceview5::VimIMContext> {
        if self.vim.borrow().is_some() {
            return None;
        }
        // No borrow is held while GTK runs: attaching and detaching emit notifications whose
        // handlers read the Vim state.
        let attached = Vim::attach(&self.view);
        let im = attached.im().clone();
        self.vim.replace(Some(attached));
        Some(im)
    }

    /// Turns Vim emulation off.
    pub fn disable_vim(&self) {
        let vim = self.vim.take();
        if let Some(vim) = vim {
            vim.detach();
        }
    }

    /// The Vim input-method context while Vim is on.
    pub fn vim(&self) -> Option<sourceview5::VimIMContext> {
        self.vim.borrow().as_ref().map(|v| v.im().clone())
    }

    /// Vim mode and pending command, while Vim is on.
    pub fn vim_status(&self) -> Option<VimStatus> {
        self.vim.borrow().as_ref().map(Vim::status)
    }

    /// True when Vim is on and its key guard stops this key press before Vim sees it (see
    /// [`Vim::blocks_key`]). Code that feeds keys to [`Editor::vim`] itself must ask first, as
    /// the guard controller does for real key presses.
    pub fn vim_blocks_key(&self, keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
        self.vim
            .borrow()
            .as_ref()
            .is_some_and(|vim| vim.blocks_key(keyval, state))
    }

    /// The cursor as 1-based (line, visual column).
    pub fn cursor_position(&self) -> (u32, u32) {
        let iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let line = u32::try_from(iter.line()).unwrap_or(0) + 1;
        (line, self.view.visual_column(&iter) + 1)
    }

    /// Sets the buffer's style scheme.
    pub fn set_style_scheme(&self, scheme: Option<&sourceview5::StyleScheme>) {
        self.buffer.set_style_scheme(scheme);
    }

    /// The document byte offset of the cursor (the insert mark).
    pub fn cursor_offset(&self) -> usize {
        let iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        self.doc.byte_offset(&iter)
    }

    /// Puts the cursor at the document byte `offset` (clamped to the text, moved back to a
    /// character boundary) without scrolling; returns false when the document has no buffer.
    pub fn place_cursor_at(&self, offset: usize) -> bool {
        let Some(iter) = self.iter_near(offset) else {
            return false;
        };
        self.buffer.place_cursor(&iter);
        true
    }

    /// The iter at the document byte `offset`, clamped to the text and moved back to the start
    /// of the character it falls into.
    pub fn iter_near(&self, offset: usize) -> Option<gtk::TextIter> {
        let mut offset = offset.min(self.doc.line_index().text_len());
        loop {
            if let Some(iter) = self.doc.iter_at_byte(offset) {
                return Some(iter);
            }
            if offset == 0 || self.doc.buffer().is_none() {
                return None;
            }
            offset -= 1;
        }
    }

    /// The document byte offset of the character under the point `(x, y)` of the view (widget
    /// coordinates, as event controllers report them); `None` when the point is not over a
    /// character: the gutter, past the end of a line, or between lines.
    pub fn offset_at_point(&self, x: f64, y: f64) -> Option<usize> {
        let view = &self.view;
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        // The character containing the point (`iter_at_location` gives the nearest character
        // boundary: the next character over a character's right half).
        let (iter, _trailing) = view.iter_at_position(bx, by)?;
        if iter.ends_line() {
            return None;
        }
        let rect = view.iter_location(&iter);
        let inside = bx >= rect.x()
            && bx < rect.x() + rect.width().max(1)
            && by >= rect.y()
            && by < rect.y() + rect.height().max(1);
        inside.then(|| self.doc.byte_offset(&iter))
    }

    /// The rectangle of the document byte range `range` on the display line of `at` (a byte in
    /// `range`), in the view's widget coordinates: what a popover about that range points at.
    /// A range spanning several (display) lines is cut to `at`'s.
    pub fn range_rect(&self, range: Range<usize>, at: usize) -> Option<gtk::gdk::Rectangle> {
        let view = &self.view;
        let at_iter = self.iter_near(at)?;
        let at_rect = view.iter_location(&at_iter);
        let mut line_start = at_iter;
        if !view.starts_display_line(&line_start) {
            view.backward_display_line_start(&mut line_start);
        }
        let mut line_end = at_iter;
        if !line_end.ends_line() {
            view.forward_display_line_end(&mut line_end);
        }
        let mut start = self.iter_near(range.start)?;
        if start < line_start {
            start = line_start;
        }
        let mut end = self.iter_near(range.end)?;
        if end > line_end {
            end = line_end;
        }
        let a = view.iter_location(&start);
        let (x0, x1) = if end > start {
            let b = view.iter_location(&end);
            (a.x(), b.x())
        } else {
            (at_rect.x(), at_rect.x() + at_rect.width())
        };
        let (wx, wy) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, x0, at_rect.y());
        Some(gtk::gdk::Rectangle::new(
            wx,
            wy,
            (x1 - x0).max(1),
            at_rect.height().max(1),
        ))
    }
}

/// What asked for a hover or a jump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The pointer: resting on the text (hover), Ctrl+click (goto).
    Pointer,
    /// A key at the cursor: `K` / Alt+K (hover), `gd` / F12 (goto).
    Keyboard,
}

/// A place in a document with what the scanner knows about it: the context the [`hover`]
/// providers and [`goto`] resolvers get.
#[derive(Clone)]
pub struct TextPoint {
    pub document: DocRef,
    /// Byte offset in the document's text (`LineIndex` offsets: `\n` lines, UTF-8).
    pub offset: usize,
    /// 0-based line of `offset`.
    pub line: u32,
    /// Byte column of `offset` in its line.
    pub column: usize,
    /// The line's text, without its `\n` (a `\r` before it stays).
    pub line_text: String,
    /// The scanner context at `offset`, decided by the text before it on its line
    /// (`Document::context_at_byte`): inside `\ref{eq:|1}` it is `Arg { cmd: "ref", .. }`, on
    /// the `r` of `\ref` it is `CommandName`.
    pub context: Context,
    /// The scanner token holding the character at `offset` (`start..end` are bytes of the
    /// line); at the end of a line, the line's last token. `None` while the document has no
    /// scanner states (closed, or a large one still being scanned).
    pub token: Option<Token>,
}

impl std::fmt::Debug for TextPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextPoint")
            .field("path", &self.document.path())
            .field("offset", &self.offset)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("context", &self.context)
            .field("token", &self.token)
            .finish()
    }
}

impl TextPoint {
    /// The point at the document byte `offset`; `None` past the end of the text or inside a
    /// UTF-8 character.
    pub fn new(document: &DocRef, offset: usize) -> Option<Self> {
        let (line, column) = {
            let index = document.line_index();
            if offset > index.text_len() {
                return None;
            }
            let (line, column) = index.position(offset);
            (line, column as usize)
        };
        let line_text = document.line_text(line)?;
        if !line_text.is_char_boundary(column.min(line_text.len())) {
            return None;
        }
        let token = document.line_states.borrow().as_ref().and_then(|states| {
            let mut tokens = Vec::new();
            states.tokens(line, &line_text, &mut tokens);
            let col = column as u32;
            tokens
                .iter()
                .find(|t| t.start <= col && col < t.end)
                .or_else(|| tokens.last().filter(|t| t.end <= col))
                .copied()
        });
        Some(Self {
            context: document.context_at_byte(offset),
            document: document.clone(),
            offset,
            line,
            column,
            line_text,
            token,
        })
    }

    /// The document byte offset where the point's line starts.
    pub fn line_start(&self) -> usize {
        self.offset - self.column
    }

    /// The token's kind.
    pub fn token_kind(&self) -> Option<TokenKind> {
        self.token.map(|t| t.kind)
    }

    /// The token's range in document bytes.
    pub fn token_range(&self) -> Option<Range<usize>> {
        let t = self.token?;
        let start = self.line_start();
        Some(start + t.start as usize..start + t.end as usize)
    }

    /// The token's text.
    pub fn token_text(&self) -> Option<&str> {
        let t = self.token?;
        self.line_text.get(t.start as usize..t.end as usize)
    }

    /// The name under the point inside a name-list argument — a label of `\ref{a,b}` or
    /// `\label{a}`, a key of `\cite{a, b}`, a file of `\input{…}`, a package of
    /// `\usepackage{…}` (tokens `RefArg`, `LabelArg`, `CiteArg`, `FileArg`, `PackageArg`) — as
    /// its range in document bytes and its text, blanks trimmed. `None` elsewhere, and on a
    /// comma or blank between names.
    pub fn item(&self) -> Option<(Range<usize>, &str)> {
        let token = self.token?;
        if !matches!(
            token.kind,
            TokenKind::RefArg
                | TokenKind::LabelArg
                | TokenKind::CiteArg
                | TokenKind::FileArg
                | TokenKind::PackageArg
        ) {
            return None;
        }
        let (t0, t1) = (token.start as usize, token.end as usize);
        let text = &self.line_text;
        let col = self.column.clamp(t0, t1);
        let start = text[t0..col].rfind(',').map_or(t0, |i| t0 + i + 1);
        let end = text[col..t1].find(',').map_or(t1, |i| col + i);
        let raw = &text[start..end];
        let lead = raw.len() - raw.trim_start().len();
        let name = raw.trim();
        let (start, end) = (start + lead, start + lead + name.len());
        if name.is_empty() || self.column < start || self.column >= end.max(start + 1) {
            return None;
        }
        let base = self.line_start();
        Some((base + start..base + end, name))
    }

    /// The control word under the point (`\name`, the point on the backslash or a letter):
    /// its range in document bytes (backslash included) and the name without the backslash.
    /// The word after an escaped backslash (`\\name`, a line break) is not one.
    pub fn control_word(&self) -> Option<(Range<usize>, &str)> {
        let bytes = self.line_text.as_bytes();
        let col = self.column;
        let is_letter = |b: u8| b.is_ascii_alphabetic() || b == b'@';
        // A backslash starts a command when the run of backslashes it ends is odd.
        let starts_command = |i: usize| {
            bytes[..=i]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count()
                % 2
                == 1
        };
        let backslash = match bytes.get(col)? {
            b'\\' => col,
            &b if is_letter(b) => {
                let mut i = col;
                while i > 0 && is_letter(bytes[i - 1]) {
                    i -= 1;
                }
                i.checked_sub(1).filter(|&b| bytes[b] == b'\\')?
            }
            _ => return None,
        };
        if !starts_command(backslash) {
            return None;
        }
        let mut end = backslash + 1;
        while end < bytes.len() && is_letter(bytes[end]) {
            end += 1;
        }
        if end == backslash + 1 {
            return None;
        }
        let base = self.line_start();
        Some((
            base + backslash..base + end,
            &self.line_text[backslash + 1..end],
        ))
    }

    /// The range a hover or a link about this point covers by default: the [`TextPoint::item`],
    /// else the [`TextPoint::control_word`], else the token, else the character.
    pub fn anchor(&self) -> Range<usize> {
        if let Some((range, _)) = self.item() {
            return range;
        }
        if let Some((range, _)) = self.control_word() {
            return range;
        }
        if let Some(range) = self.token_range() {
            return range;
        }
        let len = self.line_text[self.column.min(self.line_text.len())..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
        self.offset..self.offset + len
    }
}

/// Runs `run` from an idle callback after `widget`'s next painted frame.
fn run_after_paint(widget: &gtk::Widget, run: Rc<dyn Fn()>) {
    let Some(clock) = widget.frame_clock() else {
        glib::idle_add_local_once(move || run());
        return;
    };
    let slot: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::default();
    let handler = slot.clone();
    let own_clock = clock.clone();
    slot.set(Some(clock.connect_after_paint(move |_| {
        if let Some(id) = handler.take() {
            own_clock.disconnect(id);
        }
        // Outside the frame's paint.
        let run = run.clone();
        glib::idle_add_local_once(move || run());
    })));
    widget.queue_draw();
}

/// Removes `tag` from the ranges it covers, found by walking its toggles. GTK lays out again
/// all the text that a `remove_tag` spans when the tag changes the text's size (line spacing,
/// underlines, fonts), whether or not the tag is there: one `remove_tag` over the whole buffer
/// costs about a second of layout at 60k lines, this costs little for a tag on a few ranges.
pub fn remove_tag_everywhere(buffer: &impl IsA<gtk::TextBuffer>, tag: &gtk::TextTag) {
    let buffer = buffer.as_ref();
    let mut ranges = Vec::new();
    let mut iter = buffer.start_iter();
    while iter.starts_tag(Some(tag)) || iter.forward_to_tag_toggle(Some(tag)) {
        let start = iter.offset();
        iter.forward_to_tag_toggle(Some(tag));
        ranges.push((start, iter.offset()));
        if iter.is_end() {
            break;
        }
    }
    // Removing a tag changes no offsets.
    for (start, end) in ranges {
        buffer.remove_tag(
            tag,
            &buffer.iter_at_offset(start),
            &buffer.iter_at_offset(end),
        );
    }
}

/// Takes the line-spacing tag `tag` out of `buffer`'s text and tag table. GTK lays out the
/// whole text again when a tag that changes the text's size leaves the table, even one that
/// covers nothing (about half a second at 60k lines), so the tag's spacing is unset first.
pub fn remove_spacing_tag(buffer: &impl IsA<gtk::TextBuffer>, tag: &gtk::TextTag) {
    remove_tag_everywhere(buffer, tag);
    tag.set_property("pixels-above-lines-set", false);
    tag.set_property("pixels-below-lines-set", false);
    buffer.as_ref().tag_table().remove(tag);
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Detach Vim while the view is certainly alive: a VimIMContext finalized after its view
        // deletes its buffer marks twice (a Gtk-CRITICAL in GtkSourceView 5.20).
        self.disable_vim();
        if self.closed.get() {
            self.release_view();
        }
    }
}

impl Editor {
    /// Called by the window when the editor's tab is gone and every per-editor hook has
    /// detached (`WindowState`'s `page-detached` handler): dropping the editor then frees its
    /// view, see [`Editor::release_view`].
    pub(crate) fn tab_closed(&self) {
        self.closed.set(true);
    }

    /// Lets the view of a closed tab go. GtkSourceView 5.20 never finalizes a view whose gutter
    /// has drawn a renderer (the review gutter, line numbers): a reference taken while the gutter
    /// drew its last frame keeps the view alive (removing the renderers and drawing one more
    /// frame frees it), and the view holds its buffer (the whole text, its tags and undo
    /// history), so every closed tab leaked them. Running the view's dispose breaks that cycle.
    /// By now the view is out of the widget tree, the hooks have disconnected from it and hold it
    /// weakly (dispose clears weak references), and this editor is going.
    fn release_view(&self) {
        // The scrolled window may still be the closing page's child (the editor goes while
        // `page-detached` runs), so the view leaves it first.
        if self.view.parent().is_some() {
            self.scroller.set_child(None::<&gtk::Widget>);
        }
        // A popover Lemma parented to the view and never took back (the inline edit's, CSS class
        // `lemma-…`) would make the dispose loop forever: GtkTextView's dispose removes its
        // remaining children as text children, and retries one it does not know ("GtkPopover is
        // not a child of …"). Such a view is left as it was. (GTK's and GtkSourceView's own
        // popovers are theirs to remove.)
        let mut child = self.view.first_child();
        while let Some(widget) = child {
            if widget.is::<gtk::Popover>()
                && widget
                    .css_classes()
                    .iter()
                    .any(|class| class.starts_with("lemma-"))
            {
                tracing::debug!("a closed tab's view still has {}", widget.type_().name());
                return;
            }
            child = widget.next_sibling();
        }
        // SAFETY: the view is unparented, nothing is connected to it any more, and the last
        // strong reference that Lemma uses goes with this editor (see above); a disposed widget
        // that is never used again is what GTK leaves behind for any finalized widget.
        unsafe { self.view.run_dispose() };
    }
}

/// The style scheme for the current system style: `cfg.scheme_dark` or `cfg.scheme_light`,
/// falling back to `Adwaita-dark`/`Adwaita` when the configured scheme is not installed.
pub fn style_scheme(cfg: &EditorCfg, dark: bool) -> Option<sourceview5::StyleScheme> {
    let manager = sourceview5::StyleSchemeManager::default();
    let (wanted, fallback) = if dark {
        (cfg.scheme_dark.as_str(), "Adwaita-dark")
    } else {
        (cfg.scheme_light.as_str(), "Adwaita")
    };
    manager.scheme(wanted).or_else(|| {
        tracing::warn!("style scheme {wanted:?} is not installed; using {fallback}");
        manager.scheme(fallback)
    })
}

/// Installs the app's CSS on `display`: the editor font from `cfg.font` (applied to views with
/// the `lemma-editor` class) and the shell styles (status bar, sidebar). Calling it again (a
/// config reload) replaces the font instead of adding another provider.
pub fn install_css(display: &gtk::gdk::Display, cfg: &EditorCfg) {
    thread_local! {
        static PROVIDER: RefCell<Option<(gtk::gdk::Display, gtk::CssProvider)>> =
            const { RefCell::new(None) };
    }
    let css = format!("{}\n{SHELL_CSS}", font_css(&cfg.font));
    let reused = PROVIDER.with_borrow(|p| match p {
        Some((d, provider)) if d == display => {
            provider.load_from_string(&css);
            true
        }
        _ => false,
    });
    if reused {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    PROVIDER.with_borrow_mut(|p| *p = Some((display.clone(), provider)));
}

const SHELL_CSS: &str = "
.lemma-statusbar { padding: 3px 10px; min-height: 22px; font-size: 0.86em;
                   border-top: 1px solid alpha(currentColor, 0.12); }
.lemma-statusbar .mode { font-weight: bold; padding: 0 6px; border-radius: 4px;
                         background: alpha(@accent_bg_color, 0.18); }
.lemma-statusbar .mode.insert { background: alpha(@success_color, 0.25); }
.lemma-statusbar .mode.visual { background: alpha(@warning_color, 0.25); }
.lemma-sidebar stackswitcher { margin: 6px; }
.lemma-sidebar stackswitcher button { padding: 2px 4px; min-width: 0; font-size: 0.9em; }
";

/// CSS for a Pango font description such as `"Monospace 11"` or `"Fira Code, Monospace Bold 12"`.
fn font_css(font: &str) -> String {
    let desc = pango::FontDescription::from_string(font);
    let families = desc
        .family()
        .map(|f| f.to_string())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "monospace".into());
    let families: Vec<String> = families
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| format!("\"{}\"", f.replace(['"', '\\'], "")))
        .collect();
    let size = if desc.size() > 0 {
        let unit = if desc.is_size_absolute() { "px" } else { "pt" };
        format!("{}{unit}", f64::from(desc.size()) / f64::from(pango::SCALE))
    } else {
        "11pt".into()
    };
    let weight = match desc.weight() {
        pango::Weight::Thin => 100,
        pango::Weight::Ultralight => 200,
        pango::Weight::Light | pango::Weight::Semilight => 300,
        pango::Weight::Book | pango::Weight::Normal => 400,
        pango::Weight::Medium => 500,
        pango::Weight::Semibold => 600,
        pango::Weight::Bold => 700,
        pango::Weight::Ultrabold => 800,
        pango::Weight::Heavy | pango::Weight::Ultraheavy => 900,
        _ => 400,
    };
    let style = match desc.style() {
        pango::Style::Italic => "italic",
        pango::Style::Oblique => "oblique",
        _ => "normal",
    };
    format!(
        "textview.lemma-editor {{ font-family: {}; font-size: {size}; font-weight: {weight}; \
         font-style: {style}; }}",
        families.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::font_css;

    #[test]
    fn font_descriptions_become_css() {
        assert_eq!(
            font_css("Monospace 11"),
            "textview.lemma-editor { font-family: \"Monospace\"; font-size: 11pt; \
             font-weight: 400; font-style: normal; }"
        );
        let css = font_css("Fira Code, Monospace Bold Italic 12.5");
        assert!(
            css.contains("font-family: \"Fira Code\", \"Monospace\";"),
            "{css}"
        );
        assert!(css.contains("font-size: 12.5pt;"), "{css}");
        assert!(css.contains("font-weight: 700;"), "{css}");
        assert!(css.contains("font-style: italic;"), "{css}");
        let css = font_css("");
        assert!(css.contains("font-family: \"monospace\";"), "{css}");
        assert!(css.contains("font-size: 11pt;"), "{css}");
    }
}
