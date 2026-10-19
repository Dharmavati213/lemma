//! `--demo completion` (WP2.9a): the completion popup after `\fr` in math.
//!
//! [`run_demo`] uses the open document (FILE), or writes a small demo document into a temporary
//! directory and opens it. It finds a `\fr` in math, or puts one at the first math position
//! (inserting a display formula when the document has none; the text is not saved), places the
//! cursor right after it, opens
//! the completion popup as typing the `r` would, and resolves once the popup shows its
//! proposals, so `--snapshot` captures it (popovers are drawn into the snapshot, see
//! `testmode::snapshot_png`).

use crate::editor::{Editor, completion};
use crate::window::WindowState;
use gtk::prelude::*;
use lemma_syntax::Context;
use std::rc::Rc;
use std::time::Duration;

/// The demo document used without FILE.
const DEMO_TEX: &str = r"\documentclass{article}
\usepackage{amsmath,amssymb}
\newcommand{\Z}{\mathbb{Z}}

\begin{document}
\section{Fractions}\label{sec:fractions}

For integers $a, b \in \Z$ with $b \neq 0$ the quotient is
\[
  q = \fr
\]
\end{document}
";

/// How long to wait for the popup.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Runs the completion demo (see the module docs).
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    let editor = match state.active_editor() {
        Some(editor) => editor,
        None => open_demo_document(&state).await?,
    };
    // Typing happens in insert mode (and the status bar says so in the snapshot).
    if let Some(im) = editor.vim() {
        let none = gtk::gdk::ModifierType::empty();
        crate::editor::vim::feed_key(&im, editor.view(), gtk::gdk::Key::i, none);
    }
    place_after_fr(&editor)?;
    editor.view().grab_focus();
    // GtkSourceView hides the popup when the view scrolls under it: show it once the cursor is
    // on screen. (The window is redrawn while waiting: a view that is not allocated yet asks
    // for no frame itself, and under Broadway nothing else may; a scroll asked for before the
    // view had a size is asked for again.)
    let buffer = editor.buffer();
    let on_screen = super::wait_until(TIMEOUT, "the cursor on screen", || {
        state.window.queue_draw();
        let view = editor.view();
        let shown = cursor_on_screen(&editor);
        if !shown && view.height() > 0 {
            view.scroll_to_mark(&buffer.get_insert(), 0.1, true, 0.0, 0.3);
        }
        shown
    })
    .await;
    if let Err(e) = on_screen {
        let view = editor.view();
        let visible = view.visible_rect();
        return Err(format!(
            "{e} (view {}x{}, mapped {}, visible area y {} height {})",
            view.width(),
            view.height(),
            view.is_mapped(),
            visible.y(),
            visible.height()
        ));
    }
    completion::refresh_project(&state);
    super::wait_until(TIMEOUT, "the project index", || {
        !completion::project_busy(&state)
    })
    .await
    .map_err(|e| format!("{e} ({})", completion::project_feed_state(&state)))?;
    // Under Broadway a popup can take the window's focus, and GtkSourceView then hides the
    // completion; a few attempts get past that.
    let mut shown = false;
    for _ in 0..4 {
        if !completion::show(&state, &editor) {
            return Err("no proposals after \\fr".into());
        }
        shown = super::wait_until(Duration::from_secs(4), "the completion popup", || {
            state.window.queue_draw();
            completion::popup(&editor).is_some_and(|p| p.is_mapped() && p.width() > 0)
                && !completion::shown_items(&state, &editor).is_empty()
        })
        .await
        .is_ok();
        if shown {
            break;
        }
        state.window.present();
        editor.view().grab_focus();
    }
    if !shown {
        return Err("timed out waiting for the completion popup".into());
    }
    // The list lays out its rows on the next frames.
    if let Some(popup) = completion::popup(&editor) {
        super::wait_for_frames(&popup, 1).await;
    }
    Ok(())
}

/// Whether the cursor's line is inside the view's visible area.
fn cursor_on_screen(editor: &Editor) -> bool {
    let view = editor.view();
    let buffer = editor.buffer();
    let cursor = view.iter_location(&buffer.iter_at_mark(&buffer.get_insert()));
    let visible = view.visible_rect();
    view.is_mapped()
        && cursor.y() >= visible.y()
        && cursor.y() + cursor.height() <= visible.y() + visible.height()
}

async fn open_demo_document(state: &Rc<WindowState>) -> Result<Rc<Editor>, String> {
    let dir = glib::tmp_dir().join(format!("lemma-demo-completion-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("fractions.tex");
    std::fs::write(&path, DEMO_TEX).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    state.open_file(&path).await
}

/// Inserts `\fr` at the first position in math (or a display formula holding it at the end of
/// the document body) and puts the cursor right after it.
fn place_after_fr(editor: &Editor) -> Result<(), String> {
    let doc = editor.document();
    let buffer = editor.buffer();
    let text = doc.text();
    // A `\fr` already in math (the demo document has one).
    let existing = text.match_indices("\\fr").map(|(i, _)| i).find(|&i| {
        !text[i + 3..].starts_with(|c: char| c.is_ascii_alphabetic())
            && doc.context_at_byte(i).is_math()
    });
    if let Some(cursor) = existing.and_then(|i| doc.iter_at_byte(i + 3)) {
        buffer.place_cursor(&cursor);
        return Ok(());
    }
    let at = first_math_position(&text, |offset| doc.context_at_byte(offset));
    let (insert, cursor_after) = match at {
        Some(offset) => (offset, "\\fr ".to_owned()),
        None => {
            let end = text.rfind("\\end{document}").unwrap_or(text.len());
            (end, "\\[\n  q = \\fr\n\\]\n".to_owned())
        }
    };
    let mut iter = doc
        .iter_at_byte(insert)
        .ok_or("the math position is not in the buffer")?;
    buffer.insert(&mut iter, &cursor_after);
    let fr = insert + cursor_after.find("\\fr").unwrap_or(0) + "\\fr".len();
    let cursor = doc
        .iter_at_byte(fr)
        .ok_or("the position after \\fr is not in the buffer")?;
    buffer.place_cursor(&cursor);
    Ok(())
}

/// The first byte offset of `text` that is inside math, right after a math opener (`$`, `\[`,
/// `\(`, or a line break inside a display environment), according to `context_at`.
fn first_math_position(text: &str, context_at: impl Fn(usize) -> Context) -> Option<usize> {
    let mut candidates: Vec<usize> = Vec::new();
    for (i, _) in text.match_indices('$') {
        candidates.push(i + 1);
    }
    for opener in ["\\[", "\\("] {
        candidates.extend(text.match_indices(opener).map(|(i, _)| i + opener.len()));
    }
    // Inside display environments the first full line is math.
    for (i, _) in text.match_indices("\\begin{") {
        if let Some(nl) = text[i..].find('\n') {
            let line_end = text[i + nl + 1..]
                .find('\n')
                .map_or(text.len(), |n| i + nl + 1 + n);
            candidates.push(line_end);
        }
    }
    candidates.sort_unstable();
    candidates
        .into_iter()
        .filter(|&i| i <= text.len() && text.is_char_boundary(i))
        .find(|&i| context_at(i).is_math())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_positions() {
        let math = |ranges: &'static [(usize, usize)]| {
            move |i: usize| {
                if ranges.iter().any(|&(from, to)| (from..to).contains(&i)) {
                    Context::Math {
                        display: false,
                        env: None,
                    }
                } else {
                    Context::Text
                }
            }
        };
        // `a $x$`: right after the first `$`.
        assert_eq!(first_math_position("a $x$", math(&[(3, 4)])), Some(3));
        assert_eq!(first_math_position("no math here", math(&[])), None);
        // `\[` before a later `$`.
        assert_eq!(
            first_math_position("t \\[ x \\] $y$", math(&[(4, 7), (11, 12)])),
            Some(4)
        );
    }
}
