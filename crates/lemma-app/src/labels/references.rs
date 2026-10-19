//! Find references (`win.find-references`, Shift+F12): every use of the label or citation key
//! at the cursor across the project, listed in a popover at the name; Enter or a click jumps
//! there through `editor::goto::navigate`, so Back (Alt+Left, Ctrl+O in Vim) returns.

use super::scan::{self, NameKind};
use super::{Closing, display_path, name_at_cursor, project_paths};
use crate::editor::Editor;
use crate::editor::goto::{self, GotoTarget};
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Largest file searched (bytes).
const MAX_FILE: u64 = 8 << 20;
/// Longest line text shown in a row, in characters.
const MAX_ROW_CHARS: usize = 80;
/// The list's height before it scrolls.
const MAX_LIST_HEIGHT: i32 = 360;

/// One use of a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceRow {
    pub path: PathBuf,
    /// 0-based line.
    pub line: u32,
    /// Byte offset of the name in the file's text.
    pub offset: usize,
    /// The line's text (blanks trimmed).
    pub text: String,
    /// Byte offset of the name in `text`.
    pub column: usize,
    /// Where the name is defined (the `\label`, the bibliography entry), not a use.
    pub definition: bool,
}

/// The open list of one window.
struct Shown {
    editor: Weak<Editor>,
    popover: gtk::Popover,
    rows: Vec<ReferenceRow>,
    /// The list rows of `rows` (header rows are not in it).
    list_rows: Vec<gtk::ListBoxRow>,
    closed: Cell<bool>,
    unrealize: RefCell<Option<glib::SignalHandlerId>>,
}

thread_local! {
    static SHOWN: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Shown>)>> =
        const { RefCell::new(Vec::new()) };
}

fn shown(state: &WindowState) -> Option<Rc<Shown>> {
    SHOWN.with_borrow_mut(|all| {
        all.retain(|(w, _)| w.upgrade().is_some());
        all.iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .map(|(_, s)| s.clone())
    })
}

fn set_shown(state: &WindowState, value: Option<Rc<Shown>>) {
    SHOWN.with_borrow_mut(|all| {
        all.retain(|(w, _)| w.upgrade().is_some_and(|w| w != state.window));
        if let Some(value) = value {
            all.push((state.window.downgrade(), value));
        }
    });
}

/// `win.find-references` (Shift+F12): the uses of the label or citation key at the active
/// editor's cursor, in a popover (a toast when there is no name there or it is used nowhere).
pub fn find_references(state: &Rc<WindowState>) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    let Some((kind, range, name)) = name_at_cursor(&editor) else {
        state.toast("Put the cursor on a label, a reference or a citation key");
        return;
    };
    close_references(state);
    let state = state.clone();
    let weak = Rc::downgrade(&editor);
    glib::spawn_future_local(async move {
        let rows = references(&state, kind, &name).await;
        let Some(editor) = weak.upgrade() else {
            return;
        };
        let active = state
            .active_editor()
            .is_some_and(|e| Rc::ptr_eq(&e, &editor));
        if !active {
            return;
        }
        if rows.is_empty() {
            state.toast(&format!("‘{name}’ is used nowhere in the project"));
            return;
        }
        show(&state, &editor, range, kind, &name, rows);
    });
}

/// Every place of the project where `name` occurs as a `kind` (label definitions, references,
/// citations, bibliography entries): the file of the active editor first, then by path; each
/// file by position. Documents Lemma has are searched in their live text, the others on disk.
pub fn references(
    state: &Rc<WindowState>,
    kind: NameKind,
    name: &str,
) -> impl std::future::Future<Output = Vec<ReferenceRow>> + use<> {
    let config = state.workspace.scan_config();
    let mut live = Vec::new();
    let mut disk = Vec::new();
    for path in project_paths(state) {
        match state.workspace.get(&path) {
            Some(doc) => live.push((doc.path().to_path_buf(), doc.text())),
            None => disk.push(path),
        }
    }
    let bibs: Vec<(PathBuf, Option<String>)> = if kind == NameKind::Cite {
        let index = state.workspace.index.borrow();
        index
            .bibs
            .iter()
            .filter(|(_, entries)| entries.iter().any(|e| e.key == name))
            .map(|(path, _)| (path.clone(), state.workspace.get(path).map(|d| d.text())))
            .collect()
    } else {
        Vec::new()
    };
    let first = state
        .active_editor()
        .map(|e| e.document().path().to_path_buf());
    let name = name.to_owned();
    async move {
        let mut rows = gio::spawn_blocking(move || {
            let mut rows = Vec::new();
            let texts = live
                .into_iter()
                .map(|(p, t)| (p, Some(t)))
                .chain(disk.into_iter().map(|p| (p, None)));
            for (path, text) in texts {
                let Some(text) = text.or_else(|| read(&path)) else {
                    continue;
                };
                for occurrence in scan::scan_occurrences(&text, config.clone(), &name, kind) {
                    rows.push(row(&path, &text, occurrence.range, occurrence.definition));
                }
            }
            for (path, text) in bibs {
                let Some(text) = text.or_else(|| read(&path)) else {
                    continue;
                };
                if let Some(at) = scan::bib_entry_offset(&text, &name) {
                    rows.push(row(&path, &text, at..at + name.len(), true));
                }
            }
            rows
        })
        .await
        .unwrap_or_default();
        rows.sort_by(|a, b| {
            let rank = |r: &ReferenceRow| first.as_ref() != Some(&r.path);
            (rank(a), &a.path, a.offset).cmp(&(rank(b), &b.path, b.offset))
        });
        rows
    }
}

fn read(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    Some(String::from_utf8_lossy(&std::fs::read(path).ok()?).into_owned())
}

fn row(path: &Path, text: &str, range: Range<usize>, definition: bool) -> ReferenceRow {
    let start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[range.start..]
        .find('\n')
        .map_or(text.len(), |i| range.start + i);
    let line = text[..start].bytes().filter(|&b| b == b'\n').count() as u32;
    let raw = &text[start..end];
    let lead = raw.len() - raw.trim_start().len();
    ReferenceRow {
        path: path.to_path_buf(),
        line,
        offset: range.start,
        text: raw.trim().to_owned(),
        column: range.start - start - lead,
        definition,
    }
}

/// The line of `row` as markup, its name in bold, cut around it when long.
fn row_markup(row: &ReferenceRow, name: &str) -> String {
    let text = &row.text;
    let at = row.column.min(text.len());
    let end = (at + name.len()).min(text.len());
    if !text.is_char_boundary(at) || !text.is_char_boundary(end) {
        return glib::markup_escape_text(text).to_string();
    }
    let (before, found, after) = (&text[..at], &text[at..end], &text[end..]);
    // Some context before the name, the rest after it (the label ellipsizes the end).
    let keep = MAX_ROW_CHARS / 3;
    let count = before.chars().count();
    let before = if count > keep {
        let cut: String = before.chars().skip(count - keep).collect();
        format!("…{cut}")
    } else {
        before.to_owned()
    };
    format!(
        "{}<b>{}</b>{}",
        glib::markup_escape_text(&before),
        glib::markup_escape_text(found),
        glib::markup_escape_text(after)
    )
}

fn show(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
    range: Range<usize>,
    kind: NameKind,
    name: &str,
    rows: Vec<ReferenceRow>,
) {
    let uses = rows.iter().filter(|r| !r.definition).count();
    let files = {
        let mut paths: Vec<&PathBuf> = rows.iter().map(|r| &r.path).collect();
        paths.dedup();
        paths.len()
    };
    let what = match (kind, uses) {
        (NameKind::Label, 1) => "1 reference".to_owned(),
        (NameKind::Label, n) => format!("{n} references"),
        (NameKind::Cite, 1) => "1 citation".to_owned(),
        (NameKind::Cite, n) => format!("{n} citations"),
    };
    let in_files = if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{files} files")
    };
    let title = gtk::Label::builder()
        .use_markup(true)
        .label(format!(
            "<b>{}</b>  <span alpha=\"62%\">{what} · {in_files}</span>",
            glib::markup_escape_text(name)
        ))
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::Middle)
        .max_width_chars(60)
        .css_classes(["lemma-references-title"])
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Browse)
        .activate_on_single_click(true)
        .css_classes(["navigation-sidebar", "lemma-references-list"])
        .build();
    let mut list_rows = Vec::new();
    let mut last_path: Option<&PathBuf> = None;
    for row in &rows {
        if last_path != Some(&row.path) {
            last_path = Some(&row.path);
            let header = gtk::Label::builder()
                .label(display_path(state, &row.path))
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .css_classes(["caption-heading", "dim-label", "lemma-references-file"])
                .build();
            let header_row = gtk::ListBoxRow::builder()
                .child(&header)
                .activatable(false)
                .selectable(false)
                .focusable(false)
                .css_classes(["header-row"])
                .build();
            list.append(&header_row);
        }
        let number = gtk::Label::builder()
            .label((row.line + 1).to_string())
            .xalign(1.0)
            .width_chars(4)
            .css_classes(["dim-label", "numeric", "caption"])
            .build();
        let text = gtk::Label::builder()
            .use_markup(true)
            .label(row_markup(row, name))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .max_width_chars(70)
            .css_classes(["lemma-references-line"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        content.append(&number);
        content.append(&text);
        if row.definition {
            let tag = gtk::Label::builder()
                .label(match kind {
                    NameKind::Label => "Definition",
                    NameKind::Cite => "Entry",
                })
                .css_classes(["caption", "accent"])
                .build();
            content.append(&tag);
        }
        let list_row = gtk::ListBoxRow::builder().child(&content).build();
        list_row.set_tooltip_text(Some(&format!(
            "{}:{}",
            display_path(state, &row.path),
            row.line + 1
        )));
        list.append(&list_row);
        list_rows.push(list_row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(MAX_LIST_HEIGHT)
        .child(&list)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&title);
    content.append(&scroller);
    let popover = gtk::Popover::builder()
        .child(&content)
        .position(gtk::PositionType::Bottom)
        .autohide(true)
        .has_arrow(true)
        .build();
    popover.add_css_class("lemma-references");
    let view = editor.view();
    popover.set_parent(view);
    if let Some(rect) = editor
        .range_rect(range.clone(), range.start)
        .map(|r| clamp_rect(r, view.width(), view.height()))
    {
        popover.set_pointing_to(Some(&rect));
    }
    let shown_list = Rc::new(Shown {
        editor: Rc::downgrade(editor),
        popover: popover.clone(),
        rows,
        list_rows,
        closed: Cell::new(false),
        unrealize: RefCell::new(None),
    });
    set_shown(state, Some(shown_list.clone()));

    let weak = Rc::downgrade(state);
    list.connect_row_activated(move |_, row| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let Some(shown_list) = shown(&state) else {
            return;
        };
        if let Some(i) = shown_list.list_rows.iter().position(|r| r == row) {
            // Outside the list's own signal: the popover goes.
            let state = state.clone();
            glib::idle_add_local_once(move || {
                activate_reference(&state, i);
            });
        }
    });
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&shown_list));
    popover.connect_closed(move |_| {
        if let (Some(state), Some(shown_list)) = (weak.upgrade(), ses.upgrade()) {
            close(&state, &shown_list, Closing::Later);
        }
    });
    let (weak, ses) = (Rc::downgrade(state), Rc::downgrade(&shown_list));
    let handler = view.connect_unrealize(move |_| {
        if let (Some(state), Some(shown_list)) = (weak.upgrade(), ses.upgrade()) {
            close(&state, &shown_list, Closing::TabGoes);
        }
    });
    shown_list.unrealize.replace(Some(handler));
    popover.popup();
    // The first use (else the first row) has the focus: Up/Down, Enter, Esc.
    let first = shown_list
        .rows
        .iter()
        .position(|r| !r.definition)
        .unwrap_or(0);
    if let Some(row) = shown_list.list_rows.get(first) {
        list.select_row(Some(row));
        row.grab_focus();
    }
}

fn clamp_rect(rect: gtk::gdk::Rectangle, width: i32, height: i32) -> gtk::gdk::Rectangle {
    let x = rect.x().clamp(0, (width - 1).max(0));
    let y = rect.y().clamp(0, (height - 1).max(0));
    let w = rect.width().min(width - x).max(1);
    let h = rect.height().min(height - y).max(1);
    gtk::gdk::Rectangle::new(x, y, w, h)
}

/// Closes the list (see [`Closing`]).
fn close(state: &WindowState, shown_list: &Rc<Shown>, how: Closing) {
    if shown_list.closed.replace(true) {
        return;
    }
    if shown(state).is_some_and(|s| Rc::ptr_eq(&s, shown_list)) {
        set_shown(state, None);
    }
    let editor = shown_list.editor.upgrade();
    if let (Some(editor), Some(handler)) = (&editor, shown_list.unrealize.take()) {
        editor.view().disconnect(handler);
    }
    super::close_popover(
        &shown_list.popover,
        editor
            .as_ref()
            .map(|e| e.view().upcast_ref::<gtk::Widget>()),
        how,
    );
}

/// Closes the reference list of `state`'s window, if one is open.
pub fn close_references(state: &WindowState) {
    if let Some(shown_list) = shown(state) {
        close(state, &shown_list, Closing::Now);
    }
}

/// Closes the list when it is shown in `editor` (its tab closes).
pub(super) fn close_for(state: &WindowState, editor: &Rc<Editor>) {
    if let Some(shown_list) = shown(state)
        && shown_list
            .editor
            .upgrade()
            .is_none_or(|e| Rc::ptr_eq(&e, editor))
    {
        close(state, &shown_list, Closing::TabGoes);
    }
}

/// The popover of the open list (tests).
pub fn references_popover(state: &WindowState) -> Option<gtk::Popover> {
    shown(state).map(|s| s.popover.clone())
}

/// The rows of the open list (tests).
pub fn reference_rows(state: &WindowState) -> Vec<ReferenceRow> {
    shown(state).map_or_else(Vec::new, |s| s.rows.clone())
}

/// Jumps to row `index` of the open list (what Enter or a click on it does) and closes it;
/// false when there is no such row.
pub fn activate_reference(state: &Rc<WindowState>, index: usize) -> bool {
    let Some(shown_list) = shown(state) else {
        return false;
    };
    let Some(row) = shown_list.rows.get(index).cloned() else {
        return false;
    };
    close(state, &shown_list, Closing::Now);
    let state = state.clone();
    glib::spawn_future_local(async move {
        let target = GotoTarget::File {
            path: row.path.clone(),
            offset: row.offset,
        };
        if let Err(e) = goto::navigate(&state, target).await {
            state.toast(&e);
        }
    });
    true
}
