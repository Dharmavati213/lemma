//! The `adw::Application`: settings (and their live reload), application actions and
//! accelerators, windows, and the start-up sequence (open the FILE argument, or without one the
//! project and file of the last session, restore the session, then the scripted steps of
//! [`crate::testmode`]).
//!
//! It also holds the registry of what the actions are called ([`ACTIONS`], [`action_info`],
//! [`describe_action`]) and of the keys that are not accelerators ([`KEY_LAYER_SHORTCUTS`]):
//! the command palette and the shortcuts window are generated from them and from the
//! accelerators ([`default_accels`], `accels_for_action` after `[keys]` overrides).

use crate::cli::Options;
use crate::settings::{self, ConfigFile, Reloaded, Settings, SettingsFiles};
use crate::window::WindowState;
use crate::workspace::{Workspace, normalize};
use crate::{APP_ID, editor, perf, review_ui, session, testmode};
use adw::prelude::*;
use lemma_config::{KeysCfg, Paths, Renderer};
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Default accelerators; `[keys]` in config.toml overrides them per action.
///
/// The M3 ones keep clear of Vim (ARCHITECTURE §11: with Vim on, Ctrl+W and Ctrl+O are Vim's in
/// an editor) and of the key layer: `win.pdf-search`'s Ctrl+F is enabled only while the PDF has
/// the focus, so in an editor it stays Vim's page down; hover and goto are `K` and `gd` in Vim
/// normal mode (`[vim.normal]`), and Ctrl+O there goes back after a goto. Ctrl+P (quick open)
/// takes Vim's rarely used Ctrl+P (line up; insert-mode completion), as ctrlp.vim does; Alt+Left
/// and Alt+Right take GtkSourceView's word swap; Ctrl+% takes GtkSourceView's matching-bracket
/// jump and extends it. Ctrl+K Ctrl+I (hover) cannot be an accelerator — Ctrl+K is the agent's
/// inline edit and GTK has no chords — so the hover key outside Vim is Alt+K. The editing
/// commands (`editor::editing`) take Ctrl+/ (GtkTextView's second Select All), and Ctrl+B and
/// Ctrl+I only while Vim is off: with Vim on, Bold and Italic are disabled, so the keys are Vim's
/// page up and jump forward.
///
/// The find bar's Ctrl+F, Ctrl+H, F3 and Shift+F3 are enabled only while an editor or the find
/// bar has the focus and Vim is off, so with Vim on Ctrl+F and Ctrl+H stay Vim's own keys (page
/// down; left, or Backspace in insert mode), and the Search page keeps its own Ctrl+H. Ctrl+F is
/// shared with `win.pdf-search` (see [`SHARED_ACCELS`]).
///
/// Go to Line's Ctrl+G follows the find bar's keys: with Vim on it is Vim's (file info), and in
/// the PDF's search entry it stays the next match. New File's Ctrl+N is Vim's next line (and
/// insert-mode completion), so with Vim on and an editor focused it goes to Vim, as Ctrl+W and
/// Ctrl+O do (`editor::keys`); the menu, the palette and the key elsewhere still make a new
/// file. Save As's Ctrl+Shift+S is no Vim key.
pub const DEFAULT_ACCELS: &[(&str, &[&str])] = &[
    ("app.open", &["<Control>o"]),
    ("app.quit", &["<Control>q"]),
    ("win.save", &["<Control>s"]),
    ("win.save-all", &["<Control><Alt>s"]),
    ("win.close-tab", &["<Control>w"]),
    ("win.toggle-vim", &["<Control><Alt>v"]),
    ("win.toggle-sidebar", &["F9"]),
    ("win.toggle-pdf", &["F8"]),
    ("win.toggle-agent", &["F10"]),
    ("win.build", &["F5"]),
    // M3
    ("win.command-palette", &["<Control><Shift>p"]),
    ("win.quick-open", &["<Control>p"]),
    ("win.project-search", &["<Control><Shift>f"]),
    ("win.shortcuts", &["<Control>question"]),
    ("win.preferences", &["<Control>comma"]),
    ("win.pdf-search", &["<Control>f"]),
    ("win.goto-definition", &["F12"]),
    ("win.find-references", &["<Shift>F12"]),
    ("win.rename-label", &["F2"]),
    ("win.jump-pair", &["<Control>percent"]),
    ("win.goto-back", &["<Alt>Left"]),
    ("win.goto-forward", &["<Alt>Right"]),
    ("win.show-hover", &["<Alt>k"]),
    // Find and replace in the current file.
    ("win.find", &["<Control>f"]),
    ("win.find-replace", &["<Control>h"]),
    ("win.find-next", &["F3"]),
    ("win.find-prev", &["<Shift>F3"]),
    // Go to Line, New File and Save As.
    ("win.goto-line", &["<Control>g"]),
    ("win.new-file", &["<Control>n"]),
    ("win.save-as", &["<Control><Shift>s"]),
];

/// Accelerators that several actions share, because the shell never enables two of them at the
/// same time: GTK runs the key's action that is enabled (a disabled one lets the key go on to
/// the next). `win.pdf-search` is enabled only while the PDF has the focus, `win.find` only
/// while an editor or the find bar has it.
pub const SHARED_ACCELS: &[(&str, &[&str])] = &[("<Control>f", &["win.pdf-search", "win.find"])];

/// How the shortcuts window and the command palette group actions and keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShortcutGroup {
    General,
    Editing,
    Navigation,
    Review,
    Agent,
    BuildPdf,
    Vim,
}

impl ShortcutGroup {
    /// Every group, in the order the shortcuts window shows them.
    pub const ALL: [ShortcutGroup; 7] = [
        Self::General,
        Self::Editing,
        Self::Navigation,
        Self::Review,
        Self::Agent,
        Self::BuildPdf,
        Self::Vim,
    ];

    /// The group's title.
    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Navigation => "Navigation",
            Self::Review => "Review",
            Self::Agent => "Agent",
            Self::BuildPdf => "Build and PDF",
            Self::Vim => "Vim",
        }
    }
}

/// What an action is called (see [`ACTIONS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionInfo {
    /// The detailed action name (`win.save`).
    pub name: &'static str,
    /// Its label in title case, as menus write it ("Go to Definition").
    pub label: &'static str,
    pub group: ShortcutGroup,
    /// Why the action can be disabled, for a greyed-out palette entry.
    pub unavailable: Option<&'static str>,
}

const fn info(name: &'static str, label: &'static str, group: ShortcutGroup) -> ActionInfo {
    ActionInfo {
        name,
        label,
        group,
        unavailable: None,
    }
}

/// An action of the find bar, which works only while an editor has the focus and Vim is off.
const fn find_info(name: &'static str, label: &'static str) -> ActionInfo {
    ActionInfo {
        name,
        label,
        group: ShortcutGroup::Navigation,
        unavailable: Some("Click into the editor first; with Vim on, search with /"),
    }
}

/// Go to Line, which works only while an editor has the focus and Vim is off, as the find bar.
const GOTO_LINE: ActionInfo = ActionInfo {
    name: "win.goto-line",
    label: "Go to Line…",
    group: ShortcutGroup::Navigation,
    unavailable: Some("Click into the editor first; with Vim on, type :12 for line 12"),
};

/// Every action of a window and of the application that the user can run from the command
/// palette (actions with parameters, such as `win.review-keep-turn(u)`, and the quick actions
/// `win.quick-<slug>`, which come from `prompts.toml`, are not listed). Actions added later at
/// run time are described with [`describe_action`].
pub const ACTIONS: &[ActionInfo] = {
    use ShortcutGroup::*;
    &[
        info("app.open", "Open…", General),
        info("win.new-file", "New File…", General),
        info("app.quit", "Quit", General),
        info("win.save", "Save", General),
        info("win.save-as", "Save As…", General),
        info("win.save-all", "Save All", General),
        info("win.close-tab", "Close Tab", General),
        info("win.command-palette", "Command Palette", General),
        info("win.preferences", "Preferences", General),
        info("win.shortcuts", "Keyboard Shortcuts", General),
        info("win.toggle-vim", "Vim Mode", General),
        info("win.toggle-sidebar", "Show Sidebar", General),
        info("win.toggle-pdf", "Show PDF Preview", General),
        info("win.toggle-agent", "Show Agent Panel", General),
        info("win.toggle-spell", "Check Spelling", Editing),
        info("win.rename-label", "Rename Label", Editing),
        info("win.word-count", "Word Count", Editing),
        info("win.history", "Local History", Editing),
        info("win.history-restore", "Restore from History…", Editing),
        info("win.quick-open", "Go to File…", Navigation),
        GOTO_LINE,
        info("win.project-search", "Find in Project", Navigation),
        info("win.goto-definition", "Go to Definition", Navigation),
        info("win.find-references", "Find References", Navigation),
        info("win.goto-back", "Go Back", Navigation),
        info("win.goto-forward", "Go Forward", Navigation),
        info("win.show-hover", "Show Hover", Navigation),
        info("win.jump-pair", "Jump to Matching Delimiter", Navigation),
        find_info("win.find", "Find in File"),
        find_info("win.find-replace", "Replace in File"),
        find_info("win.find-next", "Find Next"),
        find_info("win.find-prev", "Find Previous"),
        info("win.review-next", "Next Change", Review),
        info("win.review-prev", "Previous Change", Review),
        info("win.review-accept", "Accept Change", Review),
        info("win.review-reject", "Reject Change", Review),
        info(
            "win.review-accept-file",
            "Accept All Changes in File",
            Review,
        ),
        info(
            "win.review-reject-file",
            "Reject All Changes in File",
            Review,
        ),
        info("win.review-accept-all", "Accept All Changes", Review),
        info("win.review-reject-all", "Reject All Changes", Review),
        info("win.review-undo", "Undo Last Decision", Review),
        info("win.review-undo-turn", "Undo Last Turn", Review),
        info("win.review-reading-mode", "Reading Mode", Review),
        info("win.review-show-changes", "Show Changes", Review),
        info("win.agent-focus", "Focus Agent Prompt", Agent),
        info("win.agent-stop", "Stop Agent Turn", Agent),
        info("win.agent-inline-edit", "Edit with Agent…", Agent),
        info("win.build", "Build", BuildPdf),
        info("win.stop-build", "Stop Build", BuildPdf),
        info("win.clean-build", "Clean Build Files and Rebuild", BuildPdf),
        info("win.problem-next", "Next Problem", BuildPdf),
        info("win.problem-prev", "Previous Problem", BuildPdf),
        info("win.sync-forward", "Show Cursor in PDF", BuildPdf),
        info("win.pdf-invert", "Dark PDF Pages", BuildPdf),
        info("win.pdf-zoom-in", "Zoom In PDF", BuildPdf),
        info("win.pdf-zoom-out", "Zoom Out PDF", BuildPdf),
        info("win.pdf-fit-width", "Fit PDF to Width", BuildPdf),
        ActionInfo {
            name: "win.pdf-search",
            label: "Search in PDF",
            group: BuildPdf,
            unavailable: Some("Click into the PDF preview first"),
        },
        info("win.pdf-detach", "PDF in Separate Window", BuildPdf),
        info(
            "win.toggle-pdf-sidebar",
            "PDF Thumbnails and Bookmarks",
            BuildPdf,
        ),
    ]
};

/// An action's description, from [`ACTIONS`] or [`describe_action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDescription {
    pub name: String,
    pub label: String,
    pub group: ShortcutGroup,
    pub unavailable: Option<String>,
}

impl From<&ActionInfo> for ActionDescription {
    fn from(info: &ActionInfo) -> Self {
        Self {
            name: info.name.to_owned(),
            label: info.label.to_owned(),
            group: info.group,
            unavailable: info.unavailable.map(str::to_owned),
        }
    }
}

thread_local! {
    /// Descriptions added at run time ([`describe_action`]), by name.
    static DESCRIBED: RefCell<BTreeMap<String, ActionDescription>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// Describes an action added at run time (or overrides the description of one in [`ACTIONS`]).
pub fn describe_action(name: &str, label: &str, group: ShortcutGroup, unavailable: Option<&str>) {
    DESCRIBED.with_borrow_mut(|described| {
        described.insert(
            name.to_owned(),
            ActionDescription {
                name: name.to_owned(),
                label: label.to_owned(),
                group,
                unavailable: unavailable.map(str::to_owned),
            },
        );
    });
}

/// The description of the detailed action `name`.
pub fn action_info(name: &str) -> Option<ActionDescription> {
    DESCRIBED
        .with_borrow(|described| described.get(name).cloned())
        .or_else(|| ACTIONS.iter().find(|a| a.name == name).map(Into::into))
}

/// Every described action: [`ACTIONS`] in order (with run-time overrides), then the ones added
/// at run time, by name.
pub fn action_descriptions() -> Vec<ActionDescription> {
    let mut list: Vec<ActionDescription> = ACTIONS
        .iter()
        .map(|a| action_info(a.name).unwrap_or_else(|| a.into()))
        .collect();
    DESCRIBED.with_borrow(|described| {
        list.extend(
            described
                .values()
                .filter(|d| !ACTIONS.iter().any(|a| a.name == d.name))
                .cloned(),
        );
    });
    list
}

/// Keys that are not accelerators (the key layer, gestures, Vim) and what they do, as the
/// shortcuts window lists them; `[vim.normal]` bindings (`VimCfg::normal_bindings`) come on
/// top in the Vim group.
pub const KEY_LAYER_SHORTCUTS: &[(&str, &str, ShortcutGroup)] = &[
    (
        "Tab / Shift+Tab",
        "Expand a snippet, next or previous stop, step out of a closer",
        ShortcutGroup::Editing,
    ),
    ("Ctrl+Space", "Show completions", ShortcutGroup::Editing),
    (
        "Ctrl+Click",
        "Go to the definition under the pointer",
        ShortcutGroup::Navigation,
    ),
    (
        "Ctrl+Click in the PDF",
        "Show the source of that place",
        ShortcutGroup::BuildPdf,
    ),
    // Keys of the PDF view, the Search page and the find bar (not accelerators: they work only
    // there).
    (
        "/",
        "Search the PDF (in the PDF view)",
        ShortcutGroup::BuildPdf,
    ),
    (
        "n / N",
        "Next / previous match of the PDF search (in the PDF view)",
        ShortcutGroup::BuildPdf,
    ),
    (
        "BackSpace",
        "Back after a link jump (in the PDF view)",
        ShortcutGroup::BuildPdf,
    ),
    (
        "Alt+C / Alt+W / Alt+R",
        "Search page and find bar: Match Case / Whole Word / Regular Expression",
        ShortcutGroup::Navigation,
    ),
    (
        "Ctrl+H",
        "Search page: show Replace",
        ShortcutGroup::Navigation,
    ),
    (
        "Ctrl+O",
        "Go back after a jump (else Vim's jump list)",
        ShortcutGroup::Vim,
    ),
    ("%", "Jump to the matching delimiter", ShortcutGroup::Vim),
    (
        "y / Shift+Y / n",
        "Answer the agent's permission card: once / always / reject",
        ShortcutGroup::Agent,
    ),
    (
        "Up / Down",
        "Prompt box: the previous / next prompt you sent",
        ShortcutGroup::Agent,
    ),
];

/// State shared by the application's callbacks.
struct Shared {
    /// The current settings (updated by live reloads).
    settings: RefCell<Settings>,
    /// Problems with the settings files at start-up, reported once the window is up.
    settings_errors: Vec<String>,
    files: Option<SettingsFiles>,
    /// Reloads the settings files when they change (interactive runs only).
    watcher: RefCell<Option<settings::Watcher>>,
    paths: Option<Paths>,
    options: Options,
    started: Instant,
    windows: RefCell<Vec<Rc<WindowState>>>,
    /// Set when a scripted run fails; the process then exits with status 1.
    failed: Cell<bool>,
}

/// Runs the application until its last window closes (or a scripted run quits it).
///
/// `started` is the process start, which `--quit-after-ms` counts from. GApplication never sees
/// the command line: options were parsed already, and the app is `NON_UNIQUE`, so every `lemma`
/// invocation is its own process with its own window.
pub fn run(options: Options, started: Instant) -> ExitCode {
    let paths = Paths::from_env();
    let files = SettingsFiles::locate(paths.as_ref());
    // Read before the main loop starts, so the synchronous reads cannot stall a frame.
    let (settings, settings_errors) = settings::load(files.as_ref());
    choose_renderer(settings.config.editor.renderer);
    // On macOS, Finder and the Dock hand documents to the running app, which GTK passes on as
    // `open` (only with HANDLES_OPEN). Elsewhere files come from Lemma's own command line.
    let flags = if cfg!(target_os = "macos") {
        gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_OPEN
    } else {
        gio::ApplicationFlags::NON_UNIQUE
    };
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    let shared = Rc::new(Shared {
        settings: RefCell::new(settings),
        settings_errors,
        files,
        watcher: RefCell::new(None),
        paths,
        options,
        started,
        windows: RefCell::new(Vec::new()),
        failed: Cell::new(false),
    });
    let s = shared.clone();
    app.connect_startup(move |app| startup(app, &s));
    let s = shared.clone();
    app.connect_activate(move |app| activate(app, &s));
    #[cfg(target_os = "macos")]
    {
        let s = shared.clone();
        app.connect_open(move |app, files, _hint| open_handed_files(app, &s, files));
    }
    let status = app.run_with_args(&["lemma"]);
    // The callbacks hold `shared`; drop the windows explicitly before reporting.
    shared.windows.borrow_mut().clear();
    shared.watcher.replace(None);
    // Only removed while empty.
    let _ = std::fs::remove_dir(no_project_dir());
    if shared.failed.get() {
        ExitCode::FAILURE
    } else {
        status.into()
    }
}

/// The environment variable GTK reads (once, when the first window is realized) to pick its
/// renderer.
const GSK_RENDERER: &str = "GSK_RENDERER";

/// Sets [`GSK_RENDERER`] from `[editor] renderer` (M5) unless the environment has it already:
/// the user's (or a test's) choice wins. `auto` leaves Broadway (offscreen tests and snapshots)
/// to its own renderer. Only the start-up settings count; GTK keeps the renderer it picked for
/// the life of the process.
fn choose_renderer(renderer: Renderer) {
    let env_set = std::env::var_os(GSK_RENDERER).is_some();
    let backend = std::env::var("GDK_BACKEND").ok();
    let Some(value) = renderer_for(renderer, env_set, backend.as_deref()) else {
        if env_set {
            tracing::debug!("renderer: GSK_RENDERER from the environment");
        } else {
            tracing::debug!("renderer: Broadway's own");
        }
        return;
    };
    tracing::debug!("renderer: GSK_RENDERER={value} from [editor] renderer");
    // SAFETY: called from `run` before GTK or anything else starts a thread (the process has
    // only the main thread here), so nothing can read the environment concurrently.
    unsafe { std::env::set_var(GSK_RENDERER, value) };
    RENDERER_SET.with(|set| set.set(true));
}

thread_local! {
    /// [`choose_renderer`] put [`GSK_RENDERER`] into the environment (until
    /// [`forget_renderer_choice`]).
    static RENDERER_SET: Cell<bool> = const { Cell::new(false) };
}

/// The first window is realized, so GTK has read [`GSK_RENDERER`] (it reads it once, for the
/// first renderer, and keeps its choice): when [`choose_renderer`] set it, it leaves the
/// environment again. Lemma's choice is for its own windows; the programs and apps it starts
/// (latexmk, the agent, a figure opened in an image viewer) must not inherit it.
fn forget_renderer_choice() {
    if !RENDERER_SET.with(|set| set.replace(false)) {
        return;
    }
    // Other C libraries (macOS's among them) may free the removed string, which a concurrent
    // `getenv` could still be reading: there the variable stays. It is harmless where it does:
    // apps opened through Launch Services do not inherit Lemma's environment.
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return;
    }
    // SAFETY: GTK's threads may be running. Rust's own environment accesses take std's lock;
    // glibc's unsetenv only removes the entry from `environ` (no string is freed), so a
    // concurrent C `getenv` cannot read freed memory, and nothing reads this variable again.
    unsafe { std::env::remove_var(GSK_RENDERER) };
    tracing::debug!("renderer: GSK_RENDERER removed from the environment (GTK has read it)");
}

/// The `GSK_RENDERER` value [`choose_renderer`] sets, if any.
fn renderer_for(renderer: Renderer, env_set: bool, backend: Option<&str>) -> Option<&'static str> {
    if env_set || (renderer == Renderer::Auto && backend == Some("broadway")) {
        None
    } else {
        Some(renderer.gsk_name())
    }
}

/// How long after the document is open the heap is trimmed: by then the first frames, spell
/// checking's set-up and the first index have run, and their temporary buffers are free.
const TRIM_AFTER: Duration = Duration::from_millis(2000);

/// Returns the heap's free memory to the system once start-up has settled (M5). With glibc's
/// `M_MMAP_THRESHOLD` fixed (see [`crate::heap::tune`]), only the top of a heap is returned by
/// itself; start-up leaves free pages in the middle (file reading, layout, the first highlighting
/// pass).
fn trim_heap_later() {
    glib::timeout_add_local_once(TRIM_AFTER, crate::heap::trim);
}

fn startup(app: &adw::Application, shared: &Rc<Shared>) {
    perf::mark("gtk_init", shared.started);
    sourceview5::init();
    if shared.options.is_scripted()
        && let Some(settings) = gtk::Settings::default()
    {
        // Snapshots must not catch a transition halfway (Broadway draws about one frame a second).
        settings.set_gtk_enable_animations(false);
    }
    let config = shared.settings.borrow().config.clone();
    if let Some(display) = gtk::gdk::Display::default() {
        editor::install_css(&display, &config.editor);
    }
    install_actions(app, shared);
    set_accels(app, &config.keys);
    // Scripted runs (tests, snapshots, measurements) read the settings once.
    if !shared.options.is_scripted()
        && let Some(files) = &shared.files
    {
        let weak = Rc::downgrade(shared);
        let watcher = settings::Watcher::start(files, move |file| {
            if let Some(shared) = weak.upgrade() {
                glib::spawn_future_local(reload_settings(shared, file));
            }
        });
        shared.watcher.replace(Some(watcher));
    }
    // A closed window leaves the application; its state goes with it.
    let weak = Rc::downgrade(shared);
    app.connect_window_removed(move |_, window| {
        if let Some(shared) = weak.upgrade() {
            shared
                .windows
                .borrow_mut()
                .retain(|s| s.window.upcast_ref::<gtk::Window>() != window);
        }
    });
}

fn activate(app: &adw::Application, shared: &Rc<Shared>) {
    open_window(app, shared, true);
}

/// Opens the window (or presents it when it is open already). Without a FILE argument, the
/// window reopens the last session ([`last_session`]) when `reopen_last` is set; else it starts
/// in the current directory.
fn open_window(app: &adw::Application, shared: &Rc<Shared>, reopen_last: bool) {
    if let Some(state) = shared.windows.borrow().first() {
        state.window.present();
        return;
    }
    let reopen = if reopen_last {
        last_session(shared)
    } else {
        None
    };
    // The last session's root is chosen as `lemma DIR` would choose it.
    let wanted = shared
        .options
        .file
        .clone()
        .or_else(|| reopen.as_ref().map(|r| r.root.clone()));
    let root = match project_root(wanted.as_deref()) {
        Ok(root) => root,
        Err(e) => {
            tracing::error!("no usable project directory: {e}");
            eprintln!("lemma: no usable project directory: {e}");
            shared.failed.set(true);
            return;
        }
    };
    // Only a run that quits by itself must never wait for an answer; any other window is the
    // user's, and asks before discarding their changes.
    let mut settings = shared.settings.borrow().clone();
    if shared.options.quits_by_itself() {
        // Tests, snapshots and measurements leave no local history in the user's state
        // directory (review persistence is off for them too).
        let mut config = (*settings.config).clone();
        config.history.enabled = false;
        settings.config = Rc::new(config);
    }
    let state = WindowState::new_with_settings(
        app,
        &settings,
        shared.paths.clone(),
        Workspace::new(root),
        shared.options.quits_by_itself(),
    );
    if let (Some(scenario), Some(agent)) =
        (&shared.options.agent_scenario, state.agent_controller())
    {
        agent.set_scenario(Some(normalize(scenario)));
    }
    state.set_settings_files(shared.files.clone());
    if wanted.as_deref().is_some_and(is_directory) {
        // `lemma DIR` (or the last session's root): the user chose the project; opening a
        // document does not move it.
        state.pin_root();
    }
    perf::mark("window", shared.started);
    shared.windows.borrow_mut().push(state.clone());
    // Runs after GtkWindow's own realize, which creates the renderer.
    state.window.connect_realize(|_| forget_renderer_choice());
    state.window.present();
    perf::mark("presented", shared.started);
    perf::watch_first_frame(&state.window, shared.started);
    glib::spawn_future_local(start(state, shared.clone(), reopen.and_then(|r| r.file)));
}

/// The project and file `lemma` without a FILE reopens (`session::reopen`): only when the run
/// restores sessions (`[session] restore`, and a scripted run only with `--restore-session`).
fn last_session(shared: &Shared) -> Option<session::Reopen> {
    let opts = &shared.options;
    if opts.file.is_some()
        || (opts.is_scripted() && !opts.restore_session)
        || !shared.settings.borrow().config.session.restore
    {
        return None;
    }
    session::reopen(shared.paths.as_ref()?)
}

/// True when the FILE argument names an existing directory (`lemma .`, as with `nvim .`).
fn is_directory(file: &Path) -> bool {
    file.is_dir()
}

/// The project root of a window: the directory of `file`, created if it does not exist yet (the
/// file's first save needs it), `file` itself when it is a directory (`lemma .`), or the current
/// directory without a file.
///
/// Agent paths are confined to the root, so there is no fallback to an ancestor (let alone `/`):
/// when the directory cannot be used (or is `/`), the window gets a private empty directory
/// instead, where nothing of the user's can be reached.
fn project_root(file: Option<&Path>) -> Result<ProjectRoot, String> {
    let wanted = match file {
        Some(dir) if is_directory(dir) => Some(normalize(dir)),
        Some(file) => normalize(file).parent().map(Path::to_path_buf),
        None => std::env::current_dir().ok(),
    };
    // The whole file system is never a project.
    if let Some(dir) = wanted.filter(|d| d.parent().is_some()) {
        if file.is_some()
            && !dir.exists()
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            tracing::warn!("cannot create {}: {e}", dir.display());
        }
        match ProjectRoot::new(&dir) {
            Ok(root) => return Ok(root),
            Err(e) => tracing::warn!("{} cannot be the project root: {e}", dir.display()),
        }
    }
    let empty = no_project_dir();
    std::fs::create_dir_all(&empty).map_err(|e| format!("{}: {e}", empty.display()))?;
    ProjectRoot::new(&empty).map_err(|e| format!("{}: {e}", empty.display()))
}

/// This process's private, empty stand-in for a project root (see [`project_root`]).
fn no_project_dir() -> PathBuf {
    glib::user_runtime_dir().join(format!("lemma-{}-no-project", std::process::id()))
}

/// Opens the FILE argument (or `reopened`, the last session's file) and runs the scripted
/// steps.
async fn start(state: Rc<WindowState>, shared: Rc<Shared>, reopened: Option<PathBuf>) {
    let opts = &shared.options;
    let scripted = opts.is_scripted();
    let report = |message: &str| {
        if scripted {
            eprintln!("lemma: {message}");
        } else {
            state.toast(message);
        }
    };
    for e in &shared.settings_errors {
        report(e);
    }
    let config = shared.files.as_ref().map(|f| f.config.as_path());
    for e in key_problems(config, &state.config.keys) {
        report(&e);
    }
    // A directory (`lemma .`) is the project root (see `project_root`): nothing to open.
    let file = match opts.file.as_deref() {
        Some(file) => Some(file).filter(|f| !is_directory(f)),
        None => reopened.as_deref(),
    };
    if let Some(file) = file
        && let Err(e) = state.open_file(file).await
    {
        report(&e);
        if scripted {
            shared.failed.set(true);
        }
    }
    perf::mark("opened", shared.started);
    // The session of the project (M3): not in scripted runs unless asked for.
    if (!scripted || opts.restore_session)
        && state.config.session.restore
        && let Err(e) = session::restore(&state).await
    {
        report(&e);
    }
    trim_heap_later();
    if let Err(e) = perf::run(&state, shared.started).await {
        eprintln!("lemma: {e}");
        shared.failed.set(true);
    }
    if scripted && let Err(e) = testmode::run(&state, opts, shared.started).await {
        eprintln!("lemma: {e}");
        shared.failed.set(true);
    }
}

fn install_actions(app: &adw::Application, shared: &Rc<Shared>) {
    let open = gio::SimpleAction::new("open", None);
    let weak = Rc::downgrade(shared);
    let app_weak = app.downgrade();
    open.connect_activate(move |_, _| {
        if let (Some(shared), Some(app)) = (weak.upgrade(), app_weak.upgrade())
            && let Some(state) = active_window(&app, &shared)
        {
            glib::spawn_future_local(open_with_dialog(state));
        }
    });
    app.add_action(&open);

    let quit = gio::SimpleAction::new("quit", None);
    let weak = Rc::downgrade(shared);
    let app_weak = app.downgrade();
    quit.connect_activate(move |_, _| {
        let (Some(shared), Some(app)) = (weak.upgrade(), app_weak.upgrade()) else {
            return;
        };
        let windows = shared.windows.borrow().clone();
        if windows.is_empty() {
            app.quit();
        }
        // Each window asks about its unsaved documents; the app ends with the last window.
        for state in windows {
            state.window.close();
        }
    });
    app.add_action(&quit);
}

/// Documents macOS hands over (Finder's Open With, a drop on the Dock icon, `open -a Lemma
/// main.tex`): each opens in the active window, as if chosen in the Open dialog.
#[cfg(target_os = "macos")]
fn open_handed_files(app: &adw::Application, shared: &Rc<Shared>, files: &[gio::File]) {
    if shared.windows.borrow().is_empty() {
        // The documents handed over are what the user opens, not the last session.
        open_window(app, shared, false);
    }
    let Some(state) = active_window(app, shared) else {
        return;
    };
    state.window.present();
    let paths: Vec<PathBuf> = files.iter().filter_map(gio::File::path).collect();
    glib::spawn_future_local(async move {
        for path in paths {
            if is_directory(&path) {
                state.toast("A folder opens as a project from the command line: lemma DIR");
                continue;
            }
            if let Err(e) = state.open_file(&path).await {
                state.toast(&e);
            }
        }
    });
}

/// The state of the focused window, else of the first one.
fn active_window(app: &adw::Application, shared: &Shared) -> Option<Rc<WindowState>> {
    let windows = shared.windows.borrow();
    let active = app.active_window();
    windows
        .iter()
        .find(|s| active.as_ref() == Some(s.window.upcast_ref()))
        .or_else(|| windows.first())
        .cloned()
}

async fn open_with_dialog(state: Rc<WindowState>) {
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let tex = gtk::FileFilter::new();
    tex.set_name(Some("LaTeX Documents"));
    for pattern in ["*.tex", "*.sty", "*.cls", "*.bib", "*.ltx", "*.dtx"] {
        tex.add_pattern(pattern);
    }
    let all = gtk::FileFilter::new();
    all.set_name(Some("All Files"));
    all.add_pattern("*");
    filters.append(&tex);
    filters.append(&all);
    let dialog = gtk::FileDialog::builder()
        .title("Open Document")
        .modal(true)
        .filters(&filters)
        .default_filter(&tex)
        .initial_folder(&gio::File::for_path(state.workspace.root().path()))
        .build();
    let file = match dialog.open_future(Some(&state.window)).await {
        Ok(file) => file,
        // Dismissed or cancelled.
        Err(_) => return,
    };
    let Some(path) = file.path() else {
        state.toast("Only local files can be opened");
        return;
    };
    if let Err(e) = state.open_file(&path).await {
        state.toast(&e);
    }
}

/// Reads a changed settings file again and hands the new values to every window; a file with
/// errors is reported in every window and changes nothing.
async fn reload_settings(shared: Rc<Shared>, file: ConfigFile) {
    let Some(files) = shared.files.clone() else {
        return;
    };
    let reloaded = settings::reload(&files, file).await;
    let windows = shared.windows.borrow().clone();
    match reloaded {
        Ok(Reloaded::Config(config)) => {
            shared.settings.borrow_mut().config = config.clone();
            let problems = key_problems(Some(&files.config), &config.keys);
            for state in windows {
                state.set_config(config.clone());
                for e in &problems {
                    state.toast(e);
                }
            }
        }
        Ok(Reloaded::Snippets(snippets)) => {
            shared.settings.borrow_mut().snippets = snippets.clone();
            for state in windows {
                state.set_snippets(snippets.clone());
            }
        }
        Ok(Reloaded::Prompts(prompts)) => {
            shared.settings.borrow_mut().prompts = prompts.clone();
            for state in windows {
                state.set_prompts(prompts.clone());
            }
        }
        Err(e) => {
            tracing::warn!("{e}");
            for state in windows {
                state.toast(&e);
            }
        }
    }
}

thread_local! {
    /// Every action's default accelerators, as registered by [`set_accels`] and
    /// [`set_default_accels`]: a `[keys]` override that is removed on a reload brings them back.
    static DEFAULTS: RefCell<BTreeMap<String, Vec<String>>> = const { RefCell::new(BTreeMap::new()) };
}

/// Records `accels` as the default accelerators of their actions (see [`reapply_accels`]).
pub fn register_default_accels(accels: &[(&str, &[&str])]) {
    DEFAULTS.with_borrow_mut(|defaults| {
        for (action, keys) in accels {
            defaults.insert(
                (*action).to_owned(),
                keys.iter().map(|k| (*k).to_owned()).collect(),
            );
        }
    });
}

/// The default accelerators registered for `action`.
pub fn default_accels(action: &str) -> Option<Vec<String>> {
    DEFAULTS.with_borrow(|defaults| defaults.get(action).cloned())
}

/// Sets the default accelerators of a module's actions, except where `[keys]` rebinds them (an
/// empty override removes the binding; a valid one was applied by [`set_accels`]; an invalid one
/// is ignored, so the default stays), and registers them for [`reapply_accels`]. Every module
/// with default accelerators (M2 included) sets them through this function.
pub fn set_default_accels(state: &WindowState, accels: &[(&str, &[&str])]) {
    register_default_accels(accels);
    for (action, keys) in accels {
        let rebound = state
            .config
            .keys
            .bindings
            .get(*action)
            .is_some_and(|accel| accel.is_empty() || gtk::accelerator_parse(accel).is_some());
        if !rebound {
            state.app.set_accels_for_action(action, keys);
        }
    }
}

/// Installs [`DEFAULT_ACCELS`], then the `[keys]` overrides (an empty string removes a binding).
pub fn set_accels(app: &adw::Application, keys: &KeysCfg) {
    register_default_accels(DEFAULT_ACCELS);
    register_default_accels(review_ui::ACCELS);
    for (action, accels) in DEFAULT_ACCELS {
        app.set_accels_for_action(action, accels);
    }
    for (action, accel) in &keys.bindings {
        apply_override(app, action, accel);
    }
}

/// True when `action` is a detailed action name with a prefix (`win.save`,
/// `win.toggle-pdf`, `app.quit`, or one with a target such as `win.go(3)`) that can be given
/// accelerators. GTK aborts the process on a name it cannot parse (`"win.toggle pdf"`,
/// `"win.build:"`), so nothing else reaches `set_accels_for_action`.
pub fn is_valid_action_name(action: &str) -> bool {
    gio::Action::parse_detailed_name(action).is_ok_and(|(name, _)| {
        name.split_once('.')
            .is_some_and(|(prefix, rest)| !prefix.is_empty() && !rest.is_empty())
    })
}

/// The `[keys]` entries of `keys` that are ignored, as messages for the user (`config` is the
/// file they come from).
pub fn key_problems(config: Option<&Path>, keys: &KeysCfg) -> Vec<String> {
    let file = config.map_or_else(|| "config.toml".to_owned(), |p| p.display().to_string());
    keys.bindings
        .iter()
        .filter_map(|(action, accel)| {
            if !is_valid_action_name(action) {
                Some(format!(
                    "{file}: [keys] {action:?} is not an action name like \"win.save\"; \
                     it is ignored"
                ))
            } else if !accel.is_empty() && gtk::accelerator_parse(accel).is_none() {
                Some(format!(
                    "{file}: [keys] {accel:?} for {action} is not a valid accelerator; it is \
                     ignored"
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Applies one `[keys]` entry; `false` when it is not valid (nothing changes then).
fn apply_override(app: &adw::Application, action: &str, accel: &str) -> bool {
    if !is_valid_action_name(action) {
        tracing::warn!("[keys]: {action:?} is not an action name like \"win.save\"");
        false
    } else if accel.is_empty() {
        app.set_accels_for_action(action, &[]);
        true
    } else if gtk::accelerator_parse(accel).is_some() {
        app.set_accels_for_action(action, &[accel]);
        true
    } else {
        tracing::warn!("[keys]: {accel:?} for {action} is not a valid accelerator");
        false
    }
}

/// After a reload of `config.toml`: every action whose `[keys]` entry changed gets its new
/// override, or its registered default back (no binding when it has none).
pub fn reapply_accels(app: &adw::Application, old: &KeysCfg, new: &KeysCfg) {
    let actions: BTreeSet<&String> = old.bindings.keys().chain(new.bindings.keys()).collect();
    for action in actions {
        let accel = new.bindings.get(action);
        if old.bindings.get(action) == accel {
            continue;
        }
        if accel.is_some_and(|accel| apply_override(app, action, accel)) {
            continue;
        }
        // Nothing was ever bound under a name GTK cannot parse (and asking would abort).
        if !is_valid_action_name(action) {
            continue;
        }
        let defaults = default_accels(action).unwrap_or_default();
        let defaults: Vec<&str> = defaults.iter().map(String::as_str).collect();
        app.set_accels_for_action(action, &defaults);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every default accelerator of the shell and the review chords.
    fn all_defaults() -> Vec<(&'static str, &'static str)> {
        DEFAULT_ACCELS
            .iter()
            .chain(review_ui::ACCELS)
            .flat_map(|(action, accels)| accels.iter().map(move |a| (*action, *a)))
            .collect()
    }

    #[test]
    fn renderer_choice() {
        use lemma_config::Renderer::*;
        // The environment wins.
        assert_eq!(renderer_for(Cairo, true, None), None);
        assert_eq!(renderer_for(Auto, true, Some("wayland")), None);
        // `auto` is cairo, except on Broadway, which keeps its own renderer.
        assert_eq!(renderer_for(Auto, false, None), Some("cairo"));
        assert_eq!(renderer_for(Auto, false, Some("wayland")), Some("cairo"));
        assert_eq!(renderer_for(Auto, false, Some("broadway")), None);
        assert_eq!(renderer_for(Cairo, false, Some("broadway")), Some("cairo"));
        assert_eq!(renderer_for(Vulkan, false, None), Some("vulkan"));
        assert_eq!(renderer_for(Gl, false, Some("x11")), Some("ngl"));
    }

    #[test]
    fn default_accelerators_are_valid_names_and_unique() {
        let mut seen: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for (action, accel) in all_defaults() {
            assert!(is_valid_action_name(action), "{action}");
            // Normalised so `<Control>P` and `<Control>p` would count as one.
            seen.entry(accel.to_ascii_lowercase())
                .or_default()
                .push(action);
        }
        for (accel, actions) in &seen {
            let shared = SHARED_ACCELS
                .iter()
                .find(|(a, _)| a.to_ascii_lowercase() == *accel);
            match shared {
                Some((_, owners)) => assert_eq!(actions.as_slice(), *owners, "{accel} is shared"),
                None => assert_eq!(actions.len(), 1, "{accel} is bound to {actions:?}"),
            }
        }
        // Every shared key is a default of each of its actions.
        for (accel, owners) in SHARED_ACCELS {
            let actions = seen.get(&accel.to_ascii_lowercase());
            assert_eq!(actions.map(Vec::as_slice), Some(*owners), "{accel}");
        }
        // Ones the other modules set (agent panel, inline edit, build, PDF, Problems, editing
        // commands).
        for accel in [
            "<control>l",
            "<control><shift>backspace",
            "<control>k",
            "<shift>f5",
            "<control><alt>j",
            "<control><shift>i",
            "f4",
            "<shift>f4",
            "<control>slash",
            "<control>b",
            "<control>i",
        ] {
            assert!(!seen.contains_key(accel), "{accel} is taken");
        }
    }

    #[test]
    fn m3_accelerators_keep_clear_of_vim() {
        // With Vim on and an editor focused, Ctrl+W and Ctrl+O are Vim's (§11); plain keys and
        // Shift+key are typed text.
        for (action, accel) in DEFAULT_ACCELS
            .iter()
            .skip(10)
            .flat_map(|(a, accels)| accels.iter().map(move |accel| (*a, *accel)))
        {
            let lower = accel.to_ascii_lowercase();
            assert!(
                lower != "<control>w" && lower != "<control>o",
                "{action}: {accel}"
            );
            let has_modifier = ["<control>", "<alt>", "<super>"]
                .iter()
                .any(|m| lower.contains(m));
            let function_key = accel.trim_start_matches("<Shift>").starts_with('F');
            assert!(
                has_modifier || function_key,
                "{action}: {accel} would be typed"
            );
            assert!(
                action != "win.show-hover" || !lower.contains("<control>k"),
                "Ctrl+K is the inline edit"
            );
        }
        // New File's Ctrl+N is Vim's next line: in an editor with Vim on, the key layer gives
        // it to Vim, as Ctrl+W and Ctrl+O; Go to Line's Ctrl+G is off with Vim on.
        let ctrl = gtk::gdk::ModifierType::CONTROL_MASK;
        for key in [gtk::gdk::Key::n, gtk::gdk::Key::w, gtk::gdk::Key::o] {
            assert!(editor::keys::is_vim_accelerator_key(key, ctrl), "{key:?}");
        }
        assert!(!editor::keys::is_vim_accelerator_key(
            gtk::gdk::Key::s,
            ctrl
        ));
    }

    #[test]
    fn every_action_with_a_default_accelerator_is_described() {
        for (action, _) in all_defaults() {
            assert!(action_info(action).is_some(), "{action} has no label");
        }
        let mut names: Vec<&str> = ACTIONS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ACTIONS.len(), "an action is described twice");
        for a in ACTIONS {
            assert!(is_valid_action_name(a.name), "{}", a.name);
            assert!(!a.label.is_empty() && !a.label.ends_with('.'), "{}", a.name);
        }
        assert_eq!(
            action_info("win.pdf-search").and_then(|a| a.unavailable),
            Some("Click into the PDF preview first".to_owned())
        );
    }

    #[test]
    fn run_time_descriptions() {
        assert_eq!(action_info("win.quick-proofread"), None);
        describe_action(
            "win.quick-proofread",
            "Proofread",
            ShortcutGroup::Agent,
            None,
        );
        let info = action_info("win.quick-proofread").unwrap();
        assert_eq!(info.label, "Proofread");
        assert_eq!(info.group, ShortcutGroup::Agent);
        let all = action_descriptions();
        assert_eq!(all.len(), ACTIONS.len() + 1);
        assert_eq!(all.last().unwrap().name, "win.quick-proofread");
        // An override keeps the action's place.
        describe_action("win.build", "Compile", ShortcutGroup::BuildPdf, None);
        let all = action_descriptions();
        let build = all.iter().position(|a| a.name == "win.build").unwrap();
        assert_eq!(all[build].label, "Compile");
        assert_eq!(ShortcutGroup::ALL.len(), 7);
        assert_eq!(ShortcutGroup::BuildPdf.title(), "Build and PDF");
    }
}
