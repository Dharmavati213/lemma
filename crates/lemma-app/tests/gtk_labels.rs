//! WP3.2: the label tools (`labels.rs`) — undefined and unused names (following edits, saves
//! and files other programs change), find references, rename as reviewable hunks — and the
//! inline equation numbers (`editor/annotations.rs`), on the project fixture of
//! `tests/labels_common`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_labels`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `j-references`, `j-rename`, `j-unused` and
//! `j-problems-references` (each `-light.png` and `-dark.png`) there.

mod common;
mod labels_common;

use adw::prelude::*;
use labels_common::{
    AUX, BIB, CHAPTER, Fixture, MAIN, TIMEOUT, block_on, disk, find, fixture, offset_of, snapshots,
};
use lemma_app::editor::{Editor, annotations, completion, goto};
use lemma_app::labels::{self, Finding, NameKind};
use lemma_app::review_ui::{self, Decision, GroupKind};
use lemma_app::workspace::normalize;
use lemma_core::DiagSource;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    common::run(&[
        ("undefined_references_and_citations", undefined),
        (
            "undefined_follows_edits_saves_and_bibliographies",
            undefined_follows,
        ),
        (
            "undefined_shows_without_waiting_for_the_words",
            undefined_soon_after_typing,
        ),
        (
            "undefined_follows_files_changed_on_disk",
            undefined_follows_files_on_disk,
        ),
        ("problems_page_references_section_and_fix", problems_page),
        ("unused_labels_and_entries_on_demand", unused),
        ("find_references_lists_uses_across_the_project", references),
        ("find_references_of_a_citation_key", cite_references),
        (
            "rename_applies_reviewable_hunks_and_reject_restores",
            rename,
        ),
        ("rename_per_hunk_and_unsaved_documents", rename_per_hunk),
        ("rename_popover_checks_the_name", rename_popover),
        ("popovers_go_with_their_tab", popovers_close_with_tab),
        (
            "equation_numbers_from_aux_and_following_edits",
            equation_numbers,
        ),
    ])
}

fn names(findings: &[Finding]) -> Vec<(String, u32, NameKind)> {
    findings
        .iter()
        .map(|f| (f.name.clone(), f.line, f.kind))
        .collect()
}

/// Inserts `text` before the first `needle` of `editor`'s text.
fn insert_before(editor: &Editor, needle: &str, text: &str) {
    let buffer = editor.buffer();
    let mut at = editor.iter_near(offset_of(editor, needle, 0)).unwrap();
    buffer.insert(&mut at, text);
}

// ----- undefined and unused -----

fn undefined() {
    let f = fixture("labels-undefined");
    let editor = f.open("main.tex");
    f.wait_index();
    // The planted errors: an undefined reference and an unknown citation key; the commented
    // reference does not count.
    assert!(labels::cites_checked(&f.state));
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![
            ("missing".to_owned(), 10, NameKind::Cite),
            ("sec:undefined".to_owned(), 23, NameKind::Label),
        ]
    );
    // Underlined exactly, in the editor.
    let text = editor.document().text();
    let mut underlined = labels::underlined(&editor);
    underlined.sort_by_key(|r| r.start);
    let missing = find(&text, "missing}", 0);
    let undefined = find(&text, "sec:undefined", 0);
    assert_eq!(
        underlined,
        vec![
            missing..missing + "missing".len(),
            undefined..undefined + "sec:undefined".len()
        ]
    );
    // Problems with source Labels, 1-based lines.
    let problems = labels::published(&f.state);
    assert!(problems.iter().all(|p| p.source == DiagSource::Labels));
    assert!(problems.iter().all(|p| p.path == f.path("main.tex")));
    assert_eq!(
        problems
            .iter()
            .map(|p| (p.line, p.message.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Some(11), "Undefined citation ‘missing’"),
            (Some(24), "Undefined reference ‘sec:undefined’"),
        ]
    );
}

fn undefined_follows() {
    let f = fixture("labels-undefined-follows");
    let editor = f.open("main.tex");
    f.wait_index();
    // Defining the label: once the project index caught up, it is no longer undefined.
    insert_before(
        &editor,
        "\\end{document}",
        "\\section{Late}\\label{sec:undefined}\n",
    );
    f.wait_checks();
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![("missing".to_owned(), 10, NameKind::Cite)]
    );
    let text = editor.document().text();
    let missing = find(&text, "missing}", 0);
    assert_eq!(
        labels::underlined(&editor),
        vec![missing..missing + "missing".len()]
    );
    // A new reference to nothing is found too.
    insert_before(
        &editor,
        "\\end{document}",
        "See \\cref{sec:intro, sec:nowhere}.\n",
    );
    f.wait_checks();
    let found = names(&labels::undefined(&f.state));
    assert!(
        found
            .iter()
            .any(|(n, _, k)| n == "sec:nowhere" && *k == NameKind::Label),
        "{found:?}"
    );
    let text = editor.document().text();
    let nowhere = find(&text, "sec:nowhere", 0);
    assert!(labels::underlined(&editor).contains(&(nowhere..nowhere + "sec:nowhere".len())));
    // The bibliography gains the key and is saved in Lemma: the citation is defined.
    let bib = f.open("refs.bib");
    let bib_buffer = bib.buffer();
    let mut end = bib_buffer.end_iter();
    bib_buffer.insert(&mut end, "@misc{missing, title = {Found}}\n");
    block_on(bib.document().save()).unwrap();
    common::pump_until(TIMEOUT, || {
        !labels::check_pending(&f.state)
            && !labels::undefined(&f.state)
                .iter()
                .any(|f| f.name == "missing")
    });
    // A bibliography that cannot be read: citations are not checked at all.
    f.state.workspace.index.borrow_mut().bibs.clear();
    labels::check_now(&f.state);
    assert!(!labels::cites_checked(&f.state));
    assert!(
        labels::undefined(&f.state)
            .iter()
            .all(|f| f.kind == NameKind::Label)
    );
}

/// The checks wait for the project index only, not for the project's words (rebuilt 1.5 s after
/// the index changed): a reference to nothing is underlined about half a second after typing
/// stops, while the words still wait.
fn undefined_soon_after_typing() {
    let f = fixture("labels-undefined-soon");
    let editor = f.open("main.tex");
    f.wait_index();
    f.wait_checks();
    let before = labels::checks_done(&f.state);
    insert_before(&editor, "\\end{document}", "See \\ref{nope}.\n");
    let typed = Instant::now();
    common::pump_until(TIMEOUT, || {
        labels::undefined(&f.state)
            .iter()
            .any(|finding| finding.name == "nope")
    });
    let elapsed = typed.elapsed();
    let feed = completion::project_feed_state(&f.state);
    println!("    undefined after {elapsed:?} ({feed})");
    assert!(labels::checks_done(&f.state) > before);
    assert!(
        feed.contains("words timer true"),
        "the check waited for the words: {feed}"
    );
    let text = editor.document().text();
    let nope = find(&text, "nope}", 0);
    assert!(labels::underlined(&editor).contains(&(nope..nope + "nope".len())));
    if !cfg!(debug_assertions) {
        assert!(elapsed < Duration::from_millis(1200), "{elapsed:?}");
    }
}

/// The proposals after `before`, typed on a line of its own before `\end{document}` (then
/// removed again).
fn proposals_after(f: &Fixture, editor: &Editor, before: &str) -> Vec<String> {
    let line = format!("{before}\n");
    insert_before(editor, "\\end{document}", &line);
    let buffer = editor.buffer();
    let at = offset_of(editor, &line, before.len());
    buffer.place_cursor(&editor.iter_near(at).unwrap());
    // (The scanner's contexts follow the edit.)
    common::pump_for(Duration::from_millis(5));
    let labels = completion::items_at_cursor(&f.state, editor)
        .into_iter()
        .map(|item| item.label)
        .collect();
    let start = offset_of(editor, &line, 0);
    let mut a = editor.iter_near(start).unwrap();
    let mut b = editor.iter_near(start + line.len()).unwrap();
    buffer.delete(&mut a, &mut b);
    labels
}

/// Other programs change the files the project index read from disk, without a build or an
/// edit in Lemma: the chapter (not open) gains the label that was undefined, the bibliography
/// the missing key (as a reference manager exports it), a file an `\input` named before it
/// existed is written, and a deleted chapter takes its labels along. The checks, the Problems
/// page and completion follow.
fn undefined_follows_files_on_disk() {
    let f = fixture("labels-undefined-disk");
    let editor = f.open("main.tex");
    f.wait_index();
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![
            ("missing".to_owned(), 10, NameKind::Cite),
            ("sec:undefined".to_owned(), 23, NameKind::Label),
        ]
    );
    std::fs::write(
        f.path("chapters/ch1.tex"),
        format!("{CHAPTER}\\section{{Late}}\\label{{sec:undefined}}\n"),
    )
    .unwrap();
    std::fs::write(
        f.path("refs.bib"),
        format!("{BIB}@misc{{missing, title = {{Found}}, year = {{2026}}}}\n"),
    )
    .unwrap();
    common::pump_until(TIMEOUT, || labels::undefined(&f.state).is_empty());
    assert!(labels::cites_checked(&f.state));
    assert!(labels::published(&f.state).is_empty());
    assert!(proposals_after(&f, &editor, "\\cite{").contains(&"missing".to_owned()));
    assert!(proposals_after(&f, &editor, "\\ref{").contains(&"sec:undefined".to_owned()));
    // An `\input` of a file that does not exist yet, then the file.
    insert_before(
        &editor,
        "\\end{document}",
        "\\input{chapters/ch2}\nSee \\ref{sec:two}.\n",
    );
    common::pump_until(TIMEOUT, || {
        labels::undefined(&f.state)
            .iter()
            .any(|finding| finding.name == "sec:two")
    });
    std::fs::write(
        f.path("chapters/ch2.tex"),
        "\\section{Two}\\label{sec:two}\n",
    )
    .unwrap();
    common::pump_until(TIMEOUT, || labels::undefined(&f.state).is_empty());
    // The chapter deleted: what only it defined is undefined again (`sec:ch` stays defined by
    // the `.aux`).
    std::fs::remove_file(f.path("chapters/ch1.tex")).unwrap();
    common::pump_until(TIMEOUT, || {
        names(&labels::undefined(&f.state))
            == vec![("sec:undefined".to_owned(), 23, NameKind::Label)]
    });
    let chapter = f.path("chapters/ch1.tex");
    assert!(
        !f.state
            .workspace
            .index
            .borrow()
            .files
            .keys()
            .any(|p| normalize(p) == chapter)
    );
    // The bibliography, open in a tab, deleted by another program and saved again from its
    // tab: Lemma's own write, yet the index reads it again, as it could not before.
    let bib = block_on(f.state.open_file(&f.path("refs.bib"))).expect("open");
    std::fs::remove_file(f.path("refs.bib")).unwrap();
    common::pump_until(TIMEOUT, || !labels::cites_checked(&f.state));
    block_on(bib.document().save()).unwrap();
    common::pump_until(TIMEOUT, || labels::cites_checked(&f.state));
}

fn problems_page() {
    let f = fixture("labels-problems-page");
    let editor = f.open("main.tex");
    f.wait_index();
    let panel = f.state.build.get().unwrap().diagnostics();
    // The References section of the Problems page, with the names' columns.
    let findings = panel.findings(DiagSource::Labels);
    assert_eq!(
        findings
            .iter()
            .map(|f| (f.problem.line, f.problem.message.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Some(11), "Undefined citation ‘missing’"),
            (Some(24), "Undefined reference ‘sec:undefined’"),
        ]
    );
    assert_eq!(findings[1].columns, Some(5..18));
    assert!(panel.shows_filter_bar());
    // WP3.10: in the sidebar's width every filter keeps its whole label (equal widths cut
    // "References" to "Refe…" next to All, Build and Lint).
    f.state.sidebar.split.set_show_sidebar(true);
    f.state.sidebar.show_page(lemma_app::window::PAGE_PROBLEMS);
    let filter_labels = || {
        let mut labels = Vec::new();
        let mut stack = vec![f.state.sidebar.problems.clone().upcast::<gtk::Widget>()];
        let mut in_group = Vec::new();
        while let Some(w) = stack.pop() {
            let mut child = w.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
            if w.is::<adw::ToggleGroup>() {
                in_group.push(w);
            }
        }
        for group in in_group {
            let mut stack = vec![group];
            while let Some(w) = stack.pop() {
                let mut child = w.first_child();
                while let Some(c) = child {
                    child = c.next_sibling();
                    stack.push(c);
                }
                if let Ok(label) = w.downcast::<gtk::Label>() {
                    labels.push(label);
                }
            }
        }
        labels
    };
    common::pump_until(TIMEOUT, || {
        filter_labels()
            .iter()
            .any(|l| l.label() == "References" && l.is_mapped())
    });
    assert!(block_on(lemma_app::testmode::wait_for_frames(
        &f.state.window,
        1
    )));
    for label in filter_labels() {
        assert!(
            !label.layout().is_ellipsized(),
            "the filter {:?} is cut to {} px",
            label.label(),
            label.width()
        );
    }
    snapshots(&f, "j-problems-references", || {
        f.state.sidebar.show_page(lemma_app::window::PAGE_PROBLEMS);
    });
    // A typo: its row offers the label of the closest name, applied as a change to review.
    insert_before(&editor, "\\end{document}", "See \\eqref{eq:onee}.\n");
    f.wait_checks();
    let findings = panel.findings(DiagSource::Labels);
    let typo = findings
        .iter()
        .position(|f| f.problem.message.contains("eq:onee"))
        .expect("the typo is a problem");
    let actions = panel.finding_actions(DiagSource::Labels, typo);
    let labels: Vec<&str> = actions.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(labels, ["Use ‘eq:one’", "Unused Labels and Entries…"]);
    panel.run_finding_action(DiagSource::Labels, typo, 0);
    let main = editor.document().clone();
    common::pump_until(TIMEOUT, || main.text().contains("See \\eqref{eq:one}."));
    review_ui::recompute_if_stale(&main);
    let summary = review_ui::review_summary(&f.state);
    assert_eq!(summary.hunks, 1);
    assert_eq!(summary.groups[0].label, "Fix eq:onee → eq:one");
    f.wait_checks();
    assert!(
        !panel
            .findings(DiagSource::Labels)
            .iter()
            .any(|f| f.problem.message.contains("eq:onee"))
    );
    // A name with nothing close: only the list of unused names.
    let undefined = panel
        .findings(DiagSource::Labels)
        .iter()
        .position(|f| f.problem.message.contains("sec:undefined"))
        .unwrap();
    let actions = panel.finding_actions(DiagSource::Labels, undefined);
    assert_eq!(actions.len(), 1);
    panel.run_finding_action(DiagSource::Labels, undefined, 0);
    common::pump_until(TIMEOUT, || labels::unused_dialog(&f.state).is_some());
    labels::close_unused(&f.state);
    // Everything fixed: the section goes.
    insert_before(&editor, "\\end{document}", "\\label{sec:undefined}\n");
    let bib = f.open("refs.bib");
    let bib_buffer = bib.buffer();
    let mut end = bib_buffer.end_iter();
    bib_buffer.insert(&mut end, "@misc{missing, title = {Found}}\n");
    block_on(bib.document().save()).unwrap();
    common::pump_until(TIMEOUT, || {
        !labels::check_pending(&f.state) && panel.findings(DiagSource::Labels).is_empty()
    });
}

fn unused() {
    let f = fixture("labels-unused");
    let editor = f.open("main.tex");
    f.wait_index();
    let found = block_on(labels::unused(&f.state));
    assert_eq!(
        found
            .iter()
            .map(|f| (f.name.as_str(), f.kind))
            .collect::<Vec<_>>(),
        vec![("unused:one", NameKind::Label), ("spare", NameKind::Cite)]
    );
    assert_eq!(normalize(&found[0].path), f.path("chapters/ch1.tex"));
    assert_eq!(found[0].line, 2);
    assert_eq!(normalize(&found[1].path), f.path("refs.bib"));
    // Never underlined.
    let text = editor.document().text();
    assert!(
        labels::underlined(&editor)
            .iter()
            .all(|r| &text[r.clone()] != "spare")
    );
    // Listed on demand (`win.unused-labels`); choosing a row goes there.
    WidgetExt::activate_action(&f.state.window, "win.unused-labels", None).unwrap();
    common::pump_until(TIMEOUT, || labels::unused_dialog(&f.state).is_some());
    assert_eq!(labels::unused_rows(&f.state), found);
    snapshots(&f, "j-unused", || {
        labels::close_unused(&f.state);
        labels::show_unused(&f.state);
        common::pump_until(TIMEOUT, || labels::unused_dialog(&f.state).is_some());
    });
    assert!(labels::activate_unused(&f.state, 0));
    let chapter = f.path("chapters/ch1.tex");
    common::pump_until(TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| normalize(e.document().path()) == chapter)
    });
    common::pump_until(TIMEOUT, || labels::unused_dialog(&f.state).is_none());
    let active = f.state.active_editor().unwrap();
    assert_eq!(
        active.cursor_offset(),
        offset_of(&active, "\\label{unused:one}", 0)
    );
    // `\nocite{*}` uses every entry.
    f.state.select_editor(&editor);
    insert_before(&editor, "\\end{document}", "\\nocite{*}\n");
    let found = block_on(labels::unused(&f.state));
    assert!(found.iter().all(|f| f.kind == NameKind::Label), "{found:?}");
}

// ----- find references -----

fn references() {
    let f = fixture("labels-references");
    let editor = f.open("main.tex");
    f.wait_index();
    // On the label itself.
    editor.place_cursor_at(offset_of(&editor, "\\label{eq:one}", 9));
    labels::find_references(&f.state);
    common::pump_until(TIMEOUT, || labels::references_popover(&f.state).is_some());
    let rows = labels::reference_rows(&f.state);
    let main = f.path("main.tex");
    let chapter = f.path("chapters/ch1.tex");
    let listed: Vec<(bool, u32, bool)> = rows
        .iter()
        .map(|r| (normalize(&r.path) == main, r.line, r.definition))
        .collect();
    assert_eq!(
        listed,
        vec![
            (true, 9, false),
            (true, 9, false),
            (true, 9, false),
            (true, 14, true),
            (false, 1, false),
        ],
        "the active file first, each by position"
    );
    assert_eq!(normalize(&rows[4].path), chapter);
    assert_eq!(
        rows[4].text,
        "Back to Theorem~\\ref{thm:main} and \\eqref{eq:one}."
    );
    let popover = labels::references_popover(&f.state).unwrap();
    assert!(popover.is_visible());
    assert!(popover.has_css_class("lemma-references"));
    snapshots(&f, "j-references", || {
        labels::close_references(&f.state);
        editor.place_cursor_at(offset_of(&editor, "\\label{eq:one}", 9));
        labels::find_references(&f.state);
        common::pump_until(TIMEOUT, || labels::references_popover(&f.state).is_some());
    });
    // The chapter's use: it opens there; Back returns.
    let from = editor.cursor_offset();
    assert!(labels::activate_reference(&f.state, 4));
    assert!(labels::references_popover(&f.state).is_none());
    common::pump_until(TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| normalize(e.document().path()) == chapter)
    });
    let active = f.state.active_editor().unwrap();
    assert_eq!(
        active.cursor_offset(),
        offset_of(&active, "{eq:one}", 1),
        "at the name"
    );
    assert!(goto::back(&f.state));
    common::pump_until(TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| Rc::ptr_eq(&e, &editor) && e.cursor_offset() == from)
    });
    // From a reference (the cursor right after the name works too).
    editor.place_cursor_at(offset_of(&editor, "\\ref{thm:main}", 13));
    labels::find_references(&f.state);
    common::pump_until(TIMEOUT, || labels::references_popover(&f.state).is_some());
    assert_eq!(labels::reference_rows(&f.state).len(), 3);
    labels::close_references(&f.state);
    assert!(labels::references_popover(&f.state).is_none());
    // Nothing at the cursor: no list.
    editor.place_cursor_at(offset_of(&editor, "Statement", 2));
    labels::find_references(&f.state);
    common::pump_for(Duration::from_millis(200));
    assert!(labels::references_popover(&f.state).is_none());
}

fn cite_references() {
    let f = fixture("labels-cite-references");
    let editor = f.open("main.tex");
    f.wait_index();
    editor.place_cursor_at(offset_of(&editor, "{knuth,", 2));
    WidgetExt::activate_action(&f.state.window, "win.find-references", None).unwrap();
    common::pump_until(TIMEOUT, || labels::references_popover(&f.state).is_some());
    let rows = labels::reference_rows(&f.state);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(!rows[0].definition);
    assert_eq!(rows[0].line, 10);
    assert!(rows[1].definition, "the entry");
    assert_eq!(normalize(&rows[1].path), f.path("refs.bib"));
    assert_eq!(rows[1].text, "@book{knuth,");
}

// ----- rename -----

fn rename() {
    let f = fixture("labels-rename");
    let editor = f.open("main.tex");
    f.wait_index();
    let main = editor.document().clone();
    let chapter = f.path("chapters/ch1.tex");
    let outcome = block_on(labels::rename(&f.state, "thm:main", "thm:central")).unwrap();
    assert_eq!(outcome.places, 3);
    assert_eq!(outcome.files, 2);
    let renamed_main = MAIN.replace("thm:main", "thm:central");
    let renamed_chapter = CHAPTER.replace("thm:main", "thm:central");
    assert_eq!(main.text(), renamed_main);
    let chapter_doc = f
        .state
        .workspace
        .get(&chapter)
        .expect("the chapter is loaded");
    assert!(
        !chapter_doc.is_open(),
        "the chapter was not opened in a tab"
    );
    assert_eq!(chapter_doc.text(), renamed_chapter);
    // Saved (both files had no unsaved changes).
    common::pump_until(TIMEOUT, || {
        disk(&f.path("main.tex")) == renamed_main && disk(&chapter) == renamed_chapter
    });
    // Reviewable hunks in both files, one named group; the banner names it.
    review_ui::recompute_if_stale(&main);
    review_ui::recompute_if_stale(&chapter_doc);
    assert_eq!(review_ui::hunk_count(&main), 2);
    assert_eq!(review_ui::hunk_count(&chapter_doc), 1);
    let summary = review_ui::review_summary(&f.state);
    assert_eq!(summary.groups.len(), 1);
    assert_eq!(summary.groups[0].kind, GroupKind::Turn(outcome.turn));
    assert_eq!(summary.groups[0].label, "Rename thm:main → thm:central");
    if let Some(review) = f.state.review.get() {
        review.refresh();
    }
    let banner = f.state.banner.title();
    assert!(banner.contains("Rename thm:main → thm:central"), "{banner}");
    // The project index follows (the chapter is indexed again from what was saved): nothing
    // new is undefined.
    f.wait_checks();
    labels::check_now(&f.state);
    let undefined: Vec<String> = labels::undefined(&f.state)
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(undefined, ["missing", "sec:undefined"]);
    // Reject restores everything, in the buffers and on disk.
    let decided = review_ui::decide_turn(&f.state, outcome.turn, Decision::Reject);
    assert_eq!(decided.decided, 3);
    assert_eq!(main.text(), MAIN);
    assert_eq!(chapter_doc.text(), CHAPTER);
    common::pump_until(TIMEOUT, || {
        disk(&f.path("main.tex")) == MAIN && disk(&chapter) == CHAPTER
    });
    assert_eq!(review_ui::review_summary(&f.state).hunks, 0);
    // Refused names change nothing.
    for new in ["eq:one", "has space", "a,b", "thm:main", ""] {
        assert!(
            block_on(labels::rename(&f.state, "thm:main", new)).is_err(),
            "{new:?}"
        );
    }
    assert_eq!(main.text(), MAIN);
}

fn rename_per_hunk() {
    let f = fixture("labels-rename-hunks");
    let editor = f.open("main.tex");
    f.wait_index();
    let main = editor.document().clone();
    let chapter = f.path("chapters/ch1.tex");
    // main.tex has unsaved changes: they stay unsaved; the chapter is saved.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% unsaved\n");
    assert!(main.is_dirty());
    let outcome = block_on(labels::rename(&f.state, "eq:one", "eq:first")).unwrap();
    assert_eq!((outcome.places, outcome.files), (5, 2));
    assert!(main.is_dirty(), "unsaved changes stay unsaved");
    assert_eq!(disk(&f.path("main.tex")), MAIN, "not saved");
    common::pump_until(TIMEOUT, || {
        disk(&chapter) == CHAPTER.replace("eq:one", "eq:first")
    });
    // Accept main.tex's hunks, reject the chapter's: each file decides on its own.
    let chapter_doc = f.state.workspace.get(&chapter).unwrap();
    review_ui::recompute_if_stale(&main);
    review_ui::recompute_if_stale(&chapter_doc);
    review_ui::decide_document(&f.state, &main, Decision::Accept);
    review_ui::decide_document(&f.state, &chapter_doc, Decision::Reject);
    assert_eq!(
        main.text(),
        format!("% unsaved\n{}", MAIN.replace("eq:one", "eq:first"))
    );
    assert_eq!(chapter_doc.text(), CHAPTER);
    common::pump_until(TIMEOUT, || disk(&chapter) == CHAPTER);
    assert_eq!(review_ui::review_summary(&f.state).hunks, 0);
}

fn rename_popover() {
    let f = fixture("labels-rename-popover");
    let editor = f.open("main.tex");
    f.wait_index();
    // F2 on a reference: the popover asks for the new name, the part after the prefix
    // selected.
    editor.place_cursor_at(offset_of(&editor, "\\eqref{eq:one}", 9));
    WidgetExt::activate_action(&f.state.window, "win.rename-label", None).unwrap();
    common::pump_until(TIMEOUT, || labels::rename_popover(&f.state).is_some());
    let entry = labels::rename_entry(&f.state).unwrap();
    assert_eq!(entry.text(), "eq:one");
    assert_eq!(entry.selection_bounds(), Some((3, 6)));
    common::pump_until(TIMEOUT, || {
        labels::rename_status(&f.state).as_deref() == Some("5 places in 2 files")
    });
    snapshots(&f, "j-rename", || {
        labels::close_rename(&f.state);
        editor.place_cursor_at(offset_of(&editor, "\\eqref{eq:one}", 9));
        labels::rename_label(&f.state);
        common::pump_until(TIMEOUT, || {
            labels::rename_status(&f.state).as_deref() == Some("5 places in 2 files")
        });
    });
    let entry = labels::rename_entry(&f.state).unwrap();
    // Names that cannot be used are explained and refused.
    entry.set_text("eq:two");
    let status = labels::rename_status(&f.state).unwrap();
    assert!(status.contains("already a label"), "{status}");
    assert!(entry.has_css_class("error"));
    assert!(!labels::submit_rename(&f.state, "eq:two"));
    assert!(labels::rename_popover(&f.state).is_some());
    entry.set_text("eq one");
    assert_eq!(
        labels::rename_status(&f.state).as_deref(),
        Some("A label cannot contain spaces")
    );
    entry.set_text("eq:first");
    assert_eq!(
        labels::rename_status(&f.state).as_deref(),
        Some("5 places in 2 files")
    );
    assert!(!entry.has_css_class("error"));
    // Enter: the popover goes and the rename becomes hunks.
    entry.emit_activate();
    assert!(labels::rename_popover(&f.state).is_none());
    let main = editor.document().clone();
    common::pump_until(TIMEOUT, || {
        main.text() == MAIN.replace("eq:one", "eq:first")
    });
    common::pump_until(TIMEOUT, || review_ui::review_summary(&f.state).hunks == 3);
    // Not on a label, on a citation key: no popover.
    editor.place_cursor_at(offset_of(&editor, "Statement", 2));
    labels::rename_label(&f.state);
    editor.place_cursor_at(offset_of(&editor, "{knuth,", 2));
    labels::rename_label(&f.state);
    common::pump_for(Duration::from_millis(50));
    assert!(labels::rename_popover(&f.state).is_none());
    // Closing it changes nothing.
    editor.place_cursor_at(offset_of(&editor, "\\label{thm:main}", 9));
    labels::rename_label(&f.state);
    common::pump_until(TIMEOUT, || labels::rename_popover(&f.state).is_some());
    labels::close_rename(&f.state);
    assert!(labels::rename_popover(&f.state).is_none());
    assert_eq!(main.text(), MAIN.replace("eq:one", "eq:first"));
}

fn popovers_close_with_tab() {
    let f = fixture("labels-popovers-tab");
    let editor = f.open("main.tex");
    f.wait_index();
    editor.place_cursor_at(offset_of(&editor, "\\label{eq:one}", 9));
    labels::find_references(&f.state);
    common::pump_until(TIMEOUT, || labels::references_popover(&f.state).is_some());
    let popover = labels::references_popover(&f.state).unwrap();
    let view = editor.view().downgrade();
    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_for(Duration::from_millis(100));
    assert!(labels::references_popover(&f.state).is_none());
    assert!(popover.parent().is_none(), "no popover stays on the view");
    common::pump_until(TIMEOUT, || view.upgrade().is_none());
}

// ----- equation numbers -----

fn equation_numbers() {
    let f = fixture("labels-eqnumbers");
    let editor = f.open("main.tex");
    f.wait_index();
    assert_eq!(
        annotations::shown(&editor),
        vec![
            (8, "§1".to_owned()),
            (11, "1.1".to_owned()),
            (14, "(1)".to_owned()),
            (18, "(2)".to_owned()),
        ]
    );
    // The chapter's section.
    let chapter = f.open("chapters/ch1.tex");
    assert_eq!(annotations::shown(&chapter), vec![(0, "§2".to_owned())]);
    f.state.select_editor(&editor);
    // Lines inserted above move the numbers at once (before any check).
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% a\n% b\n");
    assert_eq!(
        annotations::shown(&editor)
            .iter()
            .map(|(l, _)| *l)
            .collect::<Vec<_>>(),
        vec![10, 13, 16, 20]
    );
    // Typing on a labelled line keeps its number.
    insert_before(&editor, "\\label{eq:one}", " ");
    assert_eq!(annotations::shown(&editor)[2], (16, "(1)".to_owned()));
    // A new build: new numbers from the `.aux` (read again by the project index).
    std::fs::write(
        f.path("main.aux"),
        AUX.replace("{thm:main}{{1.1}{1}}", "{thm:main}{{2.1}{3}}")
            .replace("{eq:one}{{1}{1}}", "{eq:one}{{7}{3}}"),
    )
    .unwrap();
    completion::refresh_after_build(&f.state, None);
    let state = f.state.clone();
    common::pump_until(TIMEOUT, || {
        !completion::project_busy(&state)
            && state
                .workspace
                .index
                .borrow()
                .aux
                .labels
                .get("eq:one")
                .is_some_and(|(n, _)| n == "7")
    });
    labels::check_now(&f.state);
    assert_eq!(
        annotations::shown(&editor),
        vec![
            (10, "§1".to_owned()),
            (13, "2.1".to_owned()),
            (16, "(7)".to_owned()),
            (20, "(2)".to_owned()),
        ]
    );
    // `[editor] equation_numbers = false` hides them; back on, they return.
    let mut config = (*f.state.config.get()).clone();
    config.editor.equation_numbers = false;
    f.state.set_config(Rc::new(config.clone()));
    assert!(annotations::shown(&editor).is_empty());
    assert!(annotations::shown(&chapter).is_empty());
    config.editor.equation_numbers = true;
    f.state.set_config(Rc::new(config));
    assert_eq!(annotations::shown(&editor).len(), 4);
    assert_eq!(annotations::shown(&chapter).len(), 1);
}
