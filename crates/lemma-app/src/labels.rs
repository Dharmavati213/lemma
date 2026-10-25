//! Label tools: go to definition and hovers for references, citations,
//! included files and user macros, find references, rename a label, undefined and unused names.
//!
//! - **Go to definition** (`editor::goto` resolvers, [`resolve::goto_target`]): `\ref`,
//!   `\eqref`, `\cref` and the rest of the family (`\crefrange` too) go to the `\label`;
//!   citation keys to their `.bib` entry; `\input`, `\include`, `\subfile`, `\import` and
//!   `\bibliography` to the file; `\includegraphics` opens the image in the system viewer after
//!   a confirmation; `\usepackage` of a local `.sty` (and `\documentclass` of a local `.cls`) to
//!   that file; a user macro to its `\newcommand`, `\DeclareMathOperator`, `\def`, …. On the
//!   command name itself (`\eqref` with the cursor on its letters) the first name of its
//!   argument counts.
//! - **Hovers** (`editor::hover` providers, [`resolve::hover_content`]): a reference shows what
//!   it refers to ("Theorem 3.2 · page 2" from the `.aux` file) and the first three source lines
//!   of the labelled environment — plus, when the PDF preview has a SyncTeX box for the label,
//!   a 2× crop of that region of the PDF; a label shows its number and how often it is used; a
//!   citation key its authors, year and title (or that no entry has it); a user macro its
//!   definition. In math, with the math preview on, the pointer resting on a user macro shows
//!   the formula instead (the keyboard hover, `K`/Alt+K, still shows the definition).
//! - **Find references** ([`find_references`], `win.find-references`, Shift+F12): every use
//!   of the label or citation key at the cursor across the project, in a popover list at the
//!   cursor; Enter or a click jumps there through `editor::goto::navigate` (so Back returns).
//! - **Rename label** ([`rename_label`], `win.rename-label`, F2 on a `\label` or a reference):
//!   a popover asks for the new name and checks it (characters TeX does not allow, a name the
//!   project already uses); the renames in every project file are applied **as reviewable
//!   hunks** through the review machinery (`review_ui::begin_named_turn`, origin "Rename … → …",
//!   shown in the banner and the Changes panel), so each can be accepted or rejected, or all
//!   at once. Files that were saved are saved again; files with unsaved changes stay unsaved.
//! - **Undefined** references and citations ([`undefined`]) get a wavy underline (tag
//!   [`UNDEFINED_TAG`]) on the name and a Problems entry with `DiagSource::Labels`. They are
//!   computed from the project index plus the `.aux` file, so they work before a build too, a
//!   moment after typing stops (once the project index has caught up; the project's words,
//!   rebuilt later, are not waited for), and after another program changed a file the index
//!   has (a re-exported `.bib`, a chapter changed by `git checkout`). Citations are checked
//!   only when every bibliography the project names could be read, references only when the
//!   index has every file the project includes (not in a project with more files than a window
//!   reads from disk, `completion::MAX_DISK_FILES`; the log says so once). A check looks again
//!   only at what the project index replaced since the last one ([`scan::UndefinedCheck`]):
//!   after prose typed, nothing; after an edit of a chapter, that chapter and the references
//!   to a label it added or removed.
//! - **Unused** labels and bibliography entries ([`unused`]) are listed on demand, never
//!   underlined: `win.unused-labels` ("Unused Labels and Entries…" in the main menu's Go
//!   section, the command palette, and the actions of every row of the Problems page's
//!   References section, which also offer "Use ‘closest’" for a typo).
//!
//! The inline equation numbers are `editor::annotations`; they are refreshed with the checks.
//!
//! Hooks: [`install`] once per window; [`attach`] / [`detach`] per editor tab (after the hover
//! and goto registries exist; detach before they go); the actions call [`find_references`] and
//! [`rename_label`]; [`run_hover_ref_demo`] is `--demo hover-ref` (resolves when the hover of a
//! `\ref` is shown, for `j-hover-ref.png`; with `LEMMA_DEMO_HOVER=0` it shows the equation
//! numbers of the same place without a hover, for `j-eqnumbers.png`).

mod crop;
mod demo;
mod hovers;
mod problems;
mod references;
mod rename;
pub mod resolve;
pub mod scan;

pub use crop::{stop_worker as stop_crop_worker, worker_running as crop_worker_running};
pub use demo::run_hover_ref_demo;
pub use problems::{
    activate_unused, close_unused, closest_name, published, show_unused, unused_dialog, unused_rows,
};
pub use references::{
    ReferenceRow, activate_reference, close_references, find_references, reference_rows,
    references, references_popover,
};
pub use rename::{
    RenameOutcome, close_rename, rename, rename_entry, rename_label, rename_popover, rename_status,
    submit_rename,
};
pub use scan::{Finding, NameKind};

use crate::document::{DocRef, ObserverId};
use crate::editor::{Editor, TextPoint, annotations, completion, remove_tag_everywhere};
use crate::window::WindowState;
use gtk::prelude::*;
use lemma_syntax::TokenKind;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// The tag of the wavy underline on undefined references and citations.
pub const UNDEFINED_TAG: &str = "lemma-label-undefined";
/// How long after the last change the checks run (the project index re-indexes a document
/// 300 ms after its last change; the checks then wait until it is idle, `completion::
/// index_busy`).
pub const CHECK_DELAY: Duration = Duration::from_millis(450);
/// How often a check waiting for the project index looks again.
const BUSY_RETRY: Duration = Duration::from_millis(150);
/// The colour of the underline: the warning yellow of the Problems page (an undefined
/// reference is a LaTeX warning).
const UNDERLINE_RGBA: (f32, f32, f32, f32) = (0.90, 0.65, 0.04, 1.0);

/// The label tools of one window.
struct Labels {
    state: Weak<WindowState>,
    /// The undefined names of the last check, by path.
    undefined: RefCell<BTreeMap<PathBuf, Vec<Finding>>>,
    /// What they are computed from, kept between checks (see [`scan::UndefinedCheck`]).
    scan: RefCell<scan::UndefinedCheck>,
    /// Citations were checked by the last check.
    cites_checked: Cell<bool>,
    /// The log said that references are not checked (the project has more files than the
    /// index reads from disk).
    said_capped: Cell<bool>,
    /// The checks run once the timer fires and the project index is idle.
    timer: RefCell<Option<glib::SourceId>>,
    /// Checks done (tests wait for them), and those that found nothing changed since the last
    /// one.
    checks: Cell<u64>,
    skipped: Cell<u64>,
    editors: RefCell<Vec<Attached>>,
}

/// What [`attach`] connected for one editor.
struct Attached {
    editor: Weak<Editor>,
    doc: Weak<crate::document::Document>,
    changed: ObserverId,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<Labels>>> = const { RefCell::new(Vec::new()) };
}

fn labels_of(state: &WindowState) -> Option<Rc<Labels>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.state.strong_count() > 0);
        windows
            .iter()
            .find(|w| std::ptr::eq(w.state.as_ptr(), state))
            .cloned()
    })
}

/// Called once per window after the M2 install hooks: the checks after builds and
/// configuration changes, and the styles of the hovers and popovers.
pub fn install(state: &Rc<WindowState>) {
    hovers::install_css();
    problems::install(state);
    let labels = Rc::new(Labels {
        state: Rc::downgrade(state),
        undefined: RefCell::new(BTreeMap::new()),
        scan: RefCell::new(scan::UndefinedCheck::default()),
        cites_checked: Cell::new(false),
        said_capped: Cell::new(false),
        timer: RefCell::new(None),
        checks: Cell::new(0),
        skipped: Cell::new(0),
        editors: RefCell::new(Vec::new()),
    });
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|w| w.state.strong_count() > 0);
        windows.push(labels);
    });
    if let Some(build) = state.build.get() {
        let weak = Rc::downgrade(state);
        build.connect_finished(move |_| {
            if let Some(state) = weak.upgrade() {
                // The project feed reads the new `.aux` file now; the check waits for it.
                schedule_check(&state, Duration::from_millis(100));
            }
        });
    }
    state.connect_config_changed(|state, file| {
        if file == crate::settings::ConfigFile::Config {
            for editor in state.editors() {
                annotations::refresh(state, &editor);
            }
        }
    });
    state.connect_root_changed(|state| schedule_check(state, CHECK_DELAY));
    let weak = Rc::downgrade(state);
    let saved_hook = crate::saver::connect_saved(move |path, text| {
        if let Some(state) = weak.upgrade() {
            saved(&state, path, text);
        }
    });
    state
        .window
        .connect_destroy(move |_| crate::saver::disconnect_saved(saved_hook));
}

/// A document Lemma saved that has no editor tab (a rename, a rejected hunk, the agent's edit
/// of a closed file) or a bibliography: the project index still has what it read from disk
/// before (it re-reads only open documents as they change), so the saved text is indexed
/// again — the file's structure, or the `.bib` entries — and the checks run.
fn saved(state: &Rc<WindowState>, path: &Path, text: &str) {
    let (bib, file, theorems) = {
        let index = state.workspace.index.borrow();
        let open = state.workspace.get(path).is_some_and(|d| d.is_open());
        (
            index.bibs.contains_key(path),
            index.files.contains_key(path) && !open,
            index.theorem_names(),
        )
    };
    if !bib && !file {
        return;
    }
    let (path, text) = (path.to_path_buf(), text.to_owned());
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let indexed = gio::spawn_blocking(move || {
            if bib {
                (Some(lemma_syntax::scan_bib(&text)), None)
            } else {
                (
                    None,
                    Some(lemma_syntax::index_file_with_theorems(&text, &theorems)),
                )
            }
        })
        .await;
        let (Some(state), Ok((entries, file))) = (weak.upgrade(), indexed) else {
            return;
        };
        {
            let mut index = state.workspace.index.borrow_mut();
            if let Some(entries) = entries
                && index.bibs.contains_key(&path)
            {
                index
                    .bibs
                    .insert(path.clone(), std::sync::Arc::new(entries));
            }
            let open = state.workspace.get(&path).is_some_and(|d| d.is_open());
            if let Some(file) = file
                && index.files.contains_key(&path)
                && !open
            {
                index.update_file(path, std::sync::Arc::new(file));
            }
        }
        schedule_check(&state, CHECK_DELAY);
    });
}

/// Called for every new editor tab: its document's changes schedule the checks, and the
/// undefined names known already are underlined in it.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(labels) = labels_of(state) else {
        return;
    };
    let doc = editor.document();
    let weak = Rc::downgrade(state);
    let changed = doc.connect_changed(move |_, _, _| {
        // Inside the buffer's signal emission: only a timer is (re)started.
        if let Some(state) = weak.upgrade() {
            schedule_check(&state, CHECK_DELAY);
        }
    });
    labels.editors.borrow_mut().push(Attached {
        editor: Rc::downgrade(editor),
        doc: Rc::downgrade(doc),
        changed,
    });
    underline(&labels, editor);
    // Findings published while the document was closed and not in the workspace have no
    // lines on the Problems page once it opens (they had nothing to follow the edits with): its
    // findings are published again, with the lines the project index has now.
    if !findings_for(&labels, doc.path()).is_empty() {
        let path = crate::workspace::normalize(doc.path());
        publish_problems(state, &labels, &BTreeSet::from([path]));
    }
    schedule_check(state, CHECK_DELAY);
}

/// Called when an editor tab closes: disconnects what [`attach`] connected and closes the
/// popovers shown in it.
pub fn detach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    references::close_for(state, editor);
    rename::close_for(state, editor);
    let Some(labels) = labels_of(state) else {
        return;
    };
    let removed: Vec<Attached> = {
        let mut editors = labels.editors.borrow_mut();
        let (gone, kept): (Vec<Attached>, Vec<Attached>) = editors
            .drain(..)
            .partition(|a| a.editor.upgrade().is_none_or(|e| Rc::ptr_eq(&e, editor)));
        *editors = kept;
        gone
    };
    for attached in removed {
        if let Some(doc) = attached.doc.upgrade() {
            doc.disconnect(attached.changed);
        }
    }
    // The buffer can outlive the tab as a closed document's text.
    let buffer = editor.buffer();
    if let Some(tag) = buffer.tag_table().lookup(UNDEFINED_TAG) {
        remove_tag_everywhere(buffer, &tag);
    }
}

// ----- the checks -----

/// Runs the checks `delay` from now (restarting a waiting timer).
fn schedule_check(state: &Rc<WindowState>, delay: Duration) {
    let Some(labels) = labels_of(state) else {
        return;
    };
    if let Some(timer) = labels.timer.take() {
        timer.remove();
    }
    let weak = Rc::downgrade(&labels);
    let timer = glib::timeout_add_local_once(delay, move || {
        let Some(labels) = weak.upgrade() else {
            return;
        };
        labels.timer.take();
        let Some(state) = labels.state.upgrade() else {
            return;
        };
        // Only the index counts: the words rebuilt 1.5 s after typing stops do not change it.
        if completion::index_busy(&state) {
            schedule_check(&state, BUSY_RETRY);
            return;
        }
        check(&state, &labels);
    });
    labels.timer.replace(Some(timer));
}

/// The project index reads files again that changed on disk (`completion::disk_changed`): the
/// checks run once it has caught up.
pub fn index_changing(state: &Rc<WindowState>) {
    schedule_check(state, BUSY_RETRY);
}

/// Runs the checks now, whether or not the project index is busy (tests).
pub fn check_now(state: &Rc<WindowState>) {
    if let Some(labels) = labels_of(state) {
        if let Some(timer) = labels.timer.take() {
            timer.remove();
        }
        check(state, &labels);
    }
}

/// True while a check is scheduled or waits for the project index (tests).
pub fn check_pending(state: &WindowState) -> bool {
    labels_of(state).is_some_and(|l| l.timer.borrow().is_some())
}

/// How many checks ran in `state`'s window (tests), skipped ones too.
pub fn checks_done(state: &WindowState) -> u64 {
    labels_of(state).map_or(0, |l| l.checks.get())
}

/// How many of the checks found nothing their findings come from changed since the one before
/// (prose typed), so they looked at nothing again (tests).
pub fn checks_skipped(state: &WindowState) -> u64 {
    labels_of(state).map_or(0, |l| l.skipped.get())
}

/// How many times the checks looked at the references or citations of a file, in all (tests).
pub fn files_checked(state: &WindowState) -> u64 {
    labels_of(state).map_or(0, |l| l.scan.borrow().files_checked())
}

fn check(state: &Rc<WindowState>, labels: &Rc<Labels>) {
    labels.checks.set(labels.checks.get() + 1);
    // Files the project includes past the cap of files read from disk are not indexed: a
    // reference to a label in one of them would look undefined.
    let complete = !completion::index_capped(state);
    if !complete && !labels.said_capped.replace(true) {
        tracing::info!(
            "the project has more files than the {} read from disk: undefined references are \
             not checked",
            completion::MAX_DISK_FILES
        );
    }
    if !labels
        .scan
        .borrow_mut()
        .update(&state.workspace.index.borrow(), complete)
    {
        // Nothing the findings come from changed (prose typed): the underlines and the
        // Problems page stand. The equation numbers look at their own inputs.
        labels.skipped.set(labels.skipped.get() + 1);
        for editor in state.editors() {
            annotations::refresh(state, &editor);
        }
        return;
    }
    let by_path = labels.scan.borrow().by_path();
    labels
        .cites_checked
        .set(labels.scan.borrow().cites_checked());
    let changed_paths: BTreeSet<PathBuf> = {
        let old = labels.undefined.borrow();
        old.keys()
            .chain(by_path.keys())
            .filter(|p| old.get(*p) != by_path.get(*p))
            .cloned()
            .collect()
    };
    labels.undefined.replace(by_path);
    for editor in state.editors() {
        // The underlines follow the text by themselves; only changed findings are applied
        // again (names typed since are found again from the live text).
        let path = crate::workspace::normalize(editor.document().path());
        if changed_paths.contains(&path) || changed_paths.contains(editor.document().path()) {
            underline(labels, &editor);
        }
        annotations::refresh(state, &editor);
    }
    if !changed_paths.is_empty() {
        publish_problems(state, labels, &changed_paths);
    }
}

/// The undefined references and citations of the last check, by path and line.
pub fn undefined(state: &WindowState) -> Vec<Finding> {
    labels_of(state).map_or_else(Vec::new, |labels| {
        labels
            .undefined
            .borrow()
            .values()
            .flatten()
            .cloned()
            .collect()
    })
}

/// Whether the last check looked at citations (every bibliography could be read).
pub fn cites_checked(state: &WindowState) -> bool {
    labels_of(state).is_some_and(|l| l.cites_checked.get())
}

/// Whether the last check looked at references (the project index has every file of the
/// project, see `completion::index_capped`).
pub fn labels_checked(state: &WindowState) -> bool {
    labels_of(state).is_some_and(|l| l.scan.borrow().labels_checked())
}

/// The findings of `path` (the index keys paths as the workspace does, or lexically).
fn findings_for(labels: &Labels, path: &Path) -> Vec<Finding> {
    let undefined = labels.undefined.borrow();
    undefined
        .get(path)
        .or_else(|| undefined.get(&crate::workspace::normalize(path)))
        .cloned()
        .unwrap_or_default()
}

/// Underlines the undefined names of `editor`'s document in its live text: on the line the
/// project index gives (it has just caught up when the checks run), else anywhere in the text.
/// The text is read once (only when the findings of the document changed).
fn underline(labels: &Labels, editor: &Editor) {
    let doc = editor.document();
    let buffer = editor.buffer();
    let tag = undefined_tag(buffer);
    // Where it is: an underline changes the text's size, and GTK lays out again all the text a
    // removal spans.
    remove_tag_everywhere(buffer, &tag);
    let findings = findings_for(labels, doc.path());
    if findings.is_empty() || !doc.has_line_states() {
        return;
    }
    let ranges: Vec<std::ops::Range<usize>> = doc.with_text(|text| {
        let states = doc.line_states.borrow();
        let Some(states) = states.as_ref() else {
            return Vec::new();
        };
        let index = doc.line_index();
        let mut ranges = Vec::new();
        let mut elsewhere: BTreeSet<(NameKind, &str)> = BTreeSet::new();
        let mut tokens = Vec::new();
        for finding in &findings {
            let mut found = false;
            if finding.line < index.line_count() {
                let line_range = index.line_range(finding.line);
                let line_text = &text[line_range.clone()];
                tokens.clear();
                states.tokens(finding.line, line_text, &mut tokens);
                for token in tokens
                    .iter()
                    .filter(|t| finding.kind.matches(t.kind) && t.kind != TokenKind::LabelArg)
                {
                    for (r, item) in scan::token_items(line_text, token) {
                        if item == finding.name {
                            ranges.push(line_range.start + r.start..line_range.start + r.end);
                            found = true;
                        }
                    }
                }
            }
            if !found {
                elsewhere.insert((finding.kind, finding.name.as_str()));
            }
        }
        for (kind, name) in elsewhere {
            ranges.extend(
                scan::find_occurrences(
                    text,
                    &index,
                    |line, line_text, out| states.tokens(line, line_text, out),
                    name,
                    kind,
                )
                .into_iter()
                .filter(|o| !o.definition)
                .map(|o| o.range),
            );
        }
        ranges
    });
    for range in ranges {
        if let (Some(a), Some(b)) = (doc.iter_at_byte(range.start), doc.iter_at_byte(range.end)) {
            buffer.apply_tag(&tag, &a, &b);
        }
    }
}

fn undefined_tag(buffer: &sourceview5::Buffer) -> gtk::TextTag {
    let table = buffer.tag_table();
    table.lookup(UNDEFINED_TAG).unwrap_or_else(|| {
        let (r, g, b, a) = UNDERLINE_RGBA;
        let tag = gtk::TextTag::builder()
            .name(UNDEFINED_TAG)
            .underline(pango::Underline::Error)
            .underline_rgba(&gtk::gdk::RGBA::new(r, g, b, a))
            .build();
        table.add(&tag);
        tag
    })
}

/// The underlined ranges of `editor`'s buffer as document bytes (tests).
pub fn underlined(editor: &Editor) -> Vec<std::ops::Range<usize>> {
    let buffer = editor.buffer();
    let Some(tag) = buffer.tag_table().lookup(UNDEFINED_TAG) else {
        return Vec::new();
    };
    let doc = editor.document();
    let mut out = Vec::new();
    let mut iter = buffer.start_iter();
    loop {
        if iter.starts_tag(Some(&tag)) {
            let start = iter;
            let mut end = iter;
            end.forward_to_tag_toggle(Some(&tag));
            out.push(doc.byte_offset(&start)..doc.byte_offset(&end));
            iter = end;
        }
        if !iter.forward_to_tag_toggle(Some(&tag)) {
            break;
        }
    }
    out
}

/// Gives the Problems page the undefined names (`DiagSource::Labels`), of which those in the
/// files `changed` are new.
fn publish_problems(state: &Rc<WindowState>, labels: &Labels, changed: &BTreeSet<PathBuf>) {
    let undefined: Vec<Finding> = labels
        .undefined
        .borrow()
        .values()
        .flatten()
        .cloned()
        .collect();
    problems::publish(state, &undefined, changed);
}

/// The labels and bibliography entries the project never uses, by path and line: read from
/// the project index, plus a look at the project's files for `\nocite{*}` (which uses every
/// entry).
pub fn unused(state: &Rc<WindowState>) -> impl std::future::Future<Output = Vec<Finding>> + use<> {
    let state = state.clone();
    async move {
        let (labels, files, texts) = {
            let index = state.workspace.index.borrow();
            let labels = scan::unused_labels(&index);
            let mut texts = Vec::new();
            let mut files = Vec::new();
            for path in index.files.keys() {
                match state.workspace.get(path) {
                    Some(doc) => texts.push(doc.text()),
                    None => files.push(path.clone()),
                }
            }
            (labels, files, texts)
        };
        let cite_all = texts.iter().any(|t| scan::cites_everything(t))
            || gio::spawn_blocking(move || {
                files.iter().any(|path| {
                    std::fs::read_to_string(path).is_ok_and(|t| scan::cites_everything(&t))
                })
            })
            .await
            .unwrap_or(false);
        let entries = scan::unused_entries(&state.workspace.index.borrow(), cite_all);
        let mut all = labels;
        all.extend(entries);
        all
    }
}

// ----- the registries -----

thread_local! {
    /// WP3.2's hover provider and goto resolver of each view.
    static TOOLS: RefCell<Vec<RegisteredTools>> = const { RefCell::new(Vec::new()) };
}

/// A view and the ids of WP3.2's hover provider and goto resolver on it.
type RegisteredTools = (
    glib::WeakRef<crate::editor::TexView>,
    Option<crate::editor::hover::ProviderId>,
    Option<crate::editor::goto::ResolverId>,
);

/// Records the ids `editor::hover::attach_providers` and `editor::goto::attach_resolvers` got
/// for `view` (for [`remove_tools`]).
pub(crate) fn remember_tools(
    view: &crate::editor::TexView,
    provider: Option<crate::editor::hover::ProviderId>,
    resolver: Option<crate::editor::goto::ResolverId>,
) {
    TOOLS.with_borrow_mut(|tools| {
        tools.retain(|(v, ..)| v.upgrade().is_some());
        match tools
            .iter_mut()
            .find(|(v, ..)| v.upgrade().as_ref() == Some(view))
        {
            Some(entry) => {
                entry.1 = provider.or(entry.1);
                entry.2 = resolver.or(entry.2);
            }
            None => tools.push((view.downgrade(), provider, resolver)),
        }
    });
}

/// Takes WP3.2's hover provider and goto resolver off `editor`'s view: the registries then
/// hold only what others register (the tests of the registries themselves use it).
pub fn remove_tools(editor: &Editor) {
    let view = editor.view();
    let entry = TOOLS.with_borrow_mut(|tools| {
        let at = tools
            .iter()
            .position(|(v, ..)| v.upgrade().as_ref() == Some(view))?;
        Some(tools.remove(at))
    });
    if let Some((_, provider, resolver)) = entry {
        if let Some(id) = provider {
            crate::editor::hover::unregister(view, id);
        }
        if let Some(id) = resolver {
            crate::editor::goto::unregister(view, id);
        }
    }
}

// ----- popovers -----

/// How a popover of the label tools (the reference list, the rename box) closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Closing {
    /// Right away; the editor gets the keyboard back.
    Now,
    /// From an idle callback (when called inside the popover's own signal or key handling);
    /// the editor gets the keyboard back.
    Later,
    /// Its tab is closing: right away, and the editor does not take the keyboard (it is
    /// leaving the window; a focus given to it now would outlive it).
    TabGoes,
}

/// Closes `popover` (parented to the editor `view`): takes the keyboard out of it first (an
/// entry that is unrealized with the focus warns), then pops it down and unparents it, so no
/// popover stays on the view.
pub(crate) fn close_popover(popover: &gtk::Popover, view: Option<&gtk::Widget>, how: Closing) {
    if let Some(root) = popover.root()
        && root.focus().is_some_and(|f| f.is_ancestor(popover))
    {
        let view_took_it = how != Closing::TabGoes
            && view.is_some_and(|v| v.is_mapped() && v.root().is_some() && v.grab_focus());
        if !view_took_it {
            root.set_focus(None::<&gtk::Widget>);
        }
    }
    // Popping an autohide popover down while its view leaves the window makes GTK queue a
    // draw on the view later, when it is gone (a Gtk-CRITICAL when the window closes):
    // unparenting it is enough then.
    if how != Closing::TabGoes && popover.is_visible() {
        popover.popdown();
    }
    match how {
        Closing::Later => {
            let popover = popover.clone();
            glib::idle_add_local_once(move || {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
        }
        Closing::Now | Closing::TabGoes => {
            if popover.parent().is_some() {
                popover.unparent();
            }
        }
    }
}

// ----- the name under the cursor -----

/// The name (label or citation key) at `editor`'s cursor, else at the character before it:
/// its kind, its range and its text.
pub(crate) fn name_at_cursor(
    editor: &Editor,
) -> Option<(NameKind, std::ops::Range<usize>, String)> {
    let cursor = editor.cursor_offset();
    let at = |offset: usize| {
        let point = TextPoint::new(editor.document(), offset)?;
        name_at(&point)
    };
    at(cursor).or_else(|| at(crate::editor::hover::char_before(editor, cursor)?))
}

/// The label or citation key under `point` (in a `\label`, a reference or a citation): also on
/// the command name, where the first name of its argument counts.
pub(crate) fn name_at(point: &TextPoint) -> Option<(NameKind, std::ops::Range<usize>, String)> {
    if let Some((range, name)) = point.item() {
        let kind = match point.token_kind()? {
            TokenKind::RefArg | TokenKind::LabelArg => NameKind::Label,
            TokenKind::CiteArg => NameKind::Cite,
            _ => return None,
        };
        return Some((kind, range, name.to_owned()));
    }
    let first = resolve::first_argument_item(point)?;
    name_at(&first)
}

/// Relative path of `path` for display: from the root document's directory (else the
/// workspace root).
pub(crate) fn display_path(state: &WindowState, path: &Path) -> String {
    let root = state.workspace.root().path().to_path_buf();
    let base = state
        .build
        .get()
        .and_then(|b| b.root())
        .and_then(|r| r.parent().map(Path::to_path_buf))
        .filter(|dir| path.starts_with(dir))
        .unwrap_or(root);
    path.strip_prefix(&base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Documents of the workspace and the project's other files, for searches across the project:
/// every file the project index has, plus the open LaTeX documents.
pub(crate) fn project_paths(state: &WindowState) -> Vec<PathBuf> {
    let mut paths: BTreeSet<PathBuf> = state
        .workspace
        .index
        .borrow()
        .files
        .keys()
        .cloned()
        .collect();
    for doc in state.workspace.documents() {
        if doc.is_open() && completion::is_latex(doc.path()) {
            paths.insert(doc.path().to_path_buf());
        }
    }
    // The index keys files lexically, the workspace canonically: one entry per file.
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(crate::workspace::normalize(p)))
        .collect()
}

/// `\label{name}` in the live text of `doc`: the byte offset of the name. `near` is the line
/// where the project index saw it: when the label is still there, only that line is read (no
/// copy of a large document's text for every hover and Ctrl+hover).
pub(crate) fn label_in_document(doc: &DocRef, name: &str, near: Option<u32>) -> Option<usize> {
    if let Some(line) = near
        && let Some(found) = label_on_line(doc, name, line)
    {
        return Some(found);
    }
    doc.with_text(|text| {
        let states = doc.line_states.borrow();
        match states.as_ref() {
            Some(states) => scan::find_occurrences(
                text,
                &doc.line_index(),
                |line, line_text, out| states.tokens(line, line_text, out),
                name,
                NameKind::Label,
            )
            .into_iter()
            .find(|o| o.definition)
            .map(|o| o.range.start),
            // Without scanner states (a closed document): the plain text.
            None => text
                .find(&format!("\\label{{{name}}}"))
                .map(|at| at + "\\label{".len()),
        }
    })
}

/// `\label{name}` on the 0-based `line` of `doc` (a label argument of the scanner).
fn label_on_line(doc: &DocRef, name: &str, line: u32) -> Option<usize> {
    let text = doc.line_text(line)?;
    if !text.contains(name) {
        return None;
    }
    let states = doc.line_states.borrow();
    let states = states.as_ref()?;
    let mut tokens = Vec::new();
    states.tokens(line, &text, &mut tokens);
    let column = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::LabelArg)
        .flat_map(|t| scan::token_items(&text, t))
        .find(|(_, item)| *item == name)
        .map(|(range, _)| range.start)?;
    Some(doc.line_index().line_start(line) + column)
}
