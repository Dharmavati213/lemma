//! Syntax highlighting with Lemma's own scanner (WP2.7; ARCHITECTURE §1, §4.3, §5, §7).
//!
//! # What it does
//!
//! [`attach`] runs right after an editor is created, before Vim, the review renderer and the
//! diagnostics attach, so the tags those create later take precedence over ours. For a LaTeX
//! document (one whose file name gets GtkSourceView's `latex` language: `.tex`, `.sty`, `.cls`,
//! …; `completion::is_latex`) it colours the text from `lemma_syntax` tokens (GtkSourceView's
//! bracket matching is replaced by `editor::pairs`, which switches it off while attached). Such
//! buffers never get a GtkSourceView language (`document::new_buffer`). Other buffers (`.bib`,
//! plain text) keep their GtkSourceView language.
//!
//! **Tags.** The token kinds have one `TextTag` each in the buffer's tag table, named by
//! [`tag_name`], except [`TokenKind::Text`], [`TokenKind::Brace`] and [`TokenKind::Bracket`]
//! (which look like plain text) and math content and delimiters (below). A tag's look comes from
//! the buffer's style scheme ([`style_ids`]: the first of a few `def:*` styles the scheme
//! defines, mirroring GtkSourceView's `def.lang` fallbacks), so light, dark and user schemes all
//! work, and it is restyled whenever the scheme changes. **Math** has its own style: one tag,
//! [`MATH_TAG`], covers each run of math (delimiters, content, math commands and the `&` between
//! them) with the colour of `def:special-constant` (`def:constant` in schemes without it) and a
//! background tint of that colour at `editor.math_tint` percent (configurable live, 0 = none);
//! math commands and `&` carry their own tags on top. One run instead of a tag per token halves
//! the tag toggles of math-heavy lines, which are most of a tagged line's memory. The tag
//! [`NO_SPELL_TAG`] covers every token with `TokenKind::no_spell()`: commands, math, comments,
//! verbatim and the arguments of labels, references, citations, `\begin`/`\end`, packages, files
//! and options. libspelling (WP2.9b) skips that text and rechecks it when the tag changes.
//!
//! **Lazy tagging** (§7: a 60k-line file must not carry tags it never shows). Each `\n` line has
//! a mark: untagged (none of our tags), tagged (exactly its current tokens) or stale (it may
//! carry outdated tags). Only the *window* — the visible lines ± one screen — is ever tagged:
//! - on attach and whenever the view scrolls or resizes, a high-priority idle (before the next
//!   frame is drawn) tags the visible lines that are not tagged, and a background idle then tags
//!   the rest of the window within a time budget. Lines GTK has not measured yet (a large
//!   paste) all count as visible, so the visible lines are at most 1.5 screens around the
//!   cursor;
//! - every change reports its lines through `Document::connect_edit` (marks follow inserted and
//!   removed lines; the changed lines become stale, as inserted text can carry tags) and
//!   `Document::connect_retag` (the lines `LineStates::apply_edit` re-scanned until the state
//!   converged): those that carried tags become stale, and the same idles retag the stale lines
//!   in the window;
//! - retagging a stale line reads the tags it carries and rewrites only those whose ranges
//!   differ, so a keystroke that does not change a line's colours costs no tag change (and no
//!   spell recheck);
//! - lines never in a window stay untagged, and stale lines outside it are retagged when they
//!   come into view.
//!
//! While a large document's scanner states are still being built (on the jobs thread), lines
//! near the top are scanned on their own, so the first screen is coloured at once; the
//! document's rebuild notice then retags the window.
//!
//! **Phantom text.** [`phantom_context`], [`phantom_syntax`] and [`TextSyntax`] give the review
//! overlay (`review_overlay`) the same colours for deleted text, as Pango attributes
//! ([`attrs_for`] does it for a whole text). Without a background: the deleted text keeps its
//! red tint.
//!
//! [`detach`] runs when the tab closes and disconnects everything (the buffer may live on as a
//! closed document's text). Tests use [`highlighter`] and the [`Highlighter`] inspection methods.

use super::{Editor, TexView};
use crate::document::{Document, ObserverId};
use crate::hooks::HookId;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::gdk;
use gtk::prelude::*;
use lemma_core::LineIndex;
use lemma_syntax::{ArgKind, Context, LineStates, Lines, ScanConfig, StrLines, Token, TokenKind};
use sourceview5::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The tag libspelling skips (WP2.9b); the name GtkSourceView's own engine uses.
pub const NO_SPELL_TAG: &str = "gtksourceview:context-classes:no-spell-check";
/// The tag over runs of math: the math colour and its background tint (`editor.math_tint`).
pub const MATH_TAG: &str = "lemma-syntax-math";

/// Window height assumed before the view and its window have a size.
const UNSIZED_HEIGHT: i32 = 1200;
/// Smallest "screen" (in lines) for the margin of the window.
const MIN_SCREEN: u32 = 10;
/// No row of text is less than this many pixels high: more visible lines than fit the view's
/// height at this include lines GTK has not measured yet (see `unmeasured_rows`).
const MIN_ROW_HEIGHT: i32 = 8;
/// Time the background idle spends per run on the lines around the visible ones.
const MARGIN_BUDGET: Duration = Duration::from_millis(4);
/// While a document's scanner states are being built, lines below this one are not tagged (the
/// lines above it can be scanned on their own: a line's state depends only on the lines before).
const PREFIX_SCAN_LINES: u32 = 400;

/// Line marks (see the module docs).
const UNTAGGED: u8 = 0;
const TAGGED: u8 = 1;
const STALE: u8 = 2;

/// How one token kind is shown.
struct KindSpec {
    kind: TokenKind,
    /// The tag's name; `None` for kinds shown as plain text (no tag).
    tag: Option<&'static str>,
    /// Style-scheme styles in order of preference.
    styles: &'static [&'static str],
}

const KEYWORD: &[&str] = &["def:keyword", "def:statement"];
const MATH: &[&str] = &["def:special-constant", "def:constant"];
const REFERENCE: &[&str] = &["def:identifier"];
const PREAMBLE: &[&str] = &["def:preprocessor"];

/// Indexed by `TokenKind as usize` (checked by a unit test).
const SPECS: [KindSpec; 20] = [
    KindSpec {
        kind: TokenKind::Text,
        tag: None,
        styles: &[],
    },
    KindSpec {
        kind: TokenKind::Command,
        tag: Some("lemma-syntax-command"),
        styles: KEYWORD,
    },
    KindSpec {
        kind: TokenKind::Sectioning,
        tag: Some("lemma-syntax-sectioning"),
        styles: &["def:heading", "def:keyword", "def:statement"],
    },
    KindSpec {
        kind: TokenKind::Begin,
        tag: Some("lemma-syntax-begin"),
        styles: KEYWORD,
    },
    KindSpec {
        kind: TokenKind::End,
        tag: Some("lemma-syntax-end"),
        styles: KEYWORD,
    },
    KindSpec {
        kind: TokenKind::EnvName,
        tag: Some("lemma-syntax-env-name"),
        styles: &["def:type"],
    },
    KindSpec {
        kind: TokenKind::Brace,
        tag: None,
        styles: &[],
    },
    KindSpec {
        kind: TokenKind::Bracket,
        tag: None,
        styles: &[],
    },
    KindSpec {
        kind: TokenKind::Special,
        tag: Some("lemma-syntax-special"),
        styles: &["def:special-char", "def:character", "def:constant"],
    },
    // Math content and delimiters are coloured by the math run tag (`MATH_TAG`).
    KindSpec {
        kind: TokenKind::MathDelim,
        tag: None,
        styles: MATH,
    },
    KindSpec {
        kind: TokenKind::Math,
        tag: None,
        styles: MATH,
    },
    KindSpec {
        kind: TokenKind::MathCommand,
        tag: Some("lemma-syntax-math-command"),
        styles: &["def:function", "def:identifier"],
    },
    KindSpec {
        kind: TokenKind::Comment,
        tag: Some("lemma-syntax-comment"),
        styles: &["def:comment"],
    },
    KindSpec {
        kind: TokenKind::Verbatim,
        tag: Some("lemma-syntax-verbatim"),
        // Like VimTeX (String); `def:preformatted-section` is math's colour in many schemes.
        styles: &["def:string", "def:constant"],
    },
    KindSpec {
        kind: TokenKind::LabelArg,
        tag: Some("lemma-syntax-label"),
        styles: REFERENCE,
    },
    KindSpec {
        kind: TokenKind::RefArg,
        tag: Some("lemma-syntax-ref"),
        styles: REFERENCE,
    },
    KindSpec {
        kind: TokenKind::CiteArg,
        tag: Some("lemma-syntax-cite"),
        styles: REFERENCE,
    },
    KindSpec {
        kind: TokenKind::FileArg,
        tag: Some("lemma-syntax-file"),
        styles: &["def:string", "def:constant"],
    },
    KindSpec {
        kind: TokenKind::PackageArg,
        tag: Some("lemma-syntax-package"),
        styles: PREAMBLE,
    },
    KindSpec {
        kind: TokenKind::KeyArg,
        tag: Some("lemma-syntax-key"),
        styles: PREAMBLE,
    },
];

fn spec(kind: TokenKind) -> &'static KindSpec {
    let spec = &SPECS[kind as usize];
    debug_assert_eq!(spec.kind, kind);
    spec
}

/// The name of `kind`'s tag; `None` for the kinds shown as plain text (`Text`, `Brace`,
/// `Bracket`) and for math content and delimiters (coloured by [`MATH_TAG`]).
pub fn tag_name(kind: TokenKind) -> Option<&'static str> {
    spec(kind).tag
}

/// The style-scheme styles `kind` is shown with, in order of preference (the first one the
/// scheme defines is used).
pub fn style_ids(kind: TokenKind) -> &'static [&'static str] {
    spec(kind).styles
}

/// The style `kind` gets from `scheme` (see [`style_ids`]).
pub fn scheme_style(
    scheme: Option<&sourceview5::StyleScheme>,
    kind: TokenKind,
) -> Option<sourceview5::Style> {
    first_style(scheme, spec(kind).styles)
}

fn first_style(
    scheme: Option<&sourceview5::StyleScheme>,
    ids: &[&str],
) -> Option<sourceview5::Style> {
    let scheme = scheme?;
    ids.iter().find_map(|id| scheme.style(id))
}

/// The colour of math in `scheme` (the foreground of [`TokenKind::Math`]'s style).
pub fn math_color(scheme: Option<&sourceview5::StyleScheme>) -> Option<gdk::RGBA> {
    scheme_style(scheme, TokenKind::Math)
        .filter(|s| s.is_foreground_set())
        .and_then(|s| s.foreground())
        .and_then(|c| gdk::RGBA::parse(c.as_str()).ok())
}

fn is_math(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Math | TokenKind::MathCommand | TokenKind::MathDelim
    )
}

/// What one line's tokens become: styled spans, the no-spell runs and the math runs (byte
/// ranges within the line).
#[derive(Debug, Default, PartialEq, Eq)]
struct Spans {
    styled: Vec<(TokenKind, Range<u32>)>,
    no_spell: Vec<Range<u32>>,
    math: Vec<Range<u32>>,
}

impl Spans {
    fn clear(&mut self) {
        self.styled.clear();
        self.no_spell.clear();
        self.math.clear();
    }

    /// Fills `self` from `tokens` (the tiling of one line). Neighbouring tokens that are both
    /// excluded from spell checking form one run. A math run continues across a `&` (the only
    /// special character math has) between math tokens, or at the start of a line followed by
    /// math (an `align` row starting with `&`).
    fn fill(&mut self, tokens: &[Token]) {
        self.clear();
        let extend = |runs: &mut Vec<Range<u32>>, t: &Token| match runs.last_mut() {
            Some(last) if last.end == t.start => last.end = t.end,
            _ => runs.push(t.start..t.end),
        };
        let mut in_math = false;
        for (i, t) in tokens.iter().enumerate() {
            if spec(t.kind).tag.is_some() {
                self.styled.push((t.kind, t.start..t.end));
            }
            if t.kind.no_spell() {
                extend(&mut self.no_spell, t);
            }
            let next_math = tokens.get(i + 1).is_some_and(|n| is_math(n.kind));
            in_math = is_math(t.kind)
                || (t.kind == TokenKind::Special && next_math && (i == 0 || in_math));
            if in_math {
                extend(&mut self.math, t);
            }
        }
    }
}

/// Copies the look of `style` onto `tag` like `gtk_source_style_apply`, resetting what the style
/// does not set (so a scheme change leaves nothing of the old scheme behind).
fn apply_style(tag: &gtk::TextTag, style: Option<&sourceview5::Style>) {
    for property in [
        "background-set",
        "foreground-set",
        "paragraph-background-set",
        "style-set",
        "weight-set",
        "underline-set",
        "underline-rgba-set",
        "strikethrough-set",
        "scale-set",
    ] {
        tag.set_property(property, false);
    }
    if let Some(style) = style {
        style.apply(tag);
    }
}

// ----- per-editor highlighting -----

/// The lines the highlighter keeps tagged, as last computed: the visible lines, and those plus
/// one screen above and below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagWindow {
    pub visible: Range<u32>,
    pub tagged: Range<u32>,
}

/// Our tags in one buffer.
struct Tags {
    /// Indexed by `TokenKind as usize`; `None` for kinds without a tag.
    kinds: Vec<Option<gtk::TextTag>>,
    /// [`MATH_TAG`].
    math: gtk::TextTag,
    no_spell: gtk::TextTag,
    /// Every tag above: the kinds' in kind order, then math, then no-spell.
    all: Vec<gtk::TextTag>,
    /// The index in `all` of each kind's tag (indexed by `TokenKind as usize`).
    kind_index: Vec<Option<usize>>,
}

impl Tags {
    /// Our tags in `buffer`'s table, created now (the math tag first, so it has the lowest
    /// priority and math commands show over it) or found there by name. Returns whether any
    /// existed already.
    fn new(buffer: &sourceview5::Buffer) -> (Self, bool) {
        let table = buffer.tag_table();
        let mut existed = false;
        let mut tag = |name: &str| {
            if let Some(tag) = table.lookup(name) {
                existed = true;
                return tag;
            }
            let tag = gtk::TextTag::new(Some(name));
            table.add(&tag);
            tag
        };
        let math = tag(MATH_TAG);
        let kinds: Vec<Option<gtk::TextTag>> = SPECS.iter().map(|s| s.tag.map(&mut tag)).collect();
        let no_spell = tag(NO_SPELL_TAG);
        let mut all: Vec<gtk::TextTag> = Vec::new();
        let kind_index = kinds
            .iter()
            .map(|tag| {
                tag.as_ref().map(|tag| {
                    all.push(tag.clone());
                    all.len() - 1
                })
            })
            .collect();
        all.push(math.clone());
        all.push(no_spell.clone());
        (
            Self {
                kinds,
                math,
                no_spell,
                all,
                kind_index,
            },
            existed,
        )
    }

    fn math_index(&self) -> usize {
        self.all.len() - 2
    }

    fn no_spell_index(&self) -> usize {
        self.all.len() - 1
    }

    fn index(&self, tag: &gtk::TextTag) -> Option<usize> {
        self.all.iter().position(|t| t == tag)
    }

    fn kind(&self, kind: TokenKind) -> Option<&gtk::TextTag> {
        self.kinds[kind as usize].as_ref()
    }

    fn bit(&self, tag: &gtk::TextTag) -> u32 {
        self.index(tag).map_or(0, |i| 1 << i)
    }
}

thread_local! {
    /// Highlighters of the editors that have one.
    static ACTIVE: RefCell<Vec<Rc<Highlighter>>> = const { RefCell::new(Vec::new()) };
}

/// The highlighter of `editor`, while it has one (LaTeX buffers, between [`attach`] and
/// [`detach`]).
pub fn highlighter(editor: &Editor) -> Option<Rc<Highlighter>> {
    highlighter_for(editor.view())
}

/// The highlighter of `view`, if any.
pub fn highlighter_for(view: &TexView) -> Option<Rc<Highlighter>> {
    ACTIVE.with_borrow(|active| {
        active
            .iter()
            .find(|h| h.view.upgrade().as_ref() == Some(view))
            .cloned()
    })
}

/// Starts scanner highlighting in `editor` when its document is LaTeX (see the module docs).
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if !super::completion::is_latex(editor.document().path()) || highlighter(editor).is_some() {
        return;
    }
    // Highlighters whose view is gone without a detach (cannot happen in the app) go first.
    let dead: Vec<Rc<Highlighter>> = ACTIVE.with_borrow_mut(|active| {
        let (dead, alive) = active.drain(..).partition(|h| h.view.upgrade().is_none());
        *active = alive;
        dead
    });
    for highlighter in dead {
        highlighter.shutdown();
    }
    let highlighter = Highlighter::new(state, editor);
    ACTIVE.with_borrow_mut(|active| active.push(highlighter.clone()));
    highlighter.queue_visible();
}

/// Stops it: disconnects everything and drops pending work. The tags stay on the text (the
/// buffer is on its way out with its tab). Safe when [`attach`] did nothing.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let found = ACTIVE.with_borrow_mut(|active| {
        let i = active
            .iter()
            .position(|h| h.view.upgrade().as_ref() == Some(editor.view()))?;
        Some(active.remove(i))
    });
    if let Some(highlighter) = found {
        highlighter.shutdown();
    }
}

/// Scanner highlighting of one editor (see the module docs).
pub struct Highlighter {
    view: glib::WeakRef<TexView>,
    doc: Weak<Document>,
    buffer: sourceview5::Buffer,
    tags: Tags,
    /// One mark per `\n` line (see the module docs).
    marks: RefCell<Vec<u8>>,
    /// `editor.math_tint`.
    tint: Cell<u8>,
    window: RefCell<Option<TagWindow>>,
    /// The vertical and horizontal adjustments' page sizes, as last seen.
    page_sizes: Cell<[f64; 2]>,
    /// [`estimated_rows`], computed once.
    rows: Cell<Option<u32>>,
    /// How GTK's lines map to the text's while they differ (see [`Locator`]), until the next
    /// change.
    mapping: RefCell<Option<Locator>>,
    visible_idle: RefCell<Option<glib::SourceId>>,
    margin_idle: RefCell<Option<glib::SourceId>>,
    recheck_idle: RefCell<Option<glib::SourceId>>,
    /// Time spent in this highlighter's callbacks and idles.
    work: Cell<Duration>,
    /// Passes over the visible lines so far.
    passes: Cell<u64>,
    scratch: RefCell<Scratch>,
    observers: RefCell<Vec<ObserverId>>,
    handlers: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    /// The window's config hook (`editor.math_tint`).
    hook: Cell<Option<HookId>>,
    state: Weak<WindowState>,
}

impl Highlighter {
    fn new(state: &Rc<WindowState>, editor: &Rc<Editor>) -> Rc<Self> {
        let buffer = editor.buffer().clone();
        let doc = editor.document();
        let (tags, existed) = Tags::new(&buffer);
        if existed {
            // Tags from an earlier highlighter (a closed document's buffer shown again): we do
            // not know where they are, so start from a clean text.
            let (start, end) = buffer.bounds();
            for tag in &tags.all {
                buffer.remove_tag(tag, &start, &end);
            }
        }
        let lines = doc.line_index().line_count() as usize;
        let highlighter = Rc::new(Self {
            view: editor.view().downgrade(),
            doc: Rc::downgrade(doc),
            buffer,
            tags,
            marks: RefCell::new(vec![UNTAGGED; lines]),
            tint: Cell::new(state.config.editor.math_tint),
            window: RefCell::new(None),
            page_sizes: Cell::new([0.0; 2]),
            rows: Cell::new(None),
            mapping: RefCell::new(None),
            visible_idle: RefCell::new(None),
            margin_idle: RefCell::new(None),
            recheck_idle: RefCell::new(None),
            work: Cell::new(Duration::ZERO),
            passes: Cell::new(0),
            scratch: RefCell::new(Scratch::default()),
            observers: RefCell::new(Vec::new()),
            handlers: RefCell::new(Vec::new()),
            hook: Cell::new(None),
            state: Rc::downgrade(state),
        });
        highlighter.restyle();
        highlighter.connect(state, editor.view(), doc);
        highlighter
    }

    fn connect(self: &Rc<Self>, state: &Rc<WindowState>, view: &TexView, doc: &Rc<Document>) {
        let mut observers = self.observers.borrow_mut();
        let weak = Rc::downgrade(self);
        observers.push(doc.connect_edit(move |doc, edit, _| {
            if let Some(h) = weak.upgrade() {
                // Before the change: the line index still describes the old text.
                let delta = doc.line_index().line_delta(edit);
                let rebuilding = !doc.has_line_states();
                h.timed(|| h.on_edit(delta, rebuilding));
            }
        }));
        let weak = Rc::downgrade(self);
        observers.push(doc.connect_retag(move |lines| {
            if let Some(h) = weak.upgrade() {
                h.timed(|| h.on_retag(lines));
            }
        }));
        drop(observers);

        let mut handlers = self.handlers.borrow_mut();
        let weak = Rc::downgrade(self);
        let id = self.buffer.connect_style_scheme_notify(move |_| {
            if let Some(h) = weak.upgrade() {
                h.restyle();
            }
        });
        handlers.push((self.buffer.clone().upcast(), id));
        if let Some(adjustment) = view.vadjustment() {
            let weak = Rc::downgrade(self);
            let id = adjustment.connect_value_changed(move |_| {
                if let Some(h) = weak.upgrade() {
                    h.queue_visible();
                }
            });
            handlers.push((adjustment.upcast(), id));
        }
        // `changed` also comes with every step of GTK's background validation of a long text
        // (the total height grows): a new page size (a resize, which also rewraps) is handled
        // before the next frame; other changes (lines moving into view as review bands or
        // reading mode change their height) once the main loop is otherwise idle.
        for (i, adjustment) in [view.vadjustment(), view.hadjustment()]
            .into_iter()
            .enumerate()
        {
            let Some(adjustment) = adjustment else {
                continue;
            };
            let weak = Rc::downgrade(self);
            let id = adjustment.connect_changed(move |adjustment| {
                if let Some(h) = weak.upgrade() {
                    let mut sizes = h.page_sizes.get();
                    if sizes[i] != adjustment.page_size() {
                        sizes[i] = adjustment.page_size();
                        h.page_sizes.set(sizes);
                        h.queue_visible();
                    } else {
                        h.queue_recheck();
                    }
                }
            });
            handlers.push((adjustment.upcast(), id));
        }
        // A tab shown again may have scrolled while hidden (e.g. "go to line").
        let weak = Rc::downgrade(self);
        let id = view.connect_map(move |_| {
            if let Some(h) = weak.upgrade() {
                h.queue_visible();
            }
        });
        handlers.push((view.clone().upcast(), id));
        drop(handlers);

        let weak = Rc::downgrade(self);
        self.hook
            .set(Some(state.connect_config_changed(move |state, file| {
                if file == ConfigFile::Config
                    && let Some(h) = weak.upgrade()
                {
                    h.set_math_tint(state.config.editor.math_tint);
                    // The font or the wrapping may have changed which lines are on screen.
                    h.queue_recheck();
                }
            })));
    }

    fn shutdown(&self) {
        for idle in [&self.visible_idle, &self.margin_idle, &self.recheck_idle] {
            if let Some(id) = idle.take() {
                id.remove();
            }
        }
        if let Some(doc) = self.doc.upgrade() {
            for id in self.observers.borrow_mut().drain(..) {
                doc.disconnect(id);
            }
        }
        for (object, id) in self.handlers.borrow_mut().drain(..) {
            object.disconnect(id);
        }
        if let (Some(hook), Some(state)) = (self.hook.take(), self.state.upgrade()) {
            state.disconnect(hook);
        }
    }

    // ----- inspection (tests, perf) -----

    /// The window as last computed (`None` before the first tagging pass).
    pub fn window(&self) -> Option<TagWindow> {
        self.window.borrow().clone()
    }

    /// True when line `line` may carry our tags (tagged or stale).
    pub fn is_tagged(&self, line: u32) -> bool {
        self.marks
            .borrow()
            .get(line as usize)
            .is_some_and(|&m| m != UNTAGGED)
    }

    /// True when line `line` carries exactly the tags of its current tokens.
    pub fn is_current(&self, line: u32) -> bool {
        self.marks.borrow().get(line as usize) == Some(&TAGGED)
    }

    /// The runs of lines that may carry our tags (tagged or stale), ascending.
    pub fn tagged_lines(&self) -> Vec<Range<u32>> {
        let marks = self.marks.borrow();
        let mut runs: Vec<Range<u32>> = Vec::new();
        for (i, &m) in marks.iter().enumerate() {
            if m == UNTAGGED {
                continue;
            }
            let i = i as u32;
            match runs.last_mut() {
                Some(r) if r.end == i => r.end = i + 1,
                _ => runs.push(i..i + 1),
            }
        }
        runs
    }

    /// Total time spent highlighting so far: change observers, scroll handling and idles.
    pub fn work_time(&self) -> Duration {
        self.work.get()
    }

    /// How many times the visible lines were (re)considered: once per scroll, resize or change
    /// (coalesced per main-loop iteration).
    pub fn passes(&self) -> u64 {
        self.passes.get()
    }

    /// The tag of `kind` (`None` for kinds shown as plain text).
    pub fn tag(&self, kind: TokenKind) -> Option<gtk::TextTag> {
        self.tags.kind(kind).cloned()
    }

    /// The tag over runs of math ([`MATH_TAG`]).
    pub fn math_tag(&self) -> gtk::TextTag {
        self.tags.math.clone()
    }

    /// The [`NO_SPELL_TAG`] tag.
    pub fn no_spell_tag(&self) -> gtk::TextTag {
        self.tags.no_spell.clone()
    }

    /// Runs the pending tagging of the visible lines now (what the high-priority idle would do
    /// before the next frame), leaving the rest of the window to the background idle.
    pub fn flush_visible(self: &Rc<Self>) {
        if let Some(id) = self.visible_idle.take() {
            id.remove();
            self.run_visible();
        }
    }

    /// Runs the pending tagging now (what the idles would do), until the window is tagged.
    pub fn flush(self: &Rc<Self>) {
        let recheck = self.recheck_idle.take();
        let visible = self.visible_idle.take();
        if recheck.is_some() || visible.is_some() {
            recheck
                .into_iter()
                .chain(visible)
                .for_each(glib::SourceId::remove);
            self.run_visible();
        }
        while self.margin_idle.borrow().is_some() {
            if self.run_margin() == glib::ControlFlow::Break
                && let Some(id) = self.margin_idle.take()
            {
                id.remove();
            }
        }
    }

    /// Tags every line now, whether or not it is in the window (golden tests, printing).
    pub fn tag_all(&self) {
        let count = self
            .doc
            .upgrade()
            .map_or(0, |d| d.line_index().line_count());
        self.timed(|| self.tag_lines(0..count, None));
    }

    // ----- styles -----

    /// Styles every tag from the buffer's current style scheme.
    fn restyle(&self) {
        let scheme = self.buffer.style_scheme();
        let math = first_style(scheme.as_ref(), MATH);
        for (spec, tag) in SPECS.iter().zip(&self.tags.kinds) {
            let Some(tag) = tag else {
                continue;
            };
            apply_style(tag, first_style(scheme.as_ref(), spec.styles).as_ref());
            // Tags shown over math must not take a weight or slant from the math style.
            if matches!(spec.kind, TokenKind::MathCommand | TokenKind::Special)
                && let Some(math) = &math
            {
                if (math.is_bold_set() || math.is_weight_set()) && !tag.is_weight_set() {
                    tag.set_weight(400);
                }
                if math.is_italic_set() && !tag.is_style_set() {
                    tag.set_style(pango::Style::Normal);
                }
            }
        }
        self.style_math(scheme.as_ref());
    }

    fn set_math_tint(&self, percent: u8) {
        if self.tint.replace(percent) != percent {
            self.style_math(self.buffer.style_scheme().as_ref());
        }
    }

    /// The math tag: the math style, and a background of the math colour at
    /// `editor.math_tint` percent (or the text colour when the scheme has no math colour).
    fn style_math(&self, scheme: Option<&sourceview5::StyleScheme>) {
        let tag = &self.tags.math;
        apply_style(tag, first_style(scheme, MATH).as_ref());
        let percent = self.tint.get().min(100);
        let color = math_color(scheme).or_else(|| self.view.upgrade().map(|v| v.color()));
        if let Some(c) = color
            && percent > 0
        {
            let tint = gdk::RGBA::new(
                c.red(),
                c.green(),
                c.blue(),
                c.alpha() * f32::from(percent) / 100.0,
            );
            tag.set_background_rgba(Some(&tint));
        }
    }

    // ----- changes -----

    /// Before a change that replaces lines `[first, first + removed)` by `added` lines: the marks
    /// follow the text, and the new lines are stale: text inserted inside a tagged range takes
    /// its tags, and text pasted from the same buffer brings its own. (The retag notice that
    /// follows queues the tagging; while a rebuild of the states runs there is none, so the
    /// tagging is queued here too, and every tagged line after the change becomes stale: no one
    /// says how far its effect reaches.)
    fn on_edit(self: &Rc<Self>, (first, removed, added): (u32, u32, u32), rebuilding: bool) {
        self.mapping.replace(None);
        self.queue_visible();
        let mut marks = self.marks.borrow_mut();
        let (first, end) = (first as usize, (first + removed) as usize);
        if end > marks.len() {
            // Out of step with the text (cannot happen): assume tags everywhere.
            marks.resize(end, STALE);
        }
        marks.splice(first..end, std::iter::repeat_n(STALE, added as usize));
        if rebuilding {
            for mark in &mut marks[first..] {
                if *mark == TAGGED {
                    *mark = STALE;
                }
            }
        }
    }

    /// After a change (or a rebuild of the scanner states): the tokens of `lines` may differ.
    /// Tagged lines among them become stale, and the window is brought up to date.
    fn on_retag(self: &Rc<Self>, lines: Range<u32>) {
        {
            let mut marks = self.marks.borrow_mut();
            let end = (lines.end as usize).min(marks.len());
            let start = (lines.start as usize).min(end);
            for mark in &mut marks[start..end] {
                if *mark == TAGGED {
                    *mark = STALE;
                }
            }
        }
        self.queue_visible();
    }

    // ----- scheduling -----

    /// Tags the visible lines before the next frame (a high-priority idle), then the rest of
    /// the window in the background.
    fn queue_visible(self: &Rc<Self>) {
        if self.visible_idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if let Some(h) = weak.upgrade() {
                h.visible_idle.take();
                h.run_visible();
            }
            glib::ControlFlow::Break
        });
        self.visible_idle.replace(Some(id));
    }

    /// Looks at the visible lines again once the main loop has nothing else to do (one pending
    /// check, however many layout changes ask for it).
    fn queue_recheck(self: &Rc<Self>) {
        if self.recheck_idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::idle_add_local_full(glib::Priority::LOW, move || {
            if let Some(h) = weak.upgrade() {
                h.recheck_idle.take();
                h.run_visible();
            }
            glib::ControlFlow::Break
        });
        self.recheck_idle.replace(Some(id));
    }

    fn queue_margin(self: &Rc<Self>) {
        if self.margin_idle.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        // Below every other idle (GTK's layout validation, the review bars' placement, …).
        let id = glib::idle_add_local_full(glib::Priority::LOW, move || {
            let Some(h) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let flow = h.run_margin();
            if flow == glib::ControlFlow::Break {
                h.margin_idle.take();
            }
            flow
        });
        self.margin_idle.replace(Some(id));
    }

    fn run_visible(self: &Rc<Self>) {
        self.passes.set(self.passes.get() + 1);
        let pending = self.timed(|| {
            let window = self.compute_window()?;
            self.tag_lines(window.visible.clone(), None);
            let pending = self.has_untagged(&window.tagged);
            self.window.replace(Some(window));
            Some(pending)
        });
        if pending == Some(true) {
            self.queue_margin();
        }
    }

    fn run_margin(&self) -> glib::ControlFlow {
        self.timed(|| {
            let Some(window) = self.compute_window() else {
                return glib::ControlFlow::Break;
            };
            let done = self.tag_lines(window.visible.clone(), None)
                && self.tag_lines(window.tagged.clone(), Some(MARGIN_BUDGET));
            self.window.replace(Some(window));
            if done {
                // A copy of the text kept for counting characters is not needed any more.
                if matches!(*self.mapping.borrow(), Some(Locator::Chars(_))) {
                    self.mapping.replace(None);
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        })
    }

    fn timed<R>(&self, f: impl FnOnce() -> R) -> R {
        let start = Instant::now();
        let result = f();
        self.work.set(self.work.get() + start.elapsed());
        result
    }

    /// How GTK's lines map to `index`'s now (see [`Locator`]): built once after each change.
    fn locator(&self, index: &LineIndex) -> Locator {
        let gtk_lines = u32::try_from(self.buffer.line_count()).unwrap_or(u32::MAX);
        if gtk_lines == index.line_count() {
            self.mapping.replace(None);
            return Locator::Gtk;
        }
        if let Some(locator) = self.mapping.borrow().as_ref() {
            return locator.clone();
        }
        let text = crate::document::buffer_text(&self.buffer);
        let locator = match Breaks::new(&text, index, gtk_lines) {
            Some(breaks) => Locator::Breaks(Rc::new(breaks)),
            None => Locator::Chars(Rc::new(CharsText {
                text,
                at: Cell::new((0, 0)),
            })),
        };
        self.mapping.replace(Some(locator.clone()));
        locator
    }

    fn has_untagged(&self, lines: &Range<u32>) -> bool {
        let marks = self.marks.borrow();
        let end = (lines.end as usize).min(marks.len());
        let start = (lines.start as usize).min(end);
        marks[start..end].iter().any(|&m| m != TAGGED)
    }

    /// The visible lines (from the view's visible rectangle; around the cursor before the view
    /// has a size) and the lines one screen above and below them.
    fn compute_window(&self) -> Option<TagWindow> {
        let view = self.view.upgrade()?;
        let doc = self.doc.upgrade()?;
        let index = doc.line_index();
        let count = index.line_count();
        let locator = self.locator(&index);
        let line_of = |iter: &gtk::TextIter| locator.line_of(&index, iter);
        let rect = view.visible_rect();
        if rect.height() <= 0 || rect.width() <= 0 {
            // Not laid out yet (the first frame of a new tab): guess the screen around the
            // cursor, so the first frame is coloured; the allocation brings the real window.
            let rows = self.rows.get().unwrap_or_else(|| {
                let rows = estimated_rows(&view);
                self.rows.set(Some(rows));
                rows
            });
            let cursor = line_of(&self.buffer.iter_at_mark(&self.buffer.get_insert()));
            let first = cursor.saturating_sub(rows / 2).min(count.saturating_sub(1));
            let lines = first..cursor.saturating_add(rows).min(count).max(first + 1);
            return Some(TagWindow {
                visible: lines.clone(),
                tagged: lines,
            });
        }
        let (top, _) = view.line_at_y(rect.y());
        let (bottom, _) = view.line_at_y(rect.y() + rect.height() - 1);
        let (first, last) = (line_of(&top), line_of(&bottom));
        let last = last.min(count.saturating_sub(1)).max(first.min(count));
        let first = first.min(last);
        // Right after a large insertion GTK has not measured the new lines and counts them as
        // empty, so all of them fall in the visible rectangle (a 1000-line paste: 1000 lines
        // tagged before the frame, 25 ms). No more than a screen of rows fits there: the
        // visible lines are then 1.5 screens around the cursor, where the view scrolls after a
        // paste (from the top line when the cursor is elsewhere). The background pass tags the
        // rest of the window, and GTK's measuring brings a recheck (`queue_recheck`).
        let (first, last) = match unmeasured_rows(&view, rect.height(), last - first + 1) {
            Some(rows) => {
                let cursor = line_of(&self.buffer.iter_at_mark(&self.buffer.get_insert()));
                let start = if (first..=last).contains(&cursor) {
                    cursor.saturating_sub(rows).max(first)
                } else {
                    first
                };
                let span = (rows * 3 / 2).max(MIN_SCREEN);
                (start, start.saturating_add(span - 1).min(last))
            }
            None => (first, last),
        };
        let screen = (last - first + 1).max(MIN_SCREEN);
        Some(TagWindow {
            visible: first..(last + 1).min(count),
            tagged: first.saturating_sub(screen)..last.saturating_add(1 + screen).min(count),
        })
    }

    // ----- tagging -----

    /// Tags every line of `lines` that is not tagged (untagged or stale). With a `budget`, it
    /// stops once that much time has passed (counted after its set-up, and after at least one
    /// line, so every call makes progress); returns false when it stopped with lines left.
    fn tag_lines(&self, lines: Range<u32>, budget: Option<Duration>) -> bool {
        let Some(doc) = self.doc.upgrade() else {
            return true;
        };
        let count = doc.line_index().line_count();
        let lines = lines.start.min(count)..lines.end.min(count);
        {
            let mut marks = self.marks.borrow_mut();
            if marks.len() != count as usize {
                // Out of step with the text (cannot happen): assume tags everywhere.
                *marks = vec![STALE; count as usize];
            }
            if marks[lines.start as usize..lines.end as usize]
                .iter()
                .all(|&m| m == TAGGED)
            {
                return true;
            }
        }
        let states = doc.line_states.borrow();
        let prefix;
        let (states, lines) = match states.as_ref() {
            Some(states) => (states, lines),
            None => {
                // A rebuild is running; its end retags the window.
                let Some((states, exact)) = self.prefix_states(&doc, lines.end) else {
                    return true;
                };
                prefix = states;
                (&prefix, lines.start..lines.end.min(exact))
            }
        };
        let index = doc.line_index();
        let locator = self.locator(&index);
        let mut scratch = self.scratch.borrow_mut();
        let deadline = budget.map(|b| Instant::now() + b);
        let mut tagged = 0;
        for line in lines {
            let mark = self.marks.borrow()[line as usize];
            if mark == TAGGED {
                continue;
            }
            if tagged > 0 && deadline.is_some_and(|d| Instant::now() >= d) {
                return false;
            }
            self.tag_line(states, &index, &locator, line, mark == STALE, &mut scratch);
            self.marks.borrow_mut()[line as usize] = TAGGED;
            tagged += 1;
        }
        true
    }

    /// Scanner states for the first lines of the text while the document's own are being built:
    /// `None` unless `upto` is within [`PREFIX_SCAN_LINES`]. Also returns how many lines they
    /// describe exactly.
    fn prefix_states(&self, doc: &Document, upto: u32) -> Option<(LineStates, u32)> {
        if upto > PREFIX_SCAN_LINES {
            return None;
        }
        let end = self
            .buffer
            .iter_at_line(upto as i32 + 1)
            .unwrap_or_else(|| self.buffer.end_iter());
        let text = self.buffer.slice(&self.buffer.start_iter(), &end, true);
        let index = LineIndex::new(&text);
        let mut states = LineStates::new(doc.scan_config());
        states.rebuild(&StrLines {
            text: &text,
            index: &index,
        });
        // The last line may be cut short (GTK and `\n` lines differ around a lone `\r`).
        let complete = text.matches('\n').count() as u32 + u32::from(end.is_end());
        Some((states, complete))
    }

    /// Gives line `line` the tags of its current tokens. A stale line's tags are read first and
    /// only the tags whose ranges differ are rewritten: typing usually leaves them right (GTK
    /// moves tags with the text), and every tag change costs a signal, a relayout of the line
    /// and, for the no-spell tag, a spell recheck.
    fn tag_line(
        &self,
        states: &LineStates,
        index: &LineIndex,
        locator: &Locator,
        line: u32,
        stale: bool,
        scratch: &mut Scratch,
    ) {
        let Some(loc) = locator.line(&self.buffer, index, line) else {
            return;
        };
        let Scratch {
            tokens,
            spans,
            want,
            have,
            open,
        } = scratch;
        tokens.clear();
        states.tokens(line, &loc.text, tokens);
        spans.fill(tokens);
        want.resize_with(self.tags.all.len(), Vec::new);
        want.iter_mut().for_each(Vec::clear);
        for (kind, range) in &spans.styled {
            if let Some(i) = self.tags.kind_index[*kind as usize] {
                want[i].push(range.clone());
            }
        }
        want[self.tags.math_index()].extend(spans.math.iter().cloned());
        want[self.tags.no_spell_index()].extend(spans.no_spell.iter().cloned());
        let read = stale && self.read_tags(&loc, have, open);
        if stale && !read {
            self.clear(&loc.start, &loc.end);
        }
        let buffer = &self.buffer;
        for (i, ranges) in want.iter().enumerate() {
            let tag = &self.tags.all[i];
            if read {
                if have[i] == *ranges {
                    continue;
                }
                if !have[i].is_empty() {
                    buffer.remove_tag(tag, &loc.start, &loc.end);
                }
            }
            for range in ranges {
                let start = locator.at(&loc, range.start);
                let end = locator.at(&loc, range.end);
                buffer.apply_tag(tag, &start, &end);
            }
        }
    }

    /// Reads the ranges of our tags on the line `loc` into `have` (by index in `Tags::all`, byte
    /// columns; a range running into the line break ends after it). False where positions are
    /// not GTK line indices (a line GTK breaks at a lone `\r`): the caller then clears the line
    /// instead.
    fn read_tags(
        &self,
        loc: &LineLoc,
        have: &mut Vec<Vec<Range<u32>>>,
        open: &mut Vec<Option<u32>>,
    ) -> bool {
        if loc.split {
            return false;
        }
        let n = self.tags.all.len();
        have.resize_with(n, Vec::new);
        have.iter_mut().for_each(Vec::clear);
        open.clear();
        open.resize(n, None);
        for tag in loc.start.tags() {
            if let Some(i) = self.tags.index(&tag) {
                open[i] = Some(0);
            }
        }
        let mut iter = loc.start;
        while iter.forward_to_tag_toggle(None::<&gtk::TextTag>) && iter < loc.end {
            let col = iter.line_index() as u32;
            for tag in iter.toggled_tags(false) {
                if let Some(i) = self.tags.index(&tag)
                    && let Some(start) = open[i].take()
                {
                    have[i].push(start..col);
                }
            }
            for tag in iter.toggled_tags(true) {
                if let Some(i) = self.tags.index(&tag) {
                    open[i] = Some(col);
                }
            }
        }
        for (i, start) in open.iter_mut().enumerate() {
            if let Some(start) = start.take() {
                have[i].push(start..loc.len);
            }
        }
        true
    }

    /// Removes our tags from `start..end` (one line): only those actually there, found by
    /// walking the line's tag toggles.
    fn clear(&self, start: &gtk::TextIter, end: &gtk::TextIter) {
        let mut mask = 0u32;
        for tag in start.tags() {
            mask |= self.tags.bit(&tag);
        }
        let mut iter = *start;
        while iter.forward_to_tag_toggle(None::<&gtk::TextTag>) && iter < *end {
            for tag in iter.toggled_tags(true) {
                mask |= self.tags.bit(&tag);
            }
        }
        for (i, tag) in self.tags.all.iter().enumerate() {
            if mask & (1 << i) != 0 {
                self.buffer.remove_tag(tag, start, end);
            }
        }
    }
}

/// When `lines` visible lines are more than 1.5 times the rows of `view`'s font that fit in
/// `height` pixels (GTK counts the lines it has not measured yet as empty), the number of rows
/// that fit; `None` otherwise. The font is looked at only when the lines would be less than
/// [`MIN_ROW_HEIGHT`] pixels high each.
fn unmeasured_rows(view: &TexView, height: i32, lines: u32) -> Option<u32> {
    if i64::from(lines) * i64::from(MIN_ROW_HEIGHT) <= i64::from(height) {
        return None;
    }
    let metrics = view.pango_context().metrics(None, None);
    let row = (metrics.height() / pango::SCALE).max(1);
    let rows = u32::try_from(height / row).unwrap_or(0).max(1);
    (lines > rows * 3 / 2).then_some(rows)
}

/// About how many lines fit on `view`'s screen before it has a size: its window's height (or
/// default height) over the font's line height.
fn estimated_rows(view: &TexView) -> u32 {
    let metrics = view.pango_context().metrics(None, None);
    let line = (metrics.height() / pango::SCALE).max(1);
    let window = view.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let height = window
        .as_ref()
        .map(|w| w.height())
        .filter(|&h| h > 0)
        .or_else(|| {
            window
                .as_ref()
                .map(|w| w.default_height())
                .filter(|&h| h > 0)
        })
        .unwrap_or(UNSIZED_HEIGHT);
    (height / line).clamp(20, 400) as u32
}

/// Reusable buffers of [`Highlighter::tag_line`].
#[derive(Default)]
struct Scratch {
    tokens: Vec<Token>,
    spans: Spans,
    /// Wanted and present ranges per tag (index in `Tags::all`).
    want: Vec<Vec<Range<u32>>>,
    have: Vec<Vec<Range<u32>>>,
    open: Vec<Option<u32>>,
}

/// A line located in the buffer.
struct LineLoc {
    start: gtk::TextIter,
    /// After the line's `\n` (or the end of the buffer).
    end: gtk::TextIter,
    /// The line without its `\n` (a `\r` before it stays, as in `LineIndex`).
    text: String,
    /// Bytes from `start` to `end` (the text and its `\n`).
    len: u32,
    /// GTK breaks the line (at a lone `\r` or U+2029): byte columns are not GTK line indices.
    split: bool,
}

/// Finds lines and byte positions in a buffer. GTK breaks lines at `\n`, `\r\n`, a lone `\r`
/// and U+2029, `LineIndex` only at `\n`. While the two numberings agree GTK's line lookups are
/// used; otherwise GTK's lines are mapped through the positions of the extra breaks
/// ([`Breaks`]), found in one copy of the text after each change. Should that disagree with
/// GTK's line count, positions are counted in characters of one copy of the text instead.
#[derive(Clone)]
enum Locator {
    Gtk,
    Breaks(Rc<Breaks>),
    Chars(Rc<CharsText>),
}

/// The line breaks GTK has and `LineIndex` has not.
struct Breaks {
    /// Their byte offsets, ascending.
    at: Vec<usize>,
    /// For each, the GTK line that starts after it.
    gtk_after: Vec<u32>,
}

impl Breaks {
    /// The extra breaks of `text` (indexed by `index`), if they explain GTK's `gtk_lines`.
    fn new(text: &str, index: &LineIndex, gtk_lines: u32) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut at: Vec<usize> = text
            .match_indices('\r')
            .map(|(i, _)| i)
            .filter(|&i| bytes.get(i + 1) != Some(&b'\n'))
            .collect();
        at.extend(text.match_indices('\u{2029}').map(|(i, _)| i));
        at.sort_unstable();
        let extra = u32::try_from(at.len()).ok()?;
        if index.line_count().checked_add(extra) != Some(gtk_lines) {
            return None;
        }
        let gtk_after = at
            .iter()
            .zip(0u32..)
            .map(|(&p, k)| index.line_of(p) + k + 1)
            .collect();
        Some(Self { at, gtk_after })
    }

    /// The GTK line where the line starting at byte `start` (line `line`) starts.
    fn gtk_line(&self, line: u32, start: usize) -> u32 {
        line + self.at.partition_point(|&p| p < start) as u32
    }

    /// The `LineIndex` line of GTK line `gtk`.
    fn doc_line(&self, gtk: u32) -> u32 {
        gtk - self.gtk_after.partition_point(|&s| s <= gtk) as u32
    }

    /// Whether an extra break lies in `bytes`.
    fn within(&self, bytes: Range<usize>) -> bool {
        let i = self.at.partition_point(|&p| p < bytes.start);
        self.at.get(i).is_some_and(|&p| p < bytes.end)
    }
}

/// One copy of the text, for counting characters (see [`Locator`]).
struct CharsText {
    text: glib::GString,
    /// The last (byte, char) offset converted; conversions start from it.
    at: Cell<(usize, i32)>,
}

impl CharsText {
    /// The char offset of byte `byte` (a character boundary).
    fn char_offset(&self, byte: usize) -> Option<i32> {
        let (from_byte, from_char) = self.at.get();
        let chars = if byte >= from_byte {
            from_char + i32::try_from(self.text.get(from_byte..byte)?.chars().count()).ok()?
        } else {
            from_char - i32::try_from(self.text.get(byte..from_byte)?.chars().count()).ok()?
        };
        self.at.set((byte, chars));
        Some(chars)
    }

    /// The byte offset of char `chars` (the text's end past it).
    fn byte_offset(&self, chars: i32) -> usize {
        let (from_byte, from_char) = self.at.get();
        let found = if chars >= from_char {
            let n = usize::try_from(chars - from_char).unwrap_or(0);
            self.text[from_byte..]
                .char_indices()
                .nth(n)
                .map(|(i, _)| from_byte + i)
        } else {
            let n = usize::try_from(from_char - chars).unwrap_or(1);
            self.text[..from_byte]
                .char_indices()
                .rev()
                .nth(n - 1)
                .map(|(i, _)| i)
        };
        match found {
            Some(byte) => {
                self.at.set((byte, chars));
                byte
            }
            None if chars >= from_char => {
                let _ = self.char_offset(self.text.len());
                self.text.len()
            }
            None => {
                self.at.set((0, 0));
                0
            }
        }
    }
}

impl Locator {
    fn line(&self, buffer: &sourceview5::Buffer, index: &LineIndex, line: u32) -> Option<LineLoc> {
        if line >= index.line_count() {
            return None;
        }
        let offset = index.line_start(line);
        let next = if line + 1 < index.line_count() {
            index.line_start(line + 1)
        } else {
            index.text_len()
        };
        let (start, end, split) = match self {
            Self::Gtk => {
                let start = buffer.iter_at_line(i32::try_from(line).ok()?)?;
                let mut end = start;
                if !end.forward_line() {
                    end = buffer.end_iter();
                }
                (start, end, false)
            }
            Self::Breaks(breaks) => {
                let gtk = |line: u32, at: usize| {
                    i32::try_from(breaks.gtk_line(line, at))
                        .ok()
                        .and_then(|l| buffer.iter_at_line(l))
                };
                let start = gtk(line, offset)?;
                let end = if line + 1 < index.line_count() {
                    gtk(line + 1, next)?
                } else {
                    buffer.end_iter()
                };
                (start, end, breaks.within(offset..next))
            }
            Self::Chars(chars) => {
                let start = buffer.iter_at_offset(chars.char_offset(offset)?);
                let end = buffer.iter_at_offset(chars.char_offset(next)?);
                (start, end, true)
            }
        };
        let mut text = match self {
            Self::Chars(chars) => chars.text.get(offset..next)?.to_owned(),
            _ => buffer.slice(&start, &end, true).to_string(),
        };
        let len = text.len() as u32;
        if text.ends_with('\n') {
            text.pop();
        }
        Some(LineLoc {
            start,
            end,
            text,
            len,
            split,
        })
    }

    /// The iter at byte `col` of the line `loc` (a token boundary, so a character boundary).
    fn at(&self, loc: &LineLoc, col: u32) -> gtk::TextIter {
        if !loc.split {
            let mut iter = loc.start;
            iter.set_line_index(col as i32);
            return iter;
        }
        // Across GTK lines: count characters from the line's start.
        let col = col as usize;
        if col > loc.text.len() {
            return loc.end;
        }
        let mut iter = loc.start;
        let chars = loc.text.get(..col).map_or(0, |t| t.chars().count());
        iter.forward_chars(i32::try_from(chars).unwrap_or(i32::MAX));
        iter
    }

    /// The `LineIndex` line of `iter`.
    fn line_of(&self, index: &LineIndex, iter: &gtk::TextIter) -> u32 {
        match self {
            Self::Gtk => index.line_of(doc_offset(index, iter)),
            Self::Breaks(breaks) => breaks
                .doc_line(u32::try_from(iter.line()).unwrap_or(0))
                .min(index.line_count().saturating_sub(1)),
            Self::Chars(chars) => index.line_of(chars.byte_offset(iter.offset())),
        }
    }
}

/// The byte offset of `iter` while GTK's lines are `index`'s.
fn doc_offset(index: &LineIndex, iter: &gtk::TextIter) -> usize {
    let line = u32::try_from(iter.line()).unwrap_or(0);
    let col = u32::try_from(iter.line_index()).unwrap_or(0);
    index.offset(line, col)
}

// ----- deleted ("phantom") text of the review overlay -----

/// The scanner context in which deleted text drawn above line `line` of `view`'s document
/// starts (the state at that line's start), or `None` when `view` is not highlighted by the
/// scanner (then deleted text gets no syntax colours).
pub fn phantom_context(view: &TexView, line: u32) -> Option<Context> {
    highlighter_for(view)?;
    let doc = view.document()?;
    let offset = {
        let index = doc.line_index();
        if line < index.line_count() {
            index.line_start(line)
        } else {
            index.text_len()
        }
    };
    Some(doc.context_at_byte(offset))
}

/// [`TextSyntax`] for deleted text shown in `view`: its document's scanner rules and its
/// buffer's style scheme.
pub fn phantom_syntax(view: &TexView, text: &str, start_ctx: &Context) -> TextSyntax {
    let config = view
        .document()
        .map_or_else(|| Arc::new(ScanConfig::default()), |d| d.scan_config());
    let scheme = view
        .buffer()
        .downcast::<sourceview5::Buffer>()
        .ok()
        .and_then(|b| b.style_scheme());
    TextSyntax::new(text, start_ctx, config, scheme.as_ref())
}

/// Pango attributes giving `text` the scanner's colours (foreground, weight, slant, underline;
/// no backgrounds), as if it started in `start_ctx`, with `scheme`'s styles and the default
/// scanner rules. Byte indices are those of `text`.
pub fn attrs_for(
    text: &str,
    start_ctx: &Context,
    scheme: Option<&sourceview5::StyleScheme>,
) -> pango::AttrList {
    let syntax = TextSyntax::new(text, start_ctx, Arc::new(ScanConfig::default()), scheme);
    let attrs = pango::AttrList::new();
    let index = LineIndex::new(text);
    for line in 0..index.line_count() {
        let range = index.line_range(line);
        syntax.insert_attrs(&attrs, line, &text[range.clone()], range.start as u32);
    }
    attrs
}

/// Syntax colours for a text that is not in a buffer (deleted text of a hunk): the scanner
/// states of the whole text, started in a given context, and the Pango look of each token kind.
///
/// Building scanner states starts with the rules' tables, and the review overlay builds one
/// `TextSyntax` per hunk: so the states of the last one dropped are kept for the next (unless
/// they are large), and the looks are kept per style scheme.
pub struct TextSyntax {
    /// Always `Some` until dropped (then it goes back to [`SPARE_STATES`]).
    states: Option<LineStates>,
    /// Indexed by `TokenKind as usize`.
    styles: Rc<Vec<AttrStyle>>,
}

/// Texts of at most this many lines leave their scanner states for the next [`TextSyntax`].
const SPARE_STATES_LINES: u32 = 10_000;

/// The looks of the token kinds (by `TokenKind as usize`) in a style scheme.
type SchemeLooks = (Option<sourceview5::StyleScheme>, Rc<Vec<AttrStyle>>);

thread_local! {
    static SPARE_STATES: RefCell<Option<LineStates>> = const { RefCell::new(None) };
    /// The looks of the token kinds in the last style scheme asked for.
    static ATTR_STYLES: RefCell<Option<SchemeLooks>> = const { RefCell::new(None) };
}

impl TextSyntax {
    /// Scans `text` as if it started in `start_ctx` (a line that recreates the context is
    /// scanned first), with the rules `config` and the styles of `scheme`.
    pub fn new(
        text: &str,
        start_ctx: &Context,
        config: Arc<ScanConfig>,
        scheme: Option<&sourceview5::StyleScheme>,
    ) -> Self {
        let prefix = context_prefix(start_ctx);
        let index = LineIndex::new(text);
        let mut states = SPARE_STATES
            .with_borrow_mut(Option::take)
            .filter(|s| Arc::ptr_eq(s.config(), &config) || **s.config() == *config)
            .unwrap_or_else(|| LineStates::new(config));
        states.rebuild(&Prefixed {
            prefix: &prefix,
            text,
            index: &index,
        });
        Self {
            states: Some(states),
            styles: attr_styles(scheme),
        }
    }

    /// Inserts the attributes of line `line` of the text into `attrs`, for a layout that holds
    /// that line, whose content is `text` (without its line end), at byte `base`.
    pub fn insert_attrs(&self, attrs: &pango::AttrList, line: u32, text: &str, base: u32) {
        let Some(states) = &self.states else {
            return;
        };
        let mut tokens = Vec::new();
        // Line 0 of the scanned text is the context prefix.
        states.tokens(line + 1, text, &mut tokens);
        let len = text.len() as u32;
        for token in &tokens {
            let style = &self.styles[token.kind as usize];
            let (start, end) = (token.start.min(len), token.end.min(len));
            if start < end {
                style.insert(attrs, base + start, base + end);
            }
        }
    }
}

impl Drop for TextSyntax {
    fn drop(&mut self) {
        if let Some(states) = self.states.take()
            && states.line_count() <= SPARE_STATES_LINES
        {
            SPARE_STATES.with_borrow_mut(|spare| *spare = Some(states));
        }
    }
}

/// The looks of the token kinds in `scheme` (cached for the last scheme asked for).
fn attr_styles(scheme: Option<&sourceview5::StyleScheme>) -> Rc<Vec<AttrStyle>> {
    if let Some(styles) = ATTR_STYLES.with_borrow(|cached| {
        cached
            .as_ref()
            .filter(|(s, _)| s.as_ref() == scheme)
            .map(|(_, styles)| styles.clone())
    }) {
        return styles;
    }
    let styles: Rc<Vec<AttrStyle>> = Rc::new(
        SPECS
            .iter()
            .map(|s| {
                first_style(scheme, s.styles)
                    .map_or_else(AttrStyle::default, |style| AttrStyle::new(&style))
            })
            .collect(),
    );
    ATTR_STYLES.with_borrow_mut(|cached| *cached = Some((scheme.cloned(), styles.clone())));
    styles
}

/// [`Lines`] of a text preceded by one extra line.
struct Prefixed<'a> {
    prefix: &'a str,
    text: &'a str,
    index: &'a LineIndex,
}

impl Lines for Prefixed<'_> {
    fn line_count(&self) -> u32 {
        self.index.line_count() + 1
    }

    fn line(&self, i: u32) -> Cow<'_, str> {
        match i.checked_sub(1) {
            None => Cow::Borrowed(self.prefix),
            Some(line) => Cow::Borrowed(&self.text[self.index.line_range(line)]),
        }
    }
}

/// A line that leaves the scanner in (an approximation of) `ctx` at its end.
fn context_prefix(ctx: &Context) -> String {
    match ctx {
        Context::Text | Context::CommandName { .. } => String::new(),
        Context::Math { env: Some(env), .. } => format!("\\begin{{{env}}}"),
        Context::Math {
            display: true,
            env: None,
        } => "\\[".into(),
        Context::Math {
            display: false,
            env: None,
        } => "$".into(),
        Context::TextInMath => "$\\text{".into(),
        // At a line start only a comment environment is a comment.
        Context::Comment => "\\begin{comment}".into(),
        Context::Verbatim => "\\begin{verbatim}".into(),
        Context::Arg { cmd, idx, kind, .. } => {
            let open = if *kind == ArgKind::Key { "[" } else { "{" };
            format!("\\{cmd}{}{open}", "{}".repeat(usize::from(*idx)))
        }
    }
}

/// The Pango look of one token kind.
#[derive(Clone, Debug, Default, PartialEq)]
struct AttrStyle {
    foreground: Option<gdk::RGBA>,
    weight: Option<pango::Weight>,
    italic: Option<bool>,
    underline: Option<pango::Underline>,
    strikethrough: Option<bool>,
}

impl AttrStyle {
    fn new(style: &sourceview5::Style) -> Self {
        let weight = if style.is_weight_set() {
            Some(style.weight())
        } else if style.is_bold_set() {
            Some(if style.is_bold() {
                pango::Weight::Bold
            } else {
                pango::Weight::Normal
            })
        } else {
            None
        };
        Self {
            foreground: style
                .is_foreground_set()
                .then(|| style.foreground())
                .flatten()
                .and_then(|c| gdk::RGBA::parse(c.as_str()).ok()),
            weight,
            italic: style.is_italic_set().then(|| style.is_italic()),
            underline: style.is_underline_set().then(|| style.pango_underline()),
            strikethrough: style
                .is_strikethrough_set()
                .then(|| style.is_strikethrough()),
        }
    }

    fn insert(&self, attrs: &pango::AttrList, start: u32, end: u32) {
        let add = |mut attr: pango::Attribute| {
            attr.set_start_index(start);
            attr.set_end_index(end);
            attrs.insert(attr);
        };
        let channel = |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        if let Some(c) = &self.foreground {
            add(pango::AttrColor::new_foreground(
                channel(c.red()),
                channel(c.green()),
                channel(c.blue()),
            )
            .upcast());
        }
        if let Some(weight) = self.weight {
            add(pango::AttrInt::new_weight(weight).upcast());
        }
        if let Some(italic) = self.italic {
            let style = if italic {
                pango::Style::Italic
            } else {
                pango::Style::Normal
            };
            add(pango::AttrInt::new_style(style).upcast());
        }
        if let Some(underline) = self.underline {
            add(pango::AttrInt::new_underline(underline).upcast());
        }
        if let Some(strikethrough) = self.strikethrough {
            add(pango::AttrInt::new_strikethrough(strikethrough).upcast());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_of(line: &str, ctx: &Context) -> Vec<Token> {
        let syntax_states = {
            let index = LineIndex::new(line);
            let prefix = context_prefix(ctx);
            let mut states = LineStates::new(Arc::new(ScanConfig::default()));
            states.rebuild(&Prefixed {
                prefix: &prefix,
                text: line,
                index: &index,
            });
            states
        };
        let mut out = Vec::new();
        syntax_states.tokens(1, line, &mut out);
        out
    }

    #[test]
    fn specs_are_indexed_by_kind() {
        for (i, spec) in SPECS.iter().enumerate() {
            assert_eq!(spec.kind as usize, i, "{:?}", spec.kind);
        }
        let names: Vec<&str> = SPECS.iter().filter_map(|s| s.tag).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "tag names are unique");
        assert!(!names.contains(&MATH_TAG) && !names.contains(&NO_SPELL_TAG));
        // Every kind with a tag has a style to take from the scheme; of the kinds without a tag,
        // only math has one (the math tag's).
        for s in &SPECS {
            let math = matches!(s.kind, TokenKind::Math | TokenKind::MathDelim);
            assert_eq!(s.tag.is_none(), s.styles.is_empty() || math, "{:?}", s.kind);
            assert!(!math || s.styles == MATH);
        }
    }

    #[test]
    fn extra_line_breaks_map_gtk_lines() {
        // GTK lines "a\r", "b\n", "c\u{2029}", "d\r\n", "e"; `LineIndex` lines "a\rb",
        // "c\u{2029}d\r", "e".
        let text = "a\rb\nc\u{2029}d\r\ne";
        let index = LineIndex::new(text);
        assert!(
            Breaks::new(text, &index, 4).is_none(),
            "GTK's count must match"
        );
        let breaks = Breaks::new(text, &index, 5).unwrap();
        assert_eq!(breaks.at, [1, 5]);
        let doc_lines: Vec<u32> = (0..5).map(|g| breaks.doc_line(g)).collect();
        assert_eq!(doc_lines, [0, 0, 1, 1, 2]);
        let gtk_lines: Vec<u32> = (0..3)
            .map(|l| breaks.gtk_line(l, index.line_start(l)))
            .collect();
        assert_eq!(gtk_lines, [0, 2, 4]);
        assert!(breaks.within(0..4) && breaks.within(4..11) && !breaks.within(11..12));
    }

    #[test]
    fn characters_are_counted_from_the_last_conversion() {
        // a é \n b ç: bytes 0 1 3 4 5, 7 in all.
        let chars = CharsText {
            text: "aé\nbç".into(),
            at: Cell::new((0, 0)),
        };
        assert_eq!(chars.char_offset(3), Some(2));
        assert_eq!(chars.char_offset(1), Some(1), "backwards");
        assert_eq!(chars.byte_offset(4), 5);
        assert_eq!(chars.byte_offset(0), 0);
        assert_eq!(chars.byte_offset(5), 7, "the end");
        assert_eq!(chars.byte_offset(9), 7, "past the end");
        assert_eq!(chars.byte_offset(2), 3);
    }

    #[test]
    fn spans_merge_no_spell_runs_and_math_runs() {
        let line = r"Let $x &= \alpha$ by \cite{k}~\ref{a}.";
        let tokens = tokens_of(line, &Context::Text);
        let mut spans = Spans::default();
        spans.fill(&tokens);
        let text = |r: &Range<u32>| &line[r.start as usize..r.end as usize];
        let no_spell: Vec<&str> = spans.no_spell.iter().map(text).collect();
        assert_eq!(no_spell, [r"$x &= \alpha$", r"\cite{k}~\ref{a}"]);
        let math: Vec<&str> = spans.math.iter().map(text).collect();
        assert_eq!(math, [r"$x &= \alpha$"]);
        // Text and braces have no tag.
        assert!(
            spans.styled.iter().all(|(k, _)| !matches!(
                k,
                TokenKind::Text | TokenKind::Brace | TokenKind::Bracket
            ))
        );
    }

    #[test]
    fn math_runs_continue_across_ampersands_only() {
        let mut spans = Spans::default();
        let ctx = Context::Math {
            display: true,
            env: Some("align".into()),
        };
        let row = r"  &= x \\";
        spans.fill(&tokens_of(row, &ctx));
        assert_eq!(spans.math, vec![0..row.len() as u32], "{spans:?}");
        // In text, `&` and `~` next to math are not math.
        let line = r"a & b~$y$";
        spans.fill(&tokens_of(line, &Context::Text));
        let math: Vec<&str> = spans
            .math
            .iter()
            .map(|r| &line[r.start as usize..r.end as usize])
            .collect();
        assert_eq!(math, ["$y$"]);
    }

    #[test]
    fn context_prefixes_recreate_the_context() {
        let cases = [
            (Context::Text, Context::Text),
            (
                Context::Math {
                    display: true,
                    env: Some("align*".into()),
                },
                Context::Math {
                    display: true,
                    env: Some("align*".into()),
                },
            ),
            (
                Context::Math {
                    display: true,
                    env: None,
                },
                Context::Math {
                    display: true,
                    env: None,
                },
            ),
            (
                Context::Math {
                    display: false,
                    env: None,
                },
                Context::Math {
                    display: false,
                    env: None,
                },
            ),
            (Context::TextInMath, Context::TextInMath),
            (Context::Verbatim, Context::Verbatim),
            (Context::Comment, Context::Comment),
        ];
        for (ctx, expected) in cases {
            let prefix = context_prefix(&ctx);
            let text = format!("{prefix}\nx");
            let index = LineIndex::new(&text);
            let mut states = LineStates::new(Arc::new(ScanConfig::default()));
            states.rebuild(&StrLines {
                text: &text,
                index: &index,
            });
            assert_eq!(states.context_at(1, "x", 0), expected, "prefix {prefix:?}");
        }
        let cite = Context::Arg {
            cmd: "cite".into(),
            idx: 0,
            kind: ArgKind::Cite,
            start: 0,
        };
        let tokens = tokens_of("key1,key2}", &cite);
        assert_eq!(tokens[0].kind, TokenKind::CiteArg, "{tokens:?}");
    }

    #[test]
    fn text_syntax_colours_by_line() {
        let text = "a \\emph{b}\n$x$ % c\n";
        let syntax = TextSyntax::new(text, &Context::Text, Arc::default(), None);
        // Without a scheme there are no styles, hence no attributes.
        let attrs = pango::AttrList::new();
        syntax.insert_attrs(&attrs, 0, "a \\emph{b}", 0);
        assert!(attrs.attributes().is_empty());
        // Starting in math, the first line's `x` is math (the scanner state is there).
        let syntax = TextSyntax::new(
            "x$ y",
            &Context::Math {
                display: false,
                env: None,
            },
            Arc::default(),
            None,
        );
        let mut tokens = Vec::new();
        syntax
            .states
            .as_ref()
            .unwrap()
            .tokens(1, "x$ y", &mut tokens);
        assert_eq!(tokens[0].kind, TokenKind::Math);
        assert_eq!(tokens[1].kind, TokenKind::MathDelim);
        assert_eq!(tokens[2].kind, TokenKind::Text);
    }
}
