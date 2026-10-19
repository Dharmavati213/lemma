// Owner: WP1.9 structure
//! The project-wide index: every file's structure, the bibliographies and the `.aux` numbers.
//!
//! Path resolution is purely lexical (no file system access): include targets are joined to a
//! directory, `.`/`..` are folded, and when several candidates are possible the one already in
//! [`ProjectIndex::files`] wins.

use crate::{AuxInfo, BibEntry, FileIndex, Include, IncludeKind, Label};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Include chains deeper than this are not followed (a guard against pathological projects).
const MAX_DEPTH: usize = 64;

/// How many directories (the includer's and its ancestors) [`ProjectIndex::includers_of`] tries.
const INCLUDER_ANCESTORS: usize = 6;

/// Lives on the main thread; workers send new [`FileIndex`]es and bib entries to merge in.
#[derive(Clone, Debug, Default)]
pub struct ProjectIndex {
    pub files: BTreeMap<PathBuf, Arc<FileIndex>>,
    pub bibs: BTreeMap<PathBuf, Arc<Vec<BibEntry>>>,
    pub aux: AuxInfo,
}

impl ProjectIndex {
    /// Replaces the index of `path`.
    pub fn update_file(&mut self, path: PathBuf, idx: Arc<FileIndex>) {
        self.files.insert(path, idx);
    }

    /// The file and definition of label `name` (the first by path if it is defined twice).
    pub fn find_label(&self, name: &str) -> Option<(&Path, &Label)> {
        self.files.iter().find_map(|(path, idx)| {
            idx.labels
                .iter()
                .find(|l| l.name == name)
                .map(|l| (path.as_path(), l))
        })
    }

    /// Every label in the project with its file, ordered by path and then by line.
    pub fn all_labels(&self) -> Vec<(&Path, &Label)> {
        self.files
            .iter()
            .flat_map(|(path, idx)| idx.labels.iter().map(move |l| (path.as_path(), l)))
            .collect()
    }

    /// Every bibliography entry in the project, ordered by `.bib` path and then by position.
    pub fn all_bib(&self) -> Vec<&BibEntry> {
        self.bibs
            .values()
            .flat_map(|entries| entries.iter())
            .collect()
    }

    /// Packages loaded anywhere in the project, sorted and without duplicates.
    pub fn packages(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.files.values().flat_map(|f| &f.packages).collect();
        set.into_iter().cloned().collect()
    }

    /// Theorem-like environments declared anywhere in the project (`\newtheorem`,
    /// `\declaretheorem`), sorted and without duplicates; for [`crate::index_file_with_theorems`].
    pub fn theorem_names(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self
            .files
            .values()
            .flat_map(|f| f.macros.iter().filter(|m| m.is_theorem).map(|m| &m.name))
            .collect();
        set.into_iter().cloned().collect()
    }

    /// Files reachable from `root` through \input/\include/\subfile/\import (resolved relative to root dir, .tex implied).
    ///
    /// The result starts with `root` and lists every file once, in document order (depth first).
    /// Files that are not indexed yet are listed but not followed. As in LaTeX with the `import`
    /// package, `\subfile` and `\import` targets are relative to the including file's import
    /// directory (the root's directory at top level), which then becomes the base for that file's
    /// own includes; the root's directory is the fallback.
    pub fn project_files(&self, root: &Path) -> Vec<PathBuf> {
        let root_dir = root.parent().map(normalize).unwrap_or_default();
        let mut out = vec![root.to_path_buf()];
        let mut seen = HashSet::from([root.to_path_buf(), normalize(root)]);
        self.visit(root, &root_dir, &root_dir, &mut seen, &mut out, 0);
        out
    }

    fn visit(
        &self,
        file: &Path,
        base: &Path,
        root_dir: &Path,
        seen: &mut HashSet<PathBuf>,
        out: &mut Vec<PathBuf>,
        depth: usize,
    ) {
        let Some(idx) = self.files.get(file) else {
            return;
        };
        if depth >= MAX_DEPTH {
            return;
        }
        for inc in idx.includes.iter().filter(|i| is_tex_include(i.kind)) {
            let names = candidate_names(inc);
            let path = self.pick(&names, &[base, root_dir]);
            if !seen.insert(path.clone()) {
                continue;
            }
            let child_base = match inc.kind {
                IncludeKind::Subfile | IncludeKind::Import => {
                    path.parent().map(Path::to_path_buf).unwrap_or_default()
                }
                _ => base.to_path_buf(),
            };
            out.push(path.clone());
            self.visit(&path, &child_base, root_dir, seen, out, depth + 1);
        }
    }

    /// The first `dir/name` that is indexed, else the first candidate.
    fn pick(&self, names: &[String], dirs: &[&Path]) -> PathBuf {
        let mut first = None;
        for dir in dirs {
            for name in names {
                let path = normalize(&dir.join(name));
                if self.files.contains_key(&path) {
                    return path;
                }
                first.get_or_insert(path);
            }
        }
        first.unwrap_or_default()
    }

    /// Files whose include list contains `file` (reverse include graph).
    ///
    /// Every include kind counts (`\bibliography{refs}` includes `refs.bib`, `\usepackage{x}`
    /// includes `x.sty`). Since the root is unknown here, a target is tried relative to the
    /// includer's directory and a few of its ancestors.
    pub fn includers_of(&self, file: &Path) -> Vec<PathBuf> {
        let file = normalize(file);
        self.files
            .iter()
            .filter(|(path, _)| normalize(path) != file)
            .filter(|(path, idx)| {
                let dir = path.parent().unwrap_or(Path::new(""));
                idx.includes.iter().any(|inc| {
                    candidate_names(inc).iter().any(|name| {
                        dir.ancestors()
                            .take(INCLUDER_ANCESTORS)
                            .any(|d| normalize(&d.join(name)) == file)
                    })
                })
            })
            .map(|(path, _)| path.clone())
            .collect()
    }
}

fn is_tex_include(kind: IncludeKind) -> bool {
    matches!(
        kind,
        IncludeKind::Input | IncludeKind::Include | IncludeKind::Subfile | IncludeKind::Import
    )
}

/// The file names an include target may refer to, most likely first.
fn candidate_names(inc: &Include) -> Vec<String> {
    let t = inc.target.as_str();
    let ext = Path::new(t)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match (inc.kind, ext.as_deref()) {
        (IncludeKind::Bibliography, Some("bib")) => vec![t.to_owned()],
        (IncludeKind::Bibliography, _) => vec![format!("{t}.bib"), t.to_owned()],
        (IncludeKind::Package, _) => vec![format!("{t}.sty")],
        (IncludeKind::Graphics, Some(_)) => vec![t.to_owned()],
        (IncludeKind::Graphics, None) => ["pdf", "png", "jpg", "jpeg", "eps"]
            .iter()
            .map(|e| format!("{t}.{e}"))
            .collect(),
        (_, Some("tex")) => vec![t.to_owned()],
        (_, None) | (IncludeKind::Include, _) => vec![format!("{t}.tex")],
        (_, Some(_)) => vec![t.to_owned(), format!("{t}.tex")],
    }
}

/// Folds `.` and `..` components without touching the file system.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("../../a/..")), PathBuf::from("../.."));
        assert_eq!(normalize(Path::new("a/b/../../..")), PathBuf::from(".."));
    }

    #[test]
    fn candidates_by_kind() {
        let inc = |kind, target: &str| Include {
            kind,
            target: target.to_owned(),
            line: 0,
        };
        assert_eq!(candidate_names(&inc(IncludeKind::Input, "a")), ["a.tex"]);
        assert_eq!(
            candidate_names(&inc(IncludeKind::Input, "a.TEX")),
            ["a.TEX"]
        );
        assert_eq!(
            candidate_names(&inc(IncludeKind::Input, "a.tikz")),
            ["a.tikz", "a.tikz.tex"]
        );
        assert_eq!(
            candidate_names(&inc(IncludeKind::Include, "ch.1")),
            ["ch.1.tex"]
        );
        assert_eq!(
            candidate_names(&inc(IncludeKind::Bibliography, "refs")),
            ["refs.bib", "refs"]
        );
        assert_eq!(
            candidate_names(&inc(IncludeKind::Package, "defs")),
            ["defs.sty"]
        );
        assert_eq!(
            candidate_names(&inc(IncludeKind::Graphics, "f.png")),
            ["f.png"]
        );
    }
}
