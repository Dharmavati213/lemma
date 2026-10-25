//! The Problems page's list: one `GtkListView` over a [`RowsModel`], which holds every row the
//! page lists (the build's problems, then each section of findings) in groups, one per file.
//! Each group is a section of the model, so the view gives it a header: the file and its count,
//! and above a section's first file the section's title and its count in all.
//!
//! The view makes widgets only for the rows around the ones on screen (GTK keeps about 200), and
//! [`RowsModel::update`] replaces only the groups that changed, one `items-changed` per run of
//! them: a file's new lint warnings or references cost the rows of that file, not of the page.
//! A row's widgets ([`RowWidgets`]) are bound to one [`Row`] at a time; what the row shows
//! comes from the panel when it is bound.

use crate::diagnostics_panel::{TOGGLE_WIDTH, display_message};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use lemma_core::{DiagSource, Severity};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What one row of the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Build problem `index` (of the panel's problems).
    Problem(usize),
    /// The line after the build's listed problems: how many it leaves out.
    More(usize),
    /// Finding `index` of the file `path` in `source`'s section.
    Finding(DiagSource, Rc<Path>, usize),
}

/// Where a group goes on the page, and which it is: groups of two updates with the same key are
/// the same file's rows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupKey {
    /// The `n`th run of one file's problems in the build's list.
    Build(usize),
    /// A file of a section of findings: the section's place on the page, whether the file is
    /// not the root document (which comes first), and the file.
    File(usize, bool, PathBuf),
}

/// The header of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// The file, relative to the project's directory.
    pub name: String,
    /// Its problems or findings (all of them, listed or not).
    pub count: usize,
    /// Above the first file of a section of findings: the section's title and its findings in
    /// all.
    pub section: Option<(&'static str, usize)>,
}

/// The rows of one file: one section of the model.
#[derive(Clone, Debug)]
pub struct Group {
    pub key: GroupKey,
    /// Changes whenever the rows show something else (a new build, the file's findings
    /// replaced): groups with equal keys, versions and numbers of rows have equal rows.
    pub version: u64,
    pub heading: Heading,
    pub rows: Vec<Row>,
}

/// One `items-changed` of [`RowsModel::update`]: the rows at `position..position + removed`
/// become `added` new ones, after which the groups before `old_end` of the old list and before
/// `new_end` of the new one are done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splice {
    pub position: u32,
    pub removed: u32,
    pub added: u32,
    pub old_end: usize,
    pub new_end: usize,
}

/// The splices that turn the groups `old` into `new`, each given as `(key, version, rows)` in
/// page order: one per run of groups that changed, went or came. `None` when the keys of either
/// list are not ascending (the update then replaces every row).
pub fn splices(old: &[(GroupKey, u64, u32)], new: &[(GroupKey, u64, u32)]) -> Option<Vec<Splice>> {
    let ascending = |groups: &[(GroupKey, u64, u32)]| groups.windows(2).all(|w| w[0].0 < w[1].0);
    if !ascending(old) || !ascending(new) {
        return None;
    }
    let mut out = Vec::new();
    let mut pending: Option<Splice> = None;
    let (mut i, mut j, mut position) = (0, 0, 0u32);
    while i < old.len() || j < new.len() {
        let order = match (old.get(i), new.get(j)) {
            (Some(o), Some(n)) => o.0.cmp(&n.0),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        let (removed, added) = match order {
            std::cmp::Ordering::Equal if old[i].1 == new[j].1 && old[i].2 == new[j].2 => {
                if let Some(done) = pending.take() {
                    position = done.position + done.added;
                    out.push(done);
                }
                position += old[i].2;
                i += 1;
                j += 1;
                continue;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                (old[i - 1].2, new[j - 1].2)
            }
            std::cmp::Ordering::Less => {
                i += 1;
                (old[i - 1].2, 0)
            }
            std::cmp::Ordering::Greater => {
                j += 1;
                (0, new[j - 1].2)
            }
        };
        let splice = pending.get_or_insert(Splice {
            position,
            removed: 0,
            added: 0,
            old_end: i,
            new_end: j,
        });
        splice.removed += removed;
        splice.added += added;
        splice.old_end = i;
        splice.new_end = j;
    }
    out.extend(pending);
    Some(out)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RowsModel {
        pub groups: RefCell<Vec<Rc<Group>>>,
        /// Where each group starts, then the number of rows.
        pub starts: RefCell<Vec<u32>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowsModel {
        const NAME: &'static str = "LemmaProblemRows";
        type Type = super::RowsModel;
        type Interfaces = (gio::ListModel, gtk::SectionModel);
    }

    impl ObjectImpl for RowsModel {}

    impl ListModelImpl for RowsModel {
        fn item_type(&self) -> glib::Type {
            glib::BoxedAnyObject::static_type()
        }

        fn n_items(&self) -> u32 {
            self.starts.borrow().last().copied().unwrap_or(0)
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let row = self.obj().row(position)?;
            Some(glib::BoxedAnyObject::new(row).upcast())
        }
    }

    impl SectionModelImpl for RowsModel {
        fn section(&self, position: u32) -> (u32, u32) {
            let starts = self.starts.borrow();
            let n = starts.last().copied().unwrap_or(0);
            if position >= n {
                return (n, u32::MAX);
            }
            // The last group starting at or before `position`.
            let group = starts.partition_point(|&s| s <= position) - 1;
            (starts[group], starts[group + 1])
        }
    }
}

glib::wrapper! {
    /// The rows of the Problems page, grouped by file (see the module docs).
    pub struct RowsModel(ObjectSubclass<imp::RowsModel>)
        @implements gio::ListModel, gtk::SectionModel;
}

impl Default for RowsModel {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl RowsModel {
    /// The row at `position`.
    pub fn row(&self, position: u32) -> Option<Row> {
        let (group, at) = self.locate(position)?;
        let groups = self.imp().groups.borrow();
        groups[group].rows.get(at).cloned()
    }

    /// The header of the group that holds `position`.
    pub fn heading(&self, position: u32) -> Option<Heading> {
        let (group, _) = self.locate(position)?;
        Some(self.imp().groups.borrow()[group].heading.clone())
    }

    /// The group that holds `position`, and the row's place in it.
    fn locate(&self, position: u32) -> Option<(usize, usize)> {
        let starts = self.imp().starts.borrow();
        if position >= starts.last().copied().unwrap_or(0) {
            return None;
        }
        let group = starts.partition_point(|&s| s <= position) - 1;
        Some((group, (position - starts[group]) as usize))
    }

    /// Shows `groups` (in page order) in place of the rows shown: unchanged groups keep their
    /// rows (and the view their widgets), each run of changed ones is one `items-changed`.
    pub fn update(&self, groups: Vec<Group>) {
        let new: Vec<Rc<Group>> = groups.into_iter().map(Rc::new).collect();
        let old = self.imp().groups.borrow().clone();
        let summary = |groups: &[Rc<Group>]| -> Vec<(GroupKey, u64, u32)> {
            groups
                .iter()
                .map(|g| (g.key.clone(), g.version, rows_of(g)))
                .collect()
        };
        let (old_summary, new_summary) = (summary(&old), summary(&new));
        let plan = splices(&old_summary, &new_summary);
        let plan = plan.unwrap_or_else(|| {
            vec![Splice {
                position: 0,
                removed: old_summary.iter().map(|g| g.2).sum(),
                added: new_summary.iter().map(|g| g.2).sum(),
                old_end: old.len(),
                new_end: new.len(),
            }]
        });
        // While each change is announced, the model holds the groups done so far and the old
        // ones after them, as the view expects.
        for splice in plan {
            let now: Vec<Rc<Group>> = new[..splice.new_end]
                .iter()
                .chain(&old[splice.old_end..])
                .cloned()
                .collect();
            self.set_groups(now);
            self.items_changed(splice.position, splice.removed, splice.added);
        }
        // The groups left are equal: only their headings can differ.
        self.set_groups(new);
    }

    fn set_groups(&self, groups: Vec<Rc<Group>>) {
        let mut starts = Vec::with_capacity(groups.len() + 1);
        let mut at = 0u32;
        for group in &groups {
            starts.push(at);
            at += rows_of(group);
        }
        starts.push(at);
        self.imp().starts.replace(starts);
        self.imp().groups.replace(groups);
    }
}

fn rows_of(group: &Group) -> u32 {
    u32::try_from(group.rows.len()).unwrap_or(u32::MAX)
}

/// What a row's details toggle does when it is pressed (`true`) or released.
type ToggledFn = dyn Fn(&RowWidgets, bool);

/// The widgets of one row, which the view recycles: they show one [`Row`] at a time. The
/// details toggle, the details and the line of a [`Row::More`] are made when a row first needs
/// them: most rows (bad boxes, warnings) have none, and the view keeps about 200 rows' widgets.
pub struct RowWidgets {
    /// The list item's child.
    pub root: gtk::Box,
    pub icon: gtk::Image,
    message: gtk::Label,
    location: gtk::Label,
    content: gtk::Box,
    /// Holds the details toggle, and is as wide as it in rows without one, so the line numbers
    /// align.
    toggle_slot: gtk::Box,
    toggle: RefCell<Option<gtk::ToggleButton>>,
    on_toggled: RefCell<Option<Box<ToggledFn>>>,
    /// Reveals the details (an explanation, actions) below the row.
    details: RefCell<Option<gtk::Revealer>>,
    more: RefCell<Option<gtk::Label>>,
    /// The row shown, while bound.
    pub row: RefCell<Option<Row>>,
    /// True while a bind sets the toggle: its handler then does nothing.
    binding: Cell<bool>,
    me: std::rc::Weak<RowWidgets>,
}

impl RowWidgets {
    pub fn new() -> Rc<Self> {
        let icon = gtk::Image::builder().valign(gtk::Align::Start).build();
        let message = gtk::Label::builder()
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .lines(3)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        let location = gtk::Label::builder()
            .valign(gtk::Align::Start)
            .xalign(1.0)
            .width_chars(4)
            .tooltip_text("Line")
            .css_classes(["dim-label", "numeric"])
            .build();
        let toggle_slot = gtk::Box::builder()
            .width_request(TOGGLE_WIDTH)
            .valign(gtk::Align::Start)
            .build();
        // Centred as a whole in the row (single-line messages sit level with their icon);
        // within it everything aligns to the first line.
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&icon);
        content.append(&message);
        content.append(&location);
        content.append(&toggle_slot);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("lemma-problem-row");
        root.set_valign(gtk::Align::Center);
        root.append(&content);
        Rc::new_cyclic(|me| Self {
            root,
            icon,
            message,
            location,
            content,
            toggle_slot,
            toggle: RefCell::new(None),
            on_toggled: RefCell::new(None),
            details: RefCell::new(None),
            more: RefCell::new(None),
            row: RefCell::new(None),
            binding: Cell::new(false),
            me: me.clone(),
        })
    }

    /// Calls `f` when the user presses (`true`) or releases the details toggle.
    pub fn connect_toggled(&self, f: impl Fn(&RowWidgets, bool) + 'static) {
        self.on_toggled.replace(Some(Box::new(f)));
    }

    /// Shows `problem`, with `tooltip`.
    pub fn show_problem(&self, problem: &crate::diagnostics_panel::Problem, tooltip: &str) {
        let (icon, class) = match problem.severity {
            Severity::Error => ("dialog-error-symbolic", "error"),
            Severity::Warning => ("dialog-warning-symbolic", "warning"),
            Severity::BadBox => ("format-justify-fill-symbolic", "dim-label"),
            Severity::Info => ("dialog-information-symbolic", "dim-label"),
        };
        self.icon.set_icon_name(Some(icon));
        self.icon.set_css_classes(&[class]);
        self.message.set_label(display_message(&problem.message));
        let location = match (problem.line, problem.source) {
            (Some(line), _) => line.to_string(),
            (None, DiagSource::Biber) => "biber".to_owned(),
            (None, DiagSource::Bibtex) => "bibtex".to_owned(),
            (None, _) => String::new(),
        };
        self.location.set_label(&location);
        self.root.set_tooltip_text(Some(tooltip));
        self.content.set_visible(true);
        if let Some(more) = self.more.borrow().as_ref() {
            more.set_visible(false);
        }
    }

    /// Shows the line counting the problems the list leaves out.
    pub fn show_more(&self, left_out: usize) {
        let more = self
            .more
            .borrow_mut()
            .get_or_insert_with(|| {
                let more = gtk::Label::builder()
                    .xalign(0.0)
                    .wrap(true)
                    .css_classes(["dim-label", "lemma-problems-more"])
                    .build();
                self.root.append(&more);
                more
            })
            .clone();
        more.set_label(&format!("{left_out} more not listed"));
        more.set_visible(true);
        self.content.set_visible(false);
        self.root.set_tooltip_text(None);
        self.set_toggle(None);
        self.bind_details(None);
    }

    /// Shows the details toggle, pressed when `Some(true)`, or nothing in its place.
    pub fn set_toggle(&self, toggle: Option<bool>) {
        let Some(active) = toggle else {
            if let Some(button) = self.toggle.borrow().as_ref() {
                button.set_visible(false);
            }
            return;
        };
        let button = self.toggle_button();
        button.set_visible(true);
        self.binding.set(true);
        button.set_active(active);
        self.binding.set(false);
        button.set_icon_name(if active {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
    }

    /// The details toggle, made the first time a row has details.
    fn toggle_button(&self) -> gtk::ToggleButton {
        if let Some(button) = self.toggle.borrow().as_ref() {
            return button.clone();
        }
        let button = gtk::ToggleButton::builder()
            .icon_name("pan-end-symbolic")
            .valign(gtk::Align::Start)
            .tooltip_text("Explain and Fix")
            .css_classes(["flat", "circular", "lemma-problem-toggle"])
            .build();
        let me = self.me.clone();
        button.connect_toggled(move |button| {
            if let Some(widgets) = me.upgrade()
                && !widgets.binding.get()
                && let Some(f) = widgets.on_toggled.borrow().as_ref()
            {
                f(&widgets, button.is_active());
            }
        });
        self.toggle_slot.append(&button);
        self.toggle.replace(Some(button.clone()));
        button
    }

    /// The revealer of the details, made the first time a row shows details.
    fn revealer(&self) -> gtk::Revealer {
        self.details
            .borrow_mut()
            .get_or_insert_with(|| {
                let revealer = gtk::Revealer::new();
                self.root.insert_child_after(&revealer, Some(&self.content));
                revealer
            })
            .clone()
    }

    /// Shows `content` below the row at once (a bind), or nothing when `None` (collapsed).
    pub fn bind_details(&self, content: Option<gtk::Widget>) {
        if content.is_none() && self.details.borrow().is_none() {
            return;
        }
        let details = self.revealer();
        details.set_transition_type(gtk::RevealerTransitionType::None);
        details.set_reveal_child(content.is_some());
        details.set_child(content.as_ref());
    }

    /// Slides the details in or out (a click on the toggle): `content` replaces what is below
    /// the row when it is `Some`.
    pub fn reveal_details(&self, content: Option<gtk::Widget>, revealed: bool) {
        let details = self.revealer();
        if let Some(content) = content {
            details.set_child(Some(&content));
        }
        details.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        details.set_reveal_child(revealed);
        if let Some(button) = self.toggle.borrow().as_ref() {
            button.set_icon_name(if revealed {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            });
        }
    }

    /// True while the row has details below it.
    pub fn has_details(&self) -> bool {
        self.details
            .borrow()
            .as_ref()
            .is_some_and(|d| d.child().is_some())
    }
}

/// The widgets of a group's header.
pub struct HeaderWidgets {
    /// The list header's child.
    pub root: gtk::Box,
    section: gtk::Box,
    section_title: gtk::Label,
    section_count: gtk::Label,
    name: gtk::Label,
    count: gtk::Label,
}

impl HeaderWidgets {
    pub fn new() -> Rc<Self> {
        let section_title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let section_count = gtk::Label::builder()
            .css_classes(["dim-label", "numeric", "caption"])
            .build();
        let section = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        section.add_css_class("lemma-problems-heading");
        section.append(&section_title);
        section.append(&section_count);
        let name = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build();
        let count = gtk::Label::builder()
            .css_classes(["dim-label", "numeric", "caption"])
            .build();
        let file = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        file.add_css_class("lemma-problems-header");
        file.append(&name);
        file.append(&count);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&section);
        root.append(&file);
        Rc::new(Self {
            root,
            section,
            section_title,
            section_count,
            name,
            count,
        })
    }

    /// Shows `heading`; `first` is the list's first header (with less space above it).
    pub fn show(&self, heading: &Heading, first: bool) {
        match heading.section {
            Some((title, total)) => {
                self.section_title.set_label(title);
                self.section_count.set_label(&total.to_string());
                self.section.set_visible(true);
            }
            None => self.section.set_visible(false),
        }
        if first {
            self.section.add_css_class("first");
        } else {
            self.section.remove_css_class("first");
        }
        self.name.set_label(&heading.name);
        self.count.set_label(&heading.count.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: usize) -> GroupKey {
        GroupKey::File(1, true, PathBuf::from(format!("/p/{n:02}.tex")))
    }

    fn plan(old: &[(usize, u64, u32)], new: &[(usize, u64, u32)]) -> Option<Vec<Splice>> {
        let keyed = |groups: &[(usize, u64, u32)]| -> Vec<(GroupKey, u64, u32)> {
            groups.iter().map(|g| (key(g.0), g.1, g.2)).collect()
        };
        splices(&keyed(old), &keyed(new))
    }

    fn splice(position: u32, removed: u32, added: u32, ends: (usize, usize)) -> Splice {
        Splice {
            position,
            removed,
            added,
            old_end: ends.0,
            new_end: ends.1,
        }
    }

    #[test]
    fn only_changed_groups_are_spliced() {
        // Files 1..=5 with 3 rows each; file 2 gets a fifth row, file 4 goes, file 6 comes.
        let old = [(1, 1, 3), (2, 2, 3), (3, 3, 3), (4, 4, 3), (5, 5, 3)];
        let new = [(1, 1, 3), (2, 9, 5), (3, 3, 3), (5, 5, 3), (6, 10, 2)];
        assert_eq!(
            plan(&old, &new),
            Some(vec![
                splice(3, 3, 5, (2, 2)),
                splice(11, 3, 0, (4, 3)),
                splice(14, 0, 2, (5, 5)),
            ])
        );
        // Nothing changed: nothing to announce.
        assert_eq!(plan(&old, &old), Some(Vec::new()));
        // Adjacent changes are one splice; from and to nothing.
        let next = [(1, 1, 3), (2, 7, 1), (3, 8, 1), (5, 5, 3)];
        assert_eq!(plan(&old, &next), Some(vec![splice(3, 9, 2, (4, 3))]));
        assert_eq!(plan(&[], &old), Some(vec![splice(0, 0, 15, (0, 5))]));
        assert_eq!(plan(&old, &[]), Some(vec![splice(0, 15, 0, (5, 0))]));
        // The same file with a new version but the same rows is new: its findings changed.
        let renewed = [(1, 11, 3), (2, 2, 3), (3, 3, 3), (4, 4, 3), (5, 5, 3)];
        assert_eq!(plan(&old, &renewed), Some(vec![splice(0, 3, 3, (1, 1))]));
        // Keys out of order: no plan (everything is replaced).
        assert_eq!(plan(&[(2, 1, 1), (1, 1, 1)], &old), None);
    }

    #[test]
    fn build_groups_come_before_the_sections() {
        let build = GroupKey::Build(7);
        let lint = GroupKey::File(1, false, PathBuf::from("/p/z.tex"));
        let labels = GroupKey::File(2, false, PathBuf::from("/p/a.tex"));
        let root_last = GroupKey::File(1, true, PathBuf::from("/p/a.tex"));
        assert!(build < lint && lint < root_last && root_last < labels);
    }
}
