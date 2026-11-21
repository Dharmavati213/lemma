//! Review controller (WP1.2, ARCHITECTURE §4.3–§4.4): accept/reject/navigate actions and their
//! chords, the review banner and header summary, toasts, auto-advance, and the per-document review
//! bookkeeping driven by the document's change pipeline.
//!
//! # Documents
//!
//! A [`FileReview`] lives in `Document::review`. [`start_review`] creates it (the agent's first
//! edit to a file); from then on the document's change pipeline calls [`before_edit`] (which feeds
//! `FileReview::on_edit`) and [`after_edit`] (which schedules [`recompute`]: at once for agent,
//! review, undo/redo and reload changes, 50 ms after the last keystroke for user typing). Every
//! recompute ends with `Document::notify_hunks_changed`, which the per-view renderer
//! ([`review_overlay::ViewReview`]) listens to, and with a refresh of every window's banner,
//! header summary and Changes panel.
//!
//! A review whose hunks are all decided is kept (with no hunks) while its document stays in the
//! workspace, so buffer undo of a reject and Alt+U still find it; it is dropped when the
//! document's editor closes.
//!
//! # Window
//!
//! [`install`] adds the `win.review-*` actions of §4.3 with their chords (the `[keys]` section of
//! the configuration overrides them), the banner ("Claude changed 3 files · 7 hunks" with a
//! Review button) and the summary button in the header bar. Decisions move the cursor to the next
//! hunk and centre it.
//!
//! # Agent turns (WP2.6)
//!
//! The agent bridge calls [`on_turn_started`] and [`on_turn_ended`]. Every agent edit is
//! attributed to the window's current turn (`FileReview::set_turn`), so each hunk knows the turns
//! that made it, and the Changes panel groups hunks by turn ([`review_summary`]): a turn's own
//! hunks can be kept or rejected together ([`decide_turn`], the actions
//! `win.review-keep-turn(u)` / `win.review-reject-turn(u)`, and `win.review-undo-turn` for the
//! latest turn, also `:KeepTurn [n]`, `:RejectTurn [n]` and `:UndoTurn`), as one decision that
//! Alt+U reverts. Hunks that several turns edited are never decided per turn.
//!
//! Reviews carry *review turn* numbers: the agent's turn number in this session plus a base, so
//! that turns restored from an earlier session ([`crate::review_persist`]) never share a number
//! with this session's. [`summary`], the banner, the header button and the Changes panel all count
//! from one [`ReviewSummary`].

use crate::changes_panel::ChangesPanel;
use crate::document::{DocRef, Document};
use crate::editor::Editor;
use crate::editor::review_overlay::{self, ViewReview};
use crate::review_persist;
use crate::saver::SaveError;
use crate::window::{PAGE_CHANGES, WindowState};
use crate::workspace::Workspace;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use lemma_review::{FileReview, HunkDelta, HunkId, ReviewEvent, UndoAction};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Quiet time after the last keystroke before hunks are recomputed (§4.1).
const USER_DEBOUNCE: Duration = Duration::from_millis(50);
/// Decisions remembered for Alt+U (the per-file journals hold the details).
const DECISIONS_KEPT: usize = 100;
/// Longest proposed/kept text quoted in review feedback for the agent.
const FEEDBACK_QUOTE_MAX: usize = 400;
/// Review events listed one by one per file in a feedback block; the rest are counted.
const FEEDBACK_EVENTS_PER_FILE: usize = 12;
/// Size of a feedback block beyond which events are only counted (the block is sent with the
/// next prompt: tokens the user pays for).
const FEEDBACK_MAX_BYTES: usize = 24 * 1024;

/// The review actions (all `win.*`) and their default chords (§4.3).
pub const ACCELS: &[(&str, &[&str])] = &[
    ("win.review-next", &["<Alt>bracketright"]),
    ("win.review-prev", &["<Alt>bracketleft"]),
    ("win.review-accept", &["<Alt>y"]),
    ("win.review-reject", &["<Alt>n"]),
    ("win.review-accept-file", &["<Alt><Shift>y"]),
    ("win.review-reject-file", &["<Alt><Shift>n"]),
    ("win.review-accept-all", &["<Control><Alt>y"]),
    ("win.review-reject-all", &["<Control><Alt>n"]),
    ("win.review-undo", &["<Alt>u"]),
    ("win.review-reading-mode", &["<Alt>d"]),
];

/// How `accel` (`"<Alt>y"`) is shown to the user: `"Alt+Y"` (GTK's label, which follows the
/// platform's conventions). `None` when it does not parse.
pub fn accel_label(accel: &str) -> Option<String> {
    let (key, mods) = gtk::accelerator_parse(accel)?;
    Some(gtk::accelerator_get_label(key, mods).to_string()).filter(|l| !l.is_empty())
}

/// The label of the default chord of `action` (`"win.review-accept"`), from [`ACCELS`].
pub fn default_shortcut_label(action: &str) -> Option<String> {
    let (_, accels) = ACCELS.iter().find(|(a, _)| *a == action)?;
    accel_label(accels.first()?)
}

/// The label of the first chord bound to `action` in `app` (after the `[keys]` overrides).
pub fn shortcut_label(app: &impl IsA<gtk::Application>, action: &str) -> Option<String> {
    let accels = app.as_ref().accels_for_action(action);
    accel_label(accels.first()?)
}

/// Vim ex commands and the actions they run (§4.3).
const EX_COMMANDS: &[(&str, &str)] = &[
    ("Hnext", "review-next"),
    ("Hprev", "review-prev"),
    ("Accept", "review-accept"),
    ("Reject", "review-reject"),
    ("AcceptAll", "review-accept-file"),
    ("RejectAll", "review-reject-file"),
    ("AgentAcceptAll", "review-accept-all"),
    ("AgentRejectAll", "review-reject-all"),
    ("Hundo", "review-undo"),
];

/// What to do with a hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Keep the agent's text (the baseline takes it; nothing is written).
    Accept,
    /// Restore the baseline text in the buffer and on disk.
    Reject,
}

// ----- per-document bookkeeping -----

/// Scheduling state of one reviewed document.
struct Tracked {
    doc: Weak<Document>,
    /// Debounce timer for user typing.
    timer: Option<glib::SourceId>,
    /// A recompute is queued at high priority (agent, review, undo/redo and reload changes).
    queued: bool,
    /// Document version at the last recompute.
    computed: Option<u64>,
    /// The agent created the file (`write` of a new path): rejecting all of it trashes the file.
    new_file: bool,
}

thread_local! {
    static TRACKED: RefCell<Vec<Tracked>> = const { RefCell::new(Vec::new()) };
    static CONTROLLERS: RefCell<Vec<Weak<Inner>>> = const { RefCell::new(Vec::new()) };
    /// The review events of reviews dropped before the agent heard of them (the tab of a file
    /// whose hunks were all decided closed, a file read again), oldest first: [`drain_feedback`]
    /// delivers them with the documents' own.
    static DROPPED_EVENTS: RefCell<Vec<(PathBuf, ReviewEvent)>> = const { RefCell::new(Vec::new()) };
}

/// Most events [`DROPPED_EVENTS`] keeps (the oldest go): with no agent to take them, they would
/// pile up for the whole session.
const DROPPED_EVENTS_MAX: usize = 500;

/// Keeps the `events` of `path`'s review, which is being dropped, for [`drain_feedback`]. The
/// texts of a reject are kept only as far as the feedback quotes them.
fn keep_dropped_events(path: &Path, events: Vec<ReviewEvent>) {
    if events.is_empty() {
        return;
    }
    let shorten = |text: &mut String| {
        if text.len() > 2 * FEEDBACK_QUOTE_MAX {
            let mut end = 2 * FEEDBACK_QUOTE_MAX;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
    };
    DROPPED_EVENTS.with(|dropped| {
        let mut dropped = dropped.borrow_mut();
        for mut event in events {
            if let ReviewEvent::Rejected { proposed, kept, .. } = &mut event {
                shorten(proposed);
                shorten(kept);
            }
            dropped.push((path.to_path_buf(), event));
        }
        let over = dropped.len().saturating_sub(DROPPED_EVENTS_MAX);
        dropped.drain(..over);
    });
}

/// Takes the events of dropped reviews of files under `root` (see [`keep_dropped_events`]).
fn take_dropped_events(root: &Path) -> Vec<(PathBuf, ReviewEvent)> {
    DROPPED_EVENTS.with(|dropped| {
        let mut dropped = dropped.borrow_mut();
        if dropped.is_empty() {
            return Vec::new();
        }
        let (taken, kept) = std::mem::take(&mut *dropped)
            .into_iter()
            .partition(|(path, _)| path.starts_with(root));
        *dropped = kept;
        taken
    })
}

/// Runs `f` on the bookkeeping entry of `doc`, if it is tracked.
fn with_tracked<R>(doc: &Document, f: impl FnOnce(&mut Tracked) -> R) -> Option<R> {
    TRACKED.with(|tracked| {
        let mut tracked = tracked.borrow_mut();
        tracked.retain(|t| t.doc.strong_count() > 0);
        tracked
            .iter_mut()
            .find(|t| std::ptr::eq(t.doc.as_ptr(), doc))
            .map(f)
    })
}

/// Starts reviewing `doc` if it has no review yet: its current text becomes the baseline.
/// `new_file` marks a file the agent created (rejecting all of it moves the file to the trash).
pub fn start_review(doc: &DocRef, new_file: bool) {
    let mut new_file = new_file;
    if doc.review.borrow().is_none() {
        // A review of this file saved in an earlier session that is still waiting to be restored
        // comes first: its hunks stay, and the new edit joins them.
        let (mut review, saved_new_file) =
            review_persist::take_saved(doc).unwrap_or_else(|| (FileReview::new(doc.text()), false));
        new_file |= saved_new_file;
        // Its agent edits belong to the window's current turn.
        review.set_turn(current_turn_of(doc));
        review.set_in_buffer(doc.is_open());
        doc.review.replace(Some(review));
    }
    let known = with_tracked(doc, |t| t.new_file |= new_file).is_some();
    if !known {
        TRACKED.with(|tracked| {
            tracked.borrow_mut().push(Tracked {
                doc: Rc::downgrade(doc),
                timer: None,
                queued: false,
                computed: None,
                new_file,
            });
        });
    }
}

/// Gives `doc` an existing review (e.g. one restored from disk) and recomputes its hunks. Reviews
/// must enter documents through this function or [`start_review`]: only then are the hunks
/// recomputed after changes.
pub fn adopt_review(doc: &DocRef, mut review: FileReview) {
    review.set_turn(current_turn_of(doc));
    review.set_in_buffer(doc.is_open());
    doc.review.replace(Some(review));
    start_review(doc, false);
    recompute(doc);
}

/// Stops reviewing `doc` (its review is dropped) and refreshes the review UI. The events the
/// agent has not heard of yet (a reject before the tab closed) are kept for [`drain_feedback`].
pub fn drop_review(doc: &DocRef) {
    let review = doc.review.replace(None);
    if let Some(mut review) = review {
        keep_dropped_events(doc.path(), review.drain_events());
    }
    TRACKED.with(|tracked| {
        let mut tracked = tracked.borrow_mut();
        if let Some(i) = tracked
            .iter()
            .position(|t| std::ptr::eq(t.doc.as_ptr(), Rc::as_ptr(doc)))
        {
            let entry = tracked.remove(i);
            if let Some(timer) = entry.timer {
                timer.remove();
            }
        }
    });
    doc.notify_hunks_changed();
    notify_controllers(doc);
}

/// True when the agent created `doc`'s file during the current review.
pub fn is_new_file(doc: &Document) -> bool {
    with_tracked(doc, |t| t.new_file).unwrap_or(false)
}

/// Re-diffs `doc`'s review against its current text right away and tells the views, the banner
/// and the Changes panel. Returns how the hunk list changed (empty without a review).
pub fn recompute(doc: &DocRef) -> HunkDelta {
    let delta = {
        let mut review = doc.review.borrow_mut();
        let Some(review) = review.as_mut() else {
            return HunkDelta::default();
        };
        doc.with_text(|text| review.recompute(text))
    };
    recomputed(doc);
    delta
}

/// [`recompute`] against `text`, which is `doc`'s current text: a decision that changes no text
/// (a keep, Alt+U of one) recomputes against the copy it decided on instead of copying the
/// buffer again (3.5–7 ms on `large.tex`).
fn recompute_against(doc: &DocRef, text: &str) -> HunkDelta {
    let delta = {
        let mut review = doc.review.borrow_mut();
        let Some(review) = review.as_mut() else {
            return HunkDelta::default();
        };
        review.recompute(text)
    };
    recomputed(doc);
    delta
}

/// After a recompute of `doc`'s review: the one scheduled is not needed any more, and the views,
/// the banner and the Changes panel hear of the hunks.
fn recomputed(doc: &DocRef) {
    let version = doc.version();
    with_tracked(doc, |t| {
        if let Some(timer) = t.timer.take() {
            timer.remove();
        }
        t.computed = Some(version);
    });
    doc.notify_hunks_changed();
    notify_controllers(doc);
}

/// Runs a [`recompute`] that [`after_edit`] scheduled but has not run yet (code that reads the
/// hunks right after an agent edit, before the idle, uses it).
pub fn recompute_if_stale(doc: &DocRef) {
    let stale = with_tracked(doc, |t| t.computed != Some(doc.version())).unwrap_or(false);
    if stale {
        recompute(doc);
    }
}

/// Called before every change of a document that has a review (`doc.review` is `Some`), with
/// pre-edit byte offsets: feeds `FileReview::on_edit`.
pub fn before_edit(doc: &Document, edit: &TextEdit, origin: Origin) {
    if let Some(review) = doc.review.borrow_mut().as_mut() {
        review.on_edit(edit, origin);
    }
}

/// [`before_edit`] for the changes of one `Document::apply_edits` at once, in the order they were
/// made (each with the pre-edit offsets of its moment): feeds `FileReview::on_edits`.
pub fn before_edits(doc: &Document, edits: &[TextEdit], origin: Origin) {
    if let Some(review) = doc.review.borrow_mut().as_mut() {
        review.on_edits(edits, origin);
    }
}

/// Called after every change of a document that has a review: schedules [`recompute`]. Agent,
/// review, history and reload changes are recomputed before the next frame; user typing 50 ms
/// after the last keystroke.
pub fn after_edit(doc: &Document, origin: Origin) {
    with_tracked(doc, |t| {
        if origin == Origin::User {
            if let Some(timer) = t.timer.take() {
                timer.remove();
            }
            let weak = t.doc.clone();
            t.timer = Some(glib::timeout_add_local_once(USER_DEBOUNCE, move || {
                if let Some(doc) = weak.upgrade() {
                    with_tracked(&doc, |t| t.timer = None);
                    recompute_if_stale(&doc);
                }
            }));
        } else if !t.queued {
            t.queued = true;
            let weak = t.doc.clone();
            // HIGH_IDLE runs before GDK's redraw (HIGH_IDLE + 20): no frame shows stale hunks.
            glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
                if let Some(doc) = weak.upgrade() {
                    with_tracked(&doc, |t| t.queued = false);
                    recompute_if_stale(&doc);
                }
                glib::ControlFlow::Break
            });
        }
    });
}

/// Documents of `ws` that have hunks to review, sorted by path.
pub fn reviewed_documents(ws: &Workspace) -> Vec<DocRef> {
    let mut docs: Vec<DocRef> = ws
        .documents()
        .into_iter()
        .filter(|d| hunk_count(d) > 0)
        .collect();
    docs.sort_by(|a, b| a.path().cmp(b.path()));
    docs
}

/// Number of hunks in `doc`'s review (0 without a review).
pub fn hunk_count(doc: &Document) -> usize {
    doc.review.borrow().as_ref().map_or(0, |r| r.hunks().len())
}

/// Files and hunks under review in `ws` (the counts of [`ReviewSummary`]).
pub fn summary(ws: &Workspace) -> (usize, usize) {
    let docs = reviewed_documents(ws);
    let hunks = docs.iter().map(|d| hunk_count(d)).sum();
    (docs.len(), hunks)
}

/// Takes the queued review events of every document in `ws`, and those of reviews of its files
/// dropped since ([`drop_review`]), and formats them for the agent as a `<review_feedback>`
/// block (lines 1-based; files named with `/` on every system, as in the tools' replies).
/// `None` when nothing happened.
///
/// The block goes with the next prompt, so it is kept small: per file, the first
/// [`FEEDBACK_EVENTS_PER_FILE`] events are listed (texts quoted up to [`FEEDBACK_QUOTE_MAX`]
/// bytes) and the others are counted in one line; past [`FEEDBACK_MAX_BYTES`], files are only
/// counted.
pub fn drain_feedback(ws: &Workspace) -> Option<String> {
    let root = ws.root();
    // Per file (sorted by path), oldest first: what dropped reviews left, then the reviews'.
    let mut files: BTreeMap<PathBuf, Vec<ReviewEvent>> = BTreeMap::new();
    for (path, event) in take_dropped_events(root.path()) {
        files.entry(path).or_default().push(event);
    }
    for doc in ws.documents() {
        let events = match doc.review.borrow_mut().as_mut() {
            Some(review) => review.drain_events(),
            None => continue,
        };
        if !events.is_empty() {
            files
                .entry(doc.path().to_path_buf())
                .or_default()
                .extend(events);
        }
    }
    let mut out = String::new();
    let (mut unlisted, mut unlisted_files) = (0, 0);
    for (path, events) in files {
        if out.len() >= FEEDBACK_MAX_BYTES {
            unlisted += events.len();
            unlisted_files += 1;
            continue;
        }
        let name = root.agent_name(&path);
        let mut rest = Vec::new();
        for (i, event) in events.into_iter().enumerate() {
            let line = feedback_line(&name, &event);
            if i < FEEDBACK_EVENTS_PER_FILE && out.len() + line.len() <= FEEDBACK_MAX_BYTES {
                out.push_str(&line);
            } else {
                rest.push(event);
            }
        }
        if !rest.is_empty() {
            let _ = writeln!(out, "{name}: {}", feedback_summary(&rest));
        }
    }
    if unlisted > 0 {
        let _ = writeln!(
            out,
            "…and {} in {} more (not listed)",
            plural(unlisted, "review event", "review events"),
            plural(unlisted_files, "file", "files")
        );
    }
    (!out.is_empty()).then(|| format!("<review_feedback>\n{out}</review_feedback>"))
}

/// One event of [`drain_feedback`], with its newline.
fn feedback_line(name: &str, event: &ReviewEvent) -> String {
    match event {
        ReviewEvent::Accepted { lines } => {
            format!("{name}: the user accepted {}\n", line_span(lines))
        }
        ReviewEvent::Rejected {
            lines,
            proposed,
            kept,
        } => format!(
            "{name}: the user rejected {}; your text {} was replaced by {}\n",
            line_span(lines),
            quote(proposed),
            quote(kept)
        ),
        ReviewEvent::UserEditedInHunk { lines } => {
            format!(
                "{name}: the user edited your change at {}\n",
                line_span(lines)
            )
        }
        ReviewEvent::UndoneByUser { lines } => {
            format!(
                "{name}: the user undid your change at {}\n",
                line_span(lines)
            )
        }
    }
}

/// The events of a file beyond those listed, counted: "and 580 more: the user rejected 578 of
/// your changes (lines 41–1200), accepted 2 of your changes (lines 3–9)".
fn feedback_summary(events: &[ReviewEvent]) -> String {
    let lines_of = |e: &ReviewEvent| match e {
        ReviewEvent::Accepted { lines }
        | ReviewEvent::Rejected { lines, .. }
        | ReviewEvent::UserEditedInHunk { lines }
        | ReviewEvent::UndoneByUser { lines } => lines.clone(),
    };
    let what = |verb: &str, pick: fn(&ReviewEvent) -> bool| {
        let picked: Vec<Range<u32>> = events.iter().filter(|e| pick(e)).map(lines_of).collect();
        let first = picked.iter().map(|l| l.start).min()?;
        let last = picked.iter().map(|l| l.end.max(l.start + 1)).max()?;
        let span = line_span(&(first..last));
        Some(format!("{verb} {} of your changes ({span})", picked.len()))
    };
    let parts: Vec<String> = [
        what("rejected", |e| matches!(e, ReviewEvent::Rejected { .. })),
        what("accepted", |e| matches!(e, ReviewEvent::Accepted { .. })),
        what("edited", |e| {
            matches!(e, ReviewEvent::UserEditedInHunk { .. })
        }),
        what("undid", |e| matches!(e, ReviewEvent::UndoneByUser { .. })),
    ]
    .into_iter()
    .flatten()
    .collect();
    format!("and {} more: the user {}", events.len(), parts.join(", "))
}

/// `lines 3–5` / `line 3` (1-based, inclusive) for a 0-based half-open range; an empty range
/// names the line it sits on.
fn line_span(lines: &Range<u32>) -> String {
    match lines.len() {
        0 | 1 => format!("line {}", lines.start + 1),
        _ => format!("lines {}–{}", lines.start + 1, lines.end),
    }
}

/// `text` as a quoted string with escapes (`"a\nb"`), shortened to [`FEEDBACK_QUOTE_MAX`]
/// bytes.
fn quote(text: &str) -> String {
    if text.len() <= FEEDBACK_QUOTE_MAX {
        return format!("{text:?}");
    }
    let mut end = FEEDBACK_QUOTE_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{:?}…", &text[..end])
}

// ----- the controller -----

/// Owns the review UI state of one window (stored in [`WindowState::review`]).
pub struct ReviewController {
    inner: Rc<Inner>,
}

struct Inner {
    state: Weak<WindowState>,
    reading: Cell<bool>,
    /// Recent decisions, oldest first, for Alt+U: each one's number and the documents it changed
    /// (several for Accept/Reject in all files, which Alt+U reverts as one).
    decisions: RefCell<Vec<(u64, Vec<PathBuf>)>>,
    /// The number of the decision being made: the next one [`Inner::record_decision`] records.
    next_decision: Cell<u64>,
    /// Files the agent created that a reject sent to the trash, oldest first, for Alt+U of that
    /// decision.
    trashed: RefCell<Vec<Trashed>>,
    refresh_queued: Cell<bool>,
    /// The banner was answered (Review clicked) and stays hidden until more hunks arrive.
    banner_dismissed: Cell<bool>,
    last_total: Cell<usize>,
    summary_button: gtk::Button,
    summary_label: gtk::Label,
    panel: RefCell<Option<Rc<ChangesPanel>>>,
    /// Added to the agent's turn numbers: the highest review turn restored from an earlier
    /// session (see the module docs).
    turn_base: Cell<u32>,
    /// The review turn agent edits are attributed to (0 before the first turn).
    turn: Cell<u32>,
    /// The review turn running now, between `on_turn_started` and `on_turn_ended`.
    running: Cell<Option<u32>>,
    /// What is known about each review turn (label, start time).
    turns: RefCell<BTreeMap<u32, TurnInfo>>,
    /// "Restored N pending changes from your last session", shown in the banner while the
    /// changes under review are still the restored ones (the hunks it was announced with, by
    /// [`hunks_fingerprint`]).
    notice: RefCell<Option<(String, u64)>>,
}

impl ReviewController {
    /// True while reading mode (Alt+D) hides the deleted lines.
    pub fn reading_mode(&self) -> bool {
        self.inner.reading.get()
    }

    /// Registers the Changes panel, which is refreshed with the banner.
    pub fn set_changes_panel(&self, panel: Rc<ChangesPanel>) {
        self.inner.panel.replace(Some(panel));
        self.inner.queue_refresh();
    }

    /// The Changes panel, once installed.
    pub fn changes_panel(&self) -> Option<Rc<ChangesPanel>> {
        self.inner.panel.borrow().clone()
    }

    /// The header-bar summary button ("7 changes"), hidden while nothing is under review.
    pub fn summary_button(&self) -> &gtk::Button {
        &self.inner.summary_button
    }

    /// Updates the banner, the header summary and the Changes panel now (normally this happens
    /// in an idle callback after every recompute).
    pub fn refresh(&self) {
        self.inner.refresh();
    }
}

impl Inner {
    /// The review turn of this session's agent turn `turn`: the one it was recorded as, else
    /// what it gets now (named turns move the agent's later turns, see [`begin_named_turn`]).
    fn agent_review_turn(&self, turn: u32) -> u32 {
        self.turns
            .borrow()
            .iter()
            .rev()
            .find(|(_, info)| info.this_session && info.name.is_none() && info.label == turn)
            .map_or_else(|| self.turn_base.get().saturating_add(turn), |(t, _)| *t)
    }

    fn owns(&self, doc: &Document) -> bool {
        self.state.upgrade().is_some_and(|s| {
            s.workspace
                .documents()
                .iter()
                .any(|d| std::ptr::eq(Rc::as_ptr(d), doc))
        })
    }

    fn queue_refresh(self: &Rc<Self>) {
        if self.refresh_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(inner) = weak.upgrade() {
                inner.refresh_queued.set(false);
                inner.refresh();
            }
        });
    }

    fn refresh(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let summary = review_summary(&state);
        let hunks = summary.hunks;
        if hunks > self.last_total.get() {
            self.banner_dismissed.set(false);
        }
        self.last_total.set(hunks);
        // The restore notice stays until the changes are no longer the restored ones.
        let notice = {
            let mut notice = self.notice.borrow_mut();
            if notice
                .as_ref()
                .is_some_and(|(_, ids)| *ids != hunks_fingerprint(&summary.docs))
            {
                *notice = None;
            }
            notice.as_ref().map(|(text, _)| text.clone())
        };
        if hunks == 0 {
            state.banner.set_revealed(false);
            self.summary_button.set_visible(false);
        } else {
            let counts = format!(
                "{} · {}",
                plural(summary.files, "file", "files"),
                plural(hunks, "hunk", "hunks")
            );
            let text = match self.source(&summary) {
                Source::Named(name) => format!("{name} · {counts}"),
                Source::Several => format!("Changes to review · {counts}"),
                Source::Agent => format!(
                    "{} changed {counts}",
                    crate::agent::bridge::agent_label(
                        state.config.agent.backend,
                        &state.config.agent
                    )
                ),
            };
            state.banner.set_title(notice.as_deref().unwrap_or(&text));
            state.banner.set_revealed(!self.banner_dismissed.get());
            self.summary_label
                .set_text(&plural(hunks, "change", "changes"));
            self.summary_button
                .set_tooltip_text(Some(&format!("{text}. Show the Changes panel")));
            self.summary_button.set_visible(true);
        }
        if let Some(panel) = self.panel.borrow().clone() {
            panel.refresh(&state, &summary);
        }
        review_persist::changed(&state);
    }

    /// Who made everything under review: the agent (its turns, or its edits outside any turn),
    /// one named change ([`begin_named_turn`], [`begin_named_change`]), or several sources (also
    /// changes of no one's turn: another program's change to a file under review). The banner
    /// names the agent only for its own changes.
    fn source(&self, summary: &ReviewSummary) -> Source {
        let turns = self.turns.borrow();
        let mut found: Option<Source> = None;
        for doc in &summary.docs {
            let review = doc.review.borrow();
            let Some(review) = review.as_ref() else {
                continue;
            };
            for hunk in review.hunks() {
                if hunk.turns.is_empty() {
                    return Source::Several;
                }
                for turn in &hunk.turns {
                    let source = match turns.get(turn).and_then(|info| info.name.as_deref()) {
                        Some(name) => Source::Named(name.to_owned()),
                        None => Source::Agent,
                    };
                    match &found {
                        None => found = Some(source),
                        Some(f) if *f == source => {}
                        Some(_) => return Source::Several,
                    }
                }
            }
        }
        found.unwrap_or(Source::Agent)
    }

    fn record_decision(&self, docs: &[&Document]) {
        if docs.is_empty() {
            return;
        }
        let mut decisions = self.decisions.borrow_mut();
        if decisions.len() == DECISIONS_KEPT {
            decisions.remove(0);
        }
        let number = self.next_decision.replace(self.next_decision.get() + 1);
        decisions.push((
            number,
            docs.iter().map(|d| d.path().to_path_buf()).collect(),
        ));
    }

    fn keep_trashed(&self, trashed: Trashed) {
        let mut list = self.trashed.borrow_mut();
        if list.len() == DECISIONS_KEPT {
            list.remove(0);
        }
        list.push(trashed);
    }

    /// Alt+U could not bring `trashed`'s file back for a reason that may pass (a file of that
    /// name exists, the move to the trash is still running, the file could not be made): its
    /// decision is the last one again, for another Alt+U, unless a later decision waits for
    /// Alt+U by now (one made while the restore ran).
    fn retry_trashed(&self, trashed: Trashed) {
        {
            let mut decisions = self.decisions.borrow_mut();
            match decisions.last_mut() {
                Some((number, _)) if *number > trashed.decision => return,
                // Another file of the same decision is back already.
                Some((number, paths)) if *number == trashed.decision => {
                    paths.push(trashed.path.clone());
                }
                _ => decisions.push((trashed.decision, vec![trashed.path.clone()])),
            }
        }
        self.keep_trashed(trashed);
    }
}

/// Who made the changes under review (see [`Inner::source`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Agent,
    Named(String),
    Several,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A hash of the hunk ids of `docs` (with their paths): equal while the same hunks are under
/// review.
fn hunks_fingerprint(docs: &[DocRef]) -> u64 {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for doc in docs {
        parts.push(doc.path().as_os_str().as_encoded_bytes().to_vec());
        if let Some(review) = doc.review.borrow().as_ref() {
            parts.push(
                review
                    .hunks()
                    .iter()
                    .flat_map(|h| h.id.0.to_le_bytes())
                    .collect(),
            );
        }
    }
    let slices: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    lemma_core::fnv1a64(&slices)
}

fn notify_controllers(doc: &Document) {
    let controllers: Vec<Rc<Inner>> = CONTROLLERS.with(|c| {
        let mut c = c.borrow_mut();
        c.retain(|w| w.strong_count() > 0);
        c.iter().filter_map(Weak::upgrade).collect()
    });
    for inner in controllers {
        if inner.owns(doc) {
            inner.queue_refresh();
        }
    }
}

fn controller(state: &WindowState) -> Option<&Rc<Inner>> {
    state.review.get().map(|c| &c.inner)
}

/// Called once per window after the layout is built: adds the review actions and chords, the
/// header summary button and the banner's Review button.
pub fn install(state: &Rc<WindowState>) -> ReviewController {
    if let Some(display) = gtk::gdk::Display::default() {
        review_overlay::install_css(&display);
    }
    let summary_label = gtk::Label::new(None);
    let summary_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    summary_content.append(&gtk::Image::from_icon_name("document-edit-symbolic"));
    summary_content.append(&summary_label);
    let summary_button = gtk::Button::builder()
        .child(&summary_content)
        .css_classes(["flat", "lemma-review-summary"])
        .visible(false)
        .build();
    let weak = Rc::downgrade(state);
    summary_button.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            show_changes_panel(&state);
        }
    });
    state.header.review_summary.set_child(Some(&summary_button));

    state.banner.set_button_label(Some("_Review"));
    state.banner.set_use_markup(false);
    let weak = Rc::downgrade(state);
    state.banner.connect_button_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            if let Some(inner) = controller(&state) {
                inner.banner_dismissed.set(true);
                inner.notice.replace(None);
            }
            state.banner.set_revealed(false);
            show_changes_panel(&state);
            goto_first(&state);
        }
    });

    install_actions(state);
    install_accels(state);

    let inner = Rc::new(Inner {
        state: Rc::downgrade(state),
        reading: Cell::new(false),
        decisions: RefCell::new(Vec::new()),
        next_decision: Cell::new(0),
        trashed: RefCell::new(Vec::new()),
        refresh_queued: Cell::new(false),
        banner_dismissed: Cell::new(false),
        last_total: Cell::new(0),
        summary_button,
        summary_label,
        panel: RefCell::new(None),
        turn_base: Cell::new(0),
        turn: Cell::new(0),
        running: Cell::new(None),
        turns: RefCell::new(BTreeMap::new()),
        notice: RefCell::new(None),
    });
    CONTROLLERS.with(|c| c.borrow_mut().push(Rc::downgrade(&inner)));
    install_turn_actions(state);
    ReviewController { inner }
}

fn install_actions(state: &Rc<WindowState>) {
    let window = &state.window;
    let simple = |name: &str, run: fn(&Rc<WindowState>)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                run(&state);
            }
        });
        window.add_action(&action);
    };
    simple("review-next", |s| goto_next(s, true));
    simple("review-prev", |s| goto_next(s, false));
    simple("review-accept", |s| decide_at_cursor(s, Decision::Accept));
    simple("review-reject", |s| decide_at_cursor(s, Decision::Reject));
    simple("review-accept-file", |s| {
        decide_active_file(s, Decision::Accept)
    });
    simple("review-reject-file", |s| {
        decide_active_file(s, Decision::Reject)
    });
    simple("review-accept-all", |s| {
        decide_everything(s, Decision::Accept)
    });
    simple("review-reject-all", |s| {
        decide_everything(s, Decision::Reject)
    });
    simple("review-undo", undo_last);
    simple("review-show-changes", show_changes_panel);

    let reading = gio::SimpleAction::new_stateful("review-reading-mode", None, &false.into());
    let weak = Rc::downgrade(state);
    reading.connect_activate(move |action, _| {
        if let Some(state) = weak.upgrade() {
            let on = !action
                .state()
                .and_then(|s| s.get::<bool>())
                .unwrap_or(false);
            set_reading_mode(&state, on);
        }
    });
    window.add_action(&reading);
}

/// Sets the default chords of [`ACCELS`], keeping any `[keys]` override from the configuration
/// (an empty string there removes the binding).
fn install_accels(state: &WindowState) {
    let keys = &state.config.keys.bindings;
    for (action, accels) in ACCELS {
        match keys.get(*action) {
            Some(accel) if accel.is_empty() => state.app.set_accels_for_action(action, &[]),
            Some(accel) if gtk::accelerator_parse(accel).is_some() => {
                state.app.set_accels_for_action(action, &[accel.as_str()]);
            }
            _ => state.app.set_accels_for_action(action, accels),
        }
    }
}

/// Called after an editor tab is created: attaches the review renderer to its view.
pub fn attach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let weak = Rc::downgrade(state);
    let doc = Rc::downgrade(editor.document());
    let review = ViewReview::attach(
        editor.view(),
        editor.document(),
        editor.buffer(),
        Box::new(move |id, decision| {
            if let (Some(state), Some(doc)) = (weak.upgrade(), doc.upgrade()) {
                decide_and_advance(&state, &doc, id, decision);
            }
        }),
    );
    review.set_reading(controller(state).is_some_and(|c| c.reading.get()));
    review.set_shortcuts(
        shortcut_label(&state.app, "win.review-accept").as_deref(),
        shortcut_label(&state.app, "win.review-reject").as_deref(),
    );
    // The first document of the project: its saved reviews come back.
    review_persist::editor_attached(state);
}

/// Called after an editor tab is closed, before its document is released: detaches the
/// renderer, and drops a review with nothing left to decide (so the document can go).
///
/// A document still under review stays in the workspace as a closed document with the buffer's
/// text, and later rejects and agent edits save it. If that text holds changes that were never
/// saved, the tab was closed discarding them (the user answered Discard, `:q!`, or a scripted
/// close): the closed document goes back to the file's text, so the discarded typing never
/// reaches the disk.
pub fn detach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if let Some(review) = editor.view().review_state() {
        review.detach();
    }
    let doc = editor.document();
    let clean = doc
        .review
        .borrow()
        .as_ref()
        .is_some_and(FileReview::is_clean);
    if clean {
        drop_review(doc);
    } else if doc.review.borrow().is_some() && !text_is_saved(doc) {
        // The workspace keeps the document as a closed one right after this returns.
        let (state, doc) = (Rc::downgrade(state), doc.clone());
        glib::spawn_future_local(async move {
            if doc.is_open() || doc.review.borrow().is_none() {
                return;
            }
            if let Err(e) = doc.reload().await {
                drop_review(&doc);
                if let Some(state) = state.upgrade() {
                    state.workspace.release(&doc);
                    state.toast(&format!(
                        "{e}. The review of “{}” was dropped.",
                        doc.display_name()
                    ));
                }
            }
        });
    }
}

/// True when `doc`'s text is what this document last wrote to its file (or is writing now).
fn text_is_saved(doc: &Document) -> bool {
    let charset = doc.charset();
    let hash = doc.with_text(|text| match charset.as_deref() {
        None => Some(lemma_core::fnv1a64(&[text.as_bytes()])),
        Some(charset) => glib::convert(text.as_bytes(), charset, "UTF-8")
            .ok()
            .map(|(bytes, _)| lemma_core::fnv1a64(&[&bytes[..]])),
    });
    hash.is_some_and(|h| doc.is_own_write(h))
}

/// Vim ex commands (`:Accept`, `:Hnext`, …, see §4.3). Returns `true` when the command was
/// handled. The action runs from an idle callback, after the Vim context has finished
/// dispatching the key (it may open or close editors).
pub fn ex_command(state: &Rc<WindowState>, _editor: &Rc<Editor>, command: &str) -> bool {
    if turn_ex_command(state, command) {
        return true;
    }
    let name = command.trim().trim_start_matches(':').trim();
    let Some((_, action)) = EX_COMMANDS.iter().find(|(cmd, _)| *cmd == name) else {
        return false;
    };
    let weak = Rc::downgrade(state);
    glib::idle_add_local_once(move || {
        if let Some(state) = weak.upgrade() {
            ActionGroupExt::activate_action(&state.window, action, None);
        }
    });
    true
}

// ----- agent turns (M2) -----

/// What the review knows about one review turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnInfo {
    /// The number shown for it: the agent's turn number in the session that ran it.
    pub label: u32,
    /// When it started (seconds since the Unix epoch).
    pub started_unix: u64,
    /// It ran in this window; otherwise it was restored from an earlier session.
    pub this_session: bool,
    /// The name of a change Lemma made for the user and reviews like a turn
    /// ([`begin_named_turn`]: "Replace “a” → “b”"; its `label` is 0); `None` for the agent's
    /// turns.
    pub name: Option<String>,
    /// A named change made apart from the turns ([`begin_named_change`]: a restore from
    /// History, an added citation), which Undo Last Turn leaves alone.
    pub outside: bool,
}

/// How hunks are grouped in the Changes panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKind {
    /// The hunks of this review turn alone (`Hunk::turns == [turn]`, turn > 0).
    Turn(u32),
    /// Hunks that several turns edited: never decided per turn.
    Mixed,
    /// Hunks made outside any agent turn (or whose turn is not known).
    Other,
}

impl GroupKind {
    /// The group of a hunk with these `turns`.
    pub fn of(turns: &[u32]) -> Self {
        match turns {
            [t] if *t > 0 => Self::Turn(*t),
            [_, _, ..] => Self::Mixed,
            _ => Self::Other,
        }
    }
}

/// The hunks of one group in one file.
#[derive(Clone, Debug)]
pub struct GroupFile {
    pub doc: DocRef,
    /// The hunks, in text order.
    pub hunks: Vec<HunkId>,
    /// Current lines they add and baseline lines they remove.
    pub added: u32,
    pub removed: u32,
}

/// One group of the Changes panel.
#[derive(Clone, Debug)]
pub struct TurnGroup {
    pub kind: GroupKind,
    /// "Turn 3", "Turn 2 · 10 Oct 14:03" (an earlier session), "Several turns", "Other changes".
    pub label: String,
    /// The files with hunks of this group, by path.
    pub files: Vec<GroupFile>,
    /// Number of hunks in the group.
    pub hunks: usize,
    /// The turn is still running: it cannot be kept or rejected yet.
    pub running: bool,
}

impl TurnGroup {
    /// "Turn 3 · 4 changes in 2 files".
    pub fn title(&self) -> String {
        format!(
            "{} · {} in {}",
            self.label,
            plural(self.hunks, "change", "changes"),
            plural(self.files.len(), "file", "files")
        )
    }
}

/// Everything under review in a window, counted once for the banner, the header button and the
/// Changes panel.
#[derive(Clone, Debug, Default)]
pub struct ReviewSummary {
    /// Documents with hunks, by path.
    pub docs: Vec<DocRef>,
    /// `docs.len()`.
    pub files: usize,
    /// Hunks in all of them.
    pub hunks: usize,
    /// Turn groups, newest turn first, then mixed hunks, then the others.
    pub groups: Vec<TurnGroup>,
    /// The latest review turn with hunks ([`undo_last_turn`] rejects it).
    pub latest_turn: Option<u32>,
}

impl ReviewSummary {
    /// True when some hunk belongs to an agent turn (the panel then groups by turn).
    pub fn has_turns(&self) -> bool {
        self.groups.iter().any(|g| g.kind != GroupKind::Other)
    }
}

/// Counts and groups what is under review in `state`'s window (see [`ReviewSummary`]).
pub fn review_summary(state: &WindowState) -> ReviewSummary {
    let docs = reviewed_documents(&state.workspace);
    let mut groups: BTreeMap<GroupKind, Vec<GroupFile>> = BTreeMap::new();
    let mut hunks = 0;
    let mut latest: Option<u32> = None;
    // Undo Last Turn leaves the changes made apart from the turns alone.
    let outside: HashSet<u32> = controller(state).map_or_else(HashSet::new, |c| {
        c.turns
            .borrow()
            .iter()
            .filter(|(_, info)| info.outside)
            .map(|(t, _)| *t)
            .collect()
    });
    for doc in &docs {
        let review = doc.review.borrow();
        let Some(review) = review.as_ref() else {
            continue;
        };
        let mut by_group: BTreeMap<GroupKind, GroupFile> = BTreeMap::new();
        for h in review.hunks() {
            hunks += 1;
            latest = latest.max(
                h.turns
                    .iter()
                    .copied()
                    .filter(|t| *t > 0 && !outside.contains(t))
                    .max(),
            );
            let file = by_group
                .entry(GroupKind::of(&h.turns))
                .or_insert_with(|| GroupFile {
                    doc: doc.clone(),
                    hunks: Vec::new(),
                    added: 0,
                    removed: 0,
                });
            file.hunks.push(h.id);
            file.added += h.cur_lines.len() as u32;
            file.removed += h.base_lines.len() as u32;
        }
        for (kind, file) in by_group {
            groups.entry(kind).or_default().push(file);
        }
    }
    let running = controller(state).and_then(|c| running_turn(state, c));
    let mut out: Vec<TurnGroup> = groups
        .into_iter()
        .map(|(kind, files)| TurnGroup {
            label: group_label(state, kind),
            hunks: files.iter().map(|f| f.hunks.len()).sum(),
            running: matches!(kind, GroupKind::Turn(t) if Some(t) == running),
            kind,
            files,
        })
        .collect();
    // Newest turn first; `GroupKind` orders Turn < Mixed < Other.
    out.sort_by_key(|g| match g.kind {
        GroupKind::Turn(t) => (0, std::cmp::Reverse(t)),
        GroupKind::Mixed => (1, std::cmp::Reverse(0)),
        GroupKind::Other => (2, std::cmp::Reverse(0)),
    });
    ReviewSummary {
        files: docs.len(),
        docs,
        hunks,
        groups: out,
        latest_turn: latest,
    }
}

/// The label of a group (see [`TurnGroup::label`]).
fn group_label(state: &WindowState, kind: GroupKind) -> String {
    match kind {
        GroupKind::Turn(t) => turn_label(state, t),
        GroupKind::Mixed => "Several turns".to_owned(),
        GroupKind::Other => "Other changes".to_owned(),
    }
}

/// How review turn `turn` is named: "Turn 3" for this session's turns (the agent's numbering),
/// "Turn 2 · 10 Oct 14:03" for one restored from an earlier session, and a named turn by its
/// name ([`begin_named_turn`]).
pub fn turn_label(state: &WindowState, turn: u32) -> String {
    let info = controller(state).and_then(|c| c.turns.borrow().get(&turn).cloned());
    match info {
        Some(TurnInfo {
            name: Some(name), ..
        }) => name,
        Some(info) if info.this_session => format!("Turn {}", info.label),
        Some(info) => {
            let when = i64::try_from(info.started_unix)
                .ok()
                .and_then(|unix| glib::DateTime::from_unix_local(unix).ok())
                .and_then(|d| d.format("%-d %b %H:%M").ok());
            match when {
                Some(when) => format!("Turn {} · {when}", info.label),
                None => format!("Turn {} · earlier", info.label),
            }
        }
        None => format!("Turn {turn}"),
    }
}

/// The review turn that is running in `state`'s window, if any (the agent must still be running
/// it: a turn whose end was missed does not stay "running").
fn running_turn(state: &WindowState, inner: &Inner) -> Option<u32> {
    let running = inner.running.get()?;
    state
        .agent_controller()
        .is_some_and(|c| c.is_running())
        .then_some(running)
}

/// True while review turn `turn` runs in `state`'s window: its changes cannot be kept or rejected
/// as a turn yet.
pub fn is_turn_running(state: &WindowState, turn: u32) -> bool {
    controller(state).and_then(|c| running_turn(state, c)) == Some(turn)
}

/// The hunks of `doc` in group `kind` (see [`GroupKind::of`]), in text order.
pub fn group_hunks(doc: &Document, kind: GroupKind) -> Vec<HunkId> {
    doc.review.borrow().as_ref().map_or_else(Vec::new, |r| {
        r.hunks()
            .iter()
            .filter(|h| GroupKind::of(&h.turns) == kind)
            .map(|h| h.id)
            .collect()
    })
}

/// The current review turn of the window whose workspace has `doc` (0 when none does).
fn current_turn_of(doc: &Document) -> u32 {
    let controllers: Vec<Rc<Inner>> =
        CONTROLLERS.with(|c| c.borrow().iter().filter_map(Weak::upgrade).collect());
    controllers
        .iter()
        .find(|inner| inner.owns(doc))
        .map_or(0, |inner| inner.turn.get())
}

/// Called by the agent bridge when turn `turn` starts (after the turn snapshot, before the
/// prompt is sent): every review in the window starts a new turn (`FileReview::begin_turn`) and
/// attributes agent edits to it (`FileReview::set_turn`), as do reviews created during the turn.
/// `turn` is the agent controller's number; the reviews get the review turn (see the module
/// docs).
pub fn on_turn_started(state: &Rc<WindowState>, turn: u32) {
    let Some(inner) = controller(state) else {
        return;
    };
    let review_turn = inner.turn_base.get().saturating_add(turn);
    inner.turn.set(review_turn);
    inner.running.set(Some(review_turn));
    // The changes under review are no longer just the restored ones.
    inner.notice.replace(None);
    inner.turns.borrow_mut().insert(
        review_turn,
        TurnInfo {
            label: turn,
            started_unix: unix_now(),
            this_session: true,
            name: None,
            outside: false,
        },
    );
    for doc in state.workspace.documents() {
        if let Some(review) = doc.review.borrow_mut().as_mut() {
            review.begin_turn();
            review.set_turn(review_turn);
        }
    }
    inner.queue_refresh();
}

/// Called by the agent bridge after turn `turn` ended (its `TurnEnded`), before the turn-end
/// build: the turn's group in the Changes panel can now be kept or rejected. Agent edits that
/// still arrive (an external write the watcher sees shortly after) stay attributed to it.
pub fn on_turn_ended(state: &Rc<WindowState>, turn: u32) {
    let Some(inner) = controller(state) else {
        return;
    };
    let review_turn = inner.agent_review_turn(turn);
    if inner.running.get() == Some(review_turn) {
        inner.running.set(None);
    }
    inner.queue_refresh();
}

/// Starts a change Lemma makes for the user and shows for review like an agent turn (Replace in
/// Project, Rename Label): returns a new review turn, after every turn so far (the agent's next
/// turn comes after it), named `name` in the Changes panel, the toasts and the banner
/// ("Replace “a” → “b” · 3 files · 7 hunks" while its hunks are the only ones). Apply its edits
/// with `Origin::Agent { call: 0 }` inside [`in_turn`] (after [`start_review`]); Keep Turn,
/// Reject Turn ([`decide_turn`]) and Alt+U then work on it as on an agent turn. 0 when the
/// window has no review controller.
pub fn begin_named_turn(state: &Rc<WindowState>, name: &str) -> u32 {
    begin_named(state, name, false)
}

/// [`begin_named_turn`] for a change the user makes apart from the turns (a restore from
/// History, an added citation): its hunks are named after it in the Changes panel and the
/// banner, but Undo Last Turn, which takes back the last turn's work, leaves it alone.
pub fn begin_named_change(state: &Rc<WindowState>, name: &str) -> u32 {
    begin_named(state, name, true)
}

fn begin_named(state: &Rc<WindowState>, name: &str, outside: bool) -> u32 {
    let Some(inner) = controller(state) else {
        return 0;
    };
    let agent_turn = state.agent_controller().map_or(0, |c| c.turn_id());
    let in_reviews = state
        .workspace
        .documents()
        .iter()
        .filter_map(|doc| {
            let review = doc.review.borrow();
            let review = review.as_ref()?;
            review
                .hunks()
                .iter()
                .flat_map(|h| h.turns.iter().copied())
                .chain([review.turn()])
                .max()
        })
        .max()
        .unwrap_or(0);
    let top = [
        inner.turn.get(),
        inner.turn_base.get().saturating_add(agent_turn),
        inner.turns.borrow().keys().copied().max().unwrap_or(0),
        in_reviews,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let turn = top.saturating_add(1);
    // The agent's turns are numbered `turn_base` + its number: its next one comes after this.
    inner.turn_base.set(turn.saturating_sub(agent_turn));
    inner.turns.borrow_mut().insert(
        turn,
        TurnInfo {
            label: 0,
            started_unix: unix_now(),
            this_session: true,
            name: Some(name.to_owned()),
            outside,
        },
    );
    inner.queue_refresh();
    turn
}

/// Runs `f`, which changes `doc` with `Origin::Agent` edits, with `doc`'s review attributing
/// them to review turn `turn` (a [`begin_named_turn`] or [`begin_named_change`]); the agent's
/// later edits go to its own turn again.
pub fn in_turn<R>(doc: &Document, turn: u32, f: impl FnOnce() -> R) -> R {
    let previous = doc.review.borrow_mut().as_mut().map(|review| {
        let previous = review.turn();
        review.set_turn(turn);
        previous
    });
    let out = f();
    if let (Some(previous), Some(review)) = (previous, doc.review.borrow_mut().as_mut()) {
        review.set_turn(previous);
    }
    out
}

/// The review turns known in `state`'s window (for persistence).
pub fn turn_infos(state: &WindowState) -> BTreeMap<u32, TurnInfo> {
    controller(state).map_or_else(BTreeMap::new, |c| c.turns.borrow().clone())
}

/// Takes in the turns of reviews about to be restored from an earlier session: `turns` (their
/// labels and start times) and `max`, their highest review turn. From now on this session's turns
/// are numbered after `max`; turns this session already ran (and its reviews) are renumbered when
/// they would collide.
pub fn restore_turns(state: &WindowState, turns: &BTreeMap<u32, TurnInfo>, max: u32) {
    let Some(inner) = controller(state) else {
        return;
    };
    let base = inner.turn_base.get();
    if max > base {
        let shift = max - base;
        let moved = |t: u32| if t > base { t + shift } else { t };
        // This session's turns so far (all above the old base) move above the restored ones.
        if inner.turn.get() > base {
            for doc in state.workspace.documents() {
                if let Some(review) = doc.review.borrow_mut().as_mut() {
                    review.remap_turns(moved);
                    review.set_turn(moved(review.turn()));
                }
            }
            let old = std::mem::take(&mut *inner.turns.borrow_mut());
            inner
                .turns
                .borrow_mut()
                .extend(old.into_iter().map(|(t, info)| (moved(t), info)));
            inner.running.set(inner.running.get().map(moved));
        }
        inner.turn.set(moved(inner.turn.get()));
        inner.turn_base.set(max);
    }
    let mut known = inner.turns.borrow_mut();
    for (turn, info) in turns {
        known.entry(*turn).or_insert_with(|| TurnInfo {
            this_session: false,
            ..info.clone()
        });
    }
    drop(known);
    inner.queue_refresh();
}

/// Shows "Restored N pending changes from your last session" in the banner (until the changes
/// under review are no longer just those). When other changes are under review too, the banner
/// keeps counting all of them instead.
pub fn announce_restored(state: &WindowState, restored: usize) {
    let Some(inner) = controller(state) else {
        return;
    };
    let total = summary(&state.workspace).1;
    if total != restored {
        inner.queue_refresh();
        return;
    }
    let text = format!(
        "Restored {} from your last session",
        plural(restored, "pending change", "pending changes")
    );
    let docs = reviewed_documents(&state.workspace);
    inner.notice.replace(Some((text, hunks_fingerprint(&docs))));
    inner.banner_dismissed.set(false);
    inner.queue_refresh();
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What [`decide_turn`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Hunks that were decided (or that a reject of the turn's own edits in them resolved).
    pub decided: usize,
    /// Hunks that also held other changes, out of which a reject took the turn's own edits: the
    /// other changes stay, to be decided one by one.
    pub partly: usize,
    /// Files they were in.
    pub files: usize,
    /// Hunks with edits of the turn that were left alone: those the turn shares with other
    /// turns, and those holding other changes too (decided one by one).
    pub mixed: usize,
}

/// Keeps or rejects every hunk of review turn `turn` alone, in every file, as one decision (one
/// Alt+U reverts it in every file); hunks other turns also edited are left alone. A hunk of the
/// turn that the line diff paired with another change is not kept, and rejecting takes back only
/// the turn's own edits in it (see `FileReview::reject_turn`). A toast says what happened.
/// Nothing happens while the turn is running.
pub fn decide_turn(state: &Rc<WindowState>, turn: u32, decision: Decision) -> TurnOutcome {
    let mut outcome = TurnOutcome::default();
    let label = turn_label(state, turn);
    if controller(state).and_then(|c| running_turn(state, c)) == Some(turn) {
        state.toast(&format!("{label} is still running; stop it first"));
        return outcome;
    }
    let mut decided_docs = Vec::new();
    for doc in reviewed_documents(&state.workspace) {
        let picked = decide_picked(state, &doc, Pick::Turn(turn), decision);
        // What is left of the turn: hunks other turns' edits share, or that hold more than the
        // turn's own change.
        outcome.mixed += doc.review.borrow().as_ref().map_or(0, |r| {
            r.hunks().iter().filter(|h| h.turns.contains(&turn)).count()
        });
        if picked.decided + picked.partly > 0 {
            outcome.decided += picked.decided;
            outcome.partly += picked.partly;
            outcome.files += 1;
            decided_docs.push(doc);
        }
    }
    let docs: Vec<&Document> = decided_docs.iter().map(|d| &**d).collect();
    record_decision(state, &docs);
    // Those hunks are neither kept nor rejected: they stay to be decided.
    let shared = match outcome.mixed {
        0 => String::new(),
        n => format!(
            " · {} shared with other edits {} left to review",
            plural(n, "change", "changes"),
            if n == 1 { "is" } else { "are" }
        ),
    };
    let files = plural(outcome.files, "file", "files");
    let message = match (outcome.decided, outcome.partly, decision) {
        (0, 0, _) if outcome.mixed > 0 => format!(
            "{label} has no changes of its own: its {} shared with other edits; decide {} one by one",
            plural(outcome.mixed, "change is", "changes are"),
            if outcome.mixed == 1 { "it" } else { "them" }
        ),
        (0, 0, _) => format!("{label} has no pending changes"),
        (n, _, Decision::Accept) => format!(
            "Kept {}: {} in {files}{shared}",
            label_in_sentence(&label),
            plural(n, "change", "changes"),
        ),
        (n, 0, Decision::Reject) => format!(
            "Rejected {}: {} in {files}{shared}",
            label_in_sentence(&label),
            plural(n, "change", "changes"),
        ),
        // Changes that also held other edits: only the turn's part of them went.
        (0, p, Decision::Reject) => format!(
            "Rejected {}'s part of {} in {files} (the other edits there are left to \
             review){shared}",
            label_in_sentence(&label),
            plural(p, "change", "changes"),
        ),
        (n, p, Decision::Reject) => format!(
            "Rejected {}: {}, and its part of {} more (the other edits there are left to \
             review), in {files}{shared}",
            label_in_sentence(&label),
            plural(n, "change", "changes"),
            p,
        ),
    };
    if outcome.decided + outcome.partly > 0 {
        let toast = adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .button_label("_Undo")
            .action_name("win.review-undo")
            .build();
        state.toasts.add_toast(toast);
    } else {
        state.toast(&message);
    }
    outcome
}

/// "Turn 3" → "turn 3" inside a sentence.
fn label_in_sentence(label: &str) -> String {
    match label.strip_prefix("Turn ") {
        Some(rest) => format!("turn {rest}"),
        None => label.to_owned(),
    }
}

/// "Undo last turn": rejects the latest turn with pending changes ([`decide_turn`]).
pub fn undo_last_turn(state: &Rc<WindowState>) -> TurnOutcome {
    match review_summary(state).latest_turn {
        Some(turn) => decide_turn(state, turn, Decision::Reject),
        None => {
            state.toast("No agent turn has pending changes");
            TurnOutcome::default()
        }
    }
}

/// Keeps or rejects the hunks `ids` of `doc` as one decision (a row of the Changes panel).
pub fn decide_hunks(state: &Rc<WindowState>, doc: &DocRef, ids: &[HunkId], decision: Decision) {
    let picked = decide_picked(state, doc, Pick::Hunks(ids), decision);
    if picked.decided + picked.partly > 0 {
        record_decision(state, &[&**doc]);
    }
}

/// What [`decide_picked`] decides in a document.
#[derive(Clone, Copy)]
enum Pick<'a> {
    /// The hunks of one turn alone.
    Turn(u32),
    Hunks(&'a [HunkId]),
}

/// What [`decide_picked`] did in a document.
#[derive(Clone, Copy, Debug, Default)]
struct Picked {
    /// Hunks decided (gone).
    decided: usize,
    /// Hunks a reject of a turn took the turn's own edits out of, and that stay with the other
    /// changes they held (see `FileReview::reject_turn`).
    partly: usize,
}

/// Keeps or rejects the picked hunks of `doc` as one journalled decision (without recording it
/// for Alt+U at the window level); says how many hunks it decided, and how many it took the
/// turn's part out of.
fn decide_picked(state: &Rc<WindowState>, doc: &DocRef, pick: Pick, decision: Decision) -> Picked {
    // Decisions need hunks that describe the text exactly (typing may be 50 ms ahead).
    recompute_if_stale(doc);
    if decision == Decision::Reject {
        // The review follows what closing an open user action changes edit by edit, so the
        // picked hunks keep their ids (see `decide`).
        doc.settle_user_action();
    }
    let before = hunk_count(doc);
    // The turn's own hunks: those a reject of the turn leaves to the turn (it could not tell
    // what the turn changed there) still count as the turn's afterwards.
    let own = |doc: &Document| match pick {
        Pick::Turn(turn) => doc
            .review
            .borrow()
            .as_ref()
            .map_or(0, |r| r.hunks_of_turn(turn).len()),
        Pick::Hunks(_) => 0,
    };
    let own_before = own(doc);
    let text = doc.text();
    match decision {
        Decision::Accept => {
            {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut() else {
                    return Picked::default();
                };
                match pick {
                    Pick::Turn(turn) => {
                        review.accept_turn(turn, &text);
                    }
                    Pick::Hunks(ids) => {
                        review.accept_hunks(ids, &text);
                    }
                }
            }
            recompute_against(doc, &text);
        }
        Decision::Reject => {
            let (edits, touched) = {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut() else {
                    return Picked::default();
                };
                // A set: Reject Turn (Replace in Project's Undo) can pick thousands of hunks.
                let ids: HashSet<HunkId> = match pick {
                    Pick::Turn(turn) => review.hunks_of_turn(turn).iter().map(|h| h.id).collect(),
                    Pick::Hunks(ids) => ids.iter().copied().collect(),
                };
                let touched = review
                    .hunks()
                    .iter()
                    .any(|h| h.user_touched && ids.contains(&h.id));
                let edits = match pick {
                    Pick::Turn(turn) => review.reject_turn(turn, &text),
                    Pick::Hunks(ids) => review.reject_hunks(ids, &text),
                };
                (edits, touched)
            };
            if !edits.is_empty()
                && let Err(e) = doc.apply_edits(&edits, Origin::Review)
            {
                recompute(doc);
                state.toast(&format!("Could not reject the changes: {e}"));
                return Picked::default();
            }
            recompute(doc);
            if !edits.is_empty() {
                finish_reject(state, doc, touched, &text);
                let decided = before.saturating_sub(hunk_count(doc));
                // A hunk that held more than the turn's change stays, without the turn.
                let partly = own_before.saturating_sub(decided + own(doc));
                return Picked {
                    decided,
                    // Something was taken back, if only part of a hunk.
                    partly: if decided + partly == 0 { 1 } else { partly },
                };
            }
        }
    }
    Picked {
        decided: before.saturating_sub(hunk_count(doc)),
        partly: 0,
    }
}

/// `win.review-keep-turn(u)`, `win.review-reject-turn(u)` (review turn numbers, as the Changes
/// panel uses them) and `win.review-undo-turn`.
fn install_turn_actions(state: &Rc<WindowState>) {
    let window = &state.window;
    for (name, decision) in [
        ("review-keep-turn", Decision::Accept),
        ("review-reject-turn", Decision::Reject),
    ] {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::UINT32));
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, param| {
            if let (Some(state), Some(turn)) = (weak.upgrade(), param.and_then(|p| p.get::<u32>()))
            {
                decide_turn(&state, turn, decision);
            }
        });
        window.add_action(&action);
    }
    let action = gio::SimpleAction::new("review-undo-turn", None);
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, _| {
        if let Some(state) = weak.upgrade() {
            undo_last_turn(&state);
        }
    });
    window.add_action(&action);
}

/// `:KeepTurn [n]`, `:RejectTurn [n]` (this session's turn `n`, else the latest turn with
/// changes) and `:UndoTurn`. Runs from an idle callback, like the other review commands.
fn turn_ex_command(state: &Rc<WindowState>, command: &str) -> bool {
    let mut words = command.trim().trim_start_matches(':').split_whitespace();
    let decision = match words.next() {
        Some("KeepTurn") => Some(Decision::Accept),
        Some("RejectTurn") => Some(Decision::Reject),
        Some("UndoTurn") => None,
        _ => return false,
    };
    let number = words.next().map(str::parse::<u32>);
    let weak = Rc::downgrade(state);
    glib::idle_add_local_once(move || {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let turn = match number {
            Some(Ok(n)) => controller(&state).map(|c| c.agent_review_turn(n)),
            Some(Err(_)) => {
                state.toast("Usage: :KeepTurn [n], :RejectTurn [n]");
                return;
            }
            None => review_summary(&state).latest_turn,
        };
        match (decision, turn) {
            (None, _) => {
                undo_last_turn(&state);
            }
            (Some(decision), Some(turn)) => {
                decide_turn(&state, turn, decision);
            }
            (Some(_), None) => state.toast("No agent turn has pending changes"),
        }
    });
    true
}

// ----- decisions -----

/// The hunk under `editor`'s cursor: the hunk whose lines contain the cursor line, or the pure
/// deletion anchored at it. On the last line with text, a deletion at the end of the file (drawn
/// below that line) counts too.
pub fn hunk_at_cursor(editor: &Editor) -> Option<HunkId> {
    hunk_for_line(editor.document(), cursor_line(editor))
}

/// The hunk a cursor on `line` (0-based) decides: the hunk whose lines contain it, or the pure
/// deletion anchored at it; on the last line with text, also a deletion at the end of the file.
pub fn hunk_for_line(doc: &Document, line: u32) -> Option<HunkId> {
    let review = doc.review.borrow();
    let review = review.as_ref()?;
    if let Some(h) = review.hunk_at_line(line) {
        return Some(h.id);
    }
    let last = doc.line_index().line_count().saturating_sub(1);
    (line + 1 == last)
        .then(|| review.hunk_at_line(last))
        .flatten()
        .filter(|h| h.cur_lines.is_empty())
        .map(|h| h.id)
}

/// The cursor's line in `LineIndex` numbering (0-based).
fn cursor_line(editor: &Editor) -> u32 {
    let buffer = editor.buffer();
    let iter = buffer.iter_at_mark(&buffer.get_insert());
    let doc = editor.document();
    let offset = doc.byte_offset(&iter);
    doc.line_index().line_of(offset)
}

/// Accepts or rejects one hunk of `doc` (§4.4).
///
/// - Accept moves the hunk into the baseline: no buffer change, nothing is written.
/// - Reject applies the edit restoring the baseline text as one undoable `Origin::Review`
///   action and saves the file. Rejecting a hunk the user edited shows a toast with Undo. A file
///   the agent created whose last hunk is rejected goes to the trash.
///
/// Both are journalled for Alt+U ([`undo_last`]).
pub fn decide(
    state: &Rc<WindowState>,
    doc: &DocRef,
    id: HunkId,
    decision: Decision,
) -> Result<(), String> {
    if decision == Decision::Reject {
        // What closing an open user action changes goes in first: the review records a reject
        // before its edit applies, and a refusal would leave that record wrong (see
        // `Document::settle_user_action`). A keep applies no edit, and leaves Vim's insert
        // session one undo step.
        doc.settle_user_action();
    }
    let text = doc.text();
    match decision {
        Decision::Accept => {
            let result = doc
                .review
                .borrow_mut()
                .as_mut()
                .ok_or("this file is not under review")?
                .accept(id, &text);
            if let Err(e) = result {
                recompute_against(doc, &text);
                return Err(format!("Could not accept the change: {e}"));
            }
            record_decision(state, &[&**doc]);
            recompute_against(doc, &text);
        }
        Decision::Reject => {
            let result = {
                let mut review = doc.review.borrow_mut();
                let review = review.as_mut().ok_or("this file is not under review")?;
                let touched = review.hunk(id).is_some_and(|h| h.user_touched);
                review.reject(id, &text).map(|edit| (edit, touched))
            };
            let (edit, touched) = match result {
                Ok(r) => r,
                Err(e) => {
                    recompute(doc);
                    return Err(format!("Could not reject the change: {e}"));
                }
            };
            if edit.range.is_empty() && edit.text.is_empty() {
                // The hunk held no change any more (typed back, the recompute 50 ms after
                // typing still to come): nothing was rejected, and nothing is journalled, so
                // no decision is recorded either (the next Alt+U reverted an older one).
                recompute(doc);
                return Ok(());
            }
            doc.apply_edits(&[edit], Origin::Review)
                .map_err(|e| format!("Could not reject the change: {e}"))?;
            recompute(doc);
            // Recorded after `finish_reject`, as every decision is: a file it trashes belongs
            // to this decision (see `trash_new_file`).
            finish_reject(state, doc, touched, &text);
            record_decision(state, &[&**doc]);
        }
    }
    Ok(())
}

/// Journals one decision (on one or more documents) for Alt+U.
fn record_decision(state: &WindowState, docs: &[&Document]) {
    if let Some(inner) = controller(state) {
        inner.record_decision(docs);
    }
}

/// After rejected hunks were restored in `doc`, whose text was `before` the reject: save it (or
/// trash a file the agent created that is now empty again) and, if the user had edited a
/// rejected hunk, offer Undo.
fn finish_reject(state: &Rc<WindowState>, doc: &DocRef, touched: bool, before: &str) {
    let clean = doc
        .review
        .borrow()
        .as_ref()
        .is_some_and(FileReview::is_clean);
    let trashed = clean && is_new_file(doc) && doc.with_text(str::is_empty);
    if trashed {
        trash_new_file(state, doc, before);
    } else {
        save_quietly(state, doc);
    }
    if touched {
        let title = if trashed {
            format!(
                "Rejected {}: the new file went to the trash with your edits in it",
                doc.display_name()
            )
        } else {
            "Rejected — your edits in this hunk were discarded".to_owned()
        };
        let toast = adw::Toast::builder()
            .title(title)
            .use_markup(false)
            .button_label("_Undo")
            .action_name("win.review-undo")
            .build();
        state.toasts.add_toast(toast);
    }
}

/// Saves `doc` in the background; a failure becomes a toast. When another program changed the
/// file, the toast offers to overwrite it, as for the user's own saves.
fn save_quietly(state: &Rc<WindowState>, doc: &DocRef) {
    let save = doc.save();
    let (state, name, doc) = (Rc::downgrade(state), doc.display_name(), Rc::downgrade(doc));
    glib::spawn_future_local(async move {
        let (Err(e), Some(state)) = (save.await, state.upgrade()) else {
            return;
        };
        match (e, doc.upgrade()) {
            (SaveError::ExternallyModified, Some(doc)) => state.offer_overwrite(&doc),
            (e, _) => state.toast(&format!("Could not save “{name}”: {e}")),
        }
    });
}

/// A file the agent created and the user rejected entirely (its text was `before` the reject):
/// close its tab, drop it from the workspace and move the file to the trash (on a file system
/// without a trash, delete it). Its review is kept
/// for Alt+U, which brings the file back with that text under review ([`restore_trashed`]), and
/// the text goes to the local history first (the user's typing in it may never have been saved).
/// Runs from an idle callback, since a decision may come from inside the Vim context's key
/// handling. The file belongs to the decision being made, which is recorded once it is made.
fn trash_new_file(state: &Rc<WindowState>, doc: &DocRef, before: &str) {
    let decision = controller(state).map_or(0, |inner| inner.next_decision.get());
    let (state, doc, before) = (Rc::downgrade(state), doc.clone(), before.to_owned());
    glib::idle_add_local_once(move || {
        let Some(state) = state.upgrade() else {
            return;
        };
        let done = Rc::new(Cell::new(None));
        let review = doc.review.replace(None);
        if let Some(mut review) = review {
            keep_dropped_events(doc.path(), review.drain_events());
            if let Some(inner) = controller(&state) {
                inner.keep_trashed(Trashed {
                    decision,
                    path: doc.path().to_path_buf(),
                    review,
                    done: done.clone(),
                });
            }
        }
        if !before.is_empty() {
            crate::agent::history::record(
                &state,
                doc.path(),
                &before,
                crate::agent::history::HistoryReason::Reject,
            );
        }
        drop_review(&doc);
        match state.editor_for(&doc) {
            Some(editor) => state.close_editor(&editor, true),
            None => state.workspace.release(&doc),
        }
        let path = doc.path().to_path_buf();
        glib::spawn_future_local(async move {
            let file = gio::File::for_path(&path);
            let trashed = file.trash_future(glib::Priority::DEFAULT).await;
            let gone = match &trashed {
                Ok(()) => true,
                // No trash on this file system (a container's volume, some network mounts,
                // a memory file system). The rejected file must not stay, with the agent's
                // text in it and no longer under review: it is deleted. Its text is in the
                // local history, and Alt+U brings the file back as from the trash.
                Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            let name = path.file_name().map_or_else(
                                || path.display().to_string(),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            let toast = adw::Toast::builder()
                                .title(format!("{name} was deleted: this folder has no trash"))
                                .use_markup(false)
                                .button_label("_Undo")
                                .action_name("win.review-undo")
                                .build();
                            state.toasts.add_toast(toast);
                            true
                        }
                        Err(e) => {
                            state.toast(&format!("Could not delete {}: {e}", path.display()));
                            false
                        }
                    }
                }
                Err(e) => {
                    state.toast(&format!(
                        "Could not move {} to the trash: {}",
                        path.display(),
                        e.message()
                    ));
                    false
                }
            };
            done.set(Some(gone));
        });
    });
}

/// A file the agent created whose reject sent it to the trash ([`trash_new_file`]), for Alt+U.
struct Trashed {
    /// The number of the decision that rejected it (see [`Inner::decisions`]).
    decision: u64,
    path: PathBuf,
    /// Its review, whose journal holds the reject.
    review: FileReview,
    /// Whether the move to the trash went well, once it is over.
    done: Rc<Cell<Option<bool>>>,
}

/// Alt+U of the reject that sent `trashed`'s file to the trash: the file comes back (in a tab)
/// with its review and the text the reject took, which is under review again as it was (the
/// user's edits in it too). A file of that name that another program made meanwhile is left
/// alone; once it is gone, Alt+U tries again (see [`Inner::retry_trashed`]).
async fn restore_trashed(state: Rc<WindowState>, trashed: Trashed) {
    let path = trashed.path.clone();
    let name = relative_name(&state, &path);
    let retry = |state: &WindowState, trashed: Trashed, why: String| {
        state.toast(&format!("Cannot undo the last decision: {why}"));
        if let Some(inner) = controller(state) {
            inner.retry_trashed(trashed);
        }
    };
    // The move to the trash may still run (an Alt+U right after the reject).
    let started = std::time::Instant::now();
    while trashed.done.get().is_none() && started.elapsed() < Duration::from_secs(5) {
        glib::timeout_future(Duration::from_millis(10)).await;
    }
    let done = trashed.done.get();
    if done.is_none() {
        let why = format!("{name} is still being moved to the trash");
        retry(&state, trashed, why);
        return;
    }
    // A file that could not go to the trash is still the one the reject left.
    if state.workspace.get(&path).is_some() || (done == Some(true) && path.exists()) {
        retry(&state, trashed, format!("{name} exists again"));
        return;
    }
    // An empty file, as the reject left it: the undo puts the text back.
    let created = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, ""));
    if let Err(e) = created {
        retry(&state, trashed, format!("{name}: {e}"));
        return;
    }
    let review = trashed.review;
    let editor = match state.open_file(&path).await {
        Ok(editor) => editor,
        Err(e) => {
            state.toast(&format!("Cannot undo the last decision: {e}"));
            return;
        }
    };
    let doc = editor.document().clone();
    if doc.review.borrow().is_some() || !doc.with_text(str::is_empty) {
        state.toast(&format!(
            "Cannot undo the last decision: {name} was edited since"
        ));
        return;
    }
    adopt_review(&doc, review);
    // A file the agent created again: rejecting all of it trashes it again.
    start_review(&doc, true);
    match undo_in(&state, &doc) {
        Some(Some(line)) => show_line(&state, &doc, line),
        Some(None) => {}
        None => state.toast(&format!("Cannot undo the last decision in {name}")),
    }
}

/// [`decide`], then move the cursor to the next hunk (in this file, else in the next file).
pub fn decide_and_advance(state: &Rc<WindowState>, doc: &DocRef, id: HunkId, decision: Decision) {
    let line = doc
        .review
        .borrow()
        .as_ref()
        .and_then(|r| r.hunk(id))
        .map(|h| h.cur_lines.start);
    let Some(line) = line else {
        return;
    };
    match decide(state, doc, id, decision) {
        Ok(()) => advance(state, doc, line),
        Err(e) => state.toast(&e),
    }
}

fn decide_at_cursor(state: &Rc<WindowState>, decision: Decision) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    match hunk_at_cursor(&editor) {
        Some(id) => decide_and_advance(state, editor.document(), id, decision),
        None => state.toast("No change at the cursor"),
    }
}

/// After a decision at `line` of `doc`: the first hunk at or after that line, else the first
/// hunk of the file, else the next file with hunks.
fn advance(state: &Rc<WindowState>, doc: &DocRef, line: u32) {
    let target = doc.review.borrow().as_ref().and_then(|r| {
        let hunks = r.hunks();
        hunks
            .iter()
            .find(|h| h.cur_lines.start >= line)
            .or_else(|| hunks.first())
            .map(|h| h.cur_lines.start)
    });
    match target {
        Some(line) => show_line(state, doc, line),
        None => {
            if !reviewed_documents(&state.workspace).is_empty() {
                goto_next(state, true);
            }
        }
    }
}

/// Accepts or rejects every hunk of `doc`.
pub fn decide_document(state: &Rc<WindowState>, doc: &DocRef, decision: Decision) {
    if decide_all_in(state, doc, decision) {
        record_decision(state, &[&**doc]);
    }
}

/// Accepts or rejects every hunk of `doc` without journalling the decision; true when there was
/// something to decide.
fn decide_all_in(state: &Rc<WindowState>, doc: &DocRef, decision: Decision) -> bool {
    if decision == Decision::Reject {
        // See `decide`.
        doc.settle_user_action();
    }
    let text = doc.text();
    match decision {
        Decision::Accept => {
            {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut().filter(|r| !r.hunks().is_empty()) else {
                    return false;
                };
                review.accept_all(&text);
            }
            recompute_against(doc, &text);
        }
        Decision::Reject => {
            let (edits, touched) = {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut().filter(|r| !r.hunks().is_empty()) else {
                    return false;
                };
                let touched = review.hunks().iter().any(|h| h.user_touched);
                (review.reject_all(&text), touched)
            };
            if let Err(e) = doc.apply_edits(&edits, Origin::Review) {
                recompute(doc);
                state.toast(&format!("Could not reject the changes: {e}"));
                return false;
            }
            recompute(doc);
            finish_reject(state, doc, touched, &text);
        }
    }
    true
}

fn decide_active_file(state: &Rc<WindowState>, decision: Decision) {
    if let Some(editor) = state.active_editor() {
        decide_document(state, editor.document(), decision);
    }
}

/// Accepts or rejects every hunk of every file under review, as one decision: one Alt+U
/// reverts it in every file.
pub fn decide_everything(state: &Rc<WindowState>, decision: Decision) {
    let docs: Vec<DocRef> = reviewed_documents(&state.workspace)
        .into_iter()
        .filter(|doc| decide_all_in(state, doc, decision))
        .collect();
    let docs: Vec<&Document> = docs.iter().map(|d| &**d).collect();
    record_decision(state, &docs);
}

/// Alt+U: reverts the most recent accept or reject (§4.4) in every file it changed, and moves
/// the cursor to the first hunk that came back; a file the agent created that the reject sent
/// to the trash comes back with its tab ([`restore_trashed`]). Only the most recent decision is
/// ever reverted: when it cannot be (its file left the review, or its text was edited since), a
/// toast says so and older decisions stay as they are.
pub fn undo_last(state: &Rc<WindowState>) {
    let Some(inner) = controller(state) else {
        return;
    };
    let Some((number, paths)) = inner.decisions.borrow_mut().pop() else {
        state.toast("Nothing to undo");
        return;
    };
    let mut first_restored = None;
    let mut gone = Vec::new();
    let mut stuck = Vec::new();
    let mut untrashed = false;
    for path in &paths {
        let doc = state
            .workspace
            .documents()
            .into_iter()
            .find(|d| d.path() == path);
        let Some(doc) = doc.filter(|d| d.review.borrow().is_some()) else {
            // A file the agent created that this decision sent to the trash comes back (not one
            // an earlier decision trashed: the agent may have created it again since).
            let trashed = {
                let mut trashed = inner.trashed.borrow_mut();
                let at = trashed
                    .iter()
                    .rposition(|t| t.decision == number && t.path == *path);
                at.map(|at| trashed.remove(at))
            };
            match trashed {
                Some(trashed) => {
                    untrashed = true;
                    glib::spawn_future_local(restore_trashed(state.clone(), trashed));
                }
                None => gone.push(relative_name(state, path)),
            }
            continue;
        };
        match undo_in(state, &doc) {
            Some(line) => {
                if first_restored.is_none() {
                    first_restored = Some((doc, line));
                }
            }
            None => stuck.push(relative_name(state, path)),
        }
    }
    if let Some((doc, Some(line))) = &first_restored {
        show_line(state, doc, *line);
    }
    // A file coming back from the trash says itself when it cannot.
    let restored = first_restored.is_some() || untrashed;
    let message = match (gone.as_slice(), stuck.as_slice()) {
        ([], []) => return,
        ([name], []) if !restored => {
            format!("Cannot undo the last decision: {name} is no longer under review")
        }
        ([], [name]) if !restored => {
            format!("Cannot undo the last decision: {name} was edited since")
        }
        _ if !restored => "Cannot undo the last decision".to_owned(),
        _ => {
            let names: Vec<String> = gone.into_iter().chain(stuck).collect();
            format!("The last decision was not undone in {}", names.join(", "))
        }
    };
    state.toast(&message);
}

/// Reverts the last decision journalled in `doc`'s review. `None` when there is none, or when it
/// cannot be reverted any more (it is then dropped: an older decision is never reverted in its
/// place); otherwise the first line of the hunk that came back, if any.
fn undo_in(state: &Rc<WindowState>, doc: &DocRef) -> Option<Option<u32>> {
    // The journal gives up its entry before the edits apply (see `decide`).
    doc.settle_user_action();
    let text = doc.text();
    let action = doc
        .review
        .borrow_mut()
        .as_mut()?
        .undo_last_review_action(&text);
    let delta = match action? {
        UndoAction::Unaccepted => recompute_against(doc, &text),
        UndoAction::Edits(edits) => {
            if let Err(e) = doc.apply_edits(&edits, Origin::Review) {
                state.toast(&format!("Could not restore the change: {e}"));
            }
            let delta = recompute(doc);
            save_quietly(state, doc);
            delta
        }
    };
    let line = delta.added.first().and_then(|id| {
        doc.review
            .borrow()
            .as_ref()
            .and_then(|r| r.hunk(*id))
            .map(|h| h.cur_lines.start)
    });
    Some(line)
}

// ----- navigation -----

/// Alt+] / Alt+[: the next (previous) hunk after (before) the cursor; past the last hunk of the
/// file, the first (last) hunk of the next (previous) file with hunks, which is opened. Wraps
/// around.
pub fn goto_next(state: &Rc<WindowState>, forward: bool) {
    let docs = reviewed_documents(&state.workspace);
    if docs.is_empty() {
        state.toast("No changes to review");
        return;
    }
    let active = state.active_editor();
    let current = active
        .as_ref()
        .map(|e| (e.document().clone(), cursor_line(e)));
    if let Some((doc, line)) = &current
        && let Some(target) = hunk_starts(doc).and_then(|starts| {
            if forward {
                starts.into_iter().find(|s| s > line)
            } else {
                starts.into_iter().rev().find(|s| s < line)
            }
        })
    {
        show_line(state, doc, target);
        return;
    }
    // The next file in path order (rotating), ending with the current file (wrap-around).
    let n = docs.len();
    let start = match &current {
        Some((doc, _)) => {
            let at = docs.partition_point(|d| d.path() < doc.path());
            let here = docs.get(at).is_some_and(|d| Rc::ptr_eq(d, doc));
            if forward {
                if here { at + 1 } else { at }
            } else {
                at + n - 1
            }
        }
        None if forward => 0,
        None => n - 1,
    };
    let doc = docs[start % n].clone();
    let line = hunk_starts(&doc).and_then(|starts| {
        if forward {
            starts.first().copied()
        } else {
            starts.last().copied()
        }
    });
    if let Some(line) = line {
        show_line(state, &doc, line);
    }
}

/// Moves to the first hunk of the first file under review (the banner's Review button).
fn goto_first(state: &Rc<WindowState>) {
    let docs = reviewed_documents(&state.workspace);
    if let Some(doc) = docs.first()
        && let Some(line) = hunk_starts(doc).and_then(|s| s.first().copied())
    {
        show_line(state, doc, line);
    }
}

/// First current line of every hunk of `doc`, in order.
fn hunk_starts(doc: &Document) -> Option<Vec<u32>> {
    let review = doc.review.borrow();
    let starts: Vec<u32> = review
        .as_ref()?
        .hunks()
        .iter()
        .map(|h| h.cur_lines.start)
        .collect();
    (!starts.is_empty()).then_some(starts)
}

/// Shows `doc` (opening it in a tab if needed), puts the cursor at the start of `line` and
/// centres it.
pub fn show_line(state: &Rc<WindowState>, doc: &DocRef, line: u32) {
    if let Some(editor) = state.editor_for(doc) {
        state.select_editor(&editor);
        place_cursor(&editor, line);
        return;
    }
    let (state, path) = (state.clone(), doc.path().to_path_buf());
    glib::spawn_future_local(async move {
        match state.open_file(&path).await {
            Ok(editor) => {
                place_cursor(&editor, line);
                // A new view's first layout can leave it scrolled elsewhere (a closed document
                // under review opened here showed its end): scroll again once it has drawn.
                editor.after_first_frame(|editor| {
                    let buffer = editor.buffer();
                    editor
                        .view()
                        .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
                });
            }
            Err(e) => state.toast(&e),
        }
    });
}

fn place_cursor(editor: &Editor, line: u32) {
    let doc = editor.document();
    let offset = {
        let index = doc.line_index();
        index.line_start(line.min(index.line_count().saturating_sub(1)))
    };
    let Some(iter) = doc.iter_at_byte(offset) else {
        return;
    };
    let buffer = editor.buffer();
    buffer.place_cursor(&iter);
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
}

// ----- panels and modes -----

/// Shows the sidebar with the Changes page.
pub fn show_changes_panel(state: &Rc<WindowState>) {
    state.sidebar.show_page(PAGE_CHANGES);
}

/// Turns reading mode (Alt+D) on or off: hunk bands shrink to their header and the deleted
/// lines are hidden.
pub fn set_reading_mode(state: &Rc<WindowState>, on: bool) {
    if let Some(inner) = controller(state) {
        inner.reading.set(on);
    }
    if let Some(action) = state
        .window
        .lookup_action("review-reading-mode")
        .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
    {
        action.set_state(&on.into());
    }
    for editor in state.editors() {
        if let Some(review) = editor.view().review_state() {
            review.set_reading(on);
        }
    }
}

/// The path of `doc` relative to the project root, for display: what the user sees, in the
/// system's spelling (`chapters\intro.tex` on Windows). What the agent is told names files with
/// `/` on every system instead: [`lemma_core::ProjectRoot::agent_name`].
pub fn relative_name(state: &WindowState, path: &Path) -> String {
    let root = state.workspace.root();
    root.relative(path).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_spans_are_one_based() {
        assert_eq!(line_span(&(2..5)), "lines 3–5");
        assert_eq!(line_span(&(2..3)), "line 3");
        assert_eq!(line_span(&(4..4)), "line 5");
    }

    #[test]
    fn unlisted_feedback_is_counted() {
        let rejected = |l: u32| ReviewEvent::Rejected {
            lines: l..l + 1,
            proposed: "a".into(),
            kept: "b".into(),
        };
        let events = vec![
            rejected(40),
            rejected(1199),
            ReviewEvent::Accepted { lines: 2..3 },
            ReviewEvent::UndoneByUser { lines: 5..5 },
        ];
        assert_eq!(
            feedback_summary(&events),
            "and 4 more: the user rejected 2 of your changes (lines 41–1200), accepted 1 of \
             your changes (line 3), undid 1 of your changes (line 6)"
        );
    }

    #[test]
    fn quotes_are_shortened_on_char_boundaries() {
        assert_eq!(quote("a\"b\n"), "\"a\\\"b\\n\"");
        let long = "é".repeat(FEEDBACK_QUOTE_MAX);
        let quoted = quote(&long);
        assert!(quoted.ends_with('…'));
        assert!(quoted.len() < long.len());
    }

    #[test]
    fn ex_commands_and_chords_cover_every_action() {
        for (_, action) in EX_COMMANDS {
            assert!(
                ACCELS
                    .iter()
                    .any(|(a, _)| a.strip_prefix("win.") == Some(action)),
                "{action} has no chord"
            );
        }
        let mut names: Vec<&str> = ACCELS.iter().map(|(a, _)| *a).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ACCELS.len(), "duplicate action");
    }
}
