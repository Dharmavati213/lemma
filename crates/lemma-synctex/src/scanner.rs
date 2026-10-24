use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::fmt;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::{Mutex, PoisonError};

use crate::inputs::{Input, c_path, common_suffix_len, normalize};
use crate::{SyncBox, SyncTarget, ffi};

/// Serializes every call into libsynctex.
///
/// The library is not documented as thread-safe and writes node descriptions into static
/// buffers, so two scanners on different threads must never run library code at the same time.
/// The lock is uncontended in practice (one PDF thread) and is held only around FFI calls.
static LIBRARY: Mutex<()> = Mutex::new(());

fn with_library<R>(f: impl FnOnce() -> R) -> R {
    let _guard = LIBRARY.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// A parsed synctex file: forward (source → PDF) and inverse (PDF → source) search.
///
/// Opening parses the whole file (about 8 ms for 5 pages, 15 ms for 23); a query then takes
/// tens of microseconds. Reopen the scanner whenever the `.synctex.gz` changes.
pub struct Scanner {
    raw: NonNull<ffi::synctex_scanner_s>,
    /// The directory of the PDF, canonical when the PDF exists.
    pdf_dir: PathBuf,
    /// The `Input:` records, sorted by tag; the first one is the root file.
    inputs: Vec<Input>,
    /// Where each input is on disk now (see [`Scanner::location`]), computed on first use.
    locations: Vec<OnceCell<Option<PathBuf>>>,
    /// TeX's working directory where it is now (see [`tex_cwd`]).
    tex_cwd: Option<PathBuf>,
    /// The inputs matched by earlier `forward` calls, keyed by the requested path. The scanner
    /// lives for one build, so the file system is not re-checked within its lifetime.
    matches: HashMap<PathBuf, Box<[usize]>>,
}

// SAFETY: the libsynctex scanner and the nodes it hands out are owned exclusively by this value
// and are only used through `&mut self` methods and `drop`, so sending it to another thread
// never creates shared access. The library has no thread affinity (no thread-local state, no
// callbacks), and `LIBRARY` keeps its static buffers from being used by two threads at once.
unsafe impl Send for Scanner {}

impl Scanner {
    /// Opens the synctex data of `pdf`: `<stem>.synctex.gz` (or `<stem>.synctex`) in the same
    /// directory. Returns `None` if it is missing or cannot be parsed.
    pub fn open(pdf: &Path) -> Option<Self> {
        // The library recommends full paths; it replaces the last extension itself.
        let pdf = std::path::absolute(pdf).ok()?;
        let c_pdf = c_path(&pdf)?;
        // SAFETY: `c_pdf` is NUL-terminated and outlives the call; a null build directory is
        // documented as "ignored".
        let raw = with_library(|| unsafe {
            ffi::synctex_scanner_new_with_output_file(c_pdf.as_ptr(), ptr::null(), 1)
        });
        let raw = NonNull::new(raw)?;
        let pdf_dir = lemma_core::canonicalize(&pdf)
            .unwrap_or_else(|_| normalize(&pdf))
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        // Constructed before reading the inputs so that `Drop` frees `raw` on every path.
        let mut scanner = Scanner {
            raw,
            pdf_dir,
            inputs: Vec::new(),
            locations: Vec::new(),
            tex_cwd: None,
            matches: HashMap::new(),
        };
        // SAFETY: `raw` is a live scanner that was parsed on creation.
        scanner.inputs = with_library(|| unsafe { read_inputs(raw) });
        scanner.locations = scanner.inputs.iter().map(|_| OnceCell::new()).collect();
        scanner.tex_cwd = tex_cwd(&scanner.inputs, &scanner.pdf_dir);
        Some(scanner)
    }

    /// Forward search: the boxes in the PDF typeset from 1-based `line` of `file`.
    ///
    /// `file` is a path on disk, absolute or relative to the current directory. It is matched
    /// against the input names TeX recorded, which may be relative, contain `./`, or point to
    /// where the project was before it was moved or copied. Identical boxes are reported once.
    /// An unknown file or line 0 gives no boxes.
    ///
    /// Each box is the one enclosing a matched node, in bp with the origin at the top-left
    /// corner of the page: `v_top = box_visible_v − box_visible_height`,
    /// `height = box_visible_height + box_visible_depth`, `width = |box_visible_width|`.
    pub fn forward(&mut self, file: &Path, line: u32) -> Vec<SyncBox> {
        let line = match c_int::try_from(line) {
            Ok(line) if line > 0 => line,
            _ => return Vec::new(),
        };
        if !self.matches.contains_key(file) {
            let matched = self.match_inputs(file);
            self.matches.insert(file.to_path_buf(), matched);
        }
        let matched = &self.matches[file];
        let raw = self.raw.as_ptr();
        let mut boxes = Vec::new();
        let mut seen = HashSet::new();
        with_library(|| {
            for &i in matched.iter() {
                let name = &self.inputs[i].name;
                // SAFETY: `raw` is live; `name` is a NUL-terminated input name read from this
                // scanner. Result nodes are only used before the next query.
                unsafe {
                    if ffi::synctex_display_query(raw, name.as_ptr(), line, 0, 0) <= 0 {
                        continue;
                    }
                    loop {
                        let node = ffi::synctex_scanner_next_result(raw);
                        if node.is_null() {
                            break;
                        }
                        let Some(b) = sync_box(node) else { continue };
                        let key = [b.h, b.v_top, b.width, b.height].map(f32::to_bits);
                        let key = (b.page, key);
                        if seen.insert(key) {
                            boxes.push(b);
                        }
                    }
                }
            }
        });
        boxes
    }

    /// Inverse search: the source line typeset at (`x_bp`, `y_bp`) on 1-based `page`, in bp
    /// from the top-left corner. Synchronization is per line, because pdfTeX records no columns.
    ///
    /// `file` is the canonical path of the input when it exists, re-anchored at the PDF's
    /// directory tree if the project was moved since the build; otherwise the recorded path.
    pub fn inverse(&mut self, page: u32, x_bp: f32, y_bp: f32) -> Option<SyncTarget> {
        let page = c_int::try_from(page).ok().filter(|&p| p > 0)?;
        if !(x_bp.is_finite() && y_bp.is_finite()) {
            return None;
        }
        let raw = self.raw.as_ptr();
        // SAFETY: `raw` is live; result nodes are only used before the next query.
        let (tag, line) = with_library(|| unsafe {
            if ffi::synctex_edit_query(raw, page, x_bp, y_bp) <= 0 {
                return None;
            }
            loop {
                let node = ffi::synctex_scanner_next_result(raw);
                if node.is_null() {
                    return None;
                }
                let line = ffi::synctex_node_line(node);
                if line > 0 {
                    return Some((ffi::synctex_node_tag(node), line.unsigned_abs()));
                }
            }
        })?;
        let i = self.inputs.binary_search_by_key(&tag, |i| i.tag).ok()?;
        let file = match self.location_of(i) {
            Some(location) => location.clone(),
            None => self.expected_path(&self.inputs[i]),
        };
        Some(SyncTarget { file, line })
    }

    /// The inputs that are `file`, as indices into `self.inputs`.
    ///
    /// 1. Every input whose current location is `file`, whatever name TeX recorded for it (an
    ///    input can be recorded under several names, each with its own tag, so all of them are
    ///    queried; a name reached through a symlink may differ from the file's own name).
    /// 2. Otherwise, among inputs with `file`'s name that exist nowhere any more (a stale synctex
    ///    file), the one sharing the longest path suffix with `file`, unless several distinct
    ///    names tie.
    ///
    /// Inputs that exist on disk as some other file never match.
    fn match_inputs(&self, file: &Path) -> Box<[usize]> {
        let Ok(want) = std::path::absolute(file).map(|p| normalize(&p)) else {
            return Box::default();
        };
        let Some(file_name) = want.file_name() else {
            return Box::default();
        };
        // Canonicalized as given: resolving `..` lexically first could step out of a symlink.
        if let Ok(want_canonical) = lemma_core::canonicalize(file) {
            let exact: Vec<usize> = (0..self.inputs.len())
                .filter(|&i| self.location_of(i) == Some(&want_canonical))
                .collect();
            if !exact.is_empty() {
                return self.distinct_names(exact);
            }
        }

        let stale: Vec<(usize, usize)> = (0..self.inputs.len())
            .filter(|&i| self.inputs[i].path.file_name() == Some(file_name))
            .filter(|&i| self.location_of(i).is_none())
            .map(|i| (i, common_suffix_len(&self.inputs[i].path, &want)))
            .collect();
        let Some(best) = stale.iter().map(|&(_, len)| len).max() else {
            return Box::default();
        };
        let best: Vec<usize> = stale
            .into_iter()
            .filter(|&(_, len)| len == best)
            .map(|(i, _)| i)
            .collect();
        let first = &self.inputs[best[0]].path;
        if best.iter().any(|&i| self.inputs[i].path != *first) {
            return Box::default();
        }
        self.distinct_names(best)
    }

    /// Keeps the first input of each recorded name: libsynctex resolves a name to its first tag,
    /// so querying a name again would only repeat the same results.
    fn distinct_names(&self, mut matched: Vec<usize>) -> Box<[usize]> {
        let mut seen = HashSet::new();
        matched.retain(|&i| seen.insert(self.inputs[i].name.as_c_str()));
        matched.into()
    }

    /// Where the input `i` is on disk now, cached for the scanner's lifetime (see
    /// [`Scanner::location`]).
    fn location_of(&self, i: usize) -> Option<&PathBuf> {
        self.locations[i]
            .get_or_init(|| self.location(&self.inputs[i]))
            .as_ref()
    }

    /// Where `input` is on disk now (canonical), if it exists.
    fn location(&self, input: &Input) -> Option<PathBuf> {
        if let (Some(cwd), Some(rel)) = (&self.tex_cwd, &input.rel)
            && let Ok(path) = lemma_core::canonicalize(cwd.join(rel))
        {
            return Some(path);
        }
        if input.path.is_absolute() {
            lemma_core::canonicalize(&input.path).ok()
        } else {
            None
        }
    }

    /// Where `input` would be if it existed: the best guess for an input that is gone.
    fn expected_path(&self, input: &Input) -> PathBuf {
        match (&self.tex_cwd, &input.rel) {
            (Some(cwd), Some(rel)) => normalize(&cwd.join(rel)),
            _ if input.path.is_absolute() => input.path.clone(),
            _ => normalize(&self.pdf_dir.join(&input.path)),
        }
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `synctex_scanner_new_with_output_file` and is freed only here.
        with_library(|| unsafe { ffi::synctex_scanner_free(self.raw.as_ptr()) });
    }
}

impl fmt::Debug for Scanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner")
            .field("pdf_dir", &self.pdf_dir)
            .field("tex_cwd", &self.tex_cwd)
            .field("inputs", &self.inputs.len())
            .finish_non_exhaustive()
    }
}

/// Reads the `Input:` records, sorted by tag.
///
/// # Safety
/// `raw` must be a live, parsed scanner, and the [`LIBRARY`] lock must be held.
unsafe fn read_inputs(raw: NonNull<ffi::synctex_scanner_s>) -> Vec<Input> {
    let raw = raw.as_ptr();
    let mut inputs = Vec::new();
    // SAFETY (whole loop): nodes and names are owned by the live scanner `raw`; names are
    // NUL-terminated C strings and are copied before the next call.
    let mut node = unsafe { ffi::synctex_scanner_input(raw) };
    while !node.is_null() {
        let tag = unsafe { ffi::synctex_node_tag(node) };
        let name = unsafe { ffi::synctex_scanner_get_name(raw, tag) };
        if !name.is_null() {
            let name = unsafe { CStr::from_ptr(name) };
            if !name.is_empty() {
                inputs.push(Input::new(tag, name.to_owned()));
            }
        }
        node = unsafe { ffi::synctex_node_sibling(node) };
    }
    inputs.sort_by_key(|input| input.tag);
    inputs
}

/// The box enclosing a result node, or `None` for a node without a valid page.
///
/// # Safety
/// `node` must be a result node of a live scanner, and the [`LIBRARY`] lock must be held.
unsafe fn sync_box(node: ffi::synctex_node_p) -> Option<SyncBox> {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let page = u32::try_from(ffi::synctex_node_page(node))
            .ok()
            .filter(|&p| p > 0)?;
        let v = ffi::synctex_node_box_visible_v(node);
        let height = ffi::synctex_node_box_visible_height(node);
        Some(SyncBox {
            page,
            h: ffi::synctex_node_box_visible_h(node),
            v_top: v - height,
            width: ffi::synctex_node_box_visible_width(node).abs(),
            height: height + ffi::synctex_node_box_visible_depth(node),
        })
    }
}

/// How many levels above the PDF's directory TeX's working directory may be (`-outdir` puts
/// the PDF in a subdirectory of the project). Bounds the search after a project was moved, so
/// that an unrelated file far up the tree is not taken for the root file.
const MAX_OUTDIR_DEPTH: usize = 3;

/// TeX's working directory as it is now.
///
/// It is the recorded one when the PDF still lies inside it (the normal case, including
/// `-outdir` below the project). Otherwise the project was moved or copied after the build, or
/// the output directory lies outside the project (`build.outdir = "../build"`). Then it is the
/// nearest ancestor of the PDF's directory (at most [`MAX_OUTDIR_DEPTH`] levels up) that
/// contains the root file (the first input) under its recorded relative name, provided that,
/// while the recorded root still exists, the ancestor looks like a copy of the project (see
/// [`is_copy`]); failing that, the recorded working directory if the root is still there.
fn tex_cwd(inputs: &[Input], pdf_dir: &Path) -> Option<PathBuf> {
    let root = inputs.first()?;
    if let Some(cwd) = &root.cwd
        && pdf_dir.starts_with(cwd)
    {
        return Some(cwd.clone());
    }
    let rel = root.rel.as_ref()?;
    // The project at the recorded place, if its root file is still there.
    let recorded = root.cwd.as_ref().filter(|cwd| cwd.join(rel).is_file());
    pdf_dir
        .ancestors()
        .take(MAX_OUTDIR_DEPTH + 1)
        .find(|dir| dir.join(rel).is_file() && recorded.is_none_or(|cwd| is_copy(dir, cwd, inputs)))
        .or(recorded.map(PathBuf::as_path))
        .map(Path::to_path_buf)
}

/// True when `dir` looks like a copy of the project TeX ran in at `cwd` (which still exists),
/// rather than an unrelated directory that happens to hold a file named like the root: it is
/// not `cwd` or one of its ancestors, and it has every `.tex` source under `cwd` that still
/// exists there. (Generated inputs such as `.aux` or `.toc` files are often not copied.)
fn is_copy(dir: &Path, cwd: &Path, inputs: &[Input]) -> bool {
    !cwd.starts_with(dir)
        && inputs
            .iter()
            .filter(|input| input.cwd.as_deref() == Some(cwd))
            .filter_map(|input| input.rel.as_deref())
            .filter(|rel| rel.extension().is_some_and(|ext| ext == "tex"))
            .filter(|rel| cwd.join(rel).is_file())
            .all(|rel| dir.join(rel).is_file())
}
