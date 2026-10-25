//! The "pdf" thread: owns the poppler document (which is !Send) and the SyncTeX scanner, renders pages
//! on demand and answers sync and link queries, searches and outline requests.
//!
//! Queries (links, outline, page labels) are answered as soon as they are read, and sync queries
//! as soon as the SyncTeX file is parsed (below). The longer work comes in units of one page each
//! (of a few pages for their sizes), taken in turn from the render queue ([`PdfCmd::Want`]), the
//! search in progress ([`PdfCmd::Search`]), the thumbnail queue ([`PdfCmd::WantThumbnails`]) and,
//! once the render queue is empty, the pages whose sizes a [`PdfCmd::Reload`] took from the
//! previous generation, so none of them waits for the others to finish; new commands are read
//! between units.
//!
//! **SyncTeX.** Parsing a SyncTeX file takes about 0.25 ms per page (130 ms for 500 pages, more
//! under load), so it runs on a helper thread and pages are rendered meanwhile. A Forward or
//! Inverse that finds no scanner for the file as it is now waits for the parse, and so do the
//! queries after it; they are answered in order when it ends. The scanner a parse replaces is
//! freed on the helper thread before the parse starts, so memory never holds two (about 85 KiB
//! per page). After an Open whose SyncTeX file changed (a rebuild), the file is parsed right away
//! only when SyncTeX was used since the previous Open or in the last [`RECENT_SYNC`]; otherwise
//! the old scanner is only freed, and nobody who does not sync pays for a parse. A parse holds
//! libsynctex's lock (see `lemma_synctex`), so the label preview's PDF thread (another instance of
//! this one) waits for it with its own queries. [`PdfCmd::Trim`] frees the scanner and forgets
//! the recent use.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use lemma_synctex::{Scanner, SyncBox};

use crate::render::{self, RenderError, tile_grid};
use crate::rotation::{self, Rotation};
use crate::{
    PageKey, PdfCmd, PdfEvent, RenderReq, SearchOptions, ThumbReq, heap, links, outline, search,
};

/// An untiled request for a page that must be tiled is split into its tiles only when the grid has at
/// most this many tiles; otherwise it is dropped and the view has to ask for the visible tiles.
const MAX_AUTO_TILES: usize = 256;

/// After an Open whose SyncTeX file changed, the file is parsed right away when a sync query
/// came this recently (or since the previous Open).
const RECENT_SYNC: Duration = Duration::from_secs(5 * 60);

/// How many of the pages a Reload has not read yet one unit of work reads: about 0.03–0.1 ms
/// each (more under load), so a new command waits a few milliseconds for them at most.
const SIZES_PER_UNIT: usize = 32;

/// In unit tests, rendering at this scale panics, to exercise the restart path.
#[cfg(test)]
pub(crate) const PANIC_SCALE_Q: u16 = 7777;

/// How long dropping the last [`PdfHandle`] waits for the PDF thread to stop.
const DROP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends commands to the PDF thread. Cloning gives another sender to the same thread; the thread stops
/// on [`PdfCmd::Shutdown`], which dropping the last handle sends.
///
/// Dropping the last handle stops the thread and waits for it to finish (at most a few seconds), and
/// [`PdfHandle::shutdown_and_join`] does so explicitly: poppler tears down its global state when the
/// thread frees its last document, which crashes if the process exits at the same moment.
#[derive(Clone, Debug)]
pub struct PdfHandle {
    shared: Arc<Shared>,
}

/// What the clones of a [`PdfHandle`] share; dropped with the last of them.
#[derive(Debug)]
struct Shared {
    tx: mpsc::Sender<Msg>,
    /// The thread until it has been joined (`None` also when it could not be spawned).
    thread: Mutex<Option<Thread>>,
}

#[derive(Debug)]
struct Thread {
    join: JoinHandle<()>,
    /// Disconnects once the thread body has returned, after it dropped the document.
    done: mpsc::Receiver<()>,
}

/// What the PDF thread reads from its channel.
enum Msg {
    /// A command from a [`PdfHandle`].
    Cmd(PdfCmd),
    /// A SyncTeX file parsed on a helper thread.
    Parsed(Parsed),
}

/// Spawns the "pdf" thread. Loop: drain all pending commands (keep only the last Want; Open/Links/Outline
/// are handled immediately, Forward/Inverse once the SyncTeX file is parsed), do one unit of work, repeat;
/// block on recv when idle. Panics are caught and the thread restarted with the last Open.
///
/// The thread also stops when `events` is closed. It never waits on anything but its commands and
/// `send_blocking` on `events`, so a bounded `events` channel must be drained (or closed) while a handle
/// waits for the thread to stop. If the thread cannot be spawned, the error is logged and the returned
/// handle ignores every command.
pub fn spawn(events: async_channel::Sender<PdfEvent>) -> PdfHandle {
    let (tx, rx) = mpsc::channel();
    let (done_tx, done) = mpsc::sync_channel::<()>(0);
    // The helper threads that parse SyncTeX files hand their scanners back on the same channel.
    let parsed = tx.clone();
    let thread = std::thread::Builder::new()
        .name("pdf".into())
        .spawn(move || {
            run(rx, parsed, events);
            drop(done_tx);
        });
    let thread = match thread {
        Ok(join) => Some(Thread { join, done }),
        Err(e) => {
            tracing::error!("cannot spawn the pdf thread: {e}");
            None
        }
    };
    PdfHandle {
        shared: Arc::new(Shared {
            tx,
            thread: Mutex::new(thread),
        }),
    }
}

impl PdfHandle {
    /// Queues `cmd` for the PDF thread. Ignored once the thread has stopped.
    pub fn send(&self, cmd: PdfCmd) {
        // A send error only means the thread has stopped, which the caller cannot act on.
        let _ = self.shared.tx.send(Msg::Cmd(cmd));
    }

    /// Sends [`PdfCmd::Shutdown`] and waits up to `timeout` for the thread to finish (the page being
    /// rendered completes first; pending requests are dropped). Returns true once the thread has
    /// ended, so its document is freed; false if it is still running after `timeout`. Every handle
    /// ignores commands afterwards.
    pub fn shutdown_and_join(&self, timeout: Duration) -> bool {
        self.send(PdfCmd::Shutdown);
        self.shared.join(timeout)
    }
}

impl Shared {
    /// Waits up to `timeout` for the thread to end, then joins it; true if it has ended.
    fn join(&self, timeout: Duration) -> bool {
        let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(running) = thread.as_ref() else {
            return true;
        };
        if let Err(RecvTimeoutError::Timeout) = running.done.recv_timeout(timeout) {
            return false;
        }
        if let Some(ended) = thread.take()
            && ended.join.join().is_err()
        {
            tracing::error!("the pdf thread panicked while stopping");
        }
        true
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Cmd(PdfCmd::Shutdown));
        if !self.join(DROP_JOIN_TIMEOUT) {
            tracing::warn!("the pdf thread did not stop within {DROP_JOIN_TIMEOUT:?}");
        }
    }
}

/// State that survives a panic of the worker body.
#[derive(Default)]
struct Persistent {
    /// The current document's source bytes; a restarted worker reopens exactly the same document.
    last_open: Option<OpenedBytes>,
    /// Pending render requests, most urgent first.
    queue: VecDeque<RenderReq>,
    /// Pending thumbnail requests, most urgent first.
    thumbs: VecDeque<ThumbReq>,
    /// The search in progress. Its next page is advanced before the page is searched, so a
    /// page that panics is skipped after the restart.
    search: Option<SearchJob>,
    /// The kind of work unit done last (see [`Unit`]).
    last_unit: Option<Unit>,
    /// Forward and Inverse queries waiting for the SyncTeX file to be parsed, oldest first.
    sync_waiting: VecDeque<SyncQuery>,
    /// When the last sync query came (see [`RECENT_SYNC`]).
    last_sync: Option<Instant>,
    /// A sync query came since the last Open.
    synced_since_open: bool,
    /// The number of the last SyncTeX parse started (see [`Parsing::ticket`]). It survives
    /// restarts, so a parse that a panic left running is never taken for a newer one.
    parses: u64,
}

/// The kinds of work done one page (or a few pages' sizes) at a time, in turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Render,
    Search,
    Thumbnail,
    /// The sizes of the pages a Reload has not read yet, when no page waits to be rendered.
    Sizes,
}

impl Unit {
    /// Every kind, in the order they take turns.
    const ALL: [Unit; 4] = [Unit::Render, Unit::Search, Unit::Thumbnail, Unit::Sizes];

    /// The kinds in the order they take turns, starting after `last`.
    fn order_after(last: Option<Unit>) -> [Unit; 4] {
        let start = last
            .and_then(|last| Self::ALL.iter().position(|&unit| unit == last))
            .map_or(0, |i| i + 1);
        std::array::from_fn(|i| Self::ALL[(start + i) % Self::ALL.len()])
    }
}

/// A search in progress over the document of `generation`.
struct SearchJob {
    id: u64,
    generation: u64,
    query: String,
    options: SearchOptions,
    /// The page searched first.
    start: u32,
    /// How many pages have been searched (or are being searched).
    searched: u32,
    pages: u32,
}

struct OpenedBytes {
    generation: u64,
    pdf: PathBuf,
    bytes: glib::Bytes,
    /// [`PdfEvent::Opened`] reported the previous generation's page sizes (a Reload): a
    /// restarted worker, which reads every page, reports the sizes again.
    reused_sizes: bool,
}

/// The thread body: serves commands, restarting the worker after a panic.
fn run(rx: mpsc::Receiver<Msg>, tx: mpsc::Sender<Msg>, events: async_channel::Sender<PdfEvent>) {
    let mut state = Persistent::default();
    loop {
        let served = panic::catch_unwind(AssertUnwindSafe(|| {
            Worker::restore(&events, &tx, &mut state).serve(&rx)
        }));
        match served {
            Ok(()) => return,
            Err(payload) => tracing::error!(
                "pdf worker panicked ({}); restarting",
                panic_message(payload.as_ref())
            ),
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string payload")
}

/// The open document of one generation.
struct Doc {
    generation: u64,
    pdf: PathBuf,
    doc: poppler::Document,
    /// Page sizes in bp, as reported in [`PdfEvent::Opened`] (as displayed, so turned by `/Rotate`)
    /// and in [`PdfEvent::PageSizes`].
    sizes: Vec<(f64, f64)>,
    /// How each page is turned for display; `None` for a quarter-turned page whose direction has not
    /// been needed yet (see [`Doc::rotation`]).
    rotations: Vec<Option<Rotation>>,
    /// The pages whose size and rotation are still the previous generation's (a
    /// [`PdfCmd::Reload`]), by index; empty once every page has been read (see [`Doc::read`]).
    unread: Vec<bool>,
    /// How many pages `unread` marks.
    unread_left: usize,
    /// Where the next pass over `unread` starts: every page before it has been read.
    next_unread: usize,
    /// A page read had another size than the one reported, and no [`PdfEvent::PageSizes`] has
    /// said so yet.
    resized: bool,
}

/// The size of the 0-based page `i` of `doc` as displayed, and its rotation unless it is a
/// quarter turn, whose direction is read from its text when it is needed (see [`Doc::rotation`]).
fn read_page(doc: &poppler::Document, i: i32) -> ((f64, f64), Option<Rotation>) {
    match doc.page(i) {
        Some(page) => {
            let size = page.size();
            let turned = rotation::is_quarter_turned(&page, size);
            (size, (!turned).then_some(Rotation::Upright))
        }
        None => ((0.0, 0.0), Some(Rotation::Upright)),
    }
}

impl Doc {
    /// The document with every page's size read (about 0.1 ms a page: 60 ms for 600 pages).
    fn new(generation: u64, pdf: PathBuf, doc: poppler::Document) -> Self {
        let (sizes, rotations) = (0..doc.n_pages()).map(|i| read_page(&doc, i)).unzip();
        Self {
            generation,
            pdf,
            doc,
            sizes,
            rotations,
            unread: Vec::new(),
            unread_left: 0,
            next_unread: 0,
            resized: false,
        }
    }

    /// A new generation of `old`'s file with as many pages, which takes `old`'s sizes and
    /// rotations until its own pages are read (see [`Doc::read`]).
    fn reusing(generation: u64, pdf: PathBuf, doc: poppler::Document, old: &Doc) -> Self {
        let pages = old.sizes.len();
        Self {
            generation,
            pdf,
            doc,
            sizes: old.sizes.clone(),
            rotations: old.rotations.clone(),
            unread: if pages > 0 {
                vec![true; pages]
            } else {
                Vec::new()
            },
            unread_left: pages,
            next_unread: 0,
            resized: false,
        }
    }

    /// True while some page's size is still the previous generation's.
    fn has_unread(&self) -> bool {
        self.unread_left > 0
    }

    /// Reads the size and rotation of the 0-based `page` when they are still the previous
    /// generation's. Every use of a page's size reads it first.
    fn read(&mut self, page: u32) {
        let i = page as usize;
        if !self.unread.get(i).copied().unwrap_or(false) {
            return;
        }
        let (size, rotation) = read_page(&self.doc, page as i32);
        self.resized |= size != self.sizes[i];
        self.sizes[i] = size;
        self.rotations[i] = rotation;
        self.unread[i] = false;
        self.unread_left -= 1;
        if self.unread_left == 0 {
            self.unread = Vec::new();
        }
    }

    /// Reads the next `count` pages whose sizes are still the previous generation's.
    fn read_some(&mut self, count: usize) {
        let mut read = 0;
        // Some page at or after `next_unread` is unread while `unread_left` is not 0.
        while read < count && self.has_unread() {
            if self.unread[self.next_unread] {
                self.read(self.next_unread as u32);
                read += 1;
            }
            self.next_unread += 1;
        }
    }

    /// The page sizes, when a page read had another size than the one reported since they were
    /// last taken.
    fn take_resized(&mut self) -> Option<Vec<(f64, f64)>> {
        std::mem::take(&mut self.resized).then(|| self.sizes.clone())
    }

    /// How the 0-based `page` is turned for display (upright for pages that do not exist).
    fn rotation(&mut self, page: u32) -> Rotation {
        self.read(page);
        let i = page as usize;
        match self.rotations.get(i) {
            None => Rotation::Upright,
            Some(Some(rotation)) => *rotation,
            Some(None) => {
                let rotation = self
                    .page(page)
                    .map_or(Rotation::Quarter, |p| rotation::quarter_turn(&p));
                self.rotations[i] = Some(rotation);
                rotation
            }
        }
    }

    /// A SyncTeX box (TeX's page space) on its page as displayed.
    fn box_to_display(&mut self, b: SyncBox) -> SyncBox {
        let page = b.page.saturating_sub(1);
        self.read(page);
        match self.sizes.get(page as usize) {
            Some(&size) => self.rotation(page).box_to_display(b, size),
            None => b,
        }
    }

    /// A point of the 0-based `page` as displayed, in TeX's page space.
    fn point_from_display(&mut self, page: u32, point: (f64, f64)) -> (f64, f64) {
        self.read(page);
        match self.sizes.get(page as usize) {
            Some(&size) => self.rotation(page).point_from_display(point, size),
            None => point,
        }
    }

    /// The 0-based `page`, or `None` if the document has no such page.
    fn page(&self, page: u32) -> Option<poppler::Page> {
        if (page as usize) < self.sizes.len() {
            self.doc.page(page as i32)
        } else {
            None
        }
    }
}

/// Identifies one version of a `.synctex(.gz)` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    len: u64,
}

/// The SyncTeX file next to `pdf`: `<stem>.synctex.gz`, else `<stem>.synctex`.
fn synctex_stamp(pdf: &Path) -> Option<FileStamp> {
    ["synctex.gz", "synctex"].into_iter().find_map(|ext| {
        let meta = std::fs::metadata(pdf.with_extension(ext)).ok()?;
        Some(FileStamp {
            modified: meta.modified().ok()?,
            len: meta.len(),
        })
    })
}

/// A scanner for one PDF and one version of its SyncTeX file; `scanner` is `None` if it failed to
/// open (or there is no such file).
struct SyncState {
    pdf: PathBuf,
    stamp: Option<FileStamp>,
    scanner: Option<Scanner>,
}

/// A SyncTeX parse running on a helper thread whose scanner is wanted.
struct Parsing {
    /// The parse's number, in its [`Parsed`] too: the scanner of a parse no longer wanted (one
    /// that a newer Open or a trim replaced) is freed when it comes.
    ticket: u64,
    pdf: PathBuf,
    stamp: Option<FileStamp>,
}

/// A SyncTeX parse that ended on a helper thread.
struct Parsed {
    ticket: u64,
    pdf: PathBuf,
    /// The file's stamp when the parse started.
    stamp: Option<FileStamp>,
    scanner: Option<Scanner>,
}

/// A sync query waiting for the SyncTeX file to be parsed.
enum SyncQuery {
    Forward { file: PathBuf, line: u32 },
    Inverse { page: u32, x_bp: f64, y_bp: f64 },
}

/// Frees a scanner nothing wants any more on a thread of its own: freeing takes libsynctex's
/// lock, which a parse on another thread may hold for a while.
fn free_elsewhere(scanner: Option<Scanner>) {
    let Some(scanner) = scanner else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("synctex".into())
        .spawn(move || drop(scanner));
    // The scanner went with the closure, freed here.
    if let Err(e) = spawned {
        tracing::warn!("cannot start a thread to free a SyncTeX scanner: {e}");
    }
}

struct Worker<'a> {
    events: &'a async_channel::Sender<PdfEvent>,
    /// The thread's own channel, on which the helper threads hand back their SyncTeX parses.
    tx: &'a mpsc::Sender<Msg>,
    state: &'a mut Persistent,
    doc: Option<Doc>,
    /// The scanner of the SyncTeX file parsed last.
    sync: Option<SyncState>,
    /// The parse running on a helper thread whose scanner is wanted. While one runs there is no
    /// `sync`, so this thread never waits for the library lock the parse holds.
    parsing: Option<Parsing>,
}

impl<'a> Worker<'a> {
    /// A worker with the last opened document, if any. The SyncTeX scanner is reopened lazily.
    fn restore(
        events: &'a async_channel::Sender<PdfEvent>,
        tx: &'a mpsc::Sender<Msg>,
        state: &'a mut Persistent,
    ) -> Self {
        // Taken out first: if reopening panics, the next restart begins without a document.
        let last = state.last_open.take();
        let doc = last.as_ref().and_then(|l| {
            poppler::Document::from_bytes(&l.bytes, None)
                .ok()
                .map(|d| Doc::new(l.generation, l.pdf.clone(), d))
        });
        if doc.is_some() {
            state.last_open = last;
        }
        Self {
            events,
            tx,
            state,
            doc,
            sync: None,
            parsing: None,
        }
    }

    /// After a restart: page sizes that a Reload took from the previous generation are reported
    /// as read now, and the sync queries that were waiting get a parse of their own.
    fn resume(&mut self) -> ControlFlow<()> {
        let reused = self
            .state
            .last_open
            .as_mut()
            .is_some_and(|last| std::mem::take(&mut last.reused_sizes));
        if reused && let Some(doc) = &self.doc {
            let (generation, page_sizes) = (doc.generation, doc.sizes.clone());
            self.emit(PdfEvent::PageSizes {
                generation,
                page_sizes,
            })?;
        }
        if self.state.sync_waiting.is_empty() {
            ControlFlow::Continue(())
        } else {
            self.ensure_parse()
        }
    }

    /// True while a unit of work is pending (see the module docs).
    fn busy(&self) -> bool {
        !self.state.queue.is_empty()
            || !self.state.thumbs.is_empty()
            || self.state.search.is_some()
            || self.doc.as_ref().is_some_and(Doc::has_unread)
    }

    fn serve(mut self, rx: &mpsc::Receiver<Msg>) {
        if self.resume().is_break() {
            return;
        }
        loop {
            if !self.busy() {
                let Ok(msg) = rx.recv() else { return };
                if self.receive(msg).is_break() {
                    return;
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        if self.receive(msg).is_break() {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if self.work_next().is_break() {
                return;
            }
        }
    }

    fn receive(&mut self, msg: Msg) -> ControlFlow<()> {
        match msg {
            Msg::Cmd(cmd) => self.handle(cmd),
            Msg::Parsed(parsed) => self.parsed(parsed),
        }
    }

    /// Does one unit of the next kind of pending work, in turn (see [`Unit`]).
    fn work_next(&mut self) -> ControlFlow<()> {
        for unit in Unit::order_after(self.state.last_unit) {
            let pending = match unit {
                Unit::Render => !self.state.queue.is_empty(),
                Unit::Search => self.state.search.is_some(),
                Unit::Thumbnail => !self.state.thumbs.is_empty(),
                // After the pages asked for, which read their own sizes.
                Unit::Sizes => {
                    self.state.queue.is_empty() && self.doc.as_ref().is_some_and(Doc::has_unread)
                }
            };
            if !pending {
                continue;
            }
            self.state.last_unit = Some(unit);
            return match unit {
                Unit::Render => self.render_next(),
                Unit::Search => self.search_next(),
                Unit::Thumbnail => self.thumbnail_next(),
                Unit::Sizes => self.read_sizes_next(),
            };
        }
        ControlFlow::Continue(())
    }

    fn handle(&mut self, cmd: PdfCmd) -> ControlFlow<()> {
        match cmd {
            PdfCmd::Open { generation, pdf } => self.open(generation, pdf, false),
            PdfCmd::Reload { generation, pdf } => self.open(generation, pdf, true),
            PdfCmd::Want(reqs) => {
                self.state.queue = reqs.into();
                ControlFlow::Continue(())
            }
            PdfCmd::Forward { file, line } => self.sync_query(SyncQuery::Forward { file, line }),
            PdfCmd::Inverse { page, x_bp, y_bp } => {
                self.sync_query(SyncQuery::Inverse { page, x_bp, y_bp })
            }
            PdfCmd::Links { page } => {
                let (generation, links) = self.doc.as_ref().map_or((0, Vec::new()), |doc| {
                    // poppler logs a critical warning for out-of-range pages, so check first.
                    let links = doc
                        .page(page)
                        .map(|p| links::page_links(&doc.doc, &p))
                        .unwrap_or_default();
                    (doc.generation, links)
                });
                self.emit(PdfEvent::Links {
                    generation,
                    page,
                    links,
                })
            }
            PdfCmd::Search {
                id,
                query,
                options,
                start,
            } => self.start_search(id, query, options, start),
            PdfCmd::CancelSearch => {
                self.state.search = None;
                ControlFlow::Continue(())
            }
            PdfCmd::WantThumbnails(reqs) => {
                self.state.thumbs = reqs.into();
                ControlFlow::Continue(())
            }
            PdfCmd::Outline => {
                let (generation, items) = self.doc.as_ref().map_or((0, Vec::new()), |d| {
                    (d.generation, outline::outline(&d.doc))
                });
                self.emit(PdfEvent::Outline { generation, items })
            }
            PdfCmd::PageLabels => {
                let (generation, labels) = self.doc.as_ref().map_or((0, Vec::new()), |d| {
                    (d.generation, outline::page_labels(&d.doc))
                });
                self.emit(PdfEvent::PageLabels { generation, labels })
            }
            PdfCmd::Trim => self.trim(),
            PdfCmd::Shutdown => ControlFlow::Break(()),
        }
    }

    /// Replaces the search in progress with a search of the current document for `query`.
    fn start_search(
        &mut self,
        id: u64,
        query: String,
        options: SearchOptions,
        start: u32,
    ) -> ControlFlow<()> {
        let Some(doc) = &self.doc else {
            self.state.search = None;
            return self.emit(PdfEvent::SearchDone { id, generation: 0 });
        };
        let pages = doc.sizes.len() as u32;
        self.state.search = Some(SearchJob {
            id,
            generation: doc.generation,
            query,
            options,
            start: if pages == 0 { 0 } else { start.min(pages - 1) },
            searched: 0,
            pages,
        });
        // An empty document (or query) is done at once.
        self.finish_search_if_done()
    }

    /// Searches the next page of the search in progress and reports its matches.
    fn search_next(&mut self) -> ControlFlow<()> {
        let Some(job) = &mut self.state.search else {
            return ControlFlow::Continue(());
        };
        let current = self.doc.as_ref().map(|d| d.generation);
        if current != Some(job.generation) {
            // Another document was opened meanwhile; the caller searches it anew.
            self.state.search = None;
            return ControlFlow::Continue(());
        }
        if job.searched >= job.pages {
            return self.finish_search_if_done();
        }
        let page_index = (job.start + job.searched) % job.pages;
        job.searched += 1;
        let (id, generation) = (job.id, job.generation);
        let matches = if job.query.trim().is_empty() {
            Vec::new()
        } else {
            let (query, options) = (job.query.clone(), job.options);
            self.doc
                .as_ref()
                .and_then(|d| d.page(page_index))
                .map(|page| search::find(&page, &query, options))
                .unwrap_or_default()
        };
        if !matches.is_empty() {
            self.emit(PdfEvent::SearchPage {
                id,
                generation,
                page: page_index,
                matches,
            })?;
        }
        self.finish_search_if_done()
    }

    /// Ends the search in progress with [`PdfEvent::SearchDone`] once every page is searched
    /// (at once for an empty query).
    fn finish_search_if_done(&mut self) -> ControlFlow<()> {
        let done = self
            .state
            .search
            .as_ref()
            .is_some_and(|j| j.searched >= j.pages || j.query.trim().is_empty());
        if !done {
            return ControlFlow::Continue(());
        }
        let Some(job) = self.state.search.take() else {
            return ControlFlow::Continue(());
        };
        self.emit(PdfEvent::SearchDone {
            id: job.id,
            generation: job.generation,
        })
    }

    /// Renders the first valid queued thumbnail request and emits it; stale or invalid requests
    /// are skipped.
    fn thumbnail_next(&mut self) -> ControlFlow<()> {
        let Some(doc) = &mut self.doc else {
            self.state.thumbs.clear();
            return ControlFlow::Continue(());
        };
        while let Some(req) = self.state.thumbs.pop_front() {
            if req.generation != doc.generation || req.width == 0 {
                continue;
            }
            let Some(page) = doc.page(req.page) else {
                continue;
            };
            // The strip sizes the row by the page before its thumbnail comes.
            doc.read(req.page);
            match render::render_thumbnail(&page, req) {
                Ok(thumbnail) => {
                    self.report_sizes()?;
                    return self.emit(PdfEvent::Thumbnail(thumbnail));
                }
                Err(e) => tracing::warn!("rendering the thumbnail {req:?} failed: {e}"),
            }
        }
        self.report_sizes()
    }

    /// Opens `pdf` as the document of `generation`; for a `reload` of the current document with as
    /// many pages, with the current document's page sizes (see [`PdfCmd::Reload`]).
    fn open(&mut self, generation: u64, pdf: PathBuf, reload: bool) -> ControlFlow<()> {
        let data = match std::fs::read(&pdf) {
            Ok(data) => data,
            Err(e) => {
                let error = format!("{}: {e}", pdf.display());
                return self.emit(PdfEvent::OpenFailed { generation, error });
            }
        };
        // poppler keeps a reference to the bytes, so latexmk may overwrite the file afterwards.
        let bytes = glib::Bytes::from_owned(data);
        match poppler::Document::from_bytes(&bytes, None) {
            Ok(document) => {
                let pages = usize::try_from(document.n_pages()).unwrap_or(0);
                let doc = match &self.doc {
                    Some(old) if reload && old.pdf == pdf && old.sizes.len() == pages => {
                        Doc::reusing(generation, pdf.clone(), document, old)
                    }
                    _ => Doc::new(generation, pdf.clone(), document),
                };
                let page_sizes = doc.sizes.clone();
                let reused_sizes = doc.has_unread();
                self.doc = Some(doc);
                self.state.last_open = Some(OpenedBytes {
                    generation,
                    pdf,
                    bytes,
                    reused_sizes,
                });
                self.emit(PdfEvent::Opened {
                    generation,
                    page_sizes,
                })?;
                self.sync_after_open()
            }
            Err(e) => {
                let error = format!("{}: {e}", pdf.display());
                self.emit(PdfEvent::OpenFailed { generation, error })
            }
        }
    }

    /// Reads a few more of the pages a Reload took the sizes of from the previous generation, and
    /// reports the sizes when one of them differs.
    fn read_sizes_next(&mut self) -> ControlFlow<()> {
        if let Some(doc) = &mut self.doc {
            doc.read_some(SIZES_PER_UNIT);
        }
        self.report_sizes()
    }

    /// Sends [`PdfEvent::PageSizes`] when a page read had another size than the one reported.
    fn report_sizes(&mut self) -> ControlFlow<()> {
        let Some(doc) = &mut self.doc else {
            return ControlFlow::Continue(());
        };
        match doc.take_resized() {
            Some(page_sizes) => {
                let generation = doc.generation;
                self.emit(PdfEvent::PageSizes {
                    generation,
                    page_sizes,
                })
            }
            None => ControlFlow::Continue(()),
        }
    }

    // ----- SyncTeX -----

    /// A Forward or Inverse: answered now when the current document's SyncTeX file is parsed as
    /// it is now, otherwise once it is (after the queries before it).
    fn sync_query(&mut self, query: SyncQuery) -> ControlFlow<()> {
        self.state.last_sync = Some(Instant::now());
        self.state.synced_since_open = true;
        self.state.sync_waiting.push_back(query);
        self.ensure_parse()
    }

    /// Answers the waiting sync queries when the scanner is that of the current document's
    /// SyncTeX file as it is now (or there is no such file to parse); otherwise makes sure the file
    /// is being parsed.
    fn ensure_parse(&mut self) -> ControlFlow<()> {
        let Some(pdf) = self.doc.as_ref().map(|d| d.pdf.clone()) else {
            // No document: every query is answered without a result.
            return self.answer_waiting();
        };
        let stamp = synctex_stamp(&pdf);
        if matches!(&self.sync, Some(s) if s.pdf == pdf && s.stamp == stamp) {
            return self.answer_waiting();
        }
        if stamp.is_none() {
            // No SyncTeX file: nothing to parse, and no query finds anything.
            self.parsing = None;
            let old = self.sync.replace(SyncState {
                pdf,
                stamp,
                scanner: None,
            });
            free_elsewhere(old.and_then(|s| s.scanner));
            return self.answer_waiting();
        }
        if self
            .parsing
            .as_ref()
            .is_some_and(|p| p.pdf == pdf && p.stamp == stamp)
        {
            // Answered when it ends.
            return ControlFlow::Continue(());
        }
        self.start_parse(pdf, stamp)
    }

    /// Parses the SyncTeX file of `pdf` (as of `stamp`) on a helper thread, which frees the
    /// current scanner first; [`Worker::parsed`] takes the result.
    fn start_parse(&mut self, pdf: PathBuf, stamp: Option<FileStamp>) -> ControlFlow<()> {
        self.state.parses += 1;
        let ticket = self.state.parses;
        let old = self.sync.take();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new().name("synctex".into()).spawn({
            let pdf = pdf.clone();
            move || {
                drop(old);
                let scanner = Scanner::open(&pdf);
                // Fails only once the PDF thread has stopped; the scanner is freed here then.
                let _ = tx.send(Msg::Parsed(Parsed {
                    ticket,
                    pdf,
                    stamp,
                    scanner,
                }));
            }
        });
        match spawned {
            Ok(_) => {
                self.parsing = Some(Parsing { ticket, pdf, stamp });
                ControlFlow::Continue(())
            }
            Err(e) => {
                // The old scanner went with the closure; the file is parsed here instead.
                tracing::warn!("cannot start a thread to parse the SyncTeX file: {e}");
                self.parsing = None;
                self.sync = Some(SyncState {
                    scanner: Scanner::open(&pdf),
                    pdf,
                    stamp,
                });
                self.answer_waiting()
            }
        }
    }

    /// A parse ended on a helper thread. Its scanner becomes the current one when it is still
    /// wanted and the file has not changed again meanwhile; the waiting queries are answered with
    /// it, or get a parse of the file as it is now.
    fn parsed(&mut self, parsed: Parsed) -> ControlFlow<()> {
        let Parsed {
            ticket,
            pdf,
            stamp,
            scanner,
        } = parsed;
        if self.parsing.as_ref().is_none_or(|p| p.ticket != ticket) {
            free_elsewhere(scanner);
            return ControlFlow::Continue(());
        }
        self.parsing = None;
        let current =
            self.doc.as_ref().is_some_and(|d| d.pdf == pdf) && synctex_stamp(&pdf) == stamp;
        if !current {
            free_elsewhere(scanner);
            if self.state.sync_waiting.is_empty() {
                return ControlFlow::Continue(());
            }
            return self.ensure_parse();
        }
        self.sync = Some(SyncState {
            pdf,
            stamp,
            scanner,
        });
        self.answer_waiting()
    }

    /// After an Open: the scanner of another SyncTeX file than the new document's goes, and the
    /// new file is parsed right away when queries wait or SyncTeX was used since the previous Open
    /// or in the last [`RECENT_SYNC`].
    fn sync_after_open(&mut self) -> ControlFlow<()> {
        let recent = std::mem::take(&mut self.state.synced_since_open)
            || self
                .state
                .last_sync
                .is_some_and(|t| t.elapsed() < RECENT_SYNC);
        let Some(pdf) = self.doc.as_ref().map(|d| d.pdf.clone()) else {
            return ControlFlow::Continue(());
        };
        let stamp = synctex_stamp(&pdf);
        let current = matches!(&self.sync, Some(s) if s.pdf == pdf && s.stamp == stamp)
            || self
                .parsing
                .as_ref()
                .is_some_and(|p| p.pdf == pdf && p.stamp == stamp);
        if current {
            return ControlFlow::Continue(());
        }
        // A parse of the old file is not wanted any more.
        self.parsing = None;
        if recent || !self.state.sync_waiting.is_empty() {
            return self.ensure_parse();
        }
        free_elsewhere(self.sync.take().and_then(|s| s.scanner));
        ControlFlow::Continue(())
    }

    /// Answers the waiting sync queries, oldest first, with the current scanner.
    fn answer_waiting(&mut self) -> ControlFlow<()> {
        while let Some(query) = self.state.sync_waiting.pop_front() {
            self.answer(query)?;
        }
        ControlFlow::Continue(())
    }

    fn answer(&mut self, query: SyncQuery) -> ControlFlow<()> {
        match query {
            SyncQuery::Forward { file, line } => {
                let generation = self.doc.as_ref().map_or(0, |d| d.generation);
                let mut boxes = self
                    .scanner()
                    .map(|s| s.forward(&file, line))
                    .unwrap_or_default();
                // SyncTeX boxes are in TeX's page space; the page sizes the view knows are as displayed.
                if let Some(doc) = &mut self.doc {
                    for b in &mut boxes {
                        *b = doc.box_to_display(*b);
                    }
                }
                // The view lays out the pages read for the boxes before it draws them.
                self.report_sizes()?;
                self.emit(PdfEvent::Forward { generation, boxes })
            }
            SyncQuery::Inverse { page, x_bp, y_bp } => {
                let (x_bp, y_bp) = match &mut self.doc {
                    Some(doc) => doc.point_from_display(page, (x_bp, y_bp)),
                    None => (x_bp, y_bp),
                };
                let target = page.checked_add(1).and_then(|page| {
                    self.scanner()
                        .and_then(|s| s.inverse(page, x_bp as f32, y_bp as f32))
                });
                self.report_sizes()?;
                self.emit(PdfEvent::Inverse { target })
            }
        }
    }

    /// The scanner of the current document's SyncTeX file, once parsed (see
    /// [`Worker::ensure_parse`]).
    fn scanner(&mut self) -> Option<&mut Scanner> {
        let pdf = &self.doc.as_ref()?.pdf;
        self.sync
            .as_mut()
            .filter(|s| s.pdf == *pdf)?
            .scanner
            .as_mut()
    }

    /// [`PdfCmd::Trim`]: frees the scanner (and the result of a parse ahead that no query waits
    /// for), forgets that SyncTeX was used, and gives the heap's free pages back.
    fn trim(&mut self) -> ControlFlow<()> {
        self.sync = None;
        if self.state.sync_waiting.is_empty() {
            self.parsing = None;
        }
        self.state.last_sync = None;
        self.state.synced_since_open = false;
        heap::trim();
        ControlFlow::Continue(())
    }

    // ----- rendering -----

    /// Renders the first valid queued request and emits it. Stale or invalid requests are skipped; an
    /// untiled request for a page that must be tiled is replaced by its tiles.
    fn render_next(&mut self) -> ControlFlow<()> {
        let Some(doc) = &mut self.doc else {
            self.state.queue.clear();
            return ControlFlow::Continue(());
        };
        while let Some(req) = self.state.queue.pop_front() {
            let key = req.key;
            if key.generation != doc.generation || key.page as usize >= doc.sizes.len() {
                continue;
            }
            // A page whose size a Reload took from the previous generation is tiled by its own.
            doc.read(key.page);
            let size = doc.sizes[key.page as usize];
            if key.tile.is_none()
                && let Some(grid) = tile_grid(size, key.scale_q)
            {
                push_tiles(&mut self.state.queue, req, grid);
                continue;
            }
            #[cfg(test)]
            if key.scale_q == PANIC_SCALE_Q {
                panic!("test-triggered pdf worker panic");
            }
            let Some(page) = doc.page(key.page) else {
                continue;
            };
            match render::render(&page, key) {
                Ok(rendered) => {
                    // The view lays the page out at its new size before its bitmap comes.
                    self.report_sizes()?;
                    return self.emit(PdfEvent::Rendered(rendered));
                }
                Err(RenderError::EmptyTile) => {}
                Err(e) => tracing::warn!("rendering {key:?} failed: {e}"),
            }
        }
        self.report_sizes()
    }

    /// Sends `event`; stops the worker if nobody listens any more.
    fn emit(&self, event: PdfEvent) -> ControlFlow<()> {
        match self.events.send_blocking(event) {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        }
    }
}

/// Puts the tiles of `req` at the front of `queue` in row-major order.
fn push_tiles(queue: &mut VecDeque<RenderReq>, req: RenderReq, (cols, rows): (u16, u16)) {
    if usize::from(cols) * usize::from(rows) > MAX_AUTO_TILES {
        tracing::warn!(
            "page {} at scale_q {} needs {cols}x{rows} tiles; request visible tiles instead",
            req.key.page,
            req.key.scale_q
        );
        return;
    }
    for row in (0..rows).rev() {
        for col in (0..cols).rev() {
            queue.push_front(RenderReq {
                key: PageKey {
                    tile: Some((col, row)),
                    ..req.key
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
    }

    fn next(rx: &async_channel::Receiver<PdfEvent>) -> PdfEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(ev) => return ev,
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("no event: {e:?}"),
            }
        }
    }

    fn req(generation: u64, page: u32, scale_q: u16) -> RenderReq {
        RenderReq {
            key: PageKey {
                generation,
                page,
                scale_q,
                tile: None,
                dark: false,
            },
        }
    }

    #[test]
    fn restarts_after_a_panic_with_the_same_document_and_queue() {
        let (tx, rx) = async_channel::unbounded();
        let pdf = spawn(tx);
        pdf.send(PdfCmd::Open {
            generation: 3,
            pdf: fixture("pdf/stress.pdf"),
        });
        assert!(matches!(next(&rx), PdfEvent::Opened { generation: 3, .. }));
        pdf.send(PdfCmd::Want(vec![req(3, 0, PANIC_SCALE_Q), req(3, 4, 8)]));
        // The panicking request is consumed; the rest of the queue survives the restart, and the
        // document is reopened silently under the same generation.
        match next(&rx) {
            PdfEvent::Rendered(r) => assert_eq!(r.key, req(3, 4, 8).key),
            other => panic!("unexpected {other:?}"),
        }
        pdf.send(PdfCmd::Want(vec![req(3, 5, 8)]));
        match next(&rx) {
            PdfEvent::Rendered(r) => assert_eq!(r.key, req(3, 5, 8).key),
            other => panic!("unexpected {other:?}"),
        }
        assert!(pdf.shutdown_and_join(Duration::from_secs(10)));
    }

    #[test]
    fn untiled_requests_for_huge_pages_expand_in_row_major_order() {
        let mut queue = VecDeque::from([req(1, 2, 8)]);
        let big = req(1, 0, 24);
        push_tiles(&mut queue, big, (2, 2));
        let tiles: Vec<_> = queue.iter().map(|r| r.key.tile).collect();
        assert_eq!(
            tiles,
            [Some((0, 0)), Some((1, 0)), Some((0, 1)), Some((1, 1)), None]
        );
        assert!(queue.iter().take(4).all(|r| r.key.scale_q == 24));
        push_tiles(&mut queue, big, (100, 100));
        assert_eq!(queue.len(), 5);
    }

    #[test]
    fn synctex_stamp_prefers_the_gzipped_file() {
        let pdf = fixture("proj/main.pdf");
        let gz = std::fs::metadata(fixture("proj/main.synctex.gz")).unwrap();
        let stamp = synctex_stamp(&pdf).expect("fixture has a synctex file");
        assert_eq!(stamp.len, gz.len());
        assert_eq!(synctex_stamp(&fixture("pdf/stress.pdf")), None);
    }

    #[test]
    fn units_take_turns() {
        assert_eq!(Unit::order_after(None), Unit::ALL);
        assert_eq!(
            Unit::order_after(Some(Unit::Thumbnail)),
            [Unit::Sizes, Unit::Render, Unit::Search, Unit::Thumbnail]
        );
        assert_eq!(Unit::order_after(Some(Unit::Sizes)), Unit::ALL);
    }

    /// A fresh directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lemma-pdf-worker-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        lemma_core::canonicalize(&dir).unwrap()
    }

    /// A copy of `tests/fixtures/proj` with its PDF and SyncTeX file; returns the PDF.
    fn proj_copy(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        std::fs::create_dir_all(dir.join("chapters")).unwrap();
        for file in [
            "main.tex",
            "chapters/yoneda.tex",
            "main.pdf",
            "main.synctex.gz",
        ] {
            std::fs::copy(fixture(&format!("proj/{file}")), dir.join(file)).unwrap();
        }
        dir.join("main.pdf")
    }

    /// Gives the SyncTeX file next to `pdf` another modification time, as a rebuild does.
    fn touch_synctex(pdf: &Path, n: u64) {
        std::fs::File::options()
            .write(true)
            .open(pdf.with_extension("synctex.gz"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n))
            .unwrap();
    }

    /// Hands `worker` the next SyncTeX parse that ends on a helper thread.
    fn take_parse(worker: &mut Worker, rx: &mpsc::Receiver<Msg>) {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Msg::Parsed(parsed)) => assert!(worker.parsed(parsed).is_continue()),
            Ok(Msg::Cmd(cmd)) => panic!("unexpected {cmd:?}"),
            Err(e) => panic!("no parse ended: {e}"),
        }
    }

    /// The events sent so far.
    fn drain(received: &async_channel::Receiver<PdfEvent>) -> Vec<PdfEvent> {
        std::iter::from_fn(|| received.try_recv().ok()).collect()
    }

    fn forward(pdf: &Path) -> PdfCmd {
        PdfCmd::Forward {
            file: pdf.with_file_name("main.tex"),
            line: 66,
        }
    }

    fn open(worker: &mut Worker, generation: u64, pdf: &Path) {
        let cmd = PdfCmd::Open {
            generation,
            pdf: pdf.to_path_buf(),
        };
        assert!(worker.handle(cmd).is_continue());
    }

    #[test]
    fn sync_queries_wait_for_the_parse_while_pages_render() {
        let pdf = proj_copy("wait");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        // SyncTeX was not used: nothing is parsed ahead.
        assert!(worker.parsing.is_none() && worker.sync.is_none());
        let _ = worker.handle(forward(&pdf));
        let _ = worker.handle(PdfCmd::Inverse {
            page: 1,
            x_bp: 200.0,
            y_bp: 420.0,
        });
        assert!(worker.parsing.is_some());
        assert_eq!(worker.state.sync_waiting.len(), 2);
        // Pages are rendered meanwhile.
        let _ = worker.handle(PdfCmd::Want(vec![req(1, 0, 8)]));
        assert!(worker.render_next().is_continue());
        assert!(matches!(
            drain(&received)[..],
            [
                PdfEvent::Opened { generation: 1, .. },
                PdfEvent::Rendered(_)
            ]
        ));
        take_parse(&mut worker, &rx);
        match &drain(&received)[..] {
            [
                PdfEvent::Forward {
                    generation: 1,
                    boxes,
                },
                PdfEvent::Inverse {
                    target: Some(target),
                },
            ] => {
                assert_eq!(boxes.first().map(|b| b.page), Some(2), "{boxes:?}");
                assert_eq!(target.line, 66);
            }
            other => panic!("{other:?}"),
        }
        // The scanner stays for the next queries, which are answered at once.
        let _ = worker.handle(forward(&pdf));
        assert!(matches!(drain(&received)[..], [PdfEvent::Forward { .. }]));
        assert!(worker.parsing.is_none());
    }

    #[test]
    fn a_rebuild_is_parsed_ahead_only_after_a_recent_sync() {
        let pdf = proj_copy("ahead");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        touch_synctex(&pdf, 1);
        open(&mut worker, 2, &pdf);
        assert!(worker.parsing.is_none(), "never synced");
        let _ = worker.handle(forward(&pdf));
        take_parse(&mut worker, &rx);
        assert!(worker.sync.as_ref().is_some_and(|s| s.scanner.is_some()));
        // A sync since the last Open: the rebuild's file is parsed right away, and the old
        // scanner goes first.
        touch_synctex(&pdf, 2);
        open(&mut worker, 3, &pdf);
        assert!(worker.parsing.is_some() && worker.sync.is_none());
        take_parse(&mut worker, &rx);
        assert!(worker.sync.as_ref().is_some_and(|s| s.scanner.is_some()));
        // None since, but one a moment ago.
        touch_synctex(&pdf, 3);
        open(&mut worker, 4, &pdf);
        assert!(worker.parsing.is_some());
        take_parse(&mut worker, &rx);
        // Parsing ahead answers nothing: only the one query was.
        let events = drain(&received);
        let forwards = events
            .iter()
            .filter(|e| matches!(e, PdfEvent::Forward { .. }))
            .count();
        assert_eq!(forwards, 1, "{events:?}");
        // Long ago, and none since the last Open: only the old scanner goes.
        worker.state.last_sync = Instant::now().checked_sub(RECENT_SYNC + Duration::from_secs(1));
        touch_synctex(&pdf, 4);
        open(&mut worker, 5, &pdf);
        assert!(worker.parsing.is_none() && worker.sync.is_none());
        // An unchanged SyncTeX file keeps its scanner.
        let _ = worker.handle(forward(&pdf));
        take_parse(&mut worker, &rx);
        open(&mut worker, 6, &pdf);
        assert!(worker.parsing.is_none() && worker.sync.is_some());
    }

    #[test]
    fn a_parse_of_a_file_changed_meanwhile_is_dropped() {
        let pdf = proj_copy("changed");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        let _ = worker.handle(forward(&pdf));
        let first = worker.parsing.as_ref().map(|p| p.ticket);
        touch_synctex(&pdf, 1);
        take_parse(&mut worker, &rx);
        // The file as it is now is parsed for the query.
        assert!(worker.sync.is_none());
        assert!(
            worker
                .parsing
                .as_ref()
                .is_some_and(|p| Some(p.ticket) != first)
        );
        assert!(matches!(drain(&received)[..], [PdfEvent::Opened { .. }]));
        take_parse(&mut worker, &rx);
        assert!(matches!(
            drain(&received)[..],
            [PdfEvent::Forward { generation: 1, ref boxes }] if !boxes.is_empty()
        ));
    }

    #[test]
    fn a_new_open_detaches_the_running_parse() {
        let pdf = proj_copy("detach");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        let _ = worker.handle(forward(&pdf));
        let first = worker.parsing.as_ref().map(|p| p.ticket);
        touch_synctex(&pdf, 1);
        open(&mut worker, 2, &pdf);
        assert!(
            worker
                .parsing
                .as_ref()
                .is_some_and(|p| Some(p.ticket) != first)
        );
        // Both parses end, in either order; the query is answered once, by the new one.
        take_parse(&mut worker, &rx);
        take_parse(&mut worker, &rx);
        match &drain(&received)[..] {
            [
                PdfEvent::Opened { generation: 1, .. },
                PdfEvent::Opened { generation: 2, .. },
                PdfEvent::Forward {
                    generation: 2,
                    boxes,
                },
            ] => assert!(!boxes.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn trim_frees_the_scanner_and_forgets_the_recent_sync() {
        let pdf = proj_copy("trim");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        let _ = worker.handle(forward(&pdf));
        take_parse(&mut worker, &rx);
        assert!(worker.sync.is_some());
        assert!(worker.handle(PdfCmd::Trim).is_continue());
        assert!(worker.sync.is_none());
        assert!(worker.state.last_sync.is_none() && !worker.state.synced_since_open);
        // The rebuild after a trim is not parsed ahead; the next query parses it.
        touch_synctex(&pdf, 1);
        open(&mut worker, 2, &pdf);
        assert!(worker.parsing.is_none());
        let _ = worker.handle(forward(&pdf));
        // A trim leaves the parse that a query waits for.
        let _ = worker.handle(PdfCmd::Trim);
        assert!(worker.parsing.is_some());
        take_parse(&mut worker, &rx);
        let forwards = drain(&received)
            .into_iter()
            .filter(|e| matches!(e, PdfEvent::Forward { boxes, .. } if !boxes.is_empty()))
            .count();
        assert_eq!(forwards, 2);
        // A parse ahead (after a sync) that nothing waits for goes with a trim.
        let _ = worker.handle(forward(&pdf));
        touch_synctex(&pdf, 2);
        open(&mut worker, 3, &pdf);
        assert!(worker.parsing.is_some());
        let _ = worker.handle(PdfCmd::Trim);
        assert!(worker.parsing.is_none());
        take_parse(&mut worker, &rx);
        assert!(worker.sync.is_none());
    }

    #[test]
    fn a_restart_answers_the_queries_that_waited() {
        let pdf = proj_copy("restart");
        let (events, received) = async_channel::unbounded();
        let (tx, rx) = mpsc::channel();
        let mut state = Persistent::default();
        {
            let mut worker = Worker::restore(&events, &tx, &mut state);
            open(&mut worker, 1, &pdf);
            let _ = worker.handle(forward(&pdf));
        }
        // The worker body panicked here; the query still waits.
        let mut worker = Worker::restore(&events, &tx, &mut state);
        assert!(worker.resume().is_continue());
        assert!(worker.parsing.as_ref().is_some_and(|p| p.ticket == 2));
        // The parse started before the restart is not taken for the new one.
        take_parse(&mut worker, &rx);
        take_parse(&mut worker, &rx);
        match &drain(&received)[..] {
            [
                PdfEvent::Opened { generation: 1, .. },
                PdfEvent::Forward {
                    generation: 1,
                    boxes,
                },
            ] => assert!(!boxes.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    /// Writes a PDF of blank pages of `sizes` (bp).
    fn write_pages(path: &Path, sizes: &[(f64, f64)]) {
        use std::fmt::Write;
        let kids: Vec<String> = (0..sizes.len()).map(|p| format!("{} 0 R", p + 3)).collect();
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                sizes.len()
            ),
        ];
        for (w, h) in sizes {
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Resources << >> >>"
            ));
        }
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1).unwrap();
        }
        let xref = pdf.len();
        writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            pdf,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .unwrap();
        std::fs::write(path, pdf).unwrap();
    }

    const LETTER: (f64, f64) = (612.0, 792.0);

    fn reload(worker: &mut Worker, generation: u64, pdf: &Path) {
        let cmd = PdfCmd::Reload {
            generation,
            pdf: pdf.to_path_buf(),
        };
        assert!(worker.handle(cmd).is_continue());
    }

    /// Lets the worker read every page a Reload has not read yet.
    fn read_all_sizes(worker: &mut Worker) {
        while worker.doc.as_ref().is_some_and(Doc::has_unread) {
            assert!(worker.read_sizes_next().is_continue());
        }
    }

    #[test]
    fn a_reload_reads_a_page_before_it_renders_it() {
        let dir = temp_dir("reload");
        let pdf = dir.join("doc.pdf");
        write_pages(&pdf, &[LETTER; 10]);
        let (events, received) = async_channel::unbounded();
        let (tx, _rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        let mut resized = [LETTER; 10];
        resized[7] = (300.0, 400.0);
        write_pages(&pdf, &resized);
        reload(&mut worker, 2, &pdf);
        let _ = worker.handle(PdfCmd::Want(vec![req(2, 7, 8)]));
        assert!(worker.render_next().is_continue());
        match &drain(&received)[..] {
            [
                PdfEvent::Opened { generation: 1, .. },
                PdfEvent::Opened {
                    generation: 2,
                    page_sizes: reported,
                },
                PdfEvent::PageSizes {
                    generation: 2,
                    page_sizes,
                },
                PdfEvent::Rendered(page),
            ] => {
                assert_eq!(reported[..], [LETTER; 10], "the sizes of generation 1");
                assert_eq!(page_sizes[..], resized);
                assert_eq!((page.width, page.height), (300, 400));
            }
            other => panic!("{other:?}"),
        }
        // The other pages are read a few at a time; their sizes are the same.
        read_all_sizes(&mut worker);
        assert!(drain(&received).is_empty());
        // A page changed elsewhere is reported once the pass reads it.
        write_pages(&pdf, &[LETTER; 10]);
        reload(&mut worker, 3, &pdf);
        read_all_sizes(&mut worker);
        match &drain(&received)[..] {
            [
                PdfEvent::Opened {
                    generation: 3,
                    page_sizes: reported,
                },
                PdfEvent::PageSizes {
                    generation: 3,
                    page_sizes,
                },
            ] => {
                assert_eq!(reported[..], resized);
                assert_eq!(page_sizes[..], [LETTER; 10]);
            }
            other => panic!("{other:?}"),
        }
        // A thumbnail's row is sized before it comes.
        let mut wide = [LETTER; 10];
        wide[2] = (792.0, 612.0);
        write_pages(&pdf, &wide);
        reload(&mut worker, 4, &pdf);
        let thumb = ThumbReq {
            generation: 4,
            page: 2,
            width: 66,
            dark: false,
        };
        let _ = worker.handle(PdfCmd::WantThumbnails(vec![thumb]));
        assert!(worker.thumbnail_next().is_continue());
        match &drain(&received)[..] {
            [
                PdfEvent::Opened { generation: 4, .. },
                PdfEvent::PageSizes {
                    generation: 4,
                    page_sizes,
                },
                PdfEvent::Thumbnail(thumbnail),
            ] => {
                assert_eq!(page_sizes[..], wide);
                assert_eq!((thumbnail.width, thumbnail.height), (66, 51));
            }
            other => panic!("{other:?}"),
        }
        // With another page count every page is read first, as by an Open.
        write_pages(&pdf, &[LETTER; 4]);
        reload(&mut worker, 5, &pdf);
        assert!(!worker.doc.as_ref().unwrap().has_unread());
        assert!(!worker.state.last_open.as_ref().unwrap().reused_sizes);
    }

    #[test]
    fn a_reload_reads_the_other_pages_once_none_waits_to_be_rendered() {
        let dir = temp_dir("reload-order");
        let pdf = dir.join("doc.pdf");
        write_pages(&pdf, &[LETTER; 40]);
        let (events, _received) = async_channel::unbounded();
        let (tx, _rx) = mpsc::channel();
        let mut state = Persistent::default();
        let mut worker = Worker::restore(&events, &tx, &mut state);
        open(&mut worker, 1, &pdf);
        reload(&mut worker, 2, &pdf);
        let _ = worker.handle(PdfCmd::Want(vec![req(2, 0, 8), req(2, 1, 8)]));
        let unread = |worker: &Worker| worker.doc.as_ref().map_or(0, |d| d.unread_left);
        // The pages asked for first, each read as it is rendered.
        assert!(worker.work_next().is_continue());
        assert!(worker.work_next().is_continue());
        assert!(worker.state.queue.is_empty());
        assert_eq!(unread(&worker), 38);
        // Then the others, a unit at a time.
        assert!(worker.work_next().is_continue());
        assert_eq!(unread(&worker), 38 - SIZES_PER_UNIT);
    }

    #[test]
    fn a_restart_reports_the_sizes_a_reload_reused() {
        let dir = temp_dir("reload-restart");
        let pdf = dir.join("doc.pdf");
        write_pages(&pdf, &[LETTER; 3]);
        let (events, received) = async_channel::unbounded();
        let (tx, _rx) = mpsc::channel();
        let mut state = Persistent::default();
        {
            let mut worker = Worker::restore(&events, &tx, &mut state);
            open(&mut worker, 1, &pdf);
            write_pages(&pdf, &[LETTER, (100.0, 100.0), LETTER]);
            reload(&mut worker, 2, &pdf);
        }
        // The worker body panicked here, before the pages were read.
        let mut worker = Worker::restore(&events, &tx, &mut state);
        assert!(worker.resume().is_continue());
        match &drain(&received)[..] {
            [
                PdfEvent::Opened { generation: 1, .. },
                PdfEvent::Opened { generation: 2, .. },
                PdfEvent::PageSizes {
                    generation: 2,
                    page_sizes,
                },
            ] => assert_eq!(page_sizes[..], [LETTER, (100.0, 100.0), LETTER]),
            other => panic!("{other:?}"),
        }
        // Once.
        drop(worker);
        let mut worker = Worker::restore(&events, &tx, &mut state);
        assert!(worker.resume().is_continue());
        assert!(drain(&received).is_empty());
    }
}
