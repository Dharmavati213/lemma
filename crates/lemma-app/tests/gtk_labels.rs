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
use lemma_core::{DiagSource, Origin, TextEdit};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let mut tests: Vec<common::Test> = vec![
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
        (
            "undefined_checks_look_again_only_at_what_changed",
            undefined_checks_what_changed,
        ),
        (
            "undefined_references_wait_for_every_file_past_the_cap",
            undefined_past_the_cap,
        ),
        ("problems_page_references_section_and_fix", problems_page),
        (
            "problems_keep_the_lines_of_a_file_edited_before_its_tab",
            problems_of_a_file_opened_later,
        ),
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
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_equation_numbers_large_tex", perf_equation_numbers));
        tests.push(("perf_undefined_check_big_project", perf_check_big_project));
        tests.push(("perf_equation_numbers_follow_lines", perf_numbers_follow));
        tests.push((
            "perf_undefined_references_many_files",
            perf_undefined_many_files,
        ));
    }
    common::run(&tests)
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

/// A check looks again only at what the project index replaced since the last one: after prose
/// typed (the index keeps the file's entry) at nothing, after a reference added at its file,
/// after a label added at its file and at the files that refer to it.
fn undefined_checks_what_changed() {
    let f = fixture("labels-undefined-incremental");
    let editor = f.open("main.tex");
    let chapter = f.open("chapters/ch1.tex");
    f.wait_index();
    f.wait_checks();
    let state = f.state.clone();
    // Waits for the check an edit scheduled: (checks skipped, files looked at) since `before`.
    let after = |before: (u64, u64)| {
        let checks = labels::checks_done(&state);
        common::pump_until(TIMEOUT, || labels::checks_done(&state) > checks);
        f.wait_checks();
        (
            labels::checks_skipped(&state) - before.0,
            labels::files_checked(&state) - before.1,
        )
    };
    let counts = || {
        (
            labels::checks_skipped(&state),
            labels::files_checked(&state),
        )
    };
    let before = counts();
    insert_before(&editor, "Statement $", "A claim. ");
    assert_eq!(after(before), (1, 0), "prose typed");
    let before = counts();
    let end = chapter.document().text().len();
    chapter
        .buffer()
        .insert(&mut chapter.iter_near(end).unwrap(), "\\ref{sec:later}\n");
    assert_eq!(after(before), (0, 1), "a reference");
    assert!(
        labels::undefined(&f.state)
            .iter()
            .any(|u| u.name == "sec:later")
    );
    let before = counts();
    insert_before(
        &editor,
        "\\end{document}",
        "\\section{Later}\\label{sec:later}\n",
    );
    assert_eq!(
        after(before),
        (0, 2),
        "a label, and the file referring to it"
    );
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![
            ("missing".to_owned(), 10, NameKind::Cite),
            ("sec:undefined".to_owned(), 23, NameKind::Label),
        ]
    );
}

/// A project that includes more files than a window reads from disk (`completion::
/// MAX_DISK_FILES`): a reference to a label in a file past the cap would look undefined, so
/// no reference is checked (citations still are) until the project fits again.
fn undefined_past_the_cap() {
    let f = fixture("labels-undefined-cap");
    std::fs::remove_file(f.dir.join("main.aux")).unwrap();
    let chapters = completion::MAX_DISK_FILES + 5;
    std::fs::create_dir_all(f.dir.join("c")).unwrap();
    let mut main =
        String::from("\\documentclass{book}\n\\addbibresource{refs.bib}\n\\begin{document}\n");
    for i in 0..chapters {
        main += &format!("\\input{{c/c{i:03}}}\n");
        // Each chapter refers to the next one (the last one to one that stays below).
        let next = if i + 1 < chapters { i + 1 } else { 20 };
        let text = format!("\\chapter{{C{i}}}\\label{{ch:{i}}}\nSee \\ref{{ch:{next}}}.\n");
        std::fs::write(f.dir.join(format!("c/c{i:03}.tex")), text).unwrap();
    }
    main += "\\ref{nowhere} \\cite{knuth, missing}\n\\end{document}\n";
    std::fs::write(f.dir.join("main.tex"), &main).unwrap();
    let last = u32::try_from(3 + chapters).unwrap();
    let editor = f.open("main.tex");
    completion::refresh_project(&f.state);
    let state = f.state.clone();
    common::pump_until(Duration::from_secs(120), || {
        !completion::project_busy(&state)
            && state.workspace.index.borrow().files.len() > completion::MAX_DISK_FILES
    });
    labels::check_now(&f.state);
    assert!(completion::index_capped(&f.state));
    assert!(!labels::labels_checked(&f.state) && labels::cites_checked(&f.state));
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![("missing".to_owned(), last, NameKind::Cite)]
    );
    // Ten chapters fewer: they leave the index, the five past the cap are read, and the
    // references are checked again.
    let start = offset_of(&editor, "\\input{c/c000}", 0);
    let end = offset_of(&editor, "\\input{c/c010}", 0);
    editor.buffer().delete(
        &mut editor.iter_near(start).unwrap(),
        &mut editor.iter_near(end).unwrap(),
    );
    common::pump_until(Duration::from_secs(120), || {
        let index = state.workspace.index.borrow();
        !completion::project_busy(&state)
            && index.find_label("ch:304").is_some()
            && index.find_label("ch:0").is_none()
    });
    f.wait_checks();
    assert!(!completion::index_capped(&f.state));
    assert!(labels::labels_checked(&f.state));
    assert_eq!(
        names(&labels::undefined(&f.state)),
        vec![
            ("missing".to_owned(), last - 10, NameKind::Cite),
            ("nowhere".to_owned(), last - 10, NameKind::Label),
        ]
    );
}

/// A chapter's undefined reference, published while the chapter was neither open nor in the
/// workspace, keeps its line on the Problems page when the chapter is edited without a tab (as
/// an agent's edit loads it) and then opened: the edit leaves the finding as it was, so the
/// check does not publish it again, and the tab's panel had no lines to follow it with.
fn problems_of_a_file_opened_later() {
    let f = fixture("labels-problems-later");
    let chapter = f.path("chapters/ch1.tex");
    std::fs::write(&chapter, format!("{CHAPTER}See \\ref{{sec:nowhere}}.\n")).unwrap();
    let _main = f.open("main.tex");
    f.wait_index();
    f.wait_checks();
    let panel = f.state.build.get().unwrap().diagnostics();
    let line = || {
        panel
            .findings(DiagSource::Labels)
            .iter()
            .find(|x| x.problem.message.contains("sec:nowhere"))
            .map(|x| x.problem.line)
    };
    assert_eq!(line(), Some(Some(4)));
    let doc = block_on(f.state.workspace.get_or_load(&chapter)).expect("the chapter");
    let end = doc.text().len();
    doc.apply_edits(
        &[TextEdit::insert(end, "% a note\n")],
        Origin::Agent { call: 1 },
    )
    .expect("valid edit");
    f.wait_checks();
    let _chapter = f.open("chapters/ch1.tex");
    f.wait_checks();
    assert_eq!(
        line(),
        Some(Some(4)),
        "the finding's line once the tab is open"
    );
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
    // Lines inserted above move the numbers before the next frame is laid out (before any
    // check): the line changes until then (two inserts here) add them again once.
    let lines = |editor: &Editor| {
        annotations::shown(editor)
            .iter()
            .map(|(l, _)| *l)
            .collect::<Vec<_>>()
    };
    let rebuilds = annotations::rebuilds(&editor);
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% a\n");
    buffer.insert(&mut buffer.start_iter(), "% b\n");
    assert_eq!(lines(&editor), vec![8, 11, 14, 18], "not inside the change");
    let clock = f.state.window.frame_clock().unwrap();
    let at_layout: Rc<std::cell::RefCell<Option<Vec<u32>>>> = Rc::default();
    let (seen, laid_out) = (at_layout.clone(), editor.clone());
    let id = clock.connect_layout(move |_| {
        seen.borrow_mut().get_or_insert_with(|| lines(&laid_out));
    });
    editor.view().queue_draw();
    common::pump_until(TIMEOUT, || at_layout.borrow().is_some());
    clock.disconnect(id);
    assert_eq!(at_layout.take(), Some(vec![10, 13, 16, 20]));
    assert_eq!(annotations::rebuilds(&editor), rebuilds + 1);
    // Lines inserted below every number move none.
    let end = offset_of(&editor, "\\end{document}", 0);
    editor
        .buffer()
        .insert(&mut editor.iter_near(end).unwrap(), "% c\n");
    common::pump_for(Duration::from_millis(20));
    assert_eq!(annotations::rebuilds(&editor), rebuilds + 1);
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

/// `tests/fixtures/large.tex` with a label in each of its 2500 theorems besides the 2500 of its
/// sections, a preamble defining the theorems, and an `.aux` that numbers all 5000, open in a
/// window of the fixture `name` and indexed, with its numbers shown: the fixture, the editor and
/// the count of numbers.
fn numbered_large_tex(name: &str) -> (Fixture, Rc<Editor>, usize) {
    let large = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let mut text = String::with_capacity(large.len() + (64 << 10));
    let mut theorems = 0;
    for line in large.split_inclusive('\n') {
        text.push_str(line);
        if line == "\\usepackage{amsmath}\n" {
            text.push_str("\\newtheorem{theorem}{Theorem}[section]\n\\newtheorem{lemma}{Lemma}\n");
        } else if line == "\\begin{theorem}\n" {
            text.push_str(&format!("\\label{{thm:{theorems}}}\n"));
            theorems += 1;
        }
    }
    let mut aux = String::from("\\relax\n");
    let mut numbered = 0;
    for (at, _) in text.match_indices("\\label{") {
        let name = &text[at + 7..at + 7 + text[at + 7..].find('}').unwrap()];
        aux.push_str(&format!("\\newlabel{{{name}}}{{{{{numbered}}}{{1}}}}\n"));
        numbered += 1;
    }
    let f = fixture(name);
    std::fs::write(f.path("main.tex"), &text).unwrap();
    std::fs::write(f.path("main.aux"), &aux).unwrap();
    let editor = f.open("main.tex");
    completion::refresh_project(&f.state);
    let state = f.state.clone();
    let main = f.path("main.tex");
    common::pump_until(Duration::from_secs(60), || {
        let index = state.workspace.index.borrow();
        !completion::project_busy(&state)
            && index.aux.labels.len() >= numbered
            && index
                .files
                .iter()
                .any(|(p, file)| normalize(p) == main && file.labels.len() >= numbered)
    });
    labels::check_now(&f.state);
    assert_eq!(
        annotations::shown(&editor).len(),
        numbered,
        "{theorems} theorems"
    );
    (f, editor, numbered)
}

/// The inline equation numbers of `tests/fixtures/large.tex` (60k lines) numbered as
/// [`numbered_large_tex`] does: what one refresh costs when the numbers changed (the text read
/// again, every label looked up and named), and the main loop's work after a new undefined
/// reference is underlined.
fn perf_equation_numbers() {
    let (f, editor, numbered) = numbered_large_tex("labels-perf-numbers");
    let state = f.state.clone();
    let mut times = Vec::new();
    for i in 0..9 {
        // A number that is new to the refresh: everything is computed again.
        let mut index = f.state.workspace.index.borrow_mut();
        std::sync::Arc::make_mut(&mut index.aux)
            .labels
            .insert(format!("perf:{i}"), ("1".to_owned(), "1".to_owned()));
        drop(index);
        let started = Instant::now();
        annotations::refresh(&f.state, &editor);
        times.push(started.elapsed());
    }
    assert_eq!(annotations::shown(&editor).len(), numbered);
    times.sort();
    let median = times[times.len() / 2];
    // A new undefined reference: the underlines are removed where they are, not from the whole
    // text (an underline changes the text's size: GTK would lay all of it out again).
    let context = glib::MainContext::default();
    let drain = || {
        let started = Instant::now();
        while context.pending() {
            context.iteration(false);
        }
        started.elapsed()
    };
    drain();
    let at = offset_of(&editor, "\\end{document}", 0);
    editor.buffer().insert(
        &mut editor.iter_near(at).unwrap(),
        "\\ref{perf:undefined}\n",
    );
    common::pump_until(Duration::from_secs(60), || {
        labels::undefined(&state)
            .iter()
            .any(|u| u.name == "perf:undefined")
    });
    let underlined = drain();
    assert_eq!(labels::underlined(&editor).len(), 1);
    println!(
        "    equation numbers of {numbered} labels on {} lines: refresh median {median:?}, max \
         {:?}; a new undefined reference underlined, then the main loop {underlined:?}",
        editor.buffer().line_count(),
        times[times.len() - 1]
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(median <= Duration::from_millis(15), "{median:?}");
        assert!(underlined <= Duration::from_millis(50), "{underlined:?}");
    }
}

/// The 5000 equation numbers of [`numbered_large_tex`] following line changes above them: what
/// a line break costs the main thread inside the change (as in a key's handler), and with what
/// is ready to run after it (the numbers have moved then), and ten line changes made at once (a
/// replacement, Vim's `.`) with what is ready to run after them; 9 of each, on the CPU.
fn perf_numbers_follow() {
    let (_f, editor, numbered) = numbered_large_tex("labels-perf-follow");
    let context = glib::MainContext::default();
    let drain = || {
        while context.pending() {
            context.iteration(false);
        }
    };
    let first_number = || annotations::shown(&editor)[0].0;
    let buffer = editor.buffer();
    let (mut handler, mut frame, mut many) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..9 {
        drain();
        // A line break at the end of the first line, above every number.
        let at = editor.document().line_index().line_range(0).end;
        let before = first_number();
        let mut work = Duration::ZERO;
        handler.push(common::big_project::on_cpu(|| {
            buffer.insert(&mut editor.iter_near(at).unwrap(), "\n");
        }));
        work += handler[handler.len() - 1];
        work += common::big_project::on_cpu(drain);
        assert_eq!(first_number(), before + 1);
        frame.push(work);
        // Ten lines added one by one in one action.
        let before = first_number();
        let start = editor.document().line_index().line_start(1);
        many.push(common::big_project::on_cpu(|| {
            buffer.begin_user_action();
            for _ in 0..10 {
                buffer.insert(&mut editor.iter_near(start).unwrap(), "% a line\n");
            }
            buffer.end_user_action();
            drain();
        }));
        assert_eq!(first_number(), before + 10);
    }
    let [handler, frame, many] = [handler, frame, many].map(|mut times| {
        let shown = common::big_project::median_max(&mut times);
        (times[times.len() / 2], shown)
    });
    println!(
        "    {numbered} numbers: a line break {} in the change, {} until the next frame; ten \
         line changes at once {}; load {}",
        handler.1,
        frame.1,
        many.1,
        common::big_project::load()
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budgets are checked in release)");
        return;
    }
    assert!(handler.0 <= Duration::from_millis(1), "{:?}", handler.0);
    assert!(many.0 <= Duration::from_millis(10), "{:?}", many.0);
}

/// The label checks on a generated book (`common::big_project`: 300 chapters, 20k labels, 30k
/// references, 40k citations, a 10k-entry bibliography and a `.aux` that numbers every label),
/// with the root and a chapter open: what the check that runs once typing stopped and the
/// chapter was indexed again costs the main thread (with the equation numbers of both editors;
/// the time it waited for a CPU left out), for prose (its index entry comes back the same),
/// Enter (the chapter's labels move down a line), a new `\ref`, a new `\label` and a reference
/// to nothing (underlined and published), 7 of each at different places.
fn perf_check_big_project() {
    let f = fixture("labels-perf-big");
    std::fs::remove_file(f.path("main.aux")).unwrap();
    let big = common::big_project::write(&f.dir, 300, 66, 10_000, 1);
    let _main = f.open("main.tex");
    let chapter = f.open("c/c0150.tex");
    completion::refresh_project(&f.state);
    let state = f.state.clone();
    common::pump_until(Duration::from_secs(300), || {
        let index = state.workspace.index.borrow();
        !completion::project_busy(&state)
            && index.files.len() > big.chapters
            && !index.bibs.is_empty()
            && index.aux.labels.len() >= big.labels
    });
    labels::check_now(&f.state);
    f.wait_checks();
    assert!(labels::undefined(&f.state).is_empty());
    println!("    {big:?}");
    let mut line = 40;
    let mut report = Vec::new();
    for (kind, insert) in [
        ("prose", " word"),
        ("Enter", "\n"),
        ("\\ref", " \\ref{ch:3}"),
        ("\\label", "\\label{perf:new:#}"),
        ("undefined \\ref", " \\ref{perf:nowhere:#}"),
    ] {
        let mut times = Vec::new();
        let mut attempts = 0;
        while times.len() < 7 {
            attempts += 1;
            assert!(attempts < 30, "the scheduled check kept running first");
            line = common::big_project::prose_line(&chapter.document().text(), line as usize + 3);
            let doc = chapter.document();
            let end = doc.line_index().line_range(line).end;
            let buffer = chapter.buffer();
            let mut at = chapter.iter_near(end).unwrap();
            buffer.insert(&mut at, &insert.replace('#', &attempts.to_string()));
            let checks = labels::checks_done(&f.state);
            // The chapter is indexed again 300 ms after the change; the check would run 450 ms
            // after it.
            common::pump_until(TIMEOUT, || !completion::index_busy(&state));
            if labels::checks_done(&f.state) != checks {
                f.wait_checks();
                continue;
            }
            times.push(common::big_project::on_cpu(|| labels::check_now(&f.state)));
            f.wait_checks();
        }
        report.push((kind, common::big_project::median_max(&mut times), times));
    }
    let undefined = labels::undefined(&f.state).len();
    assert!(undefined >= 7, "the references to nothing: {undefined}");
    for (kind, times, _) in &report {
        println!("    check after {kind}: median/max {times}");
    }
    println!("    load {}", common::big_project::load());
    if cfg!(debug_assertions) {
        println!("    (debug build: the budgets are checked in release)");
        return;
    }
    for (kind, _, times) in &report {
        let median = times[times.len() / 2];
        let budget = Duration::from_millis(if *kind == "prose" { 1 } else { 8 });
        assert!(median <= budget, "{kind}: median {median:?}");
    }
}

/// Chapters and undefined references per chapter of [`perf_undefined_many_files`].
const PERF_CHAPTERS: usize = 40;
const PERF_UNDEFINED: usize = 10;

/// A project whose 40 chapters cite 10 undefined labels each (402 findings with the fixture's
/// two). Enter at the start of one chapter moves its findings: prints the main loop's longest
/// step and its work in all from the key until the check that follows ran and a frame was
/// drawn, with the Problems page hidden and shown (medians and maxima of 5). Release builds
/// check that the longest step takes at most 100 ms with the page shown and 60 ms with it
/// hidden (measured 53 ms and 26 ms at a load of 10-15, 25 ms and 13 ms at 6; 4.0 s and 28 ms
/// when every check gave the panel every file's findings, one file at a time, and each refilled
/// the whole section).
fn perf_undefined_many_files() {
    let f = fixture("labels-perf-undefined");
    let inputs: String = (0..PERF_CHAPTERS)
        .map(|k| format!("\\input{{chapters/p{k}}}\n"))
        .collect();
    let main = MAIN.replace("\\end{document}\n", &format!("{inputs}\\end{{document}}\n"));
    std::fs::write(f.dir.join("main.tex"), main).unwrap();
    for k in 0..PERF_CHAPTERS {
        let text: String = (0..PERF_UNDEFINED)
            .map(|j| format!("Part {k}, see \\ref{{undef:{k}:{j}}} here.\nMore text.\n"))
            .collect();
        std::fs::write(f.dir.join(format!("chapters/p{k}.tex")), text).unwrap();
    }
    let editor = f.open("chapters/p20.tex");
    f.wait_index();
    let state = f.state.clone();
    let expected = PERF_CHAPTERS * PERF_UNDEFINED + 2;
    common::pump_until(Duration::from_secs(60), || {
        labels::check_now(&state);
        !completion::project_busy(&state) && labels::undefined(&state).len() == expected
    });
    let clock = f.state.window.frame_clock().unwrap();
    let painted = Rc::new(std::cell::Cell::new(0u64));
    let count = painted.clone();
    let handler = clock.connect_after_paint(move |_| count.set(count.get() + 1));
    let buffer = editor.buffer();
    let measure = |shown: bool| -> (Vec<Duration>, Vec<Duration>) {
        if shown {
            lemma_app::diagnostics_panel::reveal(&f.state);
        } else {
            f.state.sidebar.show_page(lemma_app::window::PAGE_OUTLINE);
        }
        assert!(block_on(lemma_app::testmode::wait_for_frames(
            &f.state.window,
            1
        )));
        let (mut longest, mut total) = (Vec::new(), Vec::new());
        for _ in 0..5 {
            common::pump_for(Duration::from_millis(100));
            let checks = labels::checks_done(&state);
            buffer.insert(&mut buffer.start_iter(), "\n");
            let (mut step, mut work) = (Duration::ZERO, Duration::ZERO);
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut frame = None;
            while frame.is_none_or(|n| painted.get() <= n) {
                assert!(Instant::now() < deadline, "no check or frame");
                let t = common::timed_iteration();
                step = step.max(t);
                work += t;
                if frame.is_none() && labels::checks_done(&state) > checks {
                    frame = Some(painted.get());
                    f.state.window.queue_draw();
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            longest.push(step);
            total.push(work);
        }
        longest.sort();
        total.sort();
        (longest, total)
    };
    let (hidden, hidden_work) = measure(false);
    let (shown, shown_work) = measure(true);
    clock.disconnect(handler);
    assert_eq!(labels::undefined(&state).len(), expected);
    let ms = |d: &Duration| format!("{:.1}", d.as_secs_f64() * 1000.0);
    println!(
        "    {expected} undefined references in {} files, Enter in one: longest main-loop step \
         (median, max): page hidden {} / {} ms (work {} ms), shown {} / {} ms (work {} ms)",
        PERF_CHAPTERS + 1,
        ms(&hidden[2]),
        ms(&hidden[4]),
        ms(&hidden_work[2]),
        ms(&shown[2]),
        ms(&shown[4]),
        ms(&shown_work[2])
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(hidden[2] <= Duration::from_millis(60), "hidden: {hidden:?}");
        assert!(shown[2] <= Duration::from_millis(100), "shown: {shown:?}");
    }
}
