//! Agent-edit review model: hunks = diff(baseline, current). Pure; the GUI mirrors every buffer change into it.
//!
//! The buffer (and the file on disk) always holds the agent's proposal. A [`FileReview`] keeps the
//! *baseline* (the text before the agent's first edit, plus everything the user accepted or typed
//! outside hunks) and describes the difference to the current text as [`Hunk`]s:
//!
//! - every buffer change is reported to [`FileReview::on_edit`] **before** it is applied, together
//!   with its [`Origin`](lemma_core::Origin); user typing outside hunks is folded into the baseline
//!   so it never becomes a hunk, typing inside a hunk becomes part of the proposal;
//! - [`FileReview::recompute`] re-diffs baseline and current text (a deterministic Myers line
//!   diff, plus intra-line word spans refined for at most 8 ms) and keeps [`HunkId`]s stable;
//! - [`FileReview::accept`] moves a hunk into the baseline, [`FileReview::reject`] returns the
//!   buffer edit that restores it; both are journalled for [`FileReview::undo_review_action`], and
//!   buffer undo (`Origin::History`) of a reject re-opens the hunk with the same id;
//! - every agent edit is attributed to the current agent turn ([`FileReview::set_turn`]), so each
//!   hunk knows the turns that made it ([`Hunk::turns`]) and a whole turn can be kept or rejected
//!   ([`FileReview::accept_turn`], [`FileReview::reject_turn`]).
//!
//! Lines are 0-based and every text position is a UTF-8 byte offset (see `lemma_core`). A line ends
//! after its `'\n'`; the last line of a text may lack one, and `'\r'` is ordinary content.

mod diff;
mod edits;
mod review;
mod text;

use lemma_core::TextEdit;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

pub use edits::{EditError, minimal_edits, str_replace, unified_diff};
pub use review::FileReview;

/// fnv1a64(old_text, new_text, occurrence index among hunks with identical (old,new)). Stable across
/// recomputes and across accept/reject of other hunks.
///
/// Concretely `fnv1a64(&[old_text, new_text, &occurrence.to_le_bytes()])` with a `u64` occurrence.
/// Occurrences are numbered in text order when hunks first appear; a hunk keeps its number while it
/// exists, so accepting or rejecting another hunk with the same texts never renumbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HunkId(pub u64);

/// Which text a [`WordSpan`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The baseline (`Hunk::old_text`, drawn red).
    Old,
    /// The current text (the proposal, drawn green).
    New,
}

/// Intra-line change; `line` is relative to the hunk's first line on that side; bytes within that line.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WordSpan {
    pub side: Side,
    pub line: u32,
    pub bytes: Range<u32>,
}

/// One changed region. Line and byte ranges are half-open; after [`FileReview::recompute`] they are
/// line-aligned (`cur_bytes` covers exactly `cur_lines`, including their `'\n'`s).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub id: HunkId,
    /// Lines in the baseline that were replaced (red). Empty for a pure insertion.
    pub base_lines: Range<u32>,
    /// Lines in current that are new (green). Empty for a pure deletion (then `cur_lines.start` is the line after).
    pub cur_lines: Range<u32>,
    /// Byte range of the proposal in the current text (empty for a pure deletion).
    pub cur_bytes: Range<usize>,
    /// Byte range of `old_text` in the baseline (empty for a pure insertion).
    pub base_bytes: Range<usize>,
    /// Baseline text of `base_lines` including trailing '\n' (drawn as phantom red lines).
    pub old_text: String,
    /// Changed words, for replacements whose lines are similar enough to pair up. A line whose
    /// changed words cover most of its non-blank characters has none (the whole line changed).
    pub words: Vec<WordSpan>,
    /// The user edited inside this hunk (amber in the gutter; Accept keeps the user's text).
    pub user_touched: bool,
    /// The agent turns whose edits made this hunk, ascending (see [`FileReview::set_turn`]):
    /// those whose recorded ranges meet its current range, a pure insertion or deletion counting
    /// where it touches. One turn for a hunk that turn alone made; several when turns edited the
    /// same lines (a *mixed* hunk, which [`FileReview::accept_turn`] and
    /// [`FileReview::reject_turn`] never touch); none when no agent edit is known for it (a hunk
    /// made by `Origin::Reload` changes, or restored from a file saved without attribution). A
    /// hunk of one turn may still hold another change the line diff paired with that turn's (a
    /// reloaded line next to it, another turn's deletion along a run of blank lines): rejecting
    /// the turn then takes back only the turn's own edits in it, and keeping the turn leaves it
    /// alone.
    pub turns: Vec<u32>,
}

/// How [`FileReview::on_edit`] placed an edit relative to the hunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditClass {
    /// In unchanged text (between hunks).
    Outside,
    /// Overlapping the given hunk (the first one, if the edit spans several).
    Inside(HunkId),
}

/// Feedback for the agent, drained with [`FileReview::drain_events`]. `lines` are 0-based lines of
/// the current text at the time of the event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEvent {
    /// The user kept a hunk.
    Accepted { lines: Range<u32> },
    /// The user rejected a hunk: `proposed` (the agent's text) was replaced by `kept` (the
    /// baseline).
    Rejected {
        lines: Range<u32>,
        proposed: String,
        kept: String,
    },
    /// The user typed inside a hunk (sent once per hunk per turn, see [`FileReview::begin_turn`]).
    UserEditedInHunk { lines: Range<u32> },
    /// The user undid (Ctrl+Z) one of the agent's edits (once per hunk per turn).
    UndoneByUser { lines: Range<u32> },
}

/// Why an accept or reject failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewError {
    /// No hunk has this id (it was decided, edited or merged since the caller looked).
    NoSuchHunk(HunkId),
    /// `current` is not the text this review was told about: a buffer change bypassed
    /// [`FileReview::on_edit`]. Call [`FileReview::recompute`] and try again.
    Stale,
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchHunk(id) => write!(f, "no hunk with id {:016x}", id.0),
            Self::Stale => f.write_str("review is out of sync with the buffer"),
        }
    }
}

impl std::error::Error for ReviewError {}

/// What `undo_review_action` asks the caller to do.
#[derive(Debug, PartialEq, Eq)]
pub enum UndoAction {
    /** baseline restored, no buffer change */
    Unaccepted,
    /** apply as Origin::Review */
    Edits(Vec<TextEdit>),
}

/// How [`FileReview::recompute`] changed the hunk list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HunkDelta {
    /// Ids that did not exist before, in text order.
    pub added: Vec<HunkId>,
    /// Ids that no longer exist, in their previous text order.
    pub removed: Vec<HunkId>,
    /// Ids that still exist but whose hunk differs (position, words or `user_touched`).
    pub changed: Vec<HunkId>,
}

impl HunkDelta {
    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A review saved across editor restarts (`$XDG_STATE_HOME/lemma/review/<fnv(root)>.json`).
///
/// The fields after `created_unix` were added in M2. They are optional in the serialized form
/// (missing ones read as empty, empty ones are not written), so files written before them still
/// load: such a review comes back with fresh hunk ids and no turns. A review saved with turns but
/// without `turn_edits` comes back with turns that cannot be kept or rejected as a whole (their
/// hunks are decided one by one).
///
/// `path` is written as a string, or as the array of its bytes when it is not valid UTF-8 (both
/// forms load).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedReview {
    #[serde(with = "path_serde")]
    pub path: PathBuf,
    pub baseline: String,
    /// `fnv1a64(&[current.as_bytes()])` of the text on disk when the review was saved.
    pub current_hash: u64,
    /// When the review was created (seconds since the Unix epoch).
    pub created_unix: u64,
    /// Which agent turn wrote which part of the current text (see [`Hunk::turns`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution: Vec<TurnRange>,
    /// What the hunks keep beyond their texts: id occurrence and `user_touched`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<PersistedHunk>,
    /// What each turn's edits replaced, where that is known exactly (see
    /// [`FileReview::reject_turn`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_edits: Vec<TurnEdit>,
    /// Places where a turn changed text that is not known exactly any more (closed intervals, as
    /// in `attribution`): their hunks are not decided per turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_turn_edits: Vec<TurnRange>,
}

/// What an agent turn's edits replaced in one place: putting `before` back in place of the current
/// bytes `bytes` undoes that turn's change there, and only that.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnEdit {
    pub turn: u32,
    pub bytes: Range<usize>,
    pub before: String,
}

/// Serde for [`PersistedReview::path`]: a string, or the array of the path's bytes when it is not
/// valid UTF-8 (a Latin-1 directory name), so that such a path still round-trips.
mod path_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => s.serialize_str(text),
            #[cfg(unix)]
            None => {
                use std::os::unix::ffi::OsStrExt as _;
                s.collect_seq(path.as_os_str().as_bytes())
            }
            #[cfg(not(unix))]
            None => s.serialize_str(&path.to_string_lossy()),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Bytes(Vec<u8>),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Text(text) => PathBuf::from(text),
            #[cfg(unix)]
            Repr::Bytes(bytes) => {
                use std::os::unix::ffi::OsStringExt as _;
                PathBuf::from(std::ffi::OsString::from_vec(bytes))
            }
            #[cfg(not(unix))]
            Repr::Bytes(bytes) => PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()),
        })
    }
}

/// Part of the current text that an agent turn wrote: the closed interval
/// `[bytes.start, bytes.end]` of byte positions (a point where the turn deleted text when the two
/// are equal).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnRange {
    pub turn: u32,
    pub bytes: Range<usize>,
}

/// The state of one hunk that its texts do not determine, saved in [`PersistedReview::hunks`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PersistedHunk {
    /// The hunk's `cur_bytes` when it was saved.
    pub cur_bytes: Range<usize>,
    /// The occurrence number of its [`HunkId`] (so it keeps its id).
    pub occurrence: u64,
    pub user_touched: bool,
}
