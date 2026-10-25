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

/// Stops reviewing `doc` (its review is dropped) and refreshes the review UI.
pub fn drop_review(doc: &DocRef) {
    doc.review.replace(None);
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
    let version = doc.version();
    with_tracked(doc, |t| {
        if let Some(timer) = t.timer.take() {
            timer.remove();
        }
        t.computed = Some(version);
    });
    doc.notify_hunks_changed();
    notify_controllers(doc);
    delta
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

/// Takes the queued review events of every document in `ws` and formats them for the agent as a
/// `<review_feedback>` block (lines 1-based). `None` when nothing happened.
///
/// The block goes with the next prompt, so it is kept small: per file, the first
/// [`FEEDBACK_EVENTS_PER_FILE`] events are listed (texts quoted up to [`FEEDBACK_QUOTE_MAX`]
/// bytes) and the others are counted in one line; past [`FEEDBACK_MAX_BYTES`], files are only
/// counted.
pub fn drain_feedback(ws: &Workspace) -> Option<String> {
    let root = ws.root().path().to_path_buf();
    let mut docs = ws.documents();
    docs.sort_by(|a, b| a.path().cmp(b.path()));
    let mut out = String::new();
    let (mut unlisted, mut unlisted_files) = (0, 0);
    for doc in docs {
        let events = match doc.review.borrow_mut().as_mut() {
            Some(review) => review.drain_events(),
            None => continue,
        };
        if events.is_empty() {
            continue;
        }
        if out.len() >= FEEDBACK_MAX_BYTES {
            unlisted += events.len();
            unlisted_files += 1;
            continue;
        }
        let name = doc
            .path()
            .strip_prefix(&root)
            .unwrap_or(doc.path())
            .display()
            .to_string();
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
    /// Recent decisions, oldest first, for Alt+U: the documents each one changed (several for
    /// Accept/Reject in all files, which Alt+U reverts as one).
    decisions: RefCell<Vec<Vec<PathBuf>>>,
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
            let text = match self.named_origin(&summary) {
                Some(Some(name)) => format!("{name} · {counts}"),
                Some(None) => format!("Changes to review · {counts}"),
                None => format!(
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

    /// Who made everything under review when it is not the agent: `Some(Some(name))` when every
    /// hunk belongs to named turns ([`begin_named_turn`]) of one name, `Some(None)` for several
    /// names, `None` when some hunk is the agent's (or made outside any turn).
    fn named_origin(&self, summary: &ReviewSummary) -> Option<Option<String>> {
        let turns = self.turns.borrow();
        let mut first: Option<String> = None;
        let mut one = true;
        for doc in &summary.docs {
            let review = doc.review.borrow();
            let Some(review) = review.as_ref() else {
                continue;
            };
            for hunk in review.hunks() {
                if hunk.turns.is_empty() {
                    return None;
                }
                for turn in &hunk.turns {
                    let name = turns.get(turn).and_then(|info| info.name.as_deref())?;
                    match &first {
                        None => first = Some(name.to_owned()),
                        Some(f) => one &= f == name,
                    }
                }
            }
        }
        let first = first?;
        Some(one.then_some(first))
    }

    fn record_decision(&self, docs: &[&Document]) {
        if docs.is_empty() {
            return;
        }
        let mut decisions = self.decisions.borrow_mut();
        if decisions.len() == DECISIONS_KEPT {
            decisions.remove(0);
        }
        decisions.push(docs.iter().map(|d| d.path().to_path_buf()).collect());
    }
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
    for doc in &docs {
        let review = doc.review.borrow();
        let Some(review) = review.as_ref() else {
            continue;
        };
        let mut by_group: BTreeMap<GroupKind, GroupFile> = BTreeMap::new();
        for h in review.hunks() {
            hunks += 1;
            latest = latest.max(h.turns.iter().copied().filter(|t| *t > 0).max());
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
        },
    );
    inner.queue_refresh();
    turn
}

/// Runs `f`, which changes `doc` with `Origin::Agent` edits, with `doc`'s review attributing
/// them to review turn `turn` (a [`begin_named_turn`]); the agent's later edits go to its own
/// turn again.
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

/// Runs `f`, which changes `doc` with `Origin::Agent` edits that no agent turn made (a restore
/// from History, which shows the old text as reviewable hunks), with `doc`'s review attributing
/// them to turn 0: they are "Other changes", never part of the last turn (Keep/Reject Turn and
/// Undo Last Turn leave them alone).
pub fn outside_turn<R>(doc: &Document, f: impl FnOnce() -> R) -> R {
    let turn = doc.review.borrow_mut().as_mut().map(|review| {
        let turn = review.turn();
        review.set_turn(0);
        turn
    });
    let out = f();
    if let (Some(turn), Some(review)) = (turn, doc.review.borrow_mut().as_mut()) {
        review.set_turn(turn);
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
        let n = decide_picked(state, &doc, Pick::Turn(turn), decision);
        // What is left of the turn: hunks other turns' edits share, or that hold more than the
        // turn's own change.
        outcome.mixed += doc.review.borrow().as_ref().map_or(0, |r| {
            r.hunks().iter().filter(|h| h.turns.contains(&turn)).count()
        });
        if n > 0 {
            outcome.decided += n;
            outcome.files += 1;
            decided_docs.push(doc);
        }
    }
    let docs: Vec<&Document> = decided_docs.iter().map(|d| &**d).collect();
    record_decision(state, &docs);
    let shared = match outcome.mixed {
        0 => String::new(),
        n => format!(
            " · {} shared with other edits {} kept",
            plural(n, "change", "changes"),
            if n == 1 { "was" } else { "were" }
        ),
    };
    let message = match (outcome.decided, decision) {
        (0, _) if outcome.mixed > 0 => format!(
            "{label} has no changes of its own: its {} shared with other edits; decide {} one by one",
            plural(outcome.mixed, "change is", "changes are"),
            if outcome.mixed == 1 { "it" } else { "them" }
        ),
        (0, _) => format!("{label} has no pending changes"),
        (n, Decision::Accept) => format!(
            "Kept {}: {} in {}{shared}",
            label_in_sentence(&label),
            plural(n, "change", "changes"),
            plural(outcome.files, "file", "files")
        ),
        (n, Decision::Reject) => format!(
            "Rejected {}: {} in {}{shared}",
            label_in_sentence(&label),
            plural(n, "change", "changes"),
            plural(outcome.files, "file", "files")
        ),
    };
    if outcome.decided > 0 {
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
    if decide_picked(state, doc, Pick::Hunks(ids), decision) > 0 {
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

/// Keeps or rejects the picked hunks of `doc` as one journalled decision (without recording it
/// for Alt+U at the window level); returns how many hunks it decided.
fn decide_picked(state: &Rc<WindowState>, doc: &DocRef, pick: Pick, decision: Decision) -> usize {
    // Decisions need hunks that describe the text exactly (typing may be 50 ms ahead).
    recompute_if_stale(doc);
    if decision == Decision::Reject {
        // The review follows what closing an open user action changes edit by edit, so the
        // picked hunks keep their ids (see `decide`).
        doc.settle_user_action();
    }
    let before = hunk_count(doc);
    let text = doc.text();
    match decision {
        Decision::Accept => {
            {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut() else {
                    return 0;
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
            recompute(doc);
        }
        Decision::Reject => {
            let (edits, touched) = {
                let mut review = doc.review.borrow_mut();
                let Some(review) = review.as_mut() else {
                    return 0;
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
                return 0;
            }
            recompute(doc);
            if !edits.is_empty() {
                finish_reject(state, doc, touched);
            }
        }
    }
    before.saturating_sub(hunk_count(doc))
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
                recompute(doc);
                return Err(format!("Could not accept the change: {e}"));
            }
            record_decision(state, &[&**doc]);
            recompute(doc);
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
            doc.apply_edits(&[edit], Origin::Review)
                .map_err(|e| format!("Could not reject the change: {e}"))?;
            record_decision(state, &[&**doc]);
            recompute(doc);
            finish_reject(state, doc, touched);
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

/// After rejected hunks were restored in `doc`: save it (or trash a file the agent created that
/// is now empty again) and, if the user had edited a rejected hunk, offer Undo.
fn finish_reject(state: &Rc<WindowState>, doc: &DocRef, touched: bool) {
    let clean = doc
        .review
        .borrow()
        .as_ref()
        .is_some_and(FileReview::is_clean);
    if clean && is_new_file(doc) && doc.with_text(str::is_empty) {
        trash_new_file(state, doc);
    } else {
        save_quietly(state, doc);
    }
    if touched {
        let toast = adw::Toast::builder()
            .title("Rejected — your edits in this hunk were discarded")
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

/// A file the agent created and the user rejected entirely: close its tab, drop it from the
/// workspace and move the file to the trash. Runs from an idle callback, since a decision may
/// come from inside the Vim context's key handling.
fn trash_new_file(state: &Rc<WindowState>, doc: &DocRef) {
    let (state, doc) = (Rc::downgrade(state), doc.clone());
    glib::idle_add_local_once(move || {
        let Some(state) = state.upgrade() else {
            return;
        };
        drop_review(&doc);
        match state.editor_for(&doc) {
            Some(editor) => state.close_editor(&editor, true),
            None => state.workspace.release(&doc),
        }
        let path = doc.path().to_path_buf();
        glib::spawn_future_local(async move {
            let file = gio::File::for_path(&path);
            if let Err(e) = file.trash_future(glib::Priority::DEFAULT).await {
                state.toast(&format!(
                    "Could not move {} to the trash: {}",
                    path.display(),
                    e.message()
                ));
            }
        });
    });
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
            recompute(doc);
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
            finish_reject(state, doc, touched);
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
/// the cursor to the first hunk that came back. Only the most recent decision is ever reverted:
/// when it cannot be (its file left the review, or its text was edited since), a toast says so
/// and older decisions stay as they are.
pub fn undo_last(state: &Rc<WindowState>) {
    let Some(inner) = controller(state) else {
        return;
    };
    let Some(paths) = inner.decisions.borrow_mut().pop() else {
        state.toast("Nothing to undo");
        return;
    };
    let mut first_restored = None;
    let mut gone = Vec::new();
    let mut stuck = Vec::new();
    for path in &paths {
        let doc = state
            .workspace
            .documents()
            .into_iter()
            .find(|d| d.path() == path);
        let Some(doc) = doc.filter(|d| d.review.borrow().is_some()) else {
            gone.push(relative_name(state, path));
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
    let message = match (gone.as_slice(), stuck.as_slice()) {
        ([], []) => return,
        ([name], []) if first_restored.is_none() => {
            format!("Cannot undo the last decision: {name} is no longer under review")
        }
        ([], [name]) if first_restored.is_none() => {
            format!("Cannot undo the last decision: {name} was edited since")
        }
        _ if first_restored.is_none() => "Cannot undo the last decision".to_owned(),
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
    let action = {
        let text = doc.text();
        let mut review = doc.review.borrow_mut();
        review.as_mut()?.undo_last_review_action(&text)
    };
    let restored_text = match action? {
        UndoAction::Unaccepted => false,
        UndoAction::Edits(edits) => {
            if let Err(e) = doc.apply_edits(&edits, Origin::Review) {
                state.toast(&format!("Could not restore the change: {e}"));
            }
            true
        }
    };
    let delta = recompute(doc);
    if restored_text {
        save_quietly(state, doc);
    }
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

/// The path of `doc` relative to the project root, for display.
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
