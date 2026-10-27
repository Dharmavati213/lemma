//! The main window: layout, window actions, editor tabs and the status bar.
//!
//! ```text
//! ┌ header: [sidebar][open][save]   main.tex — ~/proj   [review][state][build][pdf][agent][menu] ┐
//! ├ banner (hidden) ─────────────────────────────────────────────────────────────────────────────┤
//! │ sidebar        │ tab bar                          │ PDF pane          │ agent pane (hidden)  │
//! │ Outline        │ find bar (hidden)                │ [strip] PDF view  │                      │
//! │ Changes        │ editor (TexView)                 │                   │                      │
//! │ Problems       │                                  │                   │                      │
//! │ Search         │                                  │                   │                      │
//! ├ status: file · vim command · mode · [words] · Ln, Col · build ───────────────────────────────┤
//! ```
//!
//! [`WindowState`] holds the window's widgets; the feature modules (`build_ui`,
//! `pdf::pdf_controller`, `review_ui`, `diagnostics_panel`, `changes_panel`, from M2
//! `review_persist`, `agent::*` and the per-editor `editor::*` hooks, and the M3 modules) get it
//! in their `install` and `attach` hooks and fill in their slots.
//!
//! M3 (WP3.0) adds the Search page and the PDF strip slots, the word-count slot in the status
//! bar, the M3 actions (each runs its module's function; defaults in `app::DEFAULT_ACCELS`),
//! the session hooks ([`WindowState::session_changed`], `session::save` on close) and
//! [`WindowState::edit_config_file`] for Preferences.
//!
//! The find bar of the active editor (`find_bar`) sits in [`WindowState::find_slot`], between the
//! tab bar and the editors.

mod layout;

use crate::agent::{self, AgentController};
use crate::build_ui::{self, BuildController, Reason};
use crate::document::{DocRef, Document};
use crate::editor::vim::{self, FileCommand, parse_file_command};
use crate::editor::{
    self, Editor, VimMode, annotations, completion, goto, highlight, hover, keys, math_preview,
    pairs, spell, typing,
};
use crate::hooks::{HookId, Hooks};
use crate::pdf::pdf_controller::{self, PdfController};
use crate::review_ui::{self, ReviewController};
use crate::saver::SaveError;
use crate::settings::{self, ConfigFile, LiveConfig, Settings, SettingsFiles};
use crate::workspace::{Workspace, normalize};
use crate::{changes_panel, diagnostics_panel, file_commands, review_persist};
use crate::{
    find_bar, git_gutter, history_ui, labels, lint, outline_panel, palette, pdf, prefs, quick_open,
    quickfix, search_panel, session, shortcuts, wordcount,
};
use adw::prelude::*;
use lemma_config::{Config, Paths, PromptTemplate};
use lemma_snippets::SnippetSet;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Name of the Outline page of [`Sidebar::stack`].
pub const PAGE_OUTLINE: &str = "outline";
/// Name of the Changes page of [`Sidebar::stack`].
pub const PAGE_CHANGES: &str = "changes";
/// Name of the Problems page of [`Sidebar::stack`].
pub const PAGE_PROBLEMS: &str = "problems";
/// Name of the Search page of [`Sidebar::stack`] (project search, M3).
pub const PAGE_SEARCH: &str = "search";

/// Header-bar widgets.
pub struct Header {
    pub bar: adw::HeaderBar,
    /// Active document name (title) and project directory (subtitle).
    pub title: adw::WindowTitle,
    /// Runs `win.build`.
    pub build_button: gtk::Button,
    /// Slot next to the build button for the build state (spinner, ✓, error count). Empty in M0.
    pub build_indicator: adw::Bin,
    /// Slot for the review summary ("3 files · 7 hunks"). Empty in M0.
    pub review_summary: adw::Bin,
    /// The main menu (the header's menu button), in sections.
    pub menu: gio::Menu,
    /// Its file section (New File, Save As, Save All, Close Tab); local history adds "Restore
    /// from History…".
    pub menu_files: gio::Menu,
    /// Its navigation section (M3: Command Palette, Go to File, Find in Project).
    pub menu_go: gio::Menu,
    /// Its view section (Vim Mode, Sidebar, PDF Preview, PDF in Separate Window, Agent Panel);
    /// spell checking adds "Check Spelling".
    pub menu_view: gio::Menu,
}

/// The left sidebar: a stack with Outline, Changes, Problems and Search pages (the switcher
/// shows their icons). Each page is an `adw::Bin` holding a placeholder until its owner sets a
/// child.
pub struct Sidebar {
    pub split: adw::OverlaySplitView,
    pub stack: gtk::Stack,
    pub outline: adw::Bin,
    pub changes: adw::Bin,
    pub problems: adw::Bin,
    /// Project search and replace (WP3.6).
    pub search: adw::Bin,
}

impl Sidebar {
    /// Shows the sidebar with page `name` (one of [`PAGE_OUTLINE`], [`PAGE_CHANGES`],
    /// [`PAGE_PROBLEMS`] and [`PAGE_SEARCH`]).
    pub fn show_page(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        self.split.set_show_sidebar(true);
    }

    /// Hides the sidebar while it is an overlay (in a narrow window), so the editor it covers can
    /// be seen after navigating from one of its pages.
    pub fn dismiss_overlay(&self) {
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
    }
}

/// The PDF pane's collapsible left strip for page thumbnails and bookmarks (M3, WP3.4): hidden
/// unless `[pdf] thumbnails`; `win.toggle-pdf-sidebar` (the `toggle` button, which the PDF
/// controller puts at the start of its toolbar) shows and hides it.
pub struct PdfSidebar {
    /// Its content is the PDF view (set by `pdf_controller::install`).
    pub split: adw::OverlaySplitView,
    /// Holds a placeholder until WP3.4 sets the strip.
    pub slot: adw::Bin,
    pub toggle: gtk::ToggleButton,
}

/// The right sidebar for the agent panel (hidden by default), and the window's agent.
pub struct AgentPane {
    pub split: adw::OverlaySplitView,
    pub slot: adw::Bin,
    /// The window's agent (set right after the M1 install hooks; see `agent`).
    pub controller: OnceCell<Rc<AgentController>>,
}

/// The slim status bar at the bottom of the window.
pub struct StatusBar {
    pub bar: gtk::Box,
    /// Path of the active document relative to the project root, with `•` when modified.
    pub file: gtk::Label,
    /// Vim command line or pending keys.
    pub vim_command: gtk::Label,
    /// Vim mode (hidden while Vim is off).
    pub mode: gtk::Label,
    /// Slot for the word count (WP3.7), hidden until its owner makes it visible.
    pub word_count: adw::Bin,
    /// `Ln 12, Col 5`.
    pub position: gtk::Label,
    /// Build status text (WP1.6 updates it).
    pub build: gtk::Label,
}

/// The Overwrite toast offered for one document (see [`WindowState::offer_overwrite`]).
struct OverwriteOffer {
    toast: adw::Toast,
    doc: Weak<Document>,
    /// Build again for this reason once the file is overwritten (a build's save was refused).
    rebuild: Rc<Cell<Option<Reason>>>,
}

type ConfigChangedFn = dyn Fn(&Rc<WindowState>, ConfigFile);
type RootChangedFn = dyn Fn(&Rc<WindowState>);
/// A test hook for [`WindowState::send_notification`]: `(id, notification)`.
pub type NotificationHook = dyn Fn(&str, &gio::Notification);

/// Everything one window consists of. Shared as `Rc<WindowState>`; widget callbacks hold weak
/// references.
pub struct WindowState {
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    /// The configuration, always the current one: reloads of `config.toml` replace it (see
    /// [`WindowState::set_config`] and [`WindowState::connect_config_changed`]).
    pub config: LiveConfig,
    pub paths: Option<Paths>,
    pub workspace: Rc<Workspace>,
    pub toasts: adw::ToastOverlay,
    /// Hidden by default; the review controller uses it for "Claude changed 3 files · 7 hunks".
    pub banner: adw::Banner,
    pub header: Header,
    pub sidebar: Sidebar,
    pub agent: AgentPane,
    /// Editor area (start) | PDF pane (end).
    pub paned: gtk::Paned,
    /// Slot for the PDF view; holds a placeholder until WP1.6 sets its view.
    pub pdf_pane: adw::Bin,
    /// The PDF pane's strip of thumbnails and bookmarks (M3).
    pub pdf_sidebar: PdfSidebar,
    pub tab_view: adw::TabView,
    pub tab_bar: adw::TabBar,
    /// Slot for the find bar (`find_bar::install` fills it), between the tab bar and the editors.
    pub find_slot: adw::Bin,
    pub status: StatusBar,
    /// True for runs that quit by themselves (`--quit-after-ms`) and for tests: nobody can answer
    /// a dialog, so none is shown (unsaved changes are not asked about).
    pub scripted: bool,
    /// Set by `build_ui::install`.
    pub build: OnceCell<BuildController>,
    /// Set by `review_ui::install`.
    pub review: OnceCell<ReviewController>,
    /// Set by `pdf_controller::install`.
    pub pdf: OnceCell<PdfController>,
    /// The window-wide section of every editor's context menu (quick actions, WP2.5); appended
    /// to each `Editor::extra_menu`.
    pub editor_menu: gio::Menu,
    editors: RefCell<Vec<Rc<Editor>>>,
    editor_stack: gtk::Stack,
    /// Documents are opening (see [`WindowState::set_opening`]).
    opening: Cell<bool>,
    vim_enabled: Cell<bool>,
    /// The keyboard focus is in an editor or the find bar (see [`WindowState::focus_moved`]).
    editor_focused: Cell<bool>,
    close_confirmed: Cell<bool>,
    /// `None` until set: the defaults.
    snippets: RefCell<Option<Rc<SnippetSet>>>,
    /// `None` until set: the defaults.
    prompts: RefCell<Option<Rc<Vec<PromptTemplate>>>>,
    config_hooks: Hooks<ConfigChangedFn>,
    root_hooks: Hooks<RootChangedFn>,
    notification_hook: RefCell<Option<Box<NotificationHook>>>,
    focus_override: Cell<Option<bool>>,
    /// Set by [`WindowState::pin_root`]: opening a document never moves the project root.
    root_pinned: Cell<bool>,
    /// The Overwrite toast shown (or queued) per document path: at most one each.
    overwrite_offers: RefCell<HashMap<PathBuf, OverwriteOffer>>,
    /// Runs [`WindowState::trim_memory`] once the window has been in the background for
    /// `trim_delay`.
    trim_timer: RefCell<Option<glib::SourceId>>,
    trim_delay: Cell<Duration>,
    /// Returns the memory of closed tabs to the system ([`CLOSED_TAB_TRIM_DELAY`] after a close).
    heap_trim: RefCell<Option<glib::SourceId>>,
    /// The settings files the app reads (`None` without a home directory; set by the app).
    settings_files: RefCell<Option<SettingsFiles>>,
    /// Saves the session [`SessionTiming::debounce`] after the last change.
    session_timer: RefCell<Option<glib::SourceId>>,
    session: SessionTiming,
}

/// When the session is saved (see [`WindowState::session_changed`]).
struct SessionTiming {
    /// `None`: the default ([`WindowState::session_saves_enabled`]).
    autosave: Cell<Option<bool>>,
    debounce: Cell<Duration>,
}

/// How long after the last change the shell saves the session.
pub const SESSION_DEBOUNCE: Duration = Duration::from_secs(2);

/// How long after a tab closes the allocator gives the memory it freed back to the system
/// (several closes in a row are handled once).
pub const CLOSED_TAB_TRIM_DELAY: Duration = Duration::from_secs(1);

/// How long a window stays in the background before it gives memory back
/// ([`WindowState::trim_memory`], ARCHITECTURE §7).
pub const INACTIVE_TRIM_DELAY: Duration = Duration::from_secs(60);

/// The user's answer to "save changes?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveChoice {
    Save,
    Discard,
    Cancel,
}

impl WindowState {
    /// Builds a window for `workspace`, installs its actions and the hooks of later work packages.
    /// The window is not presented yet.
    pub fn new(
        app: &adw::Application,
        config: Rc<Config>,
        paths: Option<Paths>,
        workspace: Rc<Workspace>,
        scripted: bool,
    ) -> Rc<Self> {
        Self::build(app, config, None, paths, workspace, scripted)
    }

    /// [`WindowState::new`] with all of `settings` (the app passes what it read from the
    /// settings files), so the install hooks already see the user's snippets and prompts.
    pub fn new_with_settings(
        app: &adw::Application,
        settings: &Settings,
        paths: Option<Paths>,
        workspace: Rc<Workspace>,
        scripted: bool,
    ) -> Rc<Self> {
        Self::build(
            app,
            settings.config.clone(),
            Some(settings),
            paths,
            workspace,
            scripted,
        )
    }

    fn build(
        app: &adw::Application,
        config: Rc<Config>,
        settings: Option<&Settings>,
        paths: Option<Paths>,
        workspace: Rc<Workspace>,
        scripted: bool,
    ) -> Rc<Self> {
        workspace.set_scan_config(settings::scan_config(&config.editor));
        let layout::Layout {
            window,
            toasts,
            banner,
            header,
            sidebar,
            agent,
            paned,
            pdf_pane,
            pdf_sidebar,
            tab_view,
            tab_bar,
            find_slot,
            editor_stack,
            status,
        } = layout::build(app);
        pdf_sidebar.split.set_show_sidebar(config.pdf.thumbnails);
        let state = Rc::new(Self {
            app: app.clone(),
            window,
            vim_enabled: Cell::new(config.editor.vim),
            config: LiveConfig::new(config),
            paths,
            workspace,
            toasts,
            banner,
            header,
            sidebar,
            agent,
            paned,
            pdf_pane,
            pdf_sidebar,
            tab_view,
            tab_bar,
            find_slot,
            status,
            scripted,
            build: OnceCell::new(),
            review: OnceCell::new(),
            pdf: OnceCell::new(),
            editor_menu: gio::Menu::new(),
            editors: RefCell::new(Vec::new()),
            editor_stack,
            opening: Cell::new(false),
            editor_focused: Cell::new(false),
            close_confirmed: Cell::new(false),
            snippets: RefCell::new(settings.map(|s| s.snippets.clone())),
            prompts: RefCell::new(settings.map(|s| s.prompts.clone())),
            config_hooks: Hooks::default(),
            root_hooks: Hooks::default(),
            notification_hook: RefCell::new(None),
            focus_override: Cell::new(None),
            root_pinned: Cell::new(false),
            overwrite_offers: RefCell::new(HashMap::new()),
            trim_timer: RefCell::new(None),
            trim_delay: Cell::new(INACTIVE_TRIM_DELAY),
            heap_trim: RefCell::new(None),
            settings_files: RefCell::new(None),
            session_timer: RefCell::new(None),
            session: SessionTiming {
                autosave: Cell::new(None),
                debounce: Cell::new(SESSION_DEBOUNCE),
            },
        });
        state.install_actions();
        state.install_m3_actions();
        state.connect_signals();
        state.install_hooks();
        state.connect_session_triggers();
        state.refresh_status();
        state
    }

    fn install_hooks(self: &Rc<Self>) {
        // A fresh OnceCell is always empty, so these sets cannot fail.
        let _ = self.build.set(build_ui::install(self));
        let _ = self.pdf.set(pdf_controller::install(self));
        let _ = self.review.set(review_ui::install(self));
        diagnostics_panel::install(self);
        changes_panel::install(self);
        review_persist::install(self);
        let _ = self.agent.controller.set(AgentController::new(self));
        agent::panel::install(self);
        agent::inline::install(self);
        agent::quick::install(self);
        agent::watcher::install(self);
        agent::history::install(self);
        keys::install_window(self);
        goto::install(self);
        outline_panel::install(self);
        labels::install(self);
        math_preview::install(self);
        pdf::links::install(self);
        pdf::search::install(self);
        pdf::sidebar::install(self);
        pdf::detach::install(self);
        search_panel::install(self);
        find_bar::install(self);
        lint::install(self);
        quickfix::install(self);
        wordcount::install(self);
        git_gutter::install(self);
        history_ui::install(self);
        // M5 (WP5.3).
        crate::editor::envtools::install(self);
        crate::editor::align::install(self);
        crate::format::install(self);
        // M5 (WP5.5).
        crate::templates::install(self);
        crate::bibimport::install(self);
        crate::autosave::install(self);
        crate::recovery::install(self);
        // Editing commands (Toggle Comment, Bold, Italic, Emphasize).
        crate::editor::editing::install(self);
    }

    /// The M3 per-editor hooks, after the M2 ones: the hover and goto registries first, so the
    /// others can register providers and resolvers.
    fn attach_m3(self: &Rc<Self>, editor: &Rc<Editor>) {
        goto::attach(self, editor);
        hover::attach(self, editor);
        labels::attach(self, editor);
        annotations::attach(self, editor);
        math_preview::attach(self, editor);
        pairs::attach(self, editor);
        crate::editor::envtools::attach(self, editor);
        git_gutter::attach(self, editor);
        crate::paste::attach(self, editor);
        crate::autosave::attach(self, editor);
        crate::recovery::attach(self, editor);
    }

    /// [`WindowState::attach_m3`] undone, in reverse order.
    fn detach_m3(self: &Rc<Self>, editor: &Rc<Editor>) {
        crate::recovery::detach(self, editor);
        crate::autosave::detach(self, editor);
        git_gutter::detach(self, editor);
        crate::editor::envtools::detach(self, editor);
        pairs::detach(self, editor);
        math_preview::detach(self, editor);
        annotations::detach(self, editor);
        labels::detach(self, editor);
        hover::detach(self, editor);
        goto::detach(self, editor);
    }

    /// The window's agent controller (set while the window is built).
    pub fn agent_controller(&self) -> Option<&Rc<AgentController>> {
        self.agent.controller.get()
    }

    // ----- documents and editors -----

    /// Keeps the project root where it is: the user chose it (`lemma DIR`), so the first
    /// document opened does not move it (see [`WindowState::open_file`]).
    pub fn pin_root(&self) {
        self.root_pinned.set(true);
    }

    /// Whether [`WindowState::open_file`] of `path` leaves it inside the project root: it lies
    /// under the root, or it is the first document and moves the root to its directory.
    pub fn opens_inside_root(&self, path: &Path) -> bool {
        normalize(path).starts_with(self.workspace.root().path())
            || (!self.root_pinned.get() && self.workspace.is_empty())
    }

    /// Opens `path` (loading it if needed) in a tab and selects it. The first document of an
    /// empty workspace moves the project root to its directory (unless that is `/`, or the root
    /// is pinned, see [`WindowState::pin_root`]).
    pub async fn open_file(self: &Rc<Self>, path: &Path) -> Result<Rc<Editor>, String> {
        let doc = self.open_document(path).await?;
        self.show_document(&doc)
            .ok_or_else(|| format!("cannot show {}", path.display()))
    }

    /// [`WindowState::open_file`] without selecting the tab or moving the focus (the first tab
    /// of an empty window is selected all the same). The session's tabs and the command line's
    /// further FILEs open this way: a tab that is not shown is not drawn, so what waits for an
    /// editor's first frame (spell checking, completion's popup) waits until it is selected.
    pub async fn open_file_in_background(
        self: &Rc<Self>,
        path: &Path,
    ) -> Result<Rc<Editor>, String> {
        let doc = self.open_document(path).await?;
        let editor = self
            .tab_for(&doc)
            .ok_or_else(|| format!("cannot show {}", path.display()))?;
        // The first tab: its status (Vim's mode among it) once its hooks are attached.
        if self.is_active(&editor) {
            self.refresh_status();
        }
        Ok(editor)
    }

    /// Loads `path` into the workspace for [`WindowState::open_file`], moving the root first
    /// when it is the first document.
    async fn open_document(self: &Rc<Self>, path: &Path) -> Result<DocRef, String> {
        if !self.root_pinned.get()
            && self.workspace.is_empty()
            && let Some(dir) = normalize(path).parent()
            && dir.parent().is_some()
            && let Ok(root) = lemma_core::ProjectRoot::new(dir)
        {
            let moved = root.path() != self.workspace.root().path();
            self.workspace.set_root(root);
            if moved {
                self.refresh_status();
                for f in self.root_hooks.snapshot() {
                    f(self);
                }
            }
        }
        self.workspace.open(path).await
    }

    /// Shows an open document: selects its tab, creating it first if needed. `None` when the
    /// document is closed (see `Workspace::open`).
    pub fn show_document(self: &Rc<Self>, doc: &DocRef) -> Option<Rc<Editor>> {
        let editor = self.tab_for(doc)?;
        self.select_editor(&editor);
        self.refresh_status();
        Some(editor)
    }

    /// The editor tab of an open document, created (at the end, not selected) if it has none.
    /// `None` when the document is closed.
    fn tab_for(self: &Rc<Self>, doc: &DocRef) -> Option<Rc<Editor>> {
        if let Some(editor) = self.editor_for(doc) {
            return Some(editor);
        }
        let editor = Editor::new(doc, &self.config.editor)?;
        let dark = adw::StyleManager::default().is_dark();
        editor.set_style_scheme(editor::style_scheme(&self.config.editor, dark).as_ref());
        // Forward SyncTeX here too: the PDF's Source and PDF buttons are a pane away. (P is
        // Paste's mnemonic in the text view's own items.)
        let pdf = gio::Menu::new();
        pdf.append(Some("_Show in PDF"), Some("win.sync-forward"));
        editor.extra_menu().append_section(None, &pdf);
        editor.extra_menu().append_section(None, &self.editor_menu);
        // Registered before the page exists: appending the first page selects it right away.
        self.editors.borrow_mut().push(editor.clone());
        let page = self.tab_view.append(editor.widget());
        // The tooltip is Pango markup ('&' in "Smith & Jones" would make it invalid).
        page.set_tooltip(&glib::markup_escape_text(&doc.path().display().to_string()));
        self.update_tab_title(&editor);
        self.connect_editor(&editor);
        // Highlighting first: tags created later (review, diagnostics) take precedence.
        highlight::attach(self, &editor);
        if self.vim_enabled.get() {
            self.enable_vim_on(&editor);
        }
        review_ui::attach_editor(self, &editor);
        diagnostics_panel::attach_editor(self, &editor);
        completion::attach(self, &editor);
        typing::attach(self, &editor);
        keys::attach(self, &editor);
        self.attach_m3(&editor);
        let state = Rc::downgrade(self);
        editor.after_first_frame(move |editor| {
            if let Some(state) = state.upgrade()
                && state.editors.borrow().iter().any(|e| Rc::ptr_eq(e, editor))
            {
                spell::attach_after_first_frame(&state, editor);
            }
        });
        Some(editor)
    }

    /// Every editor tab, in creation order.
    pub fn editors(&self) -> Vec<Rc<Editor>> {
        self.editors.borrow().clone()
    }

    /// The editor of the selected tab.
    pub fn active_editor(&self) -> Option<Rc<Editor>> {
        let page = self.tab_view.selected_page()?;
        self.editor_for_widget(&page.child())
    }

    /// The editor showing `doc`, if any.
    pub fn editor_for(&self, doc: &DocRef) -> Option<Rc<Editor>> {
        self.editors
            .borrow()
            .iter()
            .find(|e| Rc::ptr_eq(e.document(), doc))
            .cloned()
    }

    fn editor_for_widget(&self, widget: &gtk::Widget) -> Option<Rc<Editor>> {
        self.editors
            .borrow()
            .iter()
            .find(|e| e.widget().upcast_ref::<gtk::Widget>() == widget)
            .cloned()
    }

    /// Selects `editor`'s tab and focuses its view (nothing happens once its tab is closed).
    pub fn select_editor(&self, editor: &Editor) {
        let Some(page) = self.page_of(editor) else {
            return;
        };
        self.tab_view.set_selected_page(&page);
        editor.view().grab_focus();
    }

    /// Shows a blank editor area instead of "No Document Open" while no tab is open and
    /// `opening` is set. The app sets it before the window is first shown when it is about to
    /// open documents (a FILE, the last session's file, or the tabs of the project's session),
    /// and clears it once they are open or have failed to, so the first frames do not flash the
    /// empty page.
    pub fn set_opening(&self, opening: bool) {
        self.opening.set(opening);
        self.show_editor_page();
    }

    /// The editor area's page: `tabs`, `opening` (blank, see [`WindowState::set_opening`]) or
    /// `empty` ("No Document Open").
    pub fn editor_page(&self) -> Option<glib::GString> {
        self.editor_stack.visible_child_name()
    }

    /// Shows the tabs; without any, the blank page while documents open and else the empty page.
    fn show_editor_page(&self) {
        let page = if self.tab_view.n_pages() > 0 {
            "tabs"
        } else if self.opening.get() {
            "opening"
        } else {
            "empty"
        };
        self.editor_stack.set_visible_child_name(page);
    }

    /// The tab page showing `editor`; `None` once its tab is closed (an `Editor` can outlive its
    /// tab, e.g. in a running `:wq`, and `TabView::page` must not be asked about it then).
    fn page_of(&self, editor: &Editor) -> Option<adw::TabPage> {
        let widget = editor.widget().upcast_ref::<gtk::Widget>();
        (0..self.tab_view.n_pages())
            .map(|i| self.tab_view.nth_page(i))
            .find(|page| &page.child() == widget)
    }

    fn connect_editor(self: &Rc<Self>, editor: &Rc<Editor>) {
        let buffer = editor.buffer();
        let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
        buffer.connect_cursor_position_notify(move |_| {
            if let (Some(state), Some(ed)) = (state.upgrade(), ed.upgrade())
                && state.is_active(&ed)
            {
                state.refresh_position(&ed);
                state.session_changed();
            }
        });
        if let Some(adjustment) = editor.view().vadjustment() {
            let state = Rc::downgrade(self);
            adjustment.connect_value_changed(move |_| {
                if let Some(state) = state.upgrade() {
                    state.session_changed();
                }
            });
        }
        let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
        buffer.connect_modified_changed(move |_| {
            if let (Some(state), Some(ed)) = (state.upgrade(), ed.upgrade()) {
                state.update_tab_title(&ed);
                if state.is_active(&ed) {
                    state.refresh_status();
                }
                // Saved, reloaded or discarded: there is nothing left to overwrite with.
                if !ed.document().is_dirty() {
                    state.withdraw_overwrite(ed.document());
                }
            }
        });
    }

    fn update_tab_title(&self, editor: &Editor) {
        let Some(page) = self.page_of(editor) else {
            return;
        };
        let doc = editor.document();
        let title = if doc.is_dirty() {
            format!("• {}", doc.display_name())
        } else {
            doc.display_name()
        };
        page.set_title(&title);
    }

    fn is_active(&self, editor: &Rc<Editor>) -> bool {
        self.active_editor().is_some_and(|a| Rc::ptr_eq(&a, editor))
    }

    fn page_detached(self: &Rc<Self>, page: &adw::TabPage) {
        let child = page.child();
        let editor = {
            let mut editors = self.editors.borrow_mut();
            let index = editors
                .iter()
                .position(|e| e.widget().upcast_ref::<gtk::Widget>() == &child);
            index.map(|i| editors.remove(i))
        };
        if let Some(editor) = editor {
            find_bar::detach(self, &editor);
            self.detach_m3(&editor);
            spell::detach(self, &editor);
            keys::detach(self, &editor);
            typing::detach(self, &editor);
            completion::detach(self, &editor);
            review_ui::detach_editor(self, &editor);
            diagnostics_panel::detach_editor(self, &editor);
            highlight::detach(self, &editor);
            editor.disable_vim();
            let doc = editor.document();
            self.workspace.release(doc);
            // A document that left the workspace (its changes discarded with the tab) must not
            // be written later by a stale Overwrite button.
            let kept = self
                .workspace
                .get(doc.path())
                .is_some_and(|d| Rc::ptr_eq(&d, doc));
            if !kept || !doc.is_dirty() {
                self.withdraw_overwrite(doc);
            }
            // Every hook is off the view: dropping the editor frees it (and its buffer).
            editor.tab_closed();
            self.trim_heap_soon();
        }
        self.refresh_status();
    }

    // ----- saving and building -----

    /// Saves the active document.
    pub fn save_active(self: &Rc<Self>) {
        if let Some(editor) = self.active_editor() {
            self.save_document(editor.document());
        }
    }

    /// Saves `doc` on behalf of the user (Ctrl+S, `:w`): errors become toasts and a successful
    /// save starts a build when `build.on_save` is set.
    pub fn save_document(self: &Rc<Self>, doc: &DocRef) {
        self.user_save(doc, false);
    }

    /// [`WindowState::save_document`]; `overwrite` also writes a file another program changed
    /// (`:w!`).
    fn user_save(self: &Rc<Self>, doc: &DocRef, overwrite: bool) {
        let (state, doc) = (self.clone(), doc.clone());
        glib::spawn_future_local(async move {
            let result = if overwrite {
                doc.save_overwriting().await
            } else {
                doc.save().await
            };
            state.after_user_save(&doc, result);
        });
    }

    /// Saves every dirty document on behalf of the user.
    pub fn save_all(self: &Rc<Self>) {
        self.save_all_then(|_| {});
    }

    /// Like [`WindowState::save_all`], then runs `then` if every save succeeded.
    fn save_all_then(self: &Rc<Self>, then: impl FnOnce(&Rc<Self>) + 'static) {
        let state = self.clone();
        let saves = self.workspace.save_all_dirty();
        glib::spawn_future_local(async move {
            match saves.await {
                Ok(()) => {
                    state.after_saves();
                    then(&state);
                }
                Err(failures) => state.report_save_failures(failures),
            }
        });
    }

    fn report_save_failures(&self, failures: Vec<(PathBuf, SaveError)>) {
        for (path, e) in failures {
            self.toast(&format!("Could not save {}: {e}", path.display()));
        }
    }

    /// Saves the documents `docs` on behalf of the user, concurrently, as
    /// [`WindowState::save_document`] saves one, but with one build (`build.on_save`) once they
    /// are all written, if any of them was: a build per document would run latexmk again for
    /// nothing, and its save of the documents still being written would write them twice.
    pub fn save_documents(self: &Rc<Self>, docs: &[DocRef]) {
        let saves: Vec<_> = docs.iter().map(|doc| (doc.clone(), doc.save())).collect();
        let state = self.clone();
        glib::spawn_future_local(async move {
            let mut saved = false;
            for (doc, save) in saves {
                let result = save.await;
                saved |= state.report_user_save(&doc, result);
            }
            if saved {
                state.after_saves();
            }
        });
    }

    fn after_user_save(self: &Rc<Self>, doc: &DocRef, result: Result<(), SaveError>) {
        if self.report_user_save(doc, result) {
            self.after_saves();
        }
    }

    /// Handles the result of a save the user asked for: a refused save offers to overwrite, a
    /// failed one is toasted. Returns whether `doc` was saved.
    fn report_user_save(self: &Rc<Self>, doc: &DocRef, result: Result<(), SaveError>) -> bool {
        match result {
            Ok(()) => {
                self.withdraw_overwrite(doc);
                true
            }
            Err(SaveError::ExternallyModified) => {
                self.offer_overwrite(doc);
                false
            }
            Err(e) => {
                self.toast(&format!("Could not save “{}”: {e}", doc.display_name()));
                false
            }
        }
    }

    fn after_saves(&self) {
        if self.config.build.on_save {
            self.request_build(Reason::Save);
        }
    }

    /// Says that `doc` changed on disk and could not be saved, with an Overwrite button.
    pub fn offer_overwrite(self: &Rc<Self>, doc: &DocRef) {
        self.offer_overwrite_with(doc, None);
    }

    /// Like [`WindowState::offer_overwrite`], for a build whose save of `doc` was refused: once
    /// the file is overwritten, the build is requested again for `reason`.
    pub fn offer_overwrite_and_build(self: &Rc<Self>, doc: &DocRef, reason: Reason) {
        self.offer_overwrite_with(doc, Some(reason));
    }

    /// Shows the Overwrite toast for `doc`, or updates the one already offered for it (which then
    /// also builds once overwritten, when `rebuild` is set): repeated refused saves and builds
    /// leave one toast per document, not a queue of them.
    ///
    /// The toast does not keep the document alive, and is withdrawn when the document is saved,
    /// reloaded or discarded, or leaves the workspace with its tab. Its button writes only the
    /// document the workspace still has for that path, and only while it has unsaved changes.
    fn offer_overwrite_with(self: &Rc<Self>, doc: &DocRef, rebuild: Option<Reason>) {
        let path = doc.path().to_path_buf();
        if let Some(offer) = self.overwrite_offers.borrow().get(&path)
            && std::ptr::eq(offer.doc.as_ptr(), Rc::as_ptr(doc))
        {
            if rebuild.is_some() {
                offer.rebuild.set(rebuild);
            }
            return;
        }
        self.withdraw_overwrite_at(&path);
        let toast = adw::Toast::builder()
            .title(format!(
                "“{}” was changed by another program",
                doc.display_name()
            ))
            .use_markup(false)
            .button_label("_Overwrite")
            .timeout(0)
            .build();
        let rebuild = Rc::new(Cell::new(rebuild));
        let (state, weak_doc, reason) = (Rc::downgrade(self), Rc::downgrade(doc), rebuild.clone());
        toast.connect_button_clicked(move |_| {
            let (Some(state), Some(doc)) = (state.upgrade(), weak_doc.upgrade()) else {
                return;
            };
            let current = state
                .workspace
                .get(doc.path())
                .is_some_and(|d| Rc::ptr_eq(&d, &doc));
            if !current || !doc.is_dirty() {
                tracing::info!(
                    "not overwriting {}: its unsaved changes are gone",
                    doc.path().display()
                );
                return;
            }
            let rebuild = reason.get();
            glib::spawn_future_local(async move {
                let result = doc.save_overwriting().await;
                match (result, rebuild) {
                    (Ok(()), Some(reason)) => state.request_build(reason),
                    (Err(SaveError::ExternallyModified), Some(_)) => {
                        state.offer_overwrite_with(&doc, rebuild);
                    }
                    (result, _) => state.after_user_save(&doc, result),
                }
            });
        });
        let (state, key) = (Rc::downgrade(self), path.clone());
        toast.connect_dismissed(move |toast| {
            if let Some(state) = state.upgrade() {
                let mut offers = state.overwrite_offers.borrow_mut();
                if offers.get(&key).is_some_and(|o| &o.toast == toast) {
                    offers.remove(&key);
                }
            }
        });
        self.overwrite_offers.borrow_mut().insert(
            path,
            OverwriteOffer {
                toast: toast.clone(),
                doc: Rc::downgrade(doc),
                rebuild,
            },
        );
        self.toasts.add_toast(toast);
    }

    /// Withdraws the Overwrite toast offered for `doc`, if any.
    fn withdraw_overwrite(&self, doc: &Document) {
        let offered = self
            .overwrite_offers
            .borrow()
            .get(doc.path())
            .is_some_and(|o| std::ptr::eq(o.doc.as_ptr(), doc));
        if offered {
            self.withdraw_overwrite_at(doc.path());
        }
    }

    /// Withdraws the Overwrite toast offered for the document at `path`, if any.
    fn withdraw_overwrite_at(&self, path: &Path) {
        let offer = self.overwrite_offers.borrow_mut().remove(path);
        // Dismissed outside the borrow: `dismissed` handlers run right away.
        if let Some(offer) = offer {
            offer.toast.dismiss();
        }
    }

    /// Passes a build request to the build controller.
    pub fn request_build(&self, reason: Reason) {
        if let Some(build) = self.build.get() {
            build.request(reason);
        }
    }

    /// Shows a short message (plain text).
    pub fn toast(&self, message: &str) {
        let toast = adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .build();
        self.toasts.add_toast(toast);
    }

    // ----- settings (live reload) -----

    /// Makes `config` the window's configuration (a reload of `config.toml`): applies what the
    /// shell derives from it (accelerators, editor font, style schemes, soft wrap, the scanner's
    /// math environments), then runs the [`WindowState::connect_config_changed`] callbacks with
    /// [`ConfigFile::Config`]. Nothing happens when nothing changed. `editor.vim` applies to new
    /// windows only (the toggle is the user's).
    pub fn set_config(self: &Rc<Self>, config: Rc<Config>) {
        let old = self.config.get();
        if *old == *config {
            return;
        }
        self.config.set(config.clone());
        if old.keys != config.keys {
            crate::app::reapply_accels(&self.app, &old.keys, &config.keys);
        }
        let (was, now) = (&old.editor, &config.editor);
        if was.font != now.font
            && let Some(display) = gtk::gdk::Display::default()
        {
            editor::install_css(&display, now);
        }
        if was.scheme_light != now.scheme_light || was.scheme_dark != now.scheme_dark {
            let scheme = editor::style_scheme(now, adw::StyleManager::default().is_dark());
            for editor in self.editors() {
                editor.set_style_scheme(scheme.as_ref());
            }
        }
        if was.wrap != now.wrap {
            let mode = if now.wrap {
                gtk::WrapMode::WordChar
            } else {
                gtk::WrapMode::None
            };
            for editor in self.editors() {
                editor.view().set_wrap_mode(mode);
            }
        }
        if was.math_envs != now.math_envs {
            self.workspace
                .set_scan_config(settings::scan_config(&config.editor));
        }
        if old.pdf.thumbnails != config.pdf.thumbnails {
            self.pdf_sidebar
                .split
                .set_show_sidebar(config.pdf.thumbnails);
        }
        if old.session.restore && !config.session.restore {
            self.cancel_session_save();
        }
        self.notify_config_changed(ConfigFile::Config);
    }

    /// The snippets: the defaults merged with the user's `snippets.toml`.
    pub fn snippets(&self) -> Rc<SnippetSet> {
        self.snippets
            .borrow_mut()
            .get_or_insert_with(|| Rc::new(SnippetSet::defaults()))
            .clone()
    }

    /// Replaces the snippets (a reload of `snippets.toml`) and runs the config-changed callbacks
    /// with [`ConfigFile::Snippets`].
    pub fn set_snippets(self: &Rc<Self>, snippets: Rc<SnippetSet>) {
        self.snippets.replace(Some(snippets));
        self.notify_config_changed(ConfigFile::Snippets);
    }

    /// The quick-action templates: the defaults merged with the user's `prompts.toml`.
    pub fn prompts(&self) -> Rc<Vec<PromptTemplate>> {
        self.prompts
            .borrow_mut()
            .get_or_insert_with(|| Rc::new(lemma_config::default_prompts()))
            .clone()
    }

    /// Replaces the templates (a reload of `prompts.toml`) and runs the config-changed callbacks
    /// with [`ConfigFile::Prompts`].
    pub fn set_prompts(self: &Rc<Self>, prompts: Rc<Vec<PromptTemplate>>) {
        if self.prompts.borrow().as_ref() == Some(&prompts) {
            return;
        }
        self.prompts.replace(Some(prompts));
        self.notify_config_changed(ConfigFile::Prompts);
    }

    /// Calls `f(state, file)` after `file` was reloaded and its new values are in place
    /// (`state.config`, [`WindowState::snippets`], [`WindowState::prompts`]).
    pub fn connect_config_changed(
        &self,
        f: impl Fn(&Rc<WindowState>, ConfigFile) + 'static,
    ) -> HookId {
        self.config_hooks.add(Rc::new(f))
    }

    /// Calls `f(state)` after the project root moved (the first document opened in an empty
    /// window decides it); `state.workspace.root()` is the new root.
    pub fn connect_root_changed(&self, f: impl Fn(&Rc<WindowState>) + 'static) -> HookId {
        self.root_hooks.add(Rc::new(f))
    }

    /// Removes a callback added by [`WindowState::connect_config_changed`] or
    /// [`WindowState::connect_root_changed`].
    pub fn disconnect(&self, id: HookId) {
        let _ = self.config_hooks.remove(id) || self.root_hooks.remove(id);
    }

    fn notify_config_changed(self: &Rc<Self>, file: ConfigFile) {
        for f in self.config_hooks.snapshot() {
            f(self, file);
        }
    }

    // ----- memory -----

    /// Gives memory back (ARCHITECTURE §7): the PDF view keeps only the textures of the pages on
    /// screen, the PDF thread frees its SyncTeX scanner (and trims its own heap after that), and
    /// the allocator returns its free pages to the system. Runs by itself once the window has been
    /// in the background for [`INACTIVE_TRIM_DELAY`]; hiding the PDF pane trims both PDF caches
    /// too. A PDF in a window of its own that is active (read while this one is in the
    /// background) keeps them.
    pub fn trim_memory(&self) {
        let pdf_in_use = crate::pdf::detach::window(self).is_some_and(|w| w.is_active());
        if let Some(pdf) = self.pdf.get()
            && !pdf_in_use
        {
            pdf.view().trim_cache();
            pdf.trim();
        }
        crate::heap::trim();
    }

    /// Gives the heap's free pages back to the system a moment from now (once for any number of
    /// calls meanwhile): a closed tab frees its document's buffer, which for a large file is
    /// many MiB that glibc would otherwise keep.
    fn trim_heap_soon(self: &Rc<Self>) {
        if self.heap_trim.borrow().is_some() {
            return;
        }
        let state = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(CLOSED_TAB_TRIM_DELAY, move || {
            if let Some(state) = state.upgrade() {
                state.heap_trim.take();
            }
            crate::heap::trim();
        });
        self.heap_trim.replace(Some(timer));
    }

    /// Changes how long the window must stay in the background before
    /// [`WindowState::trim_memory`] runs ([`INACTIVE_TRIM_DELAY`] by default; tests shorten it).
    pub fn set_trim_delay(&self, delay: Duration) {
        self.trim_delay.set(delay);
    }

    /// The window became active (the timer stops) or inactive (it starts).
    fn activity_changed(self: &Rc<Self>, active: bool) {
        if let Some(timer) = self.trim_timer.take() {
            timer.remove();
        }
        if active {
            return;
        }
        let state = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(self.trim_delay.get(), move || {
            if let Some(state) = state.upgrade() {
                state.trim_timer.take();
                tracing::debug!("the window is in the background; giving memory back");
                state.trim_memory();
            }
        });
        self.trim_timer.replace(Some(timer));
    }

    // ----- desktop integration -----

    /// True when the window has the keyboard focus (or what [`WindowState::set_focus_override`]
    /// says, in tests).
    pub fn window_focused(&self) -> bool {
        self.focus_override
            .get()
            .unwrap_or_else(|| self.window.is_active())
    }

    /// Makes [`WindowState::window_focused`] report `focused` (`None` = the real focus). For
    /// tests of focus-dependent behaviour (the turn-end notification).
    pub fn set_focus_override(&self, focused: Option<bool>) {
        self.focus_override.set(focused);
    }

    /// Sends a desktop notification with the id `id` (a new one replaces the previous one with
    /// the same id). Runs that quit by themselves and offscreen (Broadway) runs never send one to
    /// the desktop; a hook set with [`WindowState::set_notification_hook`] gets it instead.
    pub fn send_notification(&self, id: &str, notification: &gio::Notification) {
        if let Some(hook) = self.notification_hook.borrow().as_ref() {
            hook(id, notification);
            return;
        }
        if self.scripted || offscreen() {
            tracing::info!("not sending notification {id:?} from an offscreen or scripted run");
            return;
        }
        self.app.send_notification(Some(id), notification);
    }

    /// Routes [`WindowState::send_notification`] to `hook` (tests); `None` restores the desktop.
    pub fn set_notification_hook(&self, hook: Option<Box<NotificationHook>>) {
        self.notification_hook.replace(hook);
    }

    // ----- Vim -----

    /// True while Vim emulation is on.
    pub fn vim_enabled(&self) -> bool {
        self.vim_enabled.get()
    }

    /// Turns Vim emulation on or off in every editor (and updates the `win.toggle-vim` state).
    pub fn set_vim_enabled(self: &Rc<Self>, on: bool) {
        self.vim_enabled.set(on);
        if let Some(action) = self
            .window
            .lookup_action("toggle-vim")
            .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        {
            action.set_state(&on.into());
        }
        for editor in self.editors() {
            if on {
                self.enable_vim_on(&editor);
            } else {
                editor.disable_vim();
            }
        }
        self.refresh_vim_status();
        self.refresh_editor_actions();
    }

    fn enable_vim_on(self: &Rc<Self>, editor: &Rc<Editor>) {
        let Some(im) = editor.enable_vim() else {
            return;
        };
        for property in ["command-bar-text", "command-text"] {
            let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
            im.connect_notify_local(Some(property), move |_, _| {
                if let (Some(state), Some(ed)) = (state.upgrade(), ed.upgrade())
                    && state.is_active(&ed)
                {
                    state.refresh_vim_status();
                }
            });
        }
        let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
        im.connect_write(move |_, _, path| {
            if let (Some(state), Some(ed)) = (state.upgrade(), ed.upgrade()) {
                state.vim_write(&ed, path, false);
            }
        });
        let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
        im.connect_edit(move |_, _, path| {
            if let (Some(state), Some(ed)) = (state.upgrade(), ed.upgrade()) {
                state.vim_edit(&ed, path);
            }
        });
        let (state, ed) = (Rc::downgrade(self), Rc::downgrade(editor));
        im.connect_execute_command(move |_, command| match (state.upgrade(), ed.upgrade()) {
            (Some(state), Some(ed)) => state.vim_execute(&ed, command),
            _ => false,
        });
    }

    /// VimIMContext's `write` signal (`:w [path]`), and `:w! [path]` (`overwrite`, which also
    /// writes a file another program changed, or a `path` that exists). A `path` that is not
    /// the document's own gets a copy of the text (`file_commands::write_copy`).
    fn vim_write(self: &Rc<Self>, editor: &Rc<Editor>, path: Option<&str>, overwrite: bool) {
        if let Some(why) = path.and_then(vim::unsupported_file_name) {
            self.toast(why);
            return;
        }
        let doc = editor.document();
        let Some(target) = self.other_file(doc, path) else {
            self.user_save(doc, overwrite);
            return;
        };
        let (state, doc) = (self.clone(), doc.clone());
        glib::spawn_future_local(async move {
            if let Err(e) = file_commands::write_copy(&state, &doc, &target, overwrite).await {
                state.toast(&e);
            }
        });
    }

    /// The file an ex command's `path` names (relative to the project root), when it is not
    /// `doc`'s own.
    fn other_file(&self, doc: &Document, path: Option<&str>) -> Option<PathBuf> {
        let path = path.filter(|p| !p.is_empty())?;
        let target = normalize(&self.workspace.root().path().join(path));
        (target != doc.path()).then_some(target)
    }

    /// VimIMContext's `edit` signal: `:e path` opens `path`, relative to the project root; `:e`
    /// alone reloads the file from disk unless it has unsaved changes (`:e!` discards them, see
    /// [`WindowState::revert`]).
    fn vim_edit(self: &Rc<Self>, editor: &Rc<Editor>, path: Option<&str>) {
        let Some(path) = path.filter(|p| !p.is_empty()) else {
            if editor.document().is_dirty() {
                self.toast("No write since last change (add ! to discard the changes)");
            } else {
                self.revert(editor.document());
            }
            return;
        };
        let path = self.workspace.root().path().join(path);
        let state = self.clone();
        glib::spawn_future_local(async move {
            if let Err(e) = state.open_file(&path).await {
                state.toast(&e);
            }
        });
    }

    /// Replaces `doc`'s text with the file on disk (`:e!`), as one undoable change; unsaved
    /// changes are lost.
    pub fn revert(self: &Rc<Self>, doc: &DocRef) {
        let (state, doc) = (Rc::downgrade(self), doc.clone());
        glib::spawn_future_local(async move {
            if let Err(e) = doc.reload().await
                && let Some(state) = state.upgrade()
            {
                state.toast(&e);
            }
        });
    }

    /// VimIMContext's `execute-command` signal, which sees every ex command before Vim runs it.
    /// The review controller gets the first look; then the file and tab commands GtkSourceView
    /// does not implement (`:wq`, `:wa`, `:sav`, `:q`, `:qa`, …) are handled here. Returns true
    /// when the command was handled.
    ///
    /// Runs inside the Vim context's key handling: nothing here may close the editor
    /// synchronously.
    fn vim_execute(self: &Rc<Self>, editor: &Rc<Editor>, command: &str) -> bool {
        if review_ui::ex_command(self, editor, command) || keys::ex_command(self, editor, command) {
            return true;
        }
        // GtkSourceView's join would never return (see `Vim::blocks_key`).
        if vim::is_join_command(command) {
            let buffer = editor.buffer();
            let line = buffer.iter_at_mark(&buffer.get_insert()).line();
            if vim::join_would_hang(buffer, line) {
                editor.view().error_bell();
                return true;
            }
        }
        // GtkSourceView's `:s` scans the buffer again for every match it replaces.
        if vim::substitute::execute(editor, command) {
            return true;
        }
        let Some(command) = parse_file_command(command) else {
            return false;
        };
        match command {
            // Vim emits `write` for these (see `vim_write`).
            FileCommand::Write {
                quit: false,
                force: false,
                ..
            } => false,
            // Vim does not know `:w!`.
            FileCommand::Write {
                path,
                quit: false,
                force: true,
            } => {
                self.vim_write(editor, path.as_deref(), true);
                true
            }
            FileCommand::Write {
                path,
                quit: true,
                force,
            } => {
                if let Some(why) = path.as_deref().and_then(vim::unsupported_file_name) {
                    self.toast(why);
                    return true;
                }
                // `:wq path` writes a copy, then closes as `:q` does (`:wq!` as `:q!`).
                let copy = self.other_file(editor.document(), path.as_deref());
                let discard = copy.is_some() && force;
                // The editor may close (and go) while the save runs.
                let (state, doc, editor) = (
                    self.clone(),
                    editor.document().clone(),
                    Rc::downgrade(editor),
                );
                glib::spawn_future_local(async move {
                    let written = match copy {
                        Some(target) => {
                            let result =
                                file_commands::write_copy(&state, &doc, &target, force).await;
                            if let Err(e) = &result {
                                state.toast(e);
                            }
                            result.is_ok()
                        }
                        None => {
                            let result = if force {
                                doc.save_overwriting().await
                            } else {
                                doc.save().await
                            };
                            let saved = result.is_ok();
                            state.after_user_save(&doc, result);
                            saved
                        }
                    };
                    if written && let Some(editor) = editor.upgrade() {
                        state.close_editor(&editor, discard);
                    }
                });
                true
            }
            FileCommand::SaveAs { path: None, .. } => {
                self.toast("Argument required: :saveas needs a file name");
                true
            }
            FileCommand::SaveAs {
                path: Some(path),
                force,
            } => {
                if let Some(why) = vim::unsupported_file_name(&path) {
                    self.toast(why);
                    return true;
                }
                let path = self.workspace.root().path().join(path);
                let (state, editor) = (self.clone(), editor.clone());
                // The old tab closes once the file is written, long after this key.
                glib::spawn_future_local(async move {
                    if let Err(e) = file_commands::save_as(&state, &editor, &path, force).await {
                        state.toast(&e);
                    }
                });
                true
            }
            // Vim emits `edit` for these (see `vim_edit`).
            FileCommand::Edit { force: false, .. } => false,
            // Vim does not know `:e!`: it reloads the file, discarding unsaved changes.
            FileCommand::Edit {
                path: None,
                force: true,
            } => {
                self.revert(editor.document());
                true
            }
            FileCommand::Edit {
                path: Some(path),
                force: true,
            } => {
                self.vim_edit(editor, Some(&path));
                true
            }
            FileCommand::WriteAll { quit } => {
                self.save_all_then(move |state| {
                    if quit {
                        state.window.close();
                    }
                });
                true
            }
            // Closing runs from an idle callback: dropping the editor detaches its Vim context,
            // which must not happen while that context is still dispatching this key.
            FileCommand::Quit { force } => {
                let (state, editor) = (Rc::downgrade(self), Rc::downgrade(editor));
                glib::idle_add_local_once(move || {
                    if let (Some(state), Some(editor)) = (state.upgrade(), editor.upgrade()) {
                        state.close_editor(&editor, force);
                    }
                });
                true
            }
            FileCommand::QuitAll { force } => {
                let state = Rc::downgrade(self);
                glib::idle_add_local_once(move || {
                    if let Some(state) = state.upgrade() {
                        if force {
                            // `:qa!` throws the unsaved changes away, their copies too.
                            for doc in state.workspace.dirty_documents() {
                                crate::recovery::discard(&state, &doc);
                            }
                            state.close_confirmed.set(true);
                        }
                        state.window.close();
                    }
                });
                true
            }
        }
    }

    /// Closes `editor`'s tab: asks about unsaved changes, unless `discard` is set. Nothing
    /// happens once its tab is closed.
    ///
    /// Discarding the changes of a document under review first reloads it from disk (see
    /// `WindowState::discard_changes`), so the tab closes a moment later.
    pub fn close_editor(self: &Rc<Self>, editor: &Editor, discard: bool) {
        let Some(page) = self.page_of(editor) else {
            return;
        };
        let doc = editor.document();
        if discard && doc.review.borrow().is_some() && doc.is_dirty() {
            let Some(editor) = self.editor_for_widget(editor.widget().upcast_ref()) else {
                return;
            };
            let state = self.clone();
            glib::spawn_future_local(async move {
                state.discard_changes(editor.document()).await;
                editor.buffer().set_modified(false);
                if let Some(page) = state.page_of(&editor) {
                    state.tab_view.close_page(&page);
                }
            });
            return;
        }
        if discard {
            editor.buffer().set_modified(false);
        }
        self.tab_view.close_page(&page);
    }

    /// Moves `new`'s tab to where `old`'s is, selects it and closes `old`, discarding its unsaved
    /// changes (Save As: `new` shows the file `old` was saved as). With `old`'s tab closed
    /// already, `new` is only selected.
    pub fn replace_tab(self: &Rc<Self>, old: &Editor, new: &Editor) {
        if let (Some(old_page), Some(new_page)) = (self.page_of(old), self.page_of(new)) {
            let at = self.tab_view.page_position(&old_page);
            self.tab_view.reorder_page(&new_page, at);
        }
        self.select_editor(new);
        self.close_editor(old, true);
    }

    /// Throws away the unsaved changes of `doc`, whose tab is about to close. A document under
    /// review stays in the workspace as a closed document (with the buffer's text) after its tab
    /// closes, and every reject, agent edit or build saves it; so its text goes back to the file
    /// on disk first. If the file cannot be read, the review is dropped instead (the document
    /// then leaves the workspace with its tab). Other documents need nothing: they leave.
    async fn discard_changes(&self, doc: &DocRef) {
        if doc.review.borrow().is_none() || !doc.is_dirty() {
            return;
        }
        match doc.reload().await {
            Ok(()) => {
                review_ui::recompute(doc);
            }
            Err(e) => {
                self.toast(&format!(
                    "{e}. The review of “{}” was dropped.",
                    doc.display_name()
                ));
                review_ui::drop_review(doc);
            }
        }
    }

    // ----- status -----

    /// Updates the window title and every status-bar field from the active editor.
    pub fn refresh_status(&self) {
        let Some(editor) = self.active_editor() else {
            self.header.title.set_title("Lemma");
            self.header.title.set_subtitle(&self.root_display());
            self.window.set_title(Some("Lemma"));
            self.status.file.set_text("");
            self.status.position.set_text("");
            self.refresh_vim_status();
            return;
        };
        let doc = editor.document();
        let dirty = if doc.is_dirty() { " •" } else { "" };
        let root = self.workspace.root();
        let relative = root
            .relative(doc.path())
            .unwrap_or_else(|| doc.path())
            .display()
            .to_string();
        drop(root);
        self.status.file.set_text(&format!("{relative}{dirty}"));
        let name = doc.display_name();
        self.header.title.set_title(&format!("{name}{dirty}"));
        self.header.title.set_subtitle(&self.root_display());
        self.window.set_title(Some(&format!("{name} – Lemma")));
        self.refresh_position(&editor);
        self.refresh_vim_status();
    }

    fn refresh_position(&self, editor: &Editor) {
        let (line, col) = editor.cursor_position();
        self.status
            .position
            .set_text(&format!("Ln {line}, Col {col}"));
    }

    fn refresh_vim_status(&self) {
        let status = self.active_editor().and_then(|e| e.vim_status());
        let mode = &self.status.mode;
        mode.remove_css_class("insert");
        mode.remove_css_class("visual");
        match status {
            Some(status) => {
                mode.set_visible(true);
                mode.set_text(status.mode.label());
                match status.mode {
                    VimMode::Insert | VimMode::Replace => mode.add_css_class("insert"),
                    VimMode::Visual | VimMode::VisualLine | VimMode::VisualBlock => {
                        mode.add_css_class("visual");
                    }
                    VimMode::Normal | VimMode::CommandLine => {}
                }
                self.status.vim_command.set_text(&status.command);
            }
            None => {
                mode.set_visible(false);
                self.status.vim_command.set_text("");
            }
        }
    }

    /// The project root for display, with the home directory shown as `~`.
    fn root_display(&self) -> String {
        let root = self.workspace.root();
        let path = root.path();
        let home = self.paths.as_ref().map(|p| p.home.as_path());
        match home.and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }

    // ----- actions and signals -----

    fn install_actions(self: &Rc<Self>) {
        let window = &self.window;
        let simple = |name: &str, run: fn(&Rc<WindowState>)| {
            let action = gio::SimpleAction::new(name, None);
            let state = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(state) = state.upgrade() {
                    run(&state);
                }
            });
            window.add_action(&action);
        };
        simple("save", |s| s.save_active());
        simple("save-all", |s| s.save_all());
        simple("new-file", file_commands::new_file_dialog);
        simple("save-as", file_commands::save_as_dialog);
        simple("close-tab", |s| {
            if let Some(page) = s.tab_view.selected_page() {
                s.tab_view.close_page(&page);
            }
        });
        simple("build", |s| s.request_build(Reason::Manual));

        let vim = gio::SimpleAction::new_stateful("toggle-vim", None, &self.vim_enabled().into());
        let state = Rc::downgrade(self);
        vim.connect_activate(move |_, _| {
            if let Some(state) = state.upgrade() {
                state.set_vim_enabled(!state.vim_enabled());
            }
        });
        window.add_action(&vim);

        window.add_action(&gio::PropertyAction::new(
            "toggle-sidebar",
            &self.sidebar.split,
            "show-sidebar",
        ));
        window.add_action(&gio::PropertyAction::new(
            "toggle-pdf",
            &self.pdf_pane,
            "visible",
        ));
        window.add_action(&gio::PropertyAction::new(
            "toggle-agent",
            &self.agent.split,
            "show-sidebar",
        ));
    }

    /// The M3 actions (WP3.0). Each runs its module's function, so the modules fill them in
    /// without touching the shell; their default accelerators are in `app::DEFAULT_ACCELS`.
    fn install_m3_actions(self: &Rc<Self>) {
        let window = &self.window;
        let simple = |name: &str, run: fn(&Rc<WindowState>)| {
            let action = gio::SimpleAction::new(name, None);
            let state = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(state) = state.upgrade() {
                    run(&state);
                }
            });
            window.add_action(&action);
        };
        simple("command-palette", palette::show);
        simple("quick-open", quick_open::show);
        simple("project-search", search_panel::show);
        simple("shortcuts", shortcuts::show);
        simple("preferences", prefs::show);
        simple("pdf-search", pdf::search::show);
        simple("goto-definition", |s| {
            goto::goto_definition(s);
        });
        simple("goto-back", |s| {
            goto::back(s);
        });
        simple("goto-forward", |s| {
            goto::forward(s);
        });
        simple("show-hover", |s| {
            hover::show_at_cursor(s);
        });
        simple("find-references", labels::find_references);
        simple("rename-label", labels::rename_label);
        simple("jump-pair", pairs::jump);
        simple("history", history_ui::show);
        simple("word-count", wordcount::show);
        // Find and replace in the current file.
        simple("find", find_bar::show);
        simple("find-replace", find_bar::show_replace);
        simple("find-next", |s| find_bar::step(s, true));
        simple("find-prev", |s| find_bar::step(s, false));
        simple("goto-line", quick_open::show_goto_line);

        let detach = gio::SimpleAction::new_stateful("pdf-detach", None, &false.into());
        let state = Rc::downgrade(self);
        detach.connect_activate(move |action, _| {
            if let Some(state) = state.upgrade() {
                let wanted = !action
                    .state()
                    .and_then(|v| v.get::<bool>())
                    .unwrap_or(false);
                let detached = pdf::detach::set_detached(&state, wanted);
                action.set_state(&detached.into());
            }
        });
        window.add_action(&detach);
        window.add_action(&gio::PropertyAction::new(
            "toggle-pdf-sidebar",
            &self.pdf_sidebar.split,
            "show-sidebar",
        ));
        // Ctrl+F searches the PDF only while it has the focus, and finds in the file only while
        // an editor or the find bar has it and Vim is off; elsewhere the key stays the focused
        // widget's (Vim's page down). Ctrl+G (Go to Line) likewise, so the PDF's search entry
        // keeps it (next match) and Vim too.
        self.set_action_enabled(PDF_SEARCH_ACTION, false);
        self.refresh_editor_actions();
        let state = Rc::downgrade(self);
        window.connect_focus_widget_notify(move |window| {
            if let Some(state) = state.upgrade() {
                state.focus_moved(gtk::prelude::GtkWindowExt::focus(window).as_ref());
            }
        });
    }

    /// Enables or disables the window action `name`.
    fn set_action_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self
            .window
            .lookup_action(name)
            .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        {
            action.set_enabled(enabled);
        }
    }

    /// The keyboard focus moved to `focus`: `win.pdf-search` is enabled while it is inside the
    /// PDF pane, the find actions and Go to Line while it is in an editor or the find bar (and
    /// Vim is off, see [`WindowState::refresh_editor_actions`]). Focus moving into a popover or
    /// a dialog (the command palette) leaves them as they were, so the palette sees the state
    /// of the window behind it.
    fn focus_moved(&self, focus: Option<&gtk::Widget>) {
        let Some(focus) = focus else {
            return;
        };
        let transient = focus.ancestor(gtk::Popover::static_type()).is_some()
            || focus.ancestor(adw::Dialog::static_type()).is_some();
        if transient {
            return;
        }
        let in_pdf =
            focus == self.pdf_pane.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&self.pdf_pane);
        self.set_action_enabled(PDF_SEARCH_ACTION, in_pdf);
        self.editor_focused
            .set(focus.is_ancestor(&self.tab_view) || focus.is_ancestor(&self.find_slot));
        self.refresh_editor_actions();
    }

    /// Enables the find actions and Go to Line while an editor or the find bar has the focus
    /// and Vim is off: with Vim on, Ctrl+F, Ctrl+H and Ctrl+G are Vim's own keys (Vim searches
    /// with `/` and goes to a line with `:12`).
    fn refresh_editor_actions(&self) {
        let enabled = self.editor_focused.get() && !self.vim_enabled();
        for name in EDITOR_ACTIONS {
            self.set_action_enabled(name, enabled);
        }
    }

    // ----- session (M3) -----

    /// Connects the changes the session records (see `session.rs`) to
    /// [`WindowState::session_changed`]: tabs, panes, sizes, the sidebar page and the PDF view.
    /// Cursor moves and scrolling are connected per editor.
    fn connect_session_triggers(self: &Rc<Self>) {
        let changed = |state: &Weak<WindowState>| {
            if let Some(state) = state.upgrade() {
                state.session_changed();
            }
        };
        let state = Rc::downgrade(self);
        self.tab_view
            .connect_selected_page_notify(move |_| changed(&state));
        let state = Rc::downgrade(self);
        self.tab_view
            .connect_page_attached(move |_, _, _| changed(&state));
        let state = Rc::downgrade(self);
        self.tab_view
            .connect_page_detached(move |_, _, _| changed(&state));
        let state = Rc::downgrade(self);
        self.tab_view
            .connect_page_reordered(move |_, _, _| changed(&state));
        for split in [
            &self.sidebar.split,
            &self.agent.split,
            &self.pdf_sidebar.split,
        ] {
            let state = Rc::downgrade(self);
            split.connect_show_sidebar_notify(move |_| changed(&state));
        }
        let state = Rc::downgrade(self);
        self.sidebar
            .stack
            .connect_visible_child_name_notify(move |_| changed(&state));
        let state = Rc::downgrade(self);
        self.paned.connect_position_notify(move |_| changed(&state));
        let state = Rc::downgrade(self);
        self.pdf_pane
            .connect_visible_notify(move |_| changed(&state));
        if let Some(pdf) = self.pdf.get() {
            let state = Rc::downgrade(self);
            pdf.view().connect_changed(move |_| changed(&state));
        }
    }

    /// True when the session is saved: windows with `paths` that do not quit by themselves,
    /// while `[session] restore` is on ([`WindowState::set_session_autosave`] overrides the
    /// first part, for tests).
    pub fn session_saves_enabled(&self) -> bool {
        let wanted = self
            .session
            .autosave
            .get()
            .unwrap_or(!self.scripted && self.paths.is_some());
        wanted && self.config.session.restore
    }

    /// Switches the session saves on or off regardless of the window (`None`: the default, see
    /// [`WindowState::session_saves_enabled`]). For tests, with temporary XDG directories.
    pub fn set_session_autosave(&self, on: Option<bool>) {
        self.session.autosave.set(on);
        if on == Some(false) {
            self.cancel_session_save();
        }
    }

    /// Changes how long after the last change the session is saved ([`SESSION_DEBOUNCE`]).
    pub fn set_session_debounce(&self, delay: Duration) {
        self.session.debounce.set(delay);
    }

    /// Something the session records changed: `session::save` runs once things have been quiet
    /// for [`SESSION_DEBOUNCE`] (nothing happens while saves are off). Modules with session
    /// state of their own call it too.
    pub fn session_changed(self: &Rc<Self>) {
        if !self.session_saves_enabled() {
            return;
        }
        self.cancel_session_save();
        let state = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(self.session.debounce.get(), move || {
            if let Some(state) = state.upgrade() {
                state.session_timer.take();
                session::save(&state);
            }
        });
        self.session_timer.replace(Some(timer));
    }

    /// True while a debounced session save is waiting.
    pub fn session_save_pending(&self) -> bool {
        self.session_timer.borrow().is_some()
    }

    /// Saves the session now (when saves are on), instead of waiting for the debounce: the
    /// window is closing.
    pub fn save_session_now(self: &Rc<Self>) {
        self.cancel_session_save();
        if self.session_saves_enabled() {
            session::save(self);
        }
    }

    fn cancel_session_save(&self) {
        if let Some(timer) = self.session_timer.take() {
            timer.remove();
        }
    }

    // ----- the configuration file (M3) -----

    /// Tells the window which settings files the app reads (the app does, right after creating
    /// it; tests may point it at temporary ones).
    pub fn set_settings_files(&self, files: Option<SettingsFiles>) {
        self.settings_files.replace(files);
    }

    /// The settings files the app reads, if any.
    pub fn settings_files(&self) -> Option<SettingsFiles> {
        self.settings_files.borrow().clone()
    }

    /// The `config.toml` the app reads (`LEMMA_CONFIG`, else `~/.config/lemma/config.toml`);
    /// the "Open config file" button of Preferences opens it.
    pub fn config_file(&self) -> Option<PathBuf> {
        self.settings_files
            .borrow()
            .as_ref()
            .map(|f| f.config.clone())
    }

    /// Changes `config.toml` through `edit` (old text → new text, e.g. `|text|
    /// lemma_config::set_value(text, "pdf", "thumbnails", ConfigValue::Bool(true))`), the way
    /// Preferences does: the new text must be a valid configuration; a `config.toml` that is a
    /// symbolic link (a dotfiles repository) is written where it points; a missing file is
    /// created; the write is atomic. The new configuration applies to this window at once
    /// ([`WindowState::set_config`]); the live reload then finds nothing new. Nothing is written
    /// when the configuration is not a regular file (`LEMMA_CONFIG=/dev/null`) or there is none.
    pub fn edit_config_file(
        self: &Rc<Self>,
        edit: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<(), String> {
        let path = self
            .config_file()
            .ok_or("There is no configuration file (no home directory)")?;
        let target = writable_config_target(&path)?;
        let old = match std::fs::read_to_string(&target) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("Could not read {}: {e}", path.display())),
        };
        let new = edit(&old)?;
        let config = lemma_config::parse(&new).map_err(|e| settings::error_message(&path, &e))?;
        if new != old {
            write_atomically(&target, &new)
                .map_err(|e| format!("Could not write {}: {e}", target.display()))?;
        }
        self.set_config(Rc::new(config));
        Ok(())
    }

    fn connect_signals(self: &Rc<Self>) {
        let state = Rc::downgrade(self);
        self.tab_view.connect_selected_page_notify(move |_| {
            if let Some(state) = state.upgrade() {
                state.refresh_status();
                if let Some(editor) = state.active_editor() {
                    editor.view().grab_focus();
                }
            }
        });
        let state = Rc::downgrade(self);
        self.tab_view.connect_n_pages_notify(move |_| {
            if let Some(state) = state.upgrade() {
                state.show_editor_page();
            }
        });
        let state = Rc::downgrade(self);
        self.tab_view.connect_page_detached(move |_, page, _| {
            if let Some(state) = state.upgrade() {
                state.page_detached(page);
            }
        });
        let state = Rc::downgrade(self);
        self.tab_view.connect_close_page(move |view, page| {
            let Some(state) = state.upgrade() else {
                return glib::Propagation::Proceed;
            };
            state.close_page(view, page)
        });

        let style = adw::StyleManager::default();
        let state = Rc::downgrade(self);
        let handler = style.connect_dark_notify(move |style| {
            if let Some(state) = state.upgrade() {
                let scheme = editor::style_scheme(&state.config.editor, style.is_dark());
                for editor in state.editors() {
                    editor.set_style_scheme(scheme.as_ref());
                }
            }
        });
        let handler = RefCell::new(Some(handler));
        let state = Rc::downgrade(self);
        self.window.connect_destroy(move |_| {
            if let Some(handler) = handler.take() {
                adw::StyleManager::default().disconnect(handler);
            }
            if let Some(state) = state.upgrade() {
                for timer in [
                    state.trim_timer.take(),
                    state.heap_trim.take(),
                    state.session_timer.take(),
                ]
                .into_iter()
                .flatten()
                {
                    timer.remove();
                }
            }
        });
        let state = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            if let Some(state) = state.upgrade() {
                state.activity_changed(window.is_active());
            }
        });

        let state = Rc::downgrade(self);
        self.window
            .connect_close_request(move |_| match state.upgrade() {
                Some(state) => state.close_request(),
                None => glib::Propagation::Proceed,
            });
    }

    fn close_page(self: &Rc<Self>, view: &adw::TabView, page: &adw::TabPage) -> glib::Propagation {
        let Some(editor) = self.editor_for_widget(&page.child()) else {
            return glib::Propagation::Proceed;
        };
        let doc = editor.document().clone();
        if self.scripted || !doc.is_dirty() {
            return glib::Propagation::Proceed;
        }
        let (state, view, page) = (self.clone(), view.clone(), page.clone());
        glib::spawn_future_local(async move {
            let confirm = match state.ask_save(std::slice::from_ref(&doc)).await {
                SaveChoice::Cancel => false,
                SaveChoice::Discard => {
                    crate::recovery::discard(&state, &doc);
                    state.discard_changes(&doc).await;
                    true
                }
                SaveChoice::Save => match doc.save().await {
                    Ok(()) => true,
                    Err(e) => {
                        state.toast(&format!("Could not save “{}”: {e}", doc.display_name()));
                        false
                    }
                },
            };
            view.close_page_finish(&page, confirm);
        });
        glib::Propagation::Stop
    }

    /// Closes the window without asking about unsaved changes: the system is ending the process
    /// (`SIGTERM`, `SIGHUP`, Ctrl+C), and the changes are kept in their recovery files, which the
    /// caller writes first (`recovery::write_now`).
    pub fn close_without_asking(self: &Rc<Self>) {
        self.close_confirmed.set(true);
        self.window.close();
    }

    fn close_request(self: &Rc<Self>) -> glib::Propagation {
        // The session as the user left it: every tab is still there.
        self.save_session_now();
        if self.scripted || self.close_confirmed.get() {
            return glib::Propagation::Proceed;
        }
        let dirty = self.workspace.dirty_documents();
        if dirty.is_empty() {
            return glib::Propagation::Proceed;
        }
        let state = self.clone();
        glib::spawn_future_local(async move {
            let close = match state.ask_save(&dirty).await {
                SaveChoice::Cancel => false,
                SaveChoice::Discard => {
                    for doc in &dirty {
                        crate::recovery::discard(&state, doc);
                    }
                    true
                }
                SaveChoice::Save => match state.workspace.save_all_dirty().await {
                    Ok(()) => true,
                    Err(failures) => {
                        state.report_save_failures(failures);
                        false
                    }
                },
            };
            if close {
                state.close_confirmed.set(true);
                state.window.close();
            }
        });
        glib::Propagation::Stop
    }

    async fn ask_save(&self, docs: &[DocRef]) -> SaveChoice {
        let body = match docs {
            [doc] => format!(
                "“{}” has unsaved changes. Unsaved changes are lost if you discard them.",
                doc.display_name()
            ),
            _ => {
                let names: Vec<String> = docs.iter().map(|d| d.display_name()).collect();
                format!(
                    "{} documents have unsaved changes: {}.",
                    docs.len(),
                    names.join(", ")
                )
            }
        };
        let dialog = adw::AlertDialog::new(Some("Save Changes?"), Some(&body));
        dialog.add_responses(&[
            ("cancel", "_Cancel"),
            ("discard", "_Discard"),
            ("save", "_Save"),
        ]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        match dialog.choose_future(Some(&self.window)).await.as_str() {
            "save" => SaveChoice::Save,
            "discard" => SaveChoice::Discard,
            _ => SaveChoice::Cancel,
        }
    }
}

/// The action enabled only while the PDF pane has the focus.
const PDF_SEARCH_ACTION: &str = "pdf-search";

/// The actions of the find bar and Go to Line, enabled only while an editor or the find bar has
/// the focus and Vim is off (Ctrl+F is also `win.pdf-search`'s; Ctrl+G is the next match in the
/// PDF's search entry).
const EDITOR_ACTIONS: [&str; 5] = [
    "find",
    "find-replace",
    "find-next",
    "find-prev",
    "goto-line",
];

/// Where an edit of the configuration file `path` is written: `path`, or what it points to when
/// it is a symbolic link. Refuses a path that exists and is not a regular file (`/dev/null`).
fn writable_config_target(path: &Path) -> Result<PathBuf, String> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => lemma_core::canonicalize(path).map_err(|e| {
            format!(
                "{} is a symbolic link that cannot be followed: {e}",
                path.display()
            )
        })?,
        Ok(_) | Err(_) => path.to_path_buf(),
    };
    match std::fs::metadata(&target) {
        Ok(m) if !m.is_file() => Err(format!(
            "The configuration is read from {}, which is not a file; nothing was written",
            path.display()
        )),
        _ => Ok(target),
    }
}

/// Writes `text` to `path` through a temporary file in the same directory and a rename, so a
/// reader (the live reload) never sees half a file; keeps the file's permissions.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map_or_else(
        || "config.toml".into(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = dir.join(format!(".{name}.lemma-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// True when `LEMMA_WINDOW_SIZE` sets the size of new windows (a testing aid): the app then
/// leaves the size the last session remembered alone (`session::keep_window_size`).
pub fn size_overridden() -> bool {
    layout::size_from_env().is_some()
}

/// True when GTK draws offscreen (Broadway, as every test and scripted snapshot does).
fn offscreen() -> bool {
    gtk::gdk::Display::default().is_some_and(|d| d.type_().name() == "GdkBroadwayDisplay")
}
