//! The set of documents of one project (one per window).
//!
//! Documents are keyed by their normalized absolute path ([`normalize`]). A document is *open*
//! while an editor shows it and *closed* otherwise; closed documents stay in the workspace only
//! while something needs them (an agent-edit review, WP1.2).

use crate::document::{self, DocRef, Document};
use crate::saver::{self, DiskState, SaveError};
use gio::prelude::*;
use gtk::prelude::*;
use lemma_core::ProjectRoot;
use lemma_syntax::{ProjectIndex, ScanConfig};
use sourceview5::prelude::*;
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// The project's source files at the start of an agent turn (ARCHITECTURE §4.2 item 1), taken
/// by [`Workspace::turn_snapshot`]: the baselines of files that a program other than our tools
/// (a `sed -i`, a subagent) changes during the turn.
#[derive(Clone, Debug, Default)]
pub struct TurnSnapshot {
    /// The text of every project source file ([`is_source_file`]) that was **not** in the
    /// workspace (documents in the workspace carry their own text), while the total stays under
    /// the cap. Keys are normalized absolute paths.
    pub files: HashMap<PathBuf, String>,
    /// Every project source file that existed, whether or not its text is in `files`.
    pub existed: HashSet<PathBuf>,
    /// True when the walk saw the whole project (no cap cut it short), so a source file missing
    /// from `existed` did not exist when the turn started.
    pub complete: bool,
}

/// The snapshot cap of [`Workspace::turn_snapshot`] (`agent.snapshot_cap_mib`'s default).
pub const DEFAULT_SNAPSHOT_CAP: usize = 4 << 20;

/// File extensions of the project's sources: the files a turn snapshot keeps and the file
/// monitor reviews when they change during a turn. Build products (`.aux`, `.log`, `.pdf`, …)
/// are not among them.
pub const SOURCE_EXTENSIONS: &[&str] = &[
    "tex", "ltx", "latex", "sty", "cls", "bib", "bbx", "cbx", "dtx", "ins", "def", "cfg", "clo",
    "bst", "txt", "md",
];

/// True when `path` has one of the [`SOURCE_EXTENSIONS`].
pub fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SOURCE_EXTENSIONS.iter().any(|s| s.eq_ignore_ascii_case(e)))
}

/// What [`walk_project`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectWalk {
    /// Regular files, breadth first, each directory's entries in name order.
    pub files: Vec<PathBuf>,
    /// The directories visited, the root first (breadth first).
    pub dirs: Vec<PathBuf>,
    /// True when a cap stopped the walk before it saw everything.
    pub truncated: bool,
}

/// Lists the files and directories under `root`, breadth first. **Blocking**: run it on the
/// jobs thread. Hidden files and directories (`.git`, `.build`, editor temporaries), symbolic
/// links (they could leave the project) and the directories in `skip` (absolute paths, e.g. the
/// build output directory) are left out. Stops after `max_files` files or `max_dirs`
/// directories, setting `truncated`.
pub fn walk_project(
    root: &Path,
    skip: &[PathBuf],
    max_files: usize,
    max_dirs: usize,
) -> ProjectWalk {
    walk(root, skip, Some(max_files), max_dirs)
}

/// [`walk_project`] for the directories only (`files` stays empty): what the file monitor
/// watches.
pub fn walk_project_dirs(root: &Path, skip: &[PathBuf], max_dirs: usize) -> ProjectWalk {
    walk(root, skip, None, max_dirs)
}

fn walk(root: &Path, skip: &[PathBuf], max_files: Option<usize>, max_dirs: usize) -> ProjectWalk {
    let mut walk = ProjectWalk::default();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        if walk.dirs.len() >= max_dirs {
            walk.truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        walk.dirs.push(dir);
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            // `file_type` does not follow symbolic links.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if !skip.iter().any(|s| s == &path) {
                    queue.push_back(path);
                }
            } else if kind.is_file()
                && let Some(max_files) = max_files
            {
                if walk.files.len() >= max_files {
                    walk.truncated = true;
                    return walk;
                }
                walk.files.push(path);
            }
        }
    }
    if !queue.is_empty() {
        walk.truncated = true;
    }
    walk
}

/// Reads the snapshot of a turn (blocking; see [`Workspace::turn_snapshot_with`]).
fn take_snapshot(
    root: &Path,
    in_workspace: &HashSet<PathBuf>,
    cap: usize,
    skip: &[PathBuf],
) -> TurnSnapshot {
    let walk = walk_project(root, skip, SNAPSHOT_MAX_FILES, SNAPSHOT_MAX_DIRS);
    let mut sources: Vec<(PathBuf, u64)> = walk
        .files
        .into_iter()
        .filter(|p| is_source_file(p))
        .map(|p| {
            let size = std::fs::metadata(&p).map_or(u64::MAX, |m| m.len());
            (normalize(&p), size)
        })
        .collect();
    let existed: HashSet<PathBuf> = sources.iter().map(|(p, _)| p.clone()).collect();
    // TeX sources first, then the rest; small files first within each group, so the cap keeps
    // as many files as possible.
    let rank = |p: &Path| match p.extension().and_then(|e| e.to_str()) {
        Some("tex" | "ltx" | "latex") => 0,
        Some("bib" | "sty" | "cls") => 1,
        _ => 2,
    };
    sources.sort_by(|(a, sa), (b, sb)| (rank(a), sa, a).cmp(&(rank(b), sb, b)));
    let mut files = HashMap::new();
    let mut used = 0usize;
    for (path, size) in sources {
        if in_workspace.contains(&path) {
            continue;
        }
        let Ok(size) = usize::try_from(size) else {
            continue;
        };
        if used.saturating_add(size) > cap {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(decoded) = saver::decode(&bytes, None) {
            used += bytes.len();
            files.insert(path, decoded.text);
        }
    }
    TurnSnapshot {
        files,
        existed,
        complete: !walk.truncated,
    }
}

/// Caps of the turn snapshot's walk: a project root that is a large directory (the home
/// directory, when a loose file was opened) must not make the turn start slowly.
const SNAPSHOT_MAX_FILES: usize = 20_000;
const SNAPSHOT_MAX_DIRS: usize = 2_000;

/// All documents of a project, plus the project-wide structure index.
pub struct Workspace {
    root: RefCell<ProjectRoot>,
    docs: RefCell<HashMap<PathBuf, DocRef>>,
    /// Structure of every project file (filled by the structure pass, M2).
    pub index: RefCell<ProjectIndex>,
    /// The scanner rules of the documents' line states.
    scan_config: RefCell<Arc<ScanConfig>>,
}

impl Workspace {
    /// An empty workspace rooted at `root`.
    pub fn new(root: ProjectRoot) -> Rc<Self> {
        Rc::new(Self {
            root: RefCell::new(root),
            docs: RefCell::new(HashMap::new()),
            index: RefCell::new(ProjectIndex::default()),
            scan_config: RefCell::new(Arc::new(ScanConfig::default())),
        })
    }

    /// The scanner rules new documents get (VimTeX's rules plus `editor.math_envs`).
    pub fn scan_config(&self) -> Arc<ScanConfig> {
        self.scan_config.borrow().clone()
    }

    /// Uses `config` for every document's scanner states, now and for documents added later
    /// (open documents rebuild theirs when the rules changed).
    pub fn set_scan_config(&self, config: Arc<ScanConfig>) {
        self.scan_config.replace(config.clone());
        for doc in self.documents() {
            doc.set_scan_config(config.clone());
        }
    }

    /// The project root; agent paths are resolved against it.
    pub fn root(&self) -> Ref<'_, ProjectRoot> {
        self.root.borrow()
    }

    /// Moves the project root (e.g. to the directory of the first document opened).
    pub fn set_root(&self, root: ProjectRoot) {
        self.root.replace(root);
    }

    /// The document for `path`, if the workspace has it.
    pub fn get(&self, path: &Path) -> Option<DocRef> {
        self.docs.borrow().get(&normalize(path)).cloned()
    }

    /// Every document, open or closed, in no particular order.
    pub fn documents(&self) -> Vec<DocRef> {
        self.docs.borrow().values().cloned().collect()
    }

    /// True when the workspace has no documents.
    pub fn is_empty(&self) -> bool {
        self.docs.borrow().is_empty()
    }

    /// Documents with unsaved changes.
    pub fn dirty_documents(&self) -> Vec<DocRef> {
        self.docs
            .borrow()
            .values()
            .filter(|d| d.is_dirty())
            .cloned()
            .collect()
    }

    /// Returns `path` as an open document (with a buffer), reading it if needed. A closed
    /// document gets a buffer with its current text. A file that does not exist yet becomes an
    /// empty document that the first save creates.
    ///
    /// GtkSourceView's `FileLoader` decodes the file (UTF-8, else ISO-8859-15, …; the document is
    /// saved in the same encoding). The buffer keeps its bytes exactly, line terminators and a
    /// byte-order mark included. Binary files (with U+0000) are refused.
    ///
    /// A closed document whose file another program changed since it was read or written (a
    /// `git checkout` after its review was decided) is read again, as long as it holds nothing
    /// that is not on disk (no pending hunks, no unsaved changes); otherwise its text is shown
    /// and saving it asks before overwriting the other program's change.
    pub fn open(
        self: &Rc<Self>,
        path: &Path,
    ) -> impl Future<Output = Result<DocRef, String>> + use<> {
        let path = normalize(path);
        let this = self.clone();
        async move {
            if let Some(doc) = this.get(&path)
                && !doc.is_open()
            {
                this.drop_if_stale(&doc).await;
            }
            if let Some(doc) = this.get(&path) {
                this.ensure_open(&doc)?;
                return Ok(doc);
            }
            let cannot_open = |e: &str| format!("cannot open {}: {e}", path.display());
            let location = gio::File::for_path(&path);
            let read = match location.load_contents_future().await {
                Ok(read) => Some(read),
                Err(e) if e.matches(gio::IOErrorEnum::NotFound) => {
                    tracing::info!("{} does not exist yet; opening it empty", path.display());
                    None
                }
                Err(e) => return Err(cannot_open(e.message())),
            };
            let buffer = document::new_buffer(&path);
            let file = sourceview5::File::new();
            file.set_location(Some(&location));
            let disk = match read {
                Some((bytes, etag)) => {
                    let bytes = glib::Bytes::from_owned(bytes);
                    load(&buffer, &file, &bytes)
                        .await
                        .map_err(|e| cannot_open(&e))?;
                    DiskState::of(&bytes, etag)
                }
                None => DiskState::Absent,
            };
            // Another caller may have opened the same file while this one was reading.
            if let Some(doc) = this.get(&path) {
                this.ensure_open(&doc)?;
                return Ok(doc);
            }
            buffer.place_cursor(&buffer.start_iter());
            let charset = match &disk {
                DiskState::Present { .. } => Some(file.encoding().charset().to_string())
                    .filter(|c| !c.eq_ignore_ascii_case("UTF-8")),
                _ => None,
            };
            let doc = Document::new_open_with_scan(path.clone(), buffer, file, this.scan_config());
            doc.saver.set_loaded(charset, disk);
            this.docs.borrow_mut().insert(path, doc.clone());
            Ok(doc)
        }
    }

    /// Drops the closed document `doc` from the workspace when its text is stale: the file on
    /// disk differs from what the document last read or wrote, and the document holds nothing
    /// else (no pending hunks, no unsaved changes), so the file can simply be read again. A
    /// review with nothing left to decide goes with it.
    async fn drop_if_stale(&self, doc: &DocRef) {
        let disk = match gio::File::for_path(doc.path()).load_contents_future().await {
            Ok((bytes, _etag)) => Some(bytes),
            Err(e) if e.matches(gio::IOErrorEnum::NotFound) => None,
            // Unreadable now: keep what we have.
            Err(_) => return,
        };
        // The document may have been opened, changed or dropped meanwhile.
        if doc.is_open() || !self.get(doc.path()).is_some_and(|d| Rc::ptr_eq(&d, doc)) {
            return;
        }
        let pending = doc.review.borrow().as_ref().is_some_and(|r| !r.is_clean());
        if pending || doc.is_dirty() {
            return;
        }
        // The document has no unsaved changes, so comparing the texts is exact. (Its own-write
        // hashes are not asked: the file may hold an earlier own write put back by another
        // program, which this document no longer holds.)
        let fresh = match &disk {
            Some(bytes) => saver::decode(bytes, doc.charset().as_deref())
                .is_ok_and(|d| doc.with_text(|text| text == d.text)),
            // Deleted: an empty new document stands in for it, as for any missing file.
            None => false,
        };
        if fresh {
            return;
        }
        tracing::info!(
            "{} changed on disk; reading it again instead of its stale closed document",
            doc.path().display()
        );
        crate::review_ui::drop_review(doc);
        self.docs.borrow_mut().remove(doc.path());
    }

    fn ensure_open(&self, doc: &DocRef) -> Result<(), String> {
        if !doc.is_open() {
            doc.open_in_buffer(&document::new_buffer(doc.path()));
        }
        if doc.is_open() {
            Ok(())
        } else {
            Err(format!(
                "cannot open {}: it contains NUL characters",
                doc.path().display()
            ))
        }
    }

    /// Returns the document for `path`, open or closed; a file the workspace does not have yet is
    /// read with GIO (decoded as [`Workspace::open`] does) and added as a closed document (no
    /// buffer is created).
    pub fn get_or_load(
        self: &Rc<Self>,
        path: &Path,
    ) -> impl Future<Output = Result<DocRef, String>> + use<> {
        let path = normalize(path);
        let this = self.clone();
        async move {
            if let Some(doc) = this.get(&path) {
                return Ok(doc);
            }
            let (decoded, disk) = read(&path)
                .await
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if let Some(doc) = this.get(&path) {
                return Ok(doc);
            }
            let doc = Document::new_closed(path.clone(), decoded.text);
            doc.set_scan_config(this.scan_config());
            doc.saver.set_loaded(decoded.charset, disk);
            this.docs.borrow_mut().insert(path, doc.clone());
            Ok(doc)
        }
    }

    /// Adds an empty closed document for `path`, a file that does not exist yet (the agent's
    /// `write` of a new file), or returns the document the workspace already has. Nothing is
    /// written; the first save creates the file (and is refused if another program created it
    /// meanwhile).
    pub fn create_closed(&self, path: &Path) -> DocRef {
        let path = normalize(path);
        if let Some(doc) = self.get(&path) {
            return doc;
        }
        let doc = Document::new_closed(path.clone(), String::new());
        doc.set_scan_config(self.scan_config());
        doc.saver.set_loaded(None, DiskState::Absent);
        self.docs.borrow_mut().insert(path, doc.clone());
        doc
    }

    /// Called when the editor showing `doc` closes: a document under review stays as a closed
    /// document (with the buffer's text); any other document leaves the workspace.
    pub fn release(&self, doc: &DocRef) {
        if doc.review.borrow().is_some() {
            doc.close_buffer();
        } else {
            self.docs.borrow_mut().remove(doc.path());
        }
    }

    /// Saves every dirty document, concurrently. Returns the failures, if any.
    pub fn save_all_dirty(
        &self,
    ) -> impl Future<Output = Result<(), Vec<(PathBuf, SaveError)>>> + use<> {
        let saves: Vec<_> = self
            .dirty_documents()
            .into_iter()
            .map(|doc| (doc.path().to_path_buf(), doc.save()))
            .collect();
        async move {
            let mut failures = Vec::new();
            for (path, save) in saves {
                if let Err(e) = save.await {
                    failures.push((path, e));
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(failures)
            }
        }
    }

    /// Does nothing: the window's file monitor ([`crate::agent::watcher`], installed with the
    /// window) watches the project, since what a change means (agent hunks, a reload, a conflict
    /// bar, a toast) depends on the window's agent turn and its widgets. Kept for API stability.
    pub fn start_file_monitor(&self) {}

    /// Snapshots the project's source files for an agent turn (ARCHITECTURE §4.2 item 1), with
    /// the default cap ([`DEFAULT_SNAPSHOT_CAP`]); see [`Workspace::turn_snapshot_with`].
    pub fn turn_snapshot(&self) -> impl Future<Output = TurnSnapshot> + use<> {
        self.turn_snapshot_with(DEFAULT_SNAPSHOT_CAP, Vec::new())
    }

    /// Snapshots, on the jobs thread, the text of every project source file that is not in the
    /// workspace, until the texts reach `cap_bytes` (TeX sources first, smaller files first), and
    /// records which source files exist. `skip` lists directories to leave out (the build output
    /// directory); hidden directories (`.git`, `.build`) are always left out.
    pub fn turn_snapshot_with(
        &self,
        cap_bytes: usize,
        skip: Vec<PathBuf>,
    ) -> impl Future<Output = TurnSnapshot> + use<> {
        let root = self.root().path().to_path_buf();
        let in_workspace: HashSet<PathBuf> = self.docs.borrow().keys().cloned().collect();
        async move {
            gio::spawn_blocking(move || take_snapshot(&root, &in_workspace, cap_bytes, &skip))
                .await
                .unwrap_or_default()
        }
    }

    /// Adds a closed document holding `text` for `path` (normalized), or returns the document
    /// the workspace already has. The caller records what the file holds on disk
    /// (`doc.saver.set_loaded`); until then saves do not check the file. Used by the file monitor
    /// to review a file that another program changed during an agent turn against its text from
    /// the turn snapshot.
    pub(crate) fn insert_closed(&self, path: &Path, text: String) -> DocRef {
        let path = normalize(path);
        if let Some(doc) = self.get(&path) {
            return doc;
        }
        let doc = Document::new_closed(path.clone(), text);
        doc.set_scan_config(self.scan_config());
        self.docs.borrow_mut().insert(path, doc.clone());
        doc
    }
}

/// Decodes `bytes` into `buffer` with GtkSourceView's `FileLoader`, which also records the
/// encoding and newline type in `file`.
///
/// The loader may take a Latin-1 file whose last bytes are not ASCII for broken UTF-8 (an
/// incomplete character at the end), and then fails with a conversion fallback; such a file is
/// loaded again with ISO-8859-15 as the only candidate. Files with U+0000 fail either way.
async fn load(
    buffer: &sourceview5::Buffer,
    file: &sourceview5::File,
    bytes: &glib::Bytes,
) -> Result<(), String> {
    let mut fallback = None;
    loop {
        let stream = gio::MemoryInputStream::from_bytes(bytes);
        let loader = sourceview5::FileLoader::from_stream(buffer, file, &stream);
        if let Some(encoding) = &fallback {
            set_candidate_encodings(&loader, &[encoding]);
        }
        let (loaded, _progress) = loader.load_future(glib::Priority::DEFAULT);
        match loaded.await {
            Ok(()) => return Ok(()),
            Err(e)
                if fallback.is_none()
                    && e.matches(sourceview5::FileLoaderError::ConversionFallback) =>
            {
                fallback = sourceview5::Encoding::from_charset(saver::FALLBACK_CHARSET);
                if fallback.is_none() {
                    return Err(e.message().to_owned());
                }
            }
            Err(e) => return Err(e.message().to_owned()),
        }
    }
}

/// `gtk_source_file_loader_set_candidate_encodings`, which the bindings do not wrap.
fn set_candidate_encodings(loader: &sourceview5::FileLoader, encodings: &[&sourceview5::Encoding]) {
    use glib::translate::ToGlibPtr;
    // SAFETY: the list holds borrowed encoding pointers that outlive the call; the loader copies
    // the list (`transfer none`), and only the list cells are freed afterwards.
    unsafe {
        let mut list: *mut glib::ffi::GSList = std::ptr::null_mut();
        for encoding in encodings.iter().rev() {
            let ptr: *const sourceview5::ffi::GtkSourceEncoding = encoding.to_glib_none().0;
            list = glib::ffi::g_slist_prepend(list, ptr as glib::ffi::gpointer);
        }
        sourceview5::ffi::gtk_source_file_loader_set_candidate_encodings(
            loader.to_glib_none().0,
            list,
        );
        glib::ffi::g_slist_free(list);
    }
}

/// Reads and decodes the file at `path` (see [`saver::decode`]) and records its state on disk.
async fn read(path: &Path) -> Result<(saver::Decoded, DiskState), String> {
    let (bytes, etag) = gio::File::for_path(path)
        .load_contents_future()
        .await
        .map_err(|e| e.message().to_owned())?;
    let decoded = saver::decode(&bytes, None)?;
    Ok((decoded, DiskState::of(&bytes, etag)))
}

/// The workspace key for `path`: absolute, with symlinks and `..` resolved. A file that does not
/// exist yet is resolved through its parent directory; relative paths are taken against the
/// current directory.
pub fn normalize(path: &Path) -> PathBuf {
    if let Ok(p) = lemma_core::canonicalize(path) {
        return p;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => {
            lemma_core::canonicalize(parent).map_or_else(|_| absolute.clone(), |p| p.join(name))
        }
        _ => absolute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dots_and_missing_files() {
        let dir = std::env::temp_dir();
        let canonical = lemma_core::canonicalize(&dir).unwrap();
        assert_eq!(normalize(&dir.join(".")), canonical);
        assert_eq!(
            normalize(&dir.join("lemma-no-such-file.tex")),
            canonical.join("lemma-no-such-file.tex")
        );
        assert!(normalize(Path::new("relative.tex")).is_absolute());
    }

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lemma-ws-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, text) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        lemma_core::canonicalize(&dir).unwrap()
    }

    #[test]
    fn walks_breadth_first_without_hidden_skipped_or_linked_entries() {
        let dir = tree(
            "walk",
            &[
                ("b.tex", ""),
                ("a.tex", ""),
                ("sub/deep/x.tex", ""),
                ("sub/y.bib", ""),
                ("out/main.pdf", ""),
                (".git/HEAD", ""),
                (".hidden.tex", ""),
            ],
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.join("etc-link")).unwrap();
        let skip = [dir.join("out")];
        let walk = walk_project(&dir, &skip, 100, 100);
        let rel = |paths: &[PathBuf]| -> Vec<String> {
            paths
                .iter()
                // `/` on Windows too, as below.
                .map(|p| {
                    p.strip_prefix(&dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect()
        };
        assert_eq!(
            rel(&walk.files),
            ["a.tex", "b.tex", "sub/y.bib", "sub/deep/x.tex"]
        );
        assert_eq!(rel(&walk.dirs), ["", "sub", "sub/deep"]);
        assert!(!walk.truncated);
        let capped = walk_project(&dir, &skip, 2, 100);
        assert_eq!(rel(&capped.files), ["a.tex", "b.tex"]);
        assert!(capped.truncated);
        let dirs = walk_project_dirs(&dir, &skip, 2);
        assert!(dirs.files.is_empty());
        assert_eq!(rel(&dirs.dirs), ["", "sub"]);
        assert!(dirs.truncated);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshots_sources_outside_the_workspace_within_the_cap() {
        let dir = tree(
            "snapshot",
            &[
                ("main.tex", "main\n"),
                ("ch.tex", "chapter\n"),
                ("refs.bib", "@book{x}\n"),
                ("big.tex", &"x".repeat(100)),
                ("notes.md", "notes\n"),
                ("main.pdf", "%PDF"),
                ("latin.tex", ""),
            ],
        );
        std::fs::write(dir.join("latin.tex"), b"caf\xe9\n").unwrap();
        let open = HashSet::from([dir.join("main.tex")]);
        let snapshot = take_snapshot(&dir, &open, 40, &[]);
        let keys: HashSet<String> = snapshot
            .files
            .keys()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // main.tex is in the workspace; big.tex does not fit; TeX files first, then the rest.
        assert_eq!(
            keys,
            HashSet::from(["ch.tex", "latin.tex", "refs.bib", "notes.md"].map(String::from))
        );
        assert_eq!(snapshot.files[&dir.join("latin.tex")], "café\n");
        assert!(snapshot.existed.contains(&dir.join("main.tex")));
        assert!(snapshot.existed.contains(&dir.join("big.tex")));
        assert!(!snapshot.existed.contains(&dir.join("main.pdf")));
        assert!(snapshot.complete);
        assert!(is_source_file(Path::new("a/b.TEX")));
        assert!(!is_source_file(Path::new("a/b.aux")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
