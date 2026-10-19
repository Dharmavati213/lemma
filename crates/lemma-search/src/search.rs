//! Searching a project on a thread of its own, streaming the results ([`Searcher`]).

use crate::query::Matcher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Extensions of the files found by walking the project directory (the project graph's files
/// and the live documents are searched whatever their extension).
pub const EXTENSIONS: &[&str] = &["tex", "bib", "sty", "cls", "md"];

/// Batches the channel holds before the search thread waits for the receiver.
const CHANNEL_CAPACITY: usize = 4;
/// How often a search thread waiting for room in the channel checks it (and cancellation).
const SEND_RETRY: Duration = Duration::from_millis(2);

/// What a search looks through, in this order: `files`, then the files under `root` with one
/// of the [`EXTENSIONS`], then the remaining `live` documents. Each file is searched once.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// The directory to walk (breadth first, each directory in name order), skipping hidden
    /// files and directories (`.git`, `.build`), symbolic links and `skip_dirs`. `None`: no walk.
    pub root: Option<PathBuf>,
    /// Files searched first, in this order: the project graph in include order.
    pub files: Vec<PathBuf>,
    /// The text of open documents: searched instead of their file on disk.
    pub live: Vec<(PathBuf, Arc<str>)>,
    /// Directories the walk leaves out (absolute), such as the build output directory.
    pub skip_dirs: Vec<PathBuf>,
}

/// Bounds of one search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The search stops at the line where the count of matches reaches this
    /// ([`Summary::capped`], [`FileMatches::complete`]).
    pub max_matches: usize,
    /// Larger files on disk are skipped ([`Summary::skipped_large`]).
    pub max_file_bytes: u64,
    /// The walk stops after considering this many directory entries or directories
    /// ([`Summary::walk_truncated`]).
    pub max_walk_entries: usize,
    pub max_walk_dirs: usize,
    /// A batch is sent once it holds this many lines with matches…
    pub batch_lines: usize,
    /// …or this long after the previous one (the first file with matches goes at once).
    pub batch_interval: Duration,
    /// Longer lines are shortened to about this many bytes around their first match
    /// ([`LineMatch::text`]).
    pub line_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_matches: 20_000,
            max_file_bytes: 8 << 20,
            max_walk_entries: 50_000,
            max_walk_dirs: 4_000,
            batch_lines: 256,
            batch_interval: Duration::from_millis(30),
            line_bytes: 1024,
        }
    }
}

/// The matches in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatches {
    pub path: PathBuf,
    /// The text searched was a live document's ([`Scope::live`]), not the file on disk.
    pub live: bool,
    /// The lines with matches, in order.
    pub lines: Vec<LineMatch>,
    /// The matches in `lines`.
    pub matches: usize,
    /// False when [`Limits::max_matches`] was reached in this file: its later lines were not
    /// searched.
    pub complete: bool,
}

/// One line with matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// 0-based line number.
    pub line: u32,
    /// Byte offset of the line's start in the text searched.
    pub offset: usize,
    /// The line, without its line terminator (`\r\n` or `\n`); a line longer than
    /// [`Limits::line_bytes`] is cut to about that around its first match.
    pub text: String,
    /// Byte offset of `text` in the line (0 unless the line was cut before it).
    pub text_start: usize,
    /// `text` stops before the end of the line.
    pub cut_end: bool,
    /// The matches within `text`, as byte ranges of `text`.
    pub ranges: Vec<Range<usize>>,
    /// The matches in the whole line (`ranges` may leave out some of a cut line).
    pub matches: usize,
    /// The first match, as a byte range of the line (always within `text`).
    pub first: Range<usize>,
}

/// What a finished search saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files_searched: usize,
    pub files_with_matches: usize,
    pub lines: usize,
    pub matches: usize,
    /// Files on disk skipped because they hold a NUL byte.
    pub skipped_binary: usize,
    /// Files on disk skipped because they are larger than [`Limits::max_file_bytes`].
    pub skipped_large: usize,
    /// Files that could not be read.
    pub unreadable: usize,
    /// [`Limits::max_matches`] was reached: later lines and files were not searched.
    pub capped: bool,
    /// The walk stopped at [`Limits::max_walk_entries`] or [`Limits::max_walk_dirs`].
    pub walk_truncated: bool,
    pub elapsed: Duration,
}

/// What a search sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Files with matches, in search order.
    Files(Vec<FileMatches>),
    /// The search finished (the last event).
    Done(Summary),
}

/// Starts searches; the latest one wins. Cloning shares the "latest" (one `Searcher` per
/// search box).
#[derive(Clone, Debug, Default)]
pub struct Searcher {
    generation: Arc<AtomicU64>,
}

impl Searcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts searching `scope` with `matcher` on a new thread, cancelling the search this
    /// searcher started before. The results stream through [`Search::events`].
    pub fn start(&self, matcher: Matcher, scope: Scope, limits: Limits) -> Search {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = async_channel::bounded(CHANNEL_CAPACITY);
        let generation = self.generation.clone();
        let job = Job {
            id,
            generation: generation.clone(),
            matcher,
            scope,
            limits,
            tx,
        };
        // A thread that cannot start drops the sender: the channel closes without `Done`.
        let _ = std::thread::Builder::new()
            .name("lemma-search".into())
            .spawn(move || job.run());
        Search {
            id,
            generation,
            events: rx,
        }
    }

    /// Cancels the running search, if any.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// A running search (see [`Searcher::start`]).
#[derive(Debug)]
pub struct Search {
    id: u64,
    generation: Arc<AtomicU64>,
    events: async_channel::Receiver<Event>,
}

impl Search {
    /// The results: [`Event::Files`] batches, then [`Event::Done`], then the channel closes. A
    /// cancelled search (or one whose thread could not start) closes it without `Done`.
    /// Dropping the receiver stops the search too.
    pub fn events(&self) -> &async_channel::Receiver<Event> {
        &self.events
    }

    /// True while no later search of the same [`Searcher`] started and it was not cancelled.
    pub fn is_current(&self) -> bool {
        self.generation.load(Ordering::SeqCst) == self.id
    }

    /// Cancels this search (if it is still the current one).
    pub fn cancel(&self) {
        let _ = self.generation.compare_exchange(
            self.id,
            self.id + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Waits for the whole result (blocking): the files in order, and the summary unless the
    /// search was cancelled.
    pub fn wait(self) -> (Vec<FileMatches>, Option<Summary>) {
        let mut files = Vec::new();
        let mut summary = None;
        while let Ok(event) = self.events.recv_blocking() {
            match event {
                Event::Files(batch) => files.extend(batch),
                Event::Done(s) => summary = Some(s),
            }
        }
        (files, summary)
    }
}

struct Job {
    id: u64,
    generation: Arc<AtomicU64>,
    matcher: Matcher,
    scope: Scope,
    limits: Limits,
    tx: async_channel::Sender<Event>,
}

/// The search thread's state while it runs.
struct Run<'a> {
    job: &'a Job,
    live: HashMap<&'a Path, &'a Arc<str>>,
    seen: HashSet<PathBuf>,
    batch: Vec<FileMatches>,
    batch_lines: usize,
    last_sent: Option<Instant>,
    summary: Summary,
}

impl Job {
    fn cancelled(&self) -> bool {
        self.generation.load(Ordering::Relaxed) != self.id
    }

    /// Sends `event`, waiting while the channel is full (the receiver is behind). False when
    /// the search was cancelled meanwhile or the receiver is gone: a cancelled search never
    /// waits on a receiver that stopped reading.
    fn send(&self, mut event: Event) -> bool {
        loop {
            if self.cancelled() {
                return false;
            }
            match self.tx.try_send(event) {
                Ok(()) => return true,
                Err(async_channel::TrySendError::Closed(_)) => return false,
                Err(async_channel::TrySendError::Full(back)) => {
                    event = back;
                    std::thread::sleep(SEND_RETRY);
                }
            }
        }
    }

    fn run(self) {
        let started = Instant::now();
        let mut run = Run {
            job: &self,
            live: self
                .scope
                .live
                .iter()
                .map(|(p, t)| (p.as_path(), t))
                .collect(),
            seen: HashSet::new(),
            batch: Vec::new(),
            batch_lines: 0,
            last_sent: None,
            summary: Summary::default(),
        };
        let finished = run.all().is_continue() || run.summary.capped;
        if !finished || self.cancelled() {
            return;
        }
        if !run.batch.is_empty() {
            let batch = std::mem::take(&mut run.batch);
            if !self.send(Event::Files(batch)) {
                return;
            }
        }
        let mut summary = run.summary;
        summary.elapsed = started.elapsed();
        self.send(Event::Done(summary));
    }
}

impl Run<'_> {
    /// Visits every file in scope order; `Break` when cancelled, capped or disconnected.
    fn all(&mut self) -> ControlFlow<()> {
        let job = self.job;
        for path in &job.scope.files {
            self.visit(path)?;
        }
        if let Some(root) = &job.scope.root {
            self.walk(root)?;
        }
        for (path, _) in &job.scope.live {
            self.visit(path)?;
        }
        ControlFlow::Continue(())
    }

    fn walk(&mut self, root: &Path) -> ControlFlow<()> {
        let limits = &self.job.limits;
        let skip = &self.job.scope.skip_dirs;
        let mut queue = VecDeque::from([root.to_path_buf()]);
        let (mut dirs, mut entries_seen) = (0usize, 0usize);
        while let Some(dir) = queue.pop_front() {
            if dirs >= limits.max_walk_dirs {
                self.summary.walk_truncated = true;
                break;
            }
            dirs += 1;
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                entries_seen += 1;
                if entries_seen > limits.max_walk_entries {
                    self.summary.walk_truncated = true;
                    return ControlFlow::Continue(());
                }
                if entry.file_name().as_encoded_bytes().starts_with(b".") {
                    continue;
                }
                // `file_type` does not follow symbolic links, which could leave the project.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if !skip.contains(&path) {
                        queue.push_back(path);
                    }
                } else if kind.is_file() && has_search_extension(&path) {
                    self.visit(&path)?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn visit(&mut self, path: &Path) -> ControlFlow<()> {
        let job = self.job;
        if job.cancelled() {
            return ControlFlow::Break(());
        }
        if !self.seen.insert(path.to_path_buf()) {
            return ControlFlow::Continue(());
        }
        let (text, live) = match self.live.get(path) {
            Some(text) => (std::borrow::Cow::Borrowed(&***text), true),
            None => match read(path, job.limits.max_file_bytes) {
                Ok(text) => (std::borrow::Cow::Owned(text), false),
                Err(Skip::Large) => {
                    self.summary.skipped_large += 1;
                    return ControlFlow::Continue(());
                }
                Err(Skip::Binary) => {
                    self.summary.skipped_binary += 1;
                    return ControlFlow::Continue(());
                }
                Err(Skip::Unreadable) => {
                    self.summary.unreadable += 1;
                    return ControlFlow::Continue(());
                }
            },
        };
        self.summary.files_searched += 1;
        let mut lines = Vec::new();
        let mut matches = 0;
        let mut checked = 0u32;
        let room = job.limits.max_matches.saturating_sub(self.summary.matches);
        let flow = job.matcher.for_each_line(&text, |hits| {
            checked += 1;
            if checked.is_multiple_of(512) && job.cancelled() {
                return ControlFlow::Break(());
            }
            matches += hits.ranges.len();
            lines.push(line_match(&text, &hits, job.limits.line_bytes));
            if matches >= room {
                // The cap: this file's lines so far are the last results.
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        let complete = matches < room;
        if flow.is_break() && complete {
            return ControlFlow::Break(());
        }
        if lines.is_empty() {
            return ControlFlow::Continue(());
        }
        self.summary.files_with_matches += 1;
        self.summary.lines += lines.len();
        self.summary.matches += matches;
        self.batch_lines += lines.len();
        self.batch.push(FileMatches {
            path: path.to_path_buf(),
            live,
            lines,
            matches,
            complete,
        });
        let due = match self.last_sent {
            None => true,
            Some(at) => {
                self.batch_lines >= job.limits.batch_lines
                    || at.elapsed() >= job.limits.batch_interval
            }
        };
        if due {
            let batch = std::mem::take(&mut self.batch);
            self.batch_lines = 0;
            self.last_sent = Some(Instant::now());
            if !job.send(Event::Files(batch)) {
                return ControlFlow::Break(());
            }
        }
        if self.summary.matches >= job.limits.max_matches {
            self.summary.capped = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

/// True when `path` has one of the [`EXTENSIONS`] (any case).
pub fn has_search_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Why a file is not searched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Skip {
    Large,
    Binary,
    Unreadable,
}

/// A file's text: UTF-8 when valid, else ISO-8859-15 (what Lemma's editor reads it as).
fn read(path: &Path, max_bytes: u64) -> Result<String, Skip> {
    let size = std::fs::metadata(path).map_err(|_| Skip::Unreadable)?.len();
    if size > max_bytes {
        return Err(Skip::Large);
    }
    let bytes = std::fs::read(path).map_err(|_| Skip::Unreadable)?;
    if bytes.len() as u64 > max_bytes {
        return Err(Skip::Large);
    }
    if memchr::memchr(0, &bytes).is_some() {
        return Err(Skip::Binary);
    }
    Ok(decode(bytes))
}

/// `bytes` as text: UTF-8 when valid, else ISO-8859-15.
pub fn decode(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => e.into_bytes().iter().map(|&b| latin9(b)).collect(),
    }
}

/// The ISO-8859-15 character of byte `b` (Latin-1 except for eight positions).
fn latin9(b: u8) -> char {
    match b {
        0xA4 => '€',
        0xA6 => 'Š',
        0xA8 => 'š',
        0xB4 => 'Ž',
        0xB8 => 'ž',
        0xBC => 'Œ',
        0xBD => 'œ',
        0xBE => 'Ÿ',
        _ => char::from(b),
    }
}

/// The [`LineMatch`] of `hits` in `text`.
fn line_match(text: &str, hits: &crate::query::LineHits, line_bytes: usize) -> LineMatch {
    let full = &text[hits.span.clone()];
    let line = full.strip_suffix('\r').unwrap_or(full);
    let first = hits.ranges[0].start.min(line.len())..hits.ranges[0].end.min(line.len());
    let (start, end) = if line.len() <= line_bytes {
        (0, line.len())
    } else {
        // Some context before the first match, then as much as fits.
        let start = floor_char(line, first.start.saturating_sub(line_bytes / 8));
        let end = ceil_char(line, (start + line_bytes).max(first.end));
        (start, end)
    };
    let ranges = hits
        .ranges
        .iter()
        // Clipped to the text shown (a match of the `\r` of a CRLF line becomes empty).
        .filter(|r| r.start <= end && r.end >= start)
        .map(|r| r.start.clamp(start, end) - start..r.end.clamp(start, end) - start)
        .collect();
    LineMatch {
        line: hits.line,
        offset: hits.span.start,
        text: line[start..end].to_owned(),
        text_start: start,
        cut_end: end < line.len(),
        ranges,
        matches: hits.ranges.len(),
        first,
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
