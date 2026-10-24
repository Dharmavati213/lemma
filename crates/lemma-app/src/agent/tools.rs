//! Main-thread handlers of the MCP tools (WP2.4; ARCHITECTURE §10.4 tool table).
//!
//! [`handle`] answers one `ToolCall` **exactly once** on `call.reply` (a `SyncSender` of capacity
//! 1, so sending never blocks; a send error means the MCP thread timed out and is ignored), with
//! `Origin::Agent { call: call.call }` on every edit, and only after the change is on disk.
//! Every path goes through `ProjectRoot::resolve`; a refused one gets `"path outside project
//! root"`. The bridge spawns it with `glib::spawn_future_local(tools::handle(state, call))`.
//! Every reply also carries the review feedback queued since the last one
//! (`<review_feedback>…</review_feedback>`, see [`review_ui::drain_feedback`]).
//!
//! | Request | Reply |
//! |---|---|
//! | `ReadFile`, `Edit`, `Write` | [`agent_edits::apply`] |
//! | `ListFiles` | the files of the document (root first, from the include graph of the live texts), then the other files under the root; at most [`LIST_FILES_MAX`] entries; `.build`, `.git`, hidden directories, the build output directory and TeX's generated files are left out |
//! | `Outline` | `lemma_syntax::index_file` of the live text: sectioning, theorem-like environments, labels and includes with 1-based lines; without a path, every file of the document from its root |
//! | `Compile` | a `Reason::AgentCompile` build, awaited: status, ≤ [`COMPILE_MAX_ERRORS`] errors as `file:line: message`, warning and page counts |
//! | `Diagnostics` | the last build's problems (lines moved to the current text), optionally of one file |
//! | `Selection` | the active editor's file, cursor line and column, and selection (≤ [`SELECTION_MAX_CHARS`] characters) |
//! | `RenderPage` | a PNG at [`RENDER_SCALE`]×, at most [`RENDER_MAX_HEIGHT`] px tall (poppler on a render thread that keeps the PDF and its last pages), plus a copy in `runtime_dir/page-<n>.png` (removed when the agent stops); waits for a running build up to `build.timeout_s` + 10 s, then renders the PDF from before it with a note |
//! | `ReviewState` | `unified_diff` of every file with pending hunks, plus the review events since the last report |
//! | `ReadText`, `WriteText` | ACP `fs/*` of non-Claude agents (WP5.2): [`agent_edits::apply`]'s raw read and its write; no review feedback is taken for them (ACP's answer has no place for it) |

use super::bridge::{self, ToolRecord};
use crate::agent_edits;
use crate::build_ui::{BuildController, Reason};
use crate::diagnostics_panel::Problem;
use crate::review_ui;
use crate::saver;
use crate::window::WindowState;
use crate::workspace::{self, normalize};
use gio::prelude::*;
use gtk::prelude::*;
use lemma_agent::{EditorRequest, PromptBlock, ToolCall, ToolContent, ToolReply};
use lemma_build::{BuildOutcome, BuildStatus};
use lemma_core::{PathError, Severity};
use lemma_review::unified_diff;
use lemma_syntax::{FileIndex, IncludeKind, ProjectIndex, index_file, parse_magic};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Most entries `list_files` returns.
pub const LIST_FILES_MAX: usize = 500;
/// Most characters of a selection (`get_selection`, the prompt's editor resource).
pub const SELECTION_MAX_CHARS: usize = 20_000;
/// Most errors quoted by `compile`.
pub const COMPILE_MAX_ERRORS: usize = 20;
/// Most problems listed by `get_diagnostics`.
pub const DIAGNOSTICS_MAX: usize = 200;
/// Pixels per PDF point of `render_page` (1.5 × 72 dpi = 108 dpi).
pub const RENDER_SCALE: f64 = 1.5;
/// Tallest image `render_page` returns, in pixels (the page is scaled down to fit).
pub const RENDER_MAX_HEIGHT: i32 = 1600;
/// Widest image `render_page` returns, in pixels (landscape and poster pages).
pub const RENDER_MAX_WIDTH: i32 = 2400;

/// Files read to follow a document's includes.
const GRAPH_MAX_FILES: usize = 300;
/// Files under the root that `list_files` looks at (before leaving out generated ones).
const WALK_MAX_FILES: usize = 5_000;
const WALK_MAX_DIRS: usize = 1_000;
/// Lines quoted around the cursor in the prompt's editor resource when nothing is selected.
const CURSOR_CONTEXT_LINES: u32 = 5;
/// How often a waiting tool looks at the build again.
const BUILD_POLL: Duration = Duration::from_millis(50);

/// Files TeX and its helpers write next to the sources; `list_files` leaves them out.
const GENERATED_SUFFIXES: &[&str] = &[
    ".aux",
    ".log",
    ".fls",
    ".fdb_latexmk",
    ".synctex.gz",
    ".synctex",
    ".synctex(busy)",
    ".out",
    ".toc",
    ".lof",
    ".lot",
    ".bbl",
    ".blg",
    ".bcf",
    ".run.xml",
    ".xdv",
    ".nav",
    ".snm",
    ".vrb",
    ".idx",
    ".ind",
    ".ilg",
    ".glo",
    ".gls",
    ".glg",
    ".ist",
    ".thm",
];

/// Performs `call` and sends its reply (see the module docs).
pub async fn handle(state: Rc<WindowState>, call: ToolCall) {
    let ToolCall { call, req, reply } = call;
    let started = Instant::now();
    let tool = tool_name(&req);
    let path = request_path(&req);
    let changes_files = matches!(
        req,
        EditorRequest::Edit { .. } | EditorRequest::Write { .. } | EditorRequest::WriteText { .. }
    );
    // ACP `fs/*` answers carry no text for the agent to read feedback from.
    let acp_fs = matches!(
        req,
        EditorRequest::ReadText { .. } | EditorRequest::WriteText { .. }
    );
    // The document's version before and after tells whether the file changed (a `write` of the
    // same content changes nothing, and needs no build at the end of the turn).
    let version = |state: &WindowState| {
        path.as_deref()
            .and_then(|p| state.workspace.root().resolve(p).ok())
            .and_then(|p| state.workspace.get(&p))
            .map(|d| (Rc::as_ptr(&d), d.version()))
    };
    let before = if changes_files { version(&state) } else { None };
    let mut answer = run(&state, call, req).await;
    if changes_files
        && !answer.is_error
        && version(&state) != before
        && let Some(controller) = state.agent_controller()
    {
        bridge::note_file_changed(controller);
    }
    // From here on nothing yields until the reply is sent, so the feedback taken here is the
    // feedback this reply delivers, or puts back.
    let feedback = if acp_fs { None } else { take_feedback(&state) };
    if let Some(feedback) = &feedback {
        add_feedback(&mut answer, feedback, tool == "get_review_state");
    } else if tool == "get_review_state" {
        add_text(
            &mut answer,
            "\nThe user has not accepted, rejected or edited anything since the last report.\n",
        );
    }
    let record = ToolRecord {
        call,
        tool,
        path,
        is_error: answer.is_error,
        text: reply_text(&answer),
        images: answer
            .content
            .iter()
            .filter(|c| matches!(c, ToolContent::Png(_)))
            .count(),
        elapsed: started.elapsed(),
    };
    // The MCP thread stops waiting after its timeout (or when the client cancelled the call): a
    // late reply is dropped, and the feedback it carried is kept for the next reply or prompt.
    if reply.send(answer).is_err()
        && let Some(feedback) = feedback
    {
        requeue_feedback(&state, feedback);
    }
    if let Some(controller) = state.agent_controller() {
        controller.bridge().record_tool(record);
    }
}

/// The MCP tool name of a request (`read_file`, `get_diagnostics`, …).
pub fn tool_name(req: &EditorRequest) -> &'static str {
    match req {
        EditorRequest::ReadFile { .. } => "read_file",
        EditorRequest::Edit { .. } => "edit",
        EditorRequest::Write { .. } => "write",
        EditorRequest::ListFiles => "list_files",
        EditorRequest::Outline { .. } => "outline",
        EditorRequest::Compile => "compile",
        EditorRequest::Diagnostics { .. } => "get_diagnostics",
        EditorRequest::Selection => "get_selection",
        EditorRequest::RenderPage { .. } => "render_page",
        EditorRequest::ReviewState => "get_review_state",
        EditorRequest::ReadText { .. } => "fs/read_text_file",
        EditorRequest::WriteText { .. } => "fs/write_text_file",
    }
}

/// An error reply with `message`.
pub fn error_reply(message: &str) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(message.to_owned())],
        is_error: true,
    }
}

/// The text parts of `reply`, joined by blank lines.
pub fn reply_text(reply: &ToolReply) -> String {
    let texts: Vec<&str> = reply
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text(t) => Some(t.as_str()),
            ToolContent::Png(_) => None,
        })
        .collect();
    texts.join("\n\n")
}

fn request_path(req: &EditorRequest) -> Option<PathBuf> {
    match req {
        EditorRequest::ReadFile { path, .. }
        | EditorRequest::Edit { path, .. }
        | EditorRequest::Write { path, .. }
        | EditorRequest::ReadText { path, .. }
        | EditorRequest::WriteText { path, .. } => Some(path.clone()),
        EditorRequest::Outline { path } | EditorRequest::Diagnostics { path } => path.clone(),
        _ => None,
    }
}

/// Performs `req`; the reply has no review feedback yet ([`handle`] adds it).
async fn run(state: &Rc<WindowState>, call: u64, req: EditorRequest) -> ToolReply {
    let result = match req {
        req @ (EditorRequest::ReadFile { .. }
        | EditorRequest::Edit { .. }
        | EditorRequest::Write { .. }
        | EditorRequest::ReadText { .. }
        | EditorRequest::WriteText { .. }) => {
            return agent_edits::perform(&state.workspace, call, req).await;
        }
        EditorRequest::ListFiles => list_files(state).await.map(text),
        EditorRequest::Outline { path } => outline(state, path.as_deref()).await.map(text),
        EditorRequest::Compile => compile(state).await.map(text),
        EditorRequest::Diagnostics { path } => diagnostics(state, path.as_deref()).map(text),
        EditorRequest::Selection => selection(state).map(text),
        EditorRequest::RenderPage { page } => render_page(state, page).await,
        EditorRequest::ReviewState => Ok(text(review_state(state))),
    };
    match result {
        Ok(content) => ToolReply {
            content,
            is_error: false,
        },
        Err(message) => error_reply(&message),
    }
}

// ----- review feedback -----

const FEEDBACK_OPEN: &str = "<review_feedback>\n";
const FEEDBACK_CLOSE: &str = "</review_feedback>";

/// The review feedback to deliver now: what an earlier reply or prompt could not deliver
/// ([`requeue_feedback`]), then what the user did since ([`review_ui::drain_feedback`]), as one
/// `<review_feedback>` block. The caller delivers it, or puts it back with
/// [`requeue_feedback`].
pub(crate) fn take_feedback(state: &WindowState) -> Option<String> {
    let mut lines = state
        .agent_controller()
        .map(|c| c.bridge().take_undelivered_feedback())
        .unwrap_or_default();
    if let Some(block) = review_ui::drain_feedback(&state.workspace) {
        lines.push_str(feedback_lines(&block));
    }
    (!lines.is_empty()).then(|| format!("{FEEDBACK_OPEN}{lines}{FEEDBACK_CLOSE}"))
}

/// Keeps `feedback` (a block from [`take_feedback`] that did not reach the agent) for the next
/// reply or prompt, before anything the user does later.
pub(crate) fn requeue_feedback(state: &WindowState, feedback: String) {
    if let Some(controller) = state.agent_controller() {
        controller
            .bridge()
            .keep_undelivered_feedback(feedback_lines(&feedback));
    }
}

/// The lines of a `<review_feedback>` block.
fn feedback_lines(block: &str) -> &str {
    block
        .strip_prefix(FEEDBACK_OPEN)
        .and_then(|b| b.strip_suffix(FEEDBACK_CLOSE))
        .unwrap_or(block)
}

/// Adds `feedback` to `reply`: after its first text (`get_review_state` introduces it), or as a
/// text of its own.
fn add_feedback(reply: &mut ToolReply, feedback: &str, review_state: bool) {
    if review_state {
        add_text(reply, "\nWhat the user did since the last report:\n");
        add_text(reply, feedback);
    } else {
        add_text(reply, &format!("\n\n{feedback}"));
    }
}

/// Appends `more` to the first text of `reply` (or adds it as one).
fn add_text(reply: &mut ToolReply, more: &str) {
    match reply.content.iter_mut().find_map(|c| match c {
        ToolContent::Text(t) => Some(t),
        ToolContent::Png(_) => None,
    }) {
        Some(text) => text.push_str(more),
        None => reply
            .content
            .push(ToolContent::Text(more.trim_start().to_owned())),
    }
}

fn text(text: String) -> Vec<ToolContent> {
    vec![ToolContent::Text(text)]
}

// ----- shared helpers -----

/// `path` (as the agent wrote it: relative to the root, or absolute) resolved inside the
/// project; `"path outside project root"` otherwise.
fn resolve_path(state: &WindowState, path: &Path) -> Result<PathBuf, String> {
    state.workspace.root().resolve(path).map_err(|e| match e {
        PathError::OutsideProject(_) => "path outside project root".to_owned(),
        PathError::Io(message) => message,
    })
}

fn relative(state: &WindowState, path: &Path) -> String {
    review_ui::relative_name(state, path)
}

/// Where the agent's runtime files go (the MCP socket, `render_page` copies):
/// `$XDG_RUNTIME_DIR/lemma`.
pub(crate) fn runtime_dir(state: &WindowState) -> PathBuf {
    state.paths.as_ref().map_or_else(
        || glib::user_runtime_dir().join("lemma"),
        |p| p.runtime_dir.clone(),
    )
}

/// Creates `dir` (and its parents) readable by the user only (mode 0700; on Windows the
/// user's profile is private already, see [`crate::fsutil`]). Blocking.
pub(crate) fn create_private_dir(dir: &Path) -> Result<(), String> {
    crate::fsutil::private_dir_builder()
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    make_private(dir)?;
    Ok(())
}

/// Sets `dir` to mode 0700 unless it has that mode already.
#[cfg(unix)]
fn make_private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .permissions()
        .mode();
    if mode & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot make {} private: {e}", dir.display()))?;
    }
    Ok(())
}

/// The build output directory, when it is a directory of its own inside the project (walks and
/// the file monitor leave it out; `.build` is hidden anyway).
pub(crate) fn output_dirs(state: &WindowState) -> Vec<PathBuf> {
    let outdir = state.config.build.outdir.clone();
    let root = state.workspace.root().path().to_path_buf();
    let dir = lexical(&root.join(&outdir));
    if dir == root || !dir.starts_with(&root) {
        return Vec::new();
    }
    vec![dir]
}

/// `path` with `.` dropped and `..` folded (no file system access).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The text of `path`: the live text when the workspace has it, else the file's (decoded as the
/// editor would open it).
async fn live_text(state: &WindowState, path: &Path) -> Result<String, String> {
    if let Some(doc) = state.workspace.get(path) {
        return Ok(doc.text());
    }
    let rel = relative(state, path);
    let (bytes, _etag) = gio::File::for_path(path)
        .load_contents_future()
        .await
        .map_err(|e| {
            if e.matches(gio::IOErrorEnum::NotFound) {
                format!("file does not exist: {rel}")
            } else if e.matches(gio::IOErrorEnum::IsDirectory) {
                format!("{rel} is a directory, not a file")
            } else {
                format!("cannot read {rel}: {}", e.message())
            }
        })?;
    saver::decode(&bytes, None)
        .map(|d| d.text)
        .map_err(|e| format!("cannot read {rel}: {e}"))
}

/// Waits (polling) until no build is being prepared, running or waiting, or `deadline` passes.
async fn wait_for_builds(build: &BuildController, deadline: Instant) -> bool {
    while build.is_busy() {
        if Instant::now() > deadline {
            return false;
        }
        glib::timeout_future(BUILD_POLL).await;
    }
    true
}

fn build_deadline(state: &WindowState) -> Instant {
    Instant::now() + Duration::from_secs(state.config.build.timeout_s.saturating_add(20))
}

// ----- the document's files (include graph) -----

/// A document's files, root first, as its includes reach them.
struct Graph {
    root: PathBuf,
    /// `(path, index)`; `None` for a file that could not be read.
    files: Vec<(PathBuf, Option<Arc<FileIndex>>)>,
    /// `.bib` files named by `\bibliography` and friends (that exist).
    bibs: Vec<PathBuf>,
}

/// Where to start the include graph: the root of the last build, else the active document.
fn root_hint(state: &WindowState) -> Option<PathBuf> {
    state
        .build
        .get()
        .and_then(BuildController::root)
        .or_else(|| {
            state
                .active_editor()
                .map(|e| e.document().path().to_path_buf())
        })
}

/// The include graph of the document (see [`Graph`]), from `hint` or the project's main file.
async fn project_graph(state: &WindowState) -> Option<Graph> {
    let hint = root_hint(state);
    let project = state.workspace.root().path().to_path_buf();
    let live: HashMap<PathBuf, String> = state
        .workspace
        .documents()
        .into_iter()
        .map(|d| (d.path().to_path_buf(), d.text()))
        .collect();
    gio::spawn_blocking(move || build_graph(hint, &live, &project))
        .await
        .ok()
        .flatten()
}

/// Builds the include graph (blocking; jobs thread). Only files under `project` are read: a file
/// the includes reach outside it (`\input{../notes}`) is in the graph without an index, like a
/// file that could not be read, so no tool shows what it contains.
fn build_graph(
    hint: Option<PathBuf>,
    live: &HashMap<PathBuf, String>,
    project: &Path,
) -> Option<Graph> {
    let text_of = |path: &Path| -> Option<String> {
        let key = normalize(path);
        if !key.starts_with(project) {
            return None;
        }
        if let Some(text) = live.get(&key) {
            return Some(text.clone());
        }
        let bytes = std::fs::read(&key).ok()?;
        saver::decode(&bytes, None).ok().map(|d| d.text)
    };
    // A document outside the project (open in the window, or built last) is not a start.
    let hint = hint
        .map(|h| normalize(&h))
        .filter(|h| h.starts_with(project));
    let root = match hint {
        Some(hint) => {
            let magic = text_of(&hint).and_then(|t| parse_magic(&t).root);
            magic
                .and_then(|m| hint.parent().map(|dir| normalize(&dir.join(m))))
                .filter(|p| p.is_file() && p.starts_with(project))
                .unwrap_or(hint)
        }
        None => find_main_file(project, &text_of)?,
    };
    let mut index = ProjectIndex::default();
    let mut seen = HashSet::new();
    loop {
        let mut added = false;
        for file in index.project_files(&root) {
            if seen.len() >= GRAPH_MAX_FILES || !seen.insert(file.clone()) {
                continue;
            }
            if let Some(text) = text_of(&file) {
                index.update_file(file, Arc::new(index_file(&text)));
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    let files: Vec<(PathBuf, Option<Arc<FileIndex>>)> = index
        .project_files(&root)
        .into_iter()
        .map(|p| {
            let idx = index.files.get(&p).cloned();
            (p, idx)
        })
        .collect();
    let root_dir = root.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut bibs = Vec::new();
    for (_, idx) in &files {
        for inc in idx.iter().flat_map(|i| &i.includes) {
            if inc.kind != IncludeKind::Bibliography {
                continue;
            }
            let name = if Path::new(&inc.target).extension().is_some() {
                inc.target.clone()
            } else {
                format!("{}.bib", inc.target)
            };
            let path = lexical(&root_dir.join(name));
            if path.is_file() && !bibs.contains(&path) {
                bibs.push(path);
            }
        }
    }
    Some(Graph { root, files, bibs })
}

/// The project's main file when nothing else names one: a `.tex` file in the project directory
/// with `\documentclass`, `main.tex` first.
fn find_main_file(project: &Path, text_of: &dyn Fn(&Path) -> Option<String>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(project)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "tex") && p.is_file())
        .collect();
    candidates.sort_by_key(|p| (p.file_name() != Some("main.tex".as_ref()), p.clone()));
    candidates
        .into_iter()
        .find(|p| text_of(p).is_some_and(|t| index_file(&t).has_documentclass))
}

// ----- list_files -----

async fn list_files(state: &Rc<WindowState>) -> Result<String, String> {
    let root = state.workspace.root().path().to_path_buf();
    let skip = output_dirs(state);
    let graph = project_graph(state).await;
    // (path, note): the files the includes name, root first; those that could not be read are
    // "missing".
    let documented: Vec<(PathBuf, &'static str)> = graph.as_ref().map_or_else(Vec::new, |graph| {
        graph
            .files
            .iter()
            .map(|(p, index)| {
                let note = if *p == graph.root {
                    " (root document)"
                } else if index.is_none() {
                    " (missing)"
                } else {
                    ""
                };
                (p.clone(), note)
            })
            .chain(graph.bibs.iter().map(|p| (p.clone(), "")))
            .filter(|(p, _)| p.starts_with(&root))
            .collect()
    });
    // The walk and the keys that tell the same file apart (symbolic links, `..`) are file
    // system work: the jobs thread does it. The walk's own paths are real paths under the
    // canonical root (it follows no links), so they are their own keys.
    let walk_root = root.clone();
    let paths: Vec<PathBuf> = documented.iter().map(|(p, _)| p.clone()).collect();
    let (walk, keys) = gio::spawn_blocking(move || {
        let walk = workspace::walk_project(&walk_root, &skip, WALK_MAX_FILES, WALK_MAX_DIRS);
        let keys: Vec<PathBuf> = paths.iter().map(|p| normalize(p)).collect();
        (walk, keys)
    })
    .await
    .map_err(|_| "listing the project failed".to_owned())?;

    let mut listed: HashSet<PathBuf> = HashSet::new();
    let mut out = format!("Project root: {}\n", root.display());
    let mut count = 0usize;
    let mut omitted = 0usize;
    if !documented.is_empty() {
        out.push_str("Files of the document (the root document first):\n");
    }
    for ((path, note), key) in documented.iter().zip(keys) {
        if !listed.insert(key) {
            continue;
        }
        if count == LIST_FILES_MAX {
            omitted += 1;
            continue;
        }
        count += 1;
        let _ = writeln!(out, "{}{note}", relative(state, path));
    }
    let graph_listed = count;
    let mut others = String::new();
    for path in walk.files {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if name.is_some_and(|n| GENERATED_SUFFIXES.iter().any(|s| n.ends_with(s))) {
            continue;
        }
        if !listed.insert(path.clone()) {
            continue;
        }
        if count == LIST_FILES_MAX {
            omitted += 1;
            continue;
        }
        count += 1;
        let _ = writeln!(others, "{}", relative(state, &path));
    }
    if !others.is_empty() {
        out.push_str(if graph_listed > 0 {
            "Other files:\n"
        } else {
            "Files:\n"
        });
        out.push_str(&others);
    }
    if count == 0 {
        out.push_str("The project has no files yet.\n");
    }
    if omitted > 0 || walk.truncated {
        let _ = writeln!(
            out,
            "(the list stops at {LIST_FILES_MAX} entries; {}more files are not listed)",
            if omitted > 0 {
                format!("{omitted} ")
            } else {
                String::new()
            }
        );
    }
    Ok(out)
}

// ----- outline -----

async fn outline(state: &Rc<WindowState>, path: Option<&Path>) -> Result<String, String> {
    match path {
        Some(path) => {
            let path = resolve_path(state, path)?;
            let text = live_text(state, &path).await?;
            let index = gio::spawn_blocking(move || index_file(&text))
                .await
                .map_err(|_| "indexing the file failed".to_owned())?;
            let mut out = String::from("Outline (1-based line numbers):\n");
            format_outline(&relative(state, &path), &index, &mut out);
            Ok(out)
        }
        None => {
            let graph = project_graph(state).await.ok_or(
                "No document is open and no main document was found; pass file_path to outline \
                 one file",
            )?;
            let mut out = format!(
                "Outline of the document {} and the files it includes (1-based line numbers):\n",
                relative(state, &graph.root)
            );
            let root = state.workspace.root().path().to_path_buf();
            for (path, index) in &graph.files {
                match index {
                    _ if !lexical(path).starts_with(&root) => {
                        let _ = writeln!(
                            out,
                            "{} (outside the project root; not shown)",
                            path.display()
                        );
                    }
                    Some(index) => format_outline(&relative(state, path), index, &mut out),
                    None => {
                        let _ = writeln!(out, "{} (could not be read)", relative(state, path));
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Appends `rel`'s outline: sectioning commands (indented by level), theorem-like
/// environments, labels and includes, in line order.
fn format_outline(rel: &str, index: &FileIndex, out: &mut String) {
    const LEVELS: [&str; 7] = [
        "part",
        "chapter",
        "section",
        "subsection",
        "subsubsection",
        "paragraph",
        "subparagraph",
    ];
    let _ = writeln!(out, "{rel}");
    let top = index
        .outline
        .iter()
        .filter(|i| i.level < 7)
        .map(|i| i.level)
        .min()
        .unwrap_or(0);
    // (line, order on the line, depth relative to the sections, text)
    let mut entries: Vec<(u32, u8, Option<u8>, String)> = Vec::new();
    for item in &index.outline {
        let star = if item.starred { "*" } else { "" };
        let (depth, line) = if item.level < 7 {
            let name = LEVELS[usize::from(item.level)];
            (
                Some(item.level - top),
                format!("\\{name}{star}{{{}}}", item.title),
            )
        } else {
            let env = item.env.as_deref().unwrap_or("theorem");
            let title = if item.title.is_empty() {
                String::new()
            } else {
                format!("[{}]", item.title)
            };
            (None, format!("\\begin{{{env}{star}}}{title}"))
        };
        entries.push((item.line, 0, depth, line));
    }
    for label in &index.labels {
        let env = label
            .env
            .as_deref()
            .map(|e| format!("  (in {e})"))
            .unwrap_or_default();
        entries.push((
            label.line,
            1,
            None,
            format!("\\label{{{}}}{env}", label.name),
        ));
    }
    for include in &index.includes {
        let cmd = match include.kind {
            IncludeKind::Input => "input",
            IncludeKind::Include => "include",
            IncludeKind::Subfile => "subfile",
            IncludeKind::Import => "import",
            _ => continue,
        };
        entries.push((
            include.line,
            2,
            None,
            format!("\\{cmd}{{{}}}", include.target),
        ));
    }
    entries.sort_by_key(|e| (e.0, e.1));
    if entries.is_empty() {
        out.push_str("  (no sections, theorems, labels or includes)\n");
        return;
    }
    let mut depth = 0u8;
    for (line, _, level, text) in entries {
        let indent = match level {
            Some(level) => {
                depth = level;
                usize::from(level) * 2
            }
            None => usize::from(depth) * 2 + 2,
        };
        let _ = writeln!(out, "{:>6}  {:indent$}{text}", line + 1, "");
    }
}

// ----- compile and diagnostics -----

async fn compile(state: &Rc<WindowState>) -> Result<String, String> {
    let build = state
        .build
        .get()
        .ok_or("Builds are not available in this window")?;
    let finished = state
        .agent_controller()
        .and_then(|c| c.bridge().builds_finished(state))
        .ok_or("Builds are not available in this window")?;
    if let Some(controller) = state.agent_controller() {
        bridge::note_compile(controller);
    }
    let before = finished.get();
    build.request(Reason::AgentCompile);
    if !wait_for_builds(build, build_deadline(state)).await {
        return Err("The build did not finish in time; call compile again later".to_owned());
    }
    if finished.get() == before {
        let why = state.status.build.text();
        return Err(format!("The build did not run: {why}"));
    }
    let outcome = build
        .last_outcome()
        .ok_or("The build did not report a result")?;
    match &outcome.status {
        BuildStatus::Success | BuildStatus::Errors => Ok(build_report(state, build, &outcome)),
        BuildStatus::Cancelled => Err("The build was stopped before it finished".to_owned()),
        BuildStatus::TimedOut => Err(format!(
            "The build was stopped after {} s (build.timeout_s)",
            state.config.build.timeout_s
        )),
        BuildStatus::SpawnFailed(message) => Err(format!("latexmk could not run: {message}")),
    }
}

/// The problems shown, each with its line moved to the current text.
fn current_problems(build: &BuildController) -> Vec<Problem> {
    let panel = build.diagnostics();
    panel
        .problems()
        .into_iter()
        .enumerate()
        .map(|(i, p)| panel.problem_for_agent(i).unwrap_or(p))
        .collect()
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::BadBox => "bad box",
        Severity::Info => "info",
    }
}

fn location(state: &WindowState, problem: &Problem) -> String {
    let rel = relative(state, &problem.path);
    match problem.line {
        Some(line) => format!("{rel}:{line}"),
        None => rel,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn build_report(state: &WindowState, build: &BuildController, outcome: &BuildOutcome) -> String {
    let problems = current_problems(build);
    let errors: Vec<&Problem> = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .collect();
    let warnings = problems
        .iter()
        .filter(|p| p.severity == Severity::Warning)
        .count();
    let bad_boxes = problems
        .iter()
        .filter(|p| p.severity == Severity::BadBox)
        .count();
    let seconds = outcome.elapsed.as_secs_f64();
    let mut out = if outcome.status == BuildStatus::Success && errors.is_empty() {
        format!("Build succeeded in {seconds:.1} s.")
    } else {
        format!(
            "Build failed with {} in {seconds:.1} s.",
            plural(errors.len().max(1), "error", "errors")
        )
    };
    match &outcome.report.output {
        Some((pdf, pages)) => {
            let name = pdf.file_name().map_or_else(
                || pdf.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let _ = write!(
                out,
                " {name} has {}.",
                plural(*pages as usize, "page", "pages")
            );
        }
        None => out.push_str(" No PDF was written."),
    }
    let _ = writeln!(
        out,
        " {}, {}.",
        plural(warnings, "warning", "warnings"),
        plural(bad_boxes, "bad box", "bad boxes")
    );
    if !errors.is_empty() {
        out.push_str("Errors:\n");
        for problem in errors.iter().take(COMPILE_MAX_ERRORS) {
            let _ = writeln!(out, "{}: {}", location(state, problem), problem.message);
        }
        if errors.len() > COMPILE_MAX_ERRORS {
            let _ = writeln!(
                out,
                "… and {} more errors.",
                errors.len() - COMPILE_MAX_ERRORS
            );
        }
    } else if outcome.status == BuildStatus::Errors {
        out.push_str("latexmk reported a failure without a TeX error; see get_diagnostics.\n");
    }
    if warnings + bad_boxes > 0 || errors.len() > COMPILE_MAX_ERRORS {
        out.push_str("Call get_diagnostics for every problem with its context.\n");
    }
    out
}

fn diagnostics(state: &Rc<WindowState>, path: Option<&Path>) -> Result<String, String> {
    let build = state
        .build
        .get()
        .ok_or("Builds are not available in this window")?;
    let file = path.map(|p| resolve_path(state, p)).transpose()?;
    let problems = current_problems(build);
    if build.last_outcome().is_none() && problems.is_empty() {
        return Ok("No build has run in this session yet; call compile first.".to_owned());
    }
    let shown: Vec<&Problem> = problems
        .iter()
        .filter(|p| file.as_ref().is_none_or(|f| normalize(&p.path) == *f))
        .collect();
    let scope = file
        .as_ref()
        .map(|f| format!(" in {}", relative(state, f)))
        .unwrap_or_default();
    if shown.is_empty() {
        return Ok(format!(
            "No errors, warnings or bad boxes{scope} in the last build."
        ));
    }
    let count = |s: Severity| shown.iter().filter(|p| p.severity == s).count();
    let mut out = format!(
        "{}, {}, {}{scope} in the last build (line numbers refer to the current text):\n",
        plural(count(Severity::Error), "error", "errors"),
        plural(count(Severity::Warning), "warning", "warnings"),
        plural(count(Severity::BadBox), "bad box", "bad boxes"),
    );
    for problem in shown.iter().take(DIAGNOSTICS_MAX) {
        let _ = writeln!(
            out,
            "{}: {}: {}",
            location(state, problem),
            severity_name(problem.severity),
            problem.message
        );
        if let Some(context) = &problem.context {
            for line in context.lines().filter(|l| !l.trim().is_empty()) {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    if shown.len() > DIAGNOSTICS_MAX {
        let _ = writeln!(
            out,
            "… and {} more (pass file_path to see one file's)",
            shown.len() - DIAGNOSTICS_MAX
        );
    }
    Ok(out)
}

// ----- the editor: selection and the prompt's resource -----

/// A selection in the active editor.
pub(crate) struct SelectionInfo {
    /// 1-based line and column (in characters) of its start and end.
    pub start: (u32, u32),
    pub end: (u32, u32),
    /// The selected text, cut at [`SELECTION_MAX_CHARS`] characters.
    pub text: String,
    /// Characters selected (before cutting).
    pub chars: usize,
}

/// Where the user is: the active editor's file, cursor and selection.
pub(crate) struct EditorContext {
    pub path: PathBuf,
    pub rel: String,
    /// 1-based line and column (in characters) of the cursor.
    pub line: u32,
    pub column: u32,
    pub line_count: u32,
    pub selection: Option<SelectionInfo>,
}

/// The active editor's file, cursor and selection; `None` without an editor.
pub(crate) fn editor_context(state: &WindowState) -> Option<EditorContext> {
    let editor = state.active_editor()?;
    let doc = editor.document();
    let buffer = editor.buffer();
    let position = |iter: &gtk::TextIter| -> (u32, u32) {
        let offset = doc.byte_offset(iter);
        let (line, col) = doc.line_index().position(offset);
        let chars = doc
            .line_text(line)
            .and_then(|t| t.get(..col as usize).map(|s| s.chars().count()))
            .unwrap_or(col as usize);
        (
            line + 1,
            u32::try_from(chars).unwrap_or(u32::MAX).saturating_add(1),
        )
    };
    let (line, column) = position(&buffer.iter_at_mark(&buffer.get_insert()));
    let selection = buffer.selection_bounds().map(|(start, end)| {
        let text = buffer.text(&start, &end, true);
        let chars = text.chars().count();
        let text = match text.char_indices().nth(SELECTION_MAX_CHARS) {
            Some((cut, _)) => text[..cut].to_owned(),
            None => text.to_string(),
        };
        SelectionInfo {
            start: position(&start),
            end: position(&end),
            text,
            chars,
        }
    });
    // Lines as an editor counts them: a final line break does not start another line.
    let mut line_count = doc.line_index().line_count();
    if line_count > 1 && doc.line_text(line_count - 1).is_some_and(|t| t.is_empty()) {
        line_count -= 1;
    }
    Some(EditorContext {
        path: doc.path().to_path_buf(),
        rel: relative(state, doc.path()),
        line,
        column,
        line_count,
        selection,
    })
}

fn describe_selection(selection: &SelectionInfo, out: &mut String) {
    let _ = writeln!(
        out,
        "Selected text ({}, from line {}, column {} to line {}, column {}):",
        plural(selection.chars, "character", "characters"),
        selection.start.0,
        selection.start.1,
        selection.end.0,
        selection.end.1
    );
    out.push_str(&selection.text);
    if !selection.text.ends_with('\n') {
        out.push('\n');
    }
    if selection.chars > SELECTION_MAX_CHARS {
        let _ = writeln!(
            out,
            "[the selection is cut after {SELECTION_MAX_CHARS} characters; call read_file for the rest]"
        );
    }
}

fn selection(state: &WindowState) -> Result<String, String> {
    let context = editor_context(state).ok_or("No file is open in the editor")?;
    let mut out = format!(
        "File: {}\nCursor: line {}, column {}\n",
        context.rel, context.line, context.column
    );
    match &context.selection {
        Some(selection) => describe_selection(selection, &mut out),
        None => out.push_str("No text is selected.\n"),
    }
    Ok(out)
}

/// The prompt's resource describing where the user is (§4.2.1): the current file, the cursor
/// line, and the selection or the lines around the cursor. `None` without an editor.
pub(crate) fn editor_resource(state: &WindowState) -> Option<PromptBlock> {
    let context = editor_context(state)?;
    let mut out = format!(
        "The user's editor shows {} ({}); the cursor is on line {}, column {}.\n",
        context.rel,
        plural(context.line_count as usize, "line", "lines"),
        context.line,
        context.column
    );
    match &context.selection {
        Some(selection) => describe_selection(selection, &mut out),
        None => {
            out.push_str("No text is selected. The lines around the cursor:\n");
            if let Some(doc) = state.workspace.get(&context.path) {
                let line = context.line - 1;
                let first = line.saturating_sub(CURSOR_CONTEXT_LINES);
                let last = (line + CURSOR_CONTEXT_LINES).min(context.line_count.saturating_sub(1));
                for n in first..=last {
                    if let Some(text) = doc.line_text(n) {
                        let _ = writeln!(out, "{:>6}\t{}", n + 1, text.trim_end_matches('\r'));
                    }
                }
            }
        }
    }
    let uri = glib::filename_to_uri(&context.path, None).ok()?.to_string();
    Some(PromptBlock::Resource {
        uri,
        mime: "text/plain".to_owned(),
        text: out,
    })
}

// ----- render_page -----

/// How long `render_page` waits for a running build, at most: well within the time the MCP
/// server waits for the reply (the build's timeout plus 20 s), so the page is rendered in time.
fn render_deadline(state: &WindowState) -> Instant {
    Instant::now() + Duration::from_secs(state.config.build.timeout_s.saturating_add(10))
}

/// A rendered page.
struct Rendered {
    png: Vec<u8>,
    width: i32,
    height: i32,
    pages: i32,
    copy: PathBuf,
    /// The copy's size and modification time when written (see [`remove_render_copies`]).
    stamp: Option<(u64, std::time::SystemTime)>,
}

/// The PDF to render: the last build's, else the one the preview shows, else the one an
/// earlier build left.
async fn current_pdf(state: &WindowState) -> Option<PathBuf> {
    let build = state.build.get()?;
    if let Some(pdf) = build.last_outcome().and_then(|o| o.pdf) {
        return Some(pdf);
    }
    if let Some(pdf) = state.pdf.get().and_then(|p| p.requested_pdf()) {
        return Some(pdf);
    }
    build.show_existing_pdf().await.ok()
}

async fn render_page(state: &Rc<WindowState>, page: u32) -> Result<Vec<ToolContent>, String> {
    if page == 0 {
        return Err("Pages are numbered from 1".to_owned());
    }
    // A build may be writing the PDF: render the one it produces (or, if it takes too long,
    // the one from before it).
    let mut still_building = false;
    if let Some(build) = state.build.get() {
        still_building = !wait_for_builds(build, render_deadline(state)).await;
    }
    let pdf = current_pdf(state).await.ok_or(if still_building {
        "There is no PDF yet and a build is still running; call render_page again when it has \
         finished"
    } else {
        "There is no PDF yet; call compile first"
    })?;
    let dir = runtime_dir(state);
    let name = relative(state, &pdf);
    let rendered = render_on_thread(pdf, page, dir)
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    if let Some(controller) = state.agent_controller()
        && let Some(stamp) = rendered.stamp
    {
        controller
            .bridge()
            .note_render_copy(rendered.copy.clone(), stamp);
    }
    let mut note = format!(
        "Page {page} of {} of {name}, rendered at {}×{} pixels. A copy of the image is at {}.",
        rendered.pages,
        rendered.width,
        rendered.height,
        rendered.copy.display()
    );
    if still_building {
        note.push_str(
            " A build is still running: this is the PDF from before it; call render_page again \
             when it has finished to see the new one.",
        );
    }
    Ok(vec![
        ToolContent::Png(rendered.png),
        ToolContent::Text(note),
    ])
}

/// Removes the `render_page` copies in `copies` that are still the ones written (same size and
/// modification time; another window or process may have written the same name since). Blocking
/// but cheap (one `stat` and `unlink` per page).
pub(crate) fn remove_render_copies(copies: &[(PathBuf, (u64, std::time::SystemTime))]) {
    for (path, (size, modified)) in copies {
        let ours = std::fs::metadata(path)
            .is_ok_and(|m| m.len() == *size && m.modified().ok() == Some(*modified));
        if ours {
            let _ = std::fs::remove_file(path);
        }
    }
}

// The render thread: `render_page` renders on a thread of its own that keeps the last PDF open
// (poppler documents cannot move between threads, and the jobs pool runs each job on any of its
// threads). Opening a document for every call made poppler and cairo set up its fonts again
// each time, and their caches kept tens of MiB; with the document kept, repeated calls on the
// same PDF reuse them, and a page asked for again is not rendered again ([`RENDER_CACHE_BYTES`]
// of PNGs are kept). The document and the PNGs go after [`RENDER_IDLE`] without calls, and when
// the PDF changes.

/// The open document is closed after this long without a `render_page` call.
const RENDER_IDLE: Duration = Duration::from_secs(30);
/// The PNGs of the open document's pages kept for calls that ask for them again, at most.
const RENDER_CACHE_BYTES: usize = 2 << 20;

/// One `render_page` call for the render thread.
struct RenderJob {
    pdf: PathBuf,
    page: u32,
    dir: PathBuf,
    reply: async_channel::Sender<Result<Rendered, String>>,
}

thread_local! {
    /// The render thread's queue (created with the thread, on the first call).
    static RENDER_QUEUE: std::cell::RefCell<Option<std::sync::mpsc::Sender<RenderJob>>> =
        const { std::cell::RefCell::new(None) };
}

/// Renders `page` of `pdf` on the render thread (see [`render_png`]) and writes a copy to
/// `dir/page-<n>.png`.
async fn render_on_thread(pdf: PathBuf, page: u32, dir: PathBuf) -> Result<Rendered, String> {
    let (reply, answer) = async_channel::bounded(1);
    let mut job = RenderJob {
        pdf,
        page,
        dir,
        reply,
    };
    for _ in 0..2 {
        let sent = RENDER_QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();
            let sender = queue.get_or_insert_with(spawn_render_thread);
            sender.send(job).map_err(|e| {
                // The thread is gone (it could not start, or panicked outside a job): start
                // another one.
                *queue = None;
                e.0
            })
        });
        match sent {
            Ok(()) => {
                return answer
                    .recv()
                    .await
                    .unwrap_or_else(|_| Err("rendering the page failed".to_owned()));
            }
            Err(back) => job = back,
        }
    }
    Err("rendering the page failed: the render thread does not start".to_owned())
}

fn spawn_render_thread() -> std::sync::mpsc::Sender<RenderJob> {
    let (sender, jobs) = std::sync::mpsc::channel::<RenderJob>();
    let spawned = std::thread::Builder::new()
        .name("lemma-render".into())
        .spawn(move || render_thread(&jobs));
    if let Err(e) = spawned {
        tracing::warn!("cannot start the render thread: {e}");
    }
    sender
}

/// A PDF as the render thread opened it: its path, size and modification time.
type PdfStamp = (PathBuf, u64, Option<std::time::SystemTime>);

/// The render thread's open PDF.
struct OpenPdf {
    stamp: PdfStamp,
    document: poppler::Document,
    /// Pages rendered from it, least recently used first, [`RENDER_CACHE_BYTES`] at most.
    pages: std::collections::VecDeque<PagePng>,
}

/// One rendered page.
#[derive(Clone)]
struct PagePng {
    /// 1-based.
    page: u32,
    png: Vec<u8>,
    width: i32,
    height: i32,
}

fn render_thread(jobs: &std::sync::mpsc::Receiver<RenderJob>) {
    let mut open: Option<OpenPdf> = None;
    loop {
        let job = match jobs.recv_timeout(RENDER_IDLE) {
            Ok(job) => job,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Idle: give the document (and its fonts) back.
                open = None;
                match jobs.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_job(&mut open, &job.pdf, job.page, &job.dir)
        }))
        .unwrap_or_else(|_| {
            open = None;
            Err("rendering the page failed".to_owned())
        });
        let _ = job.reply.send_blocking(result);
    }
}

/// Page `page` of `pdf` from the document of `open` (reopened when `pdf` is another file or
/// changed), rendered or taken from its PNGs, and its copy in `dir`.
fn render_job(
    open: &mut Option<OpenPdf>,
    pdf: &Path,
    page: u32,
    dir: &Path,
) -> Result<Rendered, String> {
    let meta = std::fs::metadata(pdf).map_err(|e| format!("cannot read the PDF: {e}"))?;
    let stamp: PdfStamp = (pdf.to_path_buf(), meta.len(), meta.modified().ok());
    if open.as_ref().is_none_or(|o| o.stamp != stamp) {
        *open = None;
        let bytes = std::fs::read(pdf).map_err(|e| format!("cannot read the PDF: {e}"))?;
        let document = poppler::Document::from_bytes(&glib::Bytes::from_owned(bytes), None)
            .map_err(|e| format!("cannot open the PDF (a build may be writing it): {e}"))?;
        *open = Some(OpenPdf {
            stamp,
            document,
            pages: std::collections::VecDeque::new(),
        });
    }
    let open = open.as_mut().expect("opened above");
    let pages = open.document.n_pages();
    let image = match open.pages.iter().position(|p| p.page == page) {
        Some(at) => {
            let hit = open.pages.remove(at).expect("found above");
            open.pages.push_back(hit.clone());
            hit
        }
        None => {
            let image = render_png(&open.document, page)?;
            let mut used = image.png.len();
            open.pages.push_back(image.clone());
            // Least recently used first: keep the newest that fit.
            let mut keep = open.pages.len();
            for (i, p) in open.pages.iter().enumerate().rev().skip(1) {
                used += p.png.len();
                if used > RENDER_CACHE_BYTES {
                    keep = open.pages.len() - 1 - i;
                    break;
                }
            }
            let drop = open.pages.len() - keep;
            open.pages.drain(..drop);
            image
        }
    };
    create_private_dir(dir)?;
    let copy = dir.join(format!("page-{page}.png"));
    std::fs::write(&copy, &image.png)
        .map_err(|e| format!("cannot write {}: {e}", copy.display()))?;
    let stamp = std::fs::metadata(&copy)
        .ok()
        .and_then(|m| Some((m.len(), m.modified().ok()?)));
    Ok(Rendered {
        png: image.png,
        width: image.width,
        height: image.height,
        pages,
        copy,
        stamp,
    })
}

/// Renders page `page` (1-based) of `document` on white at [`RENDER_SCALE`]× (smaller to fit
/// [`RENDER_MAX_HEIGHT`] and [`RENDER_MAX_WIDTH`]) as a PNG. Blocking (poppler and cairo on the
/// render thread).
fn render_png(document: &poppler::Document, page: u32) -> Result<PagePng, String> {
    let pages = document.n_pages();
    let index = i32::try_from(page - 1).unwrap_or(i32::MAX);
    let Some(source) = (index < pages).then(|| document.page(index)).flatten() else {
        return Err(format!(
            "page {page} does not exist: the PDF has {}",
            plural(usize::try_from(pages).unwrap_or(0), "page", "pages")
        ));
    };
    let (width_pt, height_pt) = source.size();
    if width_pt <= 0.0 || height_pt <= 0.0 {
        return Err(format!("page {page} has no size"));
    }
    let scale = RENDER_SCALE
        .min(f64::from(RENDER_MAX_HEIGHT) / height_pt)
        .min(f64::from(RENDER_MAX_WIDTH) / width_pt);
    let px = |pt: f64, max: i32| ((pt * scale).round() as i32).clamp(1, max);
    let (width, height) = (
        px(width_pt, RENDER_MAX_WIDTH),
        px(height_pt, RENDER_MAX_HEIGHT),
    );
    let surface = cairo::ImageSurface::create(cairo::Format::Rgb24, width, height)
        .map_err(|e| format!("cannot render page {page}: {e}"))?;
    {
        let cr =
            cairo::Context::new(&surface).map_err(|e| format!("cannot render page {page}: {e}"))?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint()
            .map_err(|e| format!("cannot render page {page}: {e}"))?;
        cr.scale(scale, scale);
        source.render(&cr);
    }
    surface.flush();
    let mut png = Vec::new();
    surface
        .write_to_png(&mut png)
        .map_err(|e| format!("cannot encode page {page}: {e}"))?;
    Ok(PagePng {
        page,
        png,
        width,
        height,
    })
}

// ----- get_review_state -----

fn review_state(state: &WindowState) -> String {
    let ws = &state.workspace;
    let docs = review_ui::reviewed_documents(ws);
    let mut out = String::new();
    if docs.is_empty() {
        out.push_str("No pending changes: the user has accepted or rejected all of them.\n");
    } else {
        let (files, hunks) = review_ui::summary(ws);
        let _ = writeln!(
            out,
            "{} awaiting review in {}. Unified diff of the text the user accepted (---) and \
             the current text (+++):",
            plural(hunks, "change", "changes"),
            plural(files, "file", "files")
        );
        for doc in docs {
            let rel = relative(state, doc.path());
            let review = doc.review.borrow();
            if let Some(review) = review.as_ref() {
                let diff = doc.with_text(|current| unified_diff(&rel, review.baseline(), current));
                out.push_str(&diff);
                if !diff.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    // The review feedback follows (added by `handle`).
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outlines_list_sections_theorems_labels_and_includes() {
        let index = index_file(
            "\\section{Intro}\\label{sec:intro}\n\\begin{theorem}[Main]\n\\label{thm:main}\n\
             \\end{theorem}\n\\subsection*{Aside}\n\\input{chapters/a}\n",
        );
        let mut out = String::new();
        format_outline("main.tex", &index, &mut out);
        assert_eq!(
            out,
            "main.tex\n     1  \\section{Intro}\n     1    \\label{sec:intro}\n     2    \
             \\begin{theorem}[Main]\n     3    \\label{thm:main}  (in theorem)\n     5    \
             \\subsection*{Aside}\n     6      \\input{chapters/a}\n"
        );
        let mut empty = String::new();
        format_outline("x.tex", &index_file("text\n"), &mut empty);
        assert_eq!(
            empty,
            "x.tex\n  (no sections, theorems, labels or includes)\n"
        );
    }

    #[test]
    fn the_include_graph_follows_live_texts_and_magic_roots() {
        let dir = std::env::temp_dir().join(format!("lemma-graph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("chapters")).unwrap();
        let dir = lemma_core::canonicalize(&dir).unwrap();
        std::fs::write(
            dir.join("main.tex"),
            "\\documentclass{article}\n\\addbibresource{refs.bib}\n\\begin{document}\n\
             \\input{chapters/a}\n\\end{document}\n",
        )
        .unwrap();
        std::fs::write(dir.join("refs.bib"), "@book{x}\n").unwrap();
        std::fs::write(dir.join("chapters/a.tex"), "on disk\n").unwrap();
        std::fs::write(dir.join("chapters/b.tex"), "\\section{B}\n").unwrap();
        // The live text of a.tex (unsaved) includes b.tex; the disk's does not.
        let live = HashMap::from([(
            dir.join("chapters/a.tex"),
            "% !TEX root = ../main.tex\n\\input{chapters/b}\n".to_owned(),
        )]);
        // From the chapter, its magic comment leads to the root.
        let graph = build_graph(Some(dir.join("chapters/a.tex")), &live, &dir).unwrap();
        assert_eq!(graph.root, dir.join("main.tex"));
        let files: Vec<PathBuf> = graph.files.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            files,
            [
                dir.join("main.tex"),
                dir.join("chapters/a.tex"),
                dir.join("chapters/b.tex")
            ]
        );
        assert_eq!(graph.bibs, [dir.join("refs.bib")]);
        // Without a hint: the main file of the project directory.
        let graph = build_graph(None, &HashMap::new(), &dir).unwrap();
        assert_eq!(graph.root, dir.join("main.tex"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_paths_are_folded_lexically() {
        assert_eq!(
            lexical(Path::new("/p/./out/../build")),
            PathBuf::from("/p/build")
        );
        assert_eq!(plural(1, "page", "pages"), "1 page");
        assert_eq!(plural(3, "page", "pages"), "3 pages");
    }
}
