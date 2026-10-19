//! The goto registry (M3, WP3.0): "go to definition" for whatever is under the pointer or the
//! cursor, answered by **resolvers** that other modules register — references, citations,
//! included files, local packages and user macros (WP3.2, [`PRIORITY_REFS`]) — plus the
//! window's back-jump list.
//!
//! # Resolvers
//!
//! ```ignore
//! goto::register(view, priority, |ctx: &GotoCtx| -> Option<GotoTarget> { … });
//! ```
//!
//! Resolvers run highest priority first (equal priorities in registration order); the first
//! answer wins. They run on the main thread and must be quick (index lookups). [`GotoCtx`]
//! dereferences to a [`TextPoint`] (document, byte offset, scanner context and token, and the
//! name under the point: [`TextPoint::item`], [`TextPoint::control_word`]).
//!
//! A [`GotoTarget`] is a byte offset in a file, a line of a file (optionally the first
//! occurrence of a text on it: `\label{eq:1}` on the line the project index gives), a place in
//! the PDF preview, or a file that is not text (an image), which opens in the system's default
//! application after the user confirms.
//!
//! # Triggers
//!
//! - **Ctrl+click** on the text ([`Trigger::Pointer`]); while Ctrl is held, the name under the
//!   pointer is underlined and the pointer becomes a hand when a resolver answers for it.
//! - **`gd`** in Vim normal mode (a `[vim.normal]` binding) and **F12** (`win.goto-definition`)
//!   at the cursor, else at the character before it ([`goto_definition`]); nothing found: a
//!   toast.
//!
//! # The back-jump list
//!
//! Every jump made through [`navigate`] (goto, and other modules that move the cursor for the
//! user: outline rows, search results, references) records where the cursor was. **Alt+Left**
//! (`win.goto-back`) goes back, **Alt+Right** (`win.goto-forward`) forward again; in **Vim
//! normal mode Ctrl+O** goes back while the list has entries, and is Vim's own Ctrl+O (its
//! jump list) otherwise. Positions are kept with text marks while their document is open, so
//! edits do not shift them; at most [`MAX_JUMPS`] are kept.
//!
//! Other places with a history of their own (the PDF preview's link jumps, WP3.4) hook into
//! Back with [`add_back_handler`]: the handler runs first and returns true when it went back
//! itself (e.g. because the PDF has the focus).
//!
//! # Window hooks
//!
//! The window calls [`install`] once (the jump list) and [`attach`] / [`detach`] for every
//! editor tab, before (after) the other M3 per-editor hooks. WP3.2's resolvers are registered
//! in [`attach_resolvers`] at the end of this file.

use super::{Editor, TexView, TextPoint, Trigger, VimMode, keys};
use crate::hooks::{HookId, Hooks};
use crate::pdf::pdf_view::Reveal;
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Priority of WP3.2's resolvers.
pub const PRIORITY_REFS: i32 = 200;
/// Priority of the key-layer handler of Ctrl+O in Vim normal mode: below the Vim bindings
/// (`keys::PRIORITY_VIM_BINDINGS`), so a held binding prefix is given to Vim first.
pub const PRIORITY_BACK_KEY: i32 = 50;
/// Most entries of each direction of the jump list.
pub const MAX_JUMPS: usize = 100;
/// The tag underlining the name under the pointer while Ctrl is held.
pub const LINK_TAG: &str = "lemma-goto-link";

/// Where a jump goes.
#[derive(Clone, Debug, PartialEq)]
pub enum GotoTarget {
    /// A byte offset in a (text) file, opened in a tab like any file.
    File { path: PathBuf, offset: usize },
    /// A 0-based line of a file; the cursor goes to the first occurrence of `find` on it, or to
    /// its first non-blank character. A line past the end is the last line.
    Line {
        path: PathBuf,
        line: u32,
        find: Option<String>,
    },
    /// A place in the PDF preview: the 0-based page and a rectangle in bp (top-left origin),
    /// which is flashed as SyncTeX does.
    Pdf {
        page: u32,
        rect: (f64, f64, f64, f64),
    },
    /// A file that is not text (an image): it opens in the system's default application after
    /// the user confirms.
    External { path: PathBuf },
}

/// What a resolver gets: the place ([`TextPoint`], through `Deref`) and what asked.
pub struct GotoCtx {
    pub at: TextPoint,
    pub trigger: Trigger,
}

impl Deref for GotoCtx {
    type Target = TextPoint;

    fn deref(&self) -> &TextPoint {
        &self.at
    }
}

/// A resolver (see the module docs).
pub type Resolver = dyn Fn(&GotoCtx) -> Option<GotoTarget>;

/// Identifies a resolver for [`unregister`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResolverId(u64);

/// A position the jump list can return to.
struct Jump {
    path: PathBuf,
    offset: usize,
    /// Follows edits while the document is open.
    mark: Option<gtk::TextMark>,
}

impl Jump {
    /// The current offset: the mark's while it is still in the document's buffer.
    fn offset(&self, state: &WindowState) -> usize {
        let open = state
            .workspace
            .get(&self.path)
            .and_then(|doc| Some((doc.buffer()?, doc)));
        match (&self.mark, open) {
            (Some(mark), Some((buffer, doc)))
                if !mark.is_deleted() && mark.buffer().as_ref() == Some(buffer.upcast_ref()) =>
            {
                doc.byte_offset(&buffer.iter_at_mark(mark))
            }
            _ => self.offset,
        }
    }

    fn release(self) {
        if let Some(mark) = self.mark
            && !mark.is_deleted()
            && let Some(buffer) = mark.buffer()
        {
            buffer.delete_mark(&mark);
        }
    }
}

type BackHandler = dyn Fn(&Rc<WindowState>) -> bool;
/// A test hook for `External` targets (see [`set_launch_hook`]).
pub type LaunchHook = dyn Fn(&Path) -> bool;
/// A list of jump-list positions: the file and the current byte offset (see [`jumps`]).
pub type JumpList = Vec<(PathBuf, usize)>;

/// The jump list of one window.
struct Nav {
    window: Weak<WindowState>,
    back: RefCell<Vec<Jump>>,
    forward: RefCell<Vec<Jump>>,
    back_handlers: Hooks<BackHandler>,
    launch_hook: RefCell<Option<Box<LaunchHook>>>,
}

/// The goto state of one view.
struct ViewGoto {
    view: glib::WeakRef<TexView>,
    resolvers: RefCell<Vec<(i32, ResolverId, Rc<Resolver>)>>,
    next_id: Cell<u64>,
    /// The underlined link (document bytes) while Ctrl is held over one.
    link: RefCell<Option<Range<usize>>>,
    controllers: RefCell<Vec<gtk::EventController>>,
    key_handler: Cell<Option<keys::HandlerId>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<Nav>>> = const { RefCell::new(Vec::new()) };
    static VIEWS: RefCell<Vec<Rc<ViewGoto>>> = const { RefCell::new(Vec::new()) };
}

fn nav(state: &WindowState) -> Option<Rc<Nav>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.window.strong_count() > 0);
        windows
            .iter()
            .find(|w| std::ptr::eq(w.window.as_ptr(), state))
            .cloned()
    })
}

fn lookup(view: &TexView) -> Option<Rc<ViewGoto>> {
    VIEWS.with_borrow(|views| {
        views
            .iter()
            .find(|v| v.view.upgrade().as_ref() == Some(view))
            .cloned()
    })
}

// ----- registry -----

/// Adds `resolver` to `view`'s resolvers with `priority` (higher runs first). `None` when the
/// view has no goto registry (the window gives every editor tab one, see [`attach`]).
pub fn register(
    view: &TexView,
    priority: i32,
    resolver: impl Fn(&GotoCtx) -> Option<GotoTarget> + 'static,
) -> Option<ResolverId> {
    let vg = lookup(view)?;
    let id = ResolverId(vg.next_id.get());
    vg.next_id.set(id.0 + 1);
    let mut resolvers = vg.resolvers.borrow_mut();
    let at = resolvers
        .iter()
        .position(|(p, _, _)| *p < priority)
        .unwrap_or(resolvers.len());
    resolvers.insert(at, (priority, id, Rc::new(resolver)));
    Some(id)
}

/// Removes a resolver added by [`register`].
pub fn unregister(view: &TexView, id: ResolverId) {
    if let Some(vg) = lookup(view) {
        vg.resolvers.borrow_mut().retain(|(_, i, _)| *i != id);
    }
}

/// Number of resolvers registered on `view`.
pub fn resolver_count(view: &TexView) -> usize {
    lookup(view).map_or(0, |vg| vg.resolvers.borrow().len())
}

/// Asks `view`'s resolvers about `ctx`, highest priority first; the first answer.
pub fn resolve(view: &TexView, ctx: &GotoCtx) -> Option<GotoTarget> {
    let vg = lookup(view)?;
    let resolvers: Vec<Rc<Resolver>> = vg
        .resolvers
        .borrow()
        .iter()
        .map(|(_, _, r)| r.clone())
        .collect();
    resolvers.iter().find_map(|r| r(ctx))
}

/// The target of the document byte `offset` of `editor` (exactly there).
pub fn resolve_at(editor: &Editor, offset: usize, trigger: Trigger) -> Option<GotoTarget> {
    let at = TextPoint::new(editor.document(), offset)?;
    resolve(editor.view(), &GotoCtx { at, trigger })
}

/// The target at `editor`'s cursor, else at the character before it (the end of a name typed
/// in insert mode).
pub fn resolve_at_cursor(editor: &Editor) -> Option<GotoTarget> {
    let cursor = editor.cursor_offset();
    resolve_at(editor, cursor, Trigger::Keyboard).or_else(|| {
        let before = super::hover::char_before(editor, cursor)?;
        resolve_at(editor, before, Trigger::Keyboard)
    })
}

// ----- actions -----

/// `win.goto-definition` (`gd` in Vim normal mode, F12): jumps to the target at the active
/// editor's cursor; returns false (after a toast) when there is none.
pub fn goto_definition(state: &Rc<WindowState>) -> bool {
    let Some(editor) = state.active_editor() else {
        return false;
    };
    let Some(target) = resolve_at_cursor(&editor) else {
        state.toast("Nothing to go to at the cursor");
        return false;
    };
    spawn_navigate(state, target);
    true
}

/// What a Ctrl+click on the document byte `offset` of `editor` does: jumps to its target and
/// returns true, or returns false when there is none (the click then is an ordinary click).
pub fn ctrl_click(state: &Rc<WindowState>, editor: &Editor, offset: usize) -> bool {
    let Some(target) = resolve_at(editor, offset, Trigger::Pointer) else {
        return false;
    };
    spawn_navigate(state, target);
    true
}

fn spawn_navigate(state: &Rc<WindowState>, target: GotoTarget) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        if let Err(e) = navigate(&state, target).await {
            state.toast(&e);
        }
    });
}

/// Goes to `target` and records the place the cursor left in the jump list (unless the jump
/// stays there). Other modules that move the cursor for the user (outline rows, search
/// results, reference lists) use it too, so Back returns from those jumps. Resolves when the
/// target is shown; an `External` target resolves when the user answered.
pub fn navigate(
    state: &Rc<WindowState>,
    target: GotoTarget,
) -> impl Future<Output = Result<(), String>> + use<> {
    let state = state.clone();
    async move {
        let from = current_jump(&state);
        let result = go(&state, &target).await;
        match (result, from) {
            (Ok(()), Some(from)) => {
                let here = current_position(&state);
                let moved = here.as_ref().is_none_or(|(path, offset)| {
                    *path != from.path || *offset != from.offset(&state)
                });
                if moved && let Some(nav) = nav(&state) {
                    push(&nav.back, from);
                    for jump in nav.forward.take() {
                        jump.release();
                    }
                } else {
                    from.release();
                }
                Ok(())
            }
            (result, from) => {
                if let Some(from) = from {
                    from.release();
                }
                result
            }
        }
    }
}

/// `win.goto-back` (Alt+Left; Ctrl+O in Vim normal mode): the back handlers first, then the
/// previous entry of the jump list. False (and the error bell) when there is nowhere to go.
pub fn back(state: &Rc<WindowState>) -> bool {
    if let Some(nav) = nav(state) {
        for handler in nav.back_handlers.snapshot() {
            if handler(state) {
                return true;
            }
        }
    }
    step(state, true)
}

/// `win.goto-forward` (Alt+Right): returns to where the last [`back`] came from.
pub fn forward(state: &Rc<WindowState>) -> bool {
    step(state, false)
}

fn step(state: &Rc<WindowState>, backward: bool) -> bool {
    let Some(nav) = nav(state) else {
        return false;
    };
    let (from, to) = if backward {
        (&nav.back, &nav.forward)
    } else {
        (&nav.forward, &nav.back)
    };
    let jump = from.borrow_mut().pop();
    let Some(jump) = jump else {
        if let Some(editor) = state.active_editor() {
            editor.view().error_bell();
        }
        return false;
    };
    if let Some(here) = current_jump(state) {
        push(to, here);
    }
    let offset = jump.offset(state);
    let path = jump.path.clone();
    jump.release();
    let state = state.clone();
    glib::spawn_future_local(async move {
        if let Err(e) = go(&state, &GotoTarget::File { path, offset }).await {
            state.toast(&e);
        }
    });
    true
}

fn push(list: &RefCell<Vec<Jump>>, jump: Jump) {
    let mut list = list.borrow_mut();
    list.push(jump);
    while list.len() > MAX_JUMPS {
        list.remove(0).release();
    }
}

/// True when [`back`] has an entry of the jump list to go to (back handlers aside).
pub fn can_go_back(state: &WindowState) -> bool {
    nav(state).is_some_and(|nav| !nav.back.borrow().is_empty())
}

/// True when [`forward`] has somewhere to go.
pub fn can_go_forward(state: &WindowState) -> bool {
    nav(state).is_some_and(|nav| !nav.forward.borrow().is_empty())
}

/// The jump list as (path, current offset) pairs, oldest first: back, then forward (tests).
pub fn jumps(state: &WindowState) -> (JumpList, JumpList) {
    let Some(nav) = nav(state) else {
        return (Vec::new(), Vec::new());
    };
    let list = |l: &RefCell<Vec<Jump>>| {
        l.borrow()
            .iter()
            .map(|j| (j.path.clone(), j.offset(state)))
            .collect()
    };
    (list(&nav.back), list(&nav.forward))
}

/// Runs `handler` first whenever Back is asked for (see the module docs); it returns true when
/// it went back itself. Remove it with [`remove_back_handler`].
pub fn add_back_handler(
    state: &WindowState,
    handler: impl Fn(&Rc<WindowState>) -> bool + 'static,
) -> Option<HookId> {
    Some(nav(state)?.back_handlers.add(Rc::new(handler)))
}

/// Removes a handler added by [`add_back_handler`].
pub fn remove_back_handler(state: &WindowState, id: HookId) {
    if let Some(nav) = nav(state) {
        nav.back_handlers.remove(id);
    }
}

/// Routes `External` targets to `hook` (with the file; it returns whether it "opened" it)
/// instead of the confirmation dialog and the system launcher (tests). `None` restores them.
pub fn set_launch_hook(state: &WindowState, hook: Option<Box<LaunchHook>>) {
    if let Some(nav) = nav(state) {
        nav.launch_hook.replace(hook);
    }
}

// ----- going places -----

/// The active editor's document and cursor offset.
fn current_position(state: &WindowState) -> Option<(PathBuf, usize)> {
    let editor = state.active_editor()?;
    Some((
        editor.document().path().to_path_buf(),
        editor.cursor_offset(),
    ))
}

/// The active editor's cursor as a jump-list entry (with a mark).
fn current_jump(state: &WindowState) -> Option<Jump> {
    let editor = state.active_editor()?;
    let buffer = editor.buffer();
    let iter = buffer.iter_at_mark(&buffer.get_insert());
    Some(Jump {
        path: editor.document().path().to_path_buf(),
        offset: editor.document().byte_offset(&iter),
        mark: Some(buffer.create_mark(None, &iter, true)),
    })
}

async fn go(state: &Rc<WindowState>, target: &GotoTarget) -> Result<(), String> {
    match target {
        GotoTarget::File { path, offset } => {
            let editor = state.open_file(path).await?;
            show_offset(&editor, *offset);
            Ok(())
        }
        GotoTarget::Line { path, line, find } => {
            let editor = state.open_file(path).await?;
            let offset = line_offset(&editor, *line, find.as_deref());
            show_offset(&editor, offset);
            Ok(())
        }
        GotoTarget::Pdf { page, rect } => {
            let pdf = state.pdf.get().ok_or("There is no PDF preview")?;
            if pdf.pdf().is_none() {
                return Err("There is no PDF to show yet; build the document first".into());
            }
            state.pdf_pane.set_visible(true);
            pdf.view().flash_boxes(*page, &[*rect], Reveal::Always);
            Ok(())
        }
        GotoTarget::External { path } => open_external(state, path).await,
    }
}

/// The offset of line `line` (clamped) of `editor`'s document: of the first `find` on it, else
/// of its first non-blank character.
fn line_offset(editor: &Editor, line: u32, find: Option<&str>) -> usize {
    let doc = editor.document();
    let line = line.min(doc.line_index().line_count().saturating_sub(1));
    let start = doc.line_index().line_start(line);
    let text = doc.line_text(line).unwrap_or_default();
    let column = find
        .filter(|f| !f.is_empty())
        .and_then(|f| text.find(f))
        .unwrap_or_else(|| text.len() - text.trim_start().len());
    start + column
}

/// Puts `editor`'s cursor at `offset`, scrolls it to the middle of the view (again after the
/// first frame of a new tab, whose first layout can leave it elsewhere) and focuses the view.
fn show_offset(editor: &Rc<Editor>, offset: usize) {
    editor.place_cursor_at(offset);
    let scroll = |editor: &Rc<Editor>| {
        let buffer = editor.buffer();
        editor
            .view()
            .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
    };
    scroll(editor);
    editor.after_first_frame(scroll);
    editor.view().grab_focus();
}

async fn open_external(state: &Rc<WindowState>, path: &Path) -> Result<(), String> {
    if let Some(nav) = nav(state)
        && let Some(hook) = nav.launch_hook.borrow().as_ref()
    {
        return if hook(path) {
            Ok(())
        } else {
            Err(format!("Could not open {}", path.display()))
        };
    }
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    if state.scripted {
        return Err("Other applications are not started from scripted runs".into());
    }
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    let dialog = adw::AlertDialog::new(
        Some(&format!("Open “{name}”?")),
        Some("It opens in the default application for this kind of file."),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("open", "_Open")]);
    dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("open"));
    dialog.set_close_response("cancel");
    if dialog.choose_future(Some(&state.window)).await != "open" {
        return Ok(());
    }
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    launcher
        .launch_future(Some(&state.window))
        .await
        .map_err(|e| format!("Could not open “{name}”: {e}"))
}

// ----- the pointer: Ctrl+click and the link underline -----

fn ctrl_only(state: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::ModifierType as M;
    state & (M::CONTROL_MASK | M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK | M::META_MASK)
        == M::CONTROL_MASK
}

/// What the view's motion controller does when the pointer moves to `(x, y)` with `ctrl` held
/// or not: underlines the link under it while Ctrl is held (tests call it).
pub fn pointer_moved(editor: &Editor, x: f64, y: f64, ctrl: bool) {
    let Some(vg) = lookup(editor.view()) else {
        return;
    };
    let offset = ctrl.then(|| editor.offset_at_point(x, y)).flatten();
    // Still on the underlined link: nothing to resolve again.
    if let Some(offset) = offset
        && vg
            .link
            .borrow()
            .as_ref()
            .is_some_and(|r| r.contains(&offset))
    {
        return;
    }
    let link = offset.and_then(|offset| {
        let at = TextPoint::new(editor.document(), offset)?;
        let range = at.anchor();
        let ctx = GotoCtx {
            at,
            trigger: Trigger::Pointer,
        };
        resolve(editor.view(), &ctx).map(|_| range)
    });
    set_link(&vg, editor, link);
}

/// The underlined link while Ctrl is held over one (tests).
pub fn link_range(view: &TexView) -> Option<Range<usize>> {
    lookup(view)?.link.borrow().clone()
}

fn set_link(vg: &ViewGoto, editor: &Editor, link: Option<Range<usize>>) {
    if *vg.link.borrow() == link {
        return;
    }
    let buffer = editor.buffer();
    let tag = link_tag(buffer);
    let old = vg.link.replace(link.clone());
    if let Some(old) = old
        && let (Some(a), Some(b)) = (editor.iter_near(old.start), editor.iter_near(old.end))
    {
        buffer.remove_tag(&tag, &a, &b);
    }
    match link.and_then(|r| Some((editor.iter_near(r.start)?, editor.iter_near(r.end)?))) {
        Some((a, b)) => {
            buffer.apply_tag(&tag, &a, &b);
            editor.view().set_cursor_from_name(Some("pointer"));
        }
        None => editor.view().set_cursor_from_name(Some("text")),
    }
}

fn link_tag(buffer: &sourceview5::Buffer) -> gtk::TextTag {
    let table = buffer.tag_table();
    if let Some(tag) = table.lookup(LINK_TAG) {
        return tag;
    }
    let tag = gtk::TextTag::builder()
        .name(LINK_TAG)
        .underline(pango::Underline::Single)
        .build();
    table.add(&tag);
    tag
}

// ----- window hooks -----

/// The window's jump list. Called once per window by the shell.
pub fn install(state: &Rc<WindowState>) {
    let nav = Rc::new(Nav {
        window: Rc::downgrade(state),
        back: RefCell::new(Vec::new()),
        forward: RefCell::new(Vec::new()),
        back_handlers: Hooks::default(),
        launch_hook: RefCell::new(None),
    });
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.window.strong_count() > 0);
        windows.push(nav);
    });
}

/// Gives `editor`'s view its goto registry: Ctrl+click, the Ctrl+hover underline and Ctrl+O in
/// Vim normal mode; then registers WP3.2's resolvers ([`attach_resolvers`]). Called by the
/// window for every editor tab, before the other M3 per-editor hooks.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    if lookup(view).is_some() {
        return;
    }
    let vg = Rc::new(ViewGoto {
        view: view.downgrade(),
        resolvers: RefCell::new(Vec::new()),
        next_id: Cell::new(1),
        link: RefCell::new(None),
        controllers: RefCell::new(Vec::new()),
        key_handler: Cell::new(None),
    });
    VIEWS.with_borrow_mut(|views| {
        views.retain(|v| v.view.upgrade().is_some());
        views.push(vg.clone());
    });

    // Capture phase: a Ctrl+click on a link is claimed before the view starts a selection;
    // any other click goes on to the view.
    let click = gtk::GestureClick::new();
    click.set_name(Some("lemma-goto-click"));
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (st, ed) = (Rc::downgrade(state), Rc::downgrade(editor));
    click.connect_pressed(move |gesture, n_press, x, y| {
        let (Some(state), Some(editor)) = (st.upgrade(), ed.upgrade()) else {
            return;
        };
        if n_press != 1 || !ctrl_only(gesture.current_event_state()) {
            return;
        }
        let Some(offset) = editor.offset_at_point(x, y) else {
            return;
        };
        let Some(target) = resolve_at(&editor, offset, Trigger::Pointer) else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if let Some(vg) = lookup(editor.view()) {
            set_link(&vg, &editor, None);
        }
        // After this click's dispatch: the jump may switch tabs.
        let state = state.clone();
        glib::idle_add_local_once(move || spawn_navigate(&state, target));
    });
    view.add_controller(click.clone());

    let motion = gtk::EventControllerMotion::new();
    motion.set_name(Some("lemma-goto-motion"));
    let ed = Rc::downgrade(editor);
    motion.connect_motion(move |controller, x, y| {
        if let Some(editor) = ed.upgrade() {
            let ctrl = ctrl_only(controller.current_event_state());
            // Cheap without Ctrl: nothing is resolved, and an underline is removed once.
            pointer_moved(&editor, x, y, ctrl);
        }
    });
    let ed = Rc::downgrade(editor);
    motion.connect_leave(move |_| {
        if let Some(editor) = ed.upgrade()
            && let Some(vg) = lookup(editor.view())
        {
            set_link(&vg, &editor, None);
        }
    });
    view.add_controller(motion.clone());
    vg.controllers
        .replace(vec![click.upcast(), motion.upcast()]);

    let (st, ed) = (Rc::downgrade(state), Rc::downgrade(editor));
    let handler = keys::register(view, PRIORITY_BACK_KEY, move |event, mode| {
        let (Some(state), Some(editor)) = (st.upgrade(), ed.upgrade()) else {
            return glib::Propagation::Proceed;
        };
        if !is_vim_back_key(event, mode) || !can_go_back(&state) {
            return glib::Propagation::Proceed;
        }
        // Vim's own Ctrl+O when something is pending (`2<C-o>` is Vim's).
        if editor
            .vim_status()
            .is_some_and(|status| !status.command.is_empty())
        {
            return glib::Propagation::Proceed;
        }
        // Outside the key's dispatch: going back may switch tabs.
        let state = Rc::downgrade(&state);
        glib::idle_add_local_full(glib::Priority::HIGH, move || {
            if let Some(state) = state.upgrade() {
                back(&state);
            }
            glib::ControlFlow::Break
        });
        glib::Propagation::Stop
    });
    vg.key_handler.set(handler);

    attach_resolvers(state, editor);
}

/// Ctrl+O in Vim normal mode.
fn is_vim_back_key(event: &keys::KeyEvent, mode: VimMode) -> bool {
    event.vim
        && mode == VimMode::Normal
        && ctrl_only(event.state)
        && matches!(event.keyval, gtk::gdk::Key::o | gtk::gdk::Key::O)
}

/// Removes everything [`attach`] added to `editor`'s view. Called by the window when the tab
/// closes, after the other M3 per-editor hooks.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    let Some(vg) = lookup(view) else {
        return;
    };
    VIEWS.with_borrow_mut(|views| views.retain(|v| !Rc::ptr_eq(v, &vg)));
    set_link(&vg, editor, None);
    vg.resolvers.borrow_mut().clear();
    for controller in vg.controllers.take() {
        view.remove_controller(&controller);
    }
    if let Some(id) = vg.key_handler.take() {
        keys::unregister(view, id);
    }
}

// ----- resolvers (WP3.2) -----

/// Registers WP3.2's resolvers on `editor`'s view (called at the end of [`attach`]):
///
/// - `\ref`, `\eqref`, `\cref` and the rest of the family (ranges too) → the `\label`
///   (`GotoTarget::Line` with `find: Some("\\label{name}")`);
/// - `\cite` keys → the `.bib` entry (opened read-write like any file);
/// - `\input`, `\include`, `\subfile` → the file; `\includegraphics` → `GotoTarget::External`
///   (the system viewer, after confirmation);
/// - `\usepackage` of a local `.sty` → that file;
/// - a user macro → its `\newcommand` / `\DeclareMathOperator`.
///
/// Use [`PRIORITY_REFS`]. The resolution itself is `labels::resolve::goto_target`.
fn attach_resolvers(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let weak = Rc::downgrade(state);
    let id = register(editor.view(), PRIORITY_REFS, move |ctx| {
        let state = weak.upgrade()?;
        crate::labels::resolve::goto_target(&state, ctx)
    });
    crate::labels::remember_tools(editor.view(), None, id);
}
