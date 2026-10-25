//! New File, Save As, and writing a copy (Vim's `:w path`).
//!
//! - **New File** (`win.new-file`, Ctrl+N, the main menu): a save dialog in the active
//!   document's folder (else the project root), with the name `untitled.tex`; the file chosen
//!   opens in a tab ([`new_file`]). A file that does not exist yet is an empty document that its
//!   first save creates (`Workspace::open`). One that exists opens as it is, with a toast that
//!   says so: the dialog's "Replace" never empties a file.
//! - **Save As** (`win.save-as`, Ctrl+Shift+S, the main menu; `:sav[eas] path` in Vim): writes
//!   the active document's text to another file, in the document's encoding and with its line
//!   breaks, and shows that file in the old one's tab place, the cursor where it was
//!   ([`save_as`]). The old document closes and its unsaved changes are dropped, so its file
//!   stays as it is on disk. The undo history does not come along: the new document starts a
//!   new one. The dialog asks before replacing a file; `:sav` refuses a file that exists, and
//!   `:sav!` replaces it.
//! - **A copy** (`:w path`, [`write_copy`]): the text goes to `path` as Save As writes it, and
//!   the document stays the file it was, unsaved changes and all (Vim's `:w {file}`). A file
//!   that exists is replaced only by `:w! path`. `:wq path` writes the copy and then closes the
//!   tab as `:q` does (`:wq! path` as `:q!`).
//!
//! The new file is written by a document of its own (`Workspace::insert_closed` and its saver),
//! so the write counts as Lemma's own and the file monitor does not take it for another
//! program's change; Save As keeps that document and opens it. A file that a document of the
//! workspace holds (another tab, a closed document under review or with unsaved changes) is
//! refused, so that its unsaved changes and its review are not written over; a closed document
//! that holds only what is on disk leaves the workspace first.
//!
//! Neither runs while the agent is busy with a turn (its edits go into the documents), and Save
//! As not while the document has agent changes that are not decided (they would stay behind
//! with the old file). A copy is not written in the seconds after a turn either, while the file
//! monitor still counts a new project file as the agent's (`agent::bridge::in_agent_window`).
//!
//! Hooks: [`new_file_dialog`] and [`save_as_dialog`] are the actions; the window's Vim handlers
//! call [`save_as`] and [`write_copy`]. Tests answer the file dialog with [`set_dialog_hook`].

use crate::agent::{bridge, panel};
use crate::document::DocRef;
use crate::editor::Editor;
use crate::saver::{DiskState, SaveError};
use crate::window::WindowState;
use crate::workspace::normalize;
use adw::prelude::*;
use lemma_core::Origin;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The name New File suggests.
pub const NEW_FILE_NAME: &str = "untitled.tex";

/// What a file dialog asks (see [`set_dialog_hook`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveDialog {
    /// "New File" or "Save As".
    pub title: &'static str,
    /// The folder it opens in.
    pub folder: PathBuf,
    /// The file name it suggests.
    pub name: String,
}

/// Answers a file dialog instead of the user (tests): the path chosen, `None` for Cancel.
pub type DialogHook = dyn Fn(&SaveDialog) -> Option<PathBuf>;

thread_local! {
    static DIALOG_HOOK: RefCell<Option<Rc<DialogHook>>> = const { RefCell::new(None) };
}

/// Makes New File and Save As ask `hook` instead of showing the file dialog (tests); `None`
/// shows the dialog again.
pub fn set_dialog_hook(hook: Option<Box<DialogHook>>) {
    DIALOG_HOOK.with_borrow_mut(|h| *h = hook.map(Rc::from));
}

/// Asks for a file to write with a save dialog (or the hook); `None` when cancelled, and in
/// scripted runs without a hook (nobody could answer).
async fn ask(state: &Rc<WindowState>, question: SaveDialog, accept: &str) -> Option<PathBuf> {
    if let Some(hook) = DIALOG_HOOK.with_borrow(Clone::clone) {
        return hook(&question);
    }
    if state.scripted {
        tracing::info!("no {} dialog in a scripted run", question.title);
        return None;
    }
    let dialog = gtk::FileDialog::builder()
        .title(question.title)
        .accept_label(accept)
        .modal(true)
        .initial_folder(&gio::File::for_path(&question.folder))
        .initial_name(question.name.as_str())
        .build();
    // Dismissed or cancelled: nothing to do.
    let file = dialog.save_future(Some(&state.window)).await.ok()?;
    let path = file.path();
    if path.is_none() {
        state.toast("Only local files can be written");
    }
    path
}

/// The folder of the active document, else the project root.
fn active_folder(state: &WindowState) -> PathBuf {
    state
        .active_editor()
        .and_then(|e| e.document().path().parent().map(Path::to_path_buf))
        .unwrap_or_else(|| state.workspace.root().path().to_path_buf())
}

/// `path` for messages: relative to the project root when inside it.
fn display(state: &WindowState, path: &Path) -> String {
    let root = state.workspace.root();
    root.relative(path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// ───────────── New File ─────────────

/// `win.new-file` (Ctrl+N): asks for the new file's name and folder, then opens it
/// ([`new_file`]).
pub fn new_file_dialog(state: &Rc<WindowState>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let question = SaveDialog {
            title: "New File",
            folder: active_folder(&state),
            name: NEW_FILE_NAME.to_owned(),
        };
        let Some(path) = ask(&state, question, "_Create").await else {
            return;
        };
        if let Err(e) = new_file(&state, &path).await {
            state.toast(&e);
        }
    });
}

/// Opens `path` in a tab, for New File: a file that does not exist yet is an empty document
/// that its first save creates; one that exists opens as it is, with a toast saying so.
pub async fn new_file(state: &Rc<WindowState>, path: &Path) -> Result<Rc<Editor>, String> {
    let exists = gio::File::for_path(path)
        .query_info_future(
            gio::FILE_ATTRIBUTE_STANDARD_TYPE,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await
        .is_ok();
    let editor = state.open_file(path).await?;
    if exists {
        state.toast(&format!(
            "“{}” exists already; it is opened as it is",
            display(state, editor.document().path())
        ));
    }
    Ok(editor)
}

// ───────────── Save As and copies ─────────────

/// Why nothing may be written for the user now because of the agent, if so: while it is busy
/// with a turn, and for a `copy` also in the seconds after one (see the module docs).
fn agent_refusal(state: &WindowState, copy: bool) -> Option<String> {
    let controller = state.agent_controller()?;
    let who = || bridge::agent_label(state.config.agent.backend, &state.config.agent);
    if panel::is_busy(controller) {
        return Some(format!(
            "{} is still working; wait for it or stop it first",
            who()
        ));
    }
    (copy && bridge::in_agent_window(controller)).then(|| {
        format!(
            "{}'s turn has just ended, and a new file would count as its change; write it \
             again in a few seconds",
            who()
        )
    })
}

/// Why `doc` cannot be saved under another name now, if so: the agent is busy, or it has agent
/// changes that are not decided.
fn save_as_refusal(state: &WindowState, doc: &DocRef) -> Option<String> {
    agent_refusal(state, false).or_else(|| {
        let pending = doc.review.borrow().as_ref().is_some_and(|r| !r.is_clean());
        pending.then(|| {
            format!(
                "“{}” has agent changes that are not decided; accept or reject them first",
                doc.display_name()
            )
        })
    })
}

/// Why `target` (normalized) cannot be written: a document of the workspace holds it in a tab,
/// under review, or with unsaved changes. A closed document that holds only what its file does
/// (one a project-wide replace or a citation left behind) leaves the workspace instead; it is
/// read again when needed.
fn target_refusal(state: &WindowState, target: &Path) -> Option<String> {
    let held = state.workspace.get(target)?;
    let name = display(state, target);
    if held.is_open() {
        return Some(format!(
            "“{name}” is open in another tab; save it there, or close it first"
        ));
    }
    if held.review.borrow().is_some() {
        return Some(format!(
            "“{name}” has agent changes to review; decide them first"
        ));
    }
    if held.is_dirty() {
        return Some(format!(
            "“{name}” has changes that are not saved yet; Save All writes them"
        ));
    }
    state.workspace.release(&held);
    None
}

/// Writes `doc`'s text to `target` (normalized) as a new document of the workspace, in `doc`'s
/// charset and with its line breaks, as Lemma's own write (see the module docs). Without
/// `overwrite` a file that exists is refused. On failure the new document leaves the workspace
/// again.
async fn write_new(
    state: &WindowState,
    doc: &DocRef,
    target: &Path,
    overwrite: bool,
) -> Result<DocRef, String> {
    if let Some(why) = target_refusal(state, target) {
        return Err(why);
    }
    let copy = state.workspace.insert_closed(target, doc.text());
    // Absent: a save refuses a file that exists (unless it overwrites).
    copy.saver.set_loaded(doc.charset(), DiskState::Absent);
    let saved = if overwrite {
        copy.save_overwriting().await
    } else {
        copy.save().await
    };
    let Err(e) = saved else {
        return Ok(copy);
    };
    if state
        .workspace
        .get(target)
        .is_some_and(|d| Rc::ptr_eq(&d, &copy))
    {
        state.workspace.release(&copy);
    }
    let name = display(state, target);
    Err(match e {
        SaveError::ExternallyModified => format!("“{name}” exists; add ! to write over it"),
        SaveError::Io(e) => format!("Could not write “{name}”: {e}"),
    })
}

/// `win.save-as` (Ctrl+Shift+S): asks where to save the active document, then saves it there
/// ([`save_as`]; the dialog asks before replacing a file).
pub fn save_as_dialog(state: &Rc<WindowState>) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    // Refused before the dialog, not after the user chose a name.
    if let Some(why) = save_as_refusal(state, editor.document()) {
        state.toast(&why);
        return;
    }
    let state = state.clone();
    glib::spawn_future_local(async move {
        let doc = editor.document();
        let question = SaveDialog {
            title: "Save As",
            folder: doc
                .path()
                .parent()
                .map_or_else(|| active_folder(&state), Path::to_path_buf),
            name: doc.display_name(),
        };
        let Some(path) = ask(&state, question, "_Save").await else {
            return;
        };
        if let Err(e) = save_as(&state, &editor, &path, true).await {
            state.toast(&e);
        }
    });
}

/// Save As: writes `editor`'s text to `path` and shows that file in the editor's tab place,
/// closing the old document without its unsaved changes (see the module docs). Without
/// `overwrite` a file that exists is refused (`:sav`, where `:sav!` overwrites). Its own path
/// is a plain save.
pub async fn save_as(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
    path: &Path,
    overwrite: bool,
) -> Result<(), String> {
    let doc = editor.document().clone();
    let target = normalize(path);
    if target == doc.path() {
        state.save_document(&doc);
        return Ok(());
    }
    if let Some(why) = save_as_refusal(state, &doc) {
        return Err(why);
    }
    let version = doc.version();
    write_new(state, &doc, &target, overwrite).await?;
    let new = state.open_file(&target).await?;
    // Typed while the file was written: that comes along too, unsaved.
    if doc.version() != version {
        let edits = new
            .document()
            .with_text(|written| doc.with_text(|now| lemma_review::minimal_edits(written, now)));
        new.document()
            .apply_edits(&edits, Origin::User)
            .map_err(|e| format!("Could not change the text: {e}"))?;
    }
    new.place_cursor_at(editor.cursor_offset());
    new.after_first_frame(|editor| {
        let buffer = editor.buffer();
        editor
            .view()
            .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
    });
    state.replace_tab(editor, &new);
    Ok(())
}

/// `:w path`: writes `doc`'s text to `path` and leaves the document as it is (see the module
/// docs); a file that exists only with `overwrite` (`:w!`). Its own path is a plain save.
pub async fn write_copy(
    state: &Rc<WindowState>,
    doc: &DocRef,
    path: &Path,
    overwrite: bool,
) -> Result<(), String> {
    let target = normalize(path);
    if target == doc.path() {
        state.save_document(doc);
        return Ok(());
    }
    if let Some(why) = agent_refusal(state, true) {
        return Err(why);
    }
    let copy = write_new(state, doc, &target, overwrite).await?;
    // Nothing shows the copy: its document leaves the workspace again.
    if state
        .workspace
        .get(&target)
        .is_some_and(|d| Rc::ptr_eq(&d, &copy))
    {
        state.workspace.release(&copy);
    }
    state.toast(&format!("Wrote “{}”", display(state, &target)));
    Ok(())
}
