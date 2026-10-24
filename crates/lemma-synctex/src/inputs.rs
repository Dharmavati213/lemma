//! The `Input:` records of a synctex file and how they map to files on disk.
//!
//! TeX records each input file by the name it was opened with. TeX Live writes
//! `<working directory>/./<name as found by kpathsea>` for files relative to the working
//! directory (`/home/u/paper/./chapters/intro.tex`), and the plain absolute path for files found
//! elsewhere (`/usr/share/texmf-dist/…/amsart.cls`). Other producers may record relative names.
//! The split into working directory and relative part is what lets a moved or copied project
//! still be matched: the relative part is re-anchored at the directory the PDF now lives in.

use std::ffi::CString;
use std::path::{Component, Path, PathBuf};

/// One `Input:` record.
#[derive(Debug)]
pub(crate) struct Input {
    /// The synctex tag of the file.
    pub tag: i32,
    /// The name exactly as recorded; the only form `synctex_display_query` accepts.
    pub name: CString,
    /// The recorded name, lexically normalized (relative names stay relative).
    pub path: PathBuf,
    /// TeX's working directory, when the name has the `<cwd>/./<rel>` form.
    pub cwd: Option<PathBuf>,
    /// The name relative to TeX's working directory: the `<rel>` part, or the whole name when
    /// it was recorded as a relative path.
    pub rel: Option<PathBuf>,
}

impl Input {
    pub(crate) fn new(tag: i32, name: CString) -> Self {
        let bytes = name.as_bytes();
        let recorded = path_from(bytes);
        let (cwd, rel) = if recorded.is_absolute() {
            match find_cwd_mark(bytes) {
                Some(i) => (
                    Some(normalize(&path_from(&bytes[..i]))),
                    Some(normalize(&path_from(&bytes[i + 3..]))),
                ),
                None => (None, None),
            }
        } else {
            (None, Some(normalize(&recorded)))
        };
        Input {
            tag,
            path: normalize(&recorded),
            name,
            cwd,
            rel,
        }
    }
}

/// The path TeX recorded as `bytes`: any bytes on Unix; UTF-8 on Windows, where TeX Live and
/// MiKTeX write file names that way.
pub(crate) fn path_from(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(windows)]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// `path` as the bytes the SyncTeX library takes (`None` when it cannot be one: a NUL in it, or
/// on Windows a name that is not Unicode).
pub(crate) fn c_path(path: &Path) -> Option<CString> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(windows)]
    let bytes = path.to_str()?.as_bytes();
    CString::new(bytes).ok()
}

/// Where the `/./` between TeX's working directory and the relative name starts. On Windows
/// either slash may appear on either side (`C:/paper/./intro.tex`, `C:\paper\.\intro.tex`).
fn find_cwd_mark(bytes: &[u8]) -> Option<usize> {
    let separator = |b: u8| b == b'/' || (cfg!(windows) && b == b'\\');
    bytes
        .windows(3)
        .position(|w| separator(w[0]) && w[1] == b'.' && separator(w[2]))
}

/// Removes `.` components and resolves `..` against the preceding component, without touching
/// the file system. Leading `..` of a relative path are kept; `..` at the root is dropped.
pub(crate) fn normalize(p: &Path) -> PathBuf {
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
            other => out.push(other),
        }
    }
    out
}

/// The number of trailing components `a` and `b` have in common.
pub(crate) fn common_suffix_len(a: &Path, b: &Path) -> usize {
    a.components()
        .rev()
        .zip(b.components().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> Input {
        Input::new(1, CString::new(name).unwrap())
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_are_split_at_the_first_dot_component() {
        // TeX Live: a drive and `/`.
        let i = input("C:/Users/u/paper/./chapters/./intro.tex");
        assert_eq!(i.path, Path::new(r"C:\Users\u\paper\chapters\intro.tex"));
        assert_eq!(i.cwd.as_deref(), Some(Path::new(r"C:\Users\u\paper")));
        assert_eq!(i.rel.as_deref(), Some(Path::new(r"chapters\intro.tex")));
        // Backslashes, and both kinds mixed.
        let i = input(r"D:\Thesis\.\main.tex");
        assert_eq!(i.cwd.as_deref(), Some(Path::new(r"D:\Thesis")));
        assert_eq!(i.rel.as_deref(), Some(Path::new("main.tex")));
        let i = input(r"D:\Thesis/.\ch\a.tex");
        assert_eq!(i.cwd.as_deref(), Some(Path::new(r"D:\Thesis")));
        assert_eq!(i.rel.as_deref(), Some(Path::new(r"ch\a.tex")));
        // A file found elsewhere.
        let i = input("C:/texlive/2026/texmf-dist/tex/latex/amscls/amsart.cls");
        assert_eq!((i.cwd, i.rel), (None, None));
        // Not UTF-8 is read leniently rather than dropped.
        let i = Input::new(1, CString::new(b"C:/x/./caf\xe9.tex".to_vec()).unwrap());
        assert_eq!(i.cwd.as_deref(), Some(Path::new(r"C:\x")));
    }

    #[cfg(unix)]
    #[test]
    fn tex_live_form_is_split_at_the_first_dot_component() {
        let i = input("/home/u/paper/./chapters/./intro.tex");
        assert_eq!(i.path, Path::new("/home/u/paper/chapters/intro.tex"));
        assert_eq!(i.cwd.as_deref(), Some(Path::new("/home/u/paper")));
        assert_eq!(i.rel.as_deref(), Some(Path::new("chapters/intro.tex")));
        assert_eq!(i.name.as_bytes(), b"/home/u/paper/./chapters/./intro.tex");
    }

    #[cfg(unix)]
    #[test]
    fn plain_absolute_name_has_no_working_directory() {
        let i = input("/usr/share/texmf-dist/tex/latex/amscls/amsart.cls");
        assert_eq!(i.cwd, None);
        assert_eq!(i.rel, None);
        assert_eq!(
            i.path,
            Path::new("/usr/share/texmf-dist/tex/latex/amscls/amsart.cls")
        );
    }

    #[test]
    fn relative_name_is_relative_to_the_working_directory() {
        let i = input("./chapters/../sections/a.tex");
        assert_eq!(i.cwd, None);
        assert_eq!(i.rel.as_deref(), Some(Path::new("sections/a.tex")));
        assert_eq!(i.path, Path::new("sections/a.tex"));
        let i = input("../shared/macros.tex");
        assert_eq!(i.rel.as_deref(), Some(Path::new("../shared/macros.tex")));
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(normalize(Path::new("a/../../b")), Path::new("../b"));
        assert_eq!(normalize(Path::new("./a//b/")), Path::new("a/b"));
        assert_eq!(normalize(Path::new(".")), Path::new(""));
    }

    #[test]
    fn suffix_length_counts_whole_components() {
        let a = Path::new("/old/proj/chapters/yoneda.tex");
        assert_eq!(
            common_suffix_len(a, Path::new("/new/proj/chapters/yoneda.tex")),
            3
        );
        assert_eq!(common_suffix_len(a, Path::new("chapters/yoneda.tex")), 2);
        assert_eq!(common_suffix_len(a, Path::new("/x/oneda.tex")), 0);
        assert_eq!(common_suffix_len(a, a), 5);
    }
}
