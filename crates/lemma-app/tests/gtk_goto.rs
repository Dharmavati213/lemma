//! WP3.2: go to definition (the resolvers in `editor::goto`) and the hovers (the providers in
//! `editor::hover`) for references, citations, included files, local packages, images and user
//! macros, on the project fixture of `tests/labels_common`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_goto`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `j-hover-crop`, `j-hover-source`, `j-hover-cite` and
//! `j-hover-macro` (each `-light.png` and `-dark.png`) there.

mod common;
mod labels_common;

use adw::prelude::*;
use labels_common::{
    Fixture, block_on, find, fixture, fixture_with, offset_of, proj_fixture, snapshots, text_of,
};
use lemma_app::editor::goto::{self, GotoTarget};
use lemma_app::editor::hover::{self, HoverContent, HoverCtx};
use lemma_app::editor::{Editor, TextPoint, Trigger};
use lemma_app::workspace::normalize;
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        ("goto_targets_of_every_kind", targets),
        ("goto_on_the_command_name_and_not_elsewhere", command_names),
        ("goto_follows_live_text_and_closed_files", live_and_closed),
        ("goto_definition_back_and_ctrl_click", navigation),
        ("goto_image_asks_the_system_viewer", image),
        (
            "hover_reference_shows_number_page_and_source",
            hover_reference,
        ),
        (
            "hover_reference_to_a_closed_file_reads_it",
            hover_closed_file,
        ),
        ("hover_label_citation_and_missing_names", hover_names),
        ("hover_user_macro_definition_and_math", hover_macro),
        ("hover_popover_shows_the_reference", hover_popover),
        ("hover_reference_shows_the_pdf_crop", hover_crop),
    ])
}

/// The target at the first `needle` (plus `delta`) of `editor`.
fn target(editor: &Editor, needle: &str, delta: usize) -> Option<GotoTarget> {
    goto::resolve_at(editor, offset_of(editor, needle, delta), Trigger::Keyboard)
}

/// `GotoTarget::File` at the first `needle` (plus `delta`) of the file `path`'s text.
fn file_target(path: PathBuf, text: &str, needle: &str, delta: usize) -> GotoTarget {
    GotoTarget::File {
        offset: find(text, needle, delta),
        path,
    }
}

fn targets() {
    let f = fixture("goto-targets");
    let editor = f.open("main.tex");
    f.wait_index();
    let main = f.path("main.tex");
    let text = editor.document().text();
    let at_label = |name: &str| file_target(main.clone(), &text, &format!("\\label{{{name}}}"), 7);
    // References of every form go to the `\label` (the open document: an offset).
    assert_eq!(
        target(&editor, "\\ref{thm:main}", 5),
        Some(at_label("thm:main"))
    );
    assert_eq!(
        target(&editor, "\\eqref{eq:one}", 7),
        Some(at_label("eq:one"))
    );
    assert_eq!(
        target(&editor, "\\cref{sec:intro,eq:one}", 16),
        Some(at_label("eq:one")),
        "second name of a list"
    );
    assert_eq!(
        target(&editor, "\\cref{sec:intro,eq:one}", 6),
        Some(at_label("sec:intro"))
    );
    assert_eq!(
        target(&editor, "{eq:one}{eq:two}", 9),
        Some(at_label("eq:two")),
        "the range's second argument"
    );
    assert_eq!(
        target(&editor, "$\\eqref{eq:two}$", 8),
        Some(at_label("eq:two")),
        "a reference in math"
    );
    // A label of a file that is not open: its line, as the project index gives it.
    assert_eq!(
        target(&editor, "\\ref{sec:ch}", 5),
        Some(GotoTarget::Line {
            path: f.path("chapters/ch1.tex"),
            line: 0,
            find: Some("\\label{sec:ch}".into()),
        })
    );
    // A citation key goes to its entry.
    assert_eq!(
        target(&editor, "{knuth, missing}", 2),
        Some(GotoTarget::Line {
            path: f.path("refs.bib"),
            line: 0,
            find: Some("knuth".into()),
        })
    );
    // Files: the chapter, a local package, the bibliography; an image is External.
    assert_eq!(
        target(&editor, "\\input{chapters/ch1}", 9),
        Some(GotoTarget::File {
            path: f.path("chapters/ch1.tex"),
            offset: 0
        })
    );
    assert_eq!(
        target(&editor, "\\usepackage{mydefs}", 13),
        Some(GotoTarget::File {
            path: f.path("mydefs.sty"),
            offset: 0
        })
    );
    assert_eq!(
        target(&editor, "\\addbibresource{refs.bib}", 17),
        Some(GotoTarget::File {
            path: f.path("refs.bib"),
            offset: 0
        })
    );
    assert_eq!(
        target(&editor, "{fig}", 2),
        Some(GotoTarget::External {
            path: f.path("fig.png")
        })
    );
    // User macros go to their definition.
    assert_eq!(
        target(&editor, "$\\cF \\cong", 2),
        Some(file_target(main.clone(), &text, "{\\cF}", 1))
    );
    assert_eq!(
        target(&editor, "\\Hom(A,B)", 2),
        Some(file_target(main.clone(), &text, "{\\Hom}", 1))
    );
    // Nothing: a package of the distribution, a LaTeX command, an undefined or commented
    // reference, a label itself, plain text.
    assert_eq!(target(&editor, "\\usepackage{amsmath", 13), None);
    assert_eq!(target(&editor, "\\section{Intro}", 2), None);
    assert_eq!(target(&editor, "\\ref{sec:undefined}", 6), None);
    assert_eq!(target(&editor, "% \\ref{sec:commented}", 9), None);
    assert_eq!(target(&editor, "\\label{thm:main}", 8), None);
    assert_eq!(target(&editor, "Statement", 2), None);
    assert_eq!(
        target(&editor, "\\mydef", 2),
        None,
        "a macro the index has not read"
    );
}

fn command_names() {
    let f = fixture("goto-command-names");
    let editor = f.open("main.tex");
    f.wait_index();
    let text = editor.document().text();
    let main = f.path("main.tex");
    // On `\eqref`'s letters (and its backslash) the first name of its argument counts.
    for delta in [0, 1, 3, 5] {
        assert_eq!(
            target(&editor, "\\eqref{eq:one}", delta),
            Some(file_target(main.clone(), &text, "\\label{eq:one}", 7)),
            "delta {delta}"
        );
    }
    // On its brace, F12 looks at the character before: the command.
    editor.place_cursor_at(offset_of(&editor, "\\eqref{eq:one}", 6));
    assert_eq!(
        goto::resolve_at_cursor(&editor),
        Some(file_target(main.clone(), &text, "\\label{eq:one}", 7))
    );
    // `\cite[…]{…}` skips its optional argument; `\input` gives its file.
    let line = "\\cite{knuth, missing}";
    assert!(matches!(
        target(&editor, line, 2),
        Some(GotoTarget::Line { find: Some(k), .. }) if k == "knuth"
    ));
    assert!(matches!(
        target(&editor, "\\input{chapters", 3),
        Some(GotoTarget::File { .. })
    ));
    // The point after the name, at the cursor: F12 at the end of a typed name.
    let after = offset_of(&editor, "\\ref{thm:main}", 13);
    editor.place_cursor_at(after);
    assert_eq!(
        goto::resolve_at_cursor(&editor),
        Some(file_target(main.clone(), &text, "\\label{thm:main}", 7))
    );
}

fn live_and_closed() {
    let f = fixture("goto-live");
    let editor = f.open("main.tex");
    f.wait_index();
    // Lines inserted above the label: the target follows the live text at once (before the
    // project index re-reads the document).
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% one\n% two\n");
    let text = editor.document().text();
    assert_eq!(
        target(&editor, "\\ref{thm:main}", 5),
        Some(file_target(
            f.path("main.tex"),
            &text,
            "\\label{thm:main}",
            7
        ))
    );
    // A label typed just now, not indexed yet, is found in the open documents.
    let end = buffer.end_iter().offset();
    let mut iter = buffer.iter_at_offset(end);
    buffer.insert(&mut iter, "\\label{typed:new} \\ref{typed:new}\n");
    let text = editor.document().text();
    assert_eq!(
        target(&editor, "\\ref{typed:new}", 6),
        Some(file_target(
            f.path("main.tex"),
            &text,
            "\\label{typed:new}",
            7
        ))
    );
    // The chapter, once open, is resolved in its live text too.
    let chapter = f.open("chapters/ch1.tex");
    f.state.select_editor(&editor);
    let chapter_text = chapter.document().text();
    assert_eq!(
        target(&editor, "\\ref{sec:ch}", 5),
        Some(file_target(
            f.path("chapters/ch1.tex"),
            &chapter_text,
            "\\label{sec:ch}",
            7
        ))
    );
    // From the chapter back into the root.
    let main_text = editor.document().text();
    assert_eq!(
        target(&chapter, "\\ref{thm:main}", 5),
        Some(file_target(
            f.path("main.tex"),
            &main_text,
            "\\label{thm:main}",
            7
        ))
    );
}

fn navigation() {
    let f = fixture("goto-navigation");
    let editor = f.open("main.tex");
    f.wait_index();
    // F12 on `\ref{sec:ch}`: the chapter opens at its label.
    let at = offset_of(&editor, "\\ref{sec:ch}", 6);
    editor.place_cursor_at(at);
    assert!(goto::goto_definition(&f.state));
    let chapter = f.path("chapters/ch1.tex");
    common::pump_until(labels_common::TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| normalize(e.document().path()) == chapter)
    });
    let active = f.state.active_editor().unwrap();
    assert_eq!(
        active.cursor_offset(),
        offset_of(&active, "\\label{sec:ch}", 0),
        "the line's `\\label{{sec:ch}}`"
    );
    // Back returns to the reference.
    assert!(goto::back(&f.state));
    common::pump_until(labels_common::TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| Rc::ptr_eq(&e, &editor) && e.cursor_offset() == at)
    });
    // Ctrl+click on a citation key: the bibliography opens at the entry's key.
    let key = offset_of(&editor, "{knuth,", 2);
    assert!(goto::ctrl_click(&f.state, &editor, key));
    let bib = f.path("refs.bib");
    common::pump_until(labels_common::TIMEOUT, || {
        f.state
            .active_editor()
            .is_some_and(|e| normalize(e.document().path()) == bib)
    });
    let bib_editor = f.state.active_editor().unwrap();
    assert_eq!(
        bib_editor.cursor_offset(),
        offset_of(&bib_editor, "knuth", 0)
    );
    // Once the bibliography is open, its live text gives the entry.
    f.state.select_editor(&editor);
    assert_eq!(
        target(&editor, "{knuth,", 2),
        Some(GotoTarget::File {
            path: bib.clone(),
            offset: offset_of(&bib_editor, "knuth", 0)
        })
    );
    // Ctrl+click elsewhere is an ordinary click.
    assert!(!goto::ctrl_click(
        &f.state,
        &editor,
        offset_of(&editor, "Statement", 1)
    ));
    // Ctrl+hover underlines exactly the name.
    let main_text = editor.document().text();
    let name = find(&main_text, "{thm:main}", 1);
    let view = editor.view();
    common::pump_until(labels_common::TIMEOUT, || {
        view.is_mapped() && editor.view().height() > 0
    });
    editor
        .view()
        .scroll_to_iter(&mut editor.iter_near(name).unwrap(), 0.0, true, 0.0, 0.5);
    assert!(block_on(lemma_app::testmode::wait_for_frames(
        &f.state.window,
        2
    )));
    let rect = view.iter_location(&editor.iter_near(name + 2).unwrap());
    let (x, y) = view.buffer_to_window_coords(
        gtk::TextWindowType::Widget,
        rect.x() + rect.width() / 2,
        rect.y() + rect.height() / 2,
    );
    goto::pointer_moved(&editor, f64::from(x), f64::from(y), true);
    assert_eq!(goto::link_range(view), Some(name..name + "thm:main".len()));
    goto::pointer_moved(&editor, f64::from(x), f64::from(y), false);
    assert_eq!(goto::link_range(view), None);
}

fn image() {
    let f = fixture("goto-image");
    let editor = f.open("main.tex");
    f.wait_index();
    let opened: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let seen = opened.clone();
    goto::set_launch_hook(
        &f.state,
        Some(Box::new(move |path| {
            seen.borrow_mut().push(path.to_path_buf());
            true
        })),
    );
    editor.place_cursor_at(offset_of(&editor, "{fig}", 2));
    assert!(goto::goto_definition(&f.state));
    common::pump_until(labels_common::TIMEOUT, || !opened.borrow().is_empty());
    assert_eq!(*opened.borrow(), vec![f.path("fig.png")]);
    // The editor stays where it was.
    assert!(
        f.state
            .active_editor()
            .is_some_and(|e| Rc::ptr_eq(&e, &editor))
    );
}

// ----- hovers -----

/// What the providers answer at the first `needle` (plus `delta`) of `editor`.
fn hover_at(editor: &Editor, needle: &str, delta: usize, trigger: Trigger) -> Option<HoverContent> {
    let at = TextPoint::new(editor.document(), offset_of(editor, needle, delta)).unwrap();
    hover::query(editor.view(), &HoverCtx::new(at, trigger))
}

/// The text of a content, waiting for a pending one.
fn content_text(content: HoverContent) -> String {
    match content {
        HoverContent::Markup(m) => pango::parse_markup(&m, '\0')
            .map(|(_, text, _)| text.to_string())
            .unwrap_or(m),
        HoverContent::Widget(w) => text_of(&w),
        HoverContent::Pending(future) => match block_on(future) {
            Some(content) => content_text(content),
            None => String::new(),
        },
    }
}

/// What WP3.2's provider alone answers at the first `needle` (plus `delta`) of `editor`.
fn own_hover(
    f: &Fixture,
    editor: &Editor,
    needle: &str,
    delta: usize,
    trigger: Trigger,
) -> Option<HoverContent> {
    let at = TextPoint::new(editor.document(), offset_of(editor, needle, delta)).unwrap();
    lemma_app::labels::resolve::hover_content(&f.state, &HoverCtx::new(at, trigger))
}

fn hover_text(editor: &Editor, needle: &str, delta: usize) -> String {
    content_text(
        hover_at(editor, needle, delta, Trigger::Pointer)
            .unwrap_or_else(|| panic!("a hover at {needle:?}")),
    )
}

fn hover_reference() {
    let f = fixture("hover-reference");
    let editor = f.open("main.tex");
    f.wait_index();
    // Number, title and page from the `.aux`, then the first three lines of the environment.
    let text = hover_text(&editor, "\\ref{thm:main}", 6);
    assert!(text.contains("Theorem 1.1"), "{text}");
    assert!(text.contains("Main"), "{text}");
    assert!(
        text.contains("\\begin{theorem}[Main]\\label{thm:main}"),
        "{text}"
    );
    assert!(
        text.contains("Statement $\\cF \\cong \\Hom(A,B)$"),
        "{text}"
    );
    assert!(text.contains("\\end{theorem}"), "{text}");
    assert!(text.contains("Page 1 · main.tex:12"), "{text}");
    // An equation is numbered in parentheses; its title (the section) is left out.
    let text = hover_text(&editor, "\\eqref{eq:one}", 8);
    assert!(text.contains("Equation (1)"), "{text}");
    assert!(!text.contains("Intro"), "{text}");
    assert!(text.contains("a = b"), "{text}");
    // `\eqref` in math shows the reference (before the math preview).
    let text = hover_text(&editor, "$\\eqref{eq:two}$", 9);
    assert!(text.contains("Equation (2)"), "{text}");
    assert!(text.contains("c &= d \\label{eq:two}"), "{text}");
    assert!(text.contains("Page 2"), "{text}");
    // A section label.
    let text = hover_text(&editor, "\\cref{sec:intro", 7);
    assert!(text.contains("Section 1"), "{text}");
    // The source lines follow the live text.
    let buffer = editor.buffer();
    let mut at = editor
        .iter_near(offset_of(&editor, "Statement", 0))
        .unwrap();
    buffer.insert(&mut at, "Inserted line.\n");
    let text = hover_text(&editor, "\\ref{thm:main}", 6);
    assert!(text.contains("Inserted line."), "{text}");
    assert!(!text.contains("\\end{theorem}"), "three lines: {text}");
    // An undefined reference says so.
    let text = hover_text(&editor, "\\ref{sec:undefined}", 6);
    assert!(
        text.contains("Undefined reference ‘sec:undefined’"),
        "{text}"
    );
}

fn hover_closed_file() {
    let f = fixture("hover-closed");
    let editor = f.open("main.tex");
    f.wait_index();
    // The chapter is not open: its lines are read from disk (a pending content).
    let content = hover_at(&editor, "\\ref{sec:ch}", 6, Trigger::Pointer).unwrap();
    assert!(matches!(content, HoverContent::Pending(_)));
    let text = content_text(content);
    assert!(text.contains("Section 2"), "{text}");
    assert!(text.contains("Chapter"), "{text}");
    assert!(text.contains("\\section{Chapter}\\label{sec:ch}"), "{text}");
    assert!(text.contains("Back to Theorem~\\ref{thm:main}"), "{text}");
    assert!(text.contains("chapters/ch1.tex:1"), "{text}");
}

fn hover_names() {
    let f = fixture("hover-names");
    let editor = f.open("main.tex");
    f.wait_index();
    // A label: number, page and uses.
    let text = hover_text(&editor, "\\label{eq:one}", 8);
    assert!(text.contains("Equation (1)"), "{text}");
    assert!(text.contains("Page 1"), "{text}");
    assert!(text.contains("Referenced 4 times"), "{text}");
    let text = hover_text(&editor, "\\label{eq:two}", 8);
    assert!(text.contains("Referenced 2 times"), "{text}");
    let text = hover_text(&editor, "\\label{sec:intro}", 8);
    assert!(text.contains("Referenced once"), "{text}");
    // A citation: authors, year, title (TeX turned into text), type and place.
    let text = hover_text(&editor, "{knuth, missing}", 2);
    assert!(text.contains("Knuth, Donald E."), "{text}");
    assert!(text.contains("1984"), "{text}");
    assert!(text.contains("The TeXbook"), "{text}");
    assert!(text.contains("book · refs.bib:1"), "{text}");
    let view = editor.view().clone();
    snapshots(&f, "j-hover-cite", || {
        hover::hide(&view);
        assert!(hover::show_at(
            &editor,
            offset_of(&editor, "{knuth, missing}", 2),
            Trigger::Pointer
        ));
        common::pump_until(labels_common::TIMEOUT, || {
            hover::shown_widget(&view).is_some()
        });
    });
    hover::hide(&view);
    // A key no entry has.
    let text = hover_text(&editor, "knuth, missing}", 8);
    assert!(text.contains("No entry ‘missing’"), "{text}");
    assert!(text.contains("refs.bib"), "{text}");
    // Plain text, a LaTeX command: nothing.
    assert!(hover_at(&editor, "Statement", 2, Trigger::Pointer).is_none());
    assert!(hover_at(&editor, "\\section{Intro}", 2, Trigger::Pointer).is_none());
}

fn hover_macro() {
    let f = fixture_with("hover-macro", |c| c.preview.math_hover = true);
    let editor = f.open("main.tex");
    f.wait_index();
    // In math, the pointer leaves the formula to the math preview (WP3.2's provider does not
    // answer); the keyboard (`K`) shows the definition.
    assert!(own_hover(&f, &editor, "$\\cF \\cong", 2, Trigger::Pointer).is_none());
    let text = content_text(hover_at(&editor, "$\\cF \\cong", 2, Trigger::Keyboard).unwrap());
    assert!(text.contains("\\cF"), "{text}");
    assert!(text.contains("\\newcommand{\\cF}{\\mathcal{F}}"), "{text}");
    assert!(text.contains("main.tex:4"), "{text}");
    let view = editor.view().clone();
    snapshots(&f, "j-hover-macro", || {
        hover::hide(&view);
        assert!(hover::show_at(
            &editor,
            offset_of(&editor, "$\\cF \\cong", 2),
            Trigger::Keyboard
        ));
        common::pump_until(labels_common::TIMEOUT, || {
            hover::shown_widget(&view).is_some()
        });
    });
    hover::hide(&view);
    let text = content_text(hover_at(&editor, "\\Hom(A,B)", 2, Trigger::Keyboard).unwrap());
    assert!(text.contains("math operator"), "{text}");
    assert!(text.contains("\\DeclareMathOperator{\\Hom}{Hom}"), "{text}");
    // Outside math the pointer shows it too.
    let buffer = editor.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "Text \\cF here.\n");
    common::pump_until(labels_common::TIMEOUT, || {
        editor.document().has_line_states()
    });
    let text = hover_text(&editor, "Text \\cF", 6);
    assert!(text.contains("\\newcommand{\\cF}{\\mathcal{F}}"), "{text}");
    // With the math preview off, the pointer shows definitions in math too.
    let mut config = (*f.state.config.get()).clone();
    config.preview.math_hover = false;
    f.state.set_config(Rc::new(config));
    assert!(own_hover(&f, &editor, "$\\cF \\cong", 2, Trigger::Pointer).is_some());
}

fn hover_popover() {
    let f: Fixture = fixture("hover-popover");
    let editor = f.open("main.tex");
    f.wait_index();
    let at = offset_of(&editor, "\\ref{thm:main}", 6);
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    let view = editor.view().clone();
    common::pump_until(labels_common::TIMEOUT, || {
        hover::shown_widget(&view).is_some_and(|w| !w.is::<adw::Spinner>())
    });
    let widget = hover::shown_widget(&view).unwrap();
    assert!(text_of(&widget).contains("Theorem 1.1"));
    snapshots(&f, "j-hover-source", || {
        hover::hide(&view);
        assert!(hover::show_at(&editor, at, Trigger::Pointer));
        common::pump_until(labels_common::TIMEOUT, || {
            hover::shown_widget(&view).is_some()
        });
    });
    // The popover points at the name and never takes the focus.
    let (range, trigger) = hover::shown_range(&view).unwrap();
    assert_eq!(trigger, Trigger::Pointer);
    assert_eq!(&editor.document().text()[range], "thm:main");
    assert!(!hover::popover(&view).unwrap().has_focus());
    hover::hide(&view);
    common::pump_for(Duration::from_millis(20));
    assert!(!hover::is_shown(&view));
}

/// The crop shown in a hover's widget, if any.
fn crop_of(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.type_().name() == "LemmaCropView" {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = crop_of(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

fn hover_crop() {
    let f = proj_fixture("hover-crop");
    let editor = f.open("main.tex");
    let build = f.state.build.get().unwrap();
    block_on(build.show_existing_pdf()).expect("the fixture's PDF");
    let pdf = f.state.pdf.get().unwrap();
    common::pump_until(labels_common::TIMEOUT, || pdf.pdf().is_some());
    lemma_app::editor::completion::refresh_project(&f.state);
    let state = f.state.clone();
    common::pump_until(labels_common::TIMEOUT, || {
        !lemma_app::editor::completion::project_busy(&state)
            && state
                .workspace
                .index
                .borrow()
                .aux
                .labels
                .contains_key("eq:cocycle")
    });
    let view = editor.view().clone();
    let show = || {
        hover::hide(&view);
        let at = offset_of(&editor, "\\eqref{eq:cocycle}", 8);
        editor.place_cursor_at(at);
        editor
            .view()
            .scroll_to_mark(&editor.buffer().get_insert(), 0.0, true, 0.0, 0.8);
        assert!(block_on(lemma_app::testmode::wait_for_frames(
            &f.state.window,
            2
        )));
        assert!(hover::show_at(&editor, at, Trigger::Pointer));
        common::pump_until(labels_common::TIMEOUT, || {
            hover::shown_widget(&view).is_some_and(|w| crop_of(&w).is_some())
        });
    };
    show();
    // The crop replaces the source lines: the equation's region of page 2, at 1.4 px per pt.
    let widget = hover::shown_widget(&view).unwrap();
    let crop = crop_of(&widget).unwrap();
    assert_eq!(crop.tooltip_text().as_deref(), Some("Page 2 of the PDF"));
    let (_, natural, _, _) = crop.measure(gtk::Orientation::Vertical, -1);
    assert!((15..120).contains(&natural), "one display line: {natural}");
    let text = text_of(&widget);
    assert!(text.contains("Equation (1)"), "{text}");
    assert!(lemma_app::labels::crop_worker_running(&f.state));
    snapshots(&f, "j-hover-crop", show);
    // A label the PDF does not have yet (written after the build): the source lines stay.
    let buffer = editor.buffer();
    let mut end = editor
        .iter_near(offset_of(&editor, "\\printbibliography", 0))
        .unwrap();
    buffer.insert(
        &mut end,
        "\\begin{equation}\\label{eq:new}\nx = y\n\\end{equation}\n\\eqref{eq:new}\n",
    );
    lemma_app::editor::completion::refresh_project(&f.state);
    common::pump_until(labels_common::TIMEOUT, || {
        !lemma_app::editor::completion::project_busy(&state)
            && lemma_app::labels::scan::find_label(&state.workspace.index.borrow(), "eq:new")
                .is_some()
    });
    hover::hide(&view);
    let at = offset_of(&editor, "\\eqref{eq:new}", 8);
    editor.place_cursor_at(at);
    editor
        .view()
        .scroll_to_mark(&editor.buffer().get_insert(), 0.0, true, 0.0, 0.8);
    assert!(block_on(lemma_app::testmode::wait_for_frames(
        &f.state.window,
        2
    )));
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    common::pump_for(Duration::from_millis(600));
    let widget = hover::shown_widget(&view).unwrap();
    assert!(crop_of(&widget).is_none());
    assert!(text_of(&widget).contains("x = y"));
    // While the document differs from what was built, its lines are not the PDF's: no crop
    // for the built label either.
    hover::hide(&view);
    let at = offset_of(&editor, "\\eqref{eq:cocycle}", 8);
    editor.place_cursor_at(at);
    editor
        .view()
        .scroll_to_mark(&editor.buffer().get_insert(), 0.0, true, 0.0, 0.8);
    assert!(block_on(lemma_app::testmode::wait_for_frames(
        &f.state.window,
        2
    )));
    assert!(hover::show_at(&editor, at, Trigger::Pointer));
    common::pump_for(Duration::from_millis(600));
    let widget = hover::shown_widget(&view).unwrap();
    assert!(crop_of(&widget).is_none());
    assert!(text_of(&widget).contains("\\begin{equation}\\label{eq:cocycle}"));
    // The worker goes (after a while without crops; here at once).
    lemma_app::labels::stop_crop_worker(&f.state);
    assert!(!lemma_app::labels::crop_worker_running(&f.state));
}
