//! Completion provider and snippet driver (WP2.9a; ARCHITECTURE §5 "Completion", "Snippets").
//!
//! [`attach`] runs after the editor's review renderer and diagnostics are attached. It gives the
//! view:
//!
//! - a `sourceview5::CompletionProvider` ([`Provider`]) backed by `lemma_complete::Completer`:
//!   commands (with glyphs) after `\`, environments after `\begin{`/`\end{`, labels with their
//!   `.aux` numbers, citations with author and year, files and packages in their arguments,
//!   manual snippets and dictionary/project words in text. Each row shows a kind icon, the glyph,
//!   the label (matched characters highlighted) and the detail;
//! - the snippet driver ([`snippets::Driver`]): autosnippets on typed characters, Tab/Shift+Tab,
//!   and the end of snippet sessions before non-typing edits;
//! - one key-layer handler at [`keys::PRIORITY_SNIPPETS`] that feeds both.
//!
//! # When the popup opens
//!
//! GtkSourceView's own interactive completion is left on but never produces proposals
//! ([`Provider`] answers interactive requests with an empty model and is never a "trigger"):
//! Lemma decides itself, after each **typed character** (see `snippets::typed_char`), with the
//! context at the cursor:
//!
//! - a command name (`\`, `\fr`): always;
//! - inside the argument of a reference, citation, file, package or environment command —
//!   `{` after `\ref`, `\cite`, `\begin`, `\input`, `\usepackage`, `\includegraphics`, … (the
//!   scanner's classes plus the word lists' classifiers), `,` in their lists, and the item's own
//!   characters (`eq:co`): always;
//! - a word of at least `completion.min_word` characters in text: only with `completion.auto`;
//! - never in comments or verbatim text, and never while the typed text is exactly a complete
//!   proposal (`\item`: Enter then makes a new line).
//!
//! While the popup is open GtkSourceView refilters it on word characters ([`Provider`]'s
//! `refilter` recomputes the proposals). Command and argument popups select their first row
//! (Enter accepts it); word popups do not, so Enter in prose still breaks the line. Tab never
//! accepts a proposal (it is the snippet key, as in the user's LazyVim setup). Escape closes the
//! popup (and in Vim leaves insert mode as usual). Ctrl+Space opens it anywhere.
//!
//! # Accepting
//!
//! A proposal replaces the text being completed (from the backslash for commands) as one undo
//! step: text, or a snippet through the driver (tab stops). An environment after `\begin{`
//! becomes the whole environment when the document's `\begin`/`\end` balance (comments skipped,
//! as the typing helpers count it) finds it unclosed; a command right before a `{` keeps its
//! existing arguments. `Completer::record_accepted` ranks it higher afterwards.
//!
//! After `\end{` every environment is offered (not filtered by math or text mode: inside `align`
//! the context is math, and `align` is what is being closed), the one open at the cursor first;
//! a name typed in full is not preselected.
//!
//! Only LaTeX documents ([`is_latex`]) get completion and the snippet driver (and the typing
//! helpers); other files (Markdown, `latexmkrc`, `.bib`) are not indexed either.
//!
//! # The project index
//!
//! The window's `workspace.index` (`ProjectIndex`) is kept fresh here: open documents are
//! re-indexed on the jobs thread 300 ms after their last change (one job per document at a time:
//! edits made while it runs are indexed once more after it); the files they include, the local
//! packages and classes they load (`\usepackage{mymacros}` with a `mymacros.sty` next to the
//! file or in the root's directory, a `\documentclass` whose `.cls` is next to the root: their
//! macros and theorems; a package of the distribution is looked for there once until the next
//! build), the `.tex` files next to them (to find the root), the bibliographies and the root's
//! `.aux` (in the build directory or next to it) are read from disk; after each build the
//! `.aux`, the `.bib` files and the project's file list are read again (and when the root found
//! lives in another directory than the one listed). The words of the `.tex` files read from
//! disk are collected again only when one of their texts changed (or one came or went), not
//! after a build that changed only the `.aux`. Files read from disk that another program
//! changes, creates or deletes (a re-exported `.bib`, a `git checkout`, a chapter edited
//! elsewhere, a file an `\input` named before it existed) are read again as the file monitor
//! reports them ([`disk_changed`]; a file that is gone leaves the index), and the file list is
//! made again when files come or go ([`files_moved`]). Outside the workspace folder (a central
//! `.bib`, a chapter included through `../`, a root above the folder) the monitor watches the
//! directories of those files for the index (`watcher::watch_outside`, at most
//! `watcher::MAX_OUTSIDE_DIRS`). A re-index that gives a document the index it had (prose
//! typed: the same lines, titles, labels and captions) keeps the index's entry, so the Outline
//! sees no change, and only the words are rebuilt. When the project's theorem environments
//! change (a preamble file read after the chapters that use its `\newtheorem`s, a declaration
//! typed or removed), the files whose environments use a changed name are indexed again, so
//! their theorem rows and label titles follow. A `.tex` file next to an open document joins the
//! index only when its own includes lead to an open document (sharing a `macros.tex` is not
//! enough), and whatever no open document reaches any more (a closed tab's project, a removed
//! `\input`) leaves it. The loaded packages decide which command
//! tables are active (`CompletionDb::load_packages`), the `spell.word_lists` and the project's
//! text give the words, and the project's macros decide `if_undefined` snippet bodies.
//!
//! [`detach`] undoes everything when the tab closes. The snippet set follows
//! `WindowState::snippets()` (reloads apply at once).

use super::snippets::{self, Driver};
use super::{Editor, TexView, VimMode, keys};
use crate::document::{DocRef, Document, ObserverId};
use crate::hooks::HookId;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use lemma_complete::{
    Completer, CompletionDb, CompletionRequest, Insert, Item, ItemKind, WordList, document_class,
};
use lemma_snippets::Chunk;
use lemma_syntax::{
    ArgKind, AuxInfo, BibEntry, Context, FileIndex, IncludeKind, ProjectIndex, aux_inputs,
    index_file_with_theorems, parse_aux, scan_bib,
};
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

/// Re-index an open document this long after its last change.
const REINDEX_DELAY: Duration = Duration::from_millis(300);
/// Rebuild the project words this long after the last change.
const WORDS_DELAY: Duration = Duration::from_millis(1500);
/// At most this many files are indexed from disk per window.
const MAX_DISK_FILES: usize = 300;
/// Files larger than this are not read.
const MAX_FILE_BYTES: u64 = 4 << 20;
/// Directories above an open document searched for its root (as the build does).
const NEARBY_LEVELS: usize = 2;
/// `.tex` files read per directory when looking for the root.
const MAX_NEARBY_PER_DIR: usize = 64;
/// Caps of the walk that lists the project's files for file arguments.
const WALK_MAX_FILES: usize = 4000;
const WALK_MAX_DIRS: usize = 400;
/// Longest detail shown next to a proposal (the full text is in the comment row).
const DETAIL_CHARS: usize = 48;

// ───────────── Proposals and the provider (GObjects) ─────────────

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Proposal {
        pub item: RefCell<Option<Item>>,
        /// The text being completed when the proposal was made (for highlighting).
        pub needle: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Proposal {
        const NAME: &'static str = "LemmaCompletionProposal";
        type Type = super::Proposal;
        type Interfaces = (sourceview5::CompletionProposal,);
    }

    impl ObjectImpl for Proposal {}
    impl CompletionProposalImpl for Proposal {}

    #[derive(Default)]
    pub struct Provider {
        pub attached: RefCell<Weak<Attached>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Provider {
        const NAME: &'static str = "LemmaCompletionProvider";
        type Type = super::Provider;
        type Interfaces = (sourceview5::CompletionProvider,);
    }

    impl ObjectImpl for Provider {}

    impl Provider {
        fn attached(&self) -> Option<Rc<Attached>> {
            self.attached.borrow().upgrade()
        }

        /// The proposals for `ctx`: none for GtkSourceView's interactive requests (Lemma opens
        /// the popup itself, see the module docs).
        fn model(&self, ctx: &sourceview5::CompletionContext) -> gio::ListModel {
            let store = gio::ListStore::new::<super::Proposal>();
            if ctx.activation() != sourceview5::CompletionActivation::Interactive
                && let Some(attached) = self.attached()
            {
                attached.populate(&store);
            }
            store.upcast()
        }
    }

    impl CompletionProviderImpl for Provider {
        fn title(&self) -> Option<glib::GString> {
            Some("LaTeX".into())
        }

        fn priority(&self, _context: &sourceview5::CompletionContext) -> i32 {
            0
        }

        fn is_trigger(&self, _iter: &gtk::TextIter, _c: char) -> bool {
            false
        }

        fn key_activates(
            &self,
            _context: &sourceview5::CompletionContext,
            _proposal: &sourceview5::CompletionProposal,
            _keyval: gdk::Key,
            _state: gdk::ModifierType,
        ) -> bool {
            false
        }

        fn populate(
            &self,
            context: &sourceview5::CompletionContext,
        ) -> Result<gio::ListModel, glib::Error> {
            Ok(self.model(context))
        }

        fn populate_future(
            &self,
            context: &sourceview5::CompletionContext,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<gio::ListModel, glib::Error>>>>
        {
            Box::pin(std::future::ready(Ok(self.model(context))))
        }

        fn refilter(&self, _context: &sourceview5::CompletionContext, model: &gio::ListModel) {
            if let (Some(attached), Some(store)) =
                (self.attached(), model.downcast_ref::<gio::ListStore>())
            {
                attached.refilter(store);
            }
        }

        fn display(
            &self,
            _context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
            cell: &sourceview5::CompletionCell,
        ) {
            let Some(proposal) = proposal.downcast_ref::<super::Proposal>() else {
                return;
            };
            let item = proposal.imp().item.borrow();
            let Some(item) = item.as_ref() else {
                return;
            };
            match cell.column() {
                sourceview5::CompletionColumn::Icon => cell.set_icon_name(icon_name(item.kind)),
                sourceview5::CompletionColumn::Before => {
                    let glyph = item.glyph.map(String::from);
                    cell.set_text(glyph.as_deref());
                }
                sourceview5::CompletionColumn::TypedText => {
                    let needle = glib::casefold(proposal.imp().needle.borrow().as_str());
                    match sourceview5::Completion::fuzzy_highlight(&item.label, &needle) {
                        Some(attrs) if !needle.is_empty() => {
                            cell.set_text_with_attributes(&item.label, &attrs);
                        }
                        _ => cell.set_text(Some(&item.label)),
                    }
                }
                sourceview5::CompletionColumn::After => {
                    let short = item.detail.as_deref().map(|d| shorten(d, DETAIL_CHARS));
                    cell.set_text(short.as_deref());
                }
                // The whole detail, when the row had to shorten it.
                sourceview5::CompletionColumn::Comment => cell.set_text(
                    item.detail
                        .as_deref()
                        .filter(|d| d.chars().count() > DETAIL_CHARS),
                ),
                _ => cell.set_text(None),
            }
        }

        fn activate(
            &self,
            _context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
        ) {
            let item = proposal
                .downcast_ref::<super::Proposal>()
                .and_then(|p| p.imp().item.borrow().clone());
            if let (Some(attached), Some(item)) = (self.attached(), item) {
                attached.accept(&item);
            }
        }
    }
}

glib::wrapper! {
    /// One completion proposal: a `lemma_complete::Item`.
    pub struct Proposal(ObjectSubclass<imp::Proposal>)
        @implements sourceview5::CompletionProposal;
}

impl Proposal {
    fn new(item: Item, needle: &str) -> Self {
        let proposal: Self = glib::Object::new();
        proposal.imp().item.replace(Some(item));
        proposal.imp().needle.replace(needle.to_owned());
        proposal
    }

    /// The item this proposal stands for.
    pub fn item(&self) -> Option<Item> {
        self.imp().item.borrow().clone()
    }
}

glib::wrapper! {
    /// The LaTeX completion provider of one view (see the module docs).
    pub struct Provider(ObjectSubclass<imp::Provider>)
        @implements sourceview5::CompletionProvider;
}

/// The icon of a proposal's kind (GtkSourceView's own for snippets and words, Adwaita's for
/// the rest).
fn icon_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Command => "insert-text-symbolic",
        ItemKind::UserMacro => "document-edit-symbolic",
        ItemKind::Environment => "format-indent-more-symbolic",
        ItemKind::Snippet => "completion-snippet-symbolic",
        ItemKind::Label => "user-bookmarks-symbolic",
        ItemKind::Citation => "x-office-address-book-symbolic",
        ItemKind::File => "text-x-generic-symbolic",
        ItemKind::Package => "package-x-generic-symbolic",
        ItemKind::Word => "completion-word-symbolic",
    }
}

/// Looks up the icons of every proposal kind once, so that the icon theme is loaded before the
/// first popup shows.
fn preload_icons(view: &TexView) {
    let theme = gtk::IconTheme::for_display(&view.display());
    for kind in [
        ItemKind::Command,
        ItemKind::UserMacro,
        ItemKind::Environment,
        ItemKind::Snippet,
        ItemKind::Label,
        ItemKind::Citation,
        ItemKind::File,
        ItemKind::Package,
        ItemKind::Word,
    ] {
        let _ = theme.lookup_icon(
            icon_name(kind),
            &[],
            16,
            view.scale_factor(),
            gtk::TextDirection::None,
            gtk::IconLookupFlags::empty(),
        );
    }
}

/// `s` cut to `max` characters, with an ellipsis when cut.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ───────────── What triggers the popup ─────────────

/// Why the popup opens (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A command name after `\`.
    Command,
    /// An item in the argument of a reference, citation, file, package or environment command.
    Argument,
    /// A word in text (`completion.auto`).
    Word,
}

/// Argument kinds whose items are completed.
fn completes_argument(kind: ArgKind) -> bool {
    matches!(
        kind,
        ArgKind::Ref | ArgKind::Cite | ArgKind::File | ArgKind::Env | ArgKind::Package
    )
}

/// Whether `s` ends with a backslash that is not itself escaped.
fn ends_with_unescaped_backslash(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Bytes that end an argument item when scanning backwards.
fn ends_item(b: u8) -> bool {
    matches!(
        b,
        b'{' | b'}' | b',' | b'\\' | b'$' | b'%' | b' ' | b'\t' | b'[' | b']'
    )
}

/// When the cursor (at the end of `line_before`) is inside the braces of `\cmd[…]{a, b, item`
/// on this line: the command name, without the backslash, with its `*`.
pub fn argument_command(line_before: &str) -> Option<&str> {
    let b = line_before.as_bytes();
    let mut i = b.len();
    while i > 0 && !ends_item(b[i - 1]) {
        i -= 1;
    }
    loop {
        while i > 0 && matches!(b[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        match b.get(i.checked_sub(1)?)? {
            b'{' => break,
            b',' => {
                i -= 1;
                while i > 0 && !ends_item(b[i - 1]) {
                    i -= 1;
                }
            }
            _ => return None,
        }
    }
    let mut j = i - 1;
    while j > 0 && b[j - 1] == b']' {
        let open = b[..j - 1].iter().rposition(|&c| c == b'[')?;
        if b[open..j - 1].contains(&b'{') {
            return None;
        }
        j = open;
    }
    let name_end = j;
    if j > 0 && b[j - 1] == b'*' {
        j -= 1;
    }
    let letters = b[..j]
        .iter()
        .rev()
        .take_while(|c| c.is_ascii_alphabetic())
        .count();
    if letters == 0 {
        return None;
    }
    let name_start = j - letters;
    ends_with_unescaped_backslash(&line_before[..name_start])
        .then(|| &line_before[name_start..name_end])
}

/// What typing `c` (now before the cursor) opens, given the scanner context at the cursor
/// (`at_cursor`), the request built for completion, and the `[completion]` settings.
pub fn trigger_for(
    db: &CompletionDb,
    at_cursor: &Context,
    req: &CompletionRequest<'_>,
    c: char,
    auto: bool,
    min_word: usize,
) -> Option<Trigger> {
    match at_cursor {
        Context::Comment | Context::Verbatim => return None,
        Context::Arg { kind, .. } => return completes_argument(*kind).then_some(Trigger::Argument),
        _ => {}
    }
    if req.is_command() {
        return Some(Trigger::Command);
    }
    if let Some(cmd) = argument_command(req.line_before) {
        let kind = match cmd.trim_end_matches('*') {
            "begin" | "end" => Some(ArgKind::Env),
            _ => db.arg_kind(cmd),
        };
        if kind.is_some_and(completes_argument) {
            return Some(Trigger::Argument);
        }
    }
    let text = matches!(at_cursor, Context::Text | Context::TextInMath);
    (auto && text && c.is_alphanumeric() && req.prefix.chars().count() >= min_word.max(1))
        .then_some(Trigger::Word)
}

// ───────────── Per window ─────────────

/// What one window's editors share: the tables, the ranking memory, the words and the project
/// feed.
struct Shared {
    state: Weak<WindowState>,
    db: RefCell<CompletionDb>,
    completer: RefCell<Completer>,
    words: RefCell<Rc<WordList>>,
    feed: Feed,
    editors: RefCell<Vec<Rc<Attached>>>,
    hooks: RefCell<Vec<HookId>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<Shared>>> = const { RefCell::new(Vec::new()) };
}

/// The window's shared state, if completion was ever attached in it.
fn shared_of(state: &WindowState) -> Option<Rc<Shared>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.state.strong_count() > 0);
        windows
            .iter()
            .find(|w| std::ptr::eq(w.state.as_ptr(), state))
            .cloned()
    })
}

/// The window's shared state, created on first use.
fn shared_for(state: &Rc<WindowState>) -> Rc<Shared> {
    if let Some(shared) = shared_of(state) {
        return shared;
    }
    let shared = Rc::new(Shared {
        state: Rc::downgrade(state),
        db: RefCell::new(CompletionDb::builtin()),
        completer: RefCell::new(Completer::new()),
        words: RefCell::new(Rc::new(WordList::default())),
        feed: Feed::default(),
        editors: RefCell::default(),
        hooks: RefCell::default(),
    });
    WINDOWS.with_borrow_mut(|windows| windows.push(shared.clone()));
    let weak = Rc::downgrade(&shared);
    let config = state.connect_config_changed(move |state, file| {
        if let Some(shared) = weak.upgrade() {
            shared.config_changed(state, file);
        }
    });
    let weak = Rc::downgrade(&shared);
    let root = state.connect_root_changed(move |state| {
        if let Some(shared) = weak.upgrade() {
            shared.feed.walked_dir.replace(None);
            shared.discover();
            shared.watch_outside(state);
        }
    });
    shared.hooks.replace(vec![config, root]);
    // Lemma's save of a closed document (an agent's edit, a rejected hunk) reaches the index
    // from the saved text, not through a discovery job; an open one's words come from its tab
    // until it closes (`detach`).
    let weak = Rc::downgrade(&shared);
    let saved = crate::saver::connect_saved(move |path, _| {
        if let Some(shared) = weak.upgrade()
            && shared
                .state()
                .is_none_or(|s| s.workspace.get(path).is_none_or(|d| !d.is_open()))
        {
            shared.disk_text_unread(path);
        }
    });
    state
        .window
        .connect_destroy(move |_| crate::saver::disconnect_saved(saved));
    if let Some(build) = state.build.get() {
        let weak = Rc::downgrade(&shared);
        build.connect_finished(move |outcome| {
            if let Some(shared) = weak.upgrade() {
                shared.after_build(outcome.pdf.as_deref());
            }
        });
    }
    shared.feed.dictionary_lists.replace(word_lists(state));
    shared.load_dictionary();
    shared
}

impl Shared {
    fn state(&self) -> Option<Rc<WindowState>> {
        self.state.upgrade()
    }

    fn config_changed(self: &Rc<Self>, state: &Rc<WindowState>, file: ConfigFile) {
        match file {
            ConfigFile::Config => {
                let lists = word_lists(state);
                if *self.feed.dictionary_lists.borrow() != lists {
                    self.feed.dictionary_lists.replace(lists);
                    self.load_dictionary();
                }
                if !state.config.typing.autosnippets || !state.config.completion.auto {
                    self.hide_all();
                }
            }
            ConfigFile::Snippets => self.hide_all(),
            ConfigFile::Prompts => {}
        }
    }

    fn hide_all(&self) {
        for attached in self.editors.borrow().iter() {
            attached.hide();
        }
    }

    /// Whether the project defines the macro `name` (for `if_undefined` snippet bodies).
    fn macro_defined(&self, name: &str) -> bool {
        self.state()
            .is_some_and(|state| index_defines(&state.workspace.index.borrow(), name))
    }
}

/// Whether `index` defines the macro `name` (not an environment or theorem).
fn index_defines(index: &ProjectIndex, name: &str) -> bool {
    index.files.values().any(|f| {
        f.macros
            .iter()
            .any(|m| !m.is_env && !m.is_theorem && m.name == name)
    })
}

/// The `spell.word_lists`, `~` expanded.
fn word_lists(state: &WindowState) -> Vec<PathBuf> {
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

/// A key press waiting for its character to arrive in the buffer.
#[derive(Clone, Copy)]
struct PendingKey {
    c: char,
    version: u64,
}

/// The proposals computed for a position, kept until GtkSourceView asks for them.
struct Stash {
    version: u64,
    cursor: i32,
    items: Vec<Item>,
    needle: String,
    /// One of them is exactly the typed text (accepting it changes nothing).
    exact: bool,
}

/// Completion and the snippet driver of one editor.
pub struct Attached {
    shared: Weak<Shared>,
    editor: Weak<Editor>,
    view: glib::WeakRef<TexView>,
    doc: Weak<Document>,
    driver: Rc<Driver>,
    provider: Provider,
    key_handler: Cell<Option<keys::HandlerId>>,
    doc_observers: RefCell<Vec<ObserverId>>,
    completion_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    pending_key: Cell<Option<PendingKey>>,
    stash: RefCell<Option<Stash>>,
    /// The model GtkSourceView shows (for [`shown_items`]).
    model: RefCell<glib::WeakRef<gio::ListStore>>,
    /// True while [`Attached::show`] emits `show-completion` (Ctrl+Space is the user's).
    showing: Cell<bool>,
    /// The request whose `populate` GtkSourceView runs next ([`Attached::populate`]).
    request: Cell<Option<Request>>,
    /// Whether the popup shown selects its first row.
    selecting: Cell<bool>,
    /// The popup was created ([`Attached::prime`]).
    primed: Cell<bool>,
}

/// A `show-completion` request whose proposals GtkSourceView has not asked for yet: the
/// provider's `populate` runs from a main-loop task after the request (sourceview5-rs spawns
/// `populate_future`), so a flag set only while the request is emitted is gone by then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Request {
    /// [`Attached::prime`]'s request: populate nothing.
    Prime,
    /// Lemma's own request ([`Attached::show_stash`], a retry) or the user's Ctrl+Space.
    Proposals,
}

/// Whether the file at `path` is a LaTeX source (`.tex`, `.sty`, `.cls`, …, as
/// `document::language_for` decides): completion, the snippet driver and the typing helpers work
/// only there. A Markdown file, a Makefile, `latexmkrc` or a `.bib` would otherwise have its
/// text changed by LaTeX autosnippets (`mk` → `$…$`).
pub fn is_latex(path: &Path) -> bool {
    crate::document::language_for(path).is_some_and(|l| l.id() == "latex")
}

/// Adds completion and the snippet driver to `editor` (see the module docs). Nothing happens for
/// a document that is not LaTeX ([`is_latex`]).
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if !is_latex(editor.document().path()) {
        return;
    }
    let shared = shared_for(state);
    if shared
        .editors
        .borrow()
        .iter()
        .any(|a| std::ptr::eq(a.editor.as_ptr(), Rc::as_ptr(editor)))
    {
        return;
    }
    let view = editor.view();
    let doc = editor.document();
    let driver = Driver::new(view, doc);
    let attached = Rc::new_cyclic(|weak: &Weak<Attached>| Attached {
        shared: Rc::downgrade(&shared),
        editor: Rc::downgrade(editor),
        view: view.downgrade(),
        doc: Rc::downgrade(doc),
        driver,
        provider: {
            let provider: Provider = glib::Object::new();
            provider.imp().attached.replace(weak.clone());
            provider
        },
        key_handler: Cell::new(None),
        doc_observers: RefCell::default(),
        completion_handlers: RefCell::default(),
        pending_key: Cell::new(None),
        stash: RefCell::new(None),
        model: RefCell::default(),
        showing: Cell::new(false),
        request: Cell::new(None),
        selecting: Cell::new(true),
        primed: Cell::new(false),
    });
    let completion = view.completion();
    completion.set_page_size(10);
    completion.set_show_icons(true);
    completion.set_remember_info_visibility(true);
    completion.add_provider(&attached.provider);
    // Ctrl+Space (the user's own request) selects the first row.
    let weak = Rc::downgrade(&attached);
    let show = completion.connect_show(move |completion| {
        if let Some(attached) = weak.upgrade()
            && !attached.showing.get()
        {
            completion.set_select_on_show(true);
            attached.selecting.set(true);
            attached.request.set(Some(Request::Proposals));
            attached.retry_show(false);
        }
    });
    attached.completion_handlers.replace(vec![show]);
    let weak = Rc::downgrade(&attached);
    attached.key_handler.set(keys::register(
        view,
        keys::PRIORITY_SNIPPETS,
        move |event, mode| match weak.upgrade() {
            Some(attached) => attached.on_key(event, mode),
            None => glib::Propagation::Proceed,
        },
    ));
    // Keep the project index fresh from this document.
    let weak = Rc::downgrade(&shared);
    let observer = doc.connect_changed(move |doc, _, _| {
        if let Some(shared) = weak.upgrade() {
            shared.schedule_reindex(doc.path());
        }
    });
    attached.doc_observers.replace(vec![observer]);
    // The popup and the icons of its rows are created once the view is on screen, not on the
    // first keystroke (about 5–10 ms in a release build).
    let weak = Rc::downgrade(&attached);
    editor.after_first_frame(move |editor| {
        if let Some(attached) = weak.upgrade() {
            attached.prime();
            preload_icons(editor.view());
        }
    });
    shared.editors.borrow_mut().push(attached);
    shared.index_document(doc);
    if shared.feed.walked_dir.borrow().is_none() {
        shared.walk_files();
    }
}

/// Removes completion and the snippet driver from `editor` (see the module docs). Safe when
/// [`attach`] never ran for it.
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(shared) = shared_of(state) else {
        return;
    };
    let removed = {
        let mut editors = shared.editors.borrow_mut();
        let at = editors
            .iter()
            .position(|a| std::ptr::eq(a.editor.as_ptr(), Rc::as_ptr(editor)));
        at.map(|i| editors.remove(i))
    };
    if let Some(attached) = removed {
        attached.shutdown();
        // What its file holds now (saved or reloaded while it was open) no discovery job read.
        let path = editor.document().path();
        shared.disk_text_unread(path);
        // A path that names it through a symbolic link was set aside while its live text was
        // indexed (see `merge_discovered`): it is read from disk again.
        let links = keys_naming(
            shared.feed.unreadable.borrow().iter(),
            path,
            &shared.feed.real_paths.borrow(),
        );
        {
            let mut unreadable = shared.feed.unreadable.borrow_mut();
            for link in &links {
                unreadable.remove(link);
            }
        }
        // Once the document has left the workspace (right after this), what only it reached
        // leaves the index.
        let weak = Rc::downgrade(&shared);
        glib::idle_add_local_once(move || {
            if let Some(shared) = weak.upgrade() {
                shared.index_changed(false);
            }
        });
    }
}

impl Attached {
    fn shared(&self) -> Option<Rc<Shared>> {
        self.shared.upgrade()
    }

    fn shutdown(&self) {
        if let Some(view) = self.view.upgrade() {
            if let Some(id) = self.key_handler.take() {
                keys::unregister(&view, id);
            }
            let completion = view.completion();
            completion.hide();
            for id in self.completion_handlers.take() {
                completion.disconnect(id);
            }
            completion.remove_provider(&self.provider);
        }
        if let Some(doc) = self.doc.upgrade() {
            for id in self.doc_observers.take() {
                doc.disconnect(id);
            }
        }
        self.driver.shutdown();
        self.provider.imp().attached.replace(Weak::new());
    }

    /// The key-layer handler (priority [`keys::PRIORITY_SNIPPETS`]).
    fn on_key(self: &Rc<Self>, event: &keys::KeyEvent, mode: VimMode) -> glib::Propagation {
        if mode != VimMode::Insert {
            return glib::Propagation::Proceed;
        }
        self.prime();
        let modifiers = event.state
            & (gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SUPER_MASK
                | gdk::ModifierType::META_MASK
                | gdk::ModifierType::HYPER_MASK);
        let ctrl = modifiers == gdk::ModifierType::CONTROL_MASK;
        let leaves_insert = event.keyval == gdk::Key::Escape
            || (ctrl && event.keyval == gdk::Key::bracketleft)
            || (ctrl && event.vim && event.keyval == gdk::Key::c);
        if leaves_insert {
            self.hide();
            // Vim leaves insert mode: the snippet is done (GtkSourceView's sessions do not
            // survive normal-mode edits such as `dd` inside them).
            if event.vim {
                self.driver.end_sessions();
            }
            return glib::Propagation::Proceed;
        }
        let tab = matches!(
            event.keyval,
            gdk::Key::Tab | gdk::Key::KP_Tab | gdk::Key::ISO_Left_Tab
        );
        if tab && modifiers.is_empty() {
            let back = event.keyval == gdk::Key::ISO_Left_Tab
                || event.state.contains(gdk::ModifierType::SHIFT_MASK);
            return self.tab(back);
        }
        let Some(c) = snippets::typed_char(event.keyval, event.state) else {
            return glib::Propagation::Proceed;
        };
        if self.autosnippet(c) {
            return glib::Propagation::Stop;
        }
        self.expect_char(c);
        glib::Propagation::Proceed
    }

    /// Expands the autosnippet that typing `c` completes; true when it did (the key is consumed).
    fn autosnippet(&self, c: char) -> bool {
        let Some(shared) = self.shared() else {
            return false;
        };
        let Some(state) = shared.state() else {
            return false;
        };
        if !state.config.typing.autosnippets {
            return false;
        }
        let set = state.snippets();
        let Some(m) = self.driver.autosnippet(&set, c) else {
            return false;
        };
        self.hide();
        self.driver
            .type_and_expand(c, &m, |name| shared.macro_defined(name))
    }

    /// Tab / Shift+Tab (see `snippets::TabAction`).
    fn tab(&self, back: bool) -> glib::Propagation {
        self.hide();
        let Some(shared) = self.shared() else {
            return glib::Propagation::Proceed;
        };
        let Some(state) = shared.state() else {
            return glib::Propagation::Proceed;
        };
        let set = state.snippets();
        let action = self
            .driver
            .tab(back, &set, |name| shared.macro_defined(name));
        if action.consumes_key() {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }

    /// Makes GtkSourceView create the view's popup, once, before the first real request: its
    /// first `show-completion` populates without displaying (an empty request here, closed at
    /// once), so that the first popup the user sees opens with its row selected.
    fn prime(&self) {
        if self.primed.get() {
            return;
        }
        let Some(view) = self.view.upgrade() else {
            return;
        };
        if popup_widget(&view).is_none() {
            self.emit_request(&view, Request::Prime);
            view.completion().hide();
        }
        // GtkSourceView ignores requests while the view does not have the focus: try again then.
        self.primed.set(popup_widget(&view).is_some());
    }

    /// Remembers that `c` is being typed; right after the key (before the next key and the next
    /// frame) the popup is updated for it.
    fn expect_char(self: &Rc<Self>, c: char) {
        let Some(doc) = self.doc.upgrade() else {
            return;
        };
        let scheduled = self.pending_key.replace(Some(PendingKey {
            c,
            version: doc.version(),
        }));
        if scheduled.is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(attached) = weak.upgrade() {
                attached.after_key();
            }
            glib::ControlFlow::Break
        });
    }

    /// The popup policy after a typed character (see the module docs).
    fn after_key(self: &Rc<Self>) {
        let Some(pending) = self.pending_key.take() else {
            return;
        };
        let (Some(view), Some(doc), Some(shared)) =
            (self.view.upgrade(), self.doc.upgrade(), self.shared())
        else {
            return;
        };
        let Some(state) = shared.state() else {
            return;
        };
        // The character must have arrived, right before the cursor (a typing helper may have
        // typed over a closing character instead, or the view refused the key).
        let buffer = view.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let mut before = cursor;
        if doc.version() == pending.version || !before.backward_char() || before.char() != pending.c
        {
            return;
        }
        let Some(position) = Position::at_cursor(&doc, &buffer) else {
            return;
        };
        let cfg = &state.config.completion;
        let req = CompletionRequest::new(&position.ctx, &position.line_before);
        let trigger = trigger_for(
            &shared.db.borrow(),
            &position.at_cursor,
            &req,
            pending.c,
            cfg.auto,
            cfg.min_word as usize,
        );
        let word_char = pending.c.is_alphanumeric() || pending.c == '_';
        let Some(trigger) = trigger else {
            return;
        };
        let Some(stash) = self.compute() else {
            self.hide();
            return;
        };
        let select = trigger != Trigger::Word && !stash.exact;
        if word_char && self.popup_visible() && self.selecting.get() == select {
            // GtkSourceView refilters the open popup itself (`refilter`, from the stash).
            self.stash.replace(Some(stash));
            return;
        }
        self.show_stash(stash, select);
    }

    /// Opens the popup at the cursor when there is something to propose: computes the
    /// proposals, keeps them for GtkSourceView's request, and emits `show-completion`
    /// (again from an idle callback if GtkSourceView did not show the popup: it never does on
    /// a view's first request). `select_first` selects the first row (Enter accepts it) unless
    /// the typed text already is a complete proposal (Enter then makes a new line). Returns
    /// whether there were proposals.
    fn show(self: &Rc<Self>, select_first: bool) -> bool {
        self.prime();
        match self.compute() {
            Some(stash) => {
                let select = select_first && !stash.exact;
                self.show_stash(stash, select)
            }
            None => {
                self.hide();
                false
            }
        }
    }

    /// [`Attached::show`] with proposals already computed.
    fn show_stash(self: &Rc<Self>, stash: Stash, select_first: bool) -> bool {
        let Some(view) = self.view.upgrade() else {
            return false;
        };
        if stash.items.is_empty() {
            self.hide();
            return false;
        }
        self.stash.replace(Some(stash));
        let completion = view.completion();
        if self.popup_visible() {
            // A new selection mode needs a new popup.
            completion.hide();
        }
        completion.set_select_on_show(select_first);
        self.selecting.set(select_first);
        self.emit_show(&view);
        self.retry_show(true);
        true
    }

    /// Shows the popup once more from an idle callback when GtkSourceView did not (a view's
    /// first request populates without displaying). `need_stash`: only while the proposals
    /// computed for the request still describe the cursor.
    fn retry_show(self: &Rc<Self>, need_stash: bool) {
        let weak = Rc::downgrade(self);
        let (cursor, version) = self.cursor_and_version().unwrap_or_default();
        glib::idle_add_local_once(move || {
            let Some(attached) = weak.upgrade() else {
                return;
            };
            let Some(view) = attached.view.upgrade() else {
                return;
            };
            let unchanged = if need_stash {
                attached.stash_is_current()
            } else {
                attached.cursor_and_version() == Some((cursor, version))
            };
            if !attached.popup_visible() && unchanged && view.has_focus() {
                attached.emit_show(&view);
            }
        });
    }

    /// The cursor offset and the document version.
    fn cursor_and_version(&self) -> Option<(i32, u64)> {
        let (view, doc) = (self.view.upgrade()?, self.doc.upgrade()?);
        let buffer = view.buffer();
        Some((
            buffer.iter_at_mark(&buffer.get_insert()).offset(),
            doc.version(),
        ))
    }

    fn emit_show(&self, view: &TexView) {
        self.emit_request(view, Request::Proposals);
    }

    /// Emits `show-completion`; its `populate` (later, see [`Request`]) fills by `request`.
    fn emit_request(&self, view: &TexView, request: Request) {
        self.request.set(Some(request));
        self.showing.set(true);
        view.emit_show_completion();
        self.showing.set(false);
    }

    /// Closes the popup.
    fn hide(&self) {
        if let Some(view) = self.view.upgrade()
            && self.popup_visible()
        {
            view.completion().hide();
        }
        self.stash.replace(None);
    }

    /// True while the completion popup of this view is shown.
    fn popup_visible(&self) -> bool {
        self.view
            .upgrade()
            .and_then(|view| popup_widget(&view))
            .is_some_and(|popup| popup.is_visible())
    }

    fn stash_is_current(&self) -> bool {
        let (Some(view), Some(doc)) = (self.view.upgrade(), self.doc.upgrade()) else {
            return false;
        };
        let buffer = view.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert()).offset();
        self.stash
            .borrow()
            .as_ref()
            .is_some_and(|s| s.version == doc.version() && s.cursor == cursor)
    }

    /// The proposals at the cursor, best first (at most `completion.max_items`); `None` when
    /// nothing can be completed there (a selection, a closed document, no window).
    fn compute(&self) -> Option<Stash> {
        let (view, doc, shared) = (self.view.upgrade()?, self.doc.upgrade()?, self.shared()?);
        let state = shared.state()?;
        let buffer = view.buffer();
        if buffer.has_selection() {
            return None;
        }
        let position = Position::at_cursor(&doc, &buffer)?;
        let req = CompletionRequest::new(&position.ctx, &position.line_before);
        let mut items = Vec::new();
        {
            let words = shared.words.borrow().clone();
            let index = state.workspace.index.borrow();
            shared.completer.borrow_mut().complete(
                &shared.db.borrow(),
                &state.snippets(),
                &index,
                &words,
                &req,
                &mut items,
            );
        }
        let typed = &position.line_before[req.replace_start()..];
        // After `\end{`, the environment that is open at the cursor comes first.
        let closing = argument_command(&position.line_before) == Some("end");
        if closing {
            // The text looked back at, in one slice (a GTK call per line cost as much as copying
            // the whole document): `OPEN_ENV_SCAN` characters hold at least as many bytes, and
            // `open_environment` keeps to its byte budget. From a character offset, not a line
            // start, so that a very long line cannot make it larger.
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            let mut start = cursor;
            let scan = i32::try_from(OPEN_ENV_SCAN).unwrap_or(i32::MAX);
            start.set_offset((cursor.offset() - scan).max(0));
            let before = buffer.slice(&start, &cursor, true);
            if let Some(open) = open_environment(before.rsplit('\n')) {
                promote_open_environment(&mut items, &open, typed);
            }
        }
        items.truncate(state.config.completion.max_items as usize);
        // The typed text is exactly a complete proposal (`\item`, `\end{equation`): no row is
        // preselected, so that Enter makes a new line.
        let exact = items.iter().any(|item| completes_nothing(item, typed))
            || (closing
                && items
                    .iter()
                    .any(|i| i.kind == ItemKind::Environment && i.label == typed));
        Some(Stash {
            version: doc.version(),
            cursor: buffer.iter_at_mark(&buffer.get_insert()).offset(),
            items,
            needle: req.prefix.to_owned(),
            exact,
        })
    }

    /// GtkSourceView asks for the proposals of a request: those of [`Request::Proposals`],
    /// nothing for [`Attached::prime`]'s request or one nobody made. The priming request runs
    /// after the view's first frame; filling it opened the popup by itself (the line-begin
    /// snippets right after the window opened).
    fn populate(&self, store: &gio::ListStore) {
        match self.request.take() {
            Some(Request::Proposals) => self.fill(store),
            Some(Request::Prime) | None => store.remove_all(),
        }
    }

    /// GtkSourceView refilters a request's model after the buffer or the cursor changed: only
    /// the open popup's (a hidden priming context is refiltered too).
    fn refilter(&self, store: &gio::ListStore) {
        if self.popup_visible() {
            self.fill(store);
        } else {
            store.remove_all();
        }
    }

    /// Fills `store` with the proposals at the cursor (from the stash when it is current).
    fn fill(&self, store: &gio::ListStore) {
        // The stash stays: a retried `show` asks again.
        let (items, needle) = if self.stash_is_current() {
            let stash = self.stash.borrow();
            stash
                .as_ref()
                .map(|s| (s.items.clone(), s.needle.clone()))
                .unwrap_or_default()
        } else {
            self.compute()
                .map(|s| (s.items, s.needle))
                .unwrap_or_default()
        };
        let objects: Vec<glib::Object> = items
            .into_iter()
            .map(|item| Proposal::new(item, &needle).upcast())
            .collect();
        store.splice(0, store.n_items(), &objects);
        self.model.replace(store.downgrade());
    }

    /// Accepts `item` at the cursor (see the module docs, "Accepting").
    fn accept(&self, item: &Item) {
        let (Some(view), Some(doc), Some(shared)) =
            (self.view.upgrade(), self.doc.upgrade(), self.shared())
        else {
            return;
        };
        let buffer = view.buffer();
        let Some(position) = Position::at_cursor(&doc, &buffer) else {
            return;
        };
        if !view.is_editable() {
            return;
        }
        let req = CompletionRequest::new(&position.ctx, &position.line_before);
        let typed = &position.line_before[req.replace_start()..];
        let next = position.line_after.chars().next();
        match &item.insert {
            Insert::Text(name)
                if item.kind == ItemKind::Environment
                    && argument_command(&position.line_before)
                        .is_some_and(|c| c.trim_end_matches('*') == "begin") =>
            {
                self.accept_environment(&shared, &doc, &position, name, typed);
            }
            Insert::Text(name)
                if item.kind == ItemKind::Environment
                    && argument_command(&position.line_before)
                        .is_some_and(|c| c.trim_end_matches('*') == "end") =>
            {
                let text = if next == Some('}') {
                    name.clone()
                } else {
                    format!("{name}}}")
                };
                self.driver
                    .as_undo_step(|| replace_before_cursor(&buffer, typed, &text));
            }
            Insert::Text(text) => self
                .driver
                .as_undo_step(|| replace_before_cursor(&buffer, typed, text)),
            // A command right before its arguments keeps them.
            Insert::Snippet(_)
                if matches!(item.kind, ItemKind::Command | ItemKind::UserMacro)
                    && matches!(next, Some('{' | '[')) =>
            {
                self.driver
                    .as_undo_step(|| replace_before_cursor(&buffer, typed, &item.label));
            }
            Insert::Snippet(chunks) => {
                if let Err(e) = self.driver.expand_chunks(chunks, typed) {
                    tracing::warn!("cannot insert {}: {e}", item.label);
                }
            }
        }
        shared.completer.borrow_mut().record_accepted(&item.label);
        self.stash.replace(None);
    }

    /// An environment chosen after `\begin{`: the whole environment when it is not closed
    /// further on, else just its name (and the closing brace).
    fn accept_environment(
        &self,
        shared: &Shared,
        doc: &Document,
        position: &Position,
        name: &str,
        typed: &str,
    ) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let buffer = view.buffer();
        let has_brace = position.line_after.starts_with('}');
        // The typing helpers' balance over the whole document (comments skipped), so that a
        // nested environment of the same name, or a commented-out `\end`, is no closing one.
        // A name that live rename links to its `\end` (the user is renaming an environment)
        // is closed by that `\end`, which follows the new name.
        let linked = self.editor.upgrade().is_some_and(|editor| {
            super::envtools::linked(&editor)
                .is_some_and(|l| l.name.start <= position.offset && position.offset <= l.name.end)
        });
        let closed = linked
            || doc.with_text(|text| {
                let from = position.offset.min(text.len());
                !super::typing::env_unclosed(text, from, name, false)
            });
        let begin = position.line_before.rfind("\\begin").filter(|_| !closed);
        let Some(begin) = begin else {
            let text = if has_brace {
                name.to_owned()
            } else {
                format!("{name}}}")
            };
            self.driver
                .as_undo_step(|| replace_before_cursor(&buffer, typed, &text));
            return;
        };
        let mut typed_begin = position.line_before[begin..].to_owned();
        if has_brace {
            // The brace the typing helpers paired is replaced with the rest: the cursor goes
            // after it, so the snippet replaces `\begin{name}` as a whole (one undo step).
            let mut after = buffer.iter_at_mark(&buffer.get_insert());
            after.forward_char();
            buffer.place_cursor(&after);
            typed_begin.push('}');
        }
        let chunks = shared.db.borrow().env_snippet(name);
        if let Err(e) = self.driver.expand_chunks(&chunks, &typed_begin) {
            tracing::warn!("cannot insert the environment {name}: {e}");
        }
    }
}

/// Whether accepting `item` where `typed` was typed would add nothing but trailing blanks: the
/// same text, or a snippet without stops to fill (`\item $0`). Environments are never complete
/// (accepting one adds its body and `\end`).
pub fn completes_nothing(item: &Item, typed: &str) -> bool {
    if item.kind == ItemKind::Environment {
        return false;
    }
    match &item.insert {
        Insert::Text(text) => text == typed,
        Insert::Snippet(chunks) => {
            let mut text = String::new();
            for chunk in chunks {
                match chunk {
                    Chunk::Text(t) => text.push_str(t),
                    Chunk::Stop { n: 0, default } if default.is_empty() => {}
                    _ => return false,
                }
            }
            text.trim_end() == typed
        }
    }
}

/// Replaces `typed` (the text right before the cursor) with `text` as one undo step.
fn replace_before_cursor(buffer: &gtk::TextBuffer, typed: &str, text: &str) {
    let mut end = buffer.iter_at_mark(&buffer.get_insert());
    let mut start = end;
    start.backward_chars(i32::try_from(typed.chars().count()).unwrap_or(0));
    if buffer.slice(&start, &end, true) != typed {
        return;
    }
    buffer.begin_user_action();
    if start != end {
        buffer.delete(&mut start, &mut end);
    }
    buffer.insert(&mut start, text);
    buffer.end_user_action();
}

/// Moves the environment `open` to the front of `items` (adding it when the tables do not have
/// it), when `typed` (the name typed so far after `\end{`) is a prefix of it.
fn promote_open_environment(items: &mut Vec<Item>, open: &str, typed: &str) {
    if !open.starts_with(typed) {
        return;
    }
    let best = items.first().map_or(0, |i| i.score);
    let mut item = match items
        .iter()
        .position(|i| i.kind == ItemKind::Environment && i.label == open)
    {
        Some(at) => items.remove(at),
        None => Item {
            label: open.to_owned(),
            insert: Insert::Text(open.to_owned()),
            kind: ItemKind::Environment,
            detail: Some("open environment".to_owned()),
            glyph: None,
            score: best,
        },
    };
    item.score = item.score.max(best) + 1;
    items.insert(0, item);
}

/// Bytes looked back for the environment open at a position.
const OPEN_ENV_SCAN: usize = 256 << 10;

/// The innermost environment open at the end of a text, given its lines from the last one up
/// (each without its line break): its `\begin{name}` has no `\end{name}` after it. A backward
/// scan of at most [`OPEN_ENV_SCAN`] bytes, which usually stops a few lines up; comments are
/// skipped.
pub fn open_environment<S: AsRef<str>>(lines: impl IntoIterator<Item = S>) -> Option<String> {
    let mut left = OPEN_ENV_SCAN;
    let mut closed: Vec<String> = Vec::new();
    for line in lines {
        let line = line.as_ref();
        // The line where the budget ends counts from that byte on.
        let mut from = line.len().saturating_sub(left);
        while !line.is_char_boundary(from) {
            from += 1;
        }
        for (begin, name) in env_tokens(&line[from..]).into_iter().rev() {
            if !begin {
                closed.push(name.to_owned());
            } else if closed.is_empty() {
                return Some(name.to_owned());
            } else if let Some(i) = closed.iter().rposition(|n| n == name) {
                closed.truncate(i);
            } else {
                // A `\begin` whose `\end` is missing in between: it encloses the end.
                return Some(name.to_owned());
            }
        }
        // The line and the line break before it.
        left = left.checked_sub(line.len() + 1)?;
    }
    None
}

/// The `\begin{name}` (`true`) and `\end{name}` (`false`) of `line` before its comment.
fn env_tokens(line: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => break,
            b'\\' => {
                let rest = &line[i + 1..];
                let word = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                if word == 0 {
                    // `\\`, `\%`, …: the escaped character is skipped.
                    i += 2;
                    continue;
                }
                let begin = match &rest[..word] {
                    "begin" => Some(true),
                    "end" => Some(false),
                    _ => None,
                };
                if let Some(begin) = begin
                    && let Some(arg) = rest[word..]
                        .trim_start_matches([' ', '\t'])
                        .strip_prefix('{')
                    && let Some(close) = arg.find('}')
                    && close > 0
                    && !arg[..close].contains(['{', '\\', '%'])
                {
                    out.push((begin, &arg[..close]));
                }
                i += 1 + word;
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether `rest` (the text after an unfinished `\begin{name`) contains the `\end{name}` that
/// closes it: one that is not matched by a later `\begin{name}` first. (A forward scan only;
/// accepting an environment uses the typing helpers' whole-document balance instead.)
pub fn environment_closed(rest: &str, name: &str) -> bool {
    let begin = format!("\\begin{{{name}}}");
    let end = format!("\\end{{{name}}}");
    let mut depth = 0usize;
    let mut at = 0;
    loop {
        let next_begin = rest[at..].find(&begin).map(|i| at + i);
        let next_end = rest[at..].find(&end).map(|i| at + i);
        match (next_begin, next_end) {
            (Some(b), Some(e)) if b < e => {
                depth += 1;
                at = b + begin.len();
            }
            (_, Some(e)) => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
                at = e + end.len();
            }
            (_, None) => return false,
        }
    }
}

/// The cursor's line and contexts, as completion needs them.
struct Position {
    /// Byte offset of the cursor.
    offset: usize,
    /// The line up to the cursor.
    line_before: String,
    /// The line after the cursor (without its line break).
    line_after: String,
    /// The scanner context at the cursor.
    at_cursor: Context,
    /// The context for the request: inside a command name the one at its backslash, after
    /// `\begin{`/`\end{` the one at the backslash of `\begin` (see `CompletionRequest`).
    ctx: Context,
}

impl Position {
    fn at_cursor(doc: &Document, buffer: &gtk::TextBuffer) -> Option<Self> {
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let offset = doc.byte_offset(&cursor);
        let (line, col) = doc.line_index().position(offset);
        let line_start = doc.line_index().line_start(line);
        let mut text = doc.line_text(line)?;
        if text.ends_with('\r') {
            text.pop();
        }
        let col = (col as usize).min(text.len());
        if !text.is_char_boundary(col) {
            return None;
        }
        let line_after = text.split_off(col);
        let line_before = text;
        let at_cursor = doc.context_at_byte(offset);
        let ctx = match &at_cursor {
            Context::CommandName { start } => doc.context_at_byte(line_start + *start as usize),
            // `\end{` closes whatever is open, math-starting environments included: no mode.
            Context::Arg {
                cmd,
                kind: ArgKind::Env,
                start,
                ..
            } if &**cmd == "begin" => {
                let head = line_before
                    .get(..(*start as usize).min(line_before.len()))
                    .unwrap_or("");
                match head.rfind(&format!("\\{cmd}")) {
                    Some(backslash) => doc.context_at_byte(line_start + backslash),
                    None => at_cursor.clone(),
                }
            }
            _ => at_cursor.clone(),
        };
        Some(Self {
            offset,
            line_before,
            line_after,
            at_cursor,
            ctx,
        })
    }
}

/// GtkSourceView's completion popup of `view`, once it exists (a popover child of the view).
fn popup_widget(view: &TexView) -> Option<gtk::Widget> {
    let mut child = view.first_child();
    while let Some(widget) = child {
        if widget.is::<gtk::Popover>() && widget.type_().name().starts_with("GtkSourceCompletion") {
            return Some(widget);
        }
        child = widget.next_sibling();
    }
    None
}

// ───────────── API for tests and the demo ─────────────

fn attached_of(state: &WindowState, editor: &Editor) -> Option<Rc<Attached>> {
    let shared = shared_of(state)?;
    let editors = shared.editors.borrow();
    editors
        .iter()
        .find(|a| std::ptr::eq(a.editor.as_ptr(), editor))
        .cloned()
}

/// Opens the completion popup at the cursor of `editor` as a typed trigger would (first row
/// selected). False when there is nothing to propose there.
pub fn show(state: &WindowState, editor: &Editor) -> bool {
    attached_of(state, editor).is_some_and(|a| a.show(true))
}

/// Closes the popup of `editor`.
pub fn hide(state: &WindowState, editor: &Editor) {
    if let Some(attached) = attached_of(state, editor) {
        attached.hide();
    }
}

/// True while the completion popup of `editor` is shown.
pub fn popup_visible(editor: &Editor) -> bool {
    popup_widget(editor.view()).is_some_and(|p| p.is_visible())
}

/// GtkSourceView's popup widget of `editor`, once it was shown (for snapshots).
pub fn popup(editor: &Editor) -> Option<gtk::Widget> {
    popup_widget(editor.view())
}

/// The proposals the popup's model holds now (the last ones GtkSourceView asked for).
pub fn shown_items(state: &WindowState, editor: &Editor) -> Vec<Item> {
    let Some(attached) = attached_of(state, editor) else {
        return Vec::new();
    };
    let Some(store) = attached.model.borrow().upgrade() else {
        return Vec::new();
    };
    (0..store.n_items())
        .filter_map(|i| store.item(i))
        .filter_map(|o| o.downcast::<Proposal>().ok())
        .filter_map(|p| p.item())
        .collect()
}

/// The proposals at the cursor of `editor`, as the popup would show them.
pub fn items_at_cursor(state: &WindowState, editor: &Editor) -> Vec<Item> {
    attached_of(state, editor)
        .and_then(|a| a.compute())
        .map(|s| s.items)
        .unwrap_or_default()
}

/// Accepts `item` at the cursor of `editor`, as activating its proposal does.
pub fn accept(state: &WindowState, editor: &Editor, item: &Item) {
    if let Some(attached) = attached_of(state, editor) {
        attached.accept(item);
    }
}

/// What typing `c` just now (it is before the cursor) would open, or `None`.
pub fn trigger_at_cursor(state: &WindowState, editor: &Editor, c: char) -> Option<Trigger> {
    let shared = shared_of(state)?;
    let doc = editor.document();
    let position = Position::at_cursor(doc, editor.buffer().upcast_ref())?;
    let req = CompletionRequest::new(&position.ctx, &position.line_before);
    let cfg = &state.config.completion;
    trigger_for(
        &shared.db.borrow(),
        &position.at_cursor,
        &req,
        c,
        cfg.auto,
        cfg.min_word as usize,
    )
}

/// Whether the popup of `editor` (as last shown) selects its first row, so that Enter accepts
/// it.
pub fn popup_selects_first(state: &WindowState, editor: &Editor) -> bool {
    attached_of(state, editor).is_some_and(|a| a.selecting.get())
}

/// True while a snippet that the driver of `editor` expanded is active.
pub fn snippet_active(state: &WindowState, editor: &Editor) -> bool {
    attached_of(state, editor).is_some_and(|a| a.driver.is_active())
}

/// A typing replacement (`typing::replacement`) through the driver of `editor`: types `c`, then
/// replaces `typed` before the cursor with `with` as an undo step of its own
/// (`snippets::Driver::type_and_replace`), and closes the popup. False when `editor` has no
/// driver (nothing was typed).
pub fn type_and_replace(
    state: &WindowState,
    editor: &Editor,
    c: char,
    typed: &str,
    with: &str,
) -> bool {
    let Some(attached) = attached_of(state, editor) else {
        return false;
    };
    attached.hide();
    attached.driver.type_and_replace(c, typed, with)
}

/// Whether the project defines the macro `name`, as the `if_undefined` snippet bodies see it.
pub fn macro_defined(state: &WindowState, name: &str) -> bool {
    index_defines(&state.workspace.index.borrow(), name)
}

/// Re-indexes every open document now (without the usual delay); the project feed then
/// follows their includes. Use [`project_busy`] to wait for it.
pub fn refresh_project(state: &Rc<WindowState>) {
    let Some(shared) = shared_of(state) else {
        return;
    };
    for doc in state.workspace.documents() {
        if doc.is_open() {
            shared.index_document(&doc);
        }
    }
}

/// True while the project feed has work queued or running (re-indexing, reading files,
/// rebuilding words).
pub fn project_busy(state: &WindowState) -> bool {
    shared_of(state).is_some_and(|s| s.feed.busy())
}

/// True while the project index itself has work queued or running: documents waiting to be
/// re-indexed or being re-indexed, and files being read from disk. What waits for the index
/// (the label checks, the Outline) waits for this, not for the words, the dictionary or the
/// file list ([`project_busy`] counts those too).
pub fn index_busy(state: &WindowState) -> bool {
    shared_of(state).is_some_and(|s| s.feed.index_busy())
}

/// The file monitor saw the file at `path` change, appear or go, by another program: `path` is
/// normalized (outside the workspace folder, the monitor watches the directories where the
/// discovery jobs found the index's files on disk), or, for a file outside that no job read, as
/// the index names it. When the project index has it from disk (an included file that is not
/// open, a bibliography, open or not) or could not read it before (an `\input` written before
/// its file), it is read again on the jobs thread (a file that is gone leaves the index), and
/// the label checks and the Outline follow once the index has caught up. True when that happens.
/// Open LaTeX documents are indexed from their live text and need nothing. `own_write`: Lemma
/// wrote the file (a save), whose text the index has already (`labels::saved`), so it is read
/// only when the index could not read it before (deleted by another program, then saved).
pub fn disk_changed(state: &Rc<WindowState>, path: &Path, own_write: bool) -> bool {
    let Some(shared) = shared_of(state) else {
        return false;
    };
    if !shared.disk_changed(state, path, own_write) {
        return false;
    }
    crate::labels::index_changing(state);
    crate::outline_panel::refresh(state);
    true
}

/// Paths the file monitor saw created, deleted or moved in one batch (`true`: the path exists
/// now). When the project's file list (file arguments: `\includegraphics{`, `\input{`) lacks
/// one that exists or lists one that went, the project's files are listed again. A file
/// replaced by a rename onto it (a save) changes nothing.
pub fn files_moved(state: &WindowState, moved: &[(PathBuf, bool)]) {
    if let Some(shared) = shared_of(state) {
        shared.files_moved(state, moved);
    }
}

/// How many times the project's files were listed for file arguments (tests).
pub fn file_walks(state: &WindowState) -> u64 {
    shared_of(state).map_or(0, |s| s.feed.walks.get())
}

/// How many times the words of the files read from disk were collected (tests): when the
/// dictionary or the text of such a `.tex` file changed, or one came or went.
pub fn base_word_builds(state: &WindowState) -> u64 {
    shared_of(state).map_or(0, |s| s.feed.base_generation.get())
}

/// Reads the `.aux` (first the one beside `pdf`), the bibliographies and the project's file list
/// again, as every finished build does (`BuildController::connect_finished`).
pub fn refresh_after_build(state: &WindowState, pdf: Option<&Path>) {
    if let Some(shared) = shared_of(state) {
        shared.after_build(pdf);
    }
}

/// What keeps the project feed busy, in words (for error messages).
pub fn project_feed_state(state: &WindowState) -> String {
    shared_of(state).map_or_else(|| "no completion".to_owned(), |s| s.feed.describe())
}

/// The packages whose command tables are loaded for `state`'s window.
pub fn loaded_packages(state: &WindowState) -> Vec<&'static str> {
    shared_of(state)
        .map(|s| s.db.borrow().loaded_packages())
        .unwrap_or_default()
}

// ───────────── The project feed ─────────────

/// Keeps the window's `ProjectIndex`, words, file list and command tables up to date (see the
/// module docs).
#[derive(Default)]
struct Feed {
    /// Per-document re-index timers.
    timers: RefCell<HashMap<PathBuf, glib::SourceId>>,
    /// Jobs running on the jobs thread.
    jobs: Cell<u32>,
    /// A discovery pass is running; `again` asks for another one when it ends.
    discovering: Cell<bool>,
    again: Cell<bool>,
    /// Paths that could not be read, or that name an open document through a symbolic link
    /// (its live text is indexed under its own name): not read again until a build, until the
    /// file monitor sees them change, or (a link) until that document closes.
    unreadable: RefCell<HashSet<PathBuf>>,
    /// Directories already searched for `.tex` files.
    searched: RefCell<HashSet<PathBuf>>,
    /// Files indexed from disk.
    disk_files: RefCell<HashSet<PathBuf>>,
    /// `.tex` files found next to the open documents that are not (yet) known to belong to the
    /// project: they join the index once they include one of its files (a root of an open
    /// chapter), so unrelated documents never add their labels and macros.
    candidates: RefCell<BTreeMap<PathBuf, Candidate>>,
    /// Paths to read again even though they are indexed (after a build, a change on disk, or
    /// new theorem environments).
    stale: RefCell<HashSet<PathBuf>>,
    /// The document classes of indexed files (`\documentclass{…}`).
    classes: RefCell<HashMap<PathBuf, String>>,
    /// What the last [`Shared::index_changed`] followed besides the index: a re-index that
    /// leaves a document's index as it was skips it only while these are the same.
    followed: RefCell<Option<Followed>>,
    /// The directories of the files the index reads from disk, or would read (an `\input` of
    /// a file that does not exist yet), as the last [`Shared::prune`] kept them, where they are
    /// on disk ([`Feed::real_paths`]): those outside the workspace folder are watched for the
    /// index ([`Shared::watch_outside`]).
    disk_dirs: RefCell<BTreeSet<PathBuf>>,
    /// Where each file a discovery job read or tried to read is on disk, by the path as the
    /// index names it: resolved on the jobs thread, so a file through a `refs` link in the
    /// folder to a central folder is in that folder (which the folder's watcher does not see),
    /// and a `refs.bib` linked to a reference manager's export is that export.
    real_paths: RefCell<HashMap<PathBuf, PathBuf>>,
    /// The root whose `.aux` is loaded.
    aux_root: RefCell<Option<PathBuf>>,
    /// The `.aux` beside the PDF of the last build, read before the usual places.
    aux_hint: RefCell<Option<PathBuf>>,
    /// The directory the project's files were listed from (`None`: list them again), and how
    /// many times they were listed (only the newest listing is kept).
    walked_dir: RefCell<Option<PathBuf>>,
    walks: Cell<u64>,
    /// Paths whose re-index job runs; a path in `index_again` is indexed once more when its job
    /// ends (one job per path at a time, so an older text never lands after a newer one).
    indexing: RefCell<HashSet<PathBuf>>,
    index_again: RefCell<HashSet<PathBuf>>,
    /// Bumped by every re-index job; the generation of the newest result merged per path.
    index_generation: Cell<u64>,
    merged_generation: RefCell<HashMap<PathBuf, u64>>,
    /// The project's theorem environments as the index last had them, and the ones each file
    /// (indexed or a candidate) was indexed with: a file indexed without a name it uses is
    /// indexed again ([`Shared::follow_theorems`]).
    theorems: RefCell<Theorems>,
    indexed_with: RefCell<HashMap<PathBuf, Theorems>>,
    /// The package names last loaded into the tables (they are rebuilt when one goes away).
    packages: RefCell<Vec<String>>,
    dictionary_lists: RefCell<Vec<PathBuf>>,
    /// Dictionary entries only.
    dictionary: RefCell<Arc<WordList>>,
    /// The dictionary plus the words of the project files read from disk.
    base_words: RefCell<Arc<WordList>>,
    /// A hash of the text of each file a discovery job read (indexed or a candidate), and what
    /// the base words were last built from: a discovery pass that changes no such text (the
    /// `.aux` and the bibliographies read after a build, a root read again unchanged) does not
    /// read the project's files for their words again.
    text_hashes: RefCell<HashMap<PathBuf, u64>>,
    base_inputs: RefCell<Option<BaseInputs>>,
    /// A `.tex` file the base words read may hold another text than the one hashed, as no
    /// discovery job read it: its document closed (saved or reloaded while open, when the jobs
    /// skip it), or Lemma saved it closed (the index takes the saved text, `labels::saved`). The
    /// base words are collected again on the next pass that reads from disk.
    base_stale: Cell<bool>,
    /// Bumped by every base words job, so an older one that ends last is dropped (and counts
    /// them, for tests).
    base_generation: Cell<u64>,
    words_timer: RefCell<Option<glib::SourceId>>,
    /// Bumped by every job start, so stale word rebuilds are dropped.
    words_generation: Cell<u64>,
}

impl Feed {
    /// What keeps the feed busy (for error messages).
    fn describe(&self) -> String {
        format!(
            "{} jobs, {} re-indexing, discovering {}, {} re-index timers, words timer {}",
            self.jobs.get(),
            self.indexing.borrow().len(),
            self.discovering.get(),
            self.timers.borrow().len(),
            self.words_timer.borrow().is_some()
        )
    }

    fn busy(&self) -> bool {
        self.index_busy() || self.jobs.get() > 0 || self.words_timer.borrow().is_some()
    }

    /// Work on the index itself: re-index timers and jobs, and discovery passes (see
    /// [`index_busy`]).
    fn index_busy(&self) -> bool {
        !self.timers.borrow().is_empty()
            || !self.indexing.borrow().is_empty()
            || !self.index_again.borrow().is_empty()
            || self.discovering.get()
    }
}

/// A file's index and document class (`None`: the file could not be read).
type IndexedFile = Option<(FileIndex, Option<String>)>;

/// Theorem environment names (`ProjectIndex::theorem_names`), shared by the files indexed with
/// them.
type Theorems = Arc<Vec<String>>;

/// A possible root found next to an open document: its index and document class.
type Candidate = (Arc<FileIndex>, Option<String>);

/// What [`Shared::index_changed`] follows besides the index: the project root and the open
/// LaTeX documents (their candidates are promoted, their roots' packages and `.aux` loaded).
type Followed = (Option<PathBuf>, HashSet<PathBuf>);

/// What the base words are built from: the dictionary, and the `.tex` files read from disk with
/// the hashes of their texts, by path.
type BaseInputs = (Arc<WordList>, Vec<(PathBuf, u64)>);

/// A file a discovery job read: `key`, its path normalized on the jobs thread (the key of the
/// document it would be, when it was read; else the path); `real`, where it is on disk (also
/// when it could not be read: through its directory); `listed` when it was found by listing a
/// directory (a possible root), not through an include; `hash`, of its text ([`text_hash`]).
struct ReadFile {
    path: PathBuf,
    key: PathBuf,
    real: PathBuf,
    read: IndexedFile,
    listed: bool,
    hash: u64,
}

/// What a discovery job read; each bibliography with where it is on disk.
#[derive(Default)]
struct Discovered {
    files: Vec<ReadFile>,
    bibs: Vec<(PathBuf, PathBuf, Option<Vec<BibEntry>>)>,
    aux: Option<(PathBuf, AuxInfo)>,
    searched: Vec<PathBuf>,
    /// The theorem environments the files were indexed with.
    theorems: Theorems,
}

/// What a discovery job should read.
#[derive(Default)]
struct Wanted {
    files: Vec<PathBuf>,
    bibs: Vec<PathBuf>,
    /// The root and its `.aux` candidates, first existing wins.
    aux: Option<(PathBuf, Vec<PathBuf>)>,
    dirs: Vec<PathBuf>,
    /// Files already indexed, kept as candidates, or open: not read again when listed.
    known: HashSet<PathBuf>,
    /// The file stems of the open documents: a listed file can only be their root if it names
    /// one of them (`\input{chapters/yoneda}`), so other files are not indexed at all.
    open_stems: Vec<String>,
    theorems: Theorems,
    budget: usize,
}

impl Wanted {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.bibs.is_empty() && self.aux.is_none() && self.dirs.is_empty()
    }
}

impl Shared {
    /// Re-indexes `path`'s document [`REINDEX_DELAY`] after its last change.
    fn schedule_reindex(self: &Rc<Self>, path: &Path) {
        let path = path.to_path_buf();
        if let Some(old) = self.feed.timers.borrow_mut().remove(&path) {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let key = path.clone();
        let id = glib::timeout_add_local_once(REINDEX_DELAY, move || {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.timers.borrow_mut().remove(&key);
            if let Some(doc) = shared.state().and_then(|s| s.workspace.get(&key)) {
                shared.index_document(&doc);
            }
        });
        self.feed.timers.borrow_mut().insert(path, id);
    }

    /// Indexes `doc`'s current text on the jobs thread, then merges it (LaTeX documents only).
    fn index_document(self: &Rc<Self>, doc: &DocRef) {
        let Some(state) = self.state() else {
            return;
        };
        if !is_latex(doc.path()) {
            return;
        }
        if let Some(old) = self.feed.timers.borrow_mut().remove(doc.path()) {
            old.remove();
        }
        let path = doc.path().to_path_buf();
        // One job per path: a job that runs gets one more pass with the then current text.
        if self.feed.indexing.borrow().contains(&path) {
            self.feed.index_again.borrow_mut().insert(path);
            return;
        }
        self.feed.indexing.borrow_mut().insert(path.clone());
        let generation = self.feed.index_generation.get() + 1;
        self.feed.index_generation.set(generation);
        let text = doc.text();
        let theorems = self.current_theorems(&state.workspace.index.borrow());
        let used = theorems.clone();
        // The index's entry now: the job tells whether the new one is the same.
        let old = state.workspace.index.borrow().files.get(&path).cloned();
        let weak = Rc::downgrade(self);
        self.feed.jobs.set(self.feed.jobs.get() + 1);
        glib::spawn_future_local(async move {
            let indexed = gio::spawn_blocking(move || {
                let class = document_class(&text).map(str::to_owned);
                let index = index_file_with_theorems(&text, &used);
                let same = old.filter(|old| **old == index);
                (index, class, same)
            })
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.jobs.set(shared.feed.jobs.get() - 1);
            shared.feed.indexing.borrow_mut().remove(&path);
            let again = shared.feed.index_again.borrow_mut().remove(&path);
            let open = shared
                .state()
                .and_then(|state| state.workspace.get(&path))
                .filter(|doc| doc.is_open());
            let newer_merged = shared
                .feed
                .merged_generation
                .borrow()
                .get(&path)
                .is_some_and(|merged| *merged > generation);
            if let Ok((index, class, same)) = indexed
                && let Some(state) = shared.state()
                && open.is_some()
                && !newer_merged
            {
                shared
                    .feed
                    .merged_generation
                    .borrow_mut()
                    .insert(path.clone(), generation);
                let unchanged =
                    shared.unchanged(&state, &path, same.as_ref(), class.as_deref(), &theorems);
                shared
                    .feed
                    .indexed_with
                    .borrow_mut()
                    .insert(path.clone(), theorems);
                if unchanged {
                    // Prose typed: the structure is as it was. The index keeps the file's entry,
                    // so the Outline and the label checks have nothing to look at again; only
                    // the words follow the text.
                    shared.schedule_words();
                } else {
                    shared.merge_file(path, index, class);
                    shared.index_changed(false);
                }
            }
            if again && let Some(doc) = open {
                shared.index_document(&doc);
            }
        });
    }

    /// Whether a re-index of the open document at `path` changes nothing: its new index is the
    /// same as `same` (the entry the job compared it with), which the index still holds, with
    /// the same document class, the job ran with the project's current theorem environments
    /// (`used`: names that changed while it ran made [`Self::follow_theorems`] leave the
    /// running job to its merge), and the project root and the open documents are what the
    /// last [`Self::index_changed`] followed (a tab just opened, or another root made current,
    /// still has its candidates promoted and its packages and `.aux` loaded).
    fn unchanged(
        &self,
        state: &WindowState,
        path: &Path,
        same: Option<&Arc<FileIndex>>,
        class: Option<&str>,
        used: &Theorems,
    ) -> bool {
        let Some(same) = same else {
            return false;
        };
        state
            .workspace
            .index
            .borrow()
            .files
            .get(path)
            .is_some_and(|now| Arc::ptr_eq(now, same))
            && self.feed.classes.borrow().get(path).map(String::as_str) == class
            && Arc::ptr_eq(used, &self.feed.theorems.borrow())
            && self.feed.followed.borrow().as_ref() == Some(&self.followed(state))
    }

    /// What [`Self::index_changed`] follows now besides the index (see [`Followed`]).
    fn followed(&self, state: &WindowState) -> Followed {
        (self.project_root(state), open_latex_documents(state))
    }

    fn merge_file(&self, path: PathBuf, index: FileIndex, class: Option<String>) {
        let Some(state) = self.state() else {
            return;
        };
        match class {
            Some(class) => self.feed.classes.borrow_mut().insert(path.clone(), class),
            None => self.feed.classes.borrow_mut().remove(&path),
        };
        state
            .workspace
            .index
            .borrow_mut()
            .update_file(path, Arc::new(index));
    }

    /// After the index changed: load the packages it uses, follow its includes, and refresh
    /// the words (`disk` = files read from disk changed, so the base words change too).
    fn index_changed(self: &Rc<Self>, disk: bool) {
        let Some(state) = self.state() else {
            return;
        };
        let disk = self.promote_candidates(&state) | self.prune(&state) || disk;
        let mut packages = state.workspace.index.borrow().packages();
        if let Some(class) = self
            .project_root(&state)
            .and_then(|root| self.feed.classes.borrow().get(&root).cloned())
        {
            packages.push(format!("class-{class}"));
        }
        {
            // Tables only grow: when a package went away, start again from the built-in ones.
            let mut db = self.db.borrow_mut();
            if self
                .feed
                .packages
                .borrow()
                .iter()
                .any(|p| !packages.contains(p))
            {
                *db = CompletionDb::builtin();
            }
            db.load_packages(&packages);
        }
        self.feed.packages.replace(packages);
        self.follow_theorems(&state);
        // The root found by now may live elsewhere than the directory listed so far (or the
        // workspace root changed).
        let walked = self.feed.walked_dir.borrow().clone();
        if walked.is_none_or(|dir| dir != self.root_dir(&state)) {
            self.walk_files();
        }
        self.discover();
        self.watch_outside(&state);
        if !disk {
            self.schedule_words();
        } else if self.base_words_current() {
            // No text read from disk changed: only the open documents' words are added again.
            self.rebuild_words();
        } else {
            self.rebuild_base_words();
        }
        self.feed.followed.replace(Some(self.followed(&state)));
    }

    /// Has the file monitor watch the directories outside the workspace folder that hold files
    /// the index reads from disk ([`Feed::disk_dirs`]): a central `.bib`, a chapter included
    /// through `../` or through a symbolic link in the folder (the folder's watcher does not
    /// follow links), a root above the folder. Their changes then reach the index as those of
    /// the files inside it do.
    fn watch_outside(&self, state: &Rc<WindowState>) {
        let root = state.workspace.root().path().to_path_buf();
        let dirs: BTreeSet<PathBuf> = self
            .feed
            .disk_dirs
            .borrow()
            .iter()
            .filter(|dir| !dir.starts_with(&root))
            .cloned()
            .collect();
        crate::agent::watcher::watch_outside(state, &dirs);
    }

    /// The project's theorem environments in `index`, shared with the files already indexed
    /// with the same names.
    fn current_theorems(&self, index: &ProjectIndex) -> Theorems {
        let names = index.theorem_names();
        let mut theorems = self.feed.theorems.borrow_mut();
        if **theorems != names {
            *theorems = Arc::new(names);
        }
        theorems.clone()
    }

    /// Indexes again the files that were indexed without a theorem environment they use, or
    /// with one the project no longer declares: a chapter read before (or together with) the
    /// preamble file that holds its `\newtheorem`s, or a declaration typed or removed. Open
    /// documents are indexed now, the others on the next discovery pass. This converges: what a
    /// file declares does not depend on the names it is indexed with.
    fn follow_theorems(self: &Rc<Self>, state: &WindowState) {
        let mut open = Vec::new();
        {
            let index = state.workspace.index.borrow();
            let names = self.current_theorems(&index);
            let mut indexed_with = self.feed.indexed_with.borrow_mut();
            let mut stale = self.feed.stale.borrow_mut();
            for (path, file) in &index.files {
                let Some(with) = indexed_with.get_mut(path) else {
                    continue;
                };
                if Arc::ptr_eq(with, &names) {
                    continue;
                }
                let changed =
                    |name: &str| with.iter().any(|n| n == name) != names.iter().any(|n| n == name);
                let uses_changed = file.envs.iter().any(|env| {
                    changed(&env.name) || env.name.strip_suffix('*').is_some_and(changed)
                });
                if !uses_changed {
                    // Its index is what the current names give.
                    *with = names.clone();
                } else if let Some(doc) = state.workspace.get(path).filter(|d| d.is_open()) {
                    // A running job's result is looked at again when it is merged.
                    if !self.feed.indexing.borrow().contains(path) {
                        open.push(doc);
                    }
                } else {
                    stale.insert(path.clone());
                }
            }
        }
        for doc in open {
            self.index_document(&doc);
        }
    }

    /// The project's root document: the last build's, else an indexed root that includes the
    /// active document (or is it), else any open root.
    fn project_root(&self, state: &WindowState) -> Option<PathBuf> {
        let index = state.workspace.index.borrow();
        // The build's root, unless it is another project's (whose documents were all closed
        // since): indexed, or in a directory above an open document.
        if let Some(root) = state.build.get().and_then(|b| b.root())
            && (index.files.contains_key(&root)
                || root.parent().is_some_and(|dir| {
                    state
                        .workspace
                        .documents()
                        .iter()
                        .any(|d| d.is_open() && d.path().starts_with(dir))
                }))
        {
            return Some(root);
        }
        let roots: Vec<&PathBuf> = index
            .files
            .iter()
            .filter(|(_, f)| f.has_documentclass)
            .map(|(p, _)| p)
            .collect();
        let active = state
            .active_editor()
            .map(|e| e.document().path().to_path_buf());
        if let Some(active) = &active
            && let Some(root) = roots
                .iter()
                .find(|r| index.project_files(r).iter().any(|p| p == active))
        {
            return Some((*root).clone());
        }
        roots
            .iter()
            .find(|r| state.workspace.get(r).is_some())
            .or(roots.first())
            .map(|r| (*r).clone())
    }

    /// Reads what the index refers to and does not have yet (see [`Wanted`]).
    fn discover(self: &Rc<Self>) {
        if self.feed.discovering.get() {
            self.feed.again.set(true);
            return;
        }
        let Some(state) = self.state() else {
            return;
        };
        let wanted = self.wanted(&state);
        if wanted.is_empty() {
            return;
        }
        self.feed.discovering.set(true);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = gio::spawn_blocking(move || read_wanted(wanted)).await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.discovering.set(false);
            let changed = match found {
                Ok(found) => shared.merge_discovered(found),
                Err(_) => false,
            };
            if changed || shared.feed.again.replace(false) {
                shared.index_changed(true);
            }
        });
    }

    fn wanted(&self, state: &WindowState) -> Wanted {
        let index = state.workspace.index.borrow();
        let unreadable = self.feed.unreadable.borrow();
        let stale = self.feed.stale.borrow();
        let budget = MAX_DISK_FILES.saturating_sub(self.feed.disk_files.borrow().len());
        let mut wanted = Wanted {
            theorems: self.current_theorems(&index),
            budget,
            ..Wanted::default()
        };
        // Open documents are indexed from their live text. They are looked up by the path as the
        // index names it, without the file system (`Workspace::get` would resolve the path of
        // every package of the distribution after each build); one reached through a symbolic
        // link is told by the job's `key` (see `merge_discovered`).
        let open = open_documents(state);
        let mut seen = HashSet::new();
        let mut want_file = |path: PathBuf, wanted: &mut Wanted| {
            if (!index.files.contains_key(&path) || stale.contains(&path))
                && !unreadable.contains(&path)
                && !open.contains(&path)
                && seen.insert(path.clone())
            {
                wanted.files.push(path);
            }
        };
        // Includes reachable from every indexed file, and `% !TEX root` files.
        for (path, file) in &index.files {
            for included in index.project_files(path).into_iter().skip(1) {
                want_file(included, &mut wanted);
            }
            if let Some(root) = magic_root(path, file) {
                want_file(root, &mut wanted);
            }
        }
        for path in stale.iter() {
            if index.files.contains_key(path) && path.extension().is_some_and(|e| e == "tex") {
                want_file(path.clone(), &mut wanted);
            }
        }
        // Local packages and classes, for their macros and theorems. Most packages are the
        // distribution's: their reads fail once and stay in `unreadable`. After the includes,
        // so that they never take the budget from them.
        let root_dirs = root_dirs(&index);
        let classes = self.feed.classes.borrow();
        for (path, file) in &index.files {
            let class = classes.get(path).map(String::as_str);
            for style in local_styles(path, file, class, &root_dirs) {
                want_file(style, &mut wanted);
            }
        }
        drop(classes);
        // Files read again count against the cap already: they are read first, on a budget of
        // their own, so that new files never take it and go past the cap.
        let disk_files = self.feed.disk_files.borrow();
        wanted.files.sort_by_key(|p| !disk_files.contains(p));
        wanted.budget += wanted
            .files
            .iter()
            .take_while(|p| disk_files.contains(*p))
            .count();
        drop(disk_files);
        // Bibliographies.
        let mut bib_seen = HashSet::new();
        for (path, file) in &index.files {
            for bib in bibliographies(path, file) {
                if (!index.bibs.contains_key(&bib) || stale.contains(&bib))
                    && !unreadable.contains(&bib)
                    && bib_seen.insert(bib.clone())
                {
                    wanted.bibs.push(bib);
                }
            }
        }
        wanted.known.extend(index.files.keys().cloned());
        wanted
            .known
            .extend(self.feed.candidates.borrow().keys().cloned());
        // The root's `.aux`.
        drop(index);
        if let Some(root) = self.project_root(state)
            && (self.feed.aux_root.borrow().as_ref() != Some(&root) || stale.contains(&root))
        {
            let dir = root.parent().unwrap_or(Path::new("/")).to_path_buf();
            let stem = root
                .file_stem()
                .map(|s| s.to_os_string())
                .unwrap_or_default();
            let mut aux_name = PathBuf::from(stem);
            aux_name.set_extension("aux");
            let outdir = state.config.build.outdir.clone();
            let mut candidates: Vec<PathBuf> =
                self.feed.aux_hint.borrow().iter().cloned().collect();
            candidates.extend([dir.join(&outdir).join(&aux_name), dir.join(&aux_name)]);
            wanted.aux = Some((root, candidates));
        }
        // Directories of open documents and a few above them, for their root.
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let searched = self.feed.searched.borrow();
        for doc in state.workspace.documents() {
            wanted.known.insert(doc.path().to_path_buf());
            if doc.is_open()
                && let Some(stem) = doc.path().file_stem().and_then(|s| s.to_str())
                && !wanted.open_stems.iter().any(|s| s == stem)
            {
                wanted.open_stems.push(stem.to_owned());
            }
            let Some(dir) = doc.path().parent() else {
                continue;
            };
            for dir in dir.ancestors().take(NEARBY_LEVELS + 1) {
                if dir.parent().is_none()
                    || home
                        .as_deref()
                        .is_some_and(|h| h.starts_with(dir) && h != dir)
                {
                    break;
                }
                if !searched.contains(dir) && !wanted.dirs.iter().any(|d| d == dir) {
                    wanted.dirs.push(dir.to_path_buf());
                }
                if home.as_deref() == Some(dir) {
                    break;
                }
            }
        }
        wanted
    }

    /// Merges what a discovery job read; true when the index gained something.
    fn merge_discovered(&self, found: Discovered) -> bool {
        let Some(state) = self.state() else {
            return false;
        };
        let mut changed = false;
        self.feed.searched.borrow_mut().extend(found.searched);
        let open = open_documents(&state);
        for ReadFile {
            path,
            key,
            real,
            read,
            listed,
            hash,
        } in found.files
        {
            self.feed.stale.borrow_mut().remove(&path);
            self.feed.real_paths.borrow_mut().insert(path.clone(), real);
            if open.contains(&key) {
                // The live text is indexed instead. A path that names it through a symbolic
                // link is not read again on every pass.
                if key != path {
                    self.feed.unreadable.borrow_mut().insert(path);
                }
                continue;
            }
            match read {
                Some((index, class)) => {
                    self.feed
                        .text_hashes
                        .borrow_mut()
                        .insert(path.clone(), hash);
                    self.feed
                        .indexed_with
                        .borrow_mut()
                        .insert(path.clone(), found.theorems.clone());
                    if listed {
                        self.feed
                            .candidates
                            .borrow_mut()
                            .insert(path, (Arc::new(index), class));
                    } else {
                        self.feed.disk_files.borrow_mut().insert(path.clone());
                        self.merge_file(path, index, class);
                        changed = true;
                    }
                }
                None => {
                    // Gone (deleted, or no longer readable): what it had leaves the index.
                    self.feed.text_hashes.borrow_mut().remove(&path);
                    if state
                        .workspace
                        .index
                        .borrow_mut()
                        .files
                        .remove(&path)
                        .is_some()
                    {
                        self.feed.disk_files.borrow_mut().remove(&path);
                        self.feed.classes.borrow_mut().remove(&path);
                        self.feed.indexed_with.borrow_mut().remove(&path);
                        changed = true;
                    }
                    self.feed.unreadable.borrow_mut().insert(path);
                }
            }
        }
        changed |= self.promote_candidates(&state);
        for (path, real, entries) in found.bibs {
            self.feed.stale.borrow_mut().remove(&path);
            self.feed.real_paths.borrow_mut().insert(path.clone(), real);
            let mut index = state.workspace.index.borrow_mut();
            match entries {
                Some(entries) => {
                    index.bibs.insert(path, Arc::new(entries));
                    changed = true;
                }
                None => {
                    changed |= index.bibs.remove(&path).is_some();
                    self.feed.unreadable.borrow_mut().insert(path);
                }
            }
        }
        if let Some((root, aux)) = found.aux {
            self.feed.stale.borrow_mut().remove(&root);
            let new = state.workspace.index.borrow().aux != aux;
            state.workspace.index.borrow_mut().aux = aux;
            self.feed.aux_root.replace(Some(root));
            changed |= new;
        }
        changed
    }

    /// Moves the candidates whose project tree (their includes, followed through the index and
    /// the other candidates) holds an open document into the index; true when one moved. Sharing
    /// an included file (a `macros.tex`) with an open document is not enough: a sibling document
    /// (slides, an old draft) never adds its labels and macros.
    fn promote_candidates(&self, state: &WindowState) -> bool {
        let mut moved = false;
        loop {
            if self.feed.candidates.borrow().is_empty() {
                return moved;
            }
            let project = open_latex_documents(state);
            let mut all = state.workspace.index.borrow().clone();
            for (path, (file, _)) in self.feed.candidates.borrow().iter() {
                all.update_file(path.clone(), file.clone());
            }
            let promote: Vec<PathBuf> = self
                .feed
                .candidates
                .borrow()
                .keys()
                .filter(|c| {
                    all.project_files(c)
                        .iter()
                        .skip(1)
                        .any(|f| project.contains(f))
                })
                .cloned()
                .collect();
            if promote.is_empty() {
                return moved;
            }
            for path in promote {
                let Some((file, class)) = self.feed.candidates.borrow_mut().remove(&path) else {
                    continue;
                };
                self.feed.disk_files.borrow_mut().insert(path.clone());
                match class {
                    Some(class) => self.feed.classes.borrow_mut().insert(path.clone(), class),
                    None => self.feed.classes.borrow_mut().remove(&path),
                };
                state.workspace.index.borrow_mut().update_file(path, file);
                moved = true;
            }
        }
    }

    /// Drops from the index, the bibliographies, the candidates and the bookkeeping what no
    /// open document reaches any more: the files of a closed tab's project, an `\input` that was
    /// removed, a renamed file. Kept are the open documents, the indexed files whose project
    /// contains one (their roots), the `% !TEX root` files of those, everything these include,
    /// and the local packages and classes all of them load. True when files were dropped.
    fn prune(&self, state: &WindowState) -> bool {
        let open = open_latex_documents(state);
        let mut index = state.workspace.index.borrow_mut();
        let mut roots: Vec<PathBuf> = index
            .files
            .keys()
            .filter(|path| {
                open.contains(*path) || index.project_files(path).iter().any(|f| open.contains(f))
            })
            .cloned()
            .collect();
        let magic: Vec<PathBuf> = roots
            .iter()
            .filter_map(|path| magic_root(path, index.files.get(path)?))
            .collect();
        roots.extend(magic);
        let mut keep: HashSet<PathBuf> = open.clone();
        for root in &roots {
            keep.extend(index.project_files(root));
        }
        // The local packages and classes the kept files load (see `wanted`), what those include
        // and load in turn.
        let root_dirs = root_dirs(&index);
        let classes = self.feed.classes.borrow();
        let mut queue: Vec<PathBuf> = keep.iter().cloned().collect();
        while let Some(path) = queue.pop() {
            let Some(file) = index.files.get(&path) else {
                continue;
            };
            let class = classes.get(&path).map(String::as_str);
            for style in local_styles(&path, file, class, &root_dirs) {
                if index.files.contains_key(&style) && !keep.contains(&style) {
                    for included in index.project_files(&style) {
                        if keep.insert(included.clone()) {
                            queue.push(included);
                        }
                    }
                }
            }
        }
        drop(classes);
        let before = index.files.len();
        index.files.retain(|path, _| keep.contains(path));
        let dropped = index.files.len() < before;
        let bibs: HashSet<PathBuf> = index
            .files
            .iter()
            .flat_map(|(path, file)| bibliographies(path, file))
            .collect();
        index.bibs.retain(|path, _| bibs.contains(path));
        drop(index);
        // Where what is read from disk lives: the files kept but the open documents (and the
        // includes not read yet), and the bibliographies. In the directories where the discovery
        // jobs found them on disk; a file no job read (past the cap, or before its first pass)
        // in the one the index names.
        let disk_dirs: BTreeSet<PathBuf> = {
            let real_paths = self.feed.real_paths.borrow();
            keep.iter()
                .filter(|path| !open.contains(*path))
                .chain(&bibs)
                .filter_map(|path| real_paths.get(path).unwrap_or(path).parent())
                .map(Path::to_path_buf)
                .collect()
        };
        self.feed.disk_dirs.replace(disk_dirs);
        // The `.aux` of a root that is no longer the project's goes too.
        let root = self.project_root(state);
        let aux_kept = self
            .feed
            .aux_root
            .borrow()
            .as_ref()
            .is_none_or(|aux| keep.contains(aux) || root.as_ref() == Some(aux));
        if !aux_kept {
            state.workspace.index.borrow_mut().aux = AuxInfo::default();
            self.feed.aux_root.replace(None);
        }
        self.feed
            .disk_files
            .borrow_mut()
            .retain(|p| keep.contains(p));
        self.feed
            .classes
            .borrow_mut()
            .retain(|p, _| keep.contains(p));
        self.feed
            .merged_generation
            .borrow_mut()
            .retain(|p, _| keep.contains(p));
        // Candidates next to documents that are no longer open go; their directories may be
        // listed again for a document opened later.
        let near: Vec<PathBuf> = open
            .iter()
            .filter_map(|p| p.parent())
            .flat_map(|dir| dir.ancestors().take(NEARBY_LEVELS + 1))
            .map(Path::to_path_buf)
            .collect();
        let mut gone_dirs = Vec::new();
        self.feed.candidates.borrow_mut().retain(|path, _| {
            let kept = path
                .parent()
                .is_some_and(|dir| near.iter().any(|n| n == dir));
            if !kept && let Some(dir) = path.parent() {
                gone_dirs.push(dir.to_path_buf());
            }
            kept
        });
        let mut searched = self.feed.searched.borrow_mut();
        searched.retain(|dir| near.iter().any(|n| n == dir) && !gone_dirs.contains(dir));
        let candidates = self.feed.candidates.borrow();
        self.feed
            .indexed_with
            .borrow_mut()
            .retain(|p, _| keep.contains(p) || candidates.contains_key(p));
        self.feed
            .text_hashes
            .borrow_mut()
            .retain(|p, _| keep.contains(p) || candidates.contains_key(p));
        self.feed
            .real_paths
            .borrow_mut()
            .retain(|p, _| keep.contains(p) || bibs.contains(p) || candidates.contains_key(p));
        dropped
    }

    /// The directory the project's files are listed from: the root's, else the workspace's.
    fn root_dir(&self, state: &WindowState) -> PathBuf {
        self.project_root(state)
            .and_then(|r| r.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| state.workspace.root().path().to_path_buf())
    }

    /// After a build: read the `.aux`, the bibliographies and the file list again.
    fn after_build(self: &Rc<Self>, pdf: Option<&Path>) {
        let Some(state) = self.state() else {
            return;
        };
        {
            let index = state.workspace.index.borrow();
            let mut stale = self.feed.stale.borrow_mut();
            stale.extend(index.bibs.keys().cloned());
            if let Some(root) = self.project_root(&state) {
                stale.insert(root);
            }
        }
        // The `.aux` next to the PDF the build wrote is the one to read first.
        self.feed
            .aux_hint
            .replace(pdf.map(|pdf| pdf.with_extension("aux")));
        self.feed.aux_root.replace(None);
        self.feed.unreadable.borrow_mut().clear();
        self.walk_files();
        self.discover();
    }

    /// Another program (or Lemma, `own_write`) changed, created or deleted the file at `path`
    /// (see [`disk_changed`]); true when the index reads it again.
    fn disk_changed(self: &Rc<Self>, state: &WindowState, path: &Path, own_write: bool) -> bool {
        let real_paths = self.feed.real_paths.borrow();
        let indexed = if own_write {
            Vec::new()
        } else {
            let index = state.workspace.index.borrow();
            // Open documents are indexed from their live text; bibliographies are read from
            // disk even while they are open.
            let mut keys = keys_naming(index.files.keys(), path, &real_paths);
            keys.retain(|key| state.workspace.get(key).is_none_or(|d| !d.is_open()));
            keys.extend(keys_naming(index.bibs.keys(), path, &real_paths));
            keys
        };
        let unreadable = keys_naming(self.feed.unreadable.borrow().iter(), path, &real_paths);
        drop(real_paths);
        if indexed.is_empty() && unreadable.is_empty() {
            return false;
        }
        {
            let mut set = self.feed.unreadable.borrow_mut();
            for key in &unreadable {
                set.remove(key);
            }
        }
        self.feed.stale.borrow_mut().extend(indexed);
        self.discover();
        true
    }

    /// Files came or went (see [`files_moved`]): lists the project's files again when the list
    /// does not say so already.
    fn files_moved(self: &Rc<Self>, state: &WindowState, moved: &[(PathBuf, bool)]) {
        let Some(dir) = self.feed.walked_dir.borrow().clone() else {
            // Not listed since the root moved: list them now.
            self.walk_files();
            return;
        };
        let skip = dir.join(&state.config.build.outdir);
        let differs = {
            let completer = self.completer.borrow();
            let listed: HashSet<&str> = completer.files().iter().map(String::as_str).collect();
            moved.iter().any(|(path, exists)| {
                let Some(rel) = path
                    .strip_prefix(&dir)
                    .ok()
                    .filter(|_| !path.starts_with(&skip))
                    .and_then(Path::to_str)
                else {
                    return false;
                };
                let rel = rel.replace('\\', "/");
                // A directory that went takes its files along (moved out of the project, they
                // get no events of their own).
                listed.contains(rel.as_str()) != *exists
                    || (!exists
                        && listed.iter().any(|file| {
                            file.strip_prefix(rel.as_str())
                                .is_some_and(|rest| rest.starts_with('/'))
                        }))
            })
        };
        if differs {
            self.walk_files();
        }
    }

    /// Lists the project's files (for file arguments) on the jobs thread.
    fn walk_files(self: &Rc<Self>) {
        let Some(state) = self.state() else {
            return;
        };
        let root_dir = self.root_dir(&state);
        if root_dir.parent().is_none() {
            return;
        }
        self.feed.walked_dir.replace(Some(root_dir.clone()));
        let walk = self.feed.walks.get() + 1;
        self.feed.walks.set(walk);
        let skip = vec![root_dir.join(&state.config.build.outdir)];
        let weak = Rc::downgrade(self);
        self.feed.jobs.set(self.feed.jobs.get() + 1);
        glib::spawn_future_local(async move {
            let files = gio::spawn_blocking(move || {
                let walk =
                    crate::workspace::walk_project(&root_dir, &skip, WALK_MAX_FILES, WALK_MAX_DIRS);
                walk.files
                    .iter()
                    .filter_map(|f| f.strip_prefix(&root_dir).ok())
                    .filter_map(Path::to_str)
                    .map(|s| s.replace('\\', "/"))
                    .collect::<Vec<String>>()
            })
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.jobs.set(shared.feed.jobs.get() - 1);
            // An older listing that ends after a newer one is dropped.
            if let Ok(files) = files
                && shared.feed.walks.get() == walk
            {
                shared.completer.borrow_mut().set_files(files);
            }
        });
    }

    /// Reads the `spell.word_lists` on the jobs thread.
    fn load_dictionary(self: &Rc<Self>) {
        let lists = self.feed.dictionary_lists.borrow().clone();
        let weak = Rc::downgrade(self);
        self.feed.jobs.set(self.feed.jobs.get() + 1);
        glib::spawn_future_local(async move {
            let dictionary = gio::spawn_blocking(move || {
                let texts: Vec<String> = lists
                    .iter()
                    .filter_map(|p| read_text(p, MAX_FILE_BYTES))
                    // A LaTeX source is not a word list.
                    .filter(|t| !t.contains(['\\', '$']))
                    .collect();
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                WordList::from_text(&refs)
            })
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.jobs.set(shared.feed.jobs.get() - 1);
            if let Ok(dictionary) = dictionary {
                shared.feed.dictionary.replace(Arc::new(dictionary));
                shared.rebuild_base_words();
            }
        });
    }

    /// What [`Self::rebuild_base_words`] builds the base words from now.
    fn base_inputs(&self) -> BaseInputs {
        let hashes = self.feed.text_hashes.borrow();
        let mut files: Vec<(PathBuf, u64)> = self
            .feed
            .disk_files
            .borrow()
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "tex"))
            .map(|f| (f.clone(), hashes.get(f).copied().unwrap_or_default()))
            .collect();
        files.sort_unstable();
        (self.feed.dictionary.borrow().clone(), files)
    }

    /// Whether the base words were built (or are being built) from what they would be built
    /// from now: the same dictionary, and the same `.tex` files read from disk with the same
    /// texts.
    fn base_words_current(&self) -> bool {
        if self.feed.base_stale.get() {
            return false;
        }
        let now = self.base_inputs();
        self.feed
            .base_inputs
            .borrow()
            .as_ref()
            .is_some_and(|built| Arc::ptr_eq(&built.0, &now.0) && built.1 == now.1)
    }

    /// The file at `path` (normalized) may have changed without a discovery job reading it:
    /// when the base words read it, they are collected again on the next pass that reads from
    /// disk ([`Feed::base_stale`]).
    fn disk_text_unread(&self, path: &Path) {
        let disk_files = self.feed.disk_files.borrow();
        let tex = disk_files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "tex"));
        if !keys_naming(tex, path, &self.feed.real_paths.borrow()).is_empty() {
            self.feed.base_stale.set(true);
        }
    }

    /// The dictionary plus the words of the files read from disk (jobs thread): the files are
    /// read one at a time and their words merged into the list once.
    fn rebuild_base_words(self: &Rc<Self>) {
        let inputs = self.base_inputs();
        let dictionary = inputs.0.clone();
        let files: Vec<PathBuf> = inputs.1.iter().map(|(path, _)| path.clone()).collect();
        self.feed.base_inputs.replace(Some(inputs));
        self.feed.base_stale.set(false);
        let generation = self.feed.base_generation.get() + 1;
        self.feed.base_generation.set(generation);
        let weak = Rc::downgrade(self);
        self.feed.jobs.set(self.feed.jobs.get() + 1);
        glib::spawn_future_local(async move {
            let base = gio::spawn_blocking(move || {
                let mut words = (*dictionary).clone();
                words.add_project_texts(
                    files
                        .iter()
                        .filter_map(|file| read_text(file, MAX_FILE_BYTES)),
                );
                words
            })
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.jobs.set(shared.feed.jobs.get() - 1);
            if let Ok(base) = base
                && shared.feed.base_generation.get() == generation
            {
                shared.feed.base_words.replace(Arc::new(base));
                shared.rebuild_words();
            }
        });
    }

    /// Rebuilds the words [`WORDS_DELAY`] after the last change.
    fn schedule_words(self: &Rc<Self>) {
        if let Some(old) = self.feed.words_timer.borrow_mut().take() {
            old.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(WORDS_DELAY, move || {
            if let Some(shared) = weak.upgrade() {
                shared.feed.words_timer.borrow_mut().take();
                shared.rebuild_words();
            }
        });
        self.feed.words_timer.replace(Some(id));
    }

    /// The base words plus the open documents' words (jobs thread).
    fn rebuild_words(self: &Rc<Self>) {
        let Some(state) = self.state() else {
            return;
        };
        if let Some(old) = self.feed.words_timer.borrow_mut().take() {
            old.remove();
        }
        let base = self.feed.base_words.borrow().clone();
        let texts: Vec<String> = state
            .workspace
            .documents()
            .iter()
            .filter(|d| d.is_open() && d.path().extension().is_some_and(|e| e == "tex"))
            .map(|d| d.text())
            .collect();
        let generation = self.feed.words_generation.get() + 1;
        self.feed.words_generation.set(generation);
        let weak = Rc::downgrade(self);
        self.feed.jobs.set(self.feed.jobs.get() + 1);
        glib::spawn_future_local(async move {
            let words = gio::spawn_blocking(move || {
                let mut words = (*base).clone();
                words.add_project_texts(&texts);
                words
            })
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared.feed.jobs.set(shared.feed.jobs.get() - 1);
            if let Ok(words) = words
                && shared.feed.words_generation.get() == generation
            {
                shared.words.replace(Rc::new(words));
            }
        });
    }
}

/// `dir` joined with `rel`, `.` and `..` folded without touching the file system (as
/// `ProjectIndex::project_files` does).
fn lexical_join(dir: &Path, rel: impl AsRef<Path>) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in dir.join(rel).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The keys among `keys` that name the file at the normalized `path` (as the file monitor
/// reports it), or a file under it (a directory renamed with its files). The index keys files
/// lexically, so a key with the same file name is also normalized (a directory reached through
/// a symbolic link); only those touch the file system. So does a key that a discovery job found
/// at `path` on disk, or under it ([`Feed::real_paths`]): a file linked under another name, and
/// the files of a directory a link leads to once it is deleted (the paths through the link no
/// longer resolve).
fn keys_naming<'a>(
    keys: impl Iterator<Item = &'a PathBuf>,
    path: &Path,
    real_paths: &HashMap<PathBuf, PathBuf>,
) -> Vec<PathBuf> {
    keys.filter(|key| {
        key.starts_with(path)
            || real_paths
                .get(*key)
                .is_some_and(|real| real.starts_with(path))
            || (key.file_name() == path.file_name() && crate::workspace::normalize(key) == path)
    })
    .cloned()
    .collect()
}

/// The paths of the open documents of `state`'s window (of any kind), as the workspace keys
/// them.
fn open_documents(state: &WindowState) -> HashSet<PathBuf> {
    state
        .workspace
        .documents()
        .iter()
        .filter(|d| d.is_open())
        .map(|d| d.path().to_path_buf())
        .collect()
}

/// The open LaTeX documents of `state`'s window.
fn open_latex_documents(state: &WindowState) -> HashSet<PathBuf> {
    state
        .workspace
        .documents()
        .iter()
        .filter(|d| d.is_open() && is_latex(d.path()))
        .map(|d| d.path().to_path_buf())
        .collect()
}

/// The directories of the indexed files that have a `\documentclass` (the roots): TeX runs
/// there, so it finds a package there first.
fn root_dirs(index: &ProjectIndex) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = index
        .files
        .iter()
        .filter(|(_, file)| file.has_documentclass)
        .filter_map(|(path, _)| path.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// Where the packages and the class that `file` (at `path`) loads would be if they were local:
/// `name.sty` next to it and in each of `root_dirs` for every `\usepackage{name}` and
/// `\RequirePackage{name}`, and `class.cls` next to it for its `\documentclass{class}`. The
/// paths may repeat.
fn local_styles(
    path: &Path,
    file: &FileIndex,
    class: Option<&str>,
    root_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for package in &file.packages {
        let name = format!("{package}.sty");
        out.push(lexical_join(dir, &name));
        for root in root_dirs.iter().filter(|root| root.as_path() != dir) {
            out.push(lexical_join(root, &name));
        }
    }
    if let Some(class) = class {
        out.push(lexical_join(dir, format!("{class}.cls")));
    }
    out
}

/// The `% !TEX root` file named by `file` (at `path`), as a path.
fn magic_root(path: &Path, file: &FileIndex) -> Option<PathBuf> {
    let root = file.magic.root.as_ref()?;
    let mut root = lexical_join(path.parent()?, root);
    if root.extension().is_none() {
        root.set_extension("tex");
    }
    Some(root)
}

/// The `.bib` files `file` (at `path`) names in `\bibliography`/`\addbibresource`.
fn bibliographies(path: &Path, file: &FileIndex) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    file.includes
        .iter()
        .filter(|i| i.kind == IncludeKind::Bibliography)
        .map(|i| i.target.trim())
        .filter(|target| !target.is_empty())
        .map(|target| {
            let bib = lexical_join(dir, target);
            if bib
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("bib"))
            {
                bib
            } else {
                PathBuf::from(format!("{}.bib", bib.display()))
            }
        })
        .collect()
}

/// The text of the regular file at `path` (lossy UTF-8), when it is at most `cap` bytes.
fn read_text(path: &Path, cap: u64) -> Option<String> {
    use std::io::Read;
    // Not blocking: opening a FIFO must not wait for a writer.
    let file = crate::fsutil::open_nonblocking(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > cap {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A hash of a file's text, to tell whether a file read again changed.
fn text_hash(text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The discovery job (jobs thread): reads and indexes `wanted`.
fn read_wanted(wanted: Wanted) -> Discovered {
    let mut found = Discovered {
        theorems: wanted.theorems.clone(),
        ..Discovered::default()
    };
    let mut budget = wanted.budget;
    let mut files: Vec<(PathBuf, bool)> = wanted.files.into_iter().map(|p| (p, false)).collect();
    for dir in &wanted.dirs {
        found.searched.push(dir.clone());
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut tex: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tex"))
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect();
        tex.sort();
        tex.truncate(MAX_NEARBY_PER_DIR);
        for path in tex {
            if !wanted.known.contains(&path) && !files.iter().any(|(p, _)| *p == path) {
                files.push((path, true));
            }
        }
    }
    for (path, listed) in files {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let text = read_text(&path, MAX_FILE_BYTES);
        // A listed file that names no open document cannot include one: not indexed (it is
        // listed again only after a build or a root change).
        if listed
            && text
                .as_deref()
                .is_none_or(|t| !wanted.open_stems.iter().any(|s| t.contains(s.as_str())))
        {
            continue;
        }
        let hash = text.as_deref().map(text_hash).unwrap_or_default();
        // Where it is on disk, symbolic links resolved (a file that does not exist through its
        // directory): the file monitor watches that directory for it.
        let real = crate::workspace::normalize(&path);
        let key = match text {
            Some(_) => real.clone(),
            None => path.clone(),
        };
        let read = text.map(|text| {
            let class = document_class(&text).map(str::to_owned);
            (index_file_with_theorems(&text, &wanted.theorems), class)
        });
        found.files.push(ReadFile {
            path,
            key,
            real,
            read,
            listed,
            hash,
        });
    }
    for path in wanted.bibs {
        let entries = read_text(&path, MAX_FILE_BYTES).map(|text| scan_bib(&text));
        let real = crate::workspace::normalize(&path);
        found.bibs.push((path, real, entries));
    }
    if let Some((root, candidates)) = wanted.aux {
        let mut aux = AuxInfo::default();
        if let Some((path, text)) = candidates
            .iter()
            .find_map(|p| read_text(p, MAX_FILE_BYTES).map(|t| (p.clone(), t)))
        {
            aux = parse_aux(&text);
            let dir = path.parent().unwrap_or(Path::new("/"));
            // `\include`d files have their own `.aux`, in the same output directory.
            let mut queue: Vec<PathBuf> = aux_inputs(&text)
                .iter()
                .map(|rel| lexical_join(dir, rel))
                .collect();
            let mut seen = HashSet::new();
            while let Some(input) = queue.pop() {
                if !seen.insert(input.clone()) || seen.len() > 256 {
                    continue;
                }
                if let Some(text) = read_text(&input, MAX_FILE_BYTES) {
                    let more: BTreeMap<_, _> = parse_aux(&text).labels;
                    aux.labels.extend(more);
                    queue.extend(aux_inputs(&text).iter().map(|rel| lexical_join(dir, rel)));
                }
            }
        }
        found.aux = Some((root, aux));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`open_environment`] at the end of `text`.
    fn open_at_end(text: &str) -> Option<String> {
        open_environment(text.rsplit('\n'))
    }

    #[test]
    fn open_environments() {
        for (text, open) in [
            ("\\begin{align}\n  a &= b\n\\end{al", Some("align")),
            ("\\begin{equation}\n  a = b\n", Some("equation")),
            (
                "\\begin{itemize}\n\\item a\n\\begin{enumerate}\n\\item b\n",
                Some("enumerate"),
            ),
            // A closed inner environment in between, several lines up.
            (
                "\\begin{proof}\nWe have\n\\begin{align*}\na\n\\end{align*}\nhence\n\n",
                Some("proof"),
            ),
            ("\\begin{a}\\begin{b}x\\end{b} y", Some("a")),
            // Comments are skipped; a missing `\end` encloses the end.
            (
                "\\begin{itemize}\n\\item a % \\end{itemize}\n",
                Some("itemize"),
            ),
            ("\\begin{a}\n\\begin{b}\n\\end{c}\n", Some("b")),
            ("\\begin{a}\n\\end{a}\n", None),
            ("plain text", None),
            ("", None),
        ] {
            assert_eq!(open_at_end(text).as_deref(), open, "{text:?}");
        }
    }

    #[test]
    fn open_environments_within_the_budget() {
        let filler = "x".repeat(99) + "\n";
        let lines = OPEN_ENV_SCAN / filler.len();
        let near = format!("\\begin{{near}}\n{}", filler.repeat(lines - 1));
        assert_eq!(open_at_end(&near).as_deref(), Some("near"));
        let far = format!("\\begin{{far}}\n{}", filler.repeat(lines + 1));
        assert_eq!(open_at_end(&far), None, "more than the budget up");
        // A line longer than the budget is read from where it ends, on a character boundary.
        let long = format!("\\begin{{a}}{}\\begin{{b}}", "é".repeat(OPEN_ENV_SCAN));
        assert_eq!(open_at_end(&long).as_deref(), Some("b"));
        let long = format!("\\begin{{a}}{}", "é".repeat(OPEN_ENV_SCAN));
        assert_eq!(open_at_end(&long), None);
    }

    #[test]
    fn argument_commands() {
        assert_eq!(argument_command(r"\ref{eq:co"), Some("ref"));
        assert_eq!(argument_command(r"see \cite[p.~3]{a,b, c"), Some("cite"));
        assert_eq!(argument_command(r"\cref*{"), Some("cref*"));
        assert_eq!(argument_command(r"\begin{al"), Some("begin"));
        assert_eq!(argument_command(r"\\{x"), None);
        assert_eq!(argument_command(r"{x"), None);
        assert_eq!(argument_command(r"\frac{a"), Some("frac"));
        assert_eq!(argument_command(r"\frac{a}{b"), None);
        assert_eq!(argument_command(r"\ref{a}"), None);
        assert_eq!(argument_command(""), None);
    }

    fn math() -> Context {
        Context::Math {
            display: false,
            env: None,
        }
    }

    #[test]
    fn triggers() {
        let mut db = CompletionDb::builtin();
        db.load_packages(&["hyperref".to_owned()]);
        let t = |ctx: &Context, line: &str, c: char| {
            let req = CompletionRequest::new(ctx, line);
            trigger_for(&db, ctx, &req, c, true, 2)
        };
        let text = Context::Text;
        assert_eq!(t(&math(), r"$\", '\\'), Some(Trigger::Command));
        assert_eq!(t(&math(), r"$\fr", 'r'), Some(Trigger::Command));
        assert_eq!(t(&text, r"a\\", '\\'), None, "a line break is no command");
        assert_eq!(t(&Context::Comment, r"% \fr", 'r'), None);
        assert_eq!(t(&Context::Verbatim, r"\fr", 'r'), None);
        let reference = Context::Arg {
            cmd: "ref".into(),
            idx: 0,
            kind: ArgKind::Ref,
            start: 5,
        };
        assert_eq!(t(&reference, r"\ref{", '{'), Some(Trigger::Argument));
        let label = Context::Arg {
            cmd: "label".into(),
            idx: 0,
            kind: ArgKind::Label,
            start: 7,
        };
        assert_eq!(t(&label, r"\label{", '{'), None);
        // Commands the scanner does not classify, through the word lists' classifiers.
        assert_eq!(t(&text, r"see \autoref{", '{'), Some(Trigger::Argument));
        assert_eq!(t(&math(), r"\frac{", '{'), None);
        // Words: text only, at least `min_word` characters, letters and digits only.
        assert_eq!(t(&text, "a t", 't'), None);
        assert_eq!(t(&text, "a th", 'h'), Some(Trigger::Word));
        assert_eq!(t(&math(), "$ab", 'b'), None);
        assert_eq!(t(&text, "th.", '.'), None);
        let req = CompletionRequest::new(&text, "a th");
        assert_eq!(
            trigger_for(&db, &text, &req, 'h', false, 2),
            None,
            "auto off"
        );
        assert_eq!(
            trigger_for(&db, &text, &req, 'h', true, 3),
            None,
            "min_word 3"
        );
    }

    #[test]
    fn closed_environments() {
        assert!(environment_closed("}\n  x\n\\end{align}", "align"));
        assert!(!environment_closed("}\n  x\n", "align"));
        assert!(!environment_closed(
            "}\n\\begin{align}a\\end{align}\n",
            "align"
        ));
        assert!(environment_closed(
            "}\n\\begin{align}a\\end{align}\n\\end{align}",
            "align"
        ));
        assert!(!environment_closed("\\end{align*}", "align"));
    }

    #[test]
    fn lexical_joins() {
        assert_eq!(
            lexical_join(Path::new("/p/ch"), "../refs"),
            PathBuf::from("/p/refs")
        );
        assert_eq!(
            lexical_join(Path::new("/p"), "./a/b.tex"),
            PathBuf::from("/p/a/b.tex")
        );
    }

    /// The index keys a file changed on disk names: itself, the files under a directory, a key
    /// that reaches it through a symbolic link, also once the directory the link leads to is
    /// deleted, and a key linked to it under another name.
    #[test]
    fn keys_naming_a_changed_path() {
        let dir = std::env::temp_dir().join(format!("lemma-keys-naming-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/ch1.tex"), "").unwrap();
        let dir = crate::workspace::normalize(&dir);
        let mut keys = vec![
            dir.join("real/ch1.tex"),
            dir.join("real/ch2.tex"),
            dir.join("other/ch1.tex"),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
            keys.push(dir.join("link/ch1.tex"));
        }
        let none = HashMap::new();
        let mut named = keys_naming(keys.iter(), &dir.join("real/ch1.tex"), &none);
        let mut expected = vec![dir.join("real/ch1.tex")];
        if cfg!(unix) {
            expected.push(dir.join("link/ch1.tex"));
        }
        named.sort();
        expected.sort();
        assert_eq!(named, expected);
        assert_eq!(
            keys_naming(keys.iter(), &dir.join("real"), &none),
            [dir.join("real/ch1.tex"), dir.join("real/ch2.tex")]
        );
        assert!(keys_naming(keys.iter(), &dir.join("re"), &none).is_empty());
        // The directory the link leads to, deleted: the path through the link no longer
        // resolves, but where a discovery job found it on disk tells.
        std::fs::remove_dir_all(dir.join("real")).unwrap();
        let link = dir.join("link/ch1.tex");
        let renamed = dir.join("refs.bib");
        let keys = [link.clone(), dir.join("other/ch1.tex"), renamed.clone()];
        let real_paths = HashMap::from([
            (link.clone(), dir.join("real/ch1.tex")),
            (renamed.clone(), dir.join("central/export.bib")),
        ]);
        assert_eq!(
            keys_naming(keys.iter(), &dir.join("real"), &real_paths),
            std::slice::from_ref(&link)
        );
        assert_eq!(
            keys_naming(keys.iter(), &dir.join("real/ch1.tex"), &real_paths),
            [link]
        );
        assert!(keys_naming(keys.iter(), &dir.join("real"), &none).is_empty());
        assert!(keys_naming(keys.iter(), &dir.join("real/ch3.tex"), &real_paths).is_empty());
        // A `refs.bib` linked to a reference manager's export.
        assert_eq!(
            keys_naming(keys.iter(), &dir.join("central/export.bib"), &real_paths),
            [renamed]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn complete_proposals() {
        let item = |kind, insert| Item {
            label: String::new(),
            insert,
            kind,
            detail: None,
            glyph: None,
            score: 0,
        };
        let text = |t: &str| Insert::Text(t.to_owned());
        let snippet = |body: &str| Insert::Snippet(lemma_snippets::parse_body(body).unwrap());
        assert!(completes_nothing(
            &item(ItemKind::Command, text("\\alpha")),
            "\\alpha"
        ));
        assert!(!completes_nothing(
            &item(ItemKind::Command, text("\\alpha")),
            "\\alp"
        ));
        assert!(completes_nothing(
            &item(ItemKind::Command, snippet("\\item $0")),
            "\\item"
        ));
        assert!(!completes_nothing(
            &item(ItemKind::Command, snippet("\\frac{$1}{$2}")),
            "\\frac"
        ));
        assert!(!completes_nothing(
            &item(ItemKind::Environment, text("align")),
            "align"
        ));
        assert!(completes_nothing(
            &item(ItemKind::Label, text("eq:a")),
            "eq:a"
        ));
    }

    #[test]
    fn shortened_details() {
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "ab…");
    }
}
