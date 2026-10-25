//! The label tools in the Problems page: undefined references and citations are the
//! "References" section of the panel (`DiagSource::Labels`,
//! `DiagnosticsPanel::set_findings_many`, the files whose names changed in one batch; never
//! underlined by the panel: the label tools underline the names themselves), each with the
//! actions "Use ‘closest’" (a reviewable fix, when a label or key of a similar name exists) and
//! "Unused Labels and Entries…". The unused labels and bibliography entries are listed on
//! demand (`win.unused-labels`, also in the main menu's Go section and the command palette;
//! never underlined).

use super::scan::{self, Finding, NameKind};
use super::{display_path, unused};
use crate::diagnostics_panel::{self, FindingAction, Problem};
use crate::editor::goto::{self, GotoTarget};
use crate::review_ui;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::{DiagSource, Origin};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The action that lists the unused labels and entries.
pub const UNUSED_ACTION: &str = "unused-labels";

/// The labels state of the Problems page of one window.
#[derive(Default)]
struct Page {
    /// The undefined names, as problems, with what they are.
    problems: Vec<(Problem, NameKind, String)>,
    /// The files that have findings in the panel.
    files: BTreeSet<PathBuf>,
    /// The open list of unused names and what it shows.
    dialog: Option<(adw::Dialog, Vec<Finding>)>,
}

thread_local! {
    static PAGES: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Page)>> =
        const { RefCell::new(Vec::new()) };
}

fn with_page<R>(state: &WindowState, f: impl FnOnce(&mut Page) -> R) -> R {
    PAGES.with_borrow_mut(|pages| {
        pages.retain(|(w, _)| w.upgrade().is_some());
        let at = match pages
            .iter()
            .position(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
        {
            Some(at) => at,
            None => {
                pages.push((state.window.downgrade(), Page::default()));
                pages.len() - 1
            }
        };
        f(&mut pages[at].1)
    })
}

/// Adds `win.unused-labels` (main menu, Go section) and the actions of the References
/// section's rows.
pub(super) fn install(state: &Rc<WindowState>) {
    let action = gio::SimpleAction::new(UNUSED_ACTION, None);
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, _| {
        if let Some(state) = weak.upgrade() {
            show_unused(&state);
        }
    });
    state.window.add_action(&action);
    crate::app::describe_action(
        &format!("win.{UNUSED_ACTION}"),
        "Unused Labels and Entries",
        crate::app::ShortcutGroup::Navigation,
        None,
    );
    state.header.menu_go.append(
        Some("Unused Labels and Entries…"),
        Some(&format!("win.{UNUSED_ACTION}")),
    );
    if let Some(build) = state.build.get() {
        let weak = Rc::downgrade(state);
        build
            .diagnostics()
            .set_finding_actions(DiagSource::Labels, move |finding| {
                weak.upgrade()
                    .map_or_else(Vec::new, |state| finding_actions(&state, finding))
            });
    }
}

/// The problem of an undefined name (`finding.line` is 0-based) in the file `path` (its path,
/// normalized).
fn problem_of(finding: &Finding, path: PathBuf) -> Problem {
    let (message, context) = match finding.kind {
        NameKind::Label => (
            format!("Undefined reference ‘{}’", finding.name),
            "No \\label of this name in the project or its .aux file",
        ),
        NameKind::Cite => (
            format!("Undefined citation ‘{}’", finding.name),
            "No entry with this key in the project's bibliographies",
        ),
    };
    Problem {
        path,
        line: Some(finding.line + 1),
        severity: lemma_core::Severity::Warning,
        message,
        context: Some(context.to_owned()),
        source: DiagSource::Labels,
    }
}

/// Gives the Problems page the undefined names of the last check, of which those in the files
/// `changed` (named as the project index names them) changed since the one before: these
/// files' findings in the References section are replaced in one batch, and those of files
/// without any left are cleared. The other files' rows stay as they are, their lines following
/// the edits.
pub(super) fn publish(state: &Rc<WindowState>, undefined: &[Finding], changed: &BTreeSet<PathBuf>) {
    // Each file's path is normalized once, not once per name.
    fn normalize<'a>(normalized: &mut HashMap<&'a Path, PathBuf>, path: &'a Path) -> PathBuf {
        normalized
            .entry(path)
            .or_insert_with(|| crate::workspace::normalize(path))
            .clone()
    }
    let mut normalized: HashMap<&Path, PathBuf> = HashMap::new();
    let problems: Vec<(Problem, NameKind, String)> = undefined
        .iter()
        .map(|f| {
            let path = normalize(&mut normalized, &f.path);
            (problem_of(f, path), f.kind, f.name.clone())
        })
        .collect();
    let mut by_path: BTreeMap<&Path, Vec<&(Problem, NameKind, String)>> = BTreeMap::new();
    for problem in &problems {
        by_path.entry(&problem.0.path).or_default().push(problem);
    }
    let files: BTreeSet<PathBuf> = by_path.keys().map(|p| p.to_path_buf()).collect();
    let mut update: BTreeSet<PathBuf> = changed
        .iter()
        .map(|p| normalize(&mut normalized, p))
        .collect();
    with_page(state, |page| {
        page.problems = problems.clone();
        update.extend(page.files.difference(&files).cloned());
        page.files = files;
    });
    let Some(build) = state.build.get() else {
        return;
    };
    let updates: Vec<(PathBuf, Vec<diagnostics_panel::Finding>)> = update
        .into_iter()
        .map(|path| {
            let of_path = by_path.get(path.as_path()).map_or(&[][..], Vec::as_slice);
            // The names' columns in the live text of a document Lemma has (read once).
            let columns: Vec<Option<std::ops::Range<u32>>> = match state.workspace.get(&path) {
                Some(doc) if !of_path.is_empty() => doc.with_text(|text| {
                    let index = doc.line_index();
                    of_path
                        .iter()
                        .map(|(problem, _, name)| columns(text, &index, problem, name))
                        .collect()
                }),
                _ => vec![None; of_path.len()],
            };
            let findings = of_path
                .iter()
                .zip(columns)
                .map(|((problem, ..), columns)| diagnostics_panel::Finding {
                    columns,
                    problem: problem.clone(),
                    code: None,
                })
                .collect();
            (path, findings)
        })
        .collect();
    build
        .diagnostics()
        .set_findings_many(DiagSource::Labels, updates, None);
}

/// Where on its line the name of `problem` is (for the panel's navigation).
fn columns(
    text: &str,
    index: &lemma_core::LineIndex,
    problem: &Problem,
    name: &str,
) -> Option<std::ops::Range<u32>> {
    let line = problem.line?.checked_sub(1)?;
    if line >= index.line_count() {
        return None;
    }
    let text = &text[index.line_range(line)];
    let at = text.match_indices(name).find_map(|(at, _)| {
        let before = text[..at].trim_end();
        let after = text[at + name.len()..].trim_start();
        (before.ends_with(['{', ',', '[']) && after.starts_with(['}', ',', ']'])).then_some(at)
    })?;
    Some(at as u32..(at + name.len()) as u32)
}

/// The problems the label tools gave the Problems page (tests).
pub fn published(state: &WindowState) -> Vec<Problem> {
    with_page(state, |page| {
        page.problems.iter().map(|(p, ..)| p.clone()).collect()
    })
}

/// The actions of an undefined name's row: a fix when a similar name exists, and the list of
/// unused names.
fn finding_actions(
    state: &Rc<WindowState>,
    finding: &diagnostics_panel::Finding,
) -> Vec<FindingAction> {
    let known = with_page(state, |page| {
        page.problems
            .iter()
            .find(|(p, ..)| p.path == finding.problem.path && p.message == finding.problem.message)
            .map(|(_, kind, name)| (*kind, name.clone()))
    });
    let mut actions = Vec::new();
    if let Some((kind, name)) = known
        && let Some(closest) = closest_name(state, kind, &name)
    {
        let (from, to) = (name.clone(), closest.clone());
        actions.push(FindingAction {
            label: format!("Use ‘{closest}’"),
            tooltip: Some(format!(
                "Replace ‘{name}’ with ‘{closest}’ on this line, as a change to review"
            )),
            run: Rc::new(move |state, finding| {
                let (state, finding) = (state.clone(), finding.clone());
                let (from, to) = (from.clone(), to.clone());
                glib::spawn_future_local(async move {
                    if let Err(e) = replace_on_line(&state, &finding, kind, &from, &to).await {
                        state.toast(&e);
                    }
                });
            }),
        });
    }
    actions.push(FindingAction {
        label: "Unused Labels and Entries…".to_owned(),
        tooltip: Some("List the labels and bibliography entries the project never uses".into()),
        run: Rc::new(|state, _| show_unused(state)),
    });
    actions
}

/// The label (or citation key) of the project closest to `name` by edit distance, when it is
/// close enough to be a typo.
pub fn closest_name(state: &WindowState, kind: NameKind, name: &str) -> Option<String> {
    let index = state.workspace.index.borrow();
    let names: BTreeSet<&str> = match kind {
        NameKind::Label => index
            .files
            .values()
            .flat_map(|f| f.labels.iter().map(|l| l.name.as_str()))
            .chain(index.aux.labels.keys().map(String::as_str))
            .collect(),
        NameKind::Cite => index
            .bibs
            .values()
            .flat_map(|e| e.iter().map(|e| e.key.as_str()))
            .collect(),
    };
    let limit = (name.chars().count() / 4).clamp(1, 3);
    names
        .into_iter()
        .filter(|n| *n != name)
        .map(|n| (scan::edit_distance(n, name), n))
        .filter(|(d, _)| *d <= limit)
        .min()
        .map(|(_, n)| n.to_owned())
}

/// Replaces `from` with `to` in the reference or citation arguments of `finding`'s line, as a
/// reviewable change.
async fn replace_on_line(
    state: &Rc<WindowState>,
    finding: &diagnostics_panel::Finding,
    kind: NameKind,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let path = finding.problem.path.clone();
    let line = finding
        .problem
        .line
        .and_then(|l| l.checked_sub(1))
        .ok_or("The problem has no line")?;
    let doc = match state.workspace.get(&path) {
        Some(doc) => doc,
        None => state.workspace.get_or_load(&path).await?,
    };
    let config = state.workspace.scan_config();
    let edits: Vec<lemma_core::TextEdit> = doc.with_text(|text| {
        let found = scan::scan_occurrences(text, config, from, kind);
        let on_line: Vec<scan::Occurrence> = found
            .into_iter()
            .filter(|o| o.line == line && !o.definition)
            .collect();
        scan::rename_edits(&on_line, to)
    });
    if edits.is_empty() {
        return Err(format!("‘{from}’ is no longer on line {}", line + 1));
    }
    let clean = !doc.is_dirty();
    let turn = review_ui::begin_named_turn(state, &format!("Fix {from} → {to}"));
    review_ui::start_review(&doc, false);
    review_ui::in_turn(&doc, turn, || {
        doc.apply_edits(&edits, Origin::Agent { call: 0 })
    })
    .map_err(|e| format!("Could not fix the reference: {e}"))?;
    review_ui::recompute(&doc);
    if clean {
        doc.save()
            .await
            .map_err(|e| format!("Fixed, but saving failed: {e}"))?;
    }
    Ok(())
}

/// `win.unused-labels`: the labels no reference uses and the bibliography entries no citation
/// uses, in a dialog; choosing one goes there.
pub fn show_unused(state: &Rc<WindowState>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let found = unused(&state).await;
        present(&state, found);
    });
}

fn present(state: &Rc<WindowState>, found: Vec<Finding>) {
    close_unused(state);
    let dialog = adw::Dialog::builder()
        .title("Unused Labels and Entries")
        .content_width(520)
        .content_height(560)
        .build();
    dialog.add_css_class("lemma-unused");
    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let (labels, entries): (Vec<&Finding>, Vec<&Finding>) =
        found.iter().partition(|f| f.kind == NameKind::Label);
    if found.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .title("Nothing Unused")
            .description("Every label is referenced and every bibliography entry cited.")
            .build();
        view.set_content(Some(&status));
    } else {
        let page = adw::PreferencesPage::new();
        let index = state.workspace.index.borrow();
        // Looked up once for all the findings.
        let defined = scan::labels_by_name(&index);
        let namer = scan::LabelNamer::new(&index);
        for (title, items, description) in [
            (
                "Labels",
                &labels,
                "Labels that no \\ref, \\eqref, \\cref, … of the project uses",
            ),
            (
                "Bibliography Entries",
                &entries,
                "Entries that no citation of the project uses",
            ),
        ] {
            if items.is_empty() {
                continue;
            }
            let group = adw::PreferencesGroup::builder()
                .title(format!("{title} ({})", items.len()))
                .description(description)
                .build();
            for finding in items.iter() {
                let what = match finding.kind {
                    NameKind::Label => defined
                        .get(finding.name.as_str())
                        .map(|&(_, file, label)| namer.info(file, label).title()),
                    NameKind::Cite => {
                        scan::find_entry(&index, &finding.name).map(|(_, entry)| entry.kind.clone())
                    }
                };
                let place = format!(
                    "{}:{}",
                    display_path(state, &finding.path),
                    finding.line + 1
                );
                let subtitle = match what {
                    Some(what) => format!("{what} · {place}"),
                    None => place,
                };
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&finding.name).as_str())
                    .subtitle(glib::markup_escape_text(&subtitle).as_str())
                    .activatable(true)
                    .build();
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (weak, finding) = (Rc::downgrade(state), (*finding).clone());
                row.connect_activated(move |_| {
                    if let Some(state) = weak.upgrade() {
                        go(&state, &finding);
                    }
                });
                group.add(&row);
            }
            page.add(&group);
        }
        view.set_content(Some(&page));
    }
    dialog.set_child(Some(&view));
    let weak = Rc::downgrade(state);
    dialog.connect_closed(move |dialog| {
        if let Some(state) = weak.upgrade() {
            with_page(&state, |page| {
                if page.dialog.as_ref().is_some_and(|(d, _)| d == dialog) {
                    page.dialog = None;
                }
            });
        }
    });
    with_page(state, |page| page.dialog = Some((dialog.clone(), found)));
    dialog.present(Some(&state.window));
}

/// Goes to an unused name, closing the list.
fn go(state: &Rc<WindowState>, finding: &Finding) {
    close_unused(state);
    let target = match finding.kind {
        NameKind::Label => GotoTarget::Line {
            path: finding.path.clone(),
            line: finding.line,
            find: Some(format!("\\label{{{}}}", finding.name)),
        },
        NameKind::Cite => GotoTarget::Line {
            path: finding.path.clone(),
            line: finding.line,
            find: Some(finding.name.clone()),
        },
    };
    let state = state.clone();
    glib::spawn_future_local(async move {
        if let Err(e) = goto::navigate(&state, target).await {
            state.toast(&e);
        }
    });
}

/// Closes the list of unused names, if it is open.
pub fn close_unused(state: &WindowState) {
    let dialog = with_page(state, |page| page.dialog.take());
    if let Some((dialog, _)) = dialog {
        dialog.force_close();
    }
}

/// The open list of unused names (tests).
pub fn unused_dialog(state: &WindowState) -> Option<adw::Dialog> {
    with_page(state, |page| page.dialog.as_ref().map(|(d, _)| d.clone()))
}

/// What the open list shows (tests).
pub fn unused_rows(state: &WindowState) -> Vec<Finding> {
    with_page(state, |page| {
        page.dialog
            .as_ref()
            .map_or_else(Vec::new, |(_, rows)| rows.clone())
    })
}

/// Goes to row `index` of the open list (what activating it does; tests).
pub fn activate_unused(state: &Rc<WindowState>, index: usize) -> bool {
    let finding = with_page(state, |page| {
        page.dialog
            .as_ref()
            .and_then(|(_, rows)| rows.get(index).cloned())
    });
    match finding {
        Some(finding) => {
            go(state, &finding);
            true
        }
        None => false,
    }
}
