//! Lexical path helpers. TeX, latexmk and biber print paths relative to the build directory; these
//! turn them into clean paths without touching the file system.

use std::path::{Component, Path, PathBuf};

/// Drops `.` components and folds each `..` into the component before it.
///
/// A leading `..` of a relative path is kept; `/..` is `/`.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::CurDir | Component::ParentDir) | None => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` joined to `base` when relative, then normalized.
pub(crate) fn resolve(base: &Path, path: &Path) -> PathBuf {
    normalize(&base.join(path))
}

/// `path` with `.tex` appended when it has no extension, the way `\input` and `% !TEX root` read
/// a bare name.
pub(crate) fn with_tex_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some() {
        return path;
    }
    let mut name = path.into_os_string();
    name.push(".tex");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(
            normalize(Path::new("/a/./b/../c.tex")),
            Path::new("/a/c.tex")
        );
        assert_eq!(normalize(Path::new("./main.tex")), Path::new("main.tex"));
        assert_eq!(normalize(Path::new("../x/../y")), Path::new("../y"));
        assert_eq!(normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(normalize(Path::new("a/../../b")), Path::new("../b"));
    }

    #[test]
    fn resolve_keeps_absolute_paths() {
        assert_eq!(
            resolve(Path::new("/proj"), Path::new("./chapters/a.tex")),
            Path::new("/proj/chapters/a.tex")
        );
        assert_eq!(
            resolve(Path::new("/proj"), Path::new("/usr/share/x.sty")),
            Path::new("/usr/share/x.sty")
        );
    }

    #[test]
    fn tex_extension_only_for_bare_names() {
        assert_eq!(
            with_tex_extension("../main".into()),
            Path::new("../main.tex")
        );
        assert_eq!(with_tex_extension("main.tex".into()), Path::new("main.tex"));
        assert_eq!(with_tex_extension("fig.tikz".into()), Path::new("fig.tikz"));
    }
}
