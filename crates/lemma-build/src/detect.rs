//! Which file to build and with which engine (§6).

use crate::paths::{normalize, resolve, with_tex_extension};
use crate::process::read_capped;
use lemma_syntax::{FileIndex, IncludeKind, ProjectIndex, parse_magic};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The TeX engine latexmk is told to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    PdfLatex,
    LuaLatex,
    XeLatex,
    /// Pass no engine flag; the project's latexmkrc decides.
    Latexmkrc,
}

/// Which rule of [`detect_root`] found the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSource {
    MagicComment,
    Subfiles,
    SelfDocument,
    IncludeGraph,
    Fls,
    Remembered,
    Unknown,
}

/// The result of [`detect_root`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootGuess {
    pub root: Option<PathBuf>,
    pub source: RootSource,
}

// Magic comments and the subfiles parent are read by `lemma_syntax::parse_magic`. These find a
// complete document and its packages.
static DOCUMENTCLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[^%\n]*?\\documentclass\s*(?:\[[^\]]*\])?\s*\{[^}]+\}").expect("valid regex")
});
static BEGIN_DOCUMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[^%\n]*?\\begin\s*\{document\}").expect("valid regex"));
static USEPACKAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[^%\n]*?\\(?:usepackage|RequirePackage)\s*(?:\[[^\]]*\])?\s*\{(?P<list>[^}]*)\}",
    )
    .expect("valid regex")
});

/// Packages that only work with a Unicode engine.
const UNICODE_ENGINE_PACKAGES: &[&str] = &["fontspec", "unicode-math", "polyglossia"];

/// How many directories above the file to search for a previous build's `.fls`.
const FLS_SEARCH_DEPTH: usize = 4;
/// Larger `.fls` files are read only up to this size.
const MAX_FLS_BYTES: u64 = 16 << 20;

/// Finds the root document for `file`, whose current text is `text`.
///
/// Rules, first match wins:
/// 1. `% !TEX root = …`, relative to the file's directory (`.tex` implied);
/// 2. the parent named by `\documentclass[<parent>]{subfiles}`;
/// 3. the file itself, if it has an uncommented `\documentclass` and `\begin{document}`;
/// 4. a document in `idx` that reaches the file through `\input`, `\include`, `\subfile` or
///    `\import`, or, for a `.bib` file, through `\bibliography` or `\addbibresource`
///    (preferring `remembered` when several do);
/// 5. a previous build's `.fls` (in `.build/` or next to the file, up to four directories up)
///    that lists the file as an input;
/// 6. `remembered`, the user's earlier choice, if the file is in the remembered root's directory
///    or below it (a choice made for one project does not apply to another);
/// 7. `Unknown`: the UI asks once per project.
///
/// Rule 5 reads directory listings and (regular, size-capped) `.fls` files, so call this off the
/// main thread.
pub fn detect_root(
    file: &Path,
    text: &str,
    idx: &ProjectIndex,
    remembered: Option<&Path>,
) -> RootGuess {
    let dir = file.parent().unwrap_or(Path::new(""));
    let guess = |root: PathBuf, source| RootGuess {
        root: Some(root),
        source,
    };
    let magic = parse_magic(text);
    if let Some(root) = magic.root {
        return guess(tex_file(resolve(dir, &root)), RootSource::MagicComment);
    }
    if let Some(parent) = magic.subfiles_parent {
        return guess(tex_file(resolve(dir, &parent)), RootSource::Subfiles);
    }
    if DOCUMENTCLASS.is_match(text) && BEGIN_DOCUMENT.is_match(text) {
        return guess(normalize(file), RootSource::SelfDocument);
    }
    if let Some(root) = include_graph_root(file, idx, remembered) {
        return guess(root, RootSource::IncludeGraph);
    }
    if let Some(root) = fls_root(file) {
        return guess(root, RootSource::Fls);
    }
    if let Some(root) = remembered.map(normalize)
        && root
            .parent()
            .is_some_and(|project| normalize(file).starts_with(project))
    {
        return guess(root, RootSource::Remembered);
    }
    RootGuess {
        root: None,
        source: RootSource::Unknown,
    }
}

/// Chooses the engine for the root document `root_text` in `root_dir`.
///
/// The first `% !TEX program = …` wins (latexmk itself ignores it). Otherwise a `latexmkrc` or
/// `.latexmkrc` in `root_dir` means [`Engine::Latexmkrc`]; otherwise LuaLaTeX if fontspec,
/// unicode-math or polyglossia is loaded; otherwise pdfLaTeX. An unknown program name (such as
/// `latexmk`) is ignored, and the other rules decide.
pub fn detect_engine(root_text: &str, root_dir: &Path) -> Engine {
    if let Some(engine) = parse_magic(root_text)
        .program
        .as_deref()
        .and_then(engine_for_program)
    {
        return engine;
    }
    if ["latexmkrc", ".latexmkrc"]
        .iter()
        .any(|rc| root_dir.join(rc).is_file())
    {
        return Engine::Latexmkrc;
    }
    let needs_unicode_engine = USEPACKAGE.captures_iter(root_text).any(|c| {
        c["list"]
            .split(',')
            .any(|p| UNICODE_ENGINE_PACKAGES.contains(&p.trim()))
    });
    if needs_unicode_engine {
        Engine::LuaLatex
    } else {
        Engine::PdfLatex
    }
}

fn engine_for_program(program: &str) -> Option<Engine> {
    let p = program.to_ascii_lowercase();
    if p.starts_with("lua") {
        Some(Engine::LuaLatex)
    } else if p.starts_with("xe") {
        Some(Engine::XeLatex)
    } else if p.starts_with("pdf") || p == "latex" || p == "tex" {
        Some(Engine::PdfLatex)
    } else {
        None
    }
}

/// A root named in a magic comment or subfiles option: `.tex` is implied for a bare name, and
/// also tried when the name as written does not exist (`thesis.v2` → `thesis.v2.tex`).
fn tex_file(path: PathBuf) -> PathBuf {
    if path.extension().is_none_or(|e| e == "tex") || path.exists() {
        return with_tex_extension(path);
    }
    let mut name = path.clone().into_os_string();
    name.push(".tex");
    let with_ext = PathBuf::from(name);
    if with_ext.exists() { with_ext } else { path }
}

/// A complete document in `idx` whose include graph reaches `file`.
fn include_graph_root(
    file: &Path,
    idx: &ProjectIndex,
    remembered: Option<&Path>,
) -> Option<PathBuf> {
    let target = normalize(file);
    let files: HashMap<PathBuf, &FileIndex> = idx
        .files
        .iter()
        .map(|(p, fi)| (normalize(p), fi.as_ref()))
        .collect();
    let mut roots: Vec<&PathBuf> = files
        .iter()
        .filter(|(p, fi)| fi.has_documentclass && fi.has_begin_document && **p != target)
        .map(|(p, _)| p)
        .filter(|root| reaches(&files, root, &target))
        .collect();
    roots.sort();
    let remembered = remembered.map(normalize);
    roots
        .iter()
        .find(|r| Some(r.as_path()) == remembered.as_deref())
        .or(roots.first())
        .map(|r| (*r).clone())
}

/// Whether `target` is reachable from `root` through file-inclusion commands. Targets are tried
/// relative to the root's directory (TeX's working directory) and to the including file's.
///
/// A `.bib` target is also reached through `\bibliography{name}` and `\addbibresource{name.bib}`
/// (BibTeX and biber never appear in the `.fls`, so this is the only way to find its document).
fn reaches(files: &HashMap<PathBuf, &FileIndex>, root: &Path, target: &Path) -> bool {
    let root_dir = root.parent().unwrap_or(Path::new(""));
    let target_is_bib = target.extension().is_some_and(|e| e == "bib");
    let mut seen = HashSet::from([root.to_path_buf()]);
    let mut todo = vec![root.to_path_buf()];
    while let Some(current) = todo.pop() {
        let Some(fi) = files.get(&current) else {
            continue;
        };
        let current_dir = current.parent().unwrap_or(Path::new(""));
        if target_is_bib {
            let bibliographies = fi
                .includes
                .iter()
                .filter(|inc| inc.kind == IncludeKind::Bibliography);
            for inc in bibliographies {
                for base in [root_dir, current_dir] {
                    let written = resolve(base, Path::new(inc.target.trim()));
                    if bib_candidates(written).iter().any(|c| c == target) {
                        return true;
                    }
                }
            }
        }
        let includes = fi.includes.iter().filter(|inc| {
            matches!(
                inc.kind,
                IncludeKind::Input
                    | IncludeKind::Include
                    | IncludeKind::Subfile
                    | IncludeKind::Import
            )
        });
        for inc in includes {
            for base in [root_dir, current_dir] {
                let written = resolve(base, Path::new(inc.target.trim()));
                for candidate in include_candidates(written) {
                    if candidate == target {
                        return true;
                    }
                    if files.contains_key(&candidate) && seen.insert(candidate.clone()) {
                        todo.push(candidate);
                    }
                }
            }
        }
    }
    false
}

/// TeX tries `name.tex` before `name` unless the name already ends in `.tex`.
fn include_candidates(written: PathBuf) -> Vec<PathBuf> {
    if written.extension().is_some_and(|e| e == "tex") {
        return vec![written];
    }
    let mut name = written.clone().into_os_string();
    name.push(".tex");
    vec![PathBuf::from(name), written]
}

/// BibTeX appends `.bib` to a database name; biber takes the name as written.
fn bib_candidates(written: PathBuf) -> Vec<PathBuf> {
    if written.extension().is_some_and(|e| e == "bib") {
        return vec![written];
    }
    let mut name = written.clone().into_os_string();
    name.push(".bib");
    vec![PathBuf::from(name), written]
}

/// A previous build's `.fls` that lists `file` as an input names the root: `<PWD>/<stem>.tex`.
fn fls_root(file: &Path) -> Option<PathBuf> {
    let target = lemma_core::canonicalize(file).unwrap_or_else(|_| normalize(file));
    let dir = file.parent()?;
    for ancestor in dir.ancestors().take(FLS_SEARCH_DEPTH) {
        for fls_dir in [ancestor.join(".build"), ancestor.to_path_buf()] {
            let Ok(entries) = fs::read_dir(&fls_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "fls") {
                    continue;
                }
                // Regular files only, read up to a cap: a FIFO named `*.fls` would block, and a
                // symlink to /dev/zero would never end.
                let Some(fls) = read_capped(&path, MAX_FLS_BYTES) else {
                    continue;
                };
                if let Some(root) = root_from_fls(&fls, &path, &target) {
                    return Some(root);
                }
            }
        }
    }
    None
}

fn root_from_fls(fls: &str, fls_path: &Path, target: &Path) -> Option<PathBuf> {
    let pwd = PathBuf::from(fls.lines().find_map(|l| l.strip_prefix("PWD "))?);
    let lists_target = fls
        .lines()
        .filter_map(|l| l.strip_prefix("INPUT "))
        .any(|input| resolve(&pwd, Path::new(input)) == target);
    if !lists_target {
        return None;
    }
    let mut name = fls_path.file_stem()?.to_os_string();
    name.push(".tex");
    let root = pwd.join(name);
    root.is_file().then_some(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_syntax::Include;
    use std::sync::Arc;

    /// A file in a test index: path, whether it is a complete document, and its includes.
    type IndexEntry<'a> = (&'a str, bool, &'a [(IncludeKind, &'a str)]);

    fn index(entries: &[IndexEntry<'_>]) -> ProjectIndex {
        let mut idx = ProjectIndex::default();
        for (path, complete, includes) in entries {
            let fi = FileIndex {
                includes: includes
                    .iter()
                    .map(|(kind, target)| Include {
                        kind: *kind,
                        target: (*target).to_owned(),
                        line: 0,
                    })
                    .collect(),
                has_documentclass: *complete,
                has_begin_document: *complete,
                ..FileIndex::default()
            };
            idx.files.insert(PathBuf::from(path), Arc::new(fi));
        }
        idx
    }

    fn root(file: &str, text: &str, idx: &ProjectIndex, remembered: Option<&str>) -> RootGuess {
        detect_root(Path::new(file), text, idx, remembered.map(Path::new))
    }

    fn found(path: &str, source: RootSource) -> RootGuess {
        RootGuess {
            root: Some(PathBuf::from(path)),
            source,
        }
    }

    #[test]
    fn engine_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let cases: &[(&str, Engine)] = &[
            ("\\documentclass{article}\n", Engine::PdfLatex),
            (
                "% !TEX program = lualatex\n\\documentclass{article}\n",
                Engine::LuaLatex,
            ),
            ("%!TeX TS-program = xelatex\n", Engine::XeLatex),
            (
                "% !TEX program = pdflatex\n\\usepackage{fontspec}\n",
                Engine::PdfLatex,
            ),
            ("% !tex program = LuaLaTeX\n", Engine::LuaLatex),
            ("\\usepackage{fontspec}\n", Engine::LuaLatex),
            (
                "\\usepackage[math-style=ISO]{unicode-math}\n",
                Engine::LuaLatex,
            ),
            ("\\usepackage{amsmath, polyglossia}\n", Engine::LuaLatex),
            ("% \\usepackage{fontspec}\n", Engine::PdfLatex),
            ("\\usepackage{fontspecx}\n", Engine::PdfLatex),
            (
                "% !TEX program = latexmk\n\\usepackage{fontspec}\n",
                Engine::LuaLatex,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(detect_engine(text, d), *want, "{text:?}");
        }
    }

    #[test]
    fn latexmkrc_beats_packages_but_not_magic() {
        for rc in ["latexmkrc", ".latexmkrc"] {
            let dir = tempfile::tempdir().expect("tempdir");
            fs::write(dir.path().join(rc), "$pdf_mode = 4;\n").expect("write rc");
            assert_eq!(
                detect_engine("\\usepackage{fontspec}\n", dir.path()),
                Engine::Latexmkrc
            );
            assert_eq!(
                detect_engine("% !TEX program = xelatex\n", dir.path()),
                Engine::XeLatex
            );
        }
    }

    #[test]
    fn root_table() {
        let none = ProjectIndex::default();
        let doc = "\\documentclass{article}\n\\begin{document}\nx\n\\end{document}\n";
        let cases: &[(&str, &str, Option<&str>, RootGuess)] = &[
            (
                "/p/ch/a.tex",
                "% !TEX root = ../main.tex\n\\section{A}\n",
                None,
                found("/p/main.tex", RootSource::MagicComment),
            ),
            (
                "/p/ch/a.tex",
                "%!TeX root=../main\n",
                None,
                found("/p/main.tex", RootSource::MagicComment),
            ),
            (
                "/p/ch/a.tex",
                // The magic comment beats a complete document.
                &format!("% !TeX root = ../thesis.tex\n{doc}"),
                None,
                found("/p/thesis.tex", RootSource::MagicComment),
            ),
            (
                "/p/ch/a.tex",
                "\\documentclass[../main.tex]{subfiles}\n\\begin{document}\nx\n\\end{document}\n",
                None,
                found("/p/main.tex", RootSource::Subfiles),
            ),
            (
                "/p/ch/a.tex",
                "\\documentclass[../main]{subfiles}\n",
                None,
                found("/p/main.tex", RootSource::Subfiles),
            ),
            (
                "/p/./main.tex",
                doc,
                None,
                found("/p/main.tex", RootSource::SelfDocument),
            ),
            (
                "/p/a.tex",
                "% \\documentclass{article}\n\\begin{document}\n",
                Some("/p/main.tex"),
                found("/p/main.tex", RootSource::Remembered),
            ),
            (
                "/p/ch/a.tex",
                "\\section{A}\n",
                Some("/p/main.tex"),
                found("/p/main.tex", RootSource::Remembered),
            ),
            (
                // The remembered root belongs to another project.
                "/q/notes.tex",
                "\\section{A}\n",
                Some("/p/main.tex"),
                RootGuess {
                    root: None,
                    source: RootSource::Unknown,
                },
            ),
            (
                "/p/a.tex",
                "\\section{A}\n",
                None,
                RootGuess {
                    root: None,
                    source: RootSource::Unknown,
                },
            ),
        ];
        for (file, text, remembered, want) in cases {
            assert_eq!(
                root(file, text, &none, *remembered),
                *want,
                "{file} {text:?}"
            );
        }
    }

    #[test]
    fn root_from_include_graph() {
        use IncludeKind::*;
        let idx = index(&[
            (
                "/p/main.tex",
                true,
                &[(Input, "chapters/intro"), (Graphics, "fig/a")],
            ),
            (
                "/p/chapters/intro.tex",
                false,
                &[(Include, "chapters/sub/deep")],
            ),
            ("/p/chapters/sub/deep.tex", false, &[]),
            ("/p/other.tex", true, &[(Input, "chapters/sub/deep.tex")]),
            ("/p/fig/a.tex", false, &[]),
            ("/q/rel.tex", true, &[(Subfile, "parts/x")]),
            ("/q/parts/x.tex", false, &[(Input, "y")]),
            ("/q/parts/y.tex", false, &[]),
        ]);
        let text = "\\section{Deep}\n";
        assert_eq!(
            root("/p/chapters/intro.tex", text, &idx, None),
            found("/p/main.tex", RootSource::IncludeGraph)
        );
        // Two documents include it: the sorted first, unless the remembered one is among them.
        assert_eq!(
            root("/p/chapters/sub/deep.tex", text, &idx, None),
            found("/p/main.tex", RootSource::IncludeGraph)
        );
        assert_eq!(
            root("/p/chapters/sub/deep.tex", text, &idx, Some("/p/other.tex")),
            found("/p/other.tex", RootSource::IncludeGraph)
        );
        // \includegraphics is not an inclusion.
        assert_eq!(
            root("/p/fig/a.tex", text, &idx, None).source,
            RootSource::Unknown
        );
        // Targets relative to the including file's directory are found too.
        assert_eq!(
            root("/q/parts/y.tex", text, &idx, None),
            found("/q/rel.tex", RootSource::IncludeGraph)
        );
    }

    #[test]
    fn root_from_previous_build_fls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = lemma_core::canonicalize(dir.path()).expect("canonical tempdir");
        fs::create_dir_all(p.join(".build")).expect("mkdir");
        fs::create_dir_all(p.join("chapters")).expect("mkdir");
        fs::write(p.join("thesis.tex"), "\\documentclass{book}").expect("write");
        fs::write(p.join("chapters/one.tex"), "\\chapter{One}").expect("write");
        fs::write(p.join("chapters/two.tex"), "\\chapter{Two}").expect("write");
        let fls = format!(
            "PWD {}\nINPUT /usr/share/texmf-dist/tex/latex/base/book.cls\nINPUT thesis.tex\n\
             OUTPUT .build/thesis.log\nINPUT ./chapters/one.tex\n",
            p.display()
        );
        fs::write(p.join(".build/thesis.fls"), fls).expect("write fls");
        let idx = ProjectIndex::default();
        assert_eq!(
            root(
                &p.join("chapters/one.tex").to_string_lossy(),
                "",
                &idx,
                None
            ),
            RootGuess {
                root: Some(p.join("thesis.tex")),
                source: RootSource::Fls
            }
        );
        let two = root(
            &p.join("chapters/two.tex").to_string_lossy(),
            "",
            &idx,
            None,
        );
        assert_eq!(two.source, RootSource::Unknown);
    }
}
