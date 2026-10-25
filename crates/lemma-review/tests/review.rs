//! Scenario tests of `FileReview` through its public API.

mod common;

use common::Session;
use lemma_core::{Origin, TextEdit, fnv1a64};
use lemma_review::{
    EditClass, FileReview, HunkId, ReviewError, ReviewEvent, Side, UndoAction, WordSpan,
    str_replace,
};

const DOC: &str = "\\section{Intro}\nLet $x$ be a point.\nThen $f(x)=0$.\n\\end{document}\n";

fn hunk_id(old: &str, new: &str, occurrence: u64) -> HunkId {
    HunkId(fnv1a64(&[
        old.as_bytes(),
        new.as_bytes(),
        &occurrence.to_le_bytes(),
    ]))
}

// ---- hunk shapes -------------------------------------------------------------------------------

#[test]
fn pure_insertion_in_the_middle() {
    let mut s = Session::new(DOC);
    s.agent_replace("Then", "Hence\nThen");
    s.check();
    let [h] = s.hunks() else {
        panic!("{:?}", s.hunks())
    };
    assert_eq!(h.base_lines, 2..2);
    assert_eq!(h.cur_lines, 2..3);
    assert_eq!(h.old_text, "");
    assert_eq!(&s.text[h.cur_bytes.clone()], "Hence\n");
    assert_eq!(h.base_bytes, 36..36);
    assert_eq!(h.id, hunk_id("", "Hence\n", 0));
    assert!(h.words.is_empty());
}

#[test]
fn pure_insertion_at_start_and_eof() {
    let mut s = Session::new("a\nb\n");
    s.agent_write("top\na\nb\nend\n");
    s.check();
    let hs = s.hunks();
    assert_eq!(hs.len(), 2);
    assert_eq!(
        (hs[0].base_lines.clone(), hs[0].cur_lines.clone()),
        (0..0, 0..1)
    );
    assert_eq!(
        (hs[1].base_lines.clone(), hs[1].cur_lines.clone()),
        (2..2, 3..4)
    );
    assert_eq!(hs[1].cur_bytes, 8..12);
}

#[test]
fn pure_deletion_at_start_middle_and_eof() {
    for (old, line) in [("a\n", 0), ("b\n", 1), ("c\n", 2)] {
        let mut s = Session::new("a\nb\nc\n");
        s.agent_replace(old, "");
        s.check();
        let [h] = s.hunks() else {
            panic!("{:?}", s.hunks())
        };
        assert_eq!(h.base_lines, line..line + 1);
        assert_eq!(h.cur_lines, line..line, "anchored at the line after");
        assert!(h.cur_bytes.is_empty());
        assert_eq!(h.old_text, old);
        assert_eq!(s.review.hunk_at_line(line).map(|h| h.id), Some(h.id));
        assert_eq!(s.rejected_text(), "a\nb\nc\n");
    }
}

#[test]
fn file_without_trailing_newline() {
    // Changing the last line.
    let mut s = Session::new("a\nb");
    s.agent_replace("b", "c");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!(
        (h.old_text.as_str(), &s.text[h.cur_bytes.clone()]),
        ("b", "c")
    );
    assert_eq!((h.base_lines.clone(), h.cur_lines.clone()), (1..2, 1..2));

    // Appending a line after a last line without newline changes that line too.
    let mut s = Session::new("a\nb");
    s.agent_replace("b", "b\nc");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!(
        (h.old_text.as_str(), &s.text[h.cur_bytes.clone()]),
        ("b", "b\nc")
    );
    assert_eq!(h.cur_lines, 1..3);

    // Deleting the last line (and the newline before it).
    let mut s = Session::new("a\nb");
    s.agent_replace("\nb", "");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!(
        (h.old_text.as_str(), &s.text[h.cur_bytes.clone()]),
        ("a\nb", "a")
    );
    assert_eq!(s.rejected_text(), "a\nb");

    // Adding the final newline.
    let mut s = Session::new("a\nb");
    s.agent_write("a\nb\n");
    let [h] = s.hunks() else { panic!() };
    assert_eq!(
        (h.old_text.as_str(), &s.text[h.cur_bytes.clone()]),
        ("b", "b\n")
    );
}

#[test]
fn deletion_at_eof_without_trailing_newline_left() {
    let mut s = Session::new("a\nb\nc");
    s.agent_replace("c", "");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!(h.old_text, "c");
    assert_eq!(h.cur_lines, 2..2);
    assert_eq!(h.cur_bytes, 4..4);
    assert_eq!(s.review.hunk_at_line(2).map(|h| h.id), Some(h.id));
}

#[test]
fn whole_file_replace() {
    let mut s = Session::new("one\ntwo\n");
    s.agent_write("uno\ndos\ntres\n");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!((h.base_lines.clone(), h.cur_lines.clone()), (0..2, 0..3));
    assert_eq!(h.base_bytes, 0..8);
    assert_eq!(h.cur_bytes, 0..s.text.len());
    s.accept(h.id);
    assert!(s.review.is_clean());
    assert_eq!(s.review.baseline(), "uno\ndos\ntres\n");
}

#[test]
fn empty_baseline_new_file() {
    let mut s = Session::new("");
    s.edit(
        &TextEdit::insert(0, "\\documentclass{article}\n"),
        Origin::Agent { call: 7 },
    );
    s.recompute();
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!((h.base_lines.clone(), h.cur_lines.clone()), (0..0, 0..1));
    assert_eq!(h.base_bytes, 0..0);
    let edits = s.review.reject_all(&s.text);
    assert_eq!(edits, [TextEdit::delete(0..24)]);
}

#[test]
fn adjacent_changes_are_separate_hunks_when_a_line_separates_them() {
    let mut s = Session::new("1\n2\n3\n4\n5\n");
    s.agent_write("1\nB\n3\nD\n5\n");
    s.check();
    assert_eq!(s.hunks().len(), 2);
    s.agent_write("1\nB\nC\nD\n5\n");
    s.check();
    let [h] = s.hunks() else {
        panic!("{:?}", s.hunks())
    };
    assert_eq!(h.base_lines, 1..4);
}

#[test]
fn crlf_is_content() {
    let mut s = Session::new("a\r\nb\r\nc\r\n");
    s.agent_replace("b", "B");
    s.check();
    let [h] = s.hunks() else { panic!() };
    assert_eq!(h.old_text, "b\r\n");
    assert_eq!(h.cur_lines, 1..2);
    assert_eq!(h.cur_bytes, 3..6);
    // A lone '\r' does not end a line.
    let mut s = Session::new("x\ry\nz\n");
    s.agent_replace("y", "Y");
    let [h] = s.hunks() else { panic!() };
    assert_eq!((h.old_text.as_str(), h.cur_lines.clone()), ("x\ry\n", 0..1));
}

#[test]
fn one_word_change_has_word_spans_on_both_sides() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let [h] = s.hunks() else { panic!() };
    assert_eq!(h.cur_lines, 1..2);
    let new: Vec<&WordSpan> = h.words.iter().filter(|w| w.side == Side::New).collect();
    assert!(!new.is_empty(), "{:?}", h.words);
    let new_line = &s.text[h.cur_bytes.clone()];
    let spans: Vec<&str> = new
        .iter()
        .map(|w| &new_line[w.bytes.start as usize..w.bytes.end as usize])
        .collect();
    assert!(spans.concat().contains("closed"), "{spans:?}");

    let mut s = Session::new("\\section{Intro}\nThen $f(x)=0$ for every point of the curve.\n");
    s.agent_replace("$f(x)=0$", "$g(x)=0$");
    let [h] = s.hunks() else { panic!() };
    let line = |side| h.words.iter().filter(|w| w.side == side).count();
    assert_eq!((line(Side::Old), line(Side::New)), (1, 1), "{:?}", h.words);
    let w = h.words.iter().find(|w| w.side == Side::Old).unwrap();
    assert_eq!(w.line, 0);
    assert_eq!(
        &h.old_text[w.bytes.start as usize..w.bytes.end as usize],
        "$f(x)=0$"
    );
}

#[test]
fn a_mostly_rewritten_line_has_no_word_spans() {
    // Word spans covering most of a line are noise: the whole line is the change.
    let mut s = Session::new("a\nWe prove the main theorem by induction on n.\nz\n");
    s.agent_replace(
        "We prove the main theorem by induction on n.",
        "The proof proceeds by a careful induction argument.",
    );
    let [h] = s.hunks() else { panic!() };
    assert_eq!(h.words, [], "{:?}", h.words);
    // One changed token of a short line still covers most of it.
    let mut s = Session::new(DOC);
    s.agent_replace("$f(x)=0$", "$g(x)=0$");
    let [h] = s.hunks() else { panic!() };
    assert_eq!(h.words, [], "{:?}", h.words);
    // A few changed words in a longer line keep their spans, on both sides.
    let mut s = Session::new(DOC);
    s.agent_replace("be a point", "be a closed point");
    let [h] = s.hunks() else { panic!() };
    assert!(h.words.iter().any(|w| w.side == Side::New), "{:?}", h.words);
}

// ---- ids ---------------------------------------------------------------------------------------

#[test]
fn ids_follow_the_contract_and_number_equal_pairs() {
    let mut s = Session::new("x\na\ny\na\nz\n");
    let edits = lemma_review::str_replace(&s.text, "a", "b", true).unwrap();
    s.edits(&edits, Origin::Agent { call: 1 });
    s.recompute();
    assert_eq!(
        s.ids(),
        [hunk_id("a\n", "b\n", 0), hunk_id("a\n", "b\n", 1)]
    );
    // Accepting the first keeps the second's id.
    let second = s.ids()[1];
    s.accept(s.ids()[0]);
    assert_eq!(s.ids(), [second]);
    // A new equal pair gets a fresh number, not the accepted one's... any unused one.
    s.agent_replace("x\n", "x\na\n");
    s.agent_replace("x\na\n", "x\nb\n");
    let ids = s.ids();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[1], second);
    assert_ne!(ids[0], second);
}

#[test]
fn recompute_reports_added_removed_changed() {
    let mut s = Session::new("1\n2\n3\n4\n");
    s.agent_replace("2", "two");
    let first = s.ids()[0];
    s.edit(&TextEdit::insert(8, "4b\n"), Origin::Agent { call: 2 });
    let delta = s.recompute();
    assert_eq!(delta.removed, Vec::<HunkId>::new());
    assert_eq!(delta.added.len(), 1);
    // Typing above both shifts them: the shift is already applied by on_edit.
    s.user(TextEdit::insert(0, "0\n"));
    let delta = s.recompute();
    assert!(delta.is_empty(), "{delta:?}");
    assert_eq!(s.ids()[0], first);
    s.accept(first);
    assert_eq!(s.ids().len(), 1);
}

// ---- user and history edits --------------------------------------------------------------------

#[test]
fn user_typing_outside_hunks_goes_to_the_baseline() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let id = s.ids()[0];
    // Above the hunk, on the line after it, and at the end of the file.
    assert_eq!(
        s.review.on_edit(&TextEdit::insert(1, "S"), Origin::User),
        EditClass::Outside
    );
    s.text.insert(1, 'S');
    let after = s.hunks()[0].cur_bytes.end;
    s.user(TextEdit::insert(after, "So "));
    let end = s.text.len();
    s.user(TextEdit::insert(end, "% done\n"));
    s.recompute();
    s.check();
    assert_eq!(s.ids(), [id]);
    assert!(s.review.baseline().starts_with("\\Ssection"));
    assert!(s.review.baseline().contains("So Then"));
    assert!(s.review.baseline().ends_with("% done\n"));
    assert!(s.review.drain_events().is_empty());
}

#[test]
fn user_typing_inside_a_hunk_becomes_part_of_the_proposal() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let h = s.hunks()[0].clone();
    let at = h.cur_bytes.start + 4;
    assert_eq!(
        s.review.on_edit(&TextEdit::insert(at, "Z"), Origin::User),
        EditClass::Inside(h.id)
    );
    s.text.insert(at, 'Z');
    s.user(TextEdit::insert(at + 1, "Z"));
    assert_eq!(
        s.review.drain_events(),
        [ReviewEvent::UserEditedInHunk { lines: 1..2 }],
        "one event per hunk per turn"
    );
    s.recompute();
    s.check();
    let [h2] = s.hunks() else { panic!() };
    assert!(h2.user_touched);
    assert_ne!(h2.id, h.id, "the texts changed, so the id did");
    // Still touched after more recomputes; a new turn reports again.
    s.recompute();
    assert!(s.hunks()[0].user_touched);
    s.review.begin_turn();
    s.user(TextEdit::insert(at, "W"));
    assert_eq!(s.review.drain_events().len(), 1);
    // Accept keeps the user's version.
    s.accept(s.ids()[0]);
    assert!(s.review.baseline().contains("Let WZZ$x$"));
}

#[test]
fn deleting_a_selection_across_a_hunk_is_inside() {
    let mut s = Session::new("a\nb\nc\nd\n");
    s.agent_replace("c\n", "C\n");
    // Select from the middle of "b" to the middle of "d" and delete.
    let e = TextEdit::delete(3..7);
    assert!(matches!(
        s.review.on_edit(&e, Origin::User),
        EditClass::Inside(_)
    ));
    e.apply(&mut s.text);
    s.recompute();
    s.check();
    assert_eq!(s.text, "a\nb\n");
    assert_eq!(s.review.baseline(), "a\nb\nc\nd\n");
    assert!(s.hunks()[0].user_touched);
}

#[test]
fn inside_formula_boundaries() {
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b\n", "B\n");
    let [h] = s.hunks() else { panic!() };
    let (id, start, end) = (h.id, h.cur_bytes.start, h.cur_bytes.end);
    let class = |e: TextEdit| s.review.clone().on_edit(&e, Origin::User);
    assert_eq!(class(TextEdit::insert(start, "x")), EditClass::Inside(id));
    assert_eq!(class(TextEdit::insert(end, "x")), EditClass::Outside);
    assert_eq!(
        class(TextEdit::delete(start - 1..start)),
        EditClass::Outside
    );
    assert_eq!(
        class(TextEdit::delete(start - 1..start + 1)),
        EditClass::Inside(id)
    );
    assert_eq!(class(TextEdit::delete(end..end + 1)), EditClass::Outside);
    // A pure deletion: only a deletion around its point is inside.
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b\n", "");
    let [h] = s.hunks() else { panic!() };
    let (id, p) = (h.id, h.cur_bytes.start);
    let class = |e: TextEdit| s.review.clone().on_edit(&e, Origin::User);
    assert_eq!(class(TextEdit::insert(p, "x")), EditClass::Outside);
    assert_eq!(class(TextEdit::delete(p - 1..p + 1)), EditClass::Inside(id));
    assert_eq!(class(TextEdit::delete(p..p + 1)), EditClass::Outside);
}

#[test]
fn typing_at_a_pure_deletion_point_goes_to_the_baseline() {
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b\n", "");
    let p = s.hunks()[0].cur_bytes.start;
    s.user(TextEdit::insert(p, "x"));
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), "a\nb\nxc\n");
    assert_eq!(s.pairs(), [("b\n".to_owned(), String::new())]);
}

#[test]
fn ctrl_z_of_an_agent_edit_removes_its_hunk() {
    for (old, new) in [("b\n", ""), ("b", "B"), ("b\n", "X\nY\n"), ("", "")] {
        let mut s = Session::new("a\nb\nc\n");
        let edits = if old.is_empty() {
            // A pure insertion.
            let e = vec![TextEdit::insert(2, "new\n")];
            s.edits(&e, Origin::Agent { call: 1 });
            s.recompute();
            e
        } else {
            s.agent_replace(old, new)
        };
        let replaced: Vec<String> = edits
            .iter()
            .map(|e| "a\nb\nc\n"[e.range.clone()].to_owned())
            .collect();
        assert_eq!(s.hunks().len(), 1);
        s.undo_action(&edits, &replaced);
        s.recompute();
        s.check();
        assert_eq!(s.text, "a\nb\nc\n");
        assert!(s.review.is_clean(), "{old:?}: {:?}", s.hunks());
        assert_eq!(s.review.baseline(), "a\nb\nc\n");
        let events = s.review.drain_events();
        assert!(
            matches!(events[..], [ReviewEvent::UndoneByUser { .. }]),
            "{old:?}: {events:?}"
        );
    }
}

#[test]
fn ctrl_z_of_an_agent_deletion_next_to_an_equal_line() {
    // The diff may anchor the deletion at either blank line; undo must still restore cleanly.
    let mut s = Session::new("a\n\n\nb\n");
    let edits = s.agent_replace("\n\nb", "\nb");
    s.undo_action(&edits, &["\n".to_owned()]);
    s.recompute();
    assert_eq!(s.text, "a\n\n\nb\n");
    assert!(s.review.is_clean(), "{:?}", s.hunks());
}

#[test]
fn ctrl_z_of_user_typing_outside_goes_to_the_baseline_too() {
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b", "B");
    let end = s.hunks()[0].cur_bytes.end;
    s.user(TextEdit::insert(end, "typed"));
    s.edit(&TextEdit::delete(end..end + 5), Origin::History);
    s.recompute();
    s.check();
    assert_eq!(s.review.baseline(), "a\nb\nc\n");
    assert_eq!(s.pairs(), [("b\n".to_owned(), "B\n".to_owned())]);
}

#[test]
fn redo_of_an_agent_edit_brings_the_hunk_back() {
    let mut s = Session::new("a\nb\nc\n");
    let edits = s.agent_replace("b\n", "");
    let id = s.ids()[0];
    s.undo_action(&edits, &["b\n".to_owned()]);
    s.recompute();
    assert!(s.review.is_clean());
    // Redo: GTK replays the deletion.
    s.edit(&edits[0], Origin::History);
    s.recompute();
    assert_eq!(s.ids(), [id]);
    assert_eq!(s.review.baseline(), "a\nb\nc\n");
}

/// Ctrl+Z then Ctrl+Shift+Z of one agent action with more changes than the records' cap of 512
/// (a replace_all of 600 matches): the undo is recognized change by change (the agent hears
/// `UndoneByUser`, not that the user edited its change), and the redo brings every hunk back,
/// with its id and turn. Only the records of the last 472 changes were kept: undoing the first
/// 128 looked like the user editing them, and redoing them put them into the baseline.
#[test]
fn undo_and_redo_of_an_agent_action_beyond_the_record_cap() {
    let base: String = (0..600)
        .map(|i| format!("Line {i} uses \\R here.\nPlain line {i}.\n"))
        .collect();
    let mut s = Session::new(&base);
    s.review.set_turn(1);
    let edits = str_replace(&s.text, "\\R", "\\mathbb{R}", true).unwrap();
    let replaced: Vec<String> = edits
        .iter()
        .map(|e| s.text[e.range.clone()].to_owned())
        .collect();
    s.edits(&edits, Origin::Agent { call: 1 });
    s.recompute();
    let (ids, proposal) = (s.ids(), s.text.clone());
    assert_eq!(ids.len(), 600);
    s.review.drain_events();

    s.undo_action(&edits, &replaced);
    s.recompute();
    assert_eq!(s.text, base);
    assert!(s.review.is_clean(), "{} hunks left", s.hunks().len());
    let events = s.review.drain_events();
    let undone = events
        .iter()
        .filter(|e| matches!(e, ReviewEvent::UndoneByUser { .. }))
        .count();
    assert_eq!(
        (undone, events.len()),
        (600, 600),
        "{:?}",
        events
            .iter()
            .find(|e| !matches!(e, ReviewEvent::UndoneByUser { .. }))
    );

    // Redo: GTK replays the action's changes in order.
    s.edits(&edits, Origin::History);
    s.recompute();
    assert_eq!(s.text, proposal);
    assert_eq!(s.review.baseline(), base, "changes went into the baseline");
    assert_eq!(s.ids(), ids);
    assert!(s.hunks().iter().all(|h| h.turns == [1]));
}

// ---- accept, reject, undo ----------------------------------------------------------------------

#[test]
fn accept_moves_the_hunk_into_the_baseline() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    s.agent_replace("\\end{document}", "\\end{document}\n% eof");
    assert_eq!(s.hunks().len(), 2);
    let [first, second] = [s.ids()[0], s.ids()[1]];
    s.accept(first);
    s.check();
    assert_eq!(s.ids(), [second]);
    assert!(s.review.baseline().contains("closed"));
    assert_eq!(
        s.review.drain_events(),
        [ReviewEvent::Accepted { lines: 1..2 }]
    );
    assert_eq!(
        s.review.accept(first, &s.text),
        Err(ReviewError::NoSuchHunk(first))
    );
}

#[test]
fn reject_restores_the_baseline_text() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let id = s.ids()[0];
    let (edit, replaced) = s.reject(id);
    // Trimmed to the inserted word: "Let $x$ be a |closed |point."
    assert_eq!(edit, TextEdit::delete(29..36));
    assert_eq!(replaced, "closed ");
    assert_eq!(s.text, DOC);
    assert!(s.review.is_clean());
    assert_eq!(
        s.review.drain_events(),
        [ReviewEvent::Rejected {
            lines: 1..2,
            proposed: "Let $x$ be a closed point.\n".into(),
            kept: "Let $x$ be a point.\n".into(),
        }]
    );
}

#[test]
fn reject_then_ctrl_z_reopens_the_same_hunk() {
    let mut s = Session::new(DOC);
    s.agent_replace("f(x)", "g(x)");
    s.agent_replace("Intro", "Introduction");
    assert_eq!(s.hunks().len(), 2);
    let ids = s.ids();
    let (edit, replaced) = s.reject(ids[1]);
    assert_eq!(s.ids(), [ids[0]]);
    s.undo_replace(&edit, &replaced);
    s.recompute();
    s.check();
    assert_eq!(s.ids(), ids);
    // Redo of the reject removes it again, without counting as a user edit.
    s.edit(&edit, Origin::History);
    s.recompute();
    assert_eq!(s.ids(), [ids[0]]);
    assert!(!s.hunks()[0].user_touched);
}

#[test]
fn reject_of_a_touched_hunk_then_ctrl_z_keeps_it_touched() {
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b", "B");
    s.user(TextEdit::insert(3, "!"));
    s.recompute();
    let id = s.ids()[0];
    assert!(s.hunks()[0].user_touched);
    let (edit, replaced) = s.reject(id);
    s.undo_replace(&edit, &replaced);
    s.recompute();
    assert_eq!(s.ids(), [id]);
    assert!(s.hunks()[0].user_touched);
}

#[test]
fn undo_review_action_after_accept() {
    let mut s = Session::new(DOC);
    s.agent_replace("be a point", "be a closed point");
    s.agent_replace("\\end{document}", "\\end{document}%");
    assert_eq!(s.hunks().len(), 2);
    let ids = s.ids();
    s.accept(ids[0]);
    assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
    s.check();
    assert_eq!(s.ids(), ids);
    assert_eq!(s.review.baseline(), DOC);
    assert!(!s.hunks()[0].words.is_empty());
    assert_eq!(s.undo_review(), None);
}

#[test]
fn undo_review_action_after_reject() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let id = s.ids()[0];
    let proposal = s.text.clone();
    s.reject(id);
    let Some(UndoAction::Edits(edits)) = s.undo_review() else {
        panic!()
    };
    assert_eq!(edits, [TextEdit::insert(29, "closed ")]);
    s.check();
    assert_eq!(s.text, proposal);
    assert_eq!(s.ids(), [id]);
    assert_eq!(s.undo_review(), None);
}

#[test]
fn undo_review_action_after_accept_all_and_reject_all() {
    let mut s = Session::new(DOC);
    s.agent_replace("f(x)", "g(x)");
    s.agent_replace("Intro", "Introduction");
    assert_eq!(s.hunks().len(), 2);
    let ids = s.ids();
    s.accept_all();
    assert!(s.review.is_clean());
    assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
    assert_eq!(s.ids(), ids);
    let proposal = s.text.clone();
    s.reject_all();
    assert_eq!(s.text, DOC);
    let Some(UndoAction::Edits(edits)) = s.undo_review() else {
        panic!()
    };
    assert_eq!(edits.len(), 2);
    assert!(edits[0].range.start > edits[1].range.start, "descending");
    assert_eq!(s.text, proposal);
    assert_eq!(s.ids(), ids);
}

#[test]
fn undo_review_action_skips_actions_edited_since() {
    let mut s = Session::new("a\nb\nbcde\n");
    s.agent_replace("a", "A");
    s.agent_replace("bcde", "b12e");
    let ids = s.ids();
    s.accept(ids[0]);
    let (edit, _) = s.reject(ids[1]);
    assert_eq!(
        edit,
        TextEdit {
            range: 5..7,
            text: "cd".into()
        }
    );
    // The user edits inside the restored "cd": the reject can no longer be reverted, the accept
    // still can.
    s.user(TextEdit::insert(6, "x"));
    s.recompute();
    assert_eq!(s.undo_review(), Some(UndoAction::Unaccepted));
    assert_eq!(s.ids(), [ids[0]]);
    assert_eq!(s.undo_review(), None);
}

#[test]
fn typing_next_to_restored_text_keeps_the_reject_revertible() {
    let mut s = Session::new("a\nbcde\n");
    s.agent_replace("bcde", "b12e");
    let id = s.ids()[0];
    s.reject(id);
    // Typed right before and right after "cd": the restored text itself is unchanged.
    s.user(TextEdit::insert(3, "<"));
    s.user(TextEdit::insert(6, ">"));
    s.recompute();
    let Some(UndoAction::Edits(edits)) = s.undo_review() else {
        panic!()
    };
    assert_eq!(
        edits,
        [TextEdit {
            range: 4..6,
            text: "12".into()
        }]
    );
    assert_eq!(s.text, "a\nb<12>e\n");
    // The line's texts now include the typing, so the hunk has a new id.
    assert_ne!(s.ids(), [id]);
    assert_eq!(s.pairs(), [("b<cd>e\n".to_owned(), "b<12>e\n".to_owned())]);
}

#[test]
fn undo_review_journal_is_bounded() {
    let mut s = Session::new("x\n");
    for i in 0..105 {
        s.agent_replace("x\n", &format!("x\n{i}\n"));
        s.accept(s.ids()[0]);
    }
    let mut n = 0;
    while s.undo_review().is_some() {
        n += 1;
    }
    assert_eq!(n, 100);
}

#[test]
fn stale_text_is_refused() {
    let mut s = Session::new("a\nb\n");
    s.agent_replace("b", "B");
    let id = s.ids()[0];
    assert_eq!(s.review.accept(id, "a\nB\n!"), Err(ReviewError::Stale));
    assert_eq!(s.review.reject(id, "a\nC\n"), Err(ReviewError::Stale));
    assert_eq!(
        s.review.accept(HunkId(1), &s.text),
        Err(ReviewError::NoSuchHunk(HunkId(1)))
    );
}

#[test]
fn hunk_at_line() {
    let mut s = Session::new("0\n1\n2\n3\n4\n5\n");
    s.agent_write("0\nX\nY\n2\n3\n5\n");
    let ids = s.ids();
    assert_eq!(s.hunks().len(), 2);
    assert_eq!(s.review.hunk_at_line(0), None);
    assert_eq!(s.review.hunk_at_line(1).map(|h| h.id), Some(ids[0]));
    assert_eq!(s.review.hunk_at_line(2).map(|h| h.id), Some(ids[0]));
    assert_eq!(s.review.hunk_at_line(3), None);
    // "4" was deleted: anchored at line 5 of current ("5").
    assert_eq!(s.review.hunk_at_line(5).map(|h| h.id), Some(ids[1]));
    assert_eq!(s.review.hunk_at_line(6), None);
}

// ---- persistence -------------------------------------------------------------------------------

#[test]
fn persistence_round_trip() {
    let mut s = Session::new(DOC);
    s.agent_replace("a point", "a closed point");
    let p = s.review.to_persisted("main.tex".into(), &s.text);
    let json = serde_json::to_string(&p).unwrap();
    let back: lemma_review::PersistedReview = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
    let restored = FileReview::from_persisted(&back, &s.text).unwrap();
    assert_eq!(restored.hunks(), s.hunks());
    assert_eq!(restored.baseline(), s.review.baseline());
    assert!(FileReview::from_persisted(&back, DOC).is_none());
}

#[test]
fn delta_reports_hunks_made_by_on_edit_as_added() {
    let mut s = Session::new("a\nb\nc\n");
    s.edit(
        &TextEdit {
            range: 2..3,
            text: "B".into(),
        },
        Origin::Agent { call: 1 },
    );
    let provisional = s.ids();
    let delta = s.recompute();
    assert_eq!(delta.added, s.ids());
    assert!(delta.removed.is_empty() && delta.changed.is_empty());
    assert_eq!(provisional.len(), 1);
    // Accepting removes it from the list right away; the next recompute reports it removed.
    let id = s.ids()[0];
    s.review.accept(id, &s.text).unwrap();
    assert!(s.hunks().is_empty());
    assert_eq!(s.recompute().removed, [id]);
}

#[test]
fn is_clean_before_recompute() {
    let mut s = Session::new("a\nb\n");
    s.edit(&TextEdit::insert(2, "x"), Origin::Agent { call: 1 });
    assert!(!s.review.is_clean());
    s.edit(&TextEdit::delete(2..3), Origin::Agent { call: 1 });
    assert!(s.review.is_clean(), "the agent took its edit back");
    s.recompute();
    assert!(s.hunks().is_empty());
}

#[test]
fn decisions_on_stale_text() {
    // A change that bypassed on_edit: accept_all still accepts what the buffer shows, and
    // reject_all still restores the baseline.
    let mut s = Session::new("a\nb\nc\n");
    s.agent_replace("b", "B");
    let stale = format!("{}!", s.text);
    let mut copy = s.review.clone();
    let mut rejected = stale.clone();
    lemma_core::apply_all(&mut rejected, &copy.reject_all(&stale));
    assert_eq!(rejected, "a\nb\nc\n");
    assert_eq!(copy.drain_events().len(), 1);
    s.review.accept_all(&stale);
    assert_eq!(s.review.baseline(), stale);
    assert!(s.review.hunks().is_empty());
    assert_eq!(s.review.undo_review_action(&stale), None);
}
