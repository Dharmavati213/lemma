//! Agent file tools on the main thread (WP1.2, ARCHITECTURE §4.2): `ReadFile`, `Edit` and
//! `Write`, used by `--apply-as-agent` and by the MCP tools (M2).
//!
//! Every path goes through `ProjectRoot::resolve`; anything outside the project root is refused.
//! The agent may write a path with `/` or, on Windows, with `\`; the replies name files with `/`
//! on every system ([`lemma_core::ProjectRoot::agent_name`]), so that a name copied into
//! `\input{…}` is not read as a command.
//! An edit runs against the **live** text (the buffer of an open file, the shadow of a closed
//! one), is applied as one undoable action with `Origin::Agent { call }`, becomes reviewable
//! hunks right away, and is **on disk before the reply resolves**. Replies quote the changed
//! region in `cat -n` format and carry the review feedback queued since the last tool call.
//!
//! **Files the tools may not write** ([`is_protected`]): `latexmkrc`/`.latexmkrc` (latexmk runs
//! them as Perl at every build, which would let an auto-allowed tool run code before any review)
//! and every hidden file or file in a hidden directory (`.git/config`, `.envrc`, …), which other
//! programs read as configuration. Reading them is allowed.
//!
//! **Reading** (`read_file`): `limit` lines (2000 by default) from `offset`, each cut at
//! [`MAX_LINE_CHARS`] characters, and at most [`READ_MAX_BYTES`] in all; the answer ends with
//! where to read on when it leaves lines out. A file that is not in the workspace and larger than
//! [`READ_WHOLE_MAX`] is read line by line, keeping only the lines shown.
//!
//! **ACP `fs/*`** (WP5.2): `ReadText` returns the live text itself (no line numbers; `line`/`limit`
//! select whole lines with their terminators) and `WriteText` is `Write` with a short reply; the
//! caller (`tools::handle`) adds no review feedback to either.
//!
//! **Line endings.** A file whose line breaks are mostly `\r\n` is handled as the model sees it:
//! `read_file` and the quoted excerpts show its lines without the `\r`, and the `\n` of an
//! `old_string`, `new_string` or `write` content become `\r\n` before matching and applying, so the
//! lines that do not change keep their terminators.

use crate::document::DocRef;
use crate::review_ui;
use crate::saver;
use crate::workspace::{self, Workspace};
use gio::prelude::*;
use gtk::prelude::TextBufferExt;
use lemma_agent::{EditorRequest, ToolContent, ToolReply};
use lemma_core::{LineIndex, Origin, PathError, TextEdit, agent_text, fnv1a64};
use lemma_review::{EditError, minimal_edits, str_replace};
use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Lines returned by `read_file` when the agent gives no limit.
pub const DEFAULT_READ_LIMIT: u32 = 2000;
/// Lines longer than this (in characters) are cut in `read_file` output.
pub const MAX_LINE_CHARS: usize = 2000;
/// Most bytes of a `read_file` answer: the lines after it are left out, and the answer says
/// where to read on (Claude Code's own Read takes files up to 256 KiB whole).
pub const READ_MAX_BYTES: usize = 256 << 10;
/// A file `read_file` reads from the disk (it is not in the workspace) that is larger than this
/// is read line by line, keeping only the lines the answer shows: it is never in memory whole.
pub const READ_WHOLE_MAX: u64 = 8 << 20;
/// The largest file not in the workspace whose text is read whole into memory
/// (`fs/read_text_file`; `read_file` reads a larger one than [`READ_WHOLE_MAX`] line by line).
pub const LIVE_TEXT_MAX: u64 = 64 << 20;
/// A file `read_file` reads line by line is read on after the lines of the answer at most this
/// far (some milliseconds) to count the lines left; past it the answer says that more lines
/// follow, without their number. Lines 1–100 of a file of gigabytes took as long as all of it.
const COUNT_AHEAD: u64 = 16 << 20;
/// Unchanged lines quoted around each change in an edit reply.
const CONTEXT_LINES: u32 = 3;
/// Most lines quoted in an edit reply.
const MAX_QUOTED_LINES: usize = 120;

/// Why a file tool failed; the `Display` text is what the agent reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// The path resolves outside the project root.
    OutsideRoot,
    /// The path could not be resolved.
    Path(String),
    /// The file does not exist (relative path).
    Missing(String),
    /// The path names a directory (relative path).
    NotAFile(String),
    /// The path names a named pipe, a socket or a device (relative path): reading it would wait
    /// for another program to write to it, or never end.
    Special(String),
    /// Reading the file failed.
    Read(String),
    /// Creating the file's directory failed.
    CreateDir(String),
    /// Another program changed the file while the editor holds unsaved or unreviewed changes
    /// to it (relative path).
    ChangedOnDisk(String),
    /// `old_string` does not occur in the live text.
    NotFound,
    /// `old_string` occurs this many times and `replace_all` was not set.
    NotUnique(usize),
    /// `old_string` is empty.
    EmptyOldString,
    /// `old_string` equals `new_string`.
    Unchanged,
    /// `offset` lies past the last line.
    OffsetPastEnd { offset: u32, lines: usize },
    /// The edit did not fit the document (should not happen: it was computed on the same text).
    Apply(String),
    /// The request is not one of the file tools.
    Unsupported(&'static str),
    /// The file is one the tools may not write ([`is_protected`]; relative path).
    Protected(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideRoot => f.write_str("path outside project root"),
            Self::Path(msg) | Self::Read(msg) | Self::CreateDir(msg) => f.write_str(msg),
            Self::Missing(rel) => write!(f, "file does not exist: {rel}"),
            Self::NotAFile(rel) => write!(f, "{rel} is a directory, not a file"),
            Self::Special(rel) => write!(
                f,
                "{rel} is not a regular file (a named pipe, a socket or a device); it was not \
                 read or written"
            ),
            Self::ChangedOnDisk(rel) => write!(
                f,
                "{rel} was changed by another program while the editor holds changes to it \
                 that are not saved or not yet reviewed; it was not edited"
            ),
            Self::NotFound => f.write_str(
                "old_string not found — the user may have edited this region; call read_file",
            ),
            Self::NotUnique(n) => {
                write!(
                    f,
                    "old_string matches {n} times; add context or set replace_all"
                )
            }
            Self::EmptyOldString => f.write_str(
                "old_string is empty; use write to create a file or replace its whole content",
            ),
            Self::Unchanged => {
                f.write_str("old_string and new_string are identical; nothing to change")
            }
            Self::OffsetPastEnd { offset, lines } => {
                write!(
                    f,
                    "offset {offset} is past the end of the file ({lines} lines)"
                )
            }
            Self::Apply(msg) => write!(f, "could not apply the edit: {msg}"),
            Self::Unsupported(tool) => write!(f, "{tool} is not handled by the file tools"),
            Self::Protected(rel) => write!(
                f,
                "{rel} may not be written with Lemma's tools: latexmk runs latexmkrc files as code \
                 at every build, and other programs read hidden files and directories (such as \
                 .git) as configuration; ask the user to make this change"
            ),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<EditError> for ToolError {
    fn from(e: EditError) -> Self {
        match e {
            EditError::NotFound => Self::NotFound,
            EditError::NotUnique(n) => Self::NotUnique(n),
            EditError::Empty => Self::EmptyOldString,
        }
    }
}

/// Performs one agent request against the workspace and resolves with the tool reply once the
/// change is on disk. `call` identifies the tool call; the edit carries `Origin::Agent { call }`.
///
/// Only `ReadFile`, `Edit` and `Write` are handled; other requests get an error reply.
pub fn apply(
    ws: &Rc<Workspace>,
    call: u64,
    req: EditorRequest,
) -> impl Future<Output = ToolReply> + use<> {
    let reply = perform(ws, call, req);
    let ws = ws.clone();
    async move {
        let mut reply = reply.await;
        if let Some(feedback) = review_ui::drain_feedback(&ws)
            && let Some(ToolContent::Text(text)) = reply.content.first_mut()
        {
            text.push_str("\n\n");
            text.push_str(&feedback);
        }
        reply
    }
}

/// [`apply`] without the review feedback: the MCP tools add it themselves, once the reply is
/// sure to be delivered (`tools::handle`).
pub(crate) fn perform(
    ws: &Rc<Workspace>,
    call: u64,
    req: EditorRequest,
) -> impl Future<Output = ToolReply> + use<> {
    let ws = ws.clone();
    async move {
        let result = match req {
            EditorRequest::ReadFile {
                path,
                offset,
                limit,
            } => read_file(&ws, &path, offset, limit).await,
            EditorRequest::Edit {
                path,
                old_string,
                new_string,
                replace_all,
            } => edit(&ws, call, &path, &old_string, &new_string, replace_all).await,
            EditorRequest::Write { path, content } => write(&ws, call, &path, &content).await,
            EditorRequest::ReadText { path, line, limit } => {
                read_text(&ws, &path, line, limit).await
            }
            EditorRequest::WriteText { path, content } => write(&ws, call, &path, &content).await,
            other => Err(ToolError::Unsupported(request_name(&other))),
        };
        let (text, is_error) = match result {
            Ok(text) => (text, false),
            Err(e) => (e.to_string(), true),
        };
        ToolReply {
            content: vec![ToolContent::Text(text)],
            is_error,
        }
    }
}

fn request_name(req: &EditorRequest) -> &'static str {
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

/// A resolved agent path.
struct Target {
    /// Canonical absolute path.
    path: PathBuf,
    /// Path relative to the project root, for messages: with `/` between its names on every
    /// system, as the agent is shown every path.
    rel: String,
}

impl Target {
    /// `message` of GIO or the system, which names the file as the system spells it, with the
    /// file named as the agent is shown paths.
    fn for_agent(&self, message: &str) -> String {
        agent_text(message, &self.path)
    }

    /// "cannot read `rel`: `message`", for the agent.
    fn cannot_read(&self, message: &str) -> ToolError {
        ToolError::Read(format!(
            "cannot read {}: {}",
            self.rel,
            self.for_agent(message)
        ))
    }
}

fn resolve(ws: &Workspace, path: &Path) -> Result<Target, ToolError> {
    let root = ws.root();
    let path = root.resolve(path).map_err(|e| match e {
        PathError::OutsideProject(_) => ToolError::OutsideRoot,
        PathError::Io(msg) => ToolError::Path(msg),
    })?;
    let rel = root.agent_name(&path);
    Ok(Target { path, rel })
}

/// [`resolve`] for a tool that changes the file: also refuses the files of [`is_protected`].
fn resolve_writable(ws: &Workspace, path: &Path) -> Result<Target, ToolError> {
    let target = resolve(ws, path)?;
    if is_protected(Path::new(&target.rel)) {
        return Err(ToolError::Protected(target.rel));
    }
    Ok(target)
}

/// True for a project file the agent's tools may not write (`rel` is relative to the project
/// root, as [`resolve`] gives it): a `latexmkrc` or `.latexmkrc` in any directory (latexmk runs
/// it as Perl at every build, also the automatic one at the end of a turn), and any hidden file
/// or file in a hidden directory (`.git/config`, `.git/hooks/…`, `.envrc`, …), which git and
/// other programs read as configuration or run. Writing such a file through an auto-allowed
/// tool would run code before the user reviewed anything.
///
/// A name is compared as Windows opens it: up to a `:` (`latexmkrc::$DATA` is the file's own
/// data stream) and without trailing dots and spaces (`latexmkrc.` creates `latexmkrc`). Such
/// names are refused everywhere; nothing legitimate is called that.
pub fn is_protected(rel: &Path) -> bool {
    rel.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        let opened = name.split(':').next().unwrap_or_default();
        let opened = opened.trim_end_matches(['.', ' ']);
        name.starts_with('.') || opened.eq_ignore_ascii_case("latexmkrc")
    })
}

/// True when most line breaks of `text` are `\r\n` (a file written on Windows).
fn uses_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    crlf > 0 && crlf * 2 >= text.matches('\n').count()
}

/// `text` with every `\n` that has no `\r` before it turned into `\r\n`.
fn to_crlf(text: &str) -> Cow<'_, str> {
    if !text.contains('\n') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut previous = '\0';
    for c in text.chars() {
        if c == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(c);
        previous = c;
    }
    Cow::Owned(out)
}

/// The file's type, or `None` when it does not exist. A named pipe, a socket or a device is
/// refused (as opening it in the editor is).
async fn file_type(target: &Target) -> Result<Option<gio::FileType>, ToolError> {
    let query = gio::File::for_path(&target.path).query_info_future(
        "standard::type",
        gio::FileQueryInfoFlags::NONE,
        glib::Priority::DEFAULT,
    );
    match query.await {
        Ok(info) if info.file_type() == gio::FileType::Special => {
            Err(ToolError::Special(target.rel.clone()))
        }
        Ok(info) => Ok(Some(info.file_type())),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => Ok(None),
        Err(e) => Err(target.cannot_read(e.message())),
    }
}

/// Drops a closed document that was loaded only for a request that changed nothing.
fn forget_if_unused(ws: &Workspace, doc: &DocRef) {
    if !doc.is_open() && doc.review.borrow().is_none() {
        ws.release(doc);
    }
}

/// The workspace's document for `target`, checked against the file when it is closed.
///
/// A closed document keeps the text it read or last wrote (its review may outlive every hunk).
/// When another program changed the file since (a `git checkout`, another editor), that text is
/// stale: a saved document with nothing left to review is dropped, so that the file is read
/// again (`None`), and one with changes awaiting review or not saved is refused, since editing
/// it would overwrite the other program's change. Open documents are the buffer the user sees,
/// and are used as they are.
async fn current_document(
    ws: &Rc<Workspace>,
    target: &Target,
) -> Result<Option<DocRef>, ToolError> {
    match ws.get(&target.path) {
        Some(doc) if !doc.is_open() => {}
        other => return Ok(other),
    }
    let mut disk = read_bytes(target).await?;
    // Looked up again: the document may have been opened, changed or dropped meanwhile.
    let doc = match ws.get(&target.path) {
        Some(doc) if !doc.is_open() => doc,
        other => return Ok(other),
    };
    // Bytes that are not the document's own write may still be: a save that was running when
    // the file was read can have ended since, and the version it replaced no longer counts as
    // ours. Read again until two reads agree (a few times at most), so that an earlier own
    // write is taken for another program's change only when the file really holds it.
    let hash_of = |disk: &Option<glib::Slice<u8>>| disk.as_ref().map(|b| fnv1a64(&[&b[..]]));
    for _ in 0..3 {
        let hash = hash_of(&disk);
        if hash.is_some_and(|h| doc.is_own_write(h)) {
            break;
        }
        let again = read_bytes(target).await?;
        let stable = hash_of(&again) == hash;
        disk = again;
        match ws.get(&target.path) {
            Some(still) if Rc::ptr_eq(&still, &doc) && !still.is_open() => {}
            other => return Ok(other),
        }
        if stable {
            break;
        }
    }
    let fresh = match &disk {
        Some(bytes) => {
            doc.is_own_write(fnv1a64(&[&bytes[..]]))
                || (!doc.is_dirty()
                    && saver::decode(bytes, doc.charset().as_deref())
                        .is_ok_and(|d| doc.with_text(|text| text == d.text)))
        }
        // Not written yet (a new file whose save is pending), or deleted.
        None => doc.is_dirty() || review_ui::is_new_file(&doc),
    };
    if fresh {
        return Ok(Some(doc));
    }
    let pending = doc.review.borrow().as_ref().is_some_and(|r| !r.is_clean());
    if pending || doc.is_dirty() {
        return Err(ToolError::ChangedOnDisk(target.rel.clone()));
    }
    tracing::info!(
        "{} changed on disk; dropping its stale closed document",
        target.rel
    );
    review_ui::drop_review(&doc);
    ws.release(&doc);
    Ok(None)
}

/// The file's bytes, `None` when it does not exist. A named pipe, a socket or a device is
/// refused ([`workspace::load_file`]).
async fn read_bytes(target: &Target) -> Result<Option<glib::Slice<u8>>, ToolError> {
    match workspace::load_file(&target.path).await {
        Ok((bytes, _etag)) => Ok(Some(bytes)),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => Ok(None),
        Err(e) if e.matches(gio::IOErrorEnum::NotRegularFile) => {
            Err(ToolError::Special(target.rel.clone()))
        }
        Err(e) => Err(target.cannot_read(e.message())),
    }
}

/// The document of an existing file (loading it as a closed document if needed).
async fn existing_document(ws: &Rc<Workspace>, target: &Target) -> Result<DocRef, ToolError> {
    if let Some(doc) = current_document(ws, target).await? {
        return Ok(doc);
    }
    match file_type(target).await? {
        None => Err(ToolError::Missing(target.rel.clone())),
        Some(gio::FileType::Directory) => Err(ToolError::NotAFile(target.rel.clone())),
        Some(_) => ws
            .get_or_load(&target.path)
            .await
            .map_err(|e| ToolError::Read(target.for_agent(&e))),
    }
}

async fn read_file(
    ws: &Rc<Workspace>,
    path: &Path,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<String, ToolError> {
    let target = resolve(ws, path)?;
    if let Some(doc) = current_document(ws, &target).await? {
        return doc.with_text(|text| numbered(text, &target.rel, offset, limit));
    }
    // Not in the workspace: the file on disk is the live text (it is not added to the
    // workspace). A large one is read line by line, never whole.
    let size = match file_size(&target).await? {
        None => return Err(ToolError::Missing(target.rel.clone())),
        Some(None) => return Err(ToolError::NotAFile(target.rel.clone())),
        Some(Some(size)) => size,
    };
    if size <= READ_WHOLE_MAX {
        let text = live_text(ws, &target).await?;
        return numbered(&text, &target.rel, offset, limit);
    }
    let Target { path, rel } = target;
    let failed = rel.clone();
    gio::spawn_blocking(move || numbered_file(&path, &rel, offset, limit))
        .await
        .unwrap_or_else(|_| Err(ToolError::Read(format!("cannot read {failed}"))))
}

/// The size of the file of `target`: `None` when it does not exist, `Some(None)` for a
/// directory. A named pipe, a socket or a device is refused: it says it is empty, and reading
/// it would wait for another program to write to it.
async fn file_size(target: &Target) -> Result<Option<Option<u64>>, ToolError> {
    let query = gio::File::for_path(&target.path).query_info_future(
        "standard::type,standard::size",
        gio::FileQueryInfoFlags::NONE,
        glib::Priority::DEFAULT,
    );
    match query.await {
        Ok(info) if info.file_type() == gio::FileType::Directory => Ok(Some(None)),
        Ok(info) if info.file_type() == gio::FileType::Special => {
            Err(ToolError::Special(target.rel.clone()))
        }
        Ok(info) => Ok(Some(Some(u64::try_from(info.size()).unwrap_or(0)))),
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => Ok(None),
        Err(e) => Err(target.cannot_read(e.message())),
    }
}

/// ACP `fs/read_text_file`: the live text of `path` as it is, or `limit` lines of it from the
/// 1-based `line` (each with its line break).
async fn read_text(
    ws: &Rc<Workspace>,
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, ToolError> {
    let target = resolve(ws, path)?;
    let text = live_text(ws, &target).await?;
    if line.is_none() && limit.is_none() {
        return Ok(text);
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let first = line.unwrap_or(1).max(1) as usize;
    if first > lines.len() && !(text.is_empty() && first == 1) {
        return Err(ToolError::OffsetPastEnd {
            offset: first as u32,
            lines: lines.len(),
        });
    }
    let limit = limit.map_or(usize::MAX, |l| l as usize);
    Ok(lines.iter().skip(first - 1).take(limit).copied().collect())
}

/// The live text of `target`: the buffer of an open file, the shadow of a closed one, else the
/// file on disk (which is not added to the workspace).
async fn live_text(ws: &Rc<Workspace>, target: &Target) -> Result<String, ToolError> {
    Ok(match current_document(ws, target).await? {
        Some(doc) => doc.text(),
        // Not in the workspace: disk is the live text. It is not added to the workspace.
        None => match file_size(target).await? {
            None => return Err(ToolError::Missing(target.rel.clone())),
            Some(None) => {
                return Err(ToolError::NotAFile(target.rel.clone()));
            }
            // Read whole, into memory: not a file of gigabytes (a log, a data set).
            Some(Some(size)) if size > LIVE_TEXT_MAX => {
                return Err(ToolError::Read(format!(
                    "cannot read {}: it is {} MiB, and a file that is not open in the editor is \
                     read whole only up to {} MiB",
                    target.rel,
                    size >> 20,
                    LIVE_TEXT_MAX >> 20
                )));
            }
            Some(Some(_)) => {
                let (bytes, _etag) = gio::File::for_path(&target.path)
                    .load_contents_future()
                    .await
                    .map_err(|e| target.cannot_read(e.message()))?;
                // Decoded as the editor would open it (UTF-8, else Latin-9).
                saver::decode(&bytes, None)
                    .map_err(|e| ToolError::Read(format!("cannot read {}: {e}", target.rel)))?
                    .text
            }
        },
    })
}

/// `cat -n` output of `limit` lines starting at the 1-based line `offset`, at most
/// [`READ_MAX_BYTES`] of it.
fn numbered(
    text: &str,
    rel: &str,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<String, ToolError> {
    if text.is_empty() {
        return Ok(format!("({rel} is empty)"));
    }
    let lines = split_lines(text);
    let mut answer = Numbered::new(offset, limit);
    for (i, line) in lines.iter().enumerate().skip(answer.first - 1) {
        if !answer.wants(i + 1) {
            break;
        }
        answer.push(i + 1, line);
    }
    answer.finish(lines.len())
}

/// A `read_file` answer being built (see [`numbered`]).
struct Numbered {
    /// The first line asked for (1-based).
    first: usize,
    /// The line after the last one asked for.
    end: usize,
    out: String,
    /// The first line the answer has no room for ([`READ_MAX_BYTES`]).
    cut_at: Option<usize>,
}

impl Numbered {
    fn new(offset: Option<u32>, limit: Option<u32>) -> Self {
        let first = offset.unwrap_or(1).max(1) as usize;
        let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).max(1) as usize;
        Self {
            first,
            end: first.saturating_add(limit),
            out: String::new(),
            cut_at: None,
        }
    }

    /// Whether line `n` goes into the answer: it was asked for, and the answer has room so far.
    fn wants(&self, n: usize) -> bool {
        self.cut_at.is_none() && (self.first..self.end).contains(&n)
    }

    /// Whether the answer has all its lines once line `n` is reached.
    fn complete_at(&self, n: usize) -> bool {
        self.cut_at.is_some() || n >= self.end
    }

    /// Adds line `n`, or ends the answer before it when it has no room for it (the first line
    /// always fits: lines are cut at [`MAX_LINE_CHARS`]).
    fn push(&mut self, n: usize, line: &str) {
        let at = self.out.len();
        push_numbered(&mut self.out, n, line);
        if at > 0 && self.out.len() > READ_MAX_BYTES {
            self.out.truncate(at);
            self.cut_at = Some(n);
        }
    }

    /// The answer, for a text of `total` lines (not empty), saying what it left out.
    fn finish(mut self, total: usize) -> Result<String, ToolError> {
        if self.first > total {
            return Err(ToolError::OffsetPastEnd {
                offset: u32::try_from(self.first).unwrap_or(u32::MAX),
                lines: total,
            });
        }
        match self.cut_at {
            Some(cut) => self.out.push_str(&format!(
                "… {} more lines, not shown: the answer stops at {} KiB (read on with offset \
                 {cut})\n",
                total + 1 - cut,
                READ_MAX_BYTES >> 10
            )),
            None => {
                let shown_to = (self.end - 1).min(total);
                if shown_to < total {
                    self.out.push_str(&format!(
                        "… {} more lines (read on with offset {})\n",
                        total - shown_to,
                        shown_to + 1
                    ));
                }
            }
        }
        Ok(self.out)
    }

    /// The answer, complete, for a text whose lines after it were not all counted (see
    /// [`COUNT_AHEAD`]): some follow.
    fn finish_uncounted(mut self) -> String {
        let (next, why) = match self.cut_at {
            Some(cut) => (
                cut,
                format!(
                    ", not shown: the answer stops at {} KiB",
                    READ_MAX_BYTES >> 10
                ),
            ),
            None => (self.end, String::new()),
        };
        self.out.push_str(&format!(
            "… more lines follow{why} (read on with offset {next})\n"
        ));
        self.out
    }
}

/// [`numbered`] of the file at `path`, read line by line: only the lines the answer shows are
/// kept, and of each only as much as it shows, so a file of any size takes little memory. Each
/// line is decoded by itself (UTF-8, else the editor's fallback charset). Like the editor, it
/// refuses a file with NUL bytes (a binary file) in what it reads. Once the answer has its lines,
/// the rest is read only to count its lines, and at most [`COUNT_AHEAD`] of it. Blocking.
fn numbered_file(
    path: &Path,
    rel: &str,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<String, ToolError> {
    numbered_file_counting(path, rel, offset, limit, COUNT_AHEAD)
}

/// [`numbered_file`], counting the lines after the answer's in at most `count_ahead` bytes.
fn numbered_file_counting(
    path: &Path,
    rel: &str,
    offset: Option<u32>,
    limit: Option<u32>,
    count_ahead: u64,
) -> Result<String, ToolError> {
    use std::io::BufRead as _;
    /// Bytes of a line kept: enough for [`MAX_LINE_CHARS`] characters and one more, which
    /// tells that the line is cut.
    const KEEP: usize = (MAX_LINE_CHARS + 1) * 4;
    let failed = |e: std::io::Error| ToolError::Read(format!("cannot read {rel}: {e}"));
    let file = std::fs::File::open(path).map_err(failed)?;
    let mut reader = std::io::BufReader::with_capacity(64 << 10, file);
    let mut answer = Numbered::new(offset, limit);
    // The line being read (1-based), its start as far as it is kept, whether more of it was
    // left out, and whether it has any byte yet.
    let (mut n, mut line, mut cut, mut started) = (1, Vec::new(), false, false);
    let mut empty = true;
    // Bytes read in all, and when the answer had all its lines.
    let (mut read, mut complete) = (0u64, None);
    loop {
        if answer.complete_at(n) {
            let since = *complete.get_or_insert(read);
            if read - since > count_ahead {
                return Ok(answer.finish_uncounted());
            }
        }
        let chunk = reader.fill_buf().map_err(failed)?;
        if chunk.is_empty() {
            break;
        }
        if chunk.contains(&0) {
            return Err(ToolError::Read(format!(
                "cannot read {rel}: it contains NUL characters (is it a binary file?)"
            )));
        }
        empty = false;
        let len = chunk.len();
        let mut rest = chunk;
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == b'\n');
            let piece = &rest[..end.unwrap_or(rest.len())];
            if answer.wants(n) {
                let room = KEEP.saturating_sub(line.len());
                line.extend_from_slice(&piece[..piece.len().min(room)]);
                cut |= piece.len() > room;
            }
            started |= !piece.is_empty();
            match end {
                Some(end) => {
                    if answer.wants(n) {
                        answer.push(n, &decode_line(&line, cut));
                    }
                    (n, cut, started) = (n + 1, false, false);
                    line.clear();
                    rest = &rest[end + 1..];
                }
                None => rest = &[],
            }
        }
        reader.consume(len);
        read += len as u64;
    }
    if empty {
        return Ok(format!("({rel} is empty)"));
    }
    // A last line without a line break.
    if started && answer.wants(n) {
        answer.push(n, &decode_line(&line, cut));
    }
    answer.finish(if started { n } else { n - 1 })
}

/// The text of a line's bytes (`cut`: only its start, which may end inside a character): UTF-8
/// when it is, else the editor's fallback charset.
fn decode_line(bytes: &[u8], cut: bool) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(e) if cut && e.error_len().is_none() => {
            Cow::Borrowed(std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default())
        }
        Err(_) => saver::decode(bytes, None).map_or_else(
            |_| String::from_utf8_lossy(bytes),
            |decoded| Cow::Owned(decoded.text),
        ),
    }
}

/// The lines of `text` without their `'\n'` (a final newline does not start another line).
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Appends `     n\tline\n`, cutting lines longer than [`MAX_LINE_CHARS`]. The `\r` of a `\r\n`
/// line break is not shown.
fn push_numbered(out: &mut String, n: usize, line: &str) {
    let line = line.strip_suffix('\r').unwrap_or(line);
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => out.push_str(&format!("{n:>6}\t{}… [line truncated]\n", &line[..cut])),
        None => out.push_str(&format!("{n:>6}\t{line}\n")),
    }
}

async fn edit(
    ws: &Rc<Workspace>,
    call: u64,
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<String, ToolError> {
    let target = resolve_writable(ws, path)?;
    if old.is_empty() {
        return Err(ToolError::EmptyOldString);
    }
    let doc = existing_document(ws, &target).await?;
    // Computed and applied without yielding, so the live text cannot change in between.
    let edits = doc.with_text(|text| {
        if !uses_crlf(text) {
            return str_replace(text, old, new, replace_all);
        }
        // The model writes `\n`; the file's lines end in `\r\n`. An old_string the model copied
        // with its `\r`s still matches as it is.
        match str_replace(text, &to_crlf(old), &to_crlf(new), replace_all) {
            Err(EditError::NotFound) => str_replace(text, old, new, replace_all),
            other => other,
        }
    });
    let edits = match edits {
        Ok(edits) if !edits.is_empty() => edits,
        result => {
            forget_if_unused(ws, &doc);
            return Err(result.map_or_else(ToolError::from, |_| ToolError::Unchanged));
        }
    };
    let applied = apply_as_agent(&doc, call, &edits, false).await?;
    Ok(format!(
        "Edited {}: {}\n{}{}",
        target.rel,
        applied.lines_summary(),
        applied.excerpt,
        applied.save_warning(&target.rel)
    ))
}

async fn write(
    ws: &Rc<Workspace>,
    call: u64,
    path: &Path,
    content: &str,
) -> Result<String, ToolError> {
    let target = resolve_writable(ws, path)?;
    let existing = match current_document(ws, &target).await? {
        Some(doc) => Some(doc),
        None => match file_type(&target).await? {
            None => None,
            Some(gio::FileType::Directory) => return Err(ToolError::NotAFile(target.rel)),
            Some(_) => Some(
                ws.get_or_load(&target.path)
                    .await
                    .map_err(|e| ToolError::Read(target.for_agent(&e)))?,
            ),
        },
    };
    let doc = match existing {
        Some(doc) => doc,
        None => {
            create_parent_dirs(&target).await?;
            // Another call, or the user, may have created the document during the await: then
            // this write replaces its text like any write of an existing file.
            match ws.get(&target.path) {
                Some(doc) => doc,
                None => {
                    let doc = ws.create_closed(&target.path);
                    let applied =
                        apply_as_agent(&doc, call, &[TextEdit::insert(0, content)], true).await?;
                    let lines = split_lines(content).len();
                    return Ok(format!(
                        "Created {} ({})\n{}",
                        target.rel,
                        match lines {
                            1 => "1 line".to_owned(),
                            n => format!("{n} lines"),
                        },
                        applied.save_warning(&target.rel)
                    ));
                }
            }
        }
    };
    // Computed and applied without yielding, so the live text cannot change in between. A file
    // with `\r\n` line breaks keeps them on the lines that do not change.
    let edits = doc.with_text(|text| {
        if uses_crlf(text) {
            minimal_edits(text, &to_crlf(content))
        } else {
            minimal_edits(text, content)
        }
    });
    if edits.is_empty() {
        forget_if_unused(ws, &doc);
        return Ok(format!(
            "{} already has this content; nothing changed",
            target.rel
        ));
    }
    let applied = apply_as_agent(&doc, call, &edits, false).await?;
    Ok(format!(
        "Wrote {}: {}\n{}{}",
        target.rel,
        applied.lines_summary(),
        applied.excerpt,
        applied.save_warning(&target.rel)
    ))
}

async fn create_parent_dirs(target: &Target) -> Result<(), ToolError> {
    let Some(parent) = target.path.parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    let rel = target.rel.clone();
    gio::spawn_blocking(move || std::fs::create_dir_all(&parent))
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("the directory job panicked")))
        .map_err(|e| ToolError::CreateDir(format!("cannot create the directory of {rel}: {e}")))
}

/// What [`apply_as_agent`] did.
struct Applied {
    /// 0-based inclusive line ranges of the new text that changed, in order.
    changed: Vec<(u32, u32)>,
    /// The changed region with context, in `cat -n` format.
    excerpt: String,
    /// Why saving failed, if it did.
    save_error: Option<String>,
}

impl Applied {
    /// `lines 3–5` (1-based) covering every change.
    fn lines_summary(&self) -> String {
        let first = self.changed.iter().map(|r| r.0).min().unwrap_or(0) + 1;
        let last = self.changed.iter().map(|r| r.1).max().unwrap_or(0) + 1;
        if first == last {
            format!("line {first}")
        } else {
            format!("lines {first}–{last}")
        }
    }

    fn save_warning(&self, rel: &str) -> String {
        match &self.save_error {
            Some(e) => {
                format!("Warning: the change is in the editor, but saving {rel} failed: {e}\n")
            }
            None => String::new(),
        }
    }
}

/// §4.2 steps 4–7: start the review (baseline = the text before this edit), apply `edits` (pre-
/// edit offsets, descending) as one undoable action with `Origin::Agent { call }`, recompute the
/// hunks, and wait until the file is saved.
async fn apply_as_agent(
    doc: &DocRef,
    call: u64,
    edits: &[TextEdit],
    new_file: bool,
) -> Result<Applied, ToolError> {
    review_ui::start_review(doc, new_file);
    doc.apply_edits(edits, Origin::Agent { call })
        .map_err(|e| ToolError::Apply(e.to_string()))?;
    review_ui::recompute(doc);
    let save = doc.save();
    // From the document's line index and the quoted lines alone: on `large.tex`, copying an open
    // buffer's whole text took 3.5–7 ms and indexing it again 1 ms, reading seven lines takes
    // 16 µs. A buffer whose lines GTK counts otherwise (a lone `\r` breaks a line for it) is
    // copied once instead, since each `line_text` would copy it then.
    let (changed, excerpt) = {
        let index = doc.line_index();
        let changed = changed_lines(edits, &index);
        let lines_match = doc
            .buffer()
            .is_none_or(|b| u32::try_from(b.line_count()).is_ok_and(|n| n == index.line_count()));
        let excerpt = if lines_match {
            excerpt(&index, &changed, |n| doc.line_text(n).unwrap_or_default())
        } else {
            doc.with_text(|text| {
                excerpt(&index, &changed, |n| text[index.line_range(n)].to_owned())
            })
        };
        (changed, excerpt)
    };
    // GIO names the file in its message, as the system spells it.
    let save_error = save
        .await
        .err()
        .map(|e| agent_text(&e.to_string(), doc.path()));
    Ok(Applied {
        changed,
        excerpt,
        save_error,
    })
}

/// The lines (0-based, inclusive) of the text after `edits` that each edit produced, in text
/// order. `edits` use pre-edit offsets and are descending; a deletion yields the line it
/// happened on.
fn changed_lines(edits: &[TextEdit], index: &LineIndex) -> Vec<(u32, u32)> {
    let mut shift = 0isize;
    let mut out = Vec::with_capacity(edits.len());
    for e in edits.iter().rev() {
        let start = e.range.start.saturating_add_signed(shift);
        let end = start + e.text.len();
        let first = index.line_of(start);
        let last = if end > start {
            index.line_of(end - 1)
        } else {
            first
        };
        out.push((first, last));
        shift += e.delta();
    }
    out
}

/// The `changed` line ranges with [`CONTEXT_LINES`] around them, merged, in `cat -n` format;
/// at most [`MAX_QUOTED_LINES`] lines. `index` is the text's, and `line(n)` its line `n` without
/// the `\n`: only the quoted lines are read.
fn excerpt(index: &LineIndex, changed: &[(u32, u32)], line: impl Fn(u32) -> String) -> String {
    // The lines of `split_lines`: a final `\n` ends the last line rather than starting one.
    let mut lines = index.line_count();
    if lines > 1 && index.line_range(lines - 1).is_empty() {
        lines -= 1;
    }
    let last_line = lines - 1;
    let mut regions: Vec<(u32, u32)> = Vec::new();
    for &(first, last) in changed {
        let region = (
            first.saturating_sub(CONTEXT_LINES),
            (last + CONTEXT_LINES).min(last_line),
        );
        match regions.last_mut() {
            Some(prev) if region.0 <= prev.1 + 1 => prev.1 = prev.1.max(region.1),
            _ => regions.push(region),
        }
    }
    let mut out = String::new();
    let mut quoted = 0;
    for (i, &(first, last)) in regions.iter().enumerate() {
        if i > 0 {
            out.push_str("   ...\n");
        }
        for n in first..=last {
            if quoted == MAX_QUOTED_LINES {
                out.push_str("   ... (more changes; call read_file to see them)\n");
                return out;
            }
            push_numbered(&mut out, n as usize + 1, &line(n));
            quoted += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_lines_follow_cat_n() {
        let text = "a\nb\nc\n";
        assert_eq!(
            numbered(text, "x.tex", None, None).unwrap(),
            "     1\ta\n     2\tb\n     3\tc\n"
        );
        assert_eq!(
            numbered(text, "x.tex", Some(2), Some(1)).unwrap(),
            "     2\tb\n… 1 more lines (read on with offset 3)\n"
        );
        assert_eq!(
            numbered("no newline", "x.tex", None, None).unwrap(),
            "     1\tno newline\n"
        );
        assert_eq!(
            numbered(text, "x.tex", Some(9), None),
            Err(ToolError::OffsetPastEnd {
                offset: 9,
                lines: 3
            })
        );
        assert_eq!(
            numbered("", "x.tex", None, None).unwrap(),
            "(x.tex is empty)"
        );
        let long = "é".repeat(MAX_LINE_CHARS + 5);
        let out = numbered(&long, "x.tex", None, None).unwrap();
        assert!(out.ends_with("… [line truncated]\n"));
        assert_eq!(out.chars().filter(|&c| c == 'é').count(), MAX_LINE_CHARS);
    }

    /// A file read from the disk line by line answers as its whole text does, and a long answer
    /// stops at [`READ_MAX_BYTES`] saying where to read on.
    #[test]
    fn files_read_line_by_line_answer_as_whole_texts() {
        let dir = std::env::temp_dir().join(format!("lemma-read-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.tex");
        let long = "é".repeat(MAX_LINE_CHARS + 5);
        let many: String = (1..=5000)
            .map(|i| format!("line {i} {}\n", "x".repeat(100)))
            .collect();
        let texts = [
            "a\nb\nc\n".to_owned(),
            "no newline".to_owned(),
            "\n".to_owned(),
            "\n\n".to_owned(),
            String::new(),
            "alpha\r\nbeta\r\n".to_owned(),
            format!("a\n{long}\nb"),
            format!("{long}\r\n"),
            many.clone(),
        ];
        let ranges = [
            (None, None),
            (Some(2), Some(1)),
            (Some(3), None),
            (Some(9), None),
            (Some(4000), Some(3000)),
            (None, Some(5000)),
        ];
        for text in &texts {
            std::fs::write(&file, text).unwrap();
            for (offset, limit) in ranges {
                assert_eq!(
                    numbered_file(&file, "x.tex", offset, limit),
                    numbered(text, "x.tex", offset, limit),
                    "{offset:?} {limit:?} of {:?}",
                    &text[..text.len().min(30)]
                );
            }
        }
        // 5000 lines of about 110 bytes: the answer stops at the cap and says where to go on.
        let out = numbered(&many, "x.tex", None, Some(5000)).unwrap();
        assert!(out.len() <= READ_MAX_BYTES + 200, "{}", out.len());
        let last = out.lines().last().unwrap();
        let next: usize = last
            .strip_suffix(')')
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{last}"));
        assert!(
            last.starts_with(&format!(
                "… {} more lines, not shown: the answer stops at 256 KiB",
                5001 - next
            )),
            "{last}"
        );
        let shown = out.lines().nth_back(1).unwrap();
        assert!(shown.starts_with(&format!("{:>6}\tline {} ", next - 1, next - 1)));
        // A line in the fallback charset, and a binary file.
        std::fs::write(&file, b"caf\xe9\nok\n").unwrap();
        assert_eq!(
            numbered_file(&file, "x.tex", None, None).unwrap(),
            "     1\tcafé\n     2\tok\n"
        );
        std::fs::write(&file, b"a\nb\0c\n").unwrap();
        assert!(matches!(
            numbered_file(&file, "x.tex", None, None),
            Err(ToolError::Read(e)) if e.contains("NUL")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once the answer has its lines, a file read line by line is read on only so far to count
    /// the lines after them; past that the answer says that more follow (lines 1–100 of a file
    /// of gigabytes took as long as all of it).
    #[test]
    fn files_read_line_by_line_stop_after_the_answer() {
        let dir = std::env::temp_dir().join(format!("lemma-read-ahead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.tex");
        let text: String = (1..=20_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, &text).unwrap();
        let read =
            |offset, limit, ahead| numbered_file_counting(&file, "x.tex", offset, limit, ahead);
        assert_eq!(
            read(Some(2), Some(2), u64::MAX),
            numbered(&text, "x.tex", Some(2), Some(2))
        );
        assert_eq!(
            read(Some(2), Some(2), 0).unwrap(),
            "     2\tline 2\n     3\tline 3\n… more lines follow (read on with offset 4)\n"
        );
        // The end comes with the last lines asked for: counted.
        assert_eq!(
            read(Some(19_998), Some(2), 0),
            numbered(&text, "x.tex", Some(19_998), Some(2))
        );
        // An answer cut at its size.
        let long: String = (1..=5000)
            .map(|i| format!("line {i} {}\n", "x".repeat(100)))
            .collect();
        std::fs::write(&file, &long).unwrap();
        let out = read(None, Some(5000), 0).unwrap();
        let last = out.lines().last().unwrap();
        assert!(
            last.starts_with(
                "… more lines follow, not shown: the answer stops at 256 KiB (read on with offset "
            ),
            "{last}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_lines_map_edits_to_the_new_text() {
        // "l0\nl1\nl2\nl3\n": replace "l1" by "x\ny" and delete "l3\n" (descending).
        let edits = vec![
            TextEdit::delete(9..12),
            TextEdit {
                range: 3..5,
                text: "x\ny".into(),
            },
        ];
        let mut text = String::from("l0\nl1\nl2\nl3\n");
        lemma_core::apply_all(&mut text, &edits);
        assert_eq!(text, "l0\nx\ny\nl2\n");
        let index = LineIndex::new(&text);
        assert_eq!(changed_lines(&edits, &index), vec![(1, 2), (4, 4)]);
    }

    /// The excerpt of `text`, read through its line index.
    fn excerpt_of(text: &str, changed: &[(u32, u32)]) -> String {
        let index = LineIndex::new(text);
        excerpt(&index, changed, |n| text[index.line_range(n)].to_owned())
    }

    #[test]
    fn excerpts_merge_close_regions_and_add_context() {
        let text: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let out = excerpt_of(&text, &[(9, 9), (12, 12), (25, 25)]);
        let numbers: Vec<&str> = out
            .lines()
            .map(|l| l.split('\t').next().unwrap().trim())
            .collect();
        assert_eq!(
            numbers,
            [
                "7", "8", "9", "10", "11", "12", "13", "14", "15", "16", "...", "23", "24", "25",
                "26", "27", "28", "29"
            ]
        );
    }

    /// The excerpt quotes the lines `split_lines` gives, read through the line index: a final
    /// line break ends the last line instead of starting an empty one, and a deletion at the end
    /// of the text quotes the lines before it.
    #[test]
    fn excerpts_quote_the_lines_of_the_text() {
        for (text, changed) in [
            ("a\nb\n", (1, 1)),
            ("a\nb", (1, 1)),
            ("a\r\nb\r\n", (1, 1)),
            ("a\nb\n", (2, 2)),
        ] {
            let out = excerpt_of(text, &[changed]);
            assert_eq!(out, "     1\ta\n     2\tb\n", "{text:?}");
        }
        assert_eq!(excerpt_of("", &[(0, 0)]), "     1\t\n");
    }

    #[test]
    fn build_and_vcs_configuration_is_protected() {
        for rel in [
            "latexmkrc",
            ".latexmkrc",
            "chapters/latexmkrc",
            "LatexMkRc",
            ".git/config",
            ".git/hooks/pre-commit",
            ".envrc",
            "sub/.hidden/file.tex",
            // Names Windows opens as `latexmkrc` (a trailing dot or space is dropped, and
            // `::$DATA` is the file's data).
            "latexmkrc.",
            "latexmkrc ",
            "LatexMkRc. .",
            "latexmkrc::$DATA",
            "chapters/latexmkrc:x",
        ] {
            assert!(is_protected(Path::new(rel)), "{rel}");
        }
        for rel in [
            "main.tex",
            "chapters/intro.tex",
            "latexmkrc.tex",
            "refs.bib",
            "my.latexmkrc",
            "latexmkrc.d/main.tex",
        ] {
            assert!(!is_protected(Path::new(rel)), "{rel}");
        }
        assert!(
            ToolError::Protected("latexmkrc".into())
                .to_string()
                .starts_with("latexmkrc may not be written with Lemma's tools")
        );
    }

    #[test]
    fn crlf_files_are_matched_and_shown_without_their_carriage_returns() {
        assert!(uses_crlf("a\r\nb\r\n"));
        assert!(uses_crlf("a\r\nb\r\nc\n"), "mostly CRLF");
        assert!(!uses_crlf("a\nb\nc\r\n"), "mostly LF");
        assert!(!uses_crlf("no break"));
        assert_eq!(to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(to_crlf("one line"), "one line");
        assert!(matches!(to_crlf("x"), Cow::Borrowed(_)));
        assert_eq!(
            numbered("alpha\r\nbeta\r\n", "x.tex", None, None).unwrap(),
            "     1\talpha\n     2\tbeta\n"
        );
    }

    #[test]
    fn errors_read_as_the_contract_says() {
        assert_eq!(
            ToolError::NotFound.to_string(),
            "old_string not found — the user may have edited this region; call read_file"
        );
        assert_eq!(
            ToolError::NotUnique(3).to_string(),
            "old_string matches 3 times; add context or set replace_all"
        );
        assert_eq!(
            ToolError::OutsideRoot.to_string(),
            "path outside project root"
        );
    }
}
