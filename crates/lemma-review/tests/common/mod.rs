//! A buffer stand-in that mirrors every change into a `FileReview`, the way the GUI's `Document`
//! does: `on_edit` before the change, with the change's origin.

#![allow(dead_code)]

use lemma_core::{Origin, TextEdit, apply_all};
use lemma_review::{FileReview, Hunk, HunkDelta, HunkId, UndoAction, str_replace};

pub struct Session {
    pub review: FileReview,
    pub text: String,
}

impl Session {
    /// A review created on the first agent edit: the baseline is the text before it.
    pub fn new(text: &str) -> Self {
        Self {
            review: FileReview::new(text.to_owned()),
            text: text.to_owned(),
        }
    }

    /// One buffer change.
    pub fn edit(&mut self, e: &TextEdit, origin: Origin) {
        self.review.on_edit(e, origin);
        e.apply(&mut self.text);
    }

    /// Several changes in descending order (one user action).
    pub fn edits(&mut self, edits: &[TextEdit], origin: Origin) {
        for e in edits {
            self.edit(e, origin);
        }
    }

    pub fn recompute(&mut self) -> HunkDelta {
        self.review.recompute(&self.text)
    }

    /// The agent's `edit` tool: str_replace against the live text, applied, recomputed.
    /// Returns the edits so tests can undo them.
    pub fn agent_replace(&mut self, old: &str, new: &str) -> Vec<TextEdit> {
        let edits = str_replace(&self.text, old, new, false).expect("agent edit applies");
        self.edits(&edits, Origin::Agent { call: 1 });
        self.recompute();
        edits
    }

    /// The agent's `write` tool on an existing file.
    pub fn agent_write(&mut self, content: &str) -> Vec<TextEdit> {
        let edits = lemma_review::minimal_edits(&self.text, content);
        self.edits(&edits, Origin::Agent { call: 2 });
        self.recompute();
        edits
    }

    /// Typing (no recompute: the GUI debounces it).
    pub fn user(&mut self, e: TextEdit) {
        self.edit(&e, Origin::User);
    }

    pub fn accept(&mut self, id: HunkId) {
        self.review.accept(id, &self.text).expect("accept");
        self.recompute();
    }

    /// Reject: the returned edit is applied as Origin::Review. Returns it with the text it
    /// replaced.
    pub fn reject(&mut self, id: HunkId) -> (TextEdit, String) {
        let e = self.review.reject(id, &self.text).expect("reject");
        let replaced = self.text[e.range.clone()].to_owned();
        self.edit(&e, Origin::Review);
        self.recompute();
        (e, replaced)
    }

    pub fn reject_all(&mut self) -> Vec<TextEdit> {
        let edits = self.review.reject_all(&self.text);
        self.edits(&edits, Origin::Review);
        self.recompute();
        edits
    }

    pub fn accept_all(&mut self) {
        self.review.accept_all(&self.text);
        self.recompute();
    }

    /// Alt+U.
    pub fn undo_review(&mut self) -> Option<UndoAction> {
        let action = self.review.undo_review_action(&self.text);
        if let Some(UndoAction::Edits(edits)) = &action {
            let edits = edits.clone();
            self.edits(&edits, Origin::Review);
        }
        self.recompute();
        action
    }

    /// GTK undo of a replace action `e` (that replaced `replaced`): delete what it inserted, then
    /// insert what it removed, both as History.
    pub fn undo_replace(&mut self, e: &TextEdit, replaced: &str) {
        let start = e.range.start;
        self.edit(
            &TextEdit::delete(start..start + e.text.len()),
            Origin::History,
        );
        self.edit(&TextEdit::insert(start, replaced), Origin::History);
    }

    /// GTK undo of a whole action of descending edits whose replaced texts are `replaced`
    /// (same order): the edits are undone in reverse order of application.
    pub fn undo_action(&mut self, edits: &[TextEdit], replaced: &[String]) {
        for (e, r) in edits.iter().zip(replaced).rev() {
            self.undo_replace(e, r);
        }
    }

    pub fn hunks(&self) -> &[Hunk] {
        self.review.hunks()
    }

    pub fn ids(&self) -> Vec<HunkId> {
        self.hunks().iter().map(|h| h.id).collect()
    }

    /// (old_text, new_text) of every hunk.
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.hunks()
            .iter()
            .map(|h| {
                (
                    h.old_text.clone(),
                    self.text[h.cur_bytes.clone()].to_owned(),
                )
            })
            .collect()
    }

    /// Checks the documented invariants of the public hunk list against the texts.
    pub fn check(&self) {
        check_hunks(self.review.baseline(), &self.text, self.hunks());
    }

    /// Applying `reject_all` to a copy gives the baseline back.
    pub fn rejected_text(&self) -> String {
        let mut copy = self.review.clone();
        let mut text = self.text.clone();
        apply_all(&mut text, &copy.reject_all(&self.text));
        text
    }
}

/// Hunks are sorted, line-aligned, their texts match both sides, and the text between them is
/// identical in baseline and current.
pub fn check_hunks(baseline: &str, current: &str, hunks: &[Hunk]) {
    let line_start = |t: &str, line: u32| -> usize {
        if line == 0 {
            return 0;
        }
        t.match_indices('\n')
            .nth(line as usize - 1)
            .map_or(t.len(), |(i, _)| i + 1)
    };
    let (mut cur, mut base) = (0, 0);
    for h in hunks {
        assert!(
            h.cur_bytes.start >= cur && h.base_bytes.start >= base,
            "{h:?}"
        );
        assert_eq!(
            current[cur..h.cur_bytes.start],
            baseline[base..h.base_bytes.start]
        );
        assert_eq!(baseline[h.base_bytes.clone()], h.old_text);
        assert_eq!(
            h.cur_bytes.start,
            line_start(current, h.cur_lines.start),
            "{h:?}"
        );
        assert_eq!(
            h.cur_bytes.end,
            line_start(current, h.cur_lines.end),
            "{h:?}"
        );
        assert_eq!(
            h.base_bytes.start,
            line_start(baseline, h.base_lines.start),
            "{h:?}"
        );
        assert_eq!(
            h.base_bytes.end,
            line_start(baseline, h.base_lines.end),
            "{h:?}"
        );
        assert!(
            !(h.cur_bytes.is_empty() && h.base_bytes.is_empty()),
            "{h:?}"
        );
        cur = h.cur_bytes.end;
        base = h.base_bytes.end;
    }
    assert_eq!(current[cur..], baseline[base..]);
}
