//! The math hover preview in the editor (ARCHITECTURE §5 "Math hover preview"). The TeX side
//! is the `lemma-preview` crate.
//!
//! # The hover
//!
//! A hover provider (`editor::hover` at [`hover::PRIORITY_MATH`], below references and
//! citations) answers inside math — inline, display or a math environment, from the scanner's
//! line states — and inside the environments of `[preview] envs` (`tikzcd`), with the rendered
//! formula. Its range is the whole formula (delimiters included), so the popover stays while
//! the pointer moves inside it. `[preview] math_hover = false` turns it off (the provider is
//! registered only while it is on, following reloads); documents that are not LaTeX get none.
//!
//! The formula is compiled with its project's preamble: the root document (the hovered one when
//! it is complete, else its `% !TEX root`, subfiles parent, the project index's root that
//! includes it, or the last build's root) gives the preamble from its live buffer when it is
//! open, from disk otherwise; definitions from outside the preamble (the root's body and the
//! hovered chapter) come along. A document without a root uses `lemma_preview`'s default
//! preamble.
//!
//! The rendered formula is a [`FormulaPicture`]: an alpha mask painted in the popover's text
//! colour (it follows the light or dark style, live), or the colours themselves when the formula
//! has some (inverted with the hue kept in dark mode). It is rendered at the editor font's size
//! and the display's scale, and shrunk to [`MAX_WIDTH`] when wider. A TeX error is shown briefly
//! instead (the message and TeX's context), as is a timeout.
//!
//! # Costs
//!
//! Nothing happens before the first hover that finds a formula: no thread, no cache, no file
//! ([`is_loaded`]). Then one pipeline serves every window of the process: at most one render
//! runs (on the jobs thread), and a request made meanwhile waits in a single slot that a newer
//! one replaces (the latest wins); rendered formulas are kept in an LRU cache of `[preview]
//! cache` entries (and at most [`CACHE_BYTES`] of pixels), keyed by the preamble, the
//! definitions, the formula and the scale, so hovering a formula again shows it at once. No TeX
//! process stays alive.
//!
//! The formats live in `Paths::preview_cache_dir()`; scripted runs (and windows without
//! paths) use a temporary directory that goes with the window, and tests set their own
//! ([`set_cache_dir`]).
//!
//! # Live preview
//!
//! With `[preview] live = true` (the default), while the cursor is inside math — inline `$…$` /
//! `\(…\)`, display math, or an environment of `[preview] envs` and the editor is in insert mode (always, without Vim),
//! the formula is rendered in a card below it, [`LIVE_DELAY`] after the last change or cursor
//! move. The space is reserved with `pixels-below-lines` on the formula's last line (tag
//! [`LIVE_TAG`]) and the card is an overlay of the view, so it never covers text. It keeps the
//! last good picture (dimmed) with the error under it while the formula does not compile, and
//! goes away when the cursor leaves the formula or insert mode. The view's signals are only
//! connected while `live` is on.
//!
//! Hooks: [`install`] once per window; [`attach`] / [`detach`] per editor tab (after the hover
//! registry exists; detach before it goes); [`run_demo`] is `--demo math-preview` (resolves
//! when a rendered formula is shown, for `k-math-preview.png` in light and dark).

use super::hover::{self, HoverContent, HoverCtx, ProviderId};
use super::{Editor, TexView, Trigger, VimMode};
use crate::document::Document;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};
use lemma_preview::{
    DEFAULT_PREAMBLE, Formula, Job, Lru, Outcome, Point, PreambleSource, PreviewError, Previewer,
    cache_key, definitions, find_formula, preamble_hash, split_preamble,
};
use lemma_syntax::Lines;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the live preview waits after the last change or cursor move.
pub const LIVE_DELAY: Duration = Duration::from_millis(400);
/// The most pixel data the cache holds (besides its `[preview] cache` entries).
pub const CACHE_BYTES: usize = 16 << 20;
/// Formulas wider than this (logical pixels) are shown smaller.
pub const MAX_WIDTH: f64 = 620.0;
/// The tag that reserves the live preview's space below a formula.
pub const LIVE_TAG: &str = "lemma-math-live";
/// The live preview card's CSS class (besides libadwaita's `card`).
pub const LIVE_CSS_CLASS: &str = "lemma-math-live";
/// Device pixels per bp at scale 1 with an 11 pt editor font: formulas a little larger than the
/// text around them (Computer Modern's x-height is smaller than a monospace font's).
const BASE_ZOOM: f64 = 1.7;
/// Space between the formula's last line and the live card, and around the card.
const LIVE_GAP: i32 = 6;
/// Documents whose preamble and definitions are remembered (per version).
const DOC_INFOS: usize = 32;

// ----- the pipeline (one per process, created at the first hover) -----

/// What a render gave: the picture, or why there is none.
pub type Delivered = Rc<Result<Rc<Picture>, PreviewError>>;

/// A rendered formula as a texture.
#[derive(Debug)]
pub struct Picture {
    pub texture: gdk::Texture,
    /// Size in device pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per logical pixel it was rendered for.
    pub device_scale: f64,
    /// An alpha mask (painted in the text colour), else colour.
    pub mask: bool,
}

impl Picture {
    fn new(image: lemma_preview::Image, device_scale: f64) -> Self {
        let (width, height, stride) = (image.width, image.height, image.stride());
        let mask = image.is_mask();
        let (format, bytes) = match image.pixels {
            lemma_preview::Pixels::Alpha(p) => (gdk::MemoryFormat::A8, p),
            lemma_preview::Pixels::Bgra(p) => (gdk::MemoryFormat::B8g8r8a8Premultiplied, p),
        };
        let texture = gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            format,
            &glib::Bytes::from_owned(bytes),
            stride,
        );
        Self {
            texture: texture.upcast(),
            width,
            height,
            device_scale: device_scale.max(0.5),
            mask,
        }
    }

    /// Bytes of pixel data.
    pub fn byte_size(&self) -> usize {
        let per_pixel = if self.mask { 1 } else { 4 };
        self.width as usize * self.height as usize * per_pixel
    }

    /// The size shown, in logical pixels: the device size at the display's scale, shrunk to
    /// [`MAX_WIDTH`].
    pub fn logical_size(&self) -> (f64, f64) {
        let w = f64::from(self.width) / self.device_scale;
        let h = f64::from(self.height) / self.device_scale;
        let shrink = (MAX_WIDTH / w).min(1.0);
        (w * shrink, h * shrink)
    }
}

/// One formula to render, with its cache key.
struct Request {
    key: u64,
    job: Job,
    /// The root read from disk (its preamble hash is learnt from the render).
    disk_root: Option<PathBuf>,
    device_scale: f64,
}

/// Counters for tests and the performance report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Renders started (TeX runs, a format build included in the first).
    pub renders: u64,
    /// Requests answered from the cache.
    pub cache_hits: u64,
    pub cache_entries: usize,
    pub cache_bytes: usize,
    /// Formats built by this process.
    pub format_builds: u64,
    /// A render is running.
    pub running: bool,
    /// A request waits for the running render.
    pub queued: bool,
}

struct Pipeline {
    previewer: Arc<Previewer>,
    /// A temporary cache directory removed with the window that made it (scripted runs).
    temp_dir: Option<PathBuf>,
    cache: RefCell<Lru<u64, Delivered>>,
    running: Cell<Option<u64>>,
    waiters: RefCell<Vec<(u64, async_channel::Sender<Delivered>)>>,
    queued: RefCell<Option<Request>>,
    /// The preamble hash of roots read from disk, from their last render.
    disk_hashes: RefCell<HashMap<PathBuf, u64>>,
    renders: Cell<u64>,
    hits: Cell<u64>,
    last: RefCell<Option<Outcome>>,
}

thread_local! {
    static PIPELINE: RefCell<Option<Rc<Pipeline>>> = const { RefCell::new(None) };
    static CACHE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static ATTACHED: RefCell<Vec<Rc<Attached>>> = const { RefCell::new(Vec::new()) };
    static DOCS: RefCell<HashMap<PathBuf, (u64, Rc<DocInfo>)>> = RefCell::new(HashMap::new());
}

/// Uses `dir` for the formats from now on (`None`: the default choice, see the module docs),
/// dropping the pipeline and its cache. For tests.
pub fn set_cache_dir(dir: Option<PathBuf>) {
    CACHE_DIR.with_borrow_mut(|d| *d = dir);
    PIPELINE.with_borrow_mut(|p| *p = None);
}

/// True once the first hover over a formula created the pipeline.
pub fn is_loaded() -> bool {
    PIPELINE.with_borrow(Option::is_some)
}

/// The pipeline's counters (all zero before it exists).
pub fn stats() -> Stats {
    PIPELINE.with_borrow(|p| {
        p.as_ref().map_or_else(Stats::default, |p| {
            let cache = p.cache.borrow();
            Stats {
                renders: p.renders.get(),
                cache_hits: p.hits.get(),
                cache_entries: cache.len(),
                cache_bytes: cache.bytes(),
                format_builds: p.previewer.format_builds(),
                running: p.running.get().is_some(),
                queued: p.queued.borrow().is_some(),
            }
        })
    })
}

/// The last render's outcome (timings, route), for tests and the performance report.
pub fn last_outcome() -> Option<Outcome> {
    PIPELINE.with_borrow(|p| p.as_ref().and_then(|p| p.last.borrow().clone()))
}

/// Empties the cache of rendered formulas.
pub fn clear_cache() {
    PIPELINE.with_borrow(|p| {
        if let Some(p) = p {
            p.cache.borrow_mut().clear();
        }
    });
}

/// The pipeline, created for `state`'s window on first use.
fn pipeline(state: &WindowState) -> Rc<Pipeline> {
    if let Some(p) = PIPELINE.with_borrow(Clone::clone) {
        return p;
    }
    let (dir, temporary) = match CACHE_DIR.with_borrow(Clone::clone) {
        Some(dir) => (dir, false),
        None => match &state.paths {
            Some(paths) if !state.scripted => (paths.preview_cache_dir(), false),
            _ => (
                std::env::temp_dir().join(format!("lemma-preview-{}", std::process::id())),
                true,
            ),
        },
    };
    let temp_dir = temporary.then(|| dir.clone());
    if temporary {
        // The temporary formats go with the window (and the pipeline that uses them).
        let dir = dir.clone();
        state.window.connect_destroy(move |_| {
            let ours = PIPELINE.with_borrow(|p| {
                p.as_ref()
                    .is_some_and(|p| p.temp_dir.as_ref() == Some(&dir))
            });
            if ours {
                PIPELINE.with_borrow_mut(|p| *p = None);
                let dir = dir.clone();
                // Off the main thread; a render still running finishes in it first.
                gio::spawn_blocking(move || {
                    let _ = std::fs::remove_dir_all(dir);
                });
            }
        });
    }
    tracing::debug!("math preview: formats in {}", dir.display());
    let p = Rc::new(Pipeline {
        previewer: Arc::new(Previewer::new(dir)),
        temp_dir,
        cache: RefCell::new(Lru::new(64, CACHE_BYTES)),
        running: Cell::new(None),
        waiters: RefCell::new(Vec::new()),
        queued: RefCell::new(None),
        disk_hashes: RefCell::new(HashMap::new()),
        renders: Cell::new(0),
        hits: Cell::new(0),
        last: RefCell::new(None),
    });
    PIPELINE.with_borrow_mut(|slot| *slot = Some(p.clone()));
    p
}

impl Pipeline {
    fn lookup(&self, key: u64, bound: usize) -> Option<Delivered> {
        let mut cache = self.cache.borrow_mut();
        if cache.max_entries() != bound {
            cache.set_bounds(bound, CACHE_BYTES);
        }
        let hit = cache.get(&key).cloned();
        if hit.is_some() {
            self.hits.set(self.hits.get() + 1);
        }
        hit
    }

    /// Resolves to the render of `request` (`None` when a newer request replaced it before it
    /// ran).
    fn request(self: &Rc<Self>, request: Request) -> async_channel::Receiver<Delivered> {
        let (tx, rx) = async_channel::bounded(1);
        self.waiters.borrow_mut().push((request.key, tx));
        match self.running.get() {
            Some(key) if key == request.key => {}
            Some(_) => {
                // Latest wins: the waiting request goes (its waiters get nothing).
                let old = self.queued.replace(Some(request));
                if let Some(old) = old {
                    let queued = self.queued.borrow().as_ref().map(|r| r.key);
                    if Some(old.key) != queued {
                        self.waiters.borrow_mut().retain(|(k, _)| *k != old.key);
                    }
                }
            }
            None => self.start(request),
        }
        rx
    }

    fn start(self: &Rc<Self>, request: Request) {
        self.running.set(Some(request.key));
        self.renders.set(self.renders.get() + 1);
        let previewer = self.previewer.clone();
        let job = request.job.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = gio::spawn_blocking(move || previewer.render(&job)).await;
            if let Some(p) = weak.upgrade() {
                p.finish(request, outcome.ok());
            }
        });
    }

    fn finish(self: &Rc<Self>, request: Request, outcome: Option<Outcome>) {
        self.running.set(None);
        let mut final_key = request.key;
        let delivered: Delivered = match outcome {
            Some(outcome) => {
                if let Some(root) = &request.disk_root {
                    self.disk_hashes
                        .borrow_mut()
                        .insert(root.clone(), outcome.preamble_hash);
                    final_key = cache_key(
                        outcome.preamble_hash,
                        &request.job.definitions,
                        &request.job.kind,
                        &request.job.formula,
                        request.job.scale,
                    );
                }
                match &outcome.result {
                    Ok(_) => tracing::debug!(
                        "math preview: rendered in {:?} (TeX {:?}, format {:?})",
                        outcome.total_time,
                        outcome.tex_time,
                        outcome.format_time
                    ),
                    Err(e) => tracing::debug!("math preview: {e}"),
                }
                let mut outcome = outcome;
                let result = std::mem::replace(
                    &mut outcome.result,
                    Err(PreviewError::Failed("shown".into())),
                );
                self.last.replace(Some(outcome));
                Rc::new(result.map(|image| Rc::new(Picture::new(image, request.device_scale))))
            }
            None => Rc::new(Err(PreviewError::Failed(
                "the preview stopped unexpectedly".into(),
            ))),
        };
        let cacheable = match &*delivered {
            Ok(_) => true,
            Err(e) => e.is_cacheable(),
        };
        if cacheable {
            let bytes = match &*delivered {
                Ok(p) => p.byte_size(),
                Err(_) => 256,
            };
            self.cache
                .borrow_mut()
                .insert(final_key, delivered.clone(), bytes);
        }
        self.deliver(&[request.key, final_key], &delivered);
        let next = self.queued.take();
        if let Some(next) = next {
            let cached = self.cache.borrow_mut().get(&next.key).cloned();
            match cached {
                Some(d) => self.deliver(&[next.key], &d),
                None => self.start(next),
            }
        }
    }

    fn deliver(&self, keys: &[u64], delivered: &Delivered) {
        let waiters: Vec<_> = {
            let mut all = self.waiters.borrow_mut();
            let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(k, _)| keys.contains(k));
            *all = rest;
            mine
        };
        for (_, tx) in waiters {
            let _ = tx.try_send(delivered.clone());
        }
    }
}

// ----- finding the formula and its project -----

/// [`Lines`] over a document.
struct DocLines<'a>(&'a Document);

impl Lines for DocLines<'_> {
    fn line_count(&self) -> u32 {
        self.0.line_index().line_count()
    }

    fn line(&self, i: u32) -> Cow<'_, str> {
        Cow::Owned(self.0.line_text(i).unwrap_or_default())
    }
}

/// The formula at the document byte `offset` of `doc` (see `lemma_preview::find_formula`) and
/// its byte range; `None` outside math and `envs`, or while the document has no scanner states.
pub fn find_at(doc: &Document, offset: usize, envs: &[String]) -> Option<(Range<usize>, Formula)> {
    let states = doc.line_states.borrow();
    let states = states.as_ref()?;
    let (line, col) = doc.line_index().position(offset);
    let formula = find_formula(states, &DocLines(doc), Point::new(line, col), envs)?;
    let index = doc.line_index();
    let start = index.offset(formula.start.line, formula.start.col);
    let end = index.offset(formula.end.line, formula.end.col);
    Some((start..end, formula))
}

/// What a document gives a preview, remembered per version: its preamble (when it is a complete
/// document) and the definitions outside it.
struct DocInfo {
    preamble: Option<Arc<str>>,
    preamble_hash: u64,
    /// Definitions after `\begin{document}` (complete documents) or in the whole text.
    definitions: String,
    complete: bool,
}

fn doc_info(doc: &Document) -> Rc<DocInfo> {
    let version = doc.version();
    let path = doc.path().to_path_buf();
    if let Some(info) = DOCS.with_borrow(|d| {
        d.get(&path)
            .filter(|(v, _)| *v == version)
            .map(|(_, i)| i.clone())
    }) {
        return info;
    }
    // A preamble may load the class from a file of its own (`\\input{preamble}`).
    let info = Rc::new(doc.with_text(|text| match split_preamble(text) {
        Some((preamble, body))
            if preamble.contains("\\documentclass") || preamble.contains("\\input") =>
        {
            DocInfo {
                preamble_hash: preamble_hash(preamble),
                preamble: Some(preamble.into()),
                definitions: definitions(body),
                complete: true,
            }
        }
        _ => DocInfo {
            preamble: None,
            preamble_hash: 0,
            definitions: definitions(text),
            complete: false,
        },
    }));
    DOCS.with_borrow_mut(|d| {
        if d.len() >= DOC_INFOS {
            d.clear();
        }
        d.insert(path, (version, info.clone()));
    });
    info
}

/// The root document of `doc` (see the module docs), or `None`: its `% !TEX root` or subfiles
/// parent, else itself when it is a complete document, else a root of the project index that
/// includes it, else the last build's root when `doc` is in its directory.
pub fn root_for(state: &WindowState, doc: &Document) -> Option<PathBuf> {
    let path = crate::workspace::normalize(doc.path());
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let index = state.workspace.index.borrow();
    let indexed = index.files.get(&path);
    let magic = match indexed {
        Some(file) => file.magic.clone(),
        None => doc.with_text(lemma_syntax::parse_magic),
    };
    if let Some(root) = magic.root.or(magic.subfiles_parent) {
        return Some(tex_path(&dir, &root));
    }
    let complete = indexed.is_some_and(|f| f.has_documentclass && f.has_begin_document);
    if complete || doc_info(doc).complete {
        return Some(path);
    }
    let mut roots: Vec<&PathBuf> = index
        .files
        .iter()
        .filter(|(p, f)| f.has_documentclass && f.has_begin_document && **p != path)
        .map(|(p, _)| p)
        .filter(|root| index.project_files(root).contains(&path))
        .collect();
    roots.sort();
    if let Some(root) = roots.first() {
        return Some((*root).clone());
    }
    drop(index);
    let built = state.build.get().and_then(|b| b.root())?;
    built
        .parent()
        .is_some_and(|d| path.starts_with(d))
        .then_some(built)
}

/// `name` (relative to `dir`, `.tex` implied) as a path.
fn tex_path(dir: &Path, name: &Path) -> PathBuf {
    let mut path = crate::workspace::normalize(&dir.join(name));
    if path.extension().is_none_or(|e| e != "tex") && !path.is_file() {
        let mut s = path.into_os_string();
        s.push(".tex");
        path = PathBuf::from(s);
    }
    path
}

/// Device pixels per bp for `editor`: [`BASE_ZOOM`] scaled by the editor font's size (11 pt is
/// 1) and the display's scale; the second value is the display's scale.
fn scale_for(state: &WindowState, editor: &Editor) -> (f64, f64) {
    let desc = pango::FontDescription::from_string(&state.config.editor.font);
    let size = f64::from(desc.size()) / f64::from(pango::SCALE);
    let points = match size {
        s if s <= 0.0 => 11.0,
        s if desc.is_size_absolute() => s * 0.75,
        s => s,
    };
    let device = editor
        .view()
        .native()
        .and_then(|n| n.surface())
        .map_or_else(|| f64::from(editor.view().scale_factor()), |s| s.scale())
        .max(1.0);
    let zoom = (BASE_ZOOM * points / 11.0).clamp(1.0, 4.0);
    (zoom * device, device)
}

/// The render request for `formula` in `editor`'s document.
fn request_for(
    state: &WindowState,
    editor: &Editor,
    pipeline: &Pipeline,
    formula: &Formula,
) -> Request {
    let doc = editor.document();
    let own = doc_info(doc);
    let dir = doc.path().parent().unwrap_or(Path::new("/")).to_path_buf();
    let root = root_for(state, doc);
    let (scale, device_scale) = scale_for(state, editor);
    let mut defs = String::new();
    let mut disk_root = None;
    let (source, hash) = match &root {
        Some(root) if *root == crate::workspace::normalize(doc.path()) && own.complete => {
            defs.push_str(&own.definitions);
            (
                PreambleSource::Text(own.preamble.clone().expect("a complete document")),
                own.preamble_hash,
            )
        }
        Some(root) => {
            let open = state.workspace.get(root);
            let info = open.as_ref().map(|d| doc_info(d));
            match info.filter(|i| i.complete) {
                Some(info) => {
                    defs.push_str(&info.definitions);
                    defs.push_str(&own.definitions);
                    (
                        PreambleSource::Text(info.preamble.clone().expect("complete")),
                        info.preamble_hash,
                    )
                }
                None if open.is_none() => {
                    defs.push_str(&own.definitions);
                    disk_root = Some(root.clone());
                    // Unknown until a render read it: a key nothing is stored under.
                    let hash = pipeline
                        .disk_hashes
                        .borrow()
                        .get(root)
                        .copied()
                        .unwrap_or_else(|| {
                            lemma_core::fnv1a64(&[
                                b"unread root",
                                root.as_os_str().as_encoded_bytes(),
                            ])
                        });
                    (PreambleSource::Root, hash)
                }
                None => {
                    defs.push_str(&own.definitions);
                    (PreambleSource::Default, preamble_hash(DEFAULT_PREAMBLE))
                }
            }
        }
        None => {
            defs.push_str(&own.definitions);
            (PreambleSource::Default, preamble_hash(DEFAULT_PREAMBLE))
        }
    };
    let job = Job {
        root: if matches!(source, PreambleSource::Default) {
            None
        } else {
            root
        },
        dir,
        preamble: source,
        definitions: defs,
        kind: formula.kind.clone(),
        formula: formula.text.clone(),
        scale,
    };
    let key = cache_key(hash, &job.definitions, &job.kind, &job.formula, job.scale);
    Request {
        key,
        job,
        disk_root,
        device_scale,
    }
}

// ----- the hover -----

fn provide(
    state: &Weak<WindowState>,
    editor: &Weak<Editor>,
    ctx: &HoverCtx,
) -> Option<HoverContent> {
    let state = state.upgrade()?;
    let editor = editor.upgrade()?;
    if !state.config.preview.math_hover {
        return None;
    }
    let (range, formula) = find_at(&ctx.document, ctx.offset, &state.config.preview.envs)?;
    ctx.set_range(range);
    let pipeline = pipeline(&state);
    let request = request_for(&state, &editor, &pipeline, &formula);
    let bound = state.config.preview.cache as usize;
    if let Some(delivered) = pipeline.lookup(request.key, bound) {
        return Some(HoverContent::Widget(content_widget(&delivered, &formula)));
    }
    let rx = pipeline.request(request);
    Some(HoverContent::pending(async move {
        let delivered = rx.recv().await.ok()?;
        Some(HoverContent::Widget(content_widget(&delivered, &formula)))
    }))
}

/// The popover content for a render: the picture, or the error.
fn content_widget(delivered: &Delivered, formula: &Formula) -> gtk::Widget {
    match &**delivered {
        Ok(picture) => {
            let widget = FormulaPicture::new(picture.clone(), &formula.text);
            widget.set_margin_top(4);
            widget.set_margin_bottom(4);
            widget.set_margin_start(4);
            widget.set_margin_end(4);
            widget.upcast()
        }
        Err(error) => error_widget(error).upcast(),
    }
}

/// A short error: an icon and a title, the message, and TeX's context in monospace.
fn error_widget(error: &PreviewError) -> gtk::Box {
    let (title, message, context) = match error {
        PreviewError::Tex {
            message,
            context,
            line,
        } => (
            match line {
                Some(line) if *line > 1 => format!("TeX error on line {line} of the formula"),
                _ => "TeX error".to_owned(),
            },
            message.clone(),
            context.clone(),
        ),
        PreviewError::Timeout(_) => ("Preview timed out".to_owned(), error.to_string(), None),
        PreviewError::Missing(_) => ("Cannot run TeX".to_owned(), error.to_string(), None),
        PreviewError::Failed(_) => ("No preview".to_owned(), error.to_string(), None),
    };
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(2)
        .margin_bottom(2)
        .margin_start(2)
        .margin_end(2)
        .can_focus(false)
        .build();
    column.add_css_class("lemma-math-error");
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    icon.add_css_class("warning");
    header.append(&icon);
    let heading = gtk::Label::builder().label(&title).xalign(0.0).build();
    heading.add_css_class("heading");
    header.append(&heading);
    column.append(&header);
    let label = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .max_width_chars(60)
            .xalign(0.0)
            .can_focus(false)
            .build()
    };
    column.append(&label(&message));
    if let Some(context) = context.filter(|c| !c.is_empty()) {
        let code = label(&context);
        code.add_css_class("monospace");
        code.add_css_class("dim-label");
        column.append(&code);
    }
    column
}

// ----- the formula widget -----

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FormulaPicture {
        pub picture: RefCell<Option<Rc<Picture>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FormulaPicture {
        const NAME: &'static str = "LemmaFormulaPicture";
        type Type = super::FormulaPicture;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("lemma-formula");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for FormulaPicture {}

    impl WidgetImpl for FormulaPicture {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (w, h) = self
                .picture
                .borrow()
                .as_ref()
                .map_or((0.0, 0.0), |p| p.logical_size());
            let size = match orientation {
                gtk::Orientation::Horizontal => w,
                _ => h,
            }
            .ceil() as i32;
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(picture) = self.picture.borrow().clone() else {
                return;
            };
            let widget = self.obj();
            let (w, h) = picture.logical_size();
            // Centred in what the widget got (it may be given more than it asked for).
            let x = ((f64::from(widget.width()) - w) / 2.0).max(0.0).round();
            let y = ((f64::from(widget.height()) - h) / 2.0).max(0.0).round();
            let rect = graphene::Rect::new(x as f32, y as f32, w as f32, h as f32);
            let color = widget.color();
            if picture.mask {
                snapshot.push_mask(gsk::MaskMode::Alpha);
                snapshot.append_texture(&picture.texture, &rect);
                snapshot.pop();
                snapshot.append_color(&color, &rect);
                snapshot.pop();
            } else {
                let dark = luminance(&color) > 0.5;
                if dark {
                    let (matrix, offset) = dark_transform();
                    snapshot.push_color_matrix(
                        &graphene::Matrix::from_float(matrix),
                        &graphene::Vec4::from_float(offset),
                    );
                }
                snapshot.append_texture(&picture.texture, &rect);
                if dark {
                    snapshot.pop();
                }
            }
        }
    }
}

glib::wrapper! {
    /// A rendered formula (see the module docs).
    pub struct FormulaPicture(ObjectSubclass<imp::FormulaPicture>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl FormulaPicture {
    /// Shows `picture`; `source` (the formula's TeX) is its accessible description.
    pub fn new(picture: Rc<Picture>, source: &str) -> Self {
        let widget: Self = glib::Object::builder()
            .property("can-focus", false)
            .property("focusable", false)
            .build();
        widget.update_property(&[gtk::accessible::Property::Label(source)]);
        widget.imp().picture.replace(Some(picture));
        widget.queue_resize();
        widget
    }

    /// The picture shown.
    pub fn picture(&self) -> Option<Rc<Picture>> {
        self.imp().picture.borrow().clone()
    }
}

fn luminance(c: &gdk::RGBA) -> f32 {
    0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue()
}

/// CSS `invert(1) hue-rotate(180deg)` as `push_color_matrix` arguments (GTK computes
/// `transpose(matrix) · pixel + offset` on unpremultiplied RGBA): light and dark swap, hues stay.
fn dark_transform() -> ([f32; 16], [f32; 4]) {
    // hue-rotate(180°), row-major (CSS filter effects); the inversion makes it 1 − H·c.
    let h: [[f32; 3]; 3] = [
        [-0.574, 1.430, 0.144],
        [0.426, 0.430, 0.144],
        [0.426, 1.430, -0.856],
    ];
    let mut m = [0.0f32; 16];
    for (i, row) in h.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            // transpose(m)[i][j] = m[j][i] = −h[i][j]
            m[j * 4 + i] = -v;
        }
    }
    m[15] = 1.0;
    (m, [1.0, 1.0, 1.0, 0.0])
}

// ----- per-editor state -----

struct Attached {
    view: glib::WeakRef<TexView>,
    editor: Weak<Editor>,
    state: Weak<WindowState>,
    provider: Cell<Option<ProviderId>>,
    live: RefCell<Option<Rc<Live>>>,
    /// The live card's widgets, created on its first use and kept (hidden) for the view's
    /// lifetime: GTK 4.22 cannot take an overlay back out of a text view
    /// (`gtk_text_view_remove` only knows anchored children), so the card is reused, never
    /// added twice, and goes with the view.
    card: Rc<RefCell<Option<CardWidgets>>>,
}

/// The widgets of a live card.
#[derive(Clone)]
struct CardWidgets {
    frame: gtk::Box,
    picture: gtk::Box,
    error: gtk::Label,
}

fn attached(view: &TexView) -> Option<Rc<Attached>> {
    ATTACHED.with_borrow(|all| {
        all.iter()
            .find(|a| a.view.upgrade().as_ref() == Some(view))
            .cloned()
    })
}

/// Called once per window after the M2 install hooks: follows `[preview] math_hover` (the
/// provider is registered only while it is on) and `[preview] live` (the live preview's signals
/// are connected only while it is on). Nothing else happens before the first hover.
pub fn install(state: &Rc<WindowState>) {
    state.connect_config_changed(|state, file| {
        if file != ConfigFile::Config {
            return;
        }
        for editor in state.editors() {
            if let Some(a) = attached(editor.view()) {
                sync_provider(state, &editor, &a);
                sync_live(state, &a);
            }
        }
    });
}

/// Called for every new editor tab of a LaTeX document: the hover provider (while `[preview]
/// math_hover`) and the live preview (while `[preview] live`).
pub fn attach(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    if !super::completion::is_latex(editor.document().path()) || attached(editor.view()).is_some() {
        return;
    }
    let a = Rc::new(Attached {
        view: editor.view().downgrade(),
        editor: Rc::downgrade(editor),
        state: Rc::downgrade(state),
        provider: Cell::new(None),
        live: RefCell::new(None),
        card: Rc::default(),
    });
    ATTACHED.with_borrow_mut(|all| {
        all.retain(|a| a.view.upgrade().is_some());
        all.push(a.clone());
    });
    sync_provider(state, editor, &a);
    sync_live(state, &a);
}

/// Registers or unregisters `a`'s hover provider as `[preview] math_hover` says.
fn sync_provider(state: &Rc<WindowState>, editor: &Rc<Editor>, a: &Attached) {
    let on = state.config.preview.math_hover;
    match a.provider.get() {
        Some(id) if !on => {
            hover::unregister(editor.view(), id);
            a.provider.set(None);
        }
        None if on => {
            let (ws, we) = (Rc::downgrade(state), Rc::downgrade(editor));
            let id = hover::register(editor.view(), hover::PRIORITY_MATH, move |ctx| {
                provide(&ws, &we, ctx)
            });
            a.provider.set(id);
        }
        _ => {}
    }
}

/// Called when an editor tab closes (before the hover registry goes).
pub fn detach(_state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(a) = attached(editor.view()) else {
        return;
    };
    ATTACHED.with_borrow_mut(|all| all.retain(|x| !Rc::ptr_eq(x, &a)));
    if let Some(id) = a.provider.take() {
        hover::unregister(editor.view(), id);
    }
    if let Some(live) = a.live.take() {
        live.stop();
    }
}

// ----- the live preview -----

struct Live {
    editor: Weak<Editor>,
    state: Weak<WindowState>,
    /// The view's card widgets ([`Attached::card`]).
    widgets: Rc<RefCell<Option<CardWidgets>>>,
    timer: RefCell<Option<glib::SourceId>>,
    signals: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    /// The Vim context whose mode notifications are connected.
    vim: RefCell<Option<(sourceview5::VimIMContext, glib::SignalHandlerId)>>,
    card: RefCell<Option<LiveCard>>,
    /// Incremented by every new request and by hiding: an older result is dropped.
    generation: Cell<u64>,
}

/// The card below a formula.
struct LiveCard {
    frame: gtk::Box,
    picture: gtk::Box,
    error: gtk::Label,
    /// Reserves the space (in the buffer's tag table while the live preview is on).
    tag: gtk::TextTag,
    /// The formula's start and end (marks follow edits).
    start: gtk::TextMark,
    end: gtk::TextMark,
    /// The line holding the reserved space.
    line: Cell<i32>,
}

/// The live card of `editor` while it is shown (tests).
pub fn live_card(editor: &Editor) -> Option<gtk::Widget> {
    let a = attached(editor.view())?;
    let live = a.live.borrow().clone()?;
    let card = live.card.borrow();
    let card = card.as_ref()?;
    card.frame.is_visible().then(|| card.frame.clone().upcast())
}

/// The picture shown in `editor`'s live card, and its error text (empty when it compiled).
pub fn live_content(editor: &Editor) -> Option<(Option<FormulaPicture>, String)> {
    let a = attached(editor.view())?;
    let live = a.live.borrow().clone()?;
    let card = live.card.borrow();
    let card = card.as_ref()?;
    let picture = card
        .picture
        .first_child()
        .and_then(|w| w.downcast::<FormulaPicture>().ok());
    let error = if card.error.is_visible() {
        card.error.text().to_string()
    } else {
        String::new()
    };
    Some((picture, error))
}

/// The 0-based line whose `pixels-below-lines` holds the live card, and the space reserved.
pub fn live_reserved(editor: &Editor) -> Option<(i32, i32)> {
    let a = attached(editor.view())?;
    let live = a.live.borrow().clone()?;
    let card = live.card.borrow();
    let card = card.as_ref()?;
    card.frame
        .is_visible()
        .then(|| (card.line.get(), card.tag.pixels_below_lines()))
}

/// Connects or disconnects `a`'s live preview as `[preview] live` says.
fn sync_live(state: &Rc<WindowState>, a: &Rc<Attached>) {
    let on = state.config.preview.live;
    let running = a.live.borrow().is_some();
    if on == running {
        return;
    }
    if !on {
        if let Some(live) = a.live.take() {
            live.stop();
        }
        return;
    }
    let Some(editor) = a.editor.upgrade() else {
        return;
    };
    let live = Rc::new(Live {
        editor: Rc::downgrade(&editor),
        state: a.state.clone(),
        widgets: a.card.clone(),
        timer: RefCell::new(None),
        signals: RefCell::new(Vec::new()),
        vim: RefCell::new(None),
        card: RefCell::new(None),
        generation: Cell::new(0),
    });
    let buffer = editor.buffer().clone();
    let weak = Rc::downgrade(&live);
    let changed = buffer.connect_changed(move |_| {
        if let Some(live) = weak.upgrade() {
            live.place();
            live.schedule();
        }
    });
    let weak = Rc::downgrade(&live);
    let moved = buffer.connect_cursor_position_notify(move |_| {
        if let Some(live) = weak.upgrade() {
            if !live.cursor_inside() {
                live.hide();
            }
            live.schedule();
        }
    });
    live.signals.replace(vec![
        (buffer.clone().upcast(), changed),
        (buffer.upcast(), moved),
    ]);
    live.schedule();
    a.live.replace(Some(live));
}

impl Live {
    /// (Re)starts the [`LIVE_DELAY`] timer.
    fn schedule(self: &Rc<Self>) {
        self.watch_vim();
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(LIVE_DELAY, move || {
            if let Some(live) = weak.upgrade() {
                live.timer.take();
                live.update();
            }
        });
        self.timer.replace(Some(timer));
    }

    /// Follows Vim's mode (leaving insert mode hides the card at once).
    fn watch_vim(self: &Rc<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let im = editor.vim();
        let same = match (&*self.vim.borrow(), &im) {
            (Some((old, _)), Some(new)) => old == new,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some((old, id)) = self.vim.take() {
            old.disconnect(id);
        }
        if let Some(im) = im {
            let weak = Rc::downgrade(self);
            let id = im.connect_command_bar_text_notify(move |_| {
                if let Some(live) = weak.upgrade() {
                    if !live.insert_mode() {
                        live.hide();
                    } else {
                        live.schedule();
                    }
                }
            });
            self.vim.replace(Some((im, id)));
        }
    }

    fn insert_mode(&self) -> bool {
        self.editor.upgrade().is_some_and(|e| {
            e.vim_status()
                .is_none_or(|s| matches!(s.mode, VimMode::Insert | VimMode::Replace))
        })
    }

    /// The formula at the cursor (inline, display or a preview environment), with its byte range.
    fn formula_at_cursor(&self) -> Option<(Range<usize>, Formula)> {
        let (editor, state) = (self.editor.upgrade()?, self.state.upgrade()?);
        let doc = editor.document();
        let cursor = editor.cursor_offset();
        let found = find_at(doc, cursor, &state.config.preview.envs).or_else(|| {
            find_at(
                doc,
                hover::char_before(&editor, cursor)?,
                &state.config.preview.envs,
            )
        })?;
        Some(found)
    }

    /// True while the cursor is inside the formula the card shows.
    fn cursor_inside(&self) -> bool {
        let Some(editor) = self.editor.upgrade() else {
            return false;
        };
        let card = self.card.borrow();
        let Some(card) = card.as_ref().filter(|c| c.frame.is_visible()) else {
            return true;
        };
        let buffer = editor.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let (start, end) = (
            buffer.iter_at_mark(&card.start),
            buffer.iter_at_mark(&card.end),
        );
        start <= cursor && cursor <= end
    }

    /// The timer fired: render the formula at the cursor, or hide.
    fn update(self: &Rc<Self>) {
        let (Some(editor), Some(state)) = (self.editor.upgrade(), self.state.upgrade()) else {
            return;
        };
        if !state.config.preview.live || !self.insert_mode() || !editor.view().is_mapped() {
            self.hide();
            return;
        }
        let Some((range, formula)) = self.formula_at_cursor() else {
            self.hide();
            return;
        };
        let pipeline = pipeline(&state);
        let request = request_for(&state, &editor, &pipeline, &formula);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let bound = state.config.preview.cache as usize;
        if let Some(delivered) = pipeline.lookup(request.key, bound) {
            self.show(&range, &formula, &delivered);
            return;
        }
        let rx = pipeline.request(request);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(delivered) = rx.recv().await else {
                return;
            };
            let Some(live) = weak.upgrade() else {
                return;
            };
            if live.generation.get() != generation {
                return;
            }
            // Still the formula at the cursor?
            match live.formula_at_cursor() {
                Some((now, f)) if now.start == range.start && f.text == formula.text => {
                    live.show(&now, &f, &delivered);
                }
                Some(_) => {}
                None => live.hide(),
            }
        });
    }

    /// Shows `delivered` in the card below the formula at `range`.
    fn show(&self, range: &Range<usize>, formula: &Formula, delivered: &Delivered) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let view = editor.view();
        let buffer = editor.buffer();
        let (Some(start), Some(end)) = (editor.iter_near(range.start), editor.iter_near(range.end))
        else {
            return;
        };
        if self.card.borrow().is_none() {
            let existing = self.widgets.borrow().clone();
            let widgets = existing.unwrap_or_else(|| {
                let w = card_widgets();
                view.add_overlay(&w.frame, 0, 0);
                self.widgets.replace(Some(w.clone()));
                w
            });
            // Nothing of an earlier live session.
            while let Some(child) = widgets.picture.first_child() {
                widgets.picture.remove(&child);
            }
            let tag = buffer.tag_table().lookup(LIVE_TAG).unwrap_or_else(|| {
                let tag = gtk::TextTag::new(Some(LIVE_TAG));
                buffer.tag_table().add(&tag);
                tag
            });
            self.card.replace(Some(LiveCard {
                frame: widgets.frame,
                picture: widgets.picture,
                error: widgets.error,
                tag,
                start: buffer.create_mark(None, &start, true),
                end: buffer.create_mark(None, &end, false),
                line: Cell::new(-1),
            }));
        }
        let card = self.card.borrow();
        let card = card.as_ref().expect("created above");
        buffer.move_mark(&card.start, &start);
        buffer.move_mark(&card.end, &end);
        match &**delivered {
            Ok(picture) => {
                while let Some(child) = card.picture.first_child() {
                    card.picture.remove(&child);
                }
                card.picture
                    .append(&FormulaPicture::new(picture.clone(), &formula.text));
                card.picture.set_opacity(1.0);
                card.picture.set_visible(true);
                card.error.set_visible(false);
            }
            Err(e) => {
                let stale = card.picture.first_child().is_some();
                card.picture.set_visible(stale);
                card.picture.set_opacity(0.45);
                card.error.set_text(&e.to_string());
                card.error.set_visible(true);
            }
        }
        card.frame.set_visible(true);
        self.place();
    }

    /// Reserves the card's height below the formula's last line and moves the card there.
    fn place(&self) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let card = self.card.borrow();
        let Some(card) = card.as_ref().filter(|c| c.frame.is_visible()) else {
            return;
        };
        let view = editor.view();
        let buffer = editor.buffer();
        let end = buffer.iter_at_mark(&card.end);
        let line = end.line();
        let (_, natural, _, _) = card.frame.measure(gtk::Orientation::Vertical, -1);
        let reserved = natural + 2 * LIVE_GAP;
        if card.line.get() != line || card.tag.pixels_below_lines() != reserved {
            let (all_start, all_end) = buffer.bounds();
            buffer.remove_tag(&card.tag, &all_start, &all_end);
            let line_start = buffer.iter_at_line(line).unwrap_or(end);
            let mut line_end = line_start;
            if !line_end.ends_line() {
                line_end.forward_to_line_end();
            }
            line_end.forward_char();
            card.tag.set_pixels_below_lines(reserved);
            buffer.apply_tag(&card.tag, &line_start, &line_end);
            card.line.set(line);
        }
        // Below the text of the last display line of the formula.
        let mut last = end;
        if !last.ends_line() {
            last.forward_to_line_end();
        }
        let rect = view.iter_location(&last);
        let x = view.left_margin() + 12;
        view.move_overlay(&card.frame, x, rect.y() + rect.height() + LIVE_GAP);
    }

    /// Hides the card and frees its space.
    fn hide(&self) {
        self.generation.set(self.generation.get() + 1);
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let card = self.card.borrow();
        let Some(card) = card.as_ref() else {
            return;
        };
        if card.frame.is_visible() {
            card.frame.set_visible(false);
            let buffer = editor.buffer();
            let (start, end) = buffer.bounds();
            buffer.remove_tag(&card.tag, &start, &end);
            card.line.set(-1);
        }
    }

    /// Disconnects everything and removes the card, its tag and its marks.
    fn stop(&self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        for (object, id) in self.signals.take() {
            object.disconnect(id);
        }
        if let Some((im, id)) = self.vim.take() {
            im.disconnect(id);
        }
        self.hide();
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        // The card's widgets stay (hidden) in the view; see `Attached::card`.
        if let Some(card) = self.card.take() {
            let buffer = editor.buffer();
            buffer.delete_mark(&card.start);
            buffer.delete_mark(&card.end);
            buffer.tag_table().remove(&card.tag);
        }
    }
}

/// A new live card: a libadwaita card holding the picture, with an error line under it.
fn card_widgets() -> CardWidgets {
    let frame = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .can_focus(false)
        .can_target(false)
        .visible(false)
        .build();
    frame.add_css_class("card");
    frame.add_css_class(LIVE_CSS_CLASS);
    let picture = gtk::Box::builder()
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .build();
    let error = gtk::Label::builder()
        .wrap(true)
        .max_width_chars(70)
        .xalign(0.0)
        .margin_start(10)
        .margin_end(10)
        .margin_bottom(6)
        .visible(false)
        .build();
    error.add_css_class("caption");
    error.add_css_class("error");
    frame.append(&picture);
    frame.append(&error);
    CardWidgets {
        frame,
        picture,
        error,
    }
}

// ----- the demo -----

/// `--demo math-preview` (run with FILE = a document with math, e.g. a copy of
/// `tests/fixtures/proj/chapters/yoneda.tex` in a copy of the project): shows the hover of the
/// formula containing `$LEMMA_DEMO_FORMULA` (else the first `tikzcd`, display or environment
/// math, else the first inline formula) and resolves once the rendered formula is shown.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    use crate::testmode::{wait_for_frames, wait_until};
    let editor = state
        .active_editor()
        .ok_or("--demo math-preview needs a document")?;
    let doc = editor.document().clone();
    wait_until(Duration::from_secs(20), "the scanner states", || {
        doc.has_line_states()
    })
    .await?;
    // The project index names the root a moment after the document opened.
    let started = Instant::now();
    while root_for(&state, &doc).is_none() && started.elapsed() < Duration::from_secs(5) {
        glib::timeout_future(Duration::from_millis(50)).await;
    }
    let text = doc.text();
    let envs = state.config.preview.envs.clone();
    let wanted = std::env::var("LEMMA_DEMO_FORMULA").ok();
    let candidates: Vec<String> = match wanted {
        Some(w) => vec![w],
        None => [
            "\\begin{tikzcd}",
            "\\[",
            "\\begin{align",
            "\\begin{equation",
            "$",
        ]
        .map(String::from)
        .to_vec(),
    };
    let mut found = None;
    'search: for needle in &candidates {
        for (at, _) in text.match_indices(needle.as_str()) {
            // Inside the formula: past the delimiter.
            let inside = (at + needle.len()).min(text.len());
            if let Some((range, formula)) = find_at(&doc, inside, &envs) {
                found = Some((inside, range, formula));
                break 'search;
            }
        }
    }
    let (offset, range, _) = found.ok_or("the document has no formula to preview")?;
    // The formula on screen, with room above it for the popover.
    let view = editor.view().clone();
    let iter = editor.iter_near(range.start).ok_or("no such place")?;
    view.scroll_to_iter(&mut iter.clone(), 0.0, true, 0.0, 0.62);
    editor.place_cursor_at(offset);
    view.grab_focus();
    wait_for_frames(&state.window, 2).await;
    // The first line of the formula with text after the delimiter: the popover points there.
    let point = (range.start..range.end)
        .find(|&o| {
            text.is_char_boundary(o)
                && !matches!(text.as_bytes()[o], b'\\' | b'[' | b'$' | b'\n' | b' ')
                && o > offset.saturating_sub(1)
        })
        .unwrap_or(offset);
    if !hover::show_at(&editor, point, Trigger::Pointer) {
        return Err("no hover at the formula".into());
    }
    wait_until(Duration::from_secs(60), "the rendered formula", || {
        hover::shown_widget(&view)
            .is_some_and(|w| w.is::<FormulaPicture>() || w.has_css_class("lemma-math-error"))
    })
    .await?;
    let shown = hover::shown_widget(&view).ok_or("the popover went away")?;
    if !shown.is::<FormulaPicture>() {
        let mut text = Vec::new();
        let mut child = shown.first_child();
        while let Some(c) = child {
            if let Some(label) = c.downcast_ref::<gtk::Label>() {
                text.push(label.text().to_string());
            }
            child = c.next_sibling();
        }
        return Err(format!("the preview failed: {}", text.join(" ")));
    }
    wait_for_frames(&state.window, 2).await;
    if let Some(o) = last_outcome() {
        println!(
            "math-preview: {:?} route={:?} format_built={} total={:?} tex={:?}",
            o.engine, o.route, o.format_built, o.total_time, o.tex_time
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dark_transform_swaps_black_and_white_and_keeps_hues() {
        let (m, o) = dark_transform();
        // pixel' = transpose(m) · pixel + o
        let apply = |c: [f32; 3]| -> [f32; 3] {
            let mut out = [0.0; 3];
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = (0..3).map(|j| m[j * 4 + i] * c[j]).sum::<f32>() + o[i];
            }
            out
        };
        let close = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01);
        assert!(close(apply([0.0, 0.0, 0.0]), [1.0, 1.0, 1.0]));
        assert!(close(apply([1.0, 1.0, 1.0]), [0.0, 0.0, 0.0]));
        let red = apply([0.8, 0.1, 0.1]);
        assert!(red[0] > red[1] && red[0] > red[2], "red stays red: {red:?}");
        assert_eq!(m[15], 1.0);
    }
}
