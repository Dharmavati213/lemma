//! Files and folders linked to the agent: reference material it always gets in a project (the
//! pin button in the agent pane, beside the paperclip).
//!
//! # The list
//!
//! [`LinkedFiles`] (one per window, [`AgentController::linked_files`]) holds the absolute paths
//! linked to the window's project, in the order they were linked. It is kept in
//! `paths.linked_files_dir()/<fnv(root) as 16 hex>.json` (private to the user, like the
//! session), read when the window opens and when the project root moves, and written after every
//! change (an empty list removes the file). Windows without `paths` and runs that quit by
//! themselves keep it in memory only.
//!
//! # In the prompt ([`for_prompt`])
//!
//! A folder stands for the documents in it and in its subfolders (hidden ones left out): PDFs and
//! the text files `@` offers, at most [`MAX_FOLDER_FILES`] per folder. Every prompt starts (after
//! the review feedback) with a `<linked_files>` text that lists them. A file the conversation has
//! not been given yet, or that changed since (its size or modification time), is attached after
//! that text as the paperclip attaches it ([`attach::from_file`]): text inline, a PDF inline for
//! the API backend and as a link for the ACP agents, which read it with their own tools. Text
//! beyond [`MAX_TEXT_BYTES`] in one prompt goes as links (the API backend leaves it out). The
//! bridge keeps what a conversation was given with its session ([`Given`]), so a new
//! conversation gets everything again and one continued after an idle stop does not; a slash
//! command goes without them, and `/clear` and `/compact` forget what was given. A prompt that
//! sends files shows a notice naming them in the transcript.
//!
//! [`AgentController::linked_files`]: super::AgentController::linked_files

use super::PromptBlock;
use super::attach;
use super::panel::TEXT_EXTENSIONS;
use crate::hooks::{HookId, Hooks};
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::fnv1a64;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Documents sent for one linked folder, at most.
pub const MAX_FOLDER_FILES: usize = 50;
/// Text of linked files included in one prompt, at most (about 250k tokens); further text files
/// are sent as links.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// The format of [`ListFile`].
const VERSION: u32 = 1;

/// A file as a conversation was given it: its size and modification time.
pub type Stamp = (u64, SystemTime);
/// What a conversation has been given of the linked files.
pub type Given = HashMap<PathBuf, Stamp>;

/// The contents of a project's list file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ListFile {
    version: u32,
    /// The project root, for people reading the file (the file name is its hash).
    #[serde(default)]
    root: String,
    #[serde(default)]
    files: Vec<PathBuf>,
}

/// The files and folders linked to the agent in a window's project (see the module docs).
pub struct LinkedFiles {
    /// Where the lists are kept; `None`: in memory only.
    dir: Option<PathBuf>,
    root: RefCell<PathBuf>,
    paths: RefCell<Vec<PathBuf>>,
    changed: Hooks<dyn Fn()>,
}

impl LinkedFiles {
    /// The list of the project at `root`, read from `dir` (`None`: kept in memory only).
    pub fn new(dir: Option<PathBuf>, root: &Path) -> Rc<Self> {
        let paths = dir
            .as_deref()
            .map(|d| read_list(&file_for_root(d, root)))
            .unwrap_or_default();
        Rc::new(Self {
            dir,
            root: RefCell::new(root.to_path_buf()),
            paths: RefCell::new(paths),
            changed: Hooks::default(),
        })
    }

    /// The project root moved to `root`: its list replaces the one there was.
    pub fn set_root(&self, root: &Path) {
        if *self.root.borrow() == root {
            return;
        }
        let paths = self
            .dir
            .as_deref()
            .map(|d| read_list(&file_for_root(d, root)))
            .unwrap_or_default();
        self.root.replace(root.to_path_buf());
        self.paths.replace(paths);
        self.emit();
    }

    /// The linked paths, in the order they were linked.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.paths.borrow().clone()
    }

    /// Links those of `paths` (absolute) that are not linked yet; returns how many were added.
    pub fn add(&self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let added = {
            let mut list = self.paths.borrow_mut();
            let before = list.len();
            for path in paths {
                if path.is_absolute() && !list.contains(&path) {
                    list.push(path);
                }
            }
            list.len() - before
        };
        if added > 0 {
            self.save();
            self.emit();
        }
        added
    }

    /// Unlinks `path`; false when it was not linked.
    pub fn remove(&self, path: &Path) -> bool {
        let removed = {
            let mut list = self.paths.borrow_mut();
            let before = list.len();
            list.retain(|p| p != path);
            list.len() != before
        };
        if removed {
            self.save();
            self.emit();
        }
        removed
    }

    /// Calls `f` after every change of the list.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> HookId {
        self.changed.add(Rc::new(f))
    }

    fn emit(&self) {
        for f in self.changed.snapshot() {
            f();
        }
    }

    /// Writes the list on gio's thread pool; a write overtaken by a newer one of the same file
    /// is skipped.
    fn save(&self) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        static WRITTEN: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);
        let Some(dir) = &self.dir else {
            return;
        };
        let root = self.root.borrow().clone();
        let file = file_for_root(dir, &root);
        let list = ListFile {
            version: VERSION,
            root: root.to_string_lossy().into_owned(),
            files: self.paths(),
        };
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        glib::spawn_future_local(async move {
            let written = gio::spawn_blocking(move || {
                let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
                let last = written
                    .get_or_insert_default()
                    .entry(file.clone())
                    .or_default();
                if *last > seq {
                    return Ok(());
                }
                *last = seq;
                write_list(&file, &list)
            })
            .await;
            if let Ok(Err(e)) = written {
                tracing::warn!("could not save the linked files: {e}");
            }
        });
    }
}

/// The list file of the project at `root`.
fn file_for_root(dir: &Path, root: &Path) -> PathBuf {
    let hash = fnv1a64(&[root.as_os_str().as_encoded_bytes()]);
    dir.join(format!("{hash:016x}.json"))
}

/// The paths a list file holds (none when it is missing or cannot be read).
fn read_list(file: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    match serde_json::from_slice::<ListFile>(&bytes) {
        Ok(list) => list.files,
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", file.display());
            Vec::new()
        }
    }
}

/// Writes `list` to `file` through a private temporary file and a rename; an empty list removes
/// the file.
fn write_list(file: &Path, list: &ListFile) -> std::io::Result<()> {
    use std::io::Write as _;
    if list.files.is_empty() {
        return match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if let Some(dir) = file.parent() {
        crate::fsutil::private_dir_builder().create(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = crate::fsutil::private_file_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .and_then(|()| std::fs::rename(&tmp, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

// ----- the prompt -----

/// What a linked path stands for.
#[derive(Debug, PartialEq)]
enum Listed {
    File {
        path: PathBuf,
        stamp: Stamp,
    },
    /// A linked path that is gone or cannot be read.
    Missing(PathBuf),
    /// A folder without documents.
    Empty(PathBuf),
    /// A folder's documents beyond [`MAX_FOLDER_FILES`]: how many were left out.
    More {
        folder: PathBuf,
        left_out: usize,
    },
}

/// The documents of the linked `paths` (blocking: reads folders).
fn expand(paths: &[PathBuf]) -> Vec<Listed> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut file = |path: PathBuf, out: &mut Vec<Listed>| {
        if !seen.insert(path.clone()) {
            return;
        }
        match std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)))
        {
            Some(stamp) => out.push(Listed::File { path, stamp }),
            None => out.push(Listed::Missing(path)),
        }
    };
    for path in paths {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                let mut documents = Vec::new();
                documents_in(path, &mut documents);
                documents.sort();
                if documents.is_empty() {
                    out.push(Listed::Empty(path.clone()));
                }
                let left_out = documents.len().saturating_sub(MAX_FOLDER_FILES);
                for document in documents.into_iter().take(MAX_FOLDER_FILES) {
                    file(document, &mut out);
                }
                if left_out > 0 {
                    out.push(Listed::More {
                        folder: path.clone(),
                        left_out,
                    });
                }
            }
            Ok(_) => file(path.clone(), &mut out),
            Err(_) => out.push(Listed::Missing(path.clone())),
        }
    }
    out
}

/// Adds the documents in `dir` and its subfolders to `out` (hidden entries and symbolic links to
/// folders left out).
fn documents_in(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => documents_in(&path, out),
            Ok(_) if is_document(&path) => out.push(path),
            _ => {}
        }
    }
}

/// A PDF or a text file `@` offers.
fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| e == "pdf" || TEXT_EXTENSIONS.contains(&e.as_str()))
}

/// What the linked files add to a prompt.
#[derive(Debug, Default)]
pub struct ForPrompt {
    /// The `<linked_files>` text, then the files attached.
    pub blocks: Vec<PromptBlock>,
    /// The files this prompt gives the conversation.
    pub given: Vec<(PathBuf, Stamp)>,
    /// For the transcript: the files sent, and what could not be.
    pub notice: Option<String>,
}

/// The blocks for the linked `paths` in a conversation that was given `given` (see the module
/// docs). `embed_only`: the agent cannot open files itself (the API backend).
pub async fn for_prompt(paths: Vec<PathBuf>, given: Given, embed_only: bool) -> ForPrompt {
    if paths.is_empty() {
        return ForPrompt::default();
    }
    let listed = gio::spawn_blocking(move || expand(&paths))
        .await
        .unwrap_or_default();
    let mut text = String::from(
        "<linked_files>\nThe user linked these files to this project so that you always have \
         them: take them into account in all your work here. They are background material, not \
         part of the request.\n",
    );
    let mut attached = Vec::new();
    let mut out = ForPrompt::default();
    let (mut sent, mut problems) = (Vec::new(), Vec::new());
    let mut budget = MAX_TEXT_BYTES;
    for item in listed {
        match item {
            Listed::File { path, stamp } => {
                let shown = path.display();
                if given.get(&path) == Some(&stamp) {
                    let again = if embed_only {
                        ""
                    } else {
                        " (read it again if you no longer have its contents)"
                    };
                    let _ = writeln!(
                        text,
                        "- {shown}: given to you earlier in this conversation, unchanged \
                         since{again}"
                    );
                    continue;
                }
                let attachment =
                    match attach::from_file(&gio::File::for_path(&path), embed_only).await {
                        Ok(attachment) => attachment,
                        Err(e) => {
                            let _ = writeln!(text, "- {shown}: could not be sent ({e})");
                            problems.push(e);
                            continue;
                        }
                    };
                let mut link = attachment.is_link();
                let mut block = attachment.block;
                if let PromptBlock::Resource {
                    uri,
                    mime,
                    text: body,
                } = &block
                {
                    if body.len() <= budget {
                        budget -= body.len();
                    } else if embed_only {
                        let _ = writeln!(
                            text,
                            "- {shown}: left out, too much text with the other linked files"
                        );
                        problems.push(format!(
                            "{} was left out: too much text with the other linked files",
                            attachment.name
                        ));
                        continue;
                    } else {
                        block = PromptBlock::Document {
                            uri: uri.clone(),
                            mime: mime.clone(),
                            data: Vec::new(),
                        };
                        link = true;
                    }
                }
                let how = if link {
                    "attached below as a link: read it with your own tools"
                } else {
                    "attached below"
                };
                let _ = writeln!(text, "- {shown}: {how}");
                attached.push(block);
                sent.push(attachment.name);
                out.given.push((path, stamp));
            }
            Listed::Missing(path) => {
                let _ = writeln!(text, "- {}: missing", path.display());
                problems.push(format!("{} is missing", path.display()));
            }
            Listed::Empty(path) => {
                let _ = writeln!(text, "- {}: a folder without documents", path.display());
            }
            Listed::More { folder, left_out } => {
                let _ = writeln!(
                    text,
                    "- {}: {left_out} more files in this folder were not sent (at most \
                     {MAX_FOLDER_FILES} per folder)",
                    folder.display()
                );
                problems.push(format!(
                    "{left_out} files of {} were not sent (at most {MAX_FOLDER_FILES} per folder)",
                    folder.display()
                ));
            }
        }
    }
    text.push_str("</linked_files>");
    out.blocks.push(PromptBlock::Text(text));
    out.blocks.extend(attached);
    // The problems are told with the first prompt of a conversation and whenever files go out.
    let mut notice = String::new();
    if !sent.is_empty() {
        notice = format!("Sent the linked files: {}.", sent.join(", "));
    }
    if !problems.is_empty() && (!sent.is_empty() || given.is_empty()) {
        if !notice.is_empty() {
            notice.push(' ');
        }
        let _ = write!(notice, "Linked files not sent: {}.", problems.join("; "));
    }
    out.notice = (!notice.is_empty()).then_some(notice);
    out
}

// ----- the button -----

/// The agent pane's pin button: the number of linked files and folders, and a popover that lists
/// them (each with a button that unlinks it) and links more.
pub fn button(state: &Rc<WindowState>, linked: &Rc<LinkedFiles>) -> gtk::MenuButton {
    let count = gtk::Label::builder().css_classes(["numeric"]).build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    content.append(&gtk::Image::from_icon_name("view-pin-symbolic"));
    content.append(&count);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.set_placeholder(Some(
        &gtk::Label::builder()
            .label("Nothing is linked yet")
            .margin_top(12)
            .margin_bottom(12)
            .css_classes(["dim-label"])
            .build(),
    ));
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();
    let link_files = gtk::Button::with_mnemonic("Link _Files…");
    let link_folder = gtk::Button::with_mnemonic("Link F_older…");
    let actions = gtk::Box::builder().spacing(6).homogeneous(true).build();
    actions.append(&link_files);
    actions.append(&link_folder);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_width_request(300);
    body.append(
        &gtk::Label::builder()
            .label("Linked Files")
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    body.append(
        &gtk::Label::builder()
            .label(
                "The agent gets these with the first message of every conversation in this \
                 project, and again when they change. A folder stands for the PDFs and text \
                 files in it.",
            )
            .wrap(true)
            .max_width_chars(40)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    body.append(&scroller);
    body.append(&actions);
    // Above the button, which is at the bottom of the pane.
    let popover = gtk::Popover::builder()
        .child(&body)
        .position(gtk::PositionType::Top)
        .build();
    let button = gtk::MenuButton::builder()
        .child(&content)
        .popover(&popover)
        .tooltip_text("Linked Files: always given to the agent in this project")
        .css_classes(["flat"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label("Linked files")]);

    let refresh = {
        let (weak, list, count) = (Rc::downgrade(linked), list.downgrade(), count.downgrade());
        move || {
            let (Some(linked), Some(list), Some(count)) =
                (weak.upgrade(), list.upgrade(), count.upgrade())
            else {
                return;
            };
            let paths = linked.paths();
            list.remove_all();
            for path in &paths {
                list.append(&row(path, &weak));
            }
            count.set_label(&paths.len().to_string());
            count.set_visible(!paths.is_empty());
        }
    };
    refresh();
    linked.connect_changed(refresh);
    let choose = |folders: bool| {
        let (state, linked, popover) =
            (Rc::downgrade(state), Rc::downgrade(linked), popover.clone());
        move |_: &gtk::Button| {
            popover.popdown();
            if let (Some(state), Some(linked)) = (state.upgrade(), linked.upgrade()) {
                choose_and_link(&state, &linked, folders);
            }
        }
    };
    link_files.connect_clicked(choose(false));
    link_folder.connect_clicked(choose(true));
    button
}

/// A row of the popover's list: the file's icon, name and folder, and the unlink button.
fn row(path: &Path, linked: &Weak<LinkedFiles>) -> gtk::ListBoxRow {
    let icon = if path.is_dir() {
        "folder-symbolic"
    } else if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    {
        "x-office-document-symbolic"
    } else {
        "text-x-generic-symbolic"
    };
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let folder = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    texts.set_hexpand(true);
    texts.append(
        &gtk::Label::builder()
            .label(name.as_str())
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build(),
    );
    texts.append(
        &gtk::Label::builder()
            .label(folder.as_str())
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Start)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    let unlink = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Unlink")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    unlink.update_property(&[gtk::accessible::Property::Label(&format!("Unlink {name}"))]);
    let (weak, owned) = (linked.clone(), path.to_path_buf());
    unlink.connect_clicked(move |_| {
        if let Some(linked) = weak.upgrade() {
            linked.remove(&owned);
        }
    });
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_margin_start(8);
    content.set_margin_end(4);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&texts);
    content.append(&unlink);
    gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(false)
        .tooltip_text(path.display().to_string())
        .build()
}

/// A file chooser for files (`folders`: folders) to link.
fn choose_and_link(state: &Rc<WindowState>, linked: &Rc<LinkedFiles>, folders: bool) {
    let dialog = gtk::FileDialog::builder()
        .title(if folders {
            "Link Folders to the Agent"
        } else {
            "Link Files to the Agent"
        })
        .accept_label("_Link")
        .modal(true)
        .initial_folder(&gio::File::for_path(state.workspace.root().path()))
        .build();
    let (window, state, linked) = (
        state.window.clone(),
        Rc::downgrade(state),
        Rc::downgrade(linked),
    );
    glib::spawn_future_local(async move {
        let chosen = if folders {
            dialog.select_multiple_folders_future(Some(&window)).await
        } else {
            dialog.open_multiple_future(Some(&window)).await
        };
        // Dismissed or cancelled: nothing to do.
        let Ok(model) = chosen else {
            return;
        };
        let paths: Vec<PathBuf> = (0..model.n_items())
            .filter_map(|i| model.item(i).and_downcast::<gio::File>()?.path())
            .collect();
        let (Some(state), Some(linked)) = (state.upgrade(), linked.upgrade()) else {
            return;
        };
        let added = linked.add(paths);
        state.toast(&match added {
            0 => "Already linked".to_owned(),
            1 => "Linked: the agent gets it with your next message".to_owned(),
            n => format!("Linked {n}: the agent gets them with your next message"),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_stand_for_their_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("refs/old")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        for name in [
            "refs/b.pdf",
            "refs/a.tex",
            "refs/old/c.md",
            "refs/fig.png",
            ".hidden/d.tex",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let note = root.join("note.txt");
        std::fs::write(&note, "hello").unwrap();
        let files = |listed: &[Listed]| -> Vec<PathBuf> {
            listed
                .iter()
                .filter_map(|l| match l {
                    Listed::File { path, .. } => Some(path.strip_prefix(root).unwrap().to_owned()),
                    _ => None,
                })
                .collect()
        };
        let listed = expand(&[
            note.clone(),
            root.join("refs"),
            root.join("empty"),
            note.clone(),
        ]);
        assert_eq!(
            files(&listed),
            ["note.txt", "refs/a.tex", "refs/b.pdf", "refs/old/c.md"].map(PathBuf::from),
            "sorted, without images or hidden files, once each"
        );
        assert!(listed.contains(&Listed::Empty(root.join("empty"))));
        assert_eq!(
            expand(&[root.join("gone.tex")]),
            [Listed::Missing(root.join("gone.tex"))]
        );
    }

    #[test]
    fn the_list_is_kept_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let file = file_for_root(dir.path(), Path::new("/p/one"));
        assert_ne!(file, file_for_root(dir.path(), Path::new("/p/two")));
        let list = ListFile {
            version: VERSION,
            root: "/p/one".into(),
            files: vec!["/p/one/notes.md".into(), "/refs".into()],
        };
        write_list(&file, &list).unwrap();
        assert_eq!(read_list(&file), list.files);
        write_list(&file, &ListFile::default()).unwrap();
        assert!(!file.exists(), "an empty list removes the file");
        assert!(read_list(&file).is_empty());
    }
}
