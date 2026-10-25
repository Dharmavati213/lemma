//! Editing commands: Toggle Comment, Bold, Italic and Emphasize.
//!
//! - **Toggle Comment** (`win.toggle-comment`, Ctrl+/, palette): comments out the lines of the
//!   selection, or the cursor's line, with `% ` after the indentation they share; when every
//!   one of them that is not blank is a comment already, it takes one `%` (and one space after
//!   it) from each instead. Blank lines are left alone, and `\%` (a percent sign) is not a
//!   comment. A selection that ends at the start of a line does not take that line. LaTeX and
//!   BibTeX documents only.
//! - **Bold**, **Italic** and **Emphasize** (`win.bold` Ctrl+B, `win.italic` Ctrl+I,
//!   `win.emphasize` from the palette): wrap the selection in `\textbf{…}`, `\textit{…}` or
//!   `\emph{…}` (in math, Bold in `\mathbf{…}` and the other two in `\mathit{…}`), and the
//!   wrapped text stays selected; without a selection they insert the command with the cursor
//!   between its braces. When the selection is such a command already, or is the whole
//!   argument of one (an empty one around the cursor too), they remove the command instead, so
//!   a second press undoes the first. A selection with unmatched braces is refused, and so is
//!   one that ends in a comment or starts in one and leaves it (the `}` would be commented out,
//!   or the command would be). LaTeX documents only.
//!
//! Ctrl+B and Ctrl+I are Vim keys (page up, jump forward), so Bold and Italic are disabled while
//! Vim is on (they follow `win.toggle-vim`) and the keys reach Vim, as Ctrl+F does outside the
//! PDF (ARCHITECTURE §11); the palette shows them greyed out then. Ctrl+/ is no Vim key; it
//! takes GtkTextView's second Select All key (Ctrl+A stays) in the editor. In another text
//! field (the agent's prompt, a search or inline-edit entry) the keys stay the field's, so
//! Ctrl+/ selects its text there and no key edits the document out of sight.
//!
//! Every command works on the active editor and is one undo step (`Document::apply_edits`,
//! origin `User`). The text transformations are pure functions ([`comment_edits`],
//! [`style_edits`]) with table tests; `tests/gtk_envtools.rs` runs the actions in a window,
//! `tests/gtk_shell_m3.rs` presses their keys on real key events with Vim on and off, and in
//! the agent's prompt.
//!
//! Hooks: [`install`] once per window. Test API: [`toggle_comment_in`], [`style_in`].

use super::envtools::indent_of;
use super::typing::Mode;
use super::{Editor, TexView, completion};
use crate::app::{self, ShortcutGroup};
use crate::document::Document;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;

/// The actions (without `win.`), their labels and default accelerators.
const ACTIONS: &[(&str, &str, &[&str])] = &[
    ("toggle-comment", "Toggle Comment", &["<Control>slash"]),
    ("bold", "Bold", &["<Control>b"]),
    ("italic", "Italic", &["<Control>i"]),
    ("emphasize", "Emphasize", &[]),
];

/// The actions whose default keys are Vim keys: disabled while Vim is on.
const VIM_KEY_ACTIONS: &[&str] = &["bold", "italic"];

/// Why they are disabled then, for the palette.
const VIM_ON: &str = "Turn off Vim mode first";

const UNMATCHED: &str = "The selection has unmatched braces";
const ENDS_IN_COMMENT: &str = "The selection ends in a comment";
const STARTS_IN_COMMENT: &str = "The selection starts in a comment";

// ───────────── Pure text functions ─────────────

/// The edits (descending) that toggle the comments of `lines`: each line's document offset and
/// its text without the line break, in document order (see the module docs). Empty when every
/// line is blank.
pub fn comment_edits(lines: &[(usize, &str)]) -> Vec<TextEdit> {
    let filled: Vec<(usize, &str)> = lines
        .iter()
        .copied()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    let Some(&(_, first)) = filled.first() else {
        return Vec::new();
    };
    let commented = filled
        .iter()
        .all(|(_, line)| line[indent_of(line).len()..].starts_with('%'));
    let mut edits: Vec<TextEdit> = if commented {
        filled
            .iter()
            .map(|&(start, line)| {
                let at = start + indent_of(line).len();
                let space = line[at - start + 1..].starts_with(' ');
                TextEdit::delete(at..at + 1 + usize::from(space))
            })
            .collect()
    } else {
        // The indentation all of them start with (spaces and tabs: any prefix is a boundary).
        let shared = filled.iter().fold(indent_of(first), |shared, (_, line)| {
            let common = shared
                .bytes()
                .zip(indent_of(line).bytes())
                .take_while(|(a, b)| a == b)
                .count();
            &shared[..common]
        });
        filled
            .iter()
            .map(|&(start, _)| TextEdit::insert(start + shared.len(), "% "))
            .collect()
    };
    edits.reverse();
    edits
}

/// What Bold, Italic and Emphasize apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Bold,
    Italic,
    Emphasis,
}

impl Style {
    /// The command that applies the style in text (`math` false) or in math.
    pub fn command(self, math: bool) -> &'static str {
        match (self, math) {
            (Self::Bold, false) => "\\textbf",
            (Self::Bold, true) => "\\mathbf",
            (Self::Italic, false) => "\\textit",
            (Self::Emphasis, false) => "\\emph",
            (Self::Italic | Self::Emphasis, true) => "\\mathit",
        }
    }
}

/// Whether `s` ends with a backslash that is not itself escaped.
fn ends_with_unescaped_backslash(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Whether the braces of `s` match: escaped ones (`\{`) and those in comments do not count.
fn balanced(s: &str) -> bool {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'%' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    depth == 0
}

/// Whether `line` has a comment (an unescaped `%`).
fn has_comment(line: &str) -> bool {
    line.match_indices('%')
        .any(|(at, _)| !ends_with_unescaped_backslash(&line[..at]))
}

/// What Bold, Italic or Emphasize (`style`) does with the selection `selected`, which starts at
/// document offset `start`, after `before` (its first line up to it), and is followed by
/// `after` (the rest of its last line); `mode` is the mode at its start (math commands in
/// math). Returns the edits (descending) and the selection afterwards (the cursor when it is
/// empty), or why the selection cannot be wrapped (see the module docs).
pub fn style_edits(
    style: Style,
    mode: Mode,
    before: &str,
    selected: &str,
    after: &str,
    start: usize,
) -> Result<(Vec<TextEdit>, Range<usize>), &'static str> {
    let end = start + selected.len();
    if !balanced(selected) {
        return Err(UNMATCHED);
    }
    let math = mode == Mode::Math;
    // Either form unwraps, so a press in math undoes one in text and back.
    for command in [style.command(math), style.command(!math)] {
        let open = command.len() + 1;
        // The selection is the whole command: `\textbf{word}` → `word`.
        if let Some(inner) = selected
            .strip_prefix(command)
            .and_then(|s| s.strip_prefix('{'))
            .and_then(|s| s.strip_suffix('}'))
            && balanced(inner)
            && !ends_with_unescaped_backslash(before)
        {
            let edits = vec![
                TextEdit::delete(end - 1..end),
                TextEdit::delete(start..start + open),
            ];
            return Ok((edits, start..start + inner.len()));
        }
        // The selection is its whole argument: `\textbf{` before it and `}` after it.
        if let Some(head) = before
            .strip_suffix('{')
            .and_then(|b| b.strip_suffix(command))
            && !ends_with_unescaped_backslash(head)
            && after.starts_with('}')
        {
            let edits = vec![
                TextEdit::delete(end..end + 1),
                TextEdit::delete(start - open..start),
            ];
            return Ok((edits, start - open..end - open));
        }
    }
    // The command goes on the selection's first line and the `}` on its last: either both are
    // in a comment or neither is.
    let last_line = match selected.rfind('\n') {
        Some(at) => selected[at + 1..].to_owned(),
        None => format!("{before}{selected}"),
    };
    match (has_comment(before), has_comment(&last_line)) {
        (false, true) => return Err(ENDS_IN_COMMENT),
        (true, false) => return Err(STARTS_IN_COMMENT),
        _ => {}
    }
    let command = style.command(math);
    let open = command.len() + 1;
    if selected.is_empty() {
        let edits = vec![TextEdit::insert(start, format!("{command}{{}}"))];
        return Ok((edits, start + open..start + open));
    }
    let edits = vec![
        TextEdit::insert(end, "}"),
        TextEdit::insert(start, format!("{command}{{")),
    ];
    Ok((edits, start + open..end + open))
}

// ───────────── Commands ─────────────

/// Adds the actions, their default accelerators and their palette descriptions, keeps Bold
/// and Italic disabled while Vim is on, and leaves the keys to other text fields.
pub fn install(state: &Rc<WindowState>) {
    for &(name, label, _) in ACTIONS {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                run(&state, name);
            }
        });
        state.window.add_action(&action);
        let unavailable = VIM_KEY_ACTIONS.contains(&name).then_some(VIM_ON);
        app::describe_action(
            &format!("win.{name}"),
            label,
            ShortcutGroup::Editing,
            unavailable,
        );
    }
    let accels: Vec<(String, &[&str])> = ACTIONS
        .iter()
        .filter(|(_, _, keys)| !keys.is_empty())
        .map(|&(name, _, keys)| (format!("win.{name}"), keys))
        .collect();
    let accels: Vec<(&str, &[&str])> = accels.iter().map(|(n, k)| (n.as_str(), *k)).collect();
    app::set_default_accels(state, &accels);
    follow_vim(state);
    if let Some(action) = state.window.lookup_action("toggle-vim") {
        let weak = Rc::downgrade(state);
        action.connect_notify_local(Some("state"), move |_, _| {
            if let Some(state) = weak.upgrade() {
                follow_vim(&state);
            }
        });
    }
    install_text_field_keys(state);
}

/// The capture-phase controller on the window that gives the commands' keys to the focused
/// text field when it is not an editor (the agent's prompt, an entry), as the field would get
/// them without these accelerators: the commands act on the active editor, which would change
/// out of sight. Added after the window was given to the application, so it runs before the
/// accelerators' controller (as `keys::install_vim_accelerator_keys`).
fn install_text_field_keys(state: &Rc<WindowState>) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_name(Some("lemma-editing-text-field-keys"));
    let weak = Rc::downgrade(state);
    controller.connect_key_pressed(move |controller, keyval, _, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let Some(field) = other_text_field(&state) else {
            return glib::Propagation::Proceed;
        };
        // A disabled action lets its key through by itself.
        let ours = ACTIONS.iter().any(|&(name, _, _)| {
            state
                .window
                .lookup_action(name)
                .is_some_and(|a| a.is_enabled())
                && crate::palette::accel_matches(&state, &format!("win.{name}"), keyval, modifiers)
        });
        if !ours {
            return glib::Propagation::Proceed;
        }
        controller.forward(&field);
        glib::Propagation::Stop
    });
    state.window.add_controller(controller);
}

/// The widget with the keyboard focus when it is a text field other than an editor.
fn other_text_field(state: &WindowState) -> Option<gtk::Widget> {
    let focus = gtk::prelude::GtkWindowExt::focus(&state.window)?;
    let field = (focus.is::<gtk::Text>() || focus.is::<gtk::TextView>()) && !focus.is::<TexView>();
    field.then_some(focus)
}

/// Enables the actions with Vim keys while Vim is off, and disables them while it is on, so
/// that their keys reach Vim.
fn follow_vim(state: &WindowState) {
    for name in VIM_KEY_ACTIONS {
        if let Some(action) = state
            .window
            .lookup_action(name)
            .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        {
            action.set_enabled(!state.vim_enabled());
        }
    }
}

fn run(state: &Rc<WindowState>, action: &str) {
    let Some(editor) = state.active_editor() else {
        return;
    };
    let result = match action {
        "toggle-comment" => toggle_comment_in(&editor),
        "bold" => style_in(&editor, Style::Bold),
        "italic" => style_in(&editor, Style::Italic),
        "emphasize" => style_in(&editor, Style::Emphasis),
        _ => Ok(()),
    };
    if let Err(message) = result {
        state.toast(&message);
    }
}

/// Whether `path` is a document whose comments start with `%` (LaTeX and BibTeX).
fn has_percent_comments(path: &Path) -> bool {
    crate::document::language_for(path).is_some_and(|l| l.id() == "latex" || l.id() == "bibtex")
}

/// The selection of `editor` (the cursor twice without one), and whether the cursor is at its
/// start.
fn selection(editor: &Editor) -> (gtk::TextIter, gtk::TextIter, bool) {
    let buffer = editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    match buffer.selection_bounds() {
        Some((start, end)) => (start, end, cursor == start),
        None => (cursor, cursor, false),
    }
}

/// The typing helpers' mode at `offset` of `doc`.
fn mode_at(doc: &Document, offset: usize) -> Mode {
    let start = {
        let index = doc.line_index();
        index.line_start(index.line_of(offset))
    };
    Mode::of(&doc.context_at_byte(offset), |at| {
        doc.context_at_byte(start + at as usize)
    })
}

fn apply(editor: &Editor, edits: &[TextEdit]) -> Result<(), String> {
    editor
        .document()
        .apply_edits(edits, Origin::User)
        .map_err(|e| format!("Could not change the text: {e}"))
}

/// `win.toggle-comment` in `editor`.
pub fn toggle_comment_in(editor: &Editor) -> Result<(), String> {
    let doc = editor.document();
    if !has_percent_comments(doc.path()) {
        return Err("Toggle Comment works in LaTeX and BibTeX documents".into());
    }
    let (a, b, _) = selection(editor);
    let (start, end) = (doc.byte_offset(&a), doc.byte_offset(&b));
    let starts: Vec<(u32, usize)> = {
        let index = doc.line_index();
        let first = index.line_of(start);
        let mut last = index.line_of(end);
        if last > first && end == index.line_start(last) {
            last -= 1;
        }
        (first..=last).map(|l| (l, index.line_start(l))).collect()
    };
    let lines: Vec<(usize, String)> = starts
        .into_iter()
        .filter_map(|(line, start)| Some((start, doc.line_text(line)?)))
        .collect();
    let lines: Vec<(usize, &str)> = lines.iter().map(|(s, l)| (*s, l.as_str())).collect();
    let edits = comment_edits(&lines);
    if edits.is_empty() {
        return Ok(());
    }
    apply(editor, &edits)?;
    editor
        .view()
        .scroll_mark_onscreen(&editor.buffer().get_insert());
    Ok(())
}

/// `win.bold`, `win.italic` or `win.emphasize` (`style`) in `editor`.
pub fn style_in(editor: &Editor, style: Style) -> Result<(), String> {
    let doc = editor.document();
    if !completion::is_latex(doc.path()) {
        return Err("Bold, Italic and Emphasize work in LaTeX documents".into());
    }
    let buffer = editor.buffer();
    let (a, b, cursor_at_start) = selection(editor);
    let start = doc.byte_offset(&a);
    let mut line_start = a;
    line_start.set_line_offset(0);
    let mut line_end = b;
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    let before = buffer.slice(&line_start, &a, true);
    let selected = buffer.slice(&a, &b, true);
    let after = buffer.slice(&b, &line_end, true);
    let (edits, range) = style_edits(
        style,
        mode_at(doc, start),
        &before,
        &selected,
        &after,
        start,
    )?;
    apply(editor, &edits)?;
    if let (Some(from), Some(to)) = (doc.iter_at_byte(range.start), doc.iter_at_byte(range.end)) {
        // The cursor stays at the end of the selection it was at.
        if cursor_at_start {
            buffer.select_range(&from, &to);
        } else {
            buffer.select_range(&to, &from);
        }
    }
    editor.view().scroll_mark_onscreen(&buffer.get_insert());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text`'s lines with their offsets.
    fn lines(text: &str) -> Vec<(usize, &str)> {
        let mut at = 0;
        text.split('\n')
            .map(|line| {
                let start = at;
                at += line.len() + 1;
                (start, line)
            })
            .collect()
    }

    fn apply_all(text: &str, edits: &[TextEdit]) -> String {
        assert!(
            edits.windows(2).all(|w| w[1].range.end <= w[0].range.start),
            "descending: {edits:?}"
        );
        let mut s = text.to_owned();
        for e in edits {
            s.replace_range(e.range.clone(), &e.text);
        }
        s
    }

    fn toggled(text: &str) -> String {
        apply_all(text, &comment_edits(&lines(text)))
    }

    #[test]
    fn comments_go_after_the_shared_indentation() {
        let cases: &[(&str, &str)] = &[
            ("a", "% a"),
            ("a\n  b\n\n  c", "% a\n%   b\n\n%   c"),
            ("  a\n    b", "  % a\n  %   b"),
            ("\tx\n\t\ty", "\t% x\n\t% \ty"),
            // Different blanks share nothing.
            ("  a\n\tb", "%   a\n% \tb"),
            // Some lines are comments, some are not: all become comments.
            ("% a\nb", "% % a\n% b"),
            // `\%` is a percent sign, not a comment.
            ("\\% of all", "% \\% of all"),
            // A comment after text does not make a commented line.
            ("x % note", "% x % note"),
            // CRLF lines keep their `\r`.
            ("a\r\n\r\nb\r", "% a\r\n\r\n% b\r"),
        ];
        for &(text, want) in cases {
            assert_eq!(toggled(text), want, "{text:?}");
        }
    }

    #[test]
    fn uncommenting_takes_one_percent_and_one_space() {
        let cases: &[(&str, &str)] = &[
            ("% a", "a"),
            ("%a", "a"),
            ("%  a", " a"),
            ("%% a", "% a"),
            ("  % a\n  %   b\n\n%c", "  a\n    b\n\nc"),
            ("% a\r\n%\r", "a\r\n\r"),
        ];
        for &(text, want) in cases {
            assert_eq!(toggled(text), want, "{text:?}");
        }
    }

    #[test]
    fn toggling_twice_gives_the_text_back() {
        for text in [
            "a",
            "\\begin{figure}\n  \\centering\n\n  \\includegraphics{x}\n\\end{figure}",
            "  \\usepackage{x} % for y\n\\% sign",
            "% a\nb\n  %c",
            "\tx\n  y",
        ] {
            assert_eq!(toggled(&toggled(text)), text, "{text:?}");
        }
    }

    #[test]
    fn blank_lines_alone_change_nothing() {
        assert!(comment_edits(&lines("")).is_empty());
        assert!(comment_edits(&lines("  \n\t\n")).is_empty());
    }

    /// Bold, Italic or Emphasize on `text`, whose selection is between the two `|` (or at the
    /// one `|`): the text after it with the new selection marked the same way.
    fn styled(style: Style, text: &str) -> Result<String, &'static str> {
        let marks: Vec<usize> = text.match_indices('|').map(|(i, _)| i).collect();
        let (start, end) = match marks[..] {
            [at] => (at, at),
            [a, b] => (a, b - 1),
            _ => panic!("one or two | in {text:?}"),
        };
        let plain = text.replace('|', "");
        let line_start = plain[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = plain[end..].find('\n').map_or(plain.len(), |i| end + i);
        let mode = if plain[..start].matches('$').count() % 2 == 1 {
            Mode::Math
        } else if plain[line_start..start].contains('%') {
            Mode::Off
        } else {
            Mode::Text
        };
        let (edits, range) = style_edits(
            style,
            mode,
            &plain[line_start..start],
            &plain[start..end],
            &plain[end..line_end],
            start,
        )?;
        let mut out = apply_all(&plain, &edits);
        if range.is_empty() {
            out.insert(range.start, '|');
        } else {
            out.insert(range.end, '|');
            out.insert(range.start, '|');
        }
        Ok(out)
    }

    #[test]
    fn styles_wrap_and_unwrap() {
        use Style::*;
        let cases: &[(Style, &str, &str)] = &[
            // Wrapping keeps the text selected.
            (Bold, "a |word| b", "a \\textbf{|word|} b"),
            (Italic, "a |word| b", "a \\textit{|word|} b"),
            (Emphasis, "a |word| b", "a \\emph{|word|} b"),
            (Bold, "|two\nlines|", "\\textbf{|two\nlines|}"),
            // Without a selection: the command, the cursor inside.
            (Bold, "a |b", "a \\textbf{|}b"),
            (Emphasis, "|", "\\emph{|}"),
            // The selection is the command, or its argument: the command goes.
            (Bold, "a |\\textbf{word}| b", "a |word| b"),
            (Bold, "a \\textbf{|word|} b", "a |word| b"),
            (Emphasis, "\\emph{|}", "|"),
            (Italic, "x \\textit{|a {b} c|}", "x |a {b} c|"),
            (Bold, "|\\textbf{a\\}}|", "|a\\}|"),
            // In math: `\mathbf`, `\mathit`; either form unwraps anywhere.
            (Bold, "$|x|$", "$\\mathbf{|x|}$"),
            (Italic, "$|x|$", "$\\mathit{|x|}$"),
            (Emphasis, "$|x|$", "$\\mathit{|x|}$"),
            (Bold, "$|\\textbf{x}|$", "$|x|$"),
            (Bold, "|\\mathbf{x}|", "|x|"),
            // Not exactly one command: wrapped again.
            (
                Bold,
                "|\\textbf{a} and \\textbf{b}|",
                "\\textbf{|\\textbf{a} and \\textbf{b}|}",
            ),
            (Bold, "\\textbf{a|b|}", "\\textbf{a\\textbf{|b|}}"),
            (Bold, "\\textbf{|a|b}", "\\textbf{\\textbf{|a|}b}"),
            // `\\textbf{…}` is a line break and text, not the command.
            (Bold, "\\\\textbf{|x|}", "\\\\textbf{\\textbf{|x|}}"),
            // Escaped braces and braces in a comment on an earlier line do not count.
            (Bold, "|a \\{ b|", "\\textbf{|a \\{ b|}"),
            (Bold, "|a % {\nb|", "\\textbf{|a % {\nb|}"),
        ];
        for &(style, text, want) in cases {
            assert_eq!(
                styled(style, text).as_deref(),
                Ok(want),
                "{style:?} {text:?}"
            );
        }
    }

    #[test]
    fn styles_refuse_what_would_break_the_braces() {
        assert_eq!(styled(Style::Bold, "|a}| b"), Err(UNMATCHED));
        assert_eq!(styled(Style::Bold, "|{a| b}"), Err(UNMATCHED));
        assert_eq!(styled(Style::Bold, "|a % b|"), Err(ENDS_IN_COMMENT));
        assert_eq!(styled(Style::Bold, "|a\nb % c|"), Err(ENDS_IN_COMMENT));
        // The command in a comment, its `}` on a line that is not.
        assert_eq!(styled(Style::Bold, "% |a\nb|"), Err(STARTS_IN_COMMENT));
        // Inside a comment the command is comment text too, and so is the `}`.
        assert_eq!(
            styled(Style::Bold, "% |a % b|").as_deref(),
            Ok("% \\textbf{|a % b|}")
        );
        assert_eq!(
            styled(Style::Bold, "% |a\nb % c|").as_deref(),
            Ok("% \\textbf{|a\nb % c|}")
        );
        // `\%` is a percent sign.
        assert_eq!(
            styled(Style::Bold, "|50\\%|").as_deref(),
            Ok("\\textbf{|50\\%|}")
        );
    }
}
