//! The Changes sidebar page (WP1.2/WP2.6, ARCHITECTURE §4.3): every file the agent touched, with
//! counts, Accept all / Reject all per file and a plain-text diff for screen readers.
//!
//! The review controller refreshes the panel together with the banner (after every recompute),
//! from the same [`ReviewSummary`], so the counts always agree. Once the agent works in turns, the
//! changes are grouped by turn, newest first ("Turn 3 · 4 changes in 2 files", with **Keep Turn**
//! and **Reject Turn**), then the changes several turns made ("Several turns", decided one by one
//! or per file), then the others; inside each group the grouping is per file, as before. Without
//! turns the page is the plain list of files. The header's **Undo Last Turn** button rejects the
//! latest turn with pending changes.
//!
//! Activating a row shows its first change. The "Plain diff" toggle replaces the list with the
//! `unified_diff` of every file in a read-only text view.
//!
//! The widgets change only when what they show changes (the counts, not the hunk ids, which
//! change at every keystroke inside a hunk), and only those that do: rows are kept by group and
//! file, a count changes a row's subtitle in place, and only the rows and groups that came or
//! went are added or removed. A row's buttons look its document and hunks up when clicked. While
//! the page is hidden no row is built: the latest model waits until the page is shown (or its
//! rows are asked for), as the plain diff does; only the header's Undo Last Turn follows at
//! once. When an update removes the widget that had the keyboard focus (the Reject Turn button
//! just activated), the focus goes to the first file row, whose activation only shows a change,
//! or else to the editor: never to whatever widget comes next (a toast's Undo button), where the
//! next key would act.

use crate::document::{DocRef, Document};
use crate::review_ui::{self, Decision, GroupKind, ReviewSummary, TurnGroup};
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// What one row shows and decides.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RowModel {
    path: PathBuf,
    title: String,
    subtitle: String,
    /// The group whose hunks (in this file) the row's buttons decide, looked up when clicked;
    /// `None` for the whole file (the list without turns).
    group: Option<GroupKind>,
}

/// What one group shows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GroupModel {
    kind: GroupKind,
    title: String,
    running: bool,
    /// A named turn (Replace, Rename, a quick fix): its buttons say Keep All / Reject All.
    named: bool,
    rows: Vec<RowModel>,
}

/// What the page shows; the widgets change only when it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    /// Grouped by turn (else a single group of whole-file rows).
    grouped: bool,
    groups: Vec<GroupModel>,
    /// The turn "Undo Last Turn" rejects, with its label and whether it still runs.
    latest: Option<(u32, String, bool)>,
}

/// The widgets of one group: its header (with turns), its list and the list's rows.
struct GroupWidgets {
    /// What they show.
    model: GroupModel,
    header: Option<gtk::Box>,
    list: gtk::ListBox,
    /// The rows of `model.rows`, in order.
    rows: Vec<adw::ActionRow>,
}

/// The Changes page of one window.
pub struct ChangesPanel {
    root: gtk::Box,
    stack: gtk::Stack,
    /// Holds the groups' headers and lists.
    lists: gtk::Box,
    undo_turn: gtk::Button,
    diff_toggle: gtk::ToggleButton,
    diff: gtk::TextBuffer,
    /// The plain diff was not rebuilt because the page was not shown; it is when the page is
    /// shown (or its text is asked for).
    diff_stale: Cell<bool>,
    state: Weak<WindowState>,
    /// What the widgets show.
    shown: RefCell<Option<Model>>,
    /// What they will show once the page is shown (or its rows are asked for): the model of a
    /// refresh while it was hidden.
    pending: RefCell<Option<Model>>,
    /// The widgets of `shown`'s groups, in order.
    groups: RefCell<Vec<GroupWidgets>>,
}

impl ChangesPanel {
    fn new(state: &Rc<WindowState>) -> Rc<Self> {
        let title = gtk::Label::builder()
            .label("Agent Changes")
            .css_classes(["heading"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        let undo_turn = gtk::Button::builder()
            .icon_name("document-revert-symbolic")
            .action_name("win.review-undo-turn")
            .css_classes(["flat"])
            .visible(false)
            .build();
        undo_turn.update_property(&[gtk::accessible::Property::Label("Undo Last Turn")]);
        let diff_toggle = gtk::ToggleButton::builder()
            .label("Plain Diff")
            .css_classes(["flat"])
            .tooltip_text("Show the changes as a unified diff")
            .build();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(12)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        header.append(&title);
        header.append(&undo_turn);
        header.append(&diff_toggle);

        let lists = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        let tooltip = |what: &str, action: &str| match review_ui::shortcut_label(&state.app, action)
        {
            Some(keys) => format!("{what} every change in every file ({keys})"),
            None => format!("{what} every change in every file"),
        };
        let accept_all = gtk::Button::builder()
            .label("Accept All")
            .action_name("win.review-accept-all")
            .tooltip_text(tooltip("Accept", "win.review-accept-all"))
            .hexpand(true)
            .build();
        let reject_all = gtk::Button::builder()
            .label("Reject All")
            .action_name("win.review-reject-all")
            .tooltip_text(tooltip("Reject", "win.review-reject-all"))
            .css_classes(["destructive-action"])
            .hexpand(true)
            .build();
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .homogeneous(true)
            .build();
        buttons.append(&accept_all);
        buttons.append(&reject_all);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(8)
            .build();
        content.append(&lists);
        content.append(&buttons);
        let list_page = gtk::ScrolledWindow::builder()
            .child(&content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let diff = gtk::TextBuffer::new(None);
        let diff_view = gtk::TextView::builder()
            .buffer(&diff)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .left_margin(8)
            .right_margin(8)
            .top_margin(6)
            .bottom_margin(6)
            .build();
        diff_view.update_property(&[gtk::accessible::Property::Label(
            "Unified diff of the agent's changes",
        )]);
        let diff_page = gtk::ScrolledWindow::builder()
            .child(&diff_view)
            .vexpand(true)
            .build();

        let empty = adw::StatusPage::builder()
            .icon_name("document-edit-symbolic")
            .title("No Changes")
            .description("Edits made by the agent appear here for review.")
            .css_classes(["compact", "dim-label"])
            .build();
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&list_page, Some("list"));
        stack.add_named(&diff_page, Some("diff"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&stack);

        let panel = Rc::new(Self {
            root,
            stack,
            lists,
            undo_turn,
            diff_toggle,
            diff,
            diff_stale: Cell::new(false),
            state: Rc::downgrade(state),
            shown: RefCell::new(None),
            pending: RefCell::new(None),
            groups: RefCell::new(Vec::new()),
        });
        let (weak, state_weak) = (Rc::downgrade(&panel), Rc::downgrade(state));
        panel.diff_toggle.connect_toggled(move |_| {
            if let (Some(panel), Some(state)) = (weak.upgrade(), state_weak.upgrade()) {
                panel.refresh(&state, &review_ui::review_summary(&state));
            }
        });
        // The rows and the plain diff are only built while the page is shown.
        let (weak, state_weak) = (Rc::downgrade(&panel), Rc::downgrade(state));
        panel.root.connect_map(move |_| {
            if let (Some(panel), Some(state)) = (weak.upgrade(), state_weak.upgrade()) {
                panel.show_pending(&state);
                if panel.diff_stale.get() {
                    panel.fill_diff(&state, &review_ui::reviewed_documents(&state.workspace));
                }
            }
        });
        panel
    }

    /// The page's root widget.
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The "Plain diff" toggle.
    pub fn diff_toggle(&self) -> &gtk::ToggleButton {
        &self.diff_toggle
    }

    /// The header's "Undo Last Turn" button (shown while some turn has pending changes).
    pub fn undo_turn_button(&self) -> &gtk::Button {
        &self.undo_turn
    }

    /// Title (relative path) and subtitle (counts) of every file row, in order (with turns, the
    /// rows of every group in turn).
    pub fn rows(&self) -> Vec<(String, String)> {
        if let Some(state) = self.state.upgrade() {
            self.show_pending(&state);
        }
        self.shown.borrow().as_ref().map_or_else(Vec::new, |m| {
            m.groups
                .iter()
                .flat_map(|g| g.rows.iter().map(|r| (r.title.clone(), r.subtitle.clone())))
                .collect()
        })
    }

    /// With turns, each group's title ("Turn 3 · 4 changes in 2 files") and its file rows; empty
    /// for the list without turns.
    pub fn groups(&self) -> Vec<(String, Vec<(String, String)>)> {
        if let Some(state) = self.state.upgrade() {
            self.show_pending(&state);
        }
        self.shown.borrow().as_ref().map_or_else(Vec::new, |m| {
            if !m.grouped {
                return Vec::new();
            }
            m.groups
                .iter()
                .map(|g| {
                    let rows = g
                        .rows
                        .iter()
                        .map(|r| (r.title.clone(), r.subtitle.clone()))
                        .collect();
                    (g.title.clone(), rows)
                })
                .collect()
        })
    }

    /// The text of the plain diff view (filled while the toggle is on).
    pub fn diff_text(&self) -> String {
        if self.diff_stale.get()
            && let Some(state) = self.state.upgrade()
        {
            self.fill_diff(&state, &review_ui::reviewed_documents(&state.workspace));
        }
        let (start, end) = self.diff.bounds();
        self.diff.text(&start, &end, false).into()
    }

    /// Shows `summary` (see [`review_ui::review_summary`]); while the page is hidden, only
    /// remembers it.
    pub fn refresh(&self, state: &Rc<WindowState>, summary: &ReviewSummary) {
        let model = model(state, summary);
        // The header's button follows at once, shown or not (one widget).
        match &model.latest {
            Some((_, label, running)) => {
                self.undo_turn.set_visible(true);
                self.undo_turn.set_sensitive(!running);
                let what = if *running {
                    format!("{label} is still running")
                } else {
                    format!("reject every change of {label}")
                };
                self.undo_turn
                    .set_tooltip_text(Some(&format!("Undo Last Turn: {what}")));
            }
            None => self.undo_turn.set_visible(false),
        }
        if self.shown.borrow().as_ref() == Some(&model) {
            self.pending.replace(None);
        } else if self.root.is_mapped() {
            self.pending.replace(None);
            self.update(state, &model);
            self.shown.replace(Some(model));
        } else {
            self.pending.replace(Some(model));
        }
        if self.diff_toggle.is_active() {
            if self.root.is_mapped() {
                self.fill_diff(state, &summary.docs);
            } else {
                // Built when the page is shown (every recompute refreshes the panel).
                self.diff_stale.set(true);
            }
            self.stack.set_visible_child_name("diff");
        } else {
            self.diff_stale.set(false);
            self.diff.set_text("");
            self.stack
                .set_visible_child_name(if summary.docs.is_empty() {
                    "empty"
                } else {
                    "list"
                });
        }
    }

    /// Shows the model a refresh left while the page was hidden.
    fn show_pending(&self, state: &Rc<WindowState>) {
        let pending = self.pending.take();
        if let Some(model) = pending {
            self.update(state, &model);
            self.shown.replace(Some(model));
        }
    }

    /// Makes the lists show `model`: groups and rows are kept by kind and file, those whose
    /// texts changed are updated in place, and only those that came or went are added or
    /// removed.
    fn update(&self, state: &Rc<WindowState>, model: &Model) {
        let lost_focus = Cell::new(false);
        // The focus must not follow a removed widget to whatever comes next (see the module
        // docs): it is let go first.
        let remove = |parent: &gtk::Widget, widget: &gtk::Widget| {
            let focus = gtk::prelude::GtkWindowExt::focus(&state.window);
            if focus.is_some_and(|f| &f == widget || f.is_ancestor(widget)) {
                gtk::prelude::GtkWindowExt::set_focus(&state.window, None::<&gtk::Widget>);
                lost_focus.set(true);
            }
            if let Some(list) = parent.downcast_ref::<gtk::ListBox>() {
                list.remove(widget);
            } else if let Some(lists) = parent.downcast_ref::<gtk::Box>() {
                lists.remove(widget);
            }
        };
        let lists = self.lists.clone().upcast::<gtk::Widget>();
        // Grouped and plain lists have different rows: nothing is kept between them.
        let regrouped = self
            .shown
            .borrow()
            .as_ref()
            .is_none_or(|shown| shown.grouped != model.grouped);
        let mut old = self.groups.take();
        let mut groups = Vec::with_capacity(model.groups.len());
        for group in &model.groups {
            let kept = (!regrouped)
                .then(|| old.iter().position(|g| g.model.kind == group.kind))
                .flatten()
                .map(|i| old.remove(i));
            let widgets = match kept {
                Some(mut widgets) => {
                    self.update_group(state, &mut widgets, group, model.grouped, &remove);
                    widgets
                }
                None => {
                    let widgets = new_group(state, group, model.grouped);
                    if let Some(header) = &widgets.header {
                        self.lists.append(header);
                    }
                    self.lists.append(&widgets.list);
                    widgets
                }
            };
            groups.push(widgets);
        }
        for gone in old {
            if let Some(header) = &gone.header {
                remove(&lists, header.upcast_ref());
            }
            remove(&lists, gone.list.upcast_ref());
        }
        // In the model's order: each group's header, then its list.
        let mut previous: Option<gtk::Widget> = None;
        for widgets in &groups {
            let parts = widgets
                .header
                .iter()
                .map(|h| h.clone().upcast::<gtk::Widget>())
                .chain([widgets.list.clone().upcast()]);
            for part in parts {
                self.lists.reorder_child_after(&part, previous.as_ref());
                previous = Some(part);
            }
        }
        let first_row = groups.iter().find_map(|g| g.rows.first().cloned());
        self.groups.replace(groups);
        if lost_focus.get() {
            // A row, whose activation only shows a change; never a decision button, where a
            // stray key would decide something.
            let focused = first_row.is_some_and(|row| row.grab_focus());
            if !focused && let Some(editor) = state.active_editor() {
                editor.view().grab_focus();
            }
        }
    }

    /// Makes `widgets` (a group of the same kind) show `group`. A new header replaces the old
    /// one when its texts or buttons change; rows are kept by file, their texts updated in
    /// place, and only the rows of files that came or went are added or removed (`remove` takes
    /// a widget out of its parent).
    fn update_group(
        &self,
        state: &Rc<WindowState>,
        widgets: &mut GroupWidgets,
        group: &GroupModel,
        grouped: bool,
        remove: &dyn Fn(&gtk::Widget, &gtk::Widget),
    ) {
        let old = &widgets.model;
        let (lists, list) = (
            self.lists.clone().upcast::<gtk::Widget>(),
            widgets.list.clone().upcast::<gtk::Widget>(),
        );
        // What the header shows and what the rows' buttons may do.
        let same_header =
            old.title == group.title && old.running == group.running && old.named == group.named;
        if !same_header {
            if let Some(header) = widgets.header.take() {
                remove(&lists, header.upcast_ref());
            }
            if grouped {
                let header = group_header(group);
                self.lists.append(&header);
                widgets.header = Some(header);
                widgets
                    .list
                    .update_property(&[gtk::accessible::Property::Label(&group.title)]);
            }
        }
        // A running turn's rows have their buttons off: rows of another state are new rows.
        let keep_rows = old.running == group.running;
        let mut rows: Vec<Option<adw::ActionRow>> = vec![None; group.rows.len()];
        let mut at = 0;
        for (row, widget) in old.rows.iter().zip(&widgets.rows) {
            // Both lists are by path: a kept row is found at or after the last one.
            let found = keep_rows
                .then(|| group.rows[at..].iter().position(|r| r.path == row.path))
                .flatten()
                .map(|i| at + i);
            match found {
                Some(i) => {
                    let new = &group.rows[i];
                    if new.title != row.title {
                        widget.set_title(&new.title);
                    }
                    if new.subtitle != row.subtitle {
                        widget.set_subtitle(&new.subtitle);
                    }
                    rows[i] = Some(widget.clone());
                    at = i + 1;
                }
                None => remove(&list, widget.upcast_ref()),
            }
        }
        let rows: Vec<adw::ActionRow> = rows
            .into_iter()
            .zip(&group.rows)
            .enumerate()
            .map(|(i, (kept, row))| {
                kept.unwrap_or_else(|| {
                    let widget = file_row(state, group, row);
                    widgets.list.insert(&widget, i32::try_from(i).unwrap_or(-1));
                    widget
                })
            })
            .collect();
        widgets.rows = rows;
        widgets.model = group.clone();
    }

    /// Fills the plain diff view with the unified diff of `docs`.
    fn fill_diff(&self, state: &WindowState, docs: &[DocRef]) {
        self.diff_stale.set(false);
        let mut text = String::new();
        for doc in docs {
            let name = review_ui::relative_name(state, doc.path());
            let review = doc.review.borrow();
            if let Some(review) = review.as_ref() {
                doc.with_text(|current| {
                    text.push_str(&lemma_review::unified_diff(
                        &name,
                        review.baseline(),
                        current,
                    ));
                });
            }
        }
        self.diff.set_text(&text);
    }
}

/// The model of `summary`.
fn model(state: &WindowState, summary: &ReviewSummary) -> Model {
    let grouped = summary.has_turns();
    let groups = if grouped {
        let turns = review_ui::turn_infos(state);
        summary
            .groups
            .iter()
            .map(|g| GroupModel {
                kind: g.kind,
                title: group_title(g),
                running: g.running,
                named: matches!(g.kind, GroupKind::Turn(t)
                    if turns.get(&t).is_some_and(|i| i.name.is_some())),
                rows: g
                    .files
                    .iter()
                    .map(|f| RowModel {
                        path: f.doc.path().to_path_buf(),
                        title: review_ui::relative_name(state, f.doc.path()),
                        subtitle: counts(f.hunks.len(), f.added, f.removed),
                        group: Some(g.kind),
                    })
                    .collect(),
            })
            .collect()
    } else if summary.docs.is_empty() {
        Vec::new()
    } else {
        vec![GroupModel {
            kind: GroupKind::Other,
            title: String::new(),
            running: false,
            named: false,
            rows: summary
                .docs
                .iter()
                .map(|doc| whole_file_row(state, doc))
                .collect(),
        }]
    };
    let latest = summary.latest_turn.map(|t| {
        let running = summary
            .groups
            .iter()
            .any(|g| g.kind == GroupKind::Turn(t) && g.running);
        (t, review_ui::turn_label(state, t), running)
    });
    Model {
        grouped,
        groups,
        latest,
    }
}

/// "Turn 3 · 4 changes in 2 files" (with "running" while the turn runs).
fn group_title(g: &TurnGroup) -> String {
    if g.running {
        format!("{} · running", g.title())
    } else {
        g.title()
    }
}

/// A row for every hunk of `doc` (the list without turns).
fn whole_file_row(state: &WindowState, doc: &Document) -> RowModel {
    let review = doc.review.borrow();
    let hunks = review.as_ref().map_or(&[][..], |r| r.hunks());
    let added: u32 = hunks.iter().map(|h| h.cur_lines.len() as u32).sum();
    let removed: u32 = hunks.iter().map(|h| h.base_lines.len() as u32).sum();
    RowModel {
        path: doc.path().to_path_buf(),
        title: review_ui::relative_name(state, doc.path()),
        subtitle: counts(hunks.len(), added, removed),
        group: None,
    }
}

/// "3 changes · +12 −4" (current lines added, baseline lines removed).
fn counts(n: usize, added: u32, removed: u32) -> String {
    let count = match n {
        1 => "1 change".to_owned(),
        n => format!("{n} changes"),
    };
    format!("{count} · +{added} −{removed}")
}

/// The widgets of a new group: its header (with turns), and its list of file rows.
fn new_group(state: &Rc<WindowState>, group: &GroupModel, grouped: bool) -> GroupWidgets {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let rows: Vec<adw::ActionRow> = group
        .rows
        .iter()
        .map(|row| {
            let widget = file_row(state, group, row);
            list.append(&widget);
            widget
        })
        .collect();
    if grouped {
        list.update_property(&[gtk::accessible::Property::Label(&group.title)]);
    }
    GroupWidgets {
        model: group.clone(),
        header: grouped.then(|| group_header(group)),
        list,
        rows,
    }
}

/// The heading of a group, with Keep Turn / Reject Turn for a turn.
fn group_header(group: &GroupModel) -> gtk::Box {
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_start(4)
        .margin_end(4)
        .build();
    let title = gtk::Label::builder()
        .label(&group.title)
        .css_classes(["heading"])
        .xalign(0.0)
        .wrap(true)
        .build();
    header.append(&title);
    match group.kind {
        GroupKind::Turn(turn) => {
            let button = |label: &str, action: &str, tooltip: &str| {
                let button = gtk::Button::builder()
                    .label(label)
                    .action_name(action)
                    .tooltip_text(tooltip)
                    .sensitive(!group.running)
                    .build();
                button.set_action_target_value(Some(&turn.to_variant()));
                button
            };
            let buttons = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .homogeneous(true)
                .build();
            let what = |verb: &str| {
                if group.running {
                    "The turn is still running".to_owned()
                } else if group.named {
                    format!("{verb} every change of this group, in every file")
                } else {
                    format!("{verb} every change this turn alone made, in every file")
                }
            };
            // A named turn (Replace in Project, Rename Label, a quick fix) is not the agent's
            // (and "Reject All" below the groups rejects every change).
            let (keep, reject) = if group.named {
                ("Keep", "Reject")
            } else {
                ("Keep Turn", "Reject Turn")
            };
            buttons.append(&button(keep, "win.review-keep-turn", &what("Accept")));
            let reject = button(reject, "win.review-reject-turn", &what("Reject"));
            reject.add_css_class("destructive-action");
            buttons.append(&reject);
            header.append(&buttons);
        }
        GroupKind::Mixed => {
            header.append(
                &gtk::Label::builder()
                    .label("Several turns edited these lines: decide them one by one or per file.")
                    .css_classes(["dim-label", "caption"])
                    .xalign(0.0)
                    .wrap(true)
                    .build(),
            );
        }
        GroupKind::Other => {}
    }
    header
}

fn file_row(state: &Rc<WindowState>, group: &GroupModel, row: &RowModel) -> adw::ActionRow {
    let widget = adw::ActionRow::builder()
        .title(&row.title)
        .subtitle(&row.subtitle)
        .use_markup(false)
        .activatable(true)
        .tooltip_text("Show the first change")
        .build();
    // A running turn's changes are decided once it has ended, as its header says ("· running").
    let running = matches!(row.group, Some(GroupKind::Turn(_))) && group.running;
    let (accept_tip, reject_tip) = match row.group {
        None => (
            "Accept all changes in this file",
            "Reject all changes in this file",
        ),
        Some(GroupKind::Turn(_)) => (
            "Keep this turn's changes in this file",
            "Reject this turn's changes in this file",
        ),
        Some(_) => (
            "Accept these changes in this file",
            "Reject these changes in this file",
        ),
    };
    let button = |icon: &str, tooltip: &str, decision: Decision| {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .sensitive(!running)
            .build();
        let (state, path, kind) = (Rc::downgrade(state), row.path.clone(), row.group);
        button.connect_clicked(move |_| {
            let Some((state, doc)) = document(&state, &path) else {
                return;
            };
            match kind {
                Some(GroupKind::Turn(turn)) if review_ui::is_turn_running(&state, turn) => {
                    let label = review_ui::turn_label(&state, turn);
                    state.toast(&format!("{label} is still running; stop it first"));
                }
                Some(kind) => {
                    let ids = review_ui::group_hunks(&doc, kind);
                    review_ui::decide_hunks(&state, &doc, &ids, decision);
                }
                None => review_ui::decide_document(&state, &doc, decision),
            }
        });
        button
    };
    widget.add_suffix(&button(
        "object-select-symbolic",
        accept_tip,
        Decision::Accept,
    ));
    widget.add_suffix(&button("edit-undo-symbolic", reject_tip, Decision::Reject));
    let (state, path, kind) = (Rc::downgrade(state), row.path.clone(), row.group);
    widget.connect_activated(move |_| {
        let Some((state, doc)) = document(&state, &path) else {
            return;
        };
        let ids = kind.map(|kind| review_ui::group_hunks(&doc, kind));
        let first = doc.review.borrow().as_ref().and_then(|r| {
            let mine = ids
                .as_ref()
                .and_then(|ids| ids.iter().filter_map(|id| r.hunk(*id)).next());
            mine.or_else(|| r.hunks().first())
                .map(|h| h.cur_lines.start)
        });
        if let Some(line) = first {
            state.sidebar.dismiss_overlay();
            review_ui::show_line(&state, &doc, line);
        }
    });
    widget
}

/// The window and its document of `path`, looked up when a row is used: rows stay while the
/// counts change, and the document of a path may be another one by then.
fn document(state: &Weak<WindowState>, path: &Path) -> Option<(Rc<WindowState>, DocRef)> {
    let state = state.upgrade()?;
    let doc = state.workspace.get(path)?;
    Some((state, doc))
}

/// Called once per window; fills `state.sidebar.changes` and registers the panel with the review
/// controller.
pub fn install(state: &Rc<WindowState>) {
    let panel = ChangesPanel::new(state);
    state.sidebar.changes.set_child(Some(panel.widget()));
    if let Some(review) = state.review.get() {
        review.set_changes_panel(panel);
    }
}
