//! The "pdf" thread: owns the poppler document (which is !Send) and the SyncTeX scanner, renders pages
//! on demand and answers sync and link queries, searches and outline requests.
//!
//! Queries (sync, links, outline, page labels) are answered as soon as they are read. The longer
//! work comes in units of one page each, taken in turn from the render queue ([`PdfCmd::Want`]),
//! the search in progress ([`PdfCmd::Search`]) and the thumbnail queue
//! ([`PdfCmd::WantThumbnails`]), so none of them waits for the others to finish; new commands are
//! read between units.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use lemma_synctex::{Scanner, SyncBox};

use crate::render::{self, RenderError, tile_grid};
use crate::rotation::{self, Rotation};
use crate::{
    PageKey, PdfCmd, PdfEvent, RenderReq, SearchOptions, ThumbReq, links, outline, search,
};

/// An untiled request for a page that must be tiled is split into its tiles only when the grid has at
/// most this many tiles; otherwise it is dropped and the view has to ask for the visible tiles.
const MAX_AUTO_TILES: usize = 256;

/// In unit tests, rendering at this scale panics, to exercise the restart path.
#[cfg(test)]
pub(crate) const PANIC_SCALE_Q: u16 = 7777;

/// How long dropping the last [`PdfHandle`] waits for the PDF thread to stop.
const DROP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends commands to the PDF thread. Cloning gives another sender to the same thread; the thread stops
/// on [`PdfCmd::Shutdown`] or when every handle is dropped.
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
    tx: mpsc::Sender<PdfCmd>,
    /// The thread until it has been joined (`None` also when it could not be spawned).
    thread: Mutex<Option<Thread>>,
}

#[derive(Debug)]
struct Thread {
    join: JoinHandle<()>,
    /// Disconnects once the thread body has returned, after it dropped the document.
    done: mpsc::Receiver<()>,
}

/// Spawns the "pdf" thread. Loop: drain all pending commands (keep only the last Want; Open/Forward/Inverse/Links
/// are handled immediately), render one page, repeat; block on recv when idle. Panics are caught and the
/// thread restarted with the last Open.
///
/// The thread also stops when `events` is closed. It never waits on anything but its commands and
/// `send_blocking` on `events`, so a bounded `events` channel must be drained (or closed) while a handle
/// waits for the thread to stop. If the thread cannot be spawned, the error is logged and the returned
/// handle ignores every command.
pub fn spawn(events: async_channel::Sender<PdfEvent>) -> PdfHandle {
    let (tx, rx) = mpsc::channel();
    let (done_tx, done) = mpsc::sync_channel::<()>(0);
    let thread = std::thread::Builder::new()
        .name("pdf".into())
        .spawn(move || {
            run(rx, events);
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
        let _ = self.shared.tx.send(cmd);
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
        let _ = self.tx.send(PdfCmd::Shutdown);
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
}

/// The kinds of work done one page at a time, in turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Render,
    Search,
    Thumbnail,
}

impl Unit {
    /// The kinds in the order they take turns, starting after `last`.
    fn order_after(last: Option<Unit>) -> [Unit; 3] {
        match last {
            None | Some(Unit::Thumbnail) => [Unit::Render, Unit::Search, Unit::Thumbnail],
            Some(Unit::Render) => [Unit::Search, Unit::Thumbnail, Unit::Render],
            Some(Unit::Search) => [Unit::Thumbnail, Unit::Render, Unit::Search],
        }
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
}

/// The thread body: serves commands, restarting the worker after a panic.
fn run(rx: mpsc::Receiver<PdfCmd>, events: async_channel::Sender<PdfEvent>) {
    let mut state = Persistent::default();
    loop {
        let served = panic::catch_unwind(AssertUnwindSafe(|| {
            Worker::restore(&events, &mut state).serve(&rx)
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
    /// Page sizes in bp, as reported in [`PdfEvent::Opened`] (as displayed, so turned by `/Rotate`).
    sizes: Vec<(f64, f64)>,
    /// How each page is turned for display; `None` for a quarter-turned page whose direction has not
    /// been needed yet (see [`Doc::rotation`]).
    rotations: Vec<Option<Rotation>>,
}

impl Doc {
    fn new(generation: u64, pdf: PathBuf, doc: poppler::Document) -> Self {
        let (sizes, rotations): (Vec<_>, Vec<_>) = (0..doc.n_pages())
            .map(|i| match doc.page(i) {
                Some(page) => {
                    let size = page.size();
                    let turned = rotation::is_quarter_turned(&page, size);
                    (size, (!turned).then_some(Rotation::Upright))
                }
                None => ((0.0, 0.0), Some(Rotation::Upright)),
            })
            .unzip();
        Self {
            generation,
            pdf,
            doc,
            sizes,
            rotations,
        }
    }

    /// How the 0-based `page` is turned for display (upright for pages that do not exist).
    fn rotation(&mut self, page: u32) -> Rotation {
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
        match self.sizes.get(page as usize) {
            Some(&size) => self.rotation(page).box_to_display(b, size),
            None => b,
        }
    }

    /// A point of the 0-based `page` as displayed, in TeX's page space.
    fn point_from_display(&mut self, page: u32, point: (f64, f64)) -> (f64, f64) {
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

/// A scanner for one PDF and one version of its SyncTeX file; `scanner` is `None` if it failed to open.
struct SyncState {
    pdf: PathBuf,
    stamp: Option<FileStamp>,
    scanner: Option<Scanner>,
}

struct Worker<'a> {
    events: &'a async_channel::Sender<PdfEvent>,
    state: &'a mut Persistent,
    doc: Option<Doc>,
    sync: Option<SyncState>,
}

impl<'a> Worker<'a> {
    /// A worker with the last opened document, if any. The SyncTeX scanner is reopened lazily.
    fn restore(events: &'a async_channel::Sender<PdfEvent>, state: &'a mut Persistent) -> Self {
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
            state,
            doc,
            sync: None,
        }
    }

    /// True while a unit of work is pending (see the module docs).
    fn busy(&self) -> bool {
        !self.state.queue.is_empty() || !self.state.thumbs.is_empty() || self.state.search.is_some()
    }

    fn serve(mut self, rx: &mpsc::Receiver<PdfCmd>) {
        loop {
            if !self.busy() {
                let Ok(cmd) = rx.recv() else { return };
                if self.handle(cmd).is_break() {
                    return;
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        if self.handle(cmd).is_break() {
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

    /// Does one unit of the next kind of pending work, in turn (see [`Unit`]).
    fn work_next(&mut self) -> ControlFlow<()> {
        for unit in Unit::order_after(self.state.last_unit) {
            let pending = match unit {
                Unit::Render => !self.state.queue.is_empty(),
                Unit::Search => self.state.search.is_some(),
                Unit::Thumbnail => !self.state.thumbs.is_empty(),
            };
            if !pending {
                continue;
            }
            self.state.last_unit = Some(unit);
            return match unit {
                Unit::Render => self.render_next(),
                Unit::Search => self.search_next(),
                Unit::Thumbnail => self.thumbnail_next(),
            };
        }
        ControlFlow::Continue(())
    }

    fn handle(&mut self, cmd: PdfCmd) -> ControlFlow<()> {
        match cmd {
            PdfCmd::Open { generation, pdf } => self.open(generation, pdf),
            PdfCmd::Want(reqs) => {
                self.state.queue = reqs.into();
                ControlFlow::Continue(())
            }
            PdfCmd::Forward { file, line } => {
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
                self.emit(PdfEvent::Forward { generation, boxes })
            }
            PdfCmd::Inverse { page, x_bp, y_bp } => {
                let (x_bp, y_bp) = match &mut self.doc {
                    Some(doc) => doc.point_from_display(page, (x_bp, y_bp)),
                    None => (x_bp, y_bp),
                };
                let target = page.checked_add(1).and_then(|page| {
                    self.scanner()
                        .and_then(|s| s.inverse(page, x_bp as f32, y_bp as f32))
                });
                self.emit(PdfEvent::Inverse { target })
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
        let Some(doc) = &self.doc else {
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
            match render::render_thumbnail(&page, req) {
                Ok(thumbnail) => return self.emit(PdfEvent::Thumbnail(thumbnail)),
                Err(e) => tracing::warn!("rendering the thumbnail {req:?} failed: {e}"),
            }
        }
        ControlFlow::Continue(())
    }

    fn open(&mut self, generation: u64, pdf: PathBuf) -> ControlFlow<()> {
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
            Ok(doc) => {
                let doc = Doc::new(generation, pdf.clone(), doc);
                let page_sizes = doc.sizes.clone();
                self.doc = Some(doc);
                self.state.last_open = Some(OpenedBytes {
                    generation,
                    pdf,
                    bytes,
                });
                self.emit(PdfEvent::Opened {
                    generation,
                    page_sizes,
                })
            }
            Err(e) => {
                let error = format!("{}: {e}", pdf.display());
                self.emit(PdfEvent::OpenFailed { generation, error })
            }
        }
    }

    /// The scanner for the current document, (re)opened when the PDF path or its SyncTeX file changed.
    fn scanner(&mut self) -> Option<&mut Scanner> {
        let pdf = &self.doc.as_ref()?.pdf;
        let stamp = synctex_stamp(pdf);
        let current = matches!(&self.sync, Some(s) if s.pdf == *pdf && s.stamp == stamp);
        if !current {
            // Free the old scanner before parsing the new file.
            self.sync = None;
            self.sync = Some(SyncState {
                pdf: pdf.clone(),
                stamp,
                scanner: Scanner::open(pdf),
            });
        }
        self.sync.as_mut()?.scanner.as_mut()
    }

    /// Renders the first valid queued request and emits it. Stale or invalid requests are skipped; an
    /// untiled request for a page that must be tiled is replaced by its tiles.
    fn render_next(&mut self) -> ControlFlow<()> {
        let Some(doc) = &self.doc else {
            self.state.queue.clear();
            return ControlFlow::Continue(());
        };
        while let Some(req) = self.state.queue.pop_front() {
            let key = req.key;
            if key.generation != doc.generation {
                continue;
            }
            let Some(&size) = doc.sizes.get(key.page as usize) else {
                continue;
            };
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
                Ok(rendered) => return self.emit(PdfEvent::Rendered(rendered)),
                Err(RenderError::EmptyTile) => {}
                Err(e) => tracing::warn!("rendering {key:?} failed: {e}"),
            }
        }
        ControlFlow::Continue(())
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
}
