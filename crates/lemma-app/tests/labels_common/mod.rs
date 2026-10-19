//! The project fixture of WP3.2's tests (`gtk_goto.rs`, `gtk_labels.rs`): a small LaTeX project
//! with references of every kind, a chapter file, a bibliography, a local package, an image and
//! a built `.aux`, plus planted errors (an undefined reference and citation, an unused label
//! and entry).

#![allow(dead_code)]

use crate::common;
use adw::prelude::*;
use lemma_app::editor::{Editor, completion};
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub const MAIN: &str = "\\documentclass{article}\n\
\\usepackage{amsmath,amsthm}\n\
\\usepackage{mydefs}\n\
\\newcommand{\\cF}{\\mathcal{F}}\n\
\\DeclareMathOperator{\\Hom}{Hom}\n\
\\newtheorem{theorem}{Theorem}[section]\n\
\\addbibresource{refs.bib}\n\
\\begin{document}\n\
\\section{Intro}\\label{sec:intro}\n\
See Theorem~\\ref{thm:main}, \\eqref{eq:one}, \\cref{sec:intro,eq:one} and \\crefrange{eq:one}{eq:two}.\n\
Cite \\cite{knuth, missing} and \\ref{sec:ch}; also $\\eqref{eq:two}$.\n\
\\begin{theorem}[Main]\\label{thm:main}\n\
Statement $\\cF \\cong \\Hom(A,B)$ and \\mydef.\n\
\\end{theorem}\n\
\\begin{equation}\\label{eq:one}\n\
a = b\n\
\\end{equation}\n\
\\begin{align}\n\
c &= d \\label{eq:two}\n\
\\end{align}\n\
\\input{chapters/ch1}\n\
\\includegraphics[width=2cm]{fig}\n\
% \\ref{sec:commented}\n\
\\ref{sec:undefined}\n\
\\end{document}\n";

pub const CHAPTER: &str = "\\section{Chapter}\\label{sec:ch}\n\
Back to Theorem~\\ref{thm:main} and \\eqref{eq:one}.\n\
\\label{unused:one}\n";

pub const BIB: &str = "@book{knuth,\n\
  author = {Knuth, Donald E.},\n\
  title = {The \\TeX book},\n\
  year = {1984}\n\
}\n\
@article{spare, author = {Nobody}, title = {Unused}, year = {2000}}\n";

pub const AUX: &str = "\\relax\n\
\\newlabel{sec:intro}{{1}{1}}\n\
\\newlabel{thm:main}{{1.1}{1}}\n\
\\newlabel{eq:one}{{1}{1}}\n\
\\newlabel{eq:two}{{2}{2}}\n\
\\newlabel{sec:ch}{{2}{2}}\n";

pub fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

pub struct Fixture {
    pub state: Rc<WindowState>,
    pub dir: PathBuf,
}

/// The project in a fresh directory and a window on it (Vim, spelling and builds on save off).
pub fn fixture(name: &str) -> Fixture {
    fixture_with(name, |_| {})
}

pub fn fixture_with(name: &str, configure: impl FnOnce(&mut Config)) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    let files: &[(&str, &[u8])] = &[
        ("main.tex", MAIN.as_bytes()),
        ("chapters/ch1.tex", CHAPTER.as_bytes()),
        ("refs.bib", BIB.as_bytes()),
        ("main.aux", AUX.as_bytes()),
        ("mydefs.sty", b"\\newcommand{\\mydef}{x}\n"),
        ("fig.png", b"\x89PNG\r\n\x1a\nnot really"),
    ];
    for (file, bytes) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.preview.delay_ms = 60;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    /// Opens `file` in a tab, laid out and drawn.
    pub fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        assert!(block_on(testmode::wait_for_frames(&self.state.window, 1)));
        let doc = editor.document().clone();
        common::pump_until(TIMEOUT, || doc.has_line_states());
        editor
    }

    pub fn path(&self, file: &str) -> PathBuf {
        normalize(&self.dir.join(file))
    }

    /// Waits until the project index has the chapter, the bibliography and the `.aux` numbers,
    /// and the label checks ran on it.
    pub fn wait_index(&self) {
        completion::refresh_project(&self.state);
        let state = self.state.clone();
        let chapter = self.path("chapters/ch1.tex");
        common::pump_until(TIMEOUT, || {
            let index = state.workspace.index.borrow();
            !completion::project_busy(&state)
                && index.files.keys().any(|p| normalize(p) == chapter)
                && !index.bibs.is_empty()
                && !index.aux.labels.is_empty()
        });
        lemma_app::labels::check_now(&self.state);
    }

    /// Waits for the checks that edits or saves scheduled.
    pub fn wait_checks(&self) {
        let state = self.state.clone();
        common::pump_until(TIMEOUT, || {
            !lemma_app::labels::check_pending(&state) && !completion::project_busy(&state)
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// The byte offset of the first `needle` in `editor`'s text, plus `delta`.
pub fn offset_of(editor: &Editor, needle: &str, delta: usize) -> usize {
    editor
        .document()
        .text()
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
        + delta
}

/// The byte offset of the first `needle` in `text`, plus `delta`.
pub fn find(text: &str, needle: &str, delta: usize) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
        + delta
}

/// The texts of every label inside `widget`, in order.
pub fn texts(widget: &gtk::Widget) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        out.push(label.text().to_string());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        out.extend(texts(&c));
        child = c.next_sibling();
    }
    out
}

/// The texts of a widget, joined.
pub fn text_of(widget: &gtk::Widget) -> String {
    texts(widget).join("\n")
}

/// A file's text on disk.
pub fn disk(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// With `LEMMA_TEST_SNAPSHOTS=dir`: saves the window in light and dark as `dir/<name>-light.png`
/// and `dir/<name>-dark.png`, after `show()` (called again for each, after the scheme changed)
/// put up what to see.
pub fn snapshots(f: &Fixture, name: &str, mut show: impl FnMut()) {
    let Ok(dir) = std::env::var("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    for (scheme, suffix) in [
        (adw::ColorScheme::ForceLight, "light"),
        (adw::ColorScheme::ForceDark, "dark"),
    ] {
        adw::StyleManager::default().set_color_scheme(scheme);
        show();
        assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
        let shot = Path::new(&dir).join(format!("{name}-{suffix}.png"));
        block_on(testmode::snapshot_next_frame(&f.state.window, &shot)).unwrap();
        println!("  saved {}", shot.display());
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
}

/// A copy of `tests/fixtures/proj` (with its PDF and SyncTeX file) and a window on it, its PDF
/// shown.
pub fn proj_fixture(name: &str) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    copy_dir(&common::fixtures().join("proj"), &dir);
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    config.preview.delay_ms = 60;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1100, 760);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
