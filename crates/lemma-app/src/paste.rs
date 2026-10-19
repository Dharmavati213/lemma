//! Image paste and file drops.
//!
//! - **Image paste:** when the clipboard holds an image and no text, Ctrl+V (the view's
//!   `paste-clipboard` signal, also the context menu's Paste) asks for a name (default
//!   `fig-<n>`, the first free number), saves the image as `figures/<name>.png` next to the
//!   current file (the directory is created when missing; an existing file is never replaced)
//!   and inserts `\includegraphics[width=0.8\textwidth]{figures/<name>}` where the cursor was.
//!   A clipboard that also holds text pastes the text, as before (an office program copies
//!   both).
//! - **Drops:** files dropped on the text insert one line each, at the drop point when it
//!   starts a line, else from the start of the next line,
//!   `\includegraphics[width=0.8\textwidth]{…}` for images, `\input{…}` for `.tex` files and
//!   `\addbibresource{….bib}` (when the project uses biblatex) or `\bibliography{…}` for `.bib`
//!   files, with paths relative to the current file. Other files, and names with `%`, `#`,
//!   braces or backslashes (LaTeX would misread them), are skipped with a toast.
//!
//! Hooks: [`attach`] per editor tab (the view's signal handler and drop target die with the
//! view). Tests: [`paste_image`] (what the signal handler runs), [`name_prompt`] (the open
//! dialog), [`drop_files`] (what a drop runs: GTK cannot inject a drop) and the pure helpers.

use crate::editor::Editor;
use crate::window::WindowState;
use adw::prelude::*;
use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};

/// The directory, next to the current file, that pasted images go to.
pub const FIGURES_DIR: &str = "figures";

/// File extensions `\includegraphics` takes (pdfLaTeX, LuaLaTeX and XeLaTeX).
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "pdf", "eps"];

/// Clipboard MIME types that hold an image.
const IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/bmp",
    "image/tiff",
    "image/webp",
];

/// Response ids of the name dialog.
pub const RESPONSE_SAVE: &str = "save";
pub const RESPONSE_CANCEL: &str = "cancel";

/// Connects image paste and file drops to `editor`'s view.
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let view = editor.view();
    let weak_state = Rc::downgrade(state);
    let weak_editor = Rc::downgrade(editor);
    view.connect_paste_clipboard(move |view| {
        let formats = view.clipboard().formats();
        if !clipboard_has_image(&formats) {
            return;
        }
        view.stop_signal_emission_by_name("paste-clipboard");
        let (Some(state), Some(editor)) = (weak_state.upgrade(), weak_editor.upgrade()) else {
            return;
        };
        glib::spawn_future_local(async move {
            if let Err(e) = paste_image(&state, &editor).await {
                state.toast(&e);
            }
        });
    });

    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    // Before the text view's own drop target, which would insert the URIs as text.
    target.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_state = Rc::downgrade(state);
    let weak_editor: Weak<Editor> = Rc::downgrade(editor);
    target.connect_drop(move |_, value, x, y| {
        let (Some(state), Some(editor)) = (weak_state.upgrade(), weak_editor.upgrade()) else {
            return false;
        };
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let files: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        if files.is_empty() {
            state.toast("Only local files can be dropped");
            return false;
        }
        let view = editor.view();
        let (bx, by) =
            view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let offset = view
            .iter_at_location(bx, by)
            .map(|iter| editor.document().byte_offset(&iter))
            .unwrap_or_else(|| editor.cursor_offset());
        drop_files(&state, &editor, &files, offset)
    });
    view.add_controller(target);
}

/// True when `formats` offer an image and no plain text.
pub fn clipboard_has_image(formats: &gdk::ContentFormats) -> bool {
    let image = formats.contains_type(gdk::Texture::static_type())
        || IMAGE_MIME_TYPES
            .iter()
            .any(|m| formats.contain_mime_type(m));
    let text = formats.contains_type(glib::Type::STRING)
        || formats.mime_types().iter().any(|m| {
            m.starts_with("text/plain") || matches!(m.as_str(), "UTF8_STRING" | "STRING" | "TEXT")
        });
    image && !text
}

/// The first `fig-<n>` (n ≥ 1) without a `<n>.png` in `figures`. **Blocking** (a few `stat`s).
pub fn default_name(figures: &Path) -> String {
    (1..)
        .map(|n| format!("fig-{n}"))
        .find(|name| !figures.join(format!("{name}.png")).exists())
        .unwrap_or_else(|| "fig".into())
}

/// The name the user typed, cleaned: surrounding blanks and a trailing `.png` go. Letters,
/// digits, `-`, `_` and `.` are allowed (LaTeX dislikes spaces and most punctuation in graphics
/// paths); an empty name or one starting with `.` is refused.
pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let name = match name.len().checked_sub(4) {
        Some(cut) if name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".png") => {
            &name[..cut]
        }
        _ => name,
    };
    if name.is_empty() {
        return Err("Enter a name".into());
    }
    if name.starts_with('.') {
        return Err("The name cannot start with a dot".into());
    }
    if name.len() > 100 {
        return Err("The name is too long".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!("“{c}” cannot be used in a figure name"));
    }
    Ok(name.to_owned())
}

/// `\includegraphics[width=0.8\textwidth]{path}`.
pub fn include_graphics(path: &str) -> String {
    format!("\\includegraphics[width=0.8\\textwidth]{{{path}}}")
}

/// Reads the image on `editor`'s clipboard, asks for its name, saves it under `figures/` and
/// inserts `\includegraphics` where the cursor was. `Ok(None)` when the user cancelled.
pub async fn paste_image(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
) -> Result<Option<PathBuf>, String> {
    let doc = editor.document().clone();
    let buffer = editor.buffer().clone();
    // Where to insert, kept in place while the dialog is open.
    let mark = buffer.create_mark(None, &buffer.iter_at_mark(&buffer.get_insert()), false);
    let result = paste_image_at(state, editor, &mark).await;
    buffer.delete_mark(&mark);
    if let Ok(Some(path)) = &result {
        state.toast(&format!(
            "Saved {}",
            relative_path(doc.path().parent().unwrap_or(Path::new("/")), path).display()
        ));
    }
    result
}

async fn paste_image_at(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
    mark: &gtk::TextMark,
) -> Result<Option<PathBuf>, String> {
    let texture = editor
        .view()
        .clipboard()
        .read_texture_future()
        .await
        .map_err(|e| format!("Cannot read the image on the clipboard: {e}"))?
        .ok_or("The clipboard holds no image")?;
    let dir = editor
        .document()
        .path()
        .parent()
        .map(Path::to_path_buf)
        .ok_or("The document has no directory")?;
    let figures = dir.join(FIGURES_DIR);
    let Some(name) = ask_name(state, &figures).await else {
        return Ok(None);
    };
    let path = figures.join(format!("{name}.png"));
    save_png(texture, path.clone()).await?;
    insert_at_mark(
        editor,
        mark,
        &include_graphics(&format!("{FIGURES_DIR}/{name}")),
    )?;
    Ok(Some(path))
}

/// Encodes `texture` as PNG and writes it to `path` (creating its directory) on a worker thread.
/// An existing file is never replaced.
async fn save_png(texture: gdk::Texture, path: PathBuf) -> Result<(), String> {
    gio::spawn_blocking(move || -> Result<(), String> {
        use std::io::Write;
        let display = path.display().to_string();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        let png = texture.save_to_png_bytes();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Cannot save {display}: {e}"))?;
        file.write_all(&png)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("Cannot save {display}: {e}"))
    })
    .await
    .map_err(|_| "Saving the image failed".to_owned())?
}

fn insert_at_mark(editor: &Editor, mark: &gtk::TextMark, text: &str) -> Result<(), String> {
    if !editor.document().is_open() || mark.is_deleted() {
        return Err("The document was closed; the image was saved but not inserted".into());
    }
    let buffer = editor.buffer();
    let mut at = buffer.iter_at_mark(mark);
    buffer.begin_user_action();
    buffer.insert(&mut at, text);
    buffer.end_user_action();
    buffer.place_cursor(&at);
    Ok(())
}

thread_local! {
    /// The open name dialogs (tests: [`name_prompt`]).
    static PROMPTS: RefCell<Vec<(Weak<WindowState>, adw::AlertDialog, gtk::Entry)>> =
        const { RefCell::new(Vec::new()) };
}

/// The name dialog open over `state`'s window and its entry (tests).
pub fn name_prompt(state: &WindowState) -> Option<(adw::AlertDialog, gtk::Entry)> {
    PROMPTS.with_borrow(|prompts| {
        prompts
            .iter()
            .find(|(s, _, _)| std::ptr::eq(s.as_ptr(), state))
            .map(|(_, d, e)| (d.clone(), e.clone()))
    })
}

/// Asks for the image's name; `None` when cancelled.
async fn ask_name(state: &Rc<WindowState>, figures: &Path) -> Option<String> {
    let default = {
        let figures = figures.to_path_buf();
        gio::spawn_blocking(move || default_name(&figures))
            .await
            .unwrap_or_else(|_| "fig-1".into())
    };
    let dialog = adw::AlertDialog::new(
        Some("Paste Image"),
        Some("The image is saved as a PNG file in the figures folder next to this document."),
    );
    dialog.add_responses(&[(RESPONSE_CANCEL, "_Cancel"), (RESPONSE_SAVE, "_Save")]);
    dialog.set_response_appearance(RESPONSE_SAVE, adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some(RESPONSE_SAVE));
    dialog.set_close_response(RESPONSE_CANCEL);
    let entry = gtk::Entry::builder()
        .text(&default)
        .activates_default(true)
        .build();
    let error = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["error", "caption"])
        .visible(false)
        .build();
    let suffix = gtk::Label::builder()
        .label(".png")
        .css_classes(["dim-label"])
        .build();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    entry.set_hexpand(true);
    row.append(&entry);
    row.append(&suffix);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&row);
    content.append(&error);
    dialog.set_extra_child(Some(&content));
    let validate = {
        let (dialog, error, figures) = (dialog.clone(), error.clone(), figures.to_path_buf());
        move |entry: &gtk::Entry| {
            let problem = match clean_name(&entry.text()) {
                Ok(name) if figures.join(format!("{name}.png")).exists() => {
                    Some(format!("{FIGURES_DIR}/{name}.png already exists"))
                }
                Ok(_) => None,
                Err(e) => Some(e),
            };
            dialog.set_response_enabled(RESPONSE_SAVE, problem.is_none());
            error.set_visible(problem.is_some());
            error.set_label(problem.as_deref().unwrap_or(""));
        }
    };
    validate(&entry);
    entry.connect_changed(validate);
    PROMPTS.with_borrow_mut(|p| p.push((Rc::downgrade(state), dialog.clone(), entry.clone())));
    // The entry has the focus when the dialog opens, so Enter saves.
    dialog.set_focus(Some(&entry));
    let response = dialog.clone().choose_future(Some(&state.window)).await;
    PROMPTS.with_borrow_mut(|p| p.retain(|(_, d, _)| d != &dialog));
    if response != RESPONSE_SAVE {
        return None;
    }
    clean_name(&entry.text()).ok()
}

// ----- drops -----

/// What a dropped file becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropKind {
    Image,
    Tex,
    Bib,
}

/// The kind of `path`, from its extension; `None` for files Lemma does not insert.
pub fn drop_kind(path: &Path) -> Option<DropKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "tex" => Some(DropKind::Tex),
        "bib" => Some(DropKind::Bib),
        e if IMAGE_EXTENSIONS.contains(&e) => Some(DropKind::Image),
        _ => None,
    }
}

/// `to` relative to the directory `from_dir` (both absolute), with `..` where needed.
pub fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    out
}

/// The command a file dropped into `current` (a file path) inserts; `None` for other files and
/// for paths that are not UTF-8.
pub fn drop_command(current: &Path, file: &Path, biblatex: bool) -> Option<String> {
    let kind = drop_kind(file)?;
    let dir = current.parent().unwrap_or(Path::new("/"));
    let rel = relative_path(dir, file);
    let rel = rel.to_str()?;
    if unsafe_in_latex(rel) {
        return None;
    }
    let stem = |ext: &str| {
        rel.len()
            .checked_sub(ext.len())
            .filter(|&cut| rel.is_char_boundary(cut) && rel[cut..].eq_ignore_ascii_case(ext))
            .map_or(rel, |cut| &rel[..cut])
    };
    Some(match kind {
        DropKind::Image => include_graphics(rel),
        DropKind::Tex => format!("\\input{{{}}}", stem(".tex")),
        DropKind::Bib if biblatex => format!("\\addbibresource{{{rel}}}"),
        DropKind::Bib => format!("\\bibliography{{{}}}", stem(".bib")),
    })
}

/// Characters a file name in `\includegraphics`, `\input` or `\bibliography` cannot carry as
/// they are: a comment, a parameter, a group or a command would start.
const LATEX_SPECIAL: &[char] = &['%', '#', '{', '}', '\\', '\n', '\r'];

/// True when `name` holds one of [`LATEX_SPECIAL`].
pub fn unsafe_in_latex(name: &str) -> bool {
    name.contains(LATEX_SPECIAL)
}

/// True when `text` loads biblatex or names a resource with `\addbibresource` (comments
/// ignored).
pub fn uses_biblatex(text: &str) -> bool {
    text.lines().any(|line| {
        let line = strip_comment(line);
        line.contains("\\addbibresource")
            || (line.contains("\\usepackage")
                && line
                    .split(['{', '}', ','])
                    .any(|part| part.trim() == "biblatex"))
    })
}

/// `line` up to its first unescaped `%`.
pub(crate) fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'%' => return &line[..i],
            _ => i += 1,
        }
    }
    line
}

/// Inserts the commands for `files` into `editor` at byte `offset`, one per line, as one undo
/// step; files of other kinds are skipped with a toast. True when anything was inserted.
pub fn drop_files(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
    files: &[PathBuf],
    offset: usize,
) -> bool {
    let doc = editor.document();
    let biblatex = state
        .workspace
        .documents()
        .iter()
        .any(|d| d.with_text(uses_biblatex));
    let mut lines = Vec::new();
    let mut skipped = Vec::new();
    let mut refused = Vec::new();
    for file in files {
        let name = file.file_name().map_or_else(
            || file.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        match drop_command(doc.path(), file, biblatex) {
            Some(line) => lines.push(line),
            None if drop_kind(file).is_some() => refused.push(name),
            None => skipped.push(name),
        }
    }
    if !skipped.is_empty() {
        state.toast(&format!(
            "Not inserted (only images, .tex and .bib files): {}",
            skipped.join(", ")
        ));
    }
    if !refused.is_empty() {
        state.toast(&format!(
            "Not inserted (rename it without % # {{ }} or \\ first): {}",
            refused.join(", ")
        ));
    }
    if lines.is_empty() {
        return false;
    }
    let Some(mut at) = editor.iter_near(offset) else {
        return false;
    };
    // On lines of their own: inside a line, from the start of the next one.
    let mut text = lines.join("\n");
    if !at.starts_line() {
        at.forward_line();
        if !at.starts_line() {
            // The last line, without a line break.
            text.insert(0, '\n');
        }
    }
    if !at.is_end() {
        text.push('\n');
    }
    let buffer = editor.buffer();
    buffer.begin_user_action();
    buffer.insert(&mut at, &text);
    buffer.end_user_action();
    buffer.place_cursor(&at);
    editor.view().grab_focus();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_checked() {
        assert_eq!(clean_name(" fig-3 ").unwrap(), "fig-3");
        assert_eq!(clean_name("plot.PNG").unwrap(), "plot");
        assert_eq!(clean_name("a_b.v2").unwrap(), "a_b.v2");
        assert!(clean_name("").is_err());
        assert!(clean_name(".png").is_err());
        assert!(clean_name(".hidden").is_err());
        assert!(clean_name("a b").is_err());
        assert!(clean_name("../x").is_err());
        assert!(clean_name("é").is_err());
        assert!(clean_name(&"x".repeat(101)).is_err());
    }

    #[test]
    fn relative_paths() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(relative_path(&p("/a/b"), &p("/a/b/c.png")), p("c.png"));
        assert_eq!(relative_path(&p("/a/b"), &p("/a/b/f/c.png")), p("f/c.png"));
        assert_eq!(
            relative_path(&p("/a/b/ch"), &p("/a/b/f/c.png")),
            p("../f/c.png")
        );
        assert_eq!(
            relative_path(&p("/a/b"), &p("/x/y.tex")),
            p("../../x/y.tex")
        );
    }

    #[test]
    fn drop_commands() {
        let cur = Path::new("/p/chapters/intro.tex");
        assert_eq!(
            drop_command(cur, Path::new("/p/chapters/img/plot.PNG"), false).unwrap(),
            "\\includegraphics[width=0.8\\textwidth]{img/plot.PNG}"
        );
        assert_eq!(
            drop_command(cur, Path::new("/p/fig.pdf"), false).unwrap(),
            "\\includegraphics[width=0.8\\textwidth]{../fig.pdf}"
        );
        assert_eq!(
            drop_command(cur, Path::new("/p/chapters/proofs.tex"), false).unwrap(),
            "\\input{proofs}"
        );
        assert_eq!(
            drop_command(cur, Path::new("/p/refs.bib"), false).unwrap(),
            "\\bibliography{../refs}"
        );
        assert_eq!(
            drop_command(cur, Path::new("/p/refs.bib"), true).unwrap(),
            "\\addbibresource{../refs.bib}"
        );
        assert_eq!(drop_command(cur, Path::new("/p/notes.txt"), false), None);
        assert_eq!(drop_command(cur, Path::new("/p/Makefile"), false), None);
    }

    #[test]
    fn biblatex_detection() {
        assert!(uses_biblatex("\\usepackage[style=alphabetic]{biblatex}\n"));
        assert!(uses_biblatex("\\usepackage{amsmath, biblatex}\n"));
        assert!(uses_biblatex("\\addbibresource{refs.bib}\n"));
        assert!(!uses_biblatex(
            "% \\usepackage{biblatex}\n\\bibliography{refs}\n"
        ));
        assert!(!uses_biblatex("\\usepackage{biblatex-chicago-notes}\n"));
        assert!(!uses_biblatex("50\\% \\usepackage{amsmath}"));
    }

    #[test]
    fn include_graphics_text() {
        assert_eq!(
            include_graphics("figures/fig-1"),
            "\\includegraphics[width=0.8\\textwidth]{figures/fig-1}"
        );
    }
}
