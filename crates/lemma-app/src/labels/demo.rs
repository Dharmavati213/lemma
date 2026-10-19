//! `--demo hover-ref` (for `j-hover-ref.png`; `LEMMA_DEMO_HOVER=0` for `j-eqnumbers.png`).

use crate::editor::{Trigger, annotations, completion, hover};
use crate::testmode;
use crate::window::WindowState;
use gtk::prelude::*;
use std::rc::Rc;
use std::time::Duration;

/// The reference the demo hovers: the first of these the document has.
const REFERENCES: &[&str] = &["\\eqref{", "\\ref{", "\\cref{", "\\autoref{"];
/// How long the demo waits for each step.
const TIMEOUT: Duration = Duration::from_secs(20);

/// `--demo hover-ref` (run with FILE = a project document with a built `.aux`, e.g. a copy of
/// `tests/fixtures/proj/main.tex`): scrolls to the first `\eqref` (else `\ref`, …), waits for
/// the project index, the `.aux` numbers and the equation numbers, then shows the hover of
/// that reference as the pointer would and resolves once it is drawn (with its PDF crop when
/// the PDF has one). With `LEMMA_DEMO_HOVER=0` no hover is shown: the same place with its
/// equation numbers.
pub async fn run_hover_ref_demo(state: Rc<WindowState>) -> Result<(), String> {
    let with_hover = std::env::var("LEMMA_DEMO_HOVER").map_or(true, |v| v != "0");
    let editor = state
        .active_editor()
        .ok_or("--demo hover-ref needs a LaTeX file to open")?;
    let doc = editor.document().clone();
    testmode::wait_until(TIMEOUT, "the scanner", || doc.has_line_states()).await?;
    let (offset, name) = doc
        .with_text(|text| {
            REFERENCES.iter().find_map(|r| {
                let at = text.find(r)? + r.len();
                let end = text[at..].find(['}', ','])? + at;
                Some((at, text[at..end].trim().to_owned()))
            })
        })
        .ok_or("the document has no \\eqref, \\ref, \\cref or \\autoref")?;
    // The project index (the chapter files, the bibliography and the `.aux` numbers).
    completion::refresh_project(&state);
    testmode::wait_until(TIMEOUT, "the project index", || {
        !completion::project_busy(&state)
            && state
                .workspace
                .index
                .borrow()
                .aux
                .labels
                .contains_key(&name)
            && crate::labels::scan::find_label(&state.workspace.index.borrow(), &name).is_some()
    })
    .await?;
    super::check_now(&state);
    // With the hover: the reference near the bottom of the view, so that its popover (above
    // it) leaves the labelled equation in sight. Without: the labelled equation near the top,
    // with the labels after it.
    let buffer = editor.buffer();
    let (place, yalign) = if with_hover {
        (offset, 0.88)
    } else {
        let label = super::label_in_document(&doc, &name, None).unwrap_or(offset);
        (label, 0.25)
    };
    editor.place_cursor_at(place);
    editor
        .view()
        .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, yalign);
    testmode::wait_until(TIMEOUT, "the equation numbers", || {
        !annotations::shown(&editor).is_empty()
    })
    .await?;
    // Laid out and scrolled (again once the lines above have their real heights): a hover
    // hides when the view scrolls.
    let adjustment = editor.view().vadjustment();
    for _ in 0..2 {
        let mut last = None;
        for _ in 0..20 {
            if !testmode::wait_for_frames(&state.window, 1).await {
                return Err("the window drew no frame".into());
            }
            let value = adjustment.as_ref().map(|a| a.value());
            if value == last {
                break;
            }
            last = value;
        }
        editor
            .view()
            .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, yalign);
    }
    if !with_hover {
        return Ok(());
    }
    let view = editor.view().clone();
    // On the label name, as a pointer resting there.
    if !hover::show_at(&editor, offset + 1, Trigger::Pointer) {
        return Err(format!("no hover for the reference to {name}"));
    }
    testmode::wait_until(TIMEOUT, "the hover", || {
        hover::shown_widget(&view).is_some_and(|w| !w.is::<adw::Spinner>())
    })
    .await?;
    // The PDF crop, when there is one, replaces the source lines.
    let _ = testmode::wait_until(Duration::from_secs(5), "the PDF crop", || {
        super::crop::settled(&view)
    })
    .await;
    if !testmode::wait_for_frames(&state.window, 1).await {
        return Err("the window drew no frame".into());
    }
    Ok(())
}
