//! New documents from templates and the project's `CLAUDE.md`.
//!
//! - **Templates:** `win.new-from-template` ("New from Template…" in the main menu's file
//!   section and the command palette) opens a dialog listing [`TEMPLATES`] (amsart, article,
//!   beamer, a thesis as a book with chapters, and a minimal document; their files are
//!   `data/templates/<id>/`, compiled in). Create writes the template's files into the chosen
//!   folder, all or nothing: when any of them exists already nothing is written and the dialog
//!   says which. Then `main.tex` opens: in this window when it lies in the project (in an
//!   empty window the folder becomes the project root), else in a new Lemma window.
//! - **Project instructions:** `win.create-project-instructions` ("Create Project
//!   Instructions") writes `CLAUDE.md` in the project root with the conventions inferred from the
//!   project ([`infer`]): the main document and its class, the engine, the bibliography tool
//!   and files, the macros files and the language, plus the rule to edit only through Lemma's
//!   tools. It is a normal new file, opened for editing. An existing `CLAUDE.md` is never
//!   overwritten; it is opened instead.
//!
//! Hooks: [`install`] once per window (actions and menu items); [`show`] opens the dialog;
//! [`run_demo`] is `--demo templates`. Tests: [`dialog`], [`create`], [`infer`],
//! [`instructions_text`], [`create_project_instructions`].

use crate::paste::strip_comment;
use crate::window::WindowState;
use crate::workspace::walk_project;
use adw::prelude::*;
use lemma_build::Engine;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Opens a new project's `main.tex` in a new Lemma window (see [`set_launcher`]).
type Launcher = Box<dyn Fn(&Path) -> Result<(), String>>;

thread_local! {
    static LAUNCHER: RefCell<Option<Launcher>> = const { RefCell::new(None) };
}

/// Tests: replaces the new `lemma` process that opens a project created outside the window's
/// project root (`None`: the default).
pub fn set_launcher(launcher: Option<Launcher>) {
    LAUNCHER.with_borrow_mut(|l| *l = launcher);
}

/// Opens `main` in a new Lemma window: a new `lemma` process (Lemma is one window per
/// process), rooted at its folder.
fn launch(main: &Path) -> Result<(), String> {
    if let Some(result) = LAUNCHER.with_borrow(|l| l.as_ref().map(|f| f(main))) {
        return result;
    }
    let failed = |e: &dyn std::fmt::Display| format!("Lemma could not open a new window: {e}");
    let exe = std::env::current_exe().map_err(|e| failed(&e))?;
    // GSubprocess reaps the child when it exits.
    gio::Subprocess::newv(
        &[exe.as_os_str(), main.as_os_str()],
        gio::SubprocessFlags::NONE,
    )
    .map(|_| ())
    .map_err(|e| failed(&e))
}

/// One template: files relative to the chosen folder, `main.tex` among them.
#[derive(Debug)]
pub struct Template {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

/// The main document every template has.
pub const MAIN_FILE: &str = "main.tex";

macro_rules! file {
    ($id:literal, $path:literal) => {
        (
            $path,
            include_str!(concat!("../../../data/templates/", $id, "/", $path)),
        )
    };
}

/// Every template, in the dialog's order.
pub const TEMPLATES: &[Template] = &[
    Template {
        id: "amsart",
        title: "AMS Article",
        description: "amsart with theorem environments and a BibTeX bibliography (amsalpha)",
        files: &[
            file!("amsart", "main.tex"),
            file!("amsart", "references.bib"),
        ],
    },
    Template {
        id: "article",
        title: "Article",
        description: "article with amsthm, cleveref and a biblatex bibliography",
        files: &[
            file!("article", "main.tex"),
            file!("article", "references.bib"),
        ],
    },
    Template {
        id: "beamer",
        title: "Beamer Slides",
        description: "A talk: title frame, overview and sections",
        files: &[file!("beamer", "main.tex")],
    },
    Template {
        id: "thesis",
        title: "Thesis",
        description: "book with one file per chapter, a macros file and biblatex",
        files: &[
            file!("thesis", "main.tex"),
            file!("thesis", "macros.tex"),
            file!("thesis", "chapters/introduction.tex"),
            file!("thesis", "chapters/preliminaries.tex"),
            file!("thesis", "references.bib"),
        ],
    },
    Template {
        id: "minimal",
        title: "Minimal",
        description: "A bare article with amsmath and amssymb",
        files: &[file!("minimal", "main.tex")],
    },
];

/// The template called `id`.
pub fn template(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// Writes `template`'s files into `dir` (created when missing) and returns the path of its
/// `main.tex`. Nothing is written when one of the files exists already. **Blocking.**
pub fn create(template: &Template, dir: &Path) -> Result<PathBuf, String> {
    if let Some((existing, _)) = template
        .files
        .iter()
        .find(|(file, _)| dir.join(file).symlink_metadata().is_ok())
    {
        return Err(format!(
            "{existing} already exists in {}; choose an empty folder",
            dir.display()
        ));
    }
    let mut written: Vec<PathBuf> = Vec::new();
    for (file, text) in template.files {
        let path = dir.join(file);
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| write_new(&path, text));
        if let Err(e) = result {
            // All or nothing: take back what this call wrote.
            for path in written {
                let _ = std::fs::remove_file(path);
            }
            return Err(format!("Cannot write {}: {e}", path.display()));
        }
        written.push(path);
    }
    Ok(dir.join(MAIN_FILE))
}

/// Writes `text` into the new file `path`; an existing file is an error.
fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// The actions' names.
pub const TEMPLATE_ACTION: &str = "new-from-template";
pub const INSTRUCTIONS_ACTION: &str = "create-project-instructions";

/// Adds `win.new-from-template` and `win.create-project-instructions`, their menu items (main
/// menu, file section) and their palette names.
pub fn install(state: &Rc<WindowState>) {
    let add = |name: &str, label: &str, menu: &str, run: fn(&Rc<WindowState>)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(state);
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                run(&state);
            }
        });
        state.window.add_action(&action);
        let detailed = format!("win.{name}");
        crate::app::describe_action(&detailed, label, crate::app::ShortcutGroup::General, None);
        state.header.menu_files.append(Some(menu), Some(&detailed));
    };
    add(
        TEMPLATE_ACTION,
        "New Document from Template",
        "New from _Template…",
        show,
    );
    add(
        INSTRUCTIONS_ACTION,
        "Create Project Instructions (CLAUDE.md)",
        "Create Project _Instructions",
        |state| {
            let state = state.clone();
            glib::spawn_future_local(async move {
                if let Err(e) = create_project_instructions(&state).await {
                    state.toast(&e);
                }
            });
        },
    );
}

// ----- the dialog -----

/// The open "New Document from Template" dialog.
pub struct TemplateDialog {
    state: Weak<WindowState>,
    pub dialog: adw::Dialog,
    checks: Vec<gtk::CheckButton>,
    selected: Cell<usize>,
    folder: RefCell<Option<PathBuf>>,
    folder_row: adw::ActionRow,
    pub create_button: gtk::Button,
    error: gtk::Label,
    busy: Cell<bool>,
}

thread_local! {
    static DIALOGS: RefCell<Vec<Rc<TemplateDialog>>> = const { RefCell::new(Vec::new()) };
}

/// The template dialog open over `state`'s window (tests).
pub fn dialog(state: &WindowState) -> Option<Rc<TemplateDialog>> {
    DIALOGS.with_borrow(|list| {
        list.iter()
            .find(|d| std::ptr::eq(d.state.as_ptr(), state))
            .cloned()
    })
}

/// `win.new-from-template`: opens the dialog (or presents the open one).
pub fn show(state: &Rc<WindowState>) {
    if let Some(open) = dialog(state) {
        open.dialog.present(Some(&state.window));
        return;
    }
    let d = TemplateDialog::new(state);
    DIALOGS.with_borrow_mut(|list| list.push(d.clone()));
    d.dialog.connect_closed(|dialog| {
        DIALOGS.with_borrow_mut(|list| list.retain(|d| &d.dialog != dialog));
    });
    d.dialog.present(Some(&state.window));
}

impl TemplateDialog {
    fn new(state: &Rc<WindowState>) -> Rc<Self> {
        let templates = adw::PreferencesGroup::builder().title("Template").build();
        let mut checks: Vec<gtk::CheckButton> = Vec::new();
        for t in TEMPLATES {
            let check = gtk::CheckButton::new();
            if let Some(first) = checks.first() {
                check.set_group(Some(first));
            } else {
                check.set_active(true);
            }
            let row = adw::ActionRow::builder()
                .title(t.title)
                .subtitle(t.description)
                .activatable_widget(&check)
                .build();
            row.add_prefix(&check);
            templates.add(&row);
            checks.push(check);
        }
        let choose = gtk::Button::builder()
            .label("_Choose…")
            .use_underline(true)
            .valign(gtk::Align::Center)
            .build();
        let folder_row = adw::ActionRow::builder()
            .title("Folder")
            .subtitle("None chosen")
            // The subtitle is a path: plain text (`R&D` is not markup).
            .use_markup(false)
            .activatable_widget(&choose)
            .build();
        folder_row.add_suffix(&choose);
        let location = adw::PreferencesGroup::builder()
            .title("Location")
            .description("The files are created in this folder, which should be empty.")
            .build();
        location.add(&folder_row);
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["error"])
            .build();
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        page.append(&templates);
        page.append(&location);
        page.append(&error);
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&page)
            .build();
        let create_button = gtk::Button::builder()
            .label("C_reate")
            .use_underline(true)
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::new();
        header.pack_end(&create_button);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scrolled));
        let dialog = adw::Dialog::builder()
            .title("New Document from Template")
            .content_width(480)
            .child(&toolbar)
            .default_widget(&create_button)
            .build();
        let this = Rc::new(Self {
            state: Rc::downgrade(state),
            dialog,
            checks,
            selected: Cell::new(0),
            folder: RefCell::new(None),
            folder_row,
            create_button,
            error,
            busy: Cell::new(false),
        });
        for (i, check) in this.checks.iter().enumerate() {
            let weak = Rc::downgrade(&this);
            check.connect_toggled(move |check| {
                if let Some(this) = weak.upgrade()
                    && check.is_active()
                {
                    this.selected.set(i);
                    this.show_error(None);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        choose.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                glib::spawn_future_local(async move { this.choose_folder().await });
            }
        });
        let weak = Rc::downgrade(&this);
        this.create_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.create();
            }
        });
        this
    }

    /// The selected template.
    pub fn selected(&self) -> &'static Template {
        &TEMPLATES[self.selected.get()]
    }

    /// Selects the template `id` (tests); false when there is none.
    pub fn select(&self, id: &str) -> bool {
        match TEMPLATES.iter().position(|t| t.id == id) {
            Some(i) => {
                self.checks[i].set_active(true);
                true
            }
            None => false,
        }
    }

    /// The folder the files go to.
    pub fn folder(&self) -> Option<PathBuf> {
        self.folder.borrow().clone()
    }

    /// Sets the folder (what the folder chooser does).
    pub fn set_folder(&self, dir: &Path) {
        self.folder.replace(Some(dir.to_path_buf()));
        self.folder_row.set_subtitle(&dir.display().to_string());
        self.create_button.set_sensitive(!self.busy.get());
        self.show_error(None);
    }

    /// The error shown under the list, if any.
    pub fn error_text(&self) -> Option<String> {
        self.error
            .is_visible()
            .then(|| self.error.label().to_string())
    }

    fn show_error(&self, message: Option<&str>) {
        self.error.set_visible(message.is_some());
        self.error.set_label(message.unwrap_or(""));
    }

    async fn choose_folder(self: Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let initial = self
            .folder()
            .unwrap_or_else(|| state.workspace.root().path().to_path_buf());
        let chooser = gtk::FileDialog::builder()
            .title("Folder for the New Document")
            .modal(true)
            .initial_folder(&gio::File::for_path(initial))
            .build();
        let Ok(folder) = chooser.select_folder_future(Some(&state.window)).await else {
            return;
        };
        match folder.path() {
            Some(path) => self.set_folder(&path),
            None => self.show_error(Some("Choose a local folder")),
        }
    }

    /// Creates the files (on a worker thread) and opens `main.tex`; errors are shown in the
    /// dialog.
    pub fn create(self: &Rc<Self>) {
        let Some(dir) = self.folder() else {
            self.show_error(Some("Choose a folder first"));
            return;
        };
        if self.busy.replace(true) {
            return;
        }
        self.create_button.set_sensitive(false);
        let this = self.clone();
        let template = self.selected();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || create(template, &dir))
                .await
                .unwrap_or_else(|_| Err("Creating the files failed".into()));
            this.busy.set(false);
            this.create_button.set_sensitive(true);
            let Some(state) = this.state.upgrade() else {
                return;
            };
            match result {
                Ok(main) => {
                    this.dialog.close();
                    // A folder outside this window's project is a project of its own: its own
                    // window, whose root (agent, search, outline, file lists) is that folder.
                    if !state.opens_inside_root(&main) {
                        match launch(&main) {
                            Ok(()) => {
                                state.toast(&format!("{} opens in a new window", main.display()));
                                return;
                            }
                            Err(e) => state.toast(&e),
                        }
                    }
                    if let Err(e) = state.open_file(&main).await {
                        state.toast(&e);
                    }
                }
                Err(e) => this.show_error(Some(&e)),
            }
        });
    }
}

/// `--demo templates`: the dialog with the thesis template selected and a folder chosen (the
/// directory of FILE, or the project root), for its snapshot. Nothing is written.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    show(&state);
    let d = dialog(&state).ok_or("the template dialog did not open")?;
    d.select("thesis");
    let folder = state
        .active_editor()
        .and_then(|e| e.document().path().parent().map(|p| p.join("thesis")))
        .unwrap_or_else(|| state.workspace.root().path().join("thesis"));
    d.set_folder(&folder);
    crate::testmode::wait_for_frames(&state.window, 2).await;
    Ok(())
}

// ----- CLAUDE.md -----

/// The file [`create_project_instructions`] writes in the project root.
pub const INSTRUCTIONS_FILE: &str = "CLAUDE.md";

/// What [`infer`] found out about a project. Paths are relative to the project root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFacts {
    /// The main document: a `.tex` file with `\documentclass` and `\begin{document}`.
    pub main: Option<PathBuf>,
    /// Its document class.
    pub class: Option<String>,
    /// The engine latexmk runs, as [`lemma_build::detect_engine`] decides.
    pub engine: Option<Engine>,
    /// The bibliography tool: "biblatex with Biber", "biblatex with BibTeX" or "BibTeX".
    pub bibliography: Option<String>,
    /// The BibTeX style of a BibTeX bibliography.
    pub bibliography_style: Option<String>,
    /// The project's `.bib` files.
    pub bib_files: Vec<PathBuf>,
    /// Files other than the main document that define macros.
    pub macros: Vec<PathBuf>,
    /// True when the main document's preamble defines macros itself.
    pub preamble_macros: bool,
    /// The document language (babel's main language or polyglossia's default), in English.
    pub language: Option<String>,
}

/// At most this many `.tex` and `.sty` files are read by [`infer`].
const MAX_READ_FILES: usize = 200;
/// Larger files are not read by [`infer`].
const MAX_READ_BYTES: u64 = 256 << 10;

/// Infers the project's conventions from the files under `root`. **Blocking.**
pub fn infer(root: &Path) -> ProjectFacts {
    let walked = walk_project(root, &[root.join(".build")], 5000, 500);
    let mut texts: Vec<(PathBuf, String)> = Vec::new();
    let mut facts = ProjectFacts::default();
    for path in &walked.files {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        match ext.as_deref() {
            Some("bib") => facts.bib_files.push(rel),
            Some("tex" | "sty") if texts.len() < MAX_READ_FILES => {
                if std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_READ_BYTES)
                    && let Ok(text) = std::fs::read_to_string(path)
                {
                    texts.push((rel, text));
                }
            }
            _ => {}
        }
    }
    infer_from(root, &texts, facts)
}

/// [`infer`] on files already read (`texts`: paths relative to `root`).
pub fn infer_from(
    root: &Path,
    texts: &[(PathBuf, String)],
    mut facts: ProjectFacts,
) -> ProjectFacts {
    let is_main = |text: &str| {
        let body = uncommented(text);
        body.contains("\\documentclass") && body.contains("\\begin{document}")
    };
    // `main.tex` first, then the shallowest document, then by name.
    let main = texts
        .iter()
        .filter(|(p, t)| p.extension().is_some_and(|e| e == "tex") && is_main(t))
        .min_by_key(|(p, _)| {
            (
                p.file_name().is_none_or(|n| n != MAIN_FILE),
                p.components().count(),
                p.clone(),
            )
        });
    let all: Vec<String> = texts.iter().map(|(_, t)| uncommented(t)).collect();
    if let Some((path, text)) = main {
        facts.main = Some(path.clone());
        let body = uncommented(text);
        facts.class = argument_of(&body, "\\documentclass");
        let dir = root.join(path).parent().map(Path::to_path_buf);
        facts.engine = Some(lemma_build::detect_engine(
            text,
            dir.as_deref().unwrap_or(root),
        ));
        let preamble = body.split("\\begin{document}").next().unwrap_or("");
        facts.preamble_macros = macro_definitions(preamble) >= 3;
        facts.language = language(&body);
    }
    let joined = all.join("\n");
    if crate::paste::uses_biblatex(&joined) {
        let bibtex_backend = usepackage_options(&joined, "biblatex").iter().any(|o| {
            o.replace(' ', "") == "backend=bibtex" || o.replace(' ', "") == "backend=bibtex8"
        });
        facts.bibliography = Some(
            if bibtex_backend {
                "biblatex with BibTeX"
            } else {
                "biblatex with Biber"
            }
            .into(),
        );
    } else if joined.contains("\\bibliography{") {
        facts.bibliography = Some("BibTeX".into());
        facts.bibliography_style = argument_of(&joined, "\\bibliographystyle");
    }
    facts.macros = texts
        .iter()
        .zip(&all)
        .filter(|((p, _), body)| {
            Some(p) != facts.main.as_ref()
                && macro_definitions(body) >= 3
                && !body.contains("\\section")
                && !body.contains("\\chapter")
        })
        .map(|((p, _), _)| p.clone())
        .collect();
    facts
}

/// `text` without its comments.
fn uncommented(text: &str) -> String {
    text.lines()
        .map(strip_comment)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first argument of the first `command` (optional arguments skipped).
fn argument_of(text: &str, command: &str) -> Option<String> {
    let mut rest = text;
    while let Some(at) = rest.find(command) {
        rest = &rest[at + command.len()..];
        // `\bibliographystyle` also starts with `\bibliography`.
        if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        let mut s = rest.trim_start();
        if s.starts_with('[') {
            s = &s[s.find(']')? + 1..];
            s = s.trim_start();
        }
        let s = s.strip_prefix('{')?;
        return Some(s[..s.find('}')?].trim().to_owned());
    }
    None
}

/// The options of every `\usepackage[…]{package}`.
fn usepackage_options(text: &str, package: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("\\usepackage") {
        rest = &rest[at + "\\usepackage".len()..];
        let s = rest.trim_start();
        let (options, after) = match s.strip_prefix('[') {
            Some(s) => match s.find(']') {
                Some(end) => (&s[..end], &s[end + 1..]),
                None => continue,
            },
            None => ("", s),
        };
        let Some(after) = after.trim_start().strip_prefix('{') else {
            continue;
        };
        let Some(end) = after.find('}') else {
            continue;
        };
        if after[..end].split(',').any(|p| p.trim() == package) {
            out.extend(
                options
                    .split(',')
                    .map(|o| o.trim().to_owned())
                    .filter(|o| !o.is_empty()),
            );
        }
    }
    out
}

/// How many macros `text` defines.
fn macro_definitions(text: &str) -> usize {
    [
        "\\newcommand",
        "\\renewcommand",
        "\\providecommand",
        "\\DeclareMathOperator",
        "\\NewDocumentCommand",
        "\\DeclarePairedDelimiter",
        "\\def\\",
    ]
    .iter()
    .map(|c| text.matches(c).count())
    .sum()
}

/// The document language: polyglossia's `\setdefaultlanguage`/`\setmainlanguage`, else
/// babel's `main=` option or its last language option.
fn language(text: &str) -> Option<String> {
    let name = argument_of(text, "\\setdefaultlanguage")
        .or_else(|| argument_of(text, "\\setmainlanguage"))
        .or_else(|| {
            let options = usepackage_options(text, "babel");
            options
                .iter()
                .find_map(|o| o.strip_prefix("main=").map(str::to_owned))
                .or_else(|| options.iter().rev().find(|o| !o.contains('=')).cloned())
        })?;
    Some(language_name(&name))
}

/// A babel or polyglossia language name in plain English.
fn language_name(name: &str) -> String {
    let known = match name.trim() {
        "english" => "English",
        "british" | "UKenglish" | "en-GB" => "British English",
        "american" | "USenglish" | "en-US" => "American English",
        "canadian" => "Canadian English",
        "australian" => "Australian English",
        "german" | "ngerman" | "naustrian" | "austrian" => "German",
        "nswissgerman" | "swissgerman" => "Swiss German",
        "french" | "francais" | "frenchb" => "French",
        "italian" => "Italian",
        "spanish" => "Spanish",
        "portuguese" | "portuges" | "brazil" | "brazilian" => "Portuguese",
        "dutch" => "Dutch",
        "russian" => "Russian",
        "polish" => "Polish",
        "czech" => "Czech",
        "swedish" => "Swedish",
        "danish" => "Danish",
        "norsk" | "norwegian" | "nynorsk" => "Norwegian",
        "finnish" => "Finnish",
        "greek" => "Greek",
        "japanese" => "Japanese",
        "chinese" => "Chinese",
        other => {
            let mut chars = other.chars();
            return chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default();
        }
    };
    known.to_owned()
}

/// The `CLAUDE.md` text for `facts`.
pub fn instructions_text(facts: &ProjectFacts) -> String {
    let code = |p: &Path| format!("`{}`", p.display());
    let list = |paths: &[PathBuf]| paths.iter().map(|p| code(p)).collect::<Vec<_>>().join(", ");
    let mut out = String::from(
        "# Project instructions\n\n\
         This is a LaTeX project edited in Lemma. The user reviews every change you make as \
         red/green hunks and accepts or rejects each one.\n\n## Editing\n\n\
         - Edit files only through Lemma's tools (`mcp__lemma__edit`, `mcp__lemma__write`), \
         never with Bash or other tools.\n\
         - `mcp__lemma__read_file` returns the live editor buffer, including unsaved changes.\n\
         - Keep each change focused; do not reformat or reflow text you are not changing.\n\
         - Do not re-apply changes the user rejected unless asked.\n",
    );
    out.push_str("\n## Build\n\n");
    match (&facts.main, &facts.class) {
        (Some(main), Some(class)) => {
            out.push_str(&format!(
                "- Main document: {} (class `{class}`).\n",
                code(main)
            ));
        }
        (Some(main), None) => out.push_str(&format!("- Main document: {}.\n", code(main))),
        _ => out.push_str("- No main document was found yet.\n"),
    }
    let engine = match facts.engine {
        Some(Engine::PdfLatex) => Some("pdfLaTeX"),
        Some(Engine::LuaLatex) => Some("LuaLaTeX"),
        Some(Engine::XeLatex) => Some("XeLaTeX"),
        Some(Engine::Latexmkrc) => None,
        None => None,
    };
    match (engine, facts.engine) {
        (Some(engine), _) => out.push_str(&format!("- Engine: {engine}, run through latexmk.\n")),
        (None, Some(Engine::Latexmkrc)) => {
            out.push_str("- Engine: as the project's latexmkrc sets it.\n")
        }
        _ => {}
    }
    out.push_str(
        "- Build with `mcp__lemma__compile` and read errors with `mcp__lemma__get_diagnostics`; \
         do not run latexmk yourself.\n",
    );
    if facts.bibliography.is_some() || !facts.bib_files.is_empty() {
        out.push_str("\n## Bibliography\n\n");
        if let Some(tool) = &facts.bibliography {
            match &facts.bibliography_style {
                Some(style) => out.push_str(&format!("- {tool}, style `{style}`.\n")),
                None => out.push_str(&format!("- {tool}.\n")),
            }
        }
        if !facts.bib_files.is_empty() {
            out.push_str(&format!("- Entries: {}.\n", list(&facts.bib_files)));
        }
        out.push_str(
            "- Cite only entries that exist there; never invent bibliographic data. Ask before \
             adding an entry.\n",
        );
    }
    if !facts.macros.is_empty() || facts.preamble_macros {
        out.push_str("\n## Macros\n\n");
        let mut places = facts.macros.iter().map(|p| code(p)).collect::<Vec<_>>();
        if facts.preamble_macros
            && let Some(main) = &facts.main
        {
            places.push(format!("the preamble of {}", code(main)));
        }
        out.push_str(&format!(
            "- Macros are defined in {}. Use them instead of their expansion, and define new \
             notation there rather than inline.\n",
            places.join(" and ")
        ));
    }
    out.push_str("\n## Language\n\n");
    match &facts.language {
        Some(language) => out.push_str(&format!(
            "- The document is written in {language}. Keep its spelling and the terminology \
             already used in the text.\n"
        )),
        None => out.push_str("- Keep the spelling and the terminology already used in the text.\n"),
    }
    out
}

/// Writes `CLAUDE.md` into the project root (never over an existing one) and opens it.
/// `Ok(true)` when it was created, `Ok(false)` when one existed and was opened instead.
pub async fn create_project_instructions(state: &Rc<WindowState>) -> Result<bool, String> {
    let root = state.workspace.root().path().to_path_buf();
    let path = root.join(INSTRUCTIONS_FILE);
    let target = path.clone();
    let created = gio::spawn_blocking(move || -> Result<bool, String> {
        if target.symlink_metadata().is_ok() {
            return Ok(false);
        }
        let text = instructions_text(&infer(&root));
        match write_new(&target, &text) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(format!("Cannot write {}: {e}", target.display())),
        }
    })
    .await
    .unwrap_or_else(|_| Err("Creating CLAUDE.md failed".into()))?;
    state.open_file(&path).await?;
    if !created {
        state.toast("CLAUDE.md already exists; it was opened, not replaced");
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_has_a_main_document() {
        for t in TEMPLATES {
            let (_, main) = t
                .files
                .iter()
                .find(|(f, _)| *f == MAIN_FILE)
                .unwrap_or_else(|| panic!("{} has no main.tex", t.id));
            assert!(main.contains("\\documentclass"), "{}", t.id);
            assert!(main.contains("\\begin{document}"), "{}", t.id);
            assert!(main.contains("\\end{document}"), "{}", t.id);
        }
        let ids: Vec<&str> = TEMPLATES.iter().map(|t| t.id).collect();
        assert_eq!(ids, ["amsart", "article", "beamer", "thesis", "minimal"]);
        assert!(
            template("thesis")
                .unwrap()
                .files
                .iter()
                .any(|(f, _)| f.starts_with("chapters/"))
        );
        assert!(template("thesis").unwrap().files[0].1.contains("{book}"));
    }

    #[test]
    fn helpers_read_the_preamble() {
        let text = "\\documentclass[11pt]{amsart}\n\\usepackage[british, main=ngerman]{babel}\n\
                    \\usepackage[backend=bibtex,style=alphabetic]{biblatex}\n\
                    \\bibliographystyle{amsalpha}\n";
        assert_eq!(
            argument_of(text, "\\documentclass").as_deref(),
            Some("amsart")
        );
        assert_eq!(
            argument_of(text, "\\bibliographystyle").as_deref(),
            Some("amsalpha")
        );
        assert_eq!(argument_of(text, "\\bibliography"), None);
        assert_eq!(
            usepackage_options(text, "biblatex"),
            ["backend=bibtex", "style=alphabetic"]
        );
        assert_eq!(language(text).as_deref(), Some("German"));
        assert_eq!(
            language("\\usepackage[french,english]{babel}").as_deref(),
            Some("English")
        );
        assert_eq!(
            language("\\usepackage{polyglossia}\\setdefaultlanguage{british}").as_deref(),
            Some("British English")
        );
        assert_eq!(
            language("\\usepackage[klingon]{babel}").as_deref(),
            Some("Klingon")
        );
        assert_eq!(language("\\usepackage{amsmath}"), None);
    }
}
