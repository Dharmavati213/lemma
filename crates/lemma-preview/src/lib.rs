//! The math hover preview's TeX side (ARCHITECTURE §5 "Math hover preview").
//! GUI-free: it finds the formula at a point ([`find_formula`]), compiles it with the
//! document's preamble and macros, and turns the PDF into pixels ([`Image`]); the app
//! (`lemma-app/src/editor/math_preview.rs`) shows them through the hover registry.
//!
//! # Pipeline
//!
//! [`Previewer::render`] is blocking: the app calls it on its jobs thread, one job at a time.
//!
//! 1. **Preamble.** The root document's preamble (the text before `\begin{document}`; the live
//!    buffer when the app has it, else the file) or, for a document without a root,
//!    [`DEFAULT_PREAMBLE`]. Macros defined outside it (the root's body, the hovered chapter) come
//!    along as [`definitions`].
//! 2. **Format.** For pdfTeX documents, `mylatexformat` dumps the preamble into a format in
//!    `<cache dir>/<fnv(root) as 16 hex>/lemma.fmt`, rebuilt only when the preamble's hash, the
//!    pdfTeX binary or a project file the preamble reads (`\input{macros}`, a local `.sty`)
//!    changed. A preamble that cannot be dumped is remembered as such until it changes.
//! 3. **Render.** `pdflatex -fmt` compiles the formula ([`body`]) in a fresh directory next to
//!    the format, with a [`Previewer::timeout`] of 2 s. Documents for LuaTeX or XeTeX (fontspec,
//!    unicode-math, `% !TEX program`), and pdfTeX documents without a format, compile the whole
//!    preamble each time with their engine ([`Previewer::full_timeout`]).
//! 4. **Pixels.** poppler renders the page; the ink is cropped; black and grey ink becomes an
//!    alpha mask that the app paints in the current text colour (readable in dark mode); colour
//!    is kept as colour.
//!
//! A TeX error inside the formula is the result ([`PreviewError::Tex`], with the formula's own
//! line); errors before it (a definition that already exists, a package's begin-document hook)
//! are ignored when a page came out. Nothing here keeps a TeX process alive between renders,
//! and nothing runs before the first [`Previewer::render`]. TeX never gets shell escape.
//!
//! The in-memory cache of rendered formulas is the app's ([`Lru`], keyed by [`cache_key`]).
//! Tests pass a temporary cache directory, never the user's.

mod body;
mod defs;
mod formula;
mod lru;
mod raster;
mod run;

pub use body::{Body, DEFAULT_PREAMBLE, Route, body, format_source, split_preamble};
pub use defs::{MAX_DEFINITIONS, definitions};
pub use formula::{Formula, FormulaKind, MAX_BYTES, MAX_LINES, Point, find_formula};
pub use lru::Lru;
pub use raster::{Image, Pixels, rasterize};

use lemma_build::{Engine, parse_log};
use lemma_core::{Severity, fnv1a64};
use run::RunError;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// The timeout of a render with the format (ARCHITECTURE §5: 2 s).
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(2);
/// The timeout of a render that compiles the whole preamble (LuaTeX with fontspec takes a while).
pub const FULL_TIMEOUT: Duration = Duration::from_secs(10);
/// The timeout of a format build (a heavy preamble: TikZ, pgfplots, biblatex).
pub const FORMAT_TIMEOUT: Duration = Duration::from_secs(30);
/// Project directories kept in the cache directory (the least recently used go).
pub const MAX_PROJECTS: usize = 16;
/// Ink margin kept around a formula, in bp.
const MARGIN_BP: f64 = 1.5;
/// Largest root file read for its preamble.
const MAX_ROOT_BYTES: u64 = 4 << 20;
/// Name of the preview file (and so of its PDF and log).
const JOB: &str = "lemma-preview";
/// Name of the format file in a project directory.
const FORMAT_FILE: &str = "lemma.fmt";
/// The file a format is built from (a name no project uses: the build directory is searched
/// first, so a user's `\input{preamble}` must not find it).
const FORMAT_SOURCE: &str = "lemma-preview-preamble.tex";
const STAMP_FILE: &str = "stamp";
/// Version of the stamp and of the wrapper TeX: a change rebuilds every format.
const STAMP_VERSION: &str = "lemma-preview-format 1";

/// Where the preamble comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreambleSource {
    /// The root's preamble as the app has it (the live buffer; see [`split_preamble`]).
    Text(Arc<str>),
    /// Read [`Job::root`] from disk (it is not open), and take its body's definitions too.
    Root,
    /// [`DEFAULT_PREAMBLE`] (no root document).
    Default,
}

/// One formula to render.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The root document; `None` when the hovered document has none.
    pub root: Option<PathBuf>,
    /// The hovered document's directory (searched for `\input` files when there is no root).
    pub dir: PathBuf,
    pub preamble: PreambleSource,
    /// Definitions from outside the preamble ([`definitions`]).
    pub definitions: String,
    pub kind: FormulaKind,
    /// The formula's source, delimiters included ([`Formula::text`]).
    pub formula: String,
    /// Device pixels per PDF point (bp).
    pub scale: f64,
}

impl Job {
    /// The directory TeX searches for the project's own files.
    fn project_dir(&self) -> &Path {
        self.root
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(&self.dir)
    }
}

/// Why there is no image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewError {
    /// TeX reported an error in the formula (or, when no page came out, anywhere).
    Tex {
        message: String,
        /// TeX's context (the line up to the error), when it gave one.
        context: Option<String>,
        /// The 1-based line of the formula, when the error is in it.
        line: Option<u32>,
    },
    /// TeX ran longer than its timeout and was stopped.
    Timeout(Duration),
    /// The TeX program could not be started.
    Missing(String),
    /// Anything else: no page, a page without ink, an unreadable PDF, a file error.
    Failed(String),
}

impl PreviewError {
    /// True for errors that the same input gives again (worth caching).
    pub fn is_cacheable(&self) -> bool {
        matches!(self, PreviewError::Tex { .. })
    }
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreviewError::Tex {
                message,
                context,
                line,
            } => {
                write!(f, "{message}")?;
                if let Some(line) = line {
                    write!(f, " (line {line} of the formula)")?;
                }
                if let Some(context) = context {
                    write!(f, ": {context}")?;
                }
                Ok(())
            }
            PreviewError::Timeout(d) => write!(f, "TeX took longer than {} s", d.as_secs_f64()),
            PreviewError::Missing(what) => write!(f, "cannot run {what}"),
            PreviewError::Failed(why) => f.write_str(why),
        }
    }
}

/// What a render did and gave.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub result: Result<Image, PreviewError>,
    /// [`preamble_hash`] of the preamble used (for [`cache_key`] when the root was read from
    /// disk).
    pub preamble_hash: u64,
    /// How the formula was compiled (`None` when nothing ran).
    pub route: Option<Route>,
    pub engine: Engine,
    /// The format was (re)built for this render.
    pub format_built: bool,
    /// Time spent building the format.
    pub format_time: Duration,
    /// Time spent in the TeX run of the formula.
    pub tex_time: Duration,
    /// Time spent rasterizing.
    pub raster_time: Duration,
    pub total_time: Duration,
}

/// The hash of a preamble's text (a [`cache_key`] part).
pub fn preamble_hash(preamble: &str) -> u64 {
    fnv1a64(&[STAMP_VERSION.as_bytes(), preamble.as_bytes()])
}

/// The in-memory cache key of a rendered formula: everything that changes the picture.
pub fn cache_key(
    preamble_hash: u64,
    definitions: &str,
    kind: &FormulaKind,
    formula: &str,
    scale: f64,
) -> u64 {
    let kind = match kind {
        FormulaKind::Inline => "i".to_owned(),
        FormulaKind::Display => "d".to_owned(),
        FormulaKind::Environment(name) => format!("e{name}"),
    };
    // Scales closer than 1/64 px per bp give the same picture.
    let scale = ((scale * 64.0).round() as u64).to_le_bytes();
    fnv1a64(&[
        &preamble_hash.to_le_bytes(),
        definitions.as_bytes(),
        kind.as_bytes(),
        formula.as_bytes(),
        &scale,
    ])
}

/// The name of a project's directory in the cache directory: the root's FNV-1a hash as 16 hex
/// digits (a fixed name for documents without a root).
pub fn project_key(root: Option<&Path>) -> String {
    let hash = match root {
        Some(root) => fnv1a64(&[root.as_os_str().as_encoded_bytes()]),
        None => fnv1a64(&[b"lemma-preview: no root document"]),
    };
    format!("{hash:016x}")
}

/// Renders formulas (see the crate docs). Cheap to create; holds no process or file open.
#[derive(Debug)]
pub struct Previewer {
    cache_dir: PathBuf,
    /// Timeout of a render with the format.
    pub timeout: Duration,
    /// Timeout of a render that compiles the whole preamble.
    pub full_timeout: Duration,
    /// Timeout of a format build.
    pub format_timeout: Duration,
    /// The programs run (names looked up in `PATH`, or paths).
    pub pdflatex: OsString,
    pub lualatex: OsString,
    pub xelatex: OsString,
    counter: AtomicU64,
    format_builds: AtomicU64,
    tex_runs: AtomicU64,
    engine_id: OnceLock<u64>,
}

impl Previewer {
    /// A previewer keeping its formats in `cache_dir` (`Paths::preview_cache_dir()` in the
    /// app; created on the first render).
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            timeout: RENDER_TIMEOUT,
            full_timeout: FULL_TIMEOUT,
            format_timeout: FORMAT_TIMEOUT,
            pdflatex: "pdflatex".into(),
            lualatex: "lualatex".into(),
            xelatex: "xelatex".into(),
            counter: AtomicU64::new(0),
            format_builds: AtomicU64::new(0),
            tex_runs: AtomicU64::new(0),
            engine_id: OnceLock::new(),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The directory of `root`'s format and runs.
    pub fn project_dir(&self, root: Option<&Path>) -> PathBuf {
        self.cache_dir.join(project_key(root))
    }

    /// Formats built so far (by this previewer).
    pub fn format_builds(&self) -> u64 {
        self.format_builds.load(Ordering::Relaxed)
    }

    /// TeX runs of formulas so far (format builds not counted).
    pub fn tex_runs(&self) -> u64 {
        self.tex_runs.load(Ordering::Relaxed)
    }

    /// Renders `job` (blocking; see the crate docs).
    pub fn render(&self, job: &Job) -> Outcome {
        let started = Instant::now();
        let (preamble, definitions) = self.preamble_of(job);
        let hash = preamble_hash(&preamble);
        let engine = engine_for(&preamble, job.project_dir());
        let mut outcome = Outcome {
            result: Err(PreviewError::Failed("not rendered".into())),
            preamble_hash: hash,
            route: None,
            engine,
            format_built: false,
            format_time: Duration::ZERO,
            tex_time: Duration::ZERO,
            raster_time: Duration::ZERO,
            total_time: Duration::ZERO,
        };
        let project = self.project_dir(job.root.as_deref());
        if let Err(e) = fs::create_dir_all(&project) {
            outcome.result = Err(PreviewError::Failed(format!(
                "cannot create {}: {e}",
                project.display()
            )));
            return outcome;
        }
        outcome.result = self.render_in(job, &project, &preamble, &definitions, &mut outcome);
        outcome.total_time = started.elapsed();
        outcome
    }

    fn render_in(
        &self,
        job: &Job,
        project: &Path,
        preamble: &str,
        definitions: &str,
        outcome: &mut Outcome,
    ) -> Result<Image, PreviewError> {
        let mut retried = false;
        loop {
            let format = if outcome.engine == Engine::PdfLatex {
                let t = Instant::now();
                let (format, built) =
                    self.ensure_format(project, job.project_dir(), preamble, outcome.preamble_hash);
                if built {
                    outcome.format_built = true;
                    outcome.format_time += t.elapsed();
                }
                format
            } else {
                None
            };
            let route = if format.is_some() {
                Route::Format
            } else {
                Route::Full
            };
            outcome.route = Some(route);
            let program = match (route, outcome.engine) {
                (Route::Format, _) | (Route::Full, Engine::PdfLatex | Engine::Latexmkrc) => {
                    &self.pdflatex
                }
                (Route::Full, Engine::LuaLatex) => &self.lualatex,
                (Route::Full, Engine::XeLatex) => &self.xelatex,
            };
            let source = body(route, preamble, definitions, &job.kind, &job.formula);
            let t = Instant::now();
            let run = self.run_tex(project, program, format.as_deref(), &source, job, route);
            outcome.tex_time += t.elapsed();
            match run {
                // An old format pdfTeX no longer reads (an update): build it again, once.
                Err(Retry::Format) if !retried => {
                    retried = true;
                    let _ = fs::remove_file(project.join(STAMP_FILE));
                }
                Err(Retry::Format) => {
                    return Err(PreviewError::Failed(
                        "pdfTeX cannot read the preview format".into(),
                    ));
                }
                Err(Retry::Error(e)) => return Err(e),
                Ok(pdf) => {
                    let t = Instant::now();
                    let image = rasterize(pdf, job.scale, MARGIN_BP).map_err(PreviewError::Failed);
                    outcome.raster_time = t.elapsed();
                    return image;
                }
            }
        }
    }

    /// The preamble text and every definition to add.
    fn preamble_of(&self, job: &Job) -> (String, String) {
        match &job.preamble {
            PreambleSource::Text(text) => (text.to_string(), job.definitions.clone()),
            PreambleSource::Default => (DEFAULT_PREAMBLE.to_owned(), job.definitions.clone()),
            PreambleSource::Root => {
                let text = job.root.as_deref().and_then(read_root);
                match text.as_deref().and_then(split_preamble) {
                    Some((preamble, rest)) => {
                        let mut defs = definitions(rest);
                        defs.push_str(&job.definitions);
                        (preamble.to_owned(), defs)
                    }
                    None => (DEFAULT_PREAMBLE.to_owned(), job.definitions.clone()),
                }
            }
        }
    }

    /// Compiles `source` in a fresh directory of `project` and returns the PDF.
    fn run_tex(
        &self,
        project: &Path,
        program: &OsStr,
        format: Option<&Path>,
        source: &Body,
        job: &Job,
        route: Route,
    ) -> Result<Vec<u8>, Retry> {
        let dir = project.join(format!(
            "run-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.run_tex_in(&dir, program, format, source, job, route);
        let _ = fs::remove_dir_all(&dir);
        result
    }

    fn run_tex_in(
        &self,
        dir: &Path,
        program: &OsStr,
        format: Option<&Path>,
        source: &Body,
        job: &Job,
        route: Route,
    ) -> Result<Vec<u8>, Retry> {
        let failed = |what: &str, e: io::Error| {
            Retry::Error(PreviewError::Failed(format!("cannot {what}: {e}")))
        };
        fs::create_dir_all(dir).map_err(|e| failed("create the run directory", e))?;
        let tex = dir.join(format!("{JOB}.tex"));
        fs::write(&tex, &source.text).map_err(|e| failed("write the preview file", e))?;
        let mut args: Vec<OsString> = Vec::new();
        if let Some(format) = format {
            let mut arg = OsString::from("-fmt=");
            arg.push(format);
            args.push(arg);
        }
        args.extend(
            [
                "-interaction=nonstopmode",
                "-no-shell-escape",
                "-file-line-error",
            ]
            .map(OsString::from),
        );
        args.push(format!("{JOB}.tex").into());
        let texinputs = run::texinputs(
            Some(job.project_dir()),
            std::env::var_os("TEXINPUTS").as_deref(),
        );
        let timeout = match route {
            Route::Format => self.timeout,
            Route::Full => self.full_timeout,
        };
        self.tex_runs.fetch_add(1, Ordering::Relaxed);
        let status = run::run(program, &args, dir, &texinputs, timeout);
        let log = fs::read(dir.join(format!("{JOB}.log")))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let pdf = fs::read(dir.join(format!("{JOB}.pdf"))).ok();
        match status {
            Err(RunError::Spawn(e)) => {
                return Err(Retry::Error(PreviewError::Missing(format!(
                    "{}: {e}",
                    program.to_string_lossy()
                ))));
            }
            Err(RunError::Timeout) => return Err(Retry::Error(PreviewError::Timeout(timeout))),
            Ok(_) => {}
        }
        if route == Route::Format
            && pdf.is_none()
            && (log.is_empty() || log.contains("format file"))
        {
            return Err(Retry::Format);
        }
        if let Some(error) = formula_error(&log, dir, source.formula_line) {
            return Err(Retry::Error(error));
        }
        match pdf {
            Some(pdf) if !pdf.is_empty() => Ok(pdf),
            _ => Err(Retry::Error(first_error(&log, dir).unwrap_or_else(|| {
                PreviewError::Failed("TeX produced no page".into())
            }))),
        }
    }

    // ----- formats -----

    /// The format for `preamble` in `project`, built when missing or stale; `None` when it
    /// cannot be built. The second value is true when a build ran.
    fn ensure_format(
        &self,
        project: &Path,
        project_dir: &Path,
        preamble: &str,
        hash: u64,
    ) -> (Option<PathBuf>, bool) {
        let fmt = project.join(FORMAT_FILE);
        let engine = self.engine_id();
        match read_stamp(&project.join(STAMP_FILE)) {
            Some(stamp) if stamp.is_current(hash, engine) => {
                return (stamp.ok.then_some(fmt).filter(|f| f.is_file()), false);
            }
            _ => {}
        }
        self.format_builds.fetch_add(1, Ordering::Relaxed);
        prune(&self.cache_dir, project);
        let built = self.build_format(project, project_dir, preamble);
        let (ok, deps) = match built {
            Some(deps) => (true, deps),
            None => (false, Vec::new()),
        };
        let stamp = Stamp {
            preamble: hash,
            engine,
            ok,
            deps,
        };
        write_atomically(&project.join(STAMP_FILE), stamp.to_string().as_bytes());
        (ok.then_some(fmt), true)
    }

    /// Runs mylatexformat on `preamble` in a fresh directory and moves the format into place;
    /// the project files it read (with their modification times), or `None` when it failed.
    fn build_format(&self, project: &Path, project_dir: &Path, preamble: &str) -> Option<Vec<Dep>> {
        let dir = project.join(format!(
            "build-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            fs::create_dir_all(&dir).ok()?;
            fs::write(dir.join(FORMAT_SOURCE), format_source(preamble)).ok()?;
            let args: Vec<OsString> = [
                "-ini",
                "-interaction=nonstopmode",
                "-no-shell-escape",
                "-recorder",
                "-jobname=lemma-fmt",
                "&pdflatex",
                "mylatexformat.ltx",
                FORMAT_SOURCE,
            ]
            .map(OsString::from)
            .to_vec();
            let texinputs =
                run::texinputs(Some(project_dir), std::env::var_os("TEXINPUTS").as_deref());
            let status = run::run(&self.pdflatex, &args, &dir, &texinputs, self.format_timeout);
            if let Err(e) = &status {
                tracing::warn!("math preview: building the format failed: {e:?}");
            }
            let built = dir.join("lemma-fmt.fmt");
            if status.is_err() || !built.is_file() {
                let log = fs::read_to_string(dir.join("lemma-fmt.log")).unwrap_or_default();
                let first = log
                    .lines()
                    .find(|l| l.starts_with('!'))
                    .unwrap_or("no format");
                tracing::info!("math preview: the preamble cannot be dumped ({first})");
                return None;
            }
            let deps = fs::read_to_string(dir.join("lemma-fmt.fls"))
                .map(|fls| dependencies(&fls))
                .unwrap_or_default();
            fs::rename(&built, project.join(FORMAT_FILE)).ok()?;
            Some(deps)
        })();
        let _ = fs::remove_dir_all(&dir);
        result
    }

    /// Identifies the pdfTeX binary (its path, size and modification time): a format only works
    /// with the binary that dumped it.
    fn engine_id(&self) -> u64 {
        *self.engine_id.get_or_init(|| {
            let Some(path) = find_program(&self.pdflatex) else {
                return 0;
            };
            let real = fs::canonicalize(&path).unwrap_or(path);
            let meta = fs::metadata(&real).ok();
            let (size, mtime) = meta.map_or((0, 0), |m| (m.len(), mtime_ns(&m)));
            fnv1a64(&[
                real.as_os_str().as_encoded_bytes(),
                &size.to_le_bytes(),
                &mtime.to_le_bytes(),
            ])
        })
    }
}

/// What a TeX run asks for instead of a PDF.
enum Retry {
    /// pdfTeX could not load the format.
    Format,
    Error(PreviewError),
}

/// The engine for a preamble: `% !TEX program`, then fontspec / unicode-math / polyglossia
/// (LuaTeX), else pdfTeX. A latexmkrc does not decide here (the preview runs the engine itself).
fn engine_for(preamble: &str, dir: &Path) -> Engine {
    match lemma_build::detect_engine(preamble, dir) {
        Engine::Latexmkrc => lemma_build::detect_engine(preamble, Path::new("/nonexistent")),
        engine => engine,
    }
}

/// The root document's text (lossy UTF-8, its first [`MAX_ROOT_BYTES`]).
fn read_root(root: &Path) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(root).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_ROOT_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The first error of the log at or after line `formula_line` of the preview file.
fn formula_error(log: &str, dir: &Path, formula_line: u32) -> Option<PreviewError> {
    let report = parse_log(log, dir, &|p| p.exists());
    report
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && is_preview_file(&d.path))
        .find_map(|d| {
            let line = d.line.filter(|&l| l >= formula_line)?;
            Some(tex_error(
                &d.message,
                d.context.as_deref(),
                Some(line - formula_line + 1),
                formula_line,
            ))
        })
}

/// The first error of the log, wherever it is.
fn first_error(log: &str, dir: &Path) -> Option<PreviewError> {
    let report = parse_log(log, dir, &|p| p.exists());
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)?;
    Some(tex_error(&d.message, d.context.as_deref(), None, u32::MAX))
}

fn is_preview_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == OsStr::new(&format!("{JOB}.tex")))
}

/// A TeX error as shown: the message without TeX's trailing period and with "on input line
/// <n>" counted in the formula's lines (from `formula_line` of the preview file), and the context
/// without its `l.<n>` prefix (the preview file's line numbers mean nothing to the user).
fn tex_error(
    message: &str,
    context: Option<&str>,
    line: Option<u32>,
    formula_line: u32,
) -> PreviewError {
    let mut message = message.trim().trim_end_matches('.').to_owned();
    const INPUT_LINE: &str = "on input line ";
    if let Some(at) = message.find(INPUT_LINE) {
        let digits: String = message[at + INPUT_LINE.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse::<u32>()
            && n >= formula_line
        {
            let end = at + INPUT_LINE.len() + digits.len();
            message.replace_range(at..end, &format!("on line {}", n - formula_line + 1));
        }
    }
    let context = context
        .map(|c| {
            let c = c.trim();
            match c.strip_prefix("l.") {
                Some(rest) => rest
                    .trim_start_matches(|ch: char| ch.is_ascii_digit())
                    .trim()
                    .to_owned(),
                None => c.to_owned(),
            }
        })
        .filter(|c| !c.is_empty());
    PreviewError::Tex {
        message,
        context,
        line,
    }
}

/// A project file a format depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dep {
    path: PathBuf,
    size: u64,
    mtime: u128,
}

impl Dep {
    fn of(path: &Path) -> Option<Dep> {
        let meta = fs::metadata(path).ok()?;
        Some(Dep {
            path: path.to_path_buf(),
            size: meta.len(),
            mtime: mtime_ns(&meta),
        })
    }

    fn is_current(&self) -> bool {
        Dep::of(&self.path).is_some_and(|now| now == *self)
    }
}

fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// The files a format build read (the `INPUT` lines of its `.fls`) that are not part of the TeX
/// distribution: the project's macro files and the user's own packages.
fn dependencies(fls: &str) -> Vec<Dep> {
    let mut seen = std::collections::BTreeSet::new();
    fls.lines()
        .filter_map(|l| l.strip_prefix("INPUT "))
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .filter(|p| {
            let s = p.to_string_lossy();
            !(s.starts_with("/usr/")
                || s.starts_with("/etc/")
                || s.starts_with("/var/")
                || s.starts_with("/opt/")
                || s.contains("/texmf-dist/")
                || s.contains("/texmf-var/")
                || s.contains("/.texlive")
                || s.contains("/texlive/"))
        })
        .filter(|p| seen.insert(p.to_path_buf()))
        .filter_map(Dep::of)
        .collect()
}

/// What a project directory's format was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    preamble: u64,
    engine: u64,
    /// The format was built (false: the preamble cannot be dumped).
    ok: bool,
    deps: Vec<Dep>,
}

impl Stamp {
    fn is_current(&self, preamble: u64, engine: u64) -> bool {
        self.preamble == preamble && self.engine == engine && self.deps.iter().all(Dep::is_current)
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{STAMP_VERSION}")?;
        writeln!(f, "preamble {:016x}", self.preamble)?;
        writeln!(f, "engine {:016x}", self.engine)?;
        writeln!(f, "status {}", if self.ok { "ok" } else { "failed" })?;
        for dep in &self.deps {
            let path = dep.path.to_string_lossy();
            if !path.contains('\n') {
                writeln!(f, "dep {} {} {path}", dep.mtime, dep.size)?;
            }
        }
        Ok(())
    }
}

fn read_stamp(path: &Path) -> Option<Stamp> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != STAMP_VERSION {
        return None;
    }
    let mut stamp = Stamp {
        preamble: 0,
        engine: 0,
        ok: false,
        deps: Vec::new(),
    };
    for line in lines {
        let (key, value) = line.split_once(' ')?;
        match key {
            "preamble" => stamp.preamble = u64::from_str_radix(value, 16).ok()?,
            "engine" => stamp.engine = u64::from_str_radix(value, 16).ok()?,
            "status" => stamp.ok = value == "ok",
            "dep" => {
                let mut parts = value.splitn(3, ' ');
                let mtime = parts.next()?.parse().ok()?;
                let size = parts.next()?.parse().ok()?;
                let path = PathBuf::from(parts.next()?);
                stamp.deps.push(Dep { path, size, mtime });
            }
            _ => return None,
        }
    }
    Some(stamp)
}

/// Writes `path` through a temporary file and a rename (a reader never sees half of it).
fn write_atomically(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&tmp, bytes).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Keeps the cache directory small: at most [`MAX_PROJECTS`] project directories (the least
/// recently stamped go; `keep` stays), and no leftover run or build directory of a crashed
/// process older than ten minutes in `keep`.
fn prune(cache_dir: &Path, keep: &Path) {
    let stale = SystemTime::now() - Duration::from_secs(600);
    if let Ok(entries) = fs::read_dir(keep) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < stale);
            if (name.starts_with("run-") || name.starts_with("build-")) && old {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut projects: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(|e| e.path())
        .filter(|p| p != keep)
        .map(|p| {
            let used = fs::metadata(p.join(STAMP_FILE))
                .or_else(|_| fs::metadata(&p))
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (used, p)
        })
        .collect();
    if projects.len() < MAX_PROJECTS {
        return;
    }
    projects.sort();
    let excess = projects.len() + 1 - MAX_PROJECTS;
    for (_, path) in projects.into_iter().take(excess) {
        let _ = fs::remove_dir_all(path);
    }
}

/// A temporary directory for unit tests, under the repository's `target/` (not `/tmp`).
#[cfg(test)]
pub(crate) fn test_dir() -> tempfile::TempDir {
    let base = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/lemma-preview-tests"
    );
    fs::create_dir_all(base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

/// `program` as a path: itself when it has a slash, else the first match in `PATH`.
fn find_program(program: &OsStr) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        let dir = crate::test_dir();
        let file = dir.path().join("macros.tex");
        fs::write(&file, "x").unwrap();
        let stamp = Stamp {
            preamble: 0xfeed,
            engine: 7,
            ok: true,
            deps: vec![Dep::of(&file).unwrap()],
        };
        let path = dir.path().join("stamp");
        write_atomically(&path, stamp.to_string().as_bytes());
        let read = read_stamp(&path).unwrap();
        assert_eq!(read, stamp);
        assert!(read.is_current(0xfeed, 7));
        assert!(!read.is_current(0xfeee, 7));
        assert!(!read.is_current(0xfeed, 8));
        fs::write(&file, "changed").unwrap();
        assert!(!read.is_current(0xfeed, 7), "a dependency changed");
        fs::write(&path, "something else\n").unwrap();
        assert_eq!(read_stamp(&path), None);
    }

    #[test]
    fn dependencies_skip_the_distribution() {
        let dir = crate::test_dir();
        let mine = dir.path().join("defs.sty");
        fs::write(&mine, "").unwrap();
        let fls = format!(
            "PWD /x\nINPUT /usr/share/texmf-dist/tex/latex/base/article.cls\nINPUT ./lemma-preview-preamble.tex\n\
INPUT {0}\nINPUT {0}\nINPUT /home/u/.texlive2026/texmf-var/web2c/x\nOUTPUT lemma-fmt.fmt\n",
            mine.display()
        );
        let deps = dependencies(&fls);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].path, mine);
    }

    #[test]
    fn tex_errors_lose_line_prefixes() {
        assert_eq!(
            tex_error(
                "\\begin{align*} on input line 16 ended by \\end{matrix}.",
                None,
                Some(3),
                15
            )
            .to_string(),
            "\\begin{align*} on line 2 ended by \\end{matrix} (line 3 of the formula)"
        );
        assert_eq!(
            tex_error(
                "Undefined control sequence.",
                Some("l.12 $\\alpha\\foo"),
                Some(1),
                12
            ),
            PreviewError::Tex {
                message: "Undefined control sequence".into(),
                context: Some("$\\alpha\\foo".into()),
                line: Some(1)
            }
        );
        let e = tex_error("Missing $ inserted.", None, None, 1);
        assert_eq!(e.to_string(), "Missing $ inserted");
        assert!(e.is_cacheable());
        assert!(!PreviewError::Timeout(RENDER_TIMEOUT).is_cacheable());
        assert_eq!(
            PreviewError::Timeout(RENDER_TIMEOUT).to_string(),
            "TeX took longer than 2 s"
        );
    }

    #[test]
    fn engines_follow_the_preamble() {
        let dir = crate::test_dir();
        assert_eq!(
            engine_for("\\documentclass{article}", dir.path()),
            Engine::PdfLatex
        );
        assert_eq!(
            engine_for("\\documentclass{article}\\usepackage{fontspec}", dir.path()),
            Engine::LuaLatex
        );
        assert_eq!(
            engine_for(
                "% !TEX program = xelatex\n\\documentclass{article}",
                dir.path()
            ),
            Engine::XeLatex
        );
        fs::write(dir.path().join("latexmkrc"), "$pdf_mode = 1;").unwrap();
        assert_eq!(
            engine_for("\\documentclass{article}", dir.path()),
            Engine::PdfLatex
        );
    }

    #[test]
    fn keys_change_with_every_part() {
        let k = |p, d: &str, kind: &FormulaKind, f: &str, s| cache_key(p, d, kind, f, s);
        let base = k(1, "", &FormulaKind::Inline, "$x$", 2.0);
        assert_eq!(base, k(1, "", &FormulaKind::Inline, "$x$", 2.001));
        for other in [
            k(2, "", &FormulaKind::Inline, "$x$", 2.0),
            k(1, "\\def\\a{}", &FormulaKind::Inline, "$x$", 2.0),
            k(1, "", &FormulaKind::Display, "$x$", 2.0),
            k(1, "", &FormulaKind::Inline, "$y$", 2.0),
            k(1, "", &FormulaKind::Inline, "$x$", 3.0),
        ] {
            assert_ne!(base, other);
        }
        assert_ne!(
            project_key(Some(Path::new("/a/main.tex"))),
            project_key(None)
        );
        assert_eq!(project_key(None).len(), 16);
    }

    #[test]
    fn prune_keeps_the_newest_projects() {
        let cache = crate::test_dir();
        let mut dirs = Vec::new();
        for i in 0..MAX_PROJECTS + 3 {
            let d = cache.path().join(format!("{i:016x}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(STAMP_FILE), "x").unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i as u64);
            fs::File::options()
                .write(true)
                .open(d.join(STAMP_FILE))
                .unwrap()
                .set_modified(t)
                .unwrap();
            dirs.push(d);
        }
        let other = cache.path().join("not-a-project");
        fs::create_dir_all(&other).unwrap();
        let keep = dirs[0].clone();
        let leftover = keep.join("run-1-1");
        fs::create_dir_all(&leftover).unwrap();
        fs::File::open(&leftover)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        let fresh = keep.join("run-2-2");
        fs::create_dir_all(&fresh).unwrap();
        prune(cache.path(), &keep);
        let left: Vec<bool> = dirs.iter().map(|d| d.exists()).collect();
        assert!(left[0], "the project in use stays");
        assert_eq!(left.iter().filter(|x| **x).count(), MAX_PROJECTS);
        assert!(!left[1] && !left[2] && !left[3] && left[4], "{left:?}");
        assert!(other.exists());
        assert!(!leftover.exists() && fresh.exists());
    }
}
