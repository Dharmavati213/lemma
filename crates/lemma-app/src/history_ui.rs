//! The local-history timeline.
//!
//! `win.history` ("Local History…" in the main menu's file section) opens a dialog over the
//! window:
//!
//! - **Files:** a drop-down in the sidebar's header lists every file of the project that has
//!   snapshots (`index.jsonl` of `agent::history::project_dir`, read on the jobs thread after
//!   [`history::flush`]): the open documents first, then the other files, then the files deleted
//!   since their last snapshot (marked "Deleted"). It starts at the active document.
//! - **Snapshots:** [`history::entries`] of that file, newest first, grouped by day, each with
//!   its time and reason (saved, before agent turn N, before a reject); those that hold the
//!   current text say so ("same as now", known from their hash). The newest snapshot that
//!   differs from the current text is selected first.
//! - **Preview:** the selected snapshot against the current text (the live buffer when the file
//!   is open, else the file; nothing for a deleted file) as a unified diff in the editor's font,
//!   coloured like the review: red lines are current text a restore removes, green lines are
//!   what it brings back. The header says when and why the snapshot was taken and how many
//!   lines a restore adds and removes (`+12 −3`). Diffs are computed on the jobs thread; the newest
//!   selection wins. Very long diffs are cut after [`MAX_PREVIEW_LINES`].
//! - **Restore as Hunks** opens the file and applies the snapshot as reviewable hunks
//!   ([`history::restore`]): every change can be accepted or rejected, and rejecting them all
//!   gives the current text back. A deleted file comes back as a new file under review
//!   (rejecting it moves it to the trash again). The cursor goes to the first hunk.
//!
//! M2's "Restore from History…" (`win.history-restore`, a plain list) stays as it was.
//!
//! Hooks: [`install`] once per window (the menu item); [`show`] is the action. Tests:
//! [`timeline`] gives the open dialog's [`Timeline`].

use crate::agent::history::{self, HistoryEntry, HistoryReason};
use crate::editor::review_overlay::Palette;
use crate::review_ui;
use crate::window::WindowState;
use crate::workspace::normalize;
use adw::prelude::*;
use lemma_core::fnv1a64;
use serde::Deserialize;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// The preview shows at most this many diff lines.
pub const MAX_PREVIEW_LINES: usize = 4000;

/// One file of the drop-down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryFile {
    pub path: PathBuf,
    /// False for a file deleted since its last snapshot.
    pub exists: bool,
}

/// Adds "Local History…" to the main menu's file section.
pub fn install(state: &Rc<WindowState>) {
    state
        .header
        .menu_files
        .append(Some("_Local History…"), Some("win.history"));
}

/// `win.history`: opens the timeline at the active document.
pub fn show(state: &Rc<WindowState>) {
    let path = state
        .active_editor()
        .map(|e| e.document().path().to_path_buf());
    show_for(state, path.as_deref());
}

/// Opens the timeline at `path` (or at the first file with snapshots); an open one is shown
/// again.
pub fn show_for(state: &Rc<WindowState>, path: Option<&Path>) {
    if let Some(open) = timeline(state) {
        if let Some(path) = path {
            open.select_file(path);
        }
        open.dialog.present(Some(&state.window));
        return;
    }
    if state.paths.is_none() {
        state.toast("Local history is not available (no home directory)");
        return;
    }
    let timeline = Timeline::new(state);
    TIMELINES.with(|list| list.borrow_mut().push(timeline.clone()));
    timeline.dialog.present(Some(&state.window));
    let path = path.map(normalize);
    let t = timeline.clone();
    timeline.run(async move { t.load_files(path).await });
}

/// The timeline open over `state`'s window (tests).
pub fn timeline(state: &WindowState) -> Option<Rc<Timeline>> {
    TIMELINES.with(|list| {
        list.borrow()
            .iter()
            .find(|t| std::ptr::eq(t.state.as_ptr(), state))
            .cloned()
    })
}

thread_local! {
    static TIMELINES: RefCell<Vec<Rc<Timeline>>> = const { RefCell::new(Vec::new()) };
}

/// What the preview shows for the selected snapshot.
struct Selected {
    entry: HistoryEntry,
    /// It equals the current text.
    same: bool,
}

/// The open timeline dialog.
pub struct Timeline {
    state: Weak<WindowState>,
    pub dialog: adw::Dialog,
    files_model: gtk::StringList,
    files: RefCell<Vec<HistoryFile>>,
    dropdown: gtk::DropDown,
    list: gtk::ListBox,
    entries: RefCell<Vec<HistoryEntry>>,
    title: adw::WindowTitle,
    restore_button: gtk::Button,
    banner: adw::Banner,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    same: adw::StatusPage,
    error: adw::StatusPage,
    preview: sourceview5::View,
    buffer: sourceview5::Buffer,
    /// The current text of the selected file (`None` while it is read).
    current: RefCell<Option<String>>,
    selected: RefCell<Option<Selected>>,
    /// Bumped by every selection: older loads are dropped.
    generation: Cell<u64>,
    /// Loads running.
    busy: Cell<u32>,
    /// Set while the drop-down is filled (its selection is not the user's).
    filling: Cell<bool>,
    style_handler: RefCell<Option<glib::SignalHandlerId>>,
}

impl Timeline {
    fn new(state: &Rc<WindowState>) -> Rc<Self> {
        let files_model = gtk::StringList::new(&[]);
        let dropdown = gtk::DropDown::builder()
            .model(&files_model)
            .enable_search(true)
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .tooltip_text("File")
            .css_classes(["flat"])
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No snapshots")
                .css_classes(["dim-label"])
                .margin_top(24)
                .build(),
        ));
        let list_scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let sidebar_header = adw::HeaderBar::builder()
            .show_end_title_buttons(false)
            .title_widget(&dropdown)
            .build();
        let sidebar = adw::ToolbarView::new();
        sidebar.add_top_bar(&sidebar_header);
        sidebar.set_content(Some(&list_scroller));

        let title = adw::WindowTitle::new("Local History", "");
        let restore_button = gtk::Button::builder()
            .label("_Restore as Hunks")
            .use_underline(true)
            .tooltip_text("Apply this snapshot as changes to review")
            .css_classes(["suggested-action"])
            .sensitive(false)
            .build();
        let content_header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .title_widget(&title)
            .build();
        content_header.pack_end(&restore_button);
        let banner = adw::Banner::builder()
            .title("Lemma is not recording local history in this window")
            .revealed(false)
            .build();

        let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
        buffer.set_highlight_matching_brackets(false);
        let preview = sourceview5::View::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .left_margin(12)
            .right_margin(12)
            .top_margin(8)
            .bottom_margin(24)
            .css_classes(["lemma-editor", "lemma-history-preview"])
            .build();
        let preview_scroller = gtk::ScrolledWindow::builder()
            .child(&preview)
            .vexpand(true)
            .hexpand(true)
            .build();
        let status = |icon: &str, title: &str| {
            adw::StatusPage::builder()
                .icon_name(icon)
                .title(title)
                .css_classes(["compact"])
                .build()
        };
        let empty = status("document-open-recent-symbolic", "No History Yet");
        let same = status("object-select-symbolic", "Same as Now");
        let error = status("dialog-warning-symbolic", "Cannot Show This Snapshot");
        let pick = status("document-open-recent-symbolic", "No Snapshot Selected");
        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&spinner, Some("loading"));
        stack.add_named(&preview_scroller, Some("preview"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&same, Some("same"));
        stack.add_named(&error, Some("error"));
        stack.add_named(&pick, Some("pick"));
        stack.set_visible_child_name("loading");
        let content = adw::ToolbarView::new();
        content.add_top_bar(&content_header);
        content.add_top_bar(&banner);
        content.set_content(Some(&stack));

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar)
            .content(&content)
            .min_sidebar_width(230.0)
            .max_sidebar_width(300.0)
            .sidebar_width_fraction(0.3)
            .collapsed(false)
            .build();
        let dialog = adw::Dialog::builder()
            .title("Local History")
            .content_width(920)
            .content_height(600)
            .child(&split)
            .build();
        let timeline = Rc::new(Self {
            state: Rc::downgrade(state),
            dialog,
            files_model,
            files: RefCell::new(Vec::new()),
            dropdown,
            list,
            entries: RefCell::new(Vec::new()),
            title,
            restore_button,
            banner,
            stack,
            empty,
            same,
            error,
            preview,
            buffer,
            current: RefCell::new(None),
            selected: RefCell::new(None),
            generation: Cell::new(0),
            busy: Cell::new(0),
            filling: Cell::new(false),
            style_handler: RefCell::new(None),
        });
        timeline.banner.set_revealed(!state.config.history.enabled);
        timeline.connect();
        timeline.restyle();
        timeline
    }

    fn connect(self: &Rc<Self>) {
        let factory = gtk::SignalListItemFactory::new();
        let weak = Rc::downgrade(self);
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let name = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Start)
                .max_width_chars(28)
                .build();
            let deleted = gtk::Label::builder()
                .label("Deleted")
                .css_classes(["caption", "dim-label"])
                .visible(false)
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&name);
            row.append(&deleted);
            item.set_child(Some(&row));
        });
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
            let (Some(row), Some(timeline)) = (item.child(), weak.upgrade()) else {
                return;
            };
            let Some(name) = row.first_child().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(deleted) = name.next_sibling() else {
                return;
            };
            // The popup's list is filtered by the search: find the file by its name.
            let Some(shown) = item.item().and_downcast::<gtk::StringObject>() else {
                return;
            };
            let shown = shown.string();
            let exists = timeline
                .files
                .borrow()
                .iter()
                .find(|f| timeline.display_name(&f.path) == shown.as_str())
                .is_none_or(|f| f.exists);
            name.set_label(&shown);
            deleted.set_visible(!exists);
        });
        self.dropdown.set_factory(Some(&factory));

        let weak = Rc::downgrade(self);
        self.dropdown.connect_selected_notify(move |dropdown| {
            if let Some(timeline) = weak.upgrade()
                && !timeline.filling.get()
            {
                let file = timeline
                    .files
                    .borrow()
                    .get(dropdown.selected() as usize)
                    .cloned();
                if let Some(file) = file {
                    let t = timeline.clone();
                    timeline.run(async move { t.load_entries(file).await });
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.list.set_header_func(move |row, before| {
            let Some(timeline) = weak.upgrade() else {
                return;
            };
            let entries = timeline.entries.borrow();
            let day = |row: &gtk::ListBoxRow| {
                entries
                    .get(usize::try_from(row.index()).unwrap_or(usize::MAX))
                    .map(|e| day_key(e.unix))
            };
            let this = day(row);
            if this.is_some() && before.and_then(day) != this {
                let label = gtk::Label::builder()
                    .label(
                        entries
                            .get(row.index() as usize)
                            .map_or_else(String::new, |e| day_label(e.unix)),
                    )
                    .xalign(0.0)
                    .css_classes(["caption-heading", "dim-label"])
                    .margin_top(if row.index() == 0 { 6 } else { 14 })
                    .margin_bottom(4)
                    .margin_start(12)
                    .build();
                row.set_header(Some(&label));
            } else {
                row.set_header(None::<&gtk::Widget>);
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_row_selected(move |_, row| {
            if let (Some(timeline), Some(row)) = (weak.upgrade(), row) {
                let index = usize::try_from(row.index()).unwrap_or(usize::MAX);
                let t = timeline.clone();
                timeline.run(async move { t.load_preview(index).await });
            }
        });
        let weak = Rc::downgrade(self);
        self.restore_button.connect_clicked(move |_| {
            if let Some(timeline) = weak.upgrade() {
                glib::spawn_future_local(async move {
                    let _ = timeline.restore().await;
                });
            }
        });
        let weak = Rc::downgrade(self);
        let handler = adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(timeline) = weak.upgrade() {
                timeline.restyle();
            }
        });
        self.style_handler.replace(Some(handler));
        let weak = Rc::downgrade(self);
        self.dialog.connect_closed(move |_| {
            if let Some(timeline) = weak.upgrade() {
                timeline.closed();
            }
        });
    }

    /// Runs `load` (a load of the dialog), counted in [`Timeline::is_busy`] from now on.
    fn run(self: &Rc<Self>, load: impl std::future::Future<Output = ()> + 'static) {
        self.busy.set(self.busy.get() + 1);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            load.await;
            if let Some(timeline) = weak.upgrade() {
                timeline.busy.set(timeline.busy.get().saturating_sub(1));
            }
        });
    }

    fn closed(&self) {
        if let Some(handler) = self.style_handler.take() {
            adw::StyleManager::default().disconnect(handler);
        }
        // Loads still running find nothing to update.
        self.generation.set(self.generation.get() + 1);
        TIMELINES.with(|list| {
            list.borrow_mut()
                .retain(|t| !std::ptr::eq(Rc::as_ptr(t), self))
        });
    }

    fn display_name(&self, path: &Path) -> String {
        match self.state.upgrade() {
            Some(state) => review_ui::relative_name(&state, path),
            None => path.display().to_string(),
        }
    }

    // ----- loading -----

    /// Lists the files with snapshots and selects `wanted` (else the first).
    async fn load_files(self: &Rc<Self>, wanted: Option<PathBuf>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let files = history_files(&state, wanted.as_deref()).await;
        self.filling.set(true);
        let names: Vec<String> = files.iter().map(|f| self.display_name(&f.path)).collect();
        self.files.replace(files.clone());
        let old = self.files_model.n_items();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.files_model.splice(0, old, &names);
        let index = wanted
            .as_ref()
            .and_then(|w| files.iter().position(|f| &f.path == w))
            .unwrap_or(0);
        if !files.is_empty() {
            self.dropdown.set_selected(index as u32);
        }
        self.filling.set(false);
        self.dropdown.set_sensitive(files.len() > 1);
        match files.get(index).cloned() {
            Some(file) => {
                self.load_entries(file).await;
                // The keyboard starts in the list (Up and Down go through the snapshots).
                if let Some(row) = self.list.selected_row() {
                    row.grab_focus();
                }
            }
            None => {
                self.show_empty(None);
                self.restore_button.set_sensitive(false);
            }
        }
    }

    /// Lists the snapshots of `file` and selects the newest.
    async fn load_entries(self: &Rc<Self>, file: HistoryFile) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.stack.set_visible_child_name("loading");
        self.restore_button.set_sensitive(false);
        self.selected.replace(None);
        self.current.replace(None);
        let entries = history::entries(&state, &file.path).await;
        let current = current_text(&state, &file).await;
        if self.generation.get() != generation {
            return;
        }
        // Snapshots are named by the hash of their text: the ones equal to the current text are
        // known without reading them.
        let now = fnv1a64(&[current.as_bytes()]);
        self.current.replace(Some(current));
        self.list.remove_all();
        self.entries.replace(entries.clone());
        for entry in &entries {
            self.list.append(&entry_row(entry, entry.hash == now));
        }
        if entries.is_empty() {
            self.show_empty(Some(&file.path));
            return;
        }
        // The newest snapshot that differs from now (the newest is usually the last save).
        let first = entries.iter().position(|e| e.hash != now).unwrap_or(0);
        if let Some(row) = i32::try_from(first)
            .ok()
            .and_then(|i| self.list.row_at_index(i))
        {
            // Selecting it loads the preview.
            self.list.select_row(Some(&row));
        }
    }

    fn show_empty(&self, path: Option<&Path>) {
        let description = match path {
            Some(path) => format!(
                "Lemma keeps a copy of {} each time it is saved and before every agent turn.",
                glib::markup_escape_text(
                    &path
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                )
            ),
            None => "Lemma keeps a copy of each file of the project when it is saved and \
                     before every agent turn."
                .to_owned(),
        };
        self.empty.set_description(Some(&description));
        if let Some(path) = path {
            self.title.set_title(&self.display_name(path));
        }
        self.title.set_subtitle("");
        self.stack.set_visible_child_name("empty");
    }

    /// Shows the diff of entry `index` against the current text.
    async fn load_preview(self: &Rc<Self>, index: usize) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(entry) = self.entries.borrow().get(index).cloned() else {
            return;
        };
        let Some(current) = self.current.borrow().clone() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.restore_button.set_sensitive(false);
        self.selected.replace(None);
        // A short load keeps the old preview on screen instead of flashing the spinner.
        let spinner = {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(timeline) = weak.upgrade()
                    && timeline.generation.get() == generation
                {
                    timeline.stack.set_visible_child_name("loading");
                }
            })
        };
        let loaded = history::load(&state, &entry).await;
        let rel = self.display_name(&entry.path);
        let diff = match loaded {
            Ok(snapshot) => {
                let current = current.clone();
                gio::spawn_blocking(move || {
                    let diff = lemma_review::unified_diff(&rel, &current, &snapshot);
                    preview_of(&diff)
                })
                .await
                .map_err(|_| "computing the differences failed".to_owned())
            }
            Err(e) => Err(e),
        };
        if self.generation.get() != generation {
            return;
        }
        spinner.remove();
        let reason = reason_label(entry.reason);
        let when = time_label(entry.unix);
        match diff {
            Err(e) => {
                self.error
                    .set_description(Some(&glib::markup_escape_text(&e)));
                self.title.set_title(&when);
                self.title.set_subtitle(&reason);
                self.stack.set_visible_child_name("error");
            }
            Ok(preview) => {
                let same = preview.lines == 0;
                if same {
                    self.same.set_description(Some(
                        "This snapshot has the same text as the file now; there is nothing to \
                         restore.",
                    ));
                    self.title.set_title(&when);
                    self.title.set_subtitle(&reason);
                    self.stack.set_visible_child_name("same");
                } else {
                    self.title.set_title(&when);
                    self.title.set_subtitle(&format!(
                        "{reason} · +{} −{}",
                        preview.added, preview.removed
                    ));
                    self.fill_preview(&preview);
                    self.stack.set_visible_child_name("preview");
                }
                self.restore_button.set_sensitive(!same);
                self.selected.replace(Some(Selected { entry, same }));
            }
        }
    }

    // ----- the preview -----

    fn fill_preview(&self, preview: &Preview) {
        let buffer = &self.buffer;
        buffer.set_text("");
        let mut end = buffer.end_iter();
        for (line, kind) in &preview.rows {
            let tag = match kind {
                RowKind::Added => Some("added"),
                RowKind::Removed => Some("removed"),
                RowKind::Hunk => Some("hunk"),
                RowKind::Note => Some("note"),
                RowKind::Context => None,
            };
            match tag {
                Some(tag) => buffer.insert_with_tags_by_name(&mut end, line, &[tag]),
                None => buffer.insert(&mut end, line),
            }
            buffer.insert(&mut end, "\n");
        }
        buffer.place_cursor(&buffer.start_iter());
        if let Some(adjustment) = self.preview.vadjustment() {
            adjustment.set_value(0.0);
        }
    }

    /// The preview's colours, from the editor's style scheme and the review palette.
    fn restyle(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let dark = adw::StyleManager::default().is_dark();
        let scheme = crate::editor::style_scheme(&state.config.editor, dark);
        self.buffer.set_style_scheme(scheme.as_ref());
        let palette = Palette::new(None, &self.buffer);
        let table = self.buffer.tag_table();
        let tag = |name: &str| {
            table.lookup(name).unwrap_or_else(|| {
                let tag = gtk::TextTag::new(Some(name));
                table.add(&tag);
                tag
            })
        };
        tag("added").set_paragraph_background_rgba(Some(&palette.added));
        tag("removed").set_paragraph_background_rgba(Some(&palette.deleted_bg));
        tag("removed").set_foreground_rgba(Some(&palette.deleted_fg));
        let fg = self.preview.color();
        let dim = gtk::gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), 0.55);
        let hunk = tag("hunk");
        hunk.set_foreground_rgba(Some(&dim));
        hunk.set_paragraph_background_rgba(Some(&palette.band));
        hunk.set_pixels_above_lines(6);
        let note = tag("note");
        note.set_foreground_rgba(Some(&dim));
        note.set_style(pango::Style::Italic);
    }

    // ----- restoring -----

    /// Restores the selected snapshot as reviewable hunks (the dialog closes first); resolves to
    /// the number of hunks of the file afterwards.
    pub async fn restore(self: &Rc<Self>) -> Result<usize, String> {
        let state = self.state.upgrade().ok_or("the window is gone")?;
        let Some((entry, same)) = self
            .selected
            .borrow()
            .as_ref()
            .map(|s| (s.entry.clone(), s.same))
        else {
            return Err("no snapshot is selected".to_owned());
        };
        if same {
            return Ok(0);
        }
        self.dialog.close();
        let result = restore_entry(&state, &entry).await;
        match &result {
            Ok(0) => state.toast(&format!(
                "{} already has the text of {}",
                review_ui::relative_name(&state, &entry.path),
                time_label(entry.unix)
            )),
            Ok(_) => state.toast(&format!(
                "Restored {} from {}: review the changes",
                review_ui::relative_name(&state, &entry.path),
                time_label(entry.unix)
            )),
            Err(e) => state.toast(e),
        }
        result
    }

    // ----- for tests -----

    /// The files of the drop-down.
    pub fn files(&self) -> Vec<HistoryFile> {
        self.files.borrow().clone()
    }

    /// The file shown.
    pub fn selected_file(&self) -> Option<HistoryFile> {
        self.files
            .borrow()
            .get(self.dropdown.selected() as usize)
            .cloned()
    }

    /// Shows the snapshots of `path` (false when the drop-down does not have it).
    pub fn select_file(&self, path: &Path) -> bool {
        let path = normalize(path);
        let index = self.files.borrow().iter().position(|f| f.path == path);
        match index {
            Some(i) => {
                self.dropdown.set_selected(i as u32);
                true
            }
            None => false,
        }
    }

    /// The snapshots listed, newest first.
    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.entries.borrow().clone()
    }

    /// Selects snapshot `index` (its preview loads).
    pub fn select_entry(&self, index: usize) -> bool {
        let row = i32::try_from(index)
            .ok()
            .and_then(|i| self.list.row_at_index(i));
        match row {
            Some(row) => {
                self.list.select_row(Some(&row));
                true
            }
            None => false,
        }
    }

    /// The snapshot whose preview is shown.
    pub fn selected_entry(&self) -> Option<HistoryEntry> {
        self.selected.borrow().as_ref().map(|s| s.entry.clone())
    }

    /// The text of the preview.
    pub fn preview_text(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.text(&start, &end, false).to_string()
    }

    /// What the content area shows: `loading`, `preview`, `empty`, `same`, `error` or `pick`.
    pub fn page(&self) -> String {
        self.stack
            .visible_child_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
    }

    /// The header's title and subtitle.
    pub fn heading(&self) -> (String, String) {
        (
            self.title.title().to_string(),
            self.title.subtitle().to_string(),
        )
    }

    /// True while something loads.
    pub fn is_busy(&self) -> bool {
        self.busy.get() > 0
    }

    /// True when "Restore as Hunks" can be pressed.
    pub fn can_restore(&self) -> bool {
        self.restore_button.is_sensitive()
    }

    /// True when the "not recording" banner is shown.
    pub fn banner_shown(&self) -> bool {
        self.banner.is_revealed()
    }
}

// ----- helpers -----

/// One index line, as far as the timeline reads it.
#[derive(Deserialize)]
struct IndexLine {
    path: PathBuf,
    unix: u64,
}

/// The files of `state`'s project that have snapshots, plus `wanted` (see the module docs).
async fn history_files(state: &Rc<WindowState>, wanted: Option<&Path>) -> Vec<HistoryFile> {
    history::flush(state).await;
    let open: Vec<PathBuf> = state
        .editors()
        .iter()
        .map(|e| e.document().path().to_path_buf())
        .collect();
    let dir = state
        .paths
        .as_ref()
        .map(|p| history::project_dir(&p.history_dir(), state.workspace.root().path()));
    let wanted = wanted.map(Path::to_path_buf);
    gio::spawn_blocking(move || {
        let mut newest: HashMap<PathBuf, u64> = HashMap::new();
        if let Some(dir) = dir
            && let Ok(text) = std::fs::read_to_string(dir.join("index.jsonl"))
        {
            for line in text.lines() {
                if let Ok(line) = serde_json::from_str::<IndexLine>(line) {
                    let unix = newest.entry(line.path).or_default();
                    *unix = (*unix).max(line.unix);
                }
            }
        }
        if let Some(wanted) = wanted {
            newest.entry(wanted).or_default();
        }
        let mut files: Vec<(u8, PathBuf, bool)> = newest
            .into_keys()
            .map(|path| {
                let exists = path.is_file();
                let rank = match open.iter().position(|p| *p == path) {
                    Some(_) => 0,
                    None if exists => 1,
                    None => 2,
                };
                (rank, path, exists)
            })
            .collect();
        files.sort_by(|a, b| {
            let tab = |p: &Path| open.iter().position(|o| o == p).unwrap_or(usize::MAX);
            (a.0, tab(&a.1), &a.1).cmp(&(b.0, tab(&b.1), &b.1))
        });
        files
            .into_iter()
            .map(|(_, path, exists)| HistoryFile { path, exists })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The text a snapshot of `file` is compared with: the document's (unsaved changes included),
/// else the file's, else nothing (deleted).
async fn current_text(state: &Rc<WindowState>, file: &HistoryFile) -> String {
    if let Some(doc) = state.workspace.get(&file.path) {
        return doc.text();
    }
    let path = file.path.clone();
    gio::spawn_blocking(move || {
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| crate::saver::decode(&bytes, None).ok())
            .map(|d| d.text)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// Opens `entry`'s file and applies the snapshot as hunks; a deleted file comes back as a new
/// file under review. Moves the cursor to the first hunk.
async fn restore_entry(state: &Rc<WindowState>, entry: &HistoryEntry) -> Result<usize, String> {
    let path = entry.path.clone();
    let deleted = state.workspace.get(&path).is_none() && !path.exists();
    if deleted
        && let Some(parent) = path.parent()
        && path.starts_with(state.workspace.root().path())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let editor = state.open_file(&path).await?;
    let doc = editor.document().clone();
    if deleted {
        review_ui::start_review(&doc, true);
    }
    let count = history::restore(state, &doc, entry).await?;
    let first = doc
        .review
        .borrow()
        .as_ref()
        .and_then(|r| r.hunks().first().map(|h| h.cur_lines.start));
    if let Some(line) = first {
        review_ui::show_line(state, &doc, line);
    }
    Ok(count)
}

/// How a diff line is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Context,
    Added,
    Removed,
    Hunk,
    Note,
}

/// A diff prepared for the preview.
struct Preview {
    rows: Vec<(String, RowKind)>,
    /// Diff lines (0: the texts are equal).
    lines: usize,
    added: usize,
    removed: usize,
}

/// The rows of `diff` (a unified diff; its `---`/`+++` header is left out), cut after
/// [`MAX_PREVIEW_LINES`].
fn preview_of(diff: &str) -> Preview {
    let mut rows = Vec::new();
    let (mut added, mut removed, mut lines) = (0, 0, 0);
    let body = diff
        .lines()
        .skip_while(|l| l.starts_with("--- ") || l.starts_with("+++ "));
    for line in body {
        lines += 1;
        let kind = match line.as_bytes().first() {
            Some(b'+') => {
                added += 1;
                RowKind::Added
            }
            Some(b'-') => {
                removed += 1;
                RowKind::Removed
            }
            Some(b'@') => RowKind::Hunk,
            Some(b'\\') => RowKind::Note,
            _ => RowKind::Context,
        };
        if rows.len() < MAX_PREVIEW_LINES {
            rows.push((line.to_owned(), kind));
        }
    }
    if lines > rows.len() {
        rows.push((
            format!("… {} more lines", lines - rows.len()),
            RowKind::Note,
        ));
    }
    Preview {
        rows,
        lines,
        added,
        removed,
    }
}

/// A snapshot's row; `same` when it holds the current text.
fn entry_row(entry: &HistoryEntry, same: bool) -> gtk::ListBoxRow {
    let icon = match entry.reason {
        HistoryReason::Save => "document-save-symbolic",
        HistoryReason::Turn(_) => "chat-message-new-symbolic",
        HistoryReason::Reject => "edit-undo-symbolic",
    };
    let time = gtk::Label::builder()
        .label(clock_label(entry.unix))
        .xalign(0.0)
        .css_classes(["numeric"])
        .build();
    let caption = if same {
        format!("{} · same as now", reason_label(entry.reason))
    } else {
        reason_label(entry.reason)
    };
    let reason = gtk::Label::builder()
        .label(&caption)
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.append(&time);
    text.append(&reason);
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    row_box.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .css_classes(["dim-label"])
            .build(),
    );
    row_box.append(&text);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {caption}",
        time_label(entry.unix)
    ))]);
    row
}

fn reason_label(reason: HistoryReason) -> String {
    match reason {
        HistoryReason::Save => "Saved".to_owned(),
        HistoryReason::Turn(n) => format!("Before agent turn {n}"),
        HistoryReason::Reject => "Before a reject".to_owned(),
    }
}

fn local(unix: u64) -> Option<glib::DateTime> {
    i64::try_from(unix)
        .ok()
        .and_then(|u| glib::DateTime::from_unix_local(u).ok())
}

/// `14:32:10`.
fn clock_label(unix: u64) -> String {
    local(unix)
        .and_then(|t| t.format("%H:%M:%S").ok())
        .map_or_else(|| unix.to_string(), |s| s.to_string())
}

/// The day of `unix`, for grouping.
fn day_key(unix: u64) -> Option<(i32, i32)> {
    local(unix).map(|t| (t.year(), t.day_of_year()))
}

/// `Today`, `Yesterday`, `Thursday 9 October`, or `9 October 2025` for an earlier year.
fn day_label(unix: u64) -> String {
    let Some(t) = local(unix) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let days_ago = {
        let midnight = |d: &glib::DateTime| {
            glib::DateTime::from_local(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0).ok()
        };
        match (midnight(&now), midnight(&t)) {
            (Some(a), Some(b)) => (a.difference(&b).as_seconds() + 43_200) / 86_400,
            _ => i64::MAX,
        }
    };
    let format = |f: &str| {
        t.format(f)
            .map_or_else(|_| String::new(), |s| s.trim().to_owned())
    };
    match days_ago {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        2..=6 => format("%A"),
        _ if t.year() == now.year() => format("%A %e %B"),
        _ => format("%e %B %Y"),
    }
}

/// `Today, 14:32`, `Thursday 9 October, 14:32`.
fn time_label(unix: u64) -> String {
    let day = day_label(unix);
    let clock = local(unix)
        .and_then(|t| t.format("%H:%M").ok())
        .map_or_else(String::new, |s| s.to_string());
    if day.is_empty() {
        clock
    } else {
        format!("{day}, {clock}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_count_and_cut_lines() {
        let diff =
            lemma_review::unified_diff("a.tex", "one\ntwo\nthree\n", "one\n2\nthree\nfour\n");
        let preview = preview_of(&diff);
        assert_eq!((preview.added, preview.removed), (2, 1));
        assert!(preview.rows.iter().all(|(l, _)| !l.starts_with("+++")));
        assert_eq!(preview.rows[0].1, RowKind::Hunk);
        assert!(
            preview
                .rows
                .contains(&("-two".to_owned(), RowKind::Removed))
        );
        assert!(preview.rows.contains(&("+four".to_owned(), RowKind::Added)));
        assert_eq!(preview_of("").lines, 0);
        let long: String = (0..MAX_PREVIEW_LINES + 10)
            .map(|i| format!("{i}\n"))
            .collect();
        let preview = preview_of(&lemma_review::unified_diff("a.tex", "", &long));
        assert_eq!(preview.rows.len(), MAX_PREVIEW_LINES + 1);
        let (note, kind) = preview.rows.last().unwrap();
        assert_eq!(*kind, RowKind::Note);
        assert!(note.contains("more lines"), "{note}");
        assert_eq!(preview.added, MAX_PREVIEW_LINES + 10);
    }

    #[test]
    fn reasons_and_days_read_well() {
        assert_eq!(reason_label(HistoryReason::Turn(3)), "Before agent turn 3");
        let now = glib::DateTime::now_local().unwrap().to_unix() as u64;
        assert_eq!(day_label(now), "Today");
        assert_eq!(day_label(now - 86_400), "Yesterday");
        assert!(time_label(now).starts_with("Today, "));
        assert_eq!(clock_label(now).len(), 8);
        assert_ne!(day_key(now), day_key(now - 86_400));
    }
}
