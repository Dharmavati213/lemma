//! The hover contents of WP3.2: references (what they refer to, with the source of the labelled
//! environment or a crop of the PDF), labels, citations and user macros.

use super::scan::{self, LabelInfo};
use super::{display_path, label_in_document};
use crate::editor::hover::HoverContent;
use crate::window::WindowState;
use gtk::prelude::*;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Source lines a reference hover shows.
pub const SOURCE_LINES: u32 = 3;
/// Most source lines of an equation whose PDF region a reference hover shows.
const CROP_LINES: u32 = 12;
/// Longest source line shown, in characters (longer ones end in "…").
const MAX_LINE_CHARS: usize = 96;
/// Largest file read for a hover (bytes).
const MAX_READ: u64 = 8 << 20;

const CSS: &str = "
.lemma-hover-box { margin: 2px 2px; }
.lemma-hover-code { font-family: monospace; font-size: 0.9em; padding: 6px 9px;
                    border-radius: 6px; background-color: alpha(currentColor, 0.07); }
.lemma-hover-crop { border-radius: 4px; }
.lemma-hover-crop-frame { border-radius: 6px; padding: 6px 2px; background-color: white;
                          box-shadow: 0 0 0 1px alpha(currentColor, 0.1); }
.lemma-hover-crop-frame.dark { background-color: rgb(41, 41, 41); }
.lemma-hover-missing image { color: var(--warning-color); }
popover.lemma-references > contents { padding: 0; }
.lemma-references-title { margin: 10px 12px 4px 12px; }
.lemma-references-list row { min-height: 26px; padding-top: 1px; padding-bottom: 1px;
                             margin-top: 0; margin-bottom: 0; }
.lemma-references-list row.header-row { min-height: 0; }
.lemma-references-file { margin: 6px 8px 0 8px; }
.lemma-references-line { font-family: monospace; font-size: 0.9em; }
popover.lemma-rename > contents { padding: 10px 12px; }
.lemma-rename-hint { margin-top: 2px; }
";

/// Adds the styles of the hovers and popovers to the display (once).
pub(super) fn install_css() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn escape(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

/// A label for Pango `markup` (wrapping, at most 72 characters wide, never focusable).
fn markup_label(markup: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(72)
        .xalign(0.0)
        .selectable(false)
        .can_focus(false)
        .build();
    label.set_markup(markup);
    label
}

/// A dim caption.
fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(72)
        .can_focus(false)
        .css_classes(["caption", "dim-label"])
        .build()
}

/// Source lines as a code block: common indentation removed, long lines cut.
fn code_block(lines: &[String]) -> gtk::Label {
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let text: Vec<String> = lines
        .iter()
        .map(|l| {
            let l = l.get(indent..).unwrap_or(l.trim_start()).trim_end();
            if l.chars().count() > MAX_LINE_CHARS {
                let cut: String = l.chars().take(MAX_LINE_CHARS - 1).collect();
                format!("{cut}…")
            } else {
                l.to_owned()
            }
        })
        .collect();
    gtk::Label::builder()
        .label(text.join("\n"))
        .xalign(0.0)
        .selectable(false)
        .can_focus(false)
        .css_classes(["lemma-hover-code"])
        .build()
}

fn vbox() -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.add_css_class("lemma-hover-box");
    content.set_can_focus(false);
    content
}

/// "main.tex:58" (1-based line).
fn location(state: &WindowState, path: &Path, line: u32) -> String {
    format!("{}:{}", display_path(state, path), line + 1)
}

/// Content for something that is not defined: a warning icon, a title and an explanation.
fn missing(title: &str, detail: &str) -> HoverContent {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("lemma-hover-missing");
    row.add_css_class("lemma-hover-box");
    let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    icon.set_valign(gtk::Align::Start);
    let text = vbox();
    text.remove_css_class("lemma-hover-box");
    text.set_spacing(2);
    text.append(&markup_label(&format!("<b>{}</b>", escape(title))));
    if !detail.is_empty() {
        text.append(&caption(detail));
    }
    row.append(&icon);
    row.append(&text);
    HoverContent::Widget(row.upcast())
}

/// The lines `range` of the file at `path`: the live text when Lemma has the document, else
/// `None` (read it with [`read_lines`]).
fn document_lines(state: &WindowState, path: &Path, range: Range<u32>) -> Option<Vec<String>> {
    let doc = state.workspace.get(path)?;
    let count = doc.line_index().line_count();
    Some(
        (range.start..range.end.min(count))
            .filter_map(|line| doc.line_text(line))
            .map(|l| l.trim_end_matches('\r').to_owned())
            .collect(),
    )
}

/// The lines `range` of the file at `path`, read on a worker thread.
async fn read_lines(path: PathBuf, range: Range<u32>) -> Option<Vec<String>> {
    gio::spawn_blocking(move || {
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_READ {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        Some(
            text.lines()
                .skip(range.start as usize)
                .take((range.end - range.start) as usize)
                .map(|l| l.trim_end_matches('\r').to_owned())
                .collect(),
        )
    })
    .await
    .ok()
    .flatten()
}

/// The source lines `range` of `path` and then `build(lines)`: at once from the live text of a
/// document Lemma has, else after reading the file.
fn with_lines(
    state: &Rc<WindowState>,
    path: &Path,
    range: Range<u32>,
    build: impl FnOnce(Vec<String>) -> gtk::Widget + 'static,
) -> HoverContent {
    match document_lines(state, path, range.clone()) {
        Some(lines) => HoverContent::Widget(build(lines)),
        None => {
            let path = path.to_path_buf();
            HoverContent::pending(async move {
                let lines = read_lines(path, range).await.unwrap_or_default();
                Some(HoverContent::Widget(build(lines)))
            })
        }
    }
}

/// "Theorem 3.2" in bold, with the label's title after it ("fpqc descent") where it names the
/// thing itself (not for equations, whose title is their section's).
fn heading(info: &LabelInfo, title: Option<&str>) -> String {
    let mut markup = format!("<b>{}</b>", escape(&info.title()));
    if let Some(title) = title.map(scan::detex).filter(|t| !t.is_empty())
        && !info.equation
        && info.kind != "Item"
    {
        markup.push_str(&format!("  <span alpha=\"62%\">{}</span>", escape(&title)));
    }
    markup
}

/// "Page 2 · main.tex:58", or "Not built yet · …".
fn footer(state: &WindowState, info: &LabelInfo, path: &Path, line: u32) -> String {
    let built = !state.workspace.index.borrow().aux.labels.is_empty();
    let place = location(state, path, line);
    match (&info.page, &info.number) {
        (Some(page), _) => format!("Page {page} · {place}"),
        (None, Some(_)) => place,
        (None, None) if built => format!("Numbered by the next build · {place}"),
        (None, None) => format!("Not built yet · {place}"),
    }
}

/// The hover of a reference to `name`.
pub(super) fn reference(state: &Rc<WindowState>, name: &str) -> HoverContent {
    let found = {
        let index = state.workspace.index.borrow();
        scan::find_label(&index, name).map(|(path, file, label)| {
            let info = scan::label_info(&index, file, label);
            let typeset = scan::typeset_lines(file, label, info.equation, CROP_LINES);
            (
                path.to_path_buf(),
                info,
                scan::label_lines(file, label, SOURCE_LINES),
                typeset,
                label.line,
                label.title.clone(),
            )
        })
    };
    let Some((path, info, mut lines, mut typeset, mut label_line, title)) = found else {
        return undefined_reference(state, name);
    };
    // The project index may lag behind the text Lemma has: move the lines to the label.
    if let Some(doc) = state.workspace.get(&path)
        && let Some(offset) = label_in_document(&doc, name, Some(label_line))
    {
        let now = doc.line_index().line_of(offset);
        let shift = i64::from(now) - i64::from(label_line);
        let moved = |l: u32| u32::try_from(i64::from(l) + shift).unwrap_or(0);
        lines = moved(lines.start)..moved(lines.end);
        typeset = moved(typeset.start)..moved(typeset.end);
        label_line = now;
    }
    let heading = heading(&info, title.as_deref());
    let footer = footer(state, &info, &path, label_line);
    // Only for a label of the last build (a newer one has no place in the PDF yet).
    let crop = info
        .number
        .as_ref()
        .and_then(|_| super::crop::request(state, &path, typeset, info.page.as_deref()));
    with_lines(state, &path, lines, move |lines| {
        let content = vbox();
        content.append(&markup_label(&heading));
        let code = code_block(&lines);
        content.append(&code);
        content.append(&caption(&footer));
        if let Some(crop) = crop {
            super::crop::show_when_ready(crop, &content, &code);
        }
        content.upcast()
    })
}

/// The hover of a reference whose label the project does not define.
fn undefined_reference(state: &WindowState, name: &str) -> HoverContent {
    let aux = state.workspace.index.borrow().aux.labels.get(name).cloned();
    match aux {
        // An external document's label (xr), or one the index cannot see.
        Some((number, page)) => HoverContent::Markup(format!(
            "<b>{}</b>\n<span alpha=\"62%\">From the last build · page {}</span>",
            escape(&scan::detex(&number)),
            escape(&page)
        )),
        None => missing(
            &format!("Undefined reference ‘{name}’"),
            "No \\label of this name in the project",
        ),
    }
}

/// The hover of `\label{name}` itself: its number, page and how often it is used.
pub(super) fn label(state: &Rc<WindowState>, name: &str) -> HoverContent {
    let index = state.workspace.index.borrow();
    let uses: usize = index
        .files
        .values()
        .flat_map(|f| f.refs.iter())
        .map(|r| r.names.iter().filter(|n| n.trim() == name).count())
        .sum();
    let found = scan::find_label(&index, name).map(|(path, file, label)| {
        (
            path.to_path_buf(),
            scan::label_info(&index, file, label),
            label.title.clone(),
        )
    });
    let duplicates = index
        .files
        .values()
        .flat_map(|f| f.labels.iter())
        .filter(|l| l.name == name)
        .count();
    drop(index);
    let used = match uses {
        0 => "Not referenced anywhere".to_owned(),
        1 => "Referenced once".to_owned(),
        n => format!("Referenced {n} times"),
    };
    let content = vbox();
    match found {
        Some((_, info, title)) => {
            content.append(&markup_label(&heading(&info, title.as_deref())));
            let page = info.page.as_deref().map(|p| format!("Page {p} · "));
            content.append(&caption(&format!("{}{used}", page.unwrap_or_default())));
        }
        None => {
            content.append(&markup_label(&format!("<b>{}</b>", escape(name))));
            content.append(&caption(&used));
        }
    }
    if duplicates > 1 {
        content.append(&caption(&format!(
            "Defined {duplicates} times in the project"
        )));
    }
    HoverContent::Widget(content.upcast())
}

/// The hover of the citation key `key`.
pub(super) fn citation(state: &Rc<WindowState>, key: &str) -> HoverContent {
    let index = state.workspace.index.borrow();
    if let Some((path, entry)) = scan::find_entry(&index, key) {
        let mut markup = String::new();
        let who = entry.author.as_deref().map(scan::authors);
        match (&who, &entry.year) {
            (Some(who), Some(year)) => markup.push_str(&format!(
                "<b>{}</b>  <span alpha=\"62%\">{}</span>",
                escape(who),
                escape(year)
            )),
            (Some(who), None) => markup.push_str(&format!("<b>{}</b>", escape(who))),
            (None, Some(year)) => markup.push_str(&format!("<b>{}</b>", escape(year))),
            (None, None) => markup.push_str(&format!("<b>{}</b>", escape(key))),
        }
        if let Some(title) = &entry.title {
            markup.push_str(&format!("\n<i>{}</i>", escape(&scan::detex(title))));
        }
        let content = vbox();
        content.set_spacing(4);
        content.append(&markup_label(&markup));
        content.append(&caption(&format!(
            "{} · {}",
            entry.kind,
            location(state, path, entry.line)
        )));
        return HoverContent::Widget(content.upcast());
    }
    let named: Vec<PathBuf> = index
        .files
        .iter()
        .flat_map(|(path, file)| scan::bibliographies(path, file))
        .collect();
    let detail = if named.is_empty() {
        "The project names no bibliography (\\bibliography or \\addbibresource)".to_owned()
    } else if let Some(unread) = named.iter().find(|b| !index.bibs.contains_key(*b)) {
        format!("{} could not be read", display_path(state, unread))
    } else {
        let names: Vec<String> = named.iter().map(|b| display_path(state, b)).collect();
        format!("No entry with this key in {}", names.join(", "))
    };
    drop(index);
    missing(&format!("No entry ‘{key}’"), &detail)
}

/// The hover of the user macro `\name`: its definition; `None` when the project does not
/// define it.
pub(super) fn user_macro(state: &Rc<WindowState>, name: &str) -> Option<HoverContent> {
    let (path, line, kind, constructed) = {
        let index = state.workspace.index.borrow();
        let (path, def) = scan::find_macro(&index, name)?;
        let kind = if def.is_math_operator {
            "math operator".to_owned()
        } else {
            match def.nargs {
                0 => "macro".to_owned(),
                1 => "macro with 1 argument".to_owned(),
                n => format!("macro with {n} arguments"),
            }
        };
        let constructed = constructed_definition(name, def);
        (path.to_path_buf(), def.line, kind, constructed)
    };
    let heading = format!(
        "<b>\\{}</b>  <span alpha=\"62%\">{}</span>",
        escape(name),
        escape(&kind)
    );
    let place = location(state, &path, line);
    // The definition as written (up to a few lines, while its braces are open).
    let name = name.to_owned();
    Some(with_lines(state, &path, line..line + 4, move |lines| {
        let lines = definition_lines(&lines, &name).unwrap_or_else(|| vec![constructed]);
        let content = vbox();
        content.append(&markup_label(&heading));
        content.append(&code_block(&lines));
        content.append(&caption(&place));
        content.upcast()
    }))
}

/// The lines of a definition that starts on `lines[0]`: until its braces close (at most all of
/// `lines`); `None` when the first line does not mention `\name`.
fn definition_lines(lines: &[String], name: &str) -> Option<Vec<String>> {
    let first = lines.first()?;
    let command = format!("\\{name}");
    if !first.contains(&command) {
        return None;
    }
    let mut depth = 0i32;
    let mut out = Vec::new();
    for line in lines {
        out.push(line.clone());
        let mut escaped = false;
        for c in line.chars() {
            match c {
                '\\' => escaped = !escaped,
                '%' if !escaped => break,
                '{' if !escaped => depth += 1,
                '}' if !escaped => depth -= 1,
                _ => {}
            }
            if c != '\\' {
                escaped = false;
            }
        }
        if depth <= 0 {
            break;
        }
    }
    Some(out)
}

/// A definition written from the project index (when the source cannot be read).
fn constructed_definition(name: &str, def: &lemma_syntax::MacroDef) -> String {
    let body = def.body.as_deref().unwrap_or("");
    if def.is_math_operator {
        return format!("\\DeclareMathOperator{{\\{name}}}{{{body}}}");
    }
    let mut out = format!("\\newcommand{{\\{name}}}");
    if def.nargs > 0 {
        out.push_str(&format!("[{}]", def.nargs));
    }
    if let Some(default) = &def.optional_default {
        out.push_str(&format!("[{default}]"));
    }
    out.push_str(&format!("{{{body}}}"));
    out
}
