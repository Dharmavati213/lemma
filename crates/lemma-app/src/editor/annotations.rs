//! Inline equation numbers: a dim `(3.2)` at the end of each line
//! holding the `\label` of an equation — the bare number (`3.2`) for a theorem, a figure or
//! anything else, `§4` for a section — from the `.aux` file of the last build. They are short
//! on purpose: the line itself says what it is (`\begin{lemma}…`), and long lines leave
//! little room.
//!
//! **GtkSourceView `Annotations` are used (verified usable in 5.20):** an annotation provider
//! per view, one annotation of style `None` per labelled line. GtkSourceView draws them as
//! small pills right-aligned in the text area on the line's last display row, in the style
//! scheme's colours (dim in light and dark), with no widgets and no drawing code of ours. The
//! pill touches the text area's right edge, so the text ends with an en space to keep it off
//! the edge.
//!
//! **Updates.** [`refresh`] recomputes a view's numbers from the live text (the `\label`s of
//! the scanner's label arguments, so comments and verbatim are skipped) and the project index
//! (the `.aux` numbers and what each label labels); the label tools call it after their
//! checks: after typing stops, after every build (new numbers) and when the project index
//! changes; it reads the text only when the `.aux` numbers or the file's labels in the project
//! index (with their lines) changed. Between two refreshes the numbers follow the edits: lines
//! inserted or removed above a label move its number with it at once, without reading
//! anything. Nothing is added again when nothing changed. Off with `[editor] equation_numbers = false` (read at use time; a configuration
//! reload refreshes every view).
//!
//! Hooks: [`attach`] / [`detach`] per editor tab (the view is held weakly; disconnect in
//! `detach`).

use super::Editor;
use crate::document::ObserverId;
use crate::labels::scan;
use crate::window::WindowState;
use lemma_syntax::{FileIndex, Label, ProjectIndex};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::{Rc, Weak};

/// What follows each number: keeps the pill's text off the text area's edge.
const PADDING: &str = "\u{2002}";

/// The numbers of one view.
struct Numbers {
    view: glib::WeakRef<super::TexView>,
    editor: Weak<Editor>,
    provider: sourceview5::AnnotationProvider,
    /// What is shown: (0-based line, text), by line.
    shown: RefCell<Vec<(u32, String)>>,
    lines_changed: RefCell<Option<ObserverId>>,
    /// The next [`refresh`] reads the text whatever it was computed from (a new view).
    dirty: Cell<bool>,
    /// What the numbers were computed from — the `.aux` numbers, and the file's labels with
    /// their lines in the project index (which changes when a label is added, removed, renamed
    /// or moves) — so a [`refresh`] with nothing new does not read the text.
    source: Cell<u64>,
}

thread_local! {
    static VIEWS: RefCell<Vec<Rc<Numbers>>> = const { RefCell::new(Vec::new()) };
}

fn lookup(editor: &Editor) -> Option<Rc<Numbers>> {
    VIEWS.with_borrow(|views| {
        views
            .iter()
            .find(|n| n.view.upgrade().as_ref() == Some(editor.view()))
            .cloned()
    })
}

/// Called for every new editor tab: LaTeX documents get the numbers.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if !super::completion::is_latex(editor.document().path()) || lookup(editor).is_some() {
        return;
    }
    let provider = sourceview5::AnnotationProvider::new();
    editor.view().annotations().add_provider(&provider);
    let numbers = Rc::new(Numbers {
        view: editor.view().downgrade(),
        editor: Rc::downgrade(editor),
        provider,
        shown: RefCell::new(Vec::new()),
        lines_changed: RefCell::new(None),
        dirty: Cell::new(true),
        source: Cell::new(0),
    });
    // Inside the change, nothing is read: the numbers only move.
    let weak = Rc::downgrade(&numbers);
    let id = editor
        .document()
        .connect_lines_changed(move |first, removed, added| {
            if let Some(numbers) = weak.upgrade() {
                follow(&numbers, first, removed, added);
            }
        });
    numbers.lines_changed.replace(Some(id));
    VIEWS.with_borrow_mut(|views| {
        views.retain(|n| n.view.upgrade().is_some());
        views.push(numbers);
    });
    refresh(state, editor);
}

/// Called when an editor tab closes.
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(numbers) = lookup(editor) else {
        return;
    };
    VIEWS.with_borrow_mut(|views| views.retain(|n| !Rc::ptr_eq(n, &numbers)));
    if let Some(id) = numbers.lines_changed.take() {
        editor.document().disconnect(id);
    }
    numbers.provider.remove_all();
    editor
        .view()
        .annotations()
        .remove_provider(&numbers.provider);
}

/// Moves the numbers with lines `first..first + removed` replaced by `added` lines (inside the
/// buffer's change: only the annotations change).
fn follow(numbers: &Numbers, first: u32, removed: u32, added: u32) {
    if removed == added {
        return;
    }
    let moved: Vec<(u32, String)> = numbers
        .shown
        .borrow()
        .iter()
        .filter_map(|(line, text)| {
            let line = *line;
            if line < first {
                Some((line, text.clone()))
            } else if line < first + removed {
                // A changed line keeps its number while it still exists.
                (line - first < added).then(|| (line, text.clone()))
            } else {
                Some((line + added - removed, text.clone()))
            }
        })
        .collect();
    show(numbers, moved);
}

/// Replaces the annotations with `numbers` (nothing happens when they are already shown).
fn show(numbers: &Numbers, wanted: Vec<(u32, String)>) {
    if *numbers.shown.borrow() == wanted {
        return;
    }
    numbers.provider.remove_all();
    for (line, text) in &wanted {
        let Ok(line) = i32::try_from(*line) else {
            continue;
        };
        numbers
            .provider
            .add_annotation(&sourceview5::Annotation::new(
                Some(&format!("{text}{PADDING}")),
                None::<gio::Icon>,
                line,
                sourceview5::AnnotationStyle::None,
            ));
    }
    numbers.shown.replace(wanted);
}

/// Recomputes `editor`'s numbers from its live text and the project index when either changed
/// in a way that matters (see the module docs).
pub fn refresh(state: &WindowState, editor: &Editor) {
    let Some(numbers) = lookup(editor) else {
        return;
    };
    if numbers.editor.upgrade().is_none() {
        return;
    }
    if !state.config.editor.equation_numbers {
        show(&numbers, Vec::new());
        numbers.source.set(0);
        return;
    }
    let doc = editor.document();
    if !doc.has_line_states() {
        return;
    }
    let index = state.workspace.index.borrow();
    if index.aux.labels.is_empty() {
        show(&numbers, Vec::new());
        numbers.source.set(0);
        return;
    }
    // Nothing new since the last time: the numbers followed the edits already.
    let path = doc.path();
    let file = index
        .files
        .get(path)
        .or_else(|| index.files.get(&crate::workspace::normalize(path)))
        .cloned();
    let source = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        index.aux.labels.hash(&mut hasher);
        if let Some(file) = &file {
            for label in &file.labels {
                (&label.name, &label.env, label.line).hash(&mut hasher);
            }
        }
        hasher.finish().max(1)
    };
    if !numbers.dirty.get() && numbers.source.get() == source {
        return;
    }
    numbers.dirty.set(false);
    numbers.source.set(source);
    // The labels of the live text, by line.
    let labels: Vec<(u32, String)> = doc.with_text(|text| {
        let states = doc.line_states.borrow();
        let Some(states) = states.as_ref() else {
            return Vec::new();
        };
        let line_index = doc.line_index();
        let mut tokens = Vec::new();
        let mut out = Vec::new();
        let mut last_line = None;
        for (at, _) in text.match_indices("\\label") {
            let line = line_index.line_of(at);
            if last_line == Some(line) {
                continue;
            }
            last_line = Some(line);
            let range = line_index.line_range(line);
            let line_text = &text[range];
            tokens.clear();
            states.tokens(line, line_text, &mut tokens);
            for token in tokens
                .iter()
                .filter(|t| t.kind == lemma_syntax::TokenKind::LabelArg)
            {
                for (_, name) in scan::token_items(line_text, token) {
                    out.push((line, name.to_owned()));
                }
            }
        }
        out
    });
    let wanted = numbers_for(&index, file.as_deref(), labels);
    drop(index);
    show(&numbers, wanted);
}

/// The numbers shown for `labels`, the `\label`s of a file's live text ((0-based line, name) in
/// line order), with `file` its entry in the project index: the annotation of each label that
/// the `.aux` numbers, from what it labels (from `file`, else from wherever the project has it),
/// those of one line joined with " · ". Each lookup goes through maps built once, so a file
/// with thousands of labels costs no more per label than one with a few.
pub fn numbers_for(
    index: &ProjectIndex,
    file: Option<&FileIndex>,
    labels: Vec<(u32, String)>,
) -> Vec<(u32, String)> {
    // The file's labels by name (the first of a name, as a search finds it); the project's only
    // for a label that is not in the file.
    let mut own: HashMap<&str, &Label> = HashMap::new();
    for label in file.iter().flat_map(|f| &f.labels) {
        own.entry(label.name.as_str()).or_insert(label);
    }
    let mut project = None;
    let namer = scan::LabelNamer::new(index);
    let mut wanted: Vec<(u32, String)> = Vec::new();
    for (line, name) in labels {
        if !index.aux.labels.contains_key(&name) {
            continue;
        }
        let found = match (file, own.get(name.as_str())) {
            (Some(file), Some(label)) => Some((file, *label)),
            _ => project
                .get_or_insert_with(|| scan::labels_by_name(index))
                .get(name.as_str())
                .map(|&(_, file, label)| (file, label)),
        };
        let Some((file_index, label)) = found else {
            continue;
        };
        let Some(text) = namer.info(file_index, label).annotation() else {
            continue;
        };
        match wanted.last_mut() {
            Some((last, shown)) if *last == line => {
                shown.push_str(" · ");
                shown.push_str(&text);
            }
            _ => wanted.push((line, text)),
        }
    }
    wanted
}

/// The numbers `editor` shows: (0-based line, text) by line (tests).
pub fn shown(editor: &Editor) -> Vec<(u32, String)> {
    lookup(editor).map_or_else(Vec::new, |n| n.shown.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_syntax::index_file;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn numbers_of_labels_by_line() {
        let main = "\\documentclass{article}\n\\newtheorem{theorem}{Theorem}[section]\n\
                    \\begin{document}\n\\section{Intro}\\label{sec:intro}\n\
                    \\begin{theorem}\\label{thm:main}\nT\n\\end{theorem}\n\
                    \\begin{align}\na \\label{eq:a}\\\\ b \\label{eq:b}\n\\end{align}\n\
                    \\label{none}\n\\input{ch}\n\\end{document}\n";
        let mut index = ProjectIndex::default();
        index.update_file(PathBuf::from("/p/main.tex"), Arc::new(index_file(main)));
        index.update_file(
            PathBuf::from("/p/ch.tex"),
            Arc::new(index_file("\\section{Chapter}\\label{sec:ch}\n")),
        );
        index.aux = lemma_syntax::parse_aux(
            "\\newlabel{sec:intro}{{1}{1}}\n\\newlabel{thm:main}{{1.1}{1}}\n\
             \\newlabel{eq:a}{{1}{1}}\n\\newlabel{eq:b}{{2}{1}}\n\\newlabel{sec:ch}{{2}{2}}\n",
        );
        let file = index.files[&PathBuf::from("/p/main.tex")].clone();
        // The live text: the labels of the index plus one only the project knows (`sec:ch`
        // moved here since the last indexing) and one the build did not number.
        let labels = vec![
            (3, "sec:intro".to_owned()),
            (4, "thm:main".to_owned()),
            (8, "eq:a".to_owned()),
            (8, "eq:b".to_owned()),
            (10, "none".to_owned()),
            (11, "sec:ch".to_owned()),
        ];
        assert_eq!(
            numbers_for(&index, Some(&file), labels.clone()),
            vec![
                (3, "§1".to_owned()),
                (4, "1.1".to_owned()),
                (8, "(1) · (2)".to_owned()),
                (11, "§2".to_owned()),
            ]
        );
        // Without the file's entry, everything comes from the project.
        assert_eq!(numbers_for(&index, None, labels).len(), 4);
    }
}
