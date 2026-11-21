//! Precompiled preambles (`[build] fast_preamble`).
//!
//! Loading the packages of a preamble is often half of a pdflatex run (0.67 s of 1.34 s for a
//! 27-page thesis with tikz-cd, hyperref and microtype), and it is the same on every save. So the
//! preamble of the root document is dumped once into a format (`mylatexformat.ltx`, as
//! `pdflatex -ini "&pdflatex" mylatexformat.ltx ./<root>.tex`), and latexmk runs pdflatex with it
//! (`-fmt`), which skips the preamble in the file. The text, the page layout, the links and the
//! SyncTeX lines are the same as without it. Both run the `pdflatex` of `PATH`, which latexmk
//! runs (also when `[build] latexmk` is a path): a format loads only in the pdfTeX that dumped
//! it, and a build with it uses the TeX installation of a build without it.
//!
//! The format lives in `<outdir>/.lemma-preamble/<stem>/` (one per root document, so that two
//! roots in one directory do not make each other's stale) with a record of what it was made
//! from: the root, a hash
//! of the root's text up to `\begin{document}`, and the size and modification time of every file
//! the dump read (packages, classes, `\input` files, pdflatex's own format), from its `.fls`. It is
//! [`Preamble::Current`] while they are all unchanged; otherwise [`Preamble::Stale`]: the build
//! runs without it and the app dumps a new one afterwards ([`dump_preamble`], about the time of
//! one more run). The dump's log keeps the preamble's warnings, which a run with the format does
//! not repeat; [`crate::Build::start_with`] adds them to the outcome.
//!
//! Only what is known to dump well uses one ([`Preamble::Unsupported`] otherwise): pdflatex
//! (XeTeX and LuaTeX cannot dump their fonts), on TeX Live and MacTeX (not on Windows), without a
//! latexmkrc (whose `$pdflatex` the format's command would override), a root file name of plain
//! characters, and a preamble that has `\documentclass` and does nothing that a format cannot keep:
//! open files (`\makeindex`, `\makeglossaries`, `\immediate`), images, PDF objects, shell escape,
//! another document's labels. A dump that fails, or that read a file of the output directory
//! (which every build changes), is recorded as failed and not tried again while its inputs stay
//! as they are; so is a format that pdflatex cannot load ([`give_up`]). A dump during which the
//! root or a file it read (outside the output directory, where the dump writes) was saved again
//! is thrown away, as it may hold the old text. A dump
//! also removes the formats of roots that were renamed or deleted since ([`remove_orphans`]).

use crate::paths::{normalize, resolve};
use crate::process::{Build, BuildStatus, MAX_LOG_BYTES, file_name, read_capped, root_parts};
use crate::request::LOG_ENV;
use crate::{BuildRequest, Engine};
use lemma_core::fnv1a64;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The format's name (`-fmt`), its log's and its record's.
const NAME: &str = "lemma-preamble";
/// The directory of the format in the output directory.
const DIR: &str = ".lemma-preamble";
/// The first line of a record.
const RECORD_VERSION: &str = "lemma-preamble 2";
/// The program that dumps the format and that latexmk runs with it, looked up in `PATH` as
/// latexmk looks up its own `pdflatex`.
const PDFLATEX: &str = "pdflatex";

/// Preamble commands that a format cannot keep (open files, images and PDF objects, shell
/// escape), or that read files a build changes.
const UNDUMPABLE: &[&str] = &[
    "\\makeindex",
    "\\makeglossaries",
    "\\makenoidxglossaries",
    "\\makenomenclature",
    "\\immediate",
    "\\openout",
    "\\write18",
    "\\ShellEscape",
    "\\pdfximage",
    "\\pdfobj",
    "\\pdfinfo",
    "\\pdfcatalog",
    "\\includegraphics",
    "\\includepdf",
    "\\tikzexternalize",
    "\\externaldocument",
    "\\externalcitedocument",
    "{minted}",
    "{pythontex}",
    "{svg}",
    "{shellesc}",
];

/// The dumps running in this process, by format directory.
static DUMPING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// What a build of a root can do with a precompiled preamble.
#[derive(Debug)]
pub enum Preamble {
    /// The format is current: build with it ([`crate::Build::start_with`]).
    Current(Format),
    /// There is none for the preamble as it is: build without, then [`dump_preamble`].
    Stale,
    /// This root cannot use one (see the module docs).
    Unsupported,
}

/// A current precompiled preamble.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    /// The directory of the format (put on `TEXFORMATS`).
    dir: PathBuf,
    /// The dump's log.
    log: PathBuf,
}

impl Format {
    /// The dump's log, with the preamble's warnings.
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// Makes latexmk's pdflatex load the format: `-pdflatex=pdflatex -fmt=lemma-preamble %O %S`
    /// after the engine flag, and the format's directory first on `TEXFORMATS`.
    pub(crate) fn apply(&self, args: &mut Vec<OsString>, env: &mut Vec<(String, String)>) {
        let at = usize::from(args.first().is_some_and(|a| a == "-pdf"));
        args.insert(at, format!("-pdflatex={PDFLATEX} -fmt={NAME} %O %S").into());
        // A trailing separator: then the default path, where pdflatex's own format is.
        let mut path = format!("{}:", self.dir.display());
        if let Ok(user) = std::env::var("TEXFORMATS") {
            path.push_str(&user);
        }
        env.push(("TEXFORMATS".to_owned(), path));
    }
}

/// Whether the build of `req` can use a precompiled preamble, and whether it is current.
/// Blocking but cheap (reads the root, and `stat`s the files the dump read).
pub fn preamble_for(req: &BuildRequest) -> Preamble {
    let Some(plan) = Plan::of(req) else {
        return Preamble::Unsupported;
    };
    let Some(record) = Record::read(&plan.dir.join(format!("{NAME}.record"))) else {
        return Preamble::Stale;
    };
    if record.hash != plan.hash || !record.inputs_unchanged() {
        return Preamble::Stale;
    }
    if !record.ok {
        return Preamble::Unsupported;
    }
    let format = Format {
        log: plan.dir.join(format!("{NAME}.log")),
        dir: plan.dir,
    };
    if !format.dir.join(format!("{NAME}.fmt")).is_file() {
        return Preamble::Stale;
    }
    Preamble::Current(format)
}

/// Blocking: dumps the preamble of `req`'s root into its format, unless it is current, not
/// supported, or being dumped already (by another build of this process). Runs `pdflatex -ini`
/// as a build runs latexmk (in the root's directory, in its own process group, within
/// `req.timeout`). Returns whether a usable format was written; a dump that fails is recorded so
/// that the next builds do not try again (see the module docs).
pub fn dump_preamble(req: &BuildRequest) -> io::Result<bool> {
    let Some(plan) = Plan::of(req) else {
        return Ok(false);
    };
    if !matches!(preamble_for(req), Preamble::Stale) {
        return Ok(false);
    }
    let Some(_guard) = DumpGuard::take(&plan.dir) else {
        return Ok(false);
    };
    fs::create_dir_all(&plan.dir)?;
    if let Some(formats) = plan.dir.parent() {
        remove_orphans(formats);
    }
    let (root_dir, stem) = root_parts(&plan.root)?;
    let mut output_dir = OsString::from("-output-directory=");
    output_dir.push(&plan.dir);
    let mut jobname = OsString::from("-jobname=");
    jobname.push(stem);
    let mut root_arg = OsString::from("./");
    root_arg.push(plan.root.file_name().unwrap_or_default());
    let args: Vec<OsString> = vec![
        "-ini".into(),
        "-interaction=nonstopmode".into(),
        "-halt-on-error".into(),
        "-recorder".into(),
        output_dir,
        jobname,
        "&pdflatex".into(),
        "mylatexformat.ltx".into(),
        root_arg,
    ];
    let env = LOG_ENV
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let out = plan.dir.join(format!("{NAME}.out"));
    let record = plan.dir.join(format!("{NAME}.record"));
    let started = SystemTime::now();
    let mut dump = match Build::launch_program(req, Path::new(PDFLATEX), &args, env, &out) {
        Ok(dump) => dump,
        Err(e) => {
            // No pdflatex: not again for this preamble.
            let failed = Record {
                root: plan.root.clone(),
                hash: plan.hash,
                ok: false,
                inputs: Vec::new(),
            };
            let _ = failed.write(&record);
            return Err(e);
        }
    };
    let (status, _, _) = dump.wait_for_exit();
    drop(dump);

    let dumped = plan.dir.join(file_name(stem, "fmt"));
    let inputs = read_inputs(
        &plan.dir.join(file_name(stem, "fls")),
        &root_dir,
        &plan.root,
    );
    // A preamble that reads what builds write would be stale after every build.
    let reads_output = inputs.iter().any(|p| p.starts_with(&plan.out_dir));
    let ok = status == BuildStatus::Success && dumped.is_file() && !reads_output;
    let inputs: Vec<_> = inputs.into_iter().map(|p| (stamp(&p), p)).collect();
    // The root may have been saved again meanwhile: then this dump is of another preamble. A
    // file it read that was saved meanwhile may be in it as it was before: its stamp would
    // still make the format current. Either way the next build dumps it again. (Not a file of
    // the output directory, where the dump writes its own files: reading one fails the dump.)
    let saved_meanwhile = saved_between(
        inputs
            .iter()
            .filter(|(_, path)| !path.starts_with(&plan.out_dir))
            .map(|(stamp, _)| *stamp),
        started,
        SystemTime::now(),
    );
    if saved_meanwhile || Plan::of(req).is_none_or(|now| now.hash != plan.hash) {
        let _ = fs::remove_file(&dumped);
        return Ok(false);
    }
    if ok {
        fs::rename(&dumped, plan.dir.join(format!("{NAME}.fmt")))?;
        fs::rename(
            plan.dir.join(file_name(stem, "log")),
            plan.dir.join(format!("{NAME}.log")),
        )?;
    } else {
        let _ = fs::remove_file(&dumped);
    }
    Record {
        root: plan.root.clone(),
        hash: plan.hash,
        ok,
        inputs: inputs.into_iter().map(|(stamp, p)| (p, stamp)).collect(),
    }
    .write(&record)?;
    Ok(ok)
}

/// Whether one of the `stamps` is a modification from `from` until `to`. A whole second may be
/// one up to two seconds later (FAT and HFS+ keep no fractions). A save in the first
/// milliseconds may have a time just before `from` (the kernel's clock for file times is coarse)
/// but TeX, which takes longer to start, read it. A time after `to` is a clock that differs,
/// not a save.
fn saved_between(
    mut stamps: impl Iterator<Item = Option<(u64, u128)>>,
    from: SystemTime,
    to: SystemTime,
) -> bool {
    const SECOND: u128 = Duration::from_secs(1).as_nanos();
    let nanos = |t: SystemTime| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let (from, to) = (nanos(from), nanos(to));
    stamps.any(|stamp| {
        stamp.is_some_and(|(_, modified)| {
            let coarse = if modified.is_multiple_of(SECOND) {
                2 * SECOND
            } else {
                0
            };
            (from.saturating_sub(coarse)..=to).contains(&modified)
        })
    })
}

/// Removes from `formats` (an output directory's `.lemma-preamble`) every format directory whose
/// record names a root that no longer exists (renamed, deleted, or its directory moved): nothing
/// would use it again. Left alone: a directory without a record of this version (a dump may be
/// writing it, in another window), one being dumped here, anything that is not a directory, and
/// everything when `formats` itself is not a directory; symbolic links are never followed, and
/// nothing outside `formats` is touched.
fn remove_orphans(formats: &Path) {
    if !fs::symlink_metadata(formats).is_ok_and(|m| m.is_dir()) {
        return;
    }
    let Ok(entries) = fs::read_dir(formats) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` does not follow a symbolic link.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        let Some(record) = Record::read(&dir.join(format!("{NAME}.record"))) else {
            continue;
        };
        let gone =
            matches!(fs::metadata(&record.root), Err(e) if e.kind() == io::ErrorKind::NotFound);
        if !gone {
            continue;
        }
        let Some(_guard) = DumpGuard::take(&dir) else {
            continue;
        };
        // `remove_dir_all` removes symbolic links inside, not what they point to.
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Records that the format of `req`'s root does not work (pdflatex could not load it): builds do
/// without it until the preamble or a file it read changes.
pub fn give_up(req: &BuildRequest) {
    let Some(plan) = Plan::of(req) else {
        return;
    };
    let path = plan.dir.join(format!("{NAME}.record"));
    if let Some(mut record) = Record::read(&path) {
        record.ok = false;
        let _ = record.write(&path);
    }
    let _ = fs::remove_file(plan.dir.join(format!("{NAME}.fmt")));
}

/// Removes the precompiled preamble of `req`'s root (Clean Build Files and Rebuild).
pub fn clear_preamble(req: &BuildRequest) {
    if let Ok((root_dir, stem)) = root_parts(&req.root) {
        let _ = fs::remove_dir_all(root_dir.join(&req.outdir).join(DIR).join(stem));
    }
}

/// Whether the last build of `req` failed because pdflatex could not load the format, from
/// latexmk's output: "I can't find the format file", "Fatal format file error" (a format of
/// another pdfTeX) or "Could not undump" (a truncated one). Only pdflatex's own messages count,
/// not the document's text in its errors, and only the start of a runaway output is read.
pub fn format_failed(req: &BuildRequest) -> bool {
    const MESSAGES: [&str; 3] = [
        "I can't find the format file",
        "Fatal format file error",
        "Could not undump",
    ];
    let Ok((root_dir, stem)) = root_parts(&req.root) else {
        return false;
    };
    let output = root_dir
        .join(&req.outdir)
        .join(file_name(stem, "latexmk.log"));
    read_capped(&output, MAX_LOG_BYTES)
        .is_some_and(|output| MESSAGES.iter().any(|m| output.contains(m)))
}

/// What a supported root's format is made from.
struct Plan {
    /// The root, absolute.
    root: PathBuf,
    /// The output directory, absolute.
    out_dir: PathBuf,
    /// The format's directory.
    dir: PathBuf,
    /// [`fnv1a64`] of the job name and the root's text up to `\begin{document}`.
    hash: u64,
}

impl Plan {
    /// `None` when `req`'s root cannot use a precompiled preamble (see the module docs).
    fn of(req: &BuildRequest) -> Option<Plan> {
        if cfg!(windows) || req.engine != Engine::PdfLatex {
            return None;
        }
        let root = normalize(&std::path::absolute(&req.root).ok()?);
        let name = root.file_name()?.to_str()?;
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        if !name.ends_with(".tex") || name.starts_with('-') || !name.chars().all(plain) {
            return None;
        }
        let root_dir = root.parent()?;
        if has_latexmkrc(root_dir) {
            return None;
        }
        let out_dir = normalize(&resolve(root_dir, Path::new(&req.outdir)));
        let dir = out_dir.join(DIR).join(root.file_stem()?);
        // TEXFORMATS is a list of paths: no separators or kpathsea expansions in this one.
        let path = dir.to_str()?;
        if path.contains([':', ';', '!', '$', '{', '}', '~', '\n']) {
            return None;
        }
        let text = fs::read(&root).ok()?;
        let text = String::from_utf8_lossy(&text);
        let preamble = preamble(&text)?;
        Some(Plan {
            hash: fnv1a64(&[name.as_bytes(), &[0], preamble.as_bytes()]),
            root,
            out_dir,
            dir,
        })
    }
}

/// The text of a root up to the end of the line of its first `\begin{document}`, when that
/// preamble can be dumped: it has `\documentclass` and none of [`UNDUMPABLE`] (comments left
/// out).
fn preamble(text: &str) -> Option<&str> {
    let mut end = 0;
    let mut class = false;
    for line in text.split_inclusive('\n') {
        end += line.len();
        let code = without_comment(line);
        if UNDUMPABLE.iter().any(|c| code.contains(c)) {
            return None;
        }
        class |= code.contains("\\documentclass");
        if code.contains("\\begin{document}") {
            return class.then(|| &text[..end]);
        }
    }
    None
}

/// `line` up to its first `%` that is not escaped (`\%`; `\\%` is a comment again).
fn without_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'%' => return &line[..i],
            _ => i += 1,
        }
    }
    line
}

/// Whether latexmk reads a latexmkrc for a root in `dir` (besides the system's).
fn has_latexmkrc(dir: &Path) -> bool {
    let mut files = vec![dir.join("latexmkrc"), dir.join(".latexmkrc")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        files.push(home.join(".latexmkrc"));
        files.push(home.join(".config/latexmk/latexmkrc"));
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        files.push(PathBuf::from(config).join("latexmk/latexmkrc"));
    }
    files.iter().any(|f| f.is_file())
}

/// The files a TeX run read, from its `.fls` (relative ones resolved against its `PWD`), but
/// not `root` (its body is not part of the format; its preamble is hashed).
fn read_inputs(fls: &Path, root_dir: &Path, root: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read(fls) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&text);
    let mut pwd = root_dir.to_path_buf();
    let mut seen = HashSet::new();
    let mut inputs = Vec::new();
    for line in text.lines() {
        if let Some(dir) = line.strip_prefix("PWD ") {
            pwd = PathBuf::from(dir);
        } else if let Some(file) = line.strip_prefix("INPUT ") {
            let path = normalize(&resolve(&pwd, Path::new(file)));
            if path != root && seen.insert(path.clone()) {
                inputs.push(path);
            }
        }
    }
    inputs
}

/// A file's size and modification time in nanoseconds; `None` when it is gone.
fn stamp(path: &Path) -> Option<(u64, u128)> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), modified.as_nanos()))
}

/// What a format was made from (`lemma-preamble.record`).
#[derive(Debug, PartialEq, Eq)]
struct Record {
    /// The root document, absolute.
    root: PathBuf,
    hash: u64,
    /// The dump worked (and pdflatex has not failed to load it).
    ok: bool,
    /// Every file the dump read, with its [`stamp`] then.
    inputs: Vec<(PathBuf, Option<(u64, u128)>)>,
}

impl Record {
    fn read(path: &Path) -> Option<Record> {
        let text = fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        if lines.next()? != RECORD_VERSION {
            return None;
        }
        let root = PathBuf::from(lines.next()?.strip_prefix("root ")?);
        let hash = u64::from_str_radix(lines.next()?.strip_prefix("hash ")?, 16).ok()?;
        let ok = match lines.next()? {
            "ok" => true,
            "failed" => false,
            _ => return None,
        };
        let mut inputs = Vec::new();
        for line in lines {
            let mut fields = line.splitn(3, ' ');
            let (len, modified, path) = (fields.next()?, fields.next()?, fields.next()?);
            let stamp = match (len, modified) {
                ("-", "-") => None,
                _ => Some((len.parse().ok()?, modified.parse().ok()?)),
            };
            inputs.push((PathBuf::from(path), stamp));
        }
        Some(Record {
            root,
            hash,
            ok,
            inputs,
        })
    }

    /// Writes the record next to `path` and renames it into place.
    fn write(&self, path: &Path) -> io::Result<()> {
        let mut text = format!(
            "{RECORD_VERSION}\nroot {}\nhash {:016x}\n{}\n",
            self.root.display(),
            self.hash,
            if self.ok { "ok" } else { "failed" }
        );
        for (input, stamp) in &self.inputs {
            let _ = match stamp {
                Some((len, modified)) => writeln!(text, "{len} {modified} {}", input.display()),
                None => writeln!(text, "- - {}", input.display()),
            };
        }
        let temp = path.with_extension("record-new");
        fs::write(&temp, text)?;
        fs::rename(&temp, path)
    }

    fn inputs_unchanged(&self) -> bool {
        self.inputs.iter().all(|(path, was)| stamp(path) == *was)
    }
}

/// Marks a format directory as being dumped for as long as it lives.
struct DumpGuard(PathBuf);

impl DumpGuard {
    fn take(dir: &Path) -> Option<DumpGuard> {
        let mut dumping = DUMPING.lock().unwrap_or_else(PoisonError::into_inner);
        if dumping.iter().any(|d| d == dir) {
            return None;
        }
        dumping.push(dir.to_path_buf());
        Some(DumpGuard(dir.to_path_buf()))
    }
}

impl Drop for DumpGuard {
    fn drop(&mut self) {
        let mut dumping = DUMPING.lock().unwrap_or_else(PoisonError::into_inner);
        dumping.retain(|d| *d != self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preamble_ends_with_the_line_of_begin_document() {
        let text = "\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\nHi\n\\end{document}\n";
        assert_eq!(
            preamble(text),
            Some("\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\n")
        );
        // A commented \begin{document} is not the end, and a document needs its class.
        let commented = "\\documentclass{article}\n% \\begin{document}\nx\n\\begin{document}\n";
        assert_eq!(preamble(commented), Some(commented));
        assert_eq!(preamble("\\input{setup}\n\\begin{document}\n"), None);
        assert_eq!(preamble("\\documentclass{article}\nno document\n"), None);
    }

    #[test]
    fn preambles_a_format_cannot_keep_are_refused() {
        let with =
            |line: &str| format!("\\documentclass{{article}}\n{line}\n\\begin{{document}}\n");
        for line in [
            "\\makeindex",
            "\\usepackage{glossaries}\\makeglossaries",
            "\\newwrite\\f\\immediate\\openout\\f=x.txt",
            "\\usepackage{minted}",
            "\\usetikzlibrary{external}\\tikzexternalize",
            "\\sbox0{\\includegraphics{logo}}",
        ] {
            assert_eq!(preamble(&with(line)), None, "{line}");
        }
        // Only in a comment, or an escaped percent before it.
        assert!(preamble(&with("% \\makeindex")).is_some());
        assert!(preamble(&with("50\\% done \\makeindex")).is_none());
        assert!(preamble(&with("\\\\% \\makeindex")).is_some());
    }

    #[test]
    fn records_round_trip_and_see_changed_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sty = dir.path().join("my style.sty");
        fs::write(&sty, "\\def\\x{1}").unwrap();
        let gone = dir.path().join("gone.cfg");
        let record = Record {
            root: dir.path().join("main.tex"),
            hash: 0x0123_4567_89ab_cdef,
            ok: true,
            inputs: vec![(sty.clone(), stamp(&sty)), (gone.clone(), None)],
        };
        let path = dir.path().join("lemma-preamble.record");
        record.write(&path).unwrap();
        let read = Record::read(&path).expect("a record");
        assert_eq!(read, record);
        assert!(read.inputs_unchanged());
        fs::write(&sty, "\\def\\x{22}").unwrap();
        assert!(!read.inputs_unchanged(), "a package changed");
        fs::write(&path, "lemma-preamble 0\n").unwrap();
        assert_eq!(Record::read(&path), None, "another version");
    }

    #[test]
    fn fls_inputs_are_absolute_without_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main.tex");
        let fls = dir.path().join("main.fls");
        // An absolute path of this system (`/usr/…` is not one on Windows).
        let format = dir.path().join("texmf").join("pdflatex.fmt");
        fs::write(
            &fls,
            format!(
                "PWD {}\nINPUT {}\nINPUT ./main.tex\nINPUT ./setup.tex\nINPUT ./setup.tex\nOUTPUT main.log\n",
                dir.path().display(),
                format.display()
            ),
        )
        .unwrap();
        assert_eq!(
            read_inputs(&fls, dir.path(), &root),
            [format, dir.path().join("setup.tex")]
        );
    }

    /// The environment variable that holds the directory of [`in_child_with_bin`]'s child.
    #[cfg(unix)]
    const CHILD_DIR: &str = "LEMMA_PREAMBLE_TEST_DIR";

    /// Runs the test `name` again in a child process whose `PATH` starts with `<dir>/bin`, where
    /// `programs` are written as shell scripts (a test cannot change its own `PATH`: other tests
    /// run on other threads). Returns `<dir>` in the child, `None` in the parent once the child
    /// passed.
    #[cfg(unix)]
    fn in_child_with_bin(name: &str, programs: &[(&str, &str)]) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            return Some(PathBuf::from(dir));
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let p = lemma_core::canonicalize(dir.path()).expect("canonical");
        for (path, script) in programs {
            let path = p.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Copied into place by `cp`: had this process written it, a child forked meanwhile
            // by a test on another thread could still hold it open for writing ("Text file
            // busy").
            let source = p.join("script.src");
            fs::write(&source, script).unwrap();
            let copied = std::process::Command::new("cp")
                .arg(&source)
                .arg(&path)
                .status();
            assert!(copied.is_ok_and(|s| s.success()));
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut path = p.join("bin").into_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", name, "--test-threads=1", "--nocapture"])
            .env(CHILD_DIR, &p)
            .env("PATH", path)
            .output()
            .expect("run the child test");
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        None
    }

    /// A fake pdfTeX that writes what a dump writes: an `.fls` that read `macros.sty`, a log, and
    /// a format that says `$1` made it.
    #[cfg(unix)]
    fn fake_dump(who: &str) -> String {
        format!(
            "#!/bin/sh\nfor a; do case $a in -output-directory=*) out=${{a#*=}};; \
             -jobname=*) job=${{a#*=}};; esac; done\n\
             printf 'PWD %s\\nINPUT ./macros.sty\\nINPUT ./main.tex\\n' \"$(pwd -P)\" > \"$out/$job.fls\"\n\
             echo {who} > \"$out/$job.fmt\"\necho log > \"$out/$job.log\"\n"
        )
    }

    /// A root whose preamble loads `macros.sty`, and the request of its build.
    #[cfg(unix)]
    fn macros_project(p: &Path, latexmk: PathBuf) -> BuildRequest {
        fs::write(
            p.join("main.tex"),
            "\\documentclass{article}\n\\usepackage{macros}\n\\begin{document}\nx\n\\end{document}\n",
        )
        .unwrap();
        let macros = p.join("macros.sty");
        fs::write(&macros, "\\def\\x{old}\n").unwrap();
        // Saved a minute ago, not during the dump.
        fs::File::options()
            .write(true)
            .open(&macros)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        BuildRequest {
            root: p.join("main.tex"),
            engine: Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(20),
            latexmk,
        }
    }

    /// latexmk runs the `pdflatex` of `PATH`, also when `[build] latexmk` is a path, and only the
    /// pdfTeX that dumped a format can load it: the dump must not take the pdfTeX beside
    /// latexmk, another TeX installation.
    #[cfg(unix)]
    #[test]
    fn the_format_is_dumped_by_the_pdflatex_the_build_runs() {
        let Some(p) = in_child_with_bin(
            "preamble::tests::the_format_is_dumped_by_the_pdflatex_the_build_runs",
            &[
                ("other-tex/latexmk", "#!/bin/sh\nexit 1\n"),
                ("other-tex/pdftex", &fake_dump("other-tex/pdftex")),
                ("other-tex/pdflatex", &fake_dump("other-tex/pdflatex")),
                ("bin/pdflatex", &fake_dump("bin/pdflatex")),
            ],
        ) else {
            return;
        };
        let req = macros_project(&p, p.join("other-tex/latexmk"));
        if Plan::of(&req).is_none() {
            eprintln!("SKIP: a latexmkrc in the home directory");
            return;
        }
        assert!(dump_preamble(&req).expect("dump"));
        let Preamble::Current(format) = preamble_for(&req) else {
            panic!("a current format");
        };
        let fmt = fs::read_to_string(format.dir.join(format!("{NAME}.fmt"))).unwrap();
        assert_eq!(fmt.trim(), "bin/pdflatex");
        let mut args = Vec::new();
        format.apply(&mut args, &mut Vec::new());
        assert_eq!(args[0], "-pdflatex=pdflatex -fmt=lemma-preamble %O %S");
    }

    /// After a rename of the root, the next dump (of the new name) removes the old name's format.
    #[cfg(unix)]
    #[test]
    fn a_renamed_roots_format_goes_with_the_next_dump() {
        let Some(p) = in_child_with_bin(
            "preamble::tests::a_renamed_roots_format_goes_with_the_next_dump",
            &[("bin/pdflatex", &fake_dump("pdflatex"))],
        ) else {
            return;
        };
        let mut req = macros_project(&p, PathBuf::from("latexmk"));
        if Plan::of(&req).is_none() {
            eprintln!("SKIP: a latexmkrc in the home directory");
            return;
        }
        assert!(dump_preamble(&req).expect("dump"));
        let old = p.join(".build/.lemma-preamble/main");
        assert!(old.join(format!("{NAME}.fmt")).is_file());
        fs::rename(&req.root, p.join("thesis.tex")).unwrap();
        req.root = p.join("thesis.tex");
        assert!(dump_preamble(&req).expect("dump"));
        assert!(!old.exists(), "the format of main.tex, which is gone");
        assert!(matches!(preamble_for(&req), Preamble::Current(_)));
    }

    /// Only directories whose record names a root that is gone are removed: not one whose root
    /// exists, one without a record (a dump in progress), a file, or anything behind a symbolic
    /// link, inside `.lemma-preamble` or as `.lemma-preamble` itself.
    #[cfg(unix)]
    #[test]
    fn only_formats_of_roots_that_are_gone_are_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        let formats = p.join(".build/.lemma-preamble");
        let format = |dir: &Path, root: &Path| {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(format!("{NAME}.fmt")), "fmt").unwrap();
            Record {
                root: root.to_path_buf(),
                hash: 1,
                ok: true,
                inputs: Vec::new(),
            }
            .write(&dir.join(format!("{NAME}.record")))
            .unwrap();
        };
        fs::write(p.join("kept.tex"), "").unwrap();
        format(&formats.join("kept"), &p.join("kept.tex"));
        format(&formats.join("gone"), &p.join("gone.tex"));
        format(&formats.join("moved"), &p.join("old-dir/moved.tex"));
        fs::create_dir_all(formats.join("dumping")).unwrap();
        fs::write(formats.join("dumping/lemma-preamble.log"), "").unwrap();
        fs::write(formats.join("a-file"), "").unwrap();
        // Outside: a format of a root that is gone, reached through a link.
        let outside = p.join("outside");
        format(&outside.join("gone"), &p.join("gone.tex"));
        std::os::unix::fs::symlink(outside.join("gone"), formats.join("link")).unwrap();

        remove_orphans(&formats);
        let mut left: Vec<String> = fs::read_dir(&formats)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["a-file", "dumping", "kept", "link"]);
        assert!(outside.join("gone/lemma-preamble.fmt").is_file());

        // `.lemma-preamble` itself a link: nothing is removed where it points.
        let linked = p.join("linked/.build");
        fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(&outside, linked.join(".lemma-preamble")).unwrap();
        remove_orphans(&linked.join(".lemma-preamble"));
        assert!(outside.join("gone/lemma-preamble.fmt").is_file());
    }

    /// A package the dump read that is saved again while the dump runs: the format may hold its
    /// old text, so it is not current. (pdflatex is a script here that writes what a dump writes,
    /// then saves `macros.sty` when told to, as an editor would meanwhile, after as long as TeX
    /// takes at least to start.)
    #[cfg(unix)]
    #[test]
    fn a_file_saved_during_the_dump_leaves_the_preamble_stale() {
        let Some(p) = in_child_with_bin(
            "preamble::tests::a_file_saved_during_the_dump_leaves_the_preamble_stale",
            &[(
                "bin/pdflatex",
                &(fake_dump("pdflatex")
                    + "[ -f save-during-dump ] && sleep 0.1 && echo '\\def\\x{new}' > macros.sty\nexit 0\n"),
            )],
        ) else {
            return;
        };
        let bin = p.join("bin");
        let root = p.join("main.tex");
        let text = |body: &str| {
            format!(
                "\\documentclass{{article}}\n\\usepackage{{macros}}\n{body}\n\\begin{{document}}\nx\n\\end{{document}}\n"
            )
        };
        fs::write(&root, text("")).unwrap();
        let macros = p.join("macros.sty");
        fs::write(&macros, "\\def\\x{old}\n").unwrap();
        // Saved a minute ago.
        fs::File::options()
            .write(true)
            .open(&macros)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let req = BuildRequest {
            root,
            engine: Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(20),
            latexmk: bin.join("latexmk"),
        };
        if Plan::of(&req).is_none() {
            eprintln!("SKIP: a latexmkrc in the home directory");
            return;
        }
        assert!(dump_preamble(&req).expect("dump"));
        assert!(matches!(preamble_for(&req), Preamble::Current(_)));

        // Another preamble, and macros.sty saved while it is dumped.
        fs::write(&req.root, text("\\newcommand\\y{y}")).unwrap();
        fs::write(p.join("save-during-dump"), "").unwrap();
        let _ = dump_preamble(&req).expect("dump");
        assert!(
            matches!(preamble_for(&req), Preamble::Stale),
            "the format has the old macros.sty"
        );
    }

    #[test]
    fn saves_count_from_the_start_of_the_dump_to_its_end() {
        let stamp = |ms: u64| Some((1, Duration::from_millis(ms).as_nanos()));
        let (from, to) = (
            UNIX_EPOCH + Duration::from_millis(100_250),
            UNIX_EPOCH + Duration::from_secs(102),
        );
        assert!(saved_between([stamp(101_500)].into_iter(), from, to));
        // A whole second may be a save up to two seconds later.
        assert!(saved_between([stamp(99_000)].into_iter(), from, to));
        assert!(!saved_between(
            [stamp(100_249), stamp(98_000), None, stamp(102_001)].into_iter(),
            from,
            to
        ));
    }

    /// pdflatex's own messages about the format say it did not load; the document's text in the
    /// output (an error's context) does not.
    #[test]
    fn a_format_failure_is_what_pdflatex_says_about_the_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let req = BuildRequest {
            root: dir.path().join("main.tex"),
            engine: Engine::PdfLatex,
            outdir: ".build".into(),
            mirror_dirs: Vec::new(),
            timeout: Duration::from_secs(180),
            latexmk: PathBuf::from("latexmk"),
        };
        assert!(!format_failed(&req), "no output");
        fs::create_dir(dir.path().join(".build")).unwrap();
        let banner = "Latexmk: applying rule 'pdflatex'...\nRule 'pdflatex':  Reasons for rerun\n\
                      This is pdfTeX, Version 3.141592653-2.6-1.40.29 (TeX Live 2026/Arch Linux) \
                      (preloaded format=lemma-preamble)\n restricted \\write18 enabled.\n";
        for (said, failed) in [
            ("I can't find the format file `lemma-preamble.fmt'!", true),
            ("(Fatal format file error; I'm stymied)", true),
            (
                "pdflatex: fatal: Could not undump 432735 16-byte item(s) from \
                 /p/.build/.lemma-preamble/main/lemma-preamble.fmt.",
                true,
            ),
            (
                "./main.tex:3: Undefined control sequence.\n\
                 l.3 The format file of TeX is \\undefinedmacro",
                false,
            ),
        ] {
            fs::write(
                dir.path().join(".build/main.latexmk.log"),
                format!("{banner}{said}\nLatexmk: Errors, so I did not complete making targets\n"),
            )
            .unwrap();
            assert_eq!(format_failed(&req), failed, "{said}");
        }
    }

    #[test]
    fn the_format_goes_after_the_engine_flag() {
        let format = Format {
            dir: PathBuf::from("/p/.build/.lemma-preamble"),
            log: PathBuf::from("/p/.build/.lemma-preamble/lemma-preamble.log"),
        };
        let mut args: Vec<OsString> = vec!["-pdf".into(), "-synctex=1".into()];
        let mut env = Vec::new();
        format.apply(&mut args, &mut env);
        assert_eq!(args[0], "-pdf");
        assert_eq!(args[1], "-pdflatex=pdflatex -fmt=lemma-preamble %O %S");
        assert_eq!(env[0].0, "TEXFORMATS");
        assert!(env[0].1.starts_with("/p/.build/.lemma-preamble:"));
    }
}
