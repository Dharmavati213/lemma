//! Rename label (`win.rename-label`, F2 on a `\label` or a reference): a popover at the name
//! asks for the new name and checks it as you type; Enter renames the label and every
//! reference to it across the project **as reviewable hunks** (`review_ui::begin_named_turn`, the
//! origin "Rename old → new" in the banner and the Changes panel), so each change can be
//! accepted or rejected — or all at once, Reject restoring every file.

use super::scan::{self, NameKind};
use super::{Closing, name_at_cursor, schedule_check};
use crate::document::DocRef;
use crate::editor::Editor;
use crate::review_ui;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::Origin;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

/// What a rename did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameOutcome {
    /// Names replaced (the `\label` and the references).
    pub places: usize,
    /// Files changed.
    pub files: usize,
    /// The review turn its hunks belong to.
    pub turn: u32,
}

/// The open rename popover of one window.
struct Session {
    editor: Weak<Editor>,
    old: String,
    popover: gtk::Popover,
    entry: gtk::Entry,
    status: gtk::Label,
    /// "7 places in 2 files", once known.
    count: RefCell<Option<String>>,
    closed: Cell<bool>,
    unrealize: RefCell<Option<glib::SignalHandlerId>>,
}

thread_local! {
    static SESSIONS: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Session>)>> =
        const { RefCell::new(Vec::new()) };
}

fn current(state: &WindowState) -> Option<Rc<Session>> {
    SESSIONS.with_borrow_mut(|all| {
        all.retain(|(w, _)| w.upgrade().is_some());
        all.iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .map(|(_, s)| s.clone())
    })
}

fn set_current(state: &WindowState, session: Option<Rc<Session>>) {
    SESSIONS.with_borrow_mut(|all| {
        all.retain(|(w, _)| w.upgrade().is_some_and(|w| w != state.window));
        if let Some(session) = session {
            all.push((state.window.downgrade(), session));
        }
    });
}

/// `win.rename-label` (F2): the rename popover for the label at the active editor's cursor (a
/// toast when the cursor is not on a label or reference).
pub fn rename_label(state: &Rc<WindowState>) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    match name_at_cursor(&editor) {
        Some((NameKind::Label, range, name)) => {
            if name.contains([',', '{', '}', '\\', '#']) {
                state.toast(&format!("‘{name}’ cannot be renamed safely"));
                return;
            }
            open(state, &editor, range, &name);
        }
        Some((NameKind::Cite, ..)) => state.toast("Only labels can be renamed"),
        None => state.toast("Put the cursor on a \\label or a reference to rename it"),
    }
}

/// Why `new` cannot replace the label `old` in `state`'s project (`None`: it can).
pub fn problem(state: &WindowState, old: &str, new: &str) -> Option<String> {
    if new == old {
        return Some("Type a new name".to_owned());
    }
    if let Some(problem) = scan::label_name_problem(new) {
        return Some(problem.to_owned());
    }
    let index = state.workspace.index.borrow();
    if let Some((path, _, label)) = scan::find_label(&index, new) {
        return Some(format!(
            "‘{new}’ is already a label ({}:{})",
            super::display_path(state, path),
            label.line + 1
        ));
    }
    // (A name only the `.aux` file still has was removed since the last build: it is free.)
    None
}

fn open(state: &Rc<WindowState>, editor: &Rc<Editor>, range: Range<usize>, old: &str) {
    close_rename(state);
    super::references::close_references(state);
    let heading = gtk::Label::builder()
        .label("Rename Label")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let entry = gtk::Entry::builder()
        .text(old)
        .width_chars(32)
        .activates_default(false)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New label name")]);
    let status = gtk::Label::builder()
        .label("Looking for its references…")
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(42)
        .css_classes(["caption", "dim-label", "lemma-rename-hint"])
        .build();
    let hint = gtk::Label::builder()
        .label("Enter renames as changes to review · Esc cancels")
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&heading);
    content.append(&entry);
    content.append(&status);
    content.append(&hint);
    let popover = gtk::Popover::builder()
        .child(&content)
        .position(gtk::PositionType::Bottom)
        .autohide(true)
        .has_arrow(true)
        .build();
    popover.add_css_class("lemma-rename");
    let view = editor.view();
    view.adopt_popover(&popover);
    if let Some(rect) = editor.range_rect(range.clone(), range.start) {
        let x = rect.x().clamp(0, (view.width() - 1).max(0));
        let y = rect.y().clamp(0, (view.height() - 1).max(0));
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            x,
            y,
            rect.width().max(1),
            rect.height().max(1),
        )));
    }
    let session = Rc::new(Session {
        editor: Rc::downgrade(editor),
        old: old.to_owned(),
        popover: popover.clone(),
        entry: entry.clone(),
        status: status.clone(),
        count: RefCell::new(None),
        closed: Cell::new(false),
        unrealize: RefCell::new(None),
    });
    set_current(state, Some(session.clone()));

    // How many places the rename changes, once known.
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    let old_name = old.to_owned();
    glib::spawn_future_local(async move {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let rows = super::references::references(&state, NameKind::Label, &old_name).await;
        let Some(session) = ses.upgrade() else {
            return;
        };
        if session.closed.get() {
            return;
        }
        let files: BTreeSet<&PathBuf> = rows.iter().map(|r| &r.path).collect();
        let text = match (rows.len(), files.len()) {
            (0, _) => "Not found in the project's files".to_owned(),
            (1, _) => "1 place in 1 file".to_owned(),
            (n, 1) => format!("{n} places in 1 file"),
            (n, f) => format!("{n} places in {f} files"),
        };
        session.count.replace(Some(text));
        validate(&state, &session);
    });

    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    entry.connect_changed(move |_| {
        if let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) {
            validate(&state, &session);
        }
    });
    let weak = Rc::downgrade(state);
    entry.connect_activate(move |entry| {
        if let Some(state) = weak.upgrade() {
            submit_rename(&state, &entry.text());
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(state);
    keys.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk::gdk::Key::Escape
            && let Some(state) = weak.upgrade()
            && let Some(session) = current(&state)
        {
            close(&state, &session, Closing::Later);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    popover.add_controller(keys);
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    popover.connect_closed(move |_| {
        if let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) {
            close(&state, &session, Closing::Later);
        }
    });
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&session));
    let handler = view.connect_unrealize(move |_| {
        if let (Some(state), Some(session)) = (weak.upgrade(), ses.upgrade()) {
            close(&state, &session, Closing::TabGoes);
        }
    });
    session.unrealize.replace(Some(handler));
    popover.popup();
    entry.grab_focus();
    // `prefix:name` labels: the part after the prefix is selected, so typing replaces it.
    let start = old.rfind(':').map_or(0, |i| old[..=i].chars().count());
    entry.select_region(start as i32, -1);
}

/// Shows why the typed name cannot be used (or the count of places again).
fn validate(state: &WindowState, session: &Session) {
    let new = session.entry.text();
    match problem(state, &session.old, new.trim()) {
        Some(problem) if new.trim() != session.old => {
            session.entry.add_css_class("error");
            session.status.remove_css_class("dim-label");
            session.status.add_css_class("error");
            session.status.set_text(&problem);
        }
        _ => {
            session.entry.remove_css_class("error");
            session.status.remove_css_class("error");
            session.status.add_css_class("dim-label");
            let count = session.count.borrow().clone();
            session
                .status
                .set_text(count.as_deref().unwrap_or("Looking for its references…"));
        }
    }
}

/// What Enter in the rename popover does: renames the label to `new` (after checking it),
/// closing the popover; false (the popover stays, showing why) when the name cannot be used.
pub fn submit_rename(state: &Rc<WindowState>, new: &str) -> bool {
    let Some(session) = current(state) else {
        return false;
    };
    let new = new.trim().to_owned();
    if let Some(problem) = problem(state, &session.old, &new) {
        session.entry.add_css_class("error");
        session.status.remove_css_class("dim-label");
        session.status.add_css_class("error");
        session.status.set_text(&problem);
        session.entry.error_bell();
        return false;
    }
    let old = session.old.clone();
    close(state, &session, Closing::Later);
    let state = state.clone();
    glib::spawn_future_local(async move {
        match rename(&state, &old, &new).await {
            Ok(outcome) if outcome.places == 0 => {
                state.toast(&format!("‘{old}’ is used nowhere in the project"));
            }
            Ok(_) => {}
            Err(e) => state.toast(&e),
        }
    });
    true
}

/// Renames the label `old` to `new` in every file of the project: the changes are applied as
/// reviewable hunks attributed to "Rename old → new", and the files that had no unsaved changes
/// are saved (the others keep their unsaved state). Resolves to what changed.
pub fn rename(
    state: &Rc<WindowState>,
    old: &str,
    new: &str,
) -> impl std::future::Future<Output = Result<RenameOutcome, String>> + use<> {
    let state = state.clone();
    let (old, new) = (old.to_owned(), new.to_owned());
    async move {
        if let Some(problem) = problem(&state, &old, &new) {
            return Err(problem);
        }
        // Which files mention it at all (read on a worker thread for the files Lemma does not
        // have), then those are loaded as documents.
        let rows = super::references::references(&state, NameKind::Label, &old).await;
        let paths: BTreeSet<PathBuf> = rows.into_iter().map(|r| r.path).collect();
        let mut docs: Vec<DocRef> = Vec::new();
        for path in paths {
            match state.workspace.get(&path) {
                Some(doc) => docs.push(doc),
                None => match state.workspace.get_or_load(&path).await {
                    Ok(doc) => docs.push(doc),
                    Err(e) => return Err(format!("Cannot rename in {}: {e}", path.display())),
                },
            }
        }
        // Found and applied in one go, so the texts cannot change in between.
        let config = state.workspace.scan_config();
        let mut changes: Vec<(DocRef, Vec<lemma_core::TextEdit>)> = Vec::new();
        let mut places = 0;
        for doc in &docs {
            let occurrences = occurrences_in(doc, &old, &config);
            places += occurrences.len();
            if !occurrences.is_empty() {
                changes.push((doc.clone(), scan::rename_edits(&occurrences, &new)));
            }
        }
        // Documents loaded only to look (nothing to rename after all) leave the workspace.
        for doc in &docs {
            if !doc.is_open()
                && doc.review.borrow().is_none()
                && !changes.iter().any(|(d, _)| Rc::ptr_eq(d, doc))
            {
                state.workspace.release(doc);
            }
        }
        if changes.is_empty() {
            return Ok(RenameOutcome {
                places: 0,
                files: 0,
                turn: 0,
            });
        }
        let clean: Vec<DocRef> = changes
            .iter()
            .filter(|(doc, _)| !doc.is_dirty())
            .map(|(doc, _)| doc.clone())
            .collect();
        // Reviewed like an agent turn of its own, named in the banner and the Changes panel.
        let turn = review_ui::begin_named_turn(&state, &format!("Rename {old} → {new}"));
        let mut failed = Vec::new();
        for (doc, edits) in &changes {
            review_ui::start_review(doc, false);
            let applied = review_ui::in_turn(doc, turn, || {
                doc.apply_edits(edits, Origin::Agent { call: 0 })
            });
            match applied {
                Ok(()) => {
                    review_ui::recompute(doc);
                }
                Err(e) => failed.push(format!("{}: {e}", doc.display_name())),
            }
        }
        if !failed.is_empty() {
            return Err(format!("Could not rename in {}", failed.join("; ")));
        }
        let files = changes.len();
        let mut failures = Vec::new();
        for doc in clean {
            if let Err(e) = doc.save().await {
                failures.push(format!("{}: {e}", doc.display_name()));
            }
        }
        schedule_check(&state, super::CHECK_DELAY);
        if !failures.is_empty() {
            state.toast(&format!(
                "Renamed, but saving failed: {}",
                failures.join("; ")
            ));
        }
        Ok(RenameOutcome {
            places,
            files,
            turn,
        })
    }
}

/// The places of the label `name` in `doc`'s current text.
fn occurrences_in(
    doc: &DocRef,
    name: &str,
    config: &std::sync::Arc<lemma_syntax::ScanConfig>,
) -> Vec<scan::Occurrence> {
    doc.with_text(|text| {
        let states = doc.line_states.borrow();
        match states.as_ref() {
            Some(states) if doc.has_line_states() => scan::find_occurrences(
                text,
                &doc.line_index(),
                |line, line_text, out| states.tokens(line, line_text, out),
                name,
                NameKind::Label,
            ),
            _ => scan::scan_occurrences(text, config.clone(), name, NameKind::Label),
        }
    })
}

fn close(state: &WindowState, session: &Rc<Session>, how: Closing) {
    if session.closed.replace(true) {
        return;
    }
    if current(state).is_some_and(|s| Rc::ptr_eq(&s, session)) {
        set_current(state, None);
    }
    let editor = session.editor.upgrade();
    if let (Some(editor), Some(handler)) = (&editor, session.unrealize.take()) {
        editor.view().disconnect(handler);
    }
    super::close_popover(
        &session.popover,
        editor
            .as_ref()
            .map(|e| e.view().upcast_ref::<gtk::Widget>()),
        how,
    );
}

/// Closes the rename popover of `state`'s window, if one is open.
pub fn close_rename(state: &WindowState) {
    if let Some(session) = current(state) {
        close(state, &session, Closing::Now);
    }
}

/// Closes the popover when it is shown in `editor` (its tab closes).
pub(super) fn close_for(state: &WindowState, editor: &Rc<Editor>) {
    if let Some(session) = current(state)
        && session
            .editor
            .upgrade()
            .is_none_or(|e| Rc::ptr_eq(&e, editor))
    {
        close(state, &session, Closing::TabGoes);
    }
}

/// The open rename popover (tests).
pub fn rename_popover(state: &WindowState) -> Option<gtk::Popover> {
    current(state).map(|s| s.popover.clone())
}

/// The entry of the open rename popover (tests).
pub fn rename_entry(state: &WindowState) -> Option<gtk::Entry> {
    current(state).map(|s| s.entry.clone())
}

/// The status line of the open rename popover: the count of places, or why the name cannot be
/// used (tests).
pub fn rename_status(state: &WindowState) -> Option<String> {
    current(state).map(|s| s.status.text().to_string())
}
