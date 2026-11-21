use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Symbolic links followed by hand (for dangling links) before giving up, like Linux's ELOOP limit.
const MAX_SYMLINK_HOPS: u32 = 40;

/// Why a path was refused by [`ProjectRoot::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The path resolves outside the project root (carries the resolved path where known).
    OutsideProject(PathBuf),
    /// The file system could not resolve the path.
    Io(String),
}

impl PathError {
    /// The message goes to the agent whose path was refused, so it names the path as the agent
    /// is shown paths ([`agent_path`]).
    fn io(path: &Path, err: impl fmt::Display) -> Self {
        Self::Io(format!("{}: {err}", agent_path(path)))
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideProject(p) => write!(f, "path outside project root: {}", p.display()),
            Self::Io(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for PathError {}

/// Canonical project root. Every agent-supplied path goes through `resolve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRoot(PathBuf);

impl ProjectRoot {
    /// Canonicalizes `dir`, which must be an existing directory.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let root = canonicalize(dir)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Self(root))
    }

    /// The canonical root directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Relative paths are joined to the root. Canonicalizes (the parent dir if the file does not exist yet,
    /// rejecting `..` components that escape). Err(OutsideProject) unless the result is under the root.
    ///
    /// Missing directories are allowed (`new/dir/file.tex`), but a `..` inside the missing part is
    /// refused because the OS could not resolve it either. (Windows resolves `..` by the names
    /// alone, before it looks at the disk, so there `new/../file.tex` is `file.tex`.) Symbolic links, including dangling ones,
    /// are followed, so a link that points outside the root is refused.
    pub fn resolve(&self, p: &Path) -> Result<PathBuf, PathError> {
        let resolved = canonicalize_lenient(&self.0.join(p), 0)?;
        if below(&self.0, &resolved).is_some() {
            Ok(resolved)
        } else {
            Err(PathError::OutsideProject(resolved))
        }
    }

    /// `p` relative to the root, if it lies under it. Purely lexical: `p` should already be
    /// canonical (e.g. returned by [`resolve`](Self::resolve)).
    pub fn relative<'a>(&self, p: &'a Path) -> Option<&'a Path> {
        below(&self.0, p)
    }

    /// The name of `p` in what Lemma tells the agent (tool replies, review feedback, prompts):
    /// relative to the root when `p` lies under it, else whole, and with `/` between its names
    /// on every system ([`agent_path`]). `p` should be canonical, as for
    /// [`relative`](Self::relative). [`resolve`](Self::resolve) takes the name back.
    pub fn agent_name(&self, p: &Path) -> String {
        agent_path(self.relative(p).unwrap_or(p))
    }
}

/// `path` as Lemma writes it for the agent: with `/` between its names on every system
/// (`chapters/intro.tex`, `C:/Users/u/paper/main.tex`, `//server/share/main.tex`). Windows
/// writes `\` there, which LaTeX reads as the start of a command when the agent copies the path
/// into `\input{…}`; and Windows takes `/` wherever it takes `\`, so a path written this way
/// still names the file when the agent hands it back.
///
/// What the user sees in Lemma's own interface, and what Lemma writes into its own files, is
/// not written this way: those keep the system's spelling (`Path::display`).
///
/// On Windows one kind of path is left as it is: a `\\?\` path, which [`canonicalize`] keeps
/// for a file that the plain path would not name. Windows reads such a path literally, so with
/// `/` it would name nothing. Elsewhere `\` is an ordinary character of a file name, and
/// nothing changes.
pub fn agent_path(path: &Path) -> String {
    let shown = path.display().to_string();
    #[cfg(windows)]
    let shown = windows::slashed(path, shown);
    shown
}

/// `text` with the system's spelling of `path` replaced by [`agent_path`]'s: for a message
/// that the operating system or a library wrote, which names the file as the system does, before
/// it goes to the agent. Nothing else in `text` changes (it may quote LaTeX, whose `\` stays).
pub fn agent_text(text: &str, path: &Path) -> String {
    let native = path.display().to_string();
    let shown = agent_path(path);
    if native == shown || native.is_empty() {
        text.to_owned()
    } else {
        text.replace(&native, &shown)
    }
}

/// `p` relative to `root`, if it lies under it (both canonical). On Windows a file whose path is
/// too long for the plain form keeps `\\?\` ([`canonicalize`]) while its root is plain: the same
/// directory written two ways, which `Path::strip_prefix` takes for two disks, so that the file
/// would count as outside its own project. There the root's `\\?\` form is tried too.
fn below<'a>(root: &Path, p: &'a Path) -> Option<&'a Path> {
    if let Ok(rest) = p.strip_prefix(root) {
        return Some(rest);
    }
    #[cfg(windows)]
    if let Some(verbatim) = windows::verbatim(root)
        && let Ok(rest) = p.strip_prefix(&verbatim)
    {
        return Some(rest);
    }
    None
}

/// `fs::canonicalize`, except that on Windows the result is a plain path (`C:\…`,
/// `\\server\share\…`) rather than the `\\?\` form `fs::canonicalize` returns, which TeX, many
/// programs and comparisons with GTK's paths do not take. The `\\?\` form is kept only where the
/// plain one would name another file or none (longer than `MAX_PATH`, or a component that Windows
/// would change, such as a trailing dot or a device name like `CON`).
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    #[cfg(windows)]
    let canonical = windows::plain(canonical);
    Ok(canonical)
}

/// Like `fs::canonicalize`, but the trailing components may not exist yet. `path` is absolute.
fn canonicalize_lenient(path: &Path, hops: u32) -> Result<PathBuf, PathError> {
    match canonicalize(path) {
        Ok(canonical) => return Ok(canonical),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(PathError::io(path, e)),
    }

    // Split into the deepest prefix that exists (without following a final symlink) and the
    // missing tail. `components()` already drops `.`; the root component always exists.
    let components: Vec<Component<'_>> = path.components().collect();
    let mut split = components.len();
    let (existing, meta) = loop {
        let prefix: PathBuf = components[..split].iter().collect();
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => break (prefix, meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound && split > 1 => split -= 1,
            Err(e) => return Err(PathError::io(&prefix, e)),
        }
    };
    let mut tail: Vec<OsString> = Vec::with_capacity(components.len() - split);
    for component in &components[split..] {
        match component {
            Component::Normal(name) => tail.push(name.to_os_string()),
            _ => return Err(PathError::OutsideProject(path.to_path_buf())),
        }
    }

    let base = match canonicalize(&existing) {
        Ok(base) => base,
        Err(e) if e.kind() == io::ErrorKind::NotFound && meta.is_symlink() => {
            // A dangling symlink: creating the file would follow it, so resolve its target.
            if hops >= MAX_SYMLINK_HOPS {
                return Err(PathError::io(path, "too many levels of symbolic links"));
            }
            let target = fs::read_link(&existing).map_err(|e| PathError::io(&existing, e))?;
            let dir = existing.parent().unwrap_or(Path::new("/"));
            let mut next = dir.join(target);
            next.extend(&tail);
            return canonicalize_lenient(&next, hops + 1);
        }
        Err(e) => return Err(PathError::io(&existing, e)),
    };
    let mut resolved = base;
    resolved.extend(&tail);
    Ok(resolved)
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::path::{Component, PathBuf, Prefix};

    /// Paths of this length or more, in UTF-16 units, need the `\\?\` form.
    const MAX_PATH: usize = 260;

    /// `path` without its `\\?\` prefix when the plain form names the same file (see
    /// [`super::canonicalize`]).
    pub(super) fn plain(path: PathBuf) -> PathBuf {
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return path;
        };
        let Some(text) = path.to_str() else {
            return path;
        };
        let plain = match prefix.kind() {
            Prefix::VerbatimDisk(_) => text[4..].to_owned(),
            Prefix::VerbatimUNC(..) => format!(r"\\{}", &text[8..]),
            _ => return path,
        };
        // Not `OsStr::len`: that counts the bytes of the WTF-8 form, two or three for each
        // non-ASCII letter, where MAX_PATH counts UTF-16 units.
        let short = plain.encode_utf16().count() < MAX_PATH;
        let plain = PathBuf::from(plain);
        let names_alike = plain.components().all(|c| match c {
            Component::Normal(name) => win32_name(name),
            _ => true,
        });
        if short && names_alike { plain } else { path }
    }

    /// `shown`, the text of `path`, with `/` for every `\` (see [`super::agent_path`]); a
    /// `\\?\` path is returned as it is.
    pub(super) fn slashed(path: &std::path::Path, shown: String) -> String {
        let verbatim = matches!(
            path.components().next(),
            Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim()
        );
        if verbatim {
            shown
        } else {
            shown.replace('\\', "/")
        }
    }

    /// The `\\?\` form of the plain path `path` (`C:\…`, `\\server\share\…`); `None` for a path
    /// that is not plain or not Unicode.
    pub(super) fn verbatim(path: &std::path::Path) -> Option<PathBuf> {
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return None;
        };
        let text = path.to_str()?;
        match prefix.kind() {
            Prefix::Disk(_) => Some(PathBuf::from(format!(r"\\?\{text}"))),
            Prefix::UNC(..) => Some(PathBuf::from(format!(r"\\?\UNC\{}", &text[2..]))),
            _ => None,
        }
    }

    /// Whether the Win32 path parser leaves the file name `name` as it is: not empty, no
    /// characters it refuses, no trailing dot or space, and not a device name (`CON`, `NUL.txt`,
    /// `COM1`, …).
    fn win32_name(name: &std::ffi::OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return false;
        };
        if name.is_empty()
            || name.ends_with(['.', ' '])
            || name.chars().any(|c| c < ' ' || r#"<>:"/\|?*"#.contains(c))
        {
            return false;
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end();
        let upper: OsString = stem.to_ascii_uppercase().into();
        let upper = upper.to_string_lossy();
        let device = matches!(
            &*upper,
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.chars().count() == 4
            && upper
                .chars()
                .nth(3)
                .is_some_and(|d| "0123456789¹²³".contains(d)));
        !device
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        #[test]
        fn verbatim_prefixes_are_dropped_when_the_plain_path_is_the_same() {
            assert_eq!(
                plain(PathBuf::from(r"\\?\C:\Users\u\thesis")),
                Path::new(r"C:\Users\u\thesis")
            );
            assert_eq!(
                plain(PathBuf::from(r"\\?\UNC\server\share\x")),
                Path::new(r"\\server\share\x")
            );
            assert_eq!(
                plain(PathBuf::from(r"C:\already\plain")),
                Path::new(r"C:\already\plain")
            );
            for kept in [
                r"\\?\C:\a\con",
                r"\\?\C:\a\nul.tex",
                r"\\?\C:\a\dot.",
                r"\\?\C:\a\COM1",
            ] {
                assert_eq!(plain(PathBuf::from(kept)), Path::new(kept));
            }
            let long = format!(r"\\?\C:\{}", "x".repeat(300));
            assert_eq!(plain(PathBuf::from(&long)), Path::new(&long));
            // MAX_PATH counts UTF-16 units: 200 accented letters are 400 bytes of UTF-8 but fit.
            let accented = "é".repeat(200);
            assert_eq!(
                plain(PathBuf::from(format!(r"\\?\C:\{accented}"))),
                Path::new(&format!(r"C:\{accented}"))
            );
            let long = format!(r"\\?\C:\{}", "é".repeat(300));
            assert_eq!(plain(PathBuf::from(&long)), Path::new(&long));
        }

        /// A file too deep for the plain form keeps `\\?\` while its root is plain: it still lies
        /// under the root, and its relative path is the same.
        #[test]
        fn a_verbatim_path_lies_under_its_plain_root() {
            use super::super::{ProjectRoot, below};
            assert_eq!(
                verbatim(Path::new(r"C:\p\q")),
                Some(PathBuf::from(r"\\?\C:\p\q"))
            );
            assert_eq!(
                verbatim(Path::new(r"\\server\share\p")),
                Some(PathBuf::from(r"\\?\UNC\server\share\p"))
            );
            assert_eq!(verbatim(Path::new(r"\\?\C:\p")), None);
            assert_eq!(verbatim(Path::new(r"relative\p")), None);
            let deep = format!(r"\\?\C:\p\{}\f.tex", "x".repeat(300));
            let rest = format!(r"{}\f.tex", "x".repeat(300));
            assert_eq!(
                below(Path::new(r"C:\p"), Path::new(&deep)),
                Some(Path::new(&rest))
            );
            assert_eq!(
                below(Path::new(r"C:\p"), Path::new(r"C:\p\f.tex")),
                Some(Path::new("f.tex"))
            );
            let unc = r"\\?\UNC\server\share\p\f.tex";
            assert_eq!(
                below(Path::new(r"\\server\share\p"), Path::new(unc)),
                Some(Path::new("f.tex"))
            );
            // Another directory, another disk.
            assert_eq!(
                below(Path::new(r"C:\p"), Path::new(r"\\?\C:\q\f.tex")),
                None
            );
            assert_eq!(
                below(Path::new(r"C:\p"), Path::new(r"\\?\D:\p\f.tex")),
                None
            );
            let root = ProjectRoot(PathBuf::from(r"C:\p"));
            assert_eq!(root.relative(Path::new(&deep)), Some(Path::new(&rest)));
        }

        /// What the agent is shown has `/` for every `\`, a drive first for an absolute path;
        /// only a `\\?\` path, which Windows reads literally, stays as it is.
        #[test]
        fn paths_for_the_agent_have_forward_slashes() {
            use super::super::{ProjectRoot, agent_path, agent_text};
            for (path, shown) in [
                (r"chapters\intro.tex", "chapters/intro.tex"),
                (r"chapters\two\intro.tex", "chapters/two/intro.tex"),
                ("chapters/intro.tex", "chapters/intro.tex"),
                (r"C:\Users\u\paper\main.tex", "C:/Users/u/paper/main.tex"),
                (r"C:\Users\u\paper/sub\a.tex", "C:/Users/u/paper/sub/a.tex"),
                (r"C:\", "C:/"),
                (
                    r"\\server\share\paper\main.tex",
                    "//server/share/paper/main.tex",
                ),
                (r"..\notes.tex", "../notes.tex"),
            ] {
                assert_eq!(agent_path(Path::new(path)), shown, "{path}");
                // The same file, whichever way it is written.
                assert_eq!(Path::new(path), Path::new(shown), "{path}");
            }
            for kept in [r"\\?\C:\a\con", r"\\?\UNC\server\share\a.tex"] {
                assert_eq!(agent_path(Path::new(kept)), kept);
            }
            let root = ProjectRoot(PathBuf::from(r"C:\Users\u\paper"));
            assert_eq!(
                root.agent_name(Path::new(r"C:\Users\u\paper\chapters\intro.tex")),
                "chapters/intro.tex"
            );
            assert_eq!(
                root.agent_name(Path::new(r"C:\Users\u\other\main.tex")),
                "C:/Users/u/other/main.tex"
            );
            // A file too deep for the plain form: its name below the root has no prefix.
            let deep = format!(r"\\?\C:\Users\u\paper\{}\f.tex", "x".repeat(300));
            assert_eq!(
                root.agent_name(Path::new(&deep)),
                format!("{}/f.tex", "x".repeat(300))
            );
            // A message of the system's that names the file: only the path changes.
            let file = Path::new(r"C:\Users\u\paper\main.tex");
            assert_eq!(
                agent_text(
                    r"Error opening file “C:\Users\u\paper\main.tex”: denied (\input{a})",
                    file
                ),
                r"Error opening file “C:/Users/u/paper/main.tex”: denied (\input{a})"
            );
        }

        /// The agent may write a path either way, relative or absolute: both name the file.
        #[test]
        fn paths_from_the_agent_may_use_either_slash() {
            use super::super::{ProjectRoot, agent_path};
            let tmp = tempfile::tempdir().unwrap();
            let proj = tmp.path().join("proj");
            std::fs::create_dir_all(proj.join("chapters")).unwrap();
            std::fs::write(proj.join("chapters").join("intro.tex"), "a").unwrap();
            let root = ProjectRoot::new(&proj).unwrap();
            let file = root.path().join("chapters").join("intro.tex");
            let absolute = file.display().to_string();
            assert!(
                absolute.contains('\\') && !absolute.contains('/'),
                "{absolute}"
            );
            let slashed = agent_path(&file);
            assert!(!slashed.contains('\\'), "{slashed}");
            assert_eq!(slashed.as_bytes()[1], b':', "a drive first: {slashed}");
            for written in [
                "chapters/intro.tex",
                r"chapters\intro.tex",
                r".\chapters\..\chapters/intro.tex",
                absolute.as_str(),
                slashed.as_str(),
            ] {
                assert_eq!(
                    root.resolve(Path::new(written)),
                    Ok(file.clone()),
                    "{written}"
                );
            }
            // A file that does not exist yet, in a directory that does not either.
            let new = root.path().join("new").join("dir").join("file.tex");
            for written in ["new/dir/file.tex", r"new\dir\file.tex", r"new/dir\file.tex"] {
                assert_eq!(
                    root.resolve(Path::new(written)),
                    Ok(new.clone()),
                    "{written}"
                );
            }
            assert_eq!(root.agent_name(&new), "new/dir/file.tex");
            // Outside the root, either way.
            for written in [r"..\outside.tex", "../outside.tex", r"C:\Windows\win.ini"] {
                assert!(root.resolve(Path::new(written)).is_err(), "{written}");
            }
        }

        #[test]
        fn canonical_paths_are_plain() {
            let dir = tempfile::tempdir().unwrap();
            let canonical = super::super::canonicalize(dir.path()).unwrap();
            assert!(
                !canonical.to_string_lossy().starts_with(r"\\?\"),
                "{}",
                canonical.display()
            );
            assert!(canonical.is_absolute());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// `<tmp>/proj/{main.tex, sub/a.tex}` and `<tmp>/outside/secret.txt`.
    struct Fixture {
        _tmp: TempDir,
        root: ProjectRoot,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(proj.join("main.tex"), "\\documentclass{article}").unwrap();
        fs::write(proj.join("sub/a.tex"), "a").unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        let root = ProjectRoot::new(&proj).unwrap();
        let outside = canonicalize(outside).unwrap();
        Fixture {
            _tmp: tmp,
            root,
            outside,
        }
    }

    fn refused(r: Result<PathBuf, PathError>) -> bool {
        matches!(r, Err(PathError::OutsideProject(_)))
    }

    #[test]
    fn new_requires_an_existing_directory() {
        let f = fixture();
        assert!(f.root.path().is_absolute());
        assert!(ProjectRoot::new(&f.root.path().join("main.tex")).is_err());
        assert!(ProjectRoot::new(&f.root.path().join("missing")).is_err());
    }

    #[test]
    fn relative_path_is_joined_to_the_root() {
        let f = fixture();
        let root = f.root.path();
        assert_eq!(
            f.root.resolve(Path::new("main.tex")),
            Ok(root.join("main.tex"))
        );
        assert_eq!(
            f.root.resolve(Path::new("./sub/../sub/a.tex")),
            Ok(root.join("sub/a.tex"))
        );
        assert_eq!(f.root.resolve(Path::new(".")), Ok(root.to_path_buf()));
    }

    #[test]
    fn absolute_path_inside_is_accepted() {
        let f = fixture();
        let p = f.root.path().join("sub/a.tex");
        assert_eq!(f.root.resolve(&p), Ok(p.clone()));
    }

    #[test]
    fn dot_dot_escape_is_refused() {
        let f = fixture();
        assert!(refused(f.root.resolve(Path::new("../outside/secret.txt"))));
        assert!(refused(
            f.root.resolve(Path::new("sub/../../outside/new.txt"))
        ));
        assert!(refused(f.root.resolve(Path::new(".."))));
        assert!(refused(f.root.resolve(&f.outside.join("secret.txt"))));
        // `..` inside a directory that does not exist cannot be resolved by the OS either; Windows
        // resolves it by the names alone, which stays inside here.
        #[cfg(unix)]
        assert!(refused(f.root.resolve(Path::new("newdir/../x.tex"))));
        #[cfg(windows)]
        assert_eq!(
            f.root.resolve(Path::new("newdir/../x.tex")),
            Ok(f.root.path().join("x.tex"))
        );
        assert!(refused(f.root.resolve(Path::new("newdir/../../outside/x"))));
    }

    #[test]
    fn sibling_with_common_prefix_is_outside() {
        let f = fixture();
        let sibling = f.root.path().with_file_name("proj2");
        fs::create_dir_all(&sibling).unwrap();
        assert!(refused(f.root.resolve(&sibling.join("x.tex"))));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let f = fixture();
        let root = f.root.path();
        symlink(&f.outside, root.join("link")).unwrap();
        symlink(f.outside.join("secret.txt"), root.join("secret-link.txt")).unwrap();
        assert!(refused(f.root.resolve(Path::new("link/secret.txt"))));
        assert!(refused(f.root.resolve(Path::new("link/new.txt"))));
        assert!(refused(f.root.resolve(Path::new("secret-link.txt"))));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_resolved_to_their_target() {
        let f = fixture();
        let root = f.root.path();
        symlink(f.outside.join("not-yet.txt"), root.join("dangling-out")).unwrap();
        symlink("sub/not-yet.tex", root.join("dangling-in")).unwrap();
        assert!(refused(f.root.resolve(Path::new("dangling-out"))));
        assert_eq!(
            f.root.resolve(Path::new("dangling-in")),
            Ok(root.join("sub/not-yet.tex"))
        );
        symlink("loop-b", root.join("loop-a")).unwrap();
        symlink("loop-a", root.join("loop-b")).unwrap();
        assert!(f.root.resolve(Path::new("loop-a")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_the_root_is_accepted() {
        let f = fixture();
        let root = f.root.path();
        symlink(root.join("sub"), root.join("alias")).unwrap();
        assert_eq!(
            f.root.resolve(Path::new("alias/a.tex")),
            Ok(root.join("sub/a.tex"))
        );
    }

    #[test]
    fn nonexistent_file_inside_is_accepted() {
        let f = fixture();
        let root = f.root.path();
        assert_eq!(
            f.root.resolve(Path::new("new.tex")),
            Ok(root.join("new.tex"))
        );
        assert_eq!(
            f.root.resolve(Path::new("sub/new.tex")),
            Ok(root.join("sub/new.tex"))
        );
        assert_eq!(
            f.root.resolve(Path::new("chapters/two/new.tex")),
            Ok(root.join("chapters/two/new.tex"))
        );
        assert_eq!(
            f.root.resolve(&root.join("abs-new.tex")),
            Ok(root.join("abs-new.tex"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_reached_through_a_symlink_is_canonical() {
        let f = fixture();
        let alias = f.root.path().with_file_name("proj-alias");
        symlink(f.root.path(), &alias).unwrap();
        let via_alias = ProjectRoot::new(&alias).unwrap();
        assert_eq!(via_alias.path(), f.root.path());
        assert_eq!(
            f.root.resolve(&alias.join("main.tex")),
            Ok(f.root.path().join("main.tex"))
        );
    }

    #[test]
    fn relative_strips_the_root() {
        let f = fixture();
        let p = f.root.resolve(Path::new("sub/a.tex")).unwrap();
        assert_eq!(f.root.relative(&p), Some(Path::new("sub/a.tex")));
        assert_eq!(f.root.relative(&f.outside), None);
    }

    /// What the agent is shown is written with `/` on every system, and names the same file
    /// when the agent hands it back.
    #[test]
    fn names_for_the_agent_use_forward_slashes_and_resolve_back() {
        let f = fixture();
        let root = f.root.path();
        // Built name by name, so with the system's own separator between them.
        let nested = Path::new("chapters").join("two").join("intro.tex");
        assert_eq!(agent_path(&nested), "chapters/two/intro.tex");
        assert_eq!(agent_path(Path::new("main.tex")), "main.tex");
        assert_eq!(agent_path(Path::new("sub/a.tex")), "sub/a.tex");
        let file = root.join("sub").join("a.tex");
        assert_eq!(f.root.agent_name(&file), "sub/a.tex");
        assert_eq!(f.root.agent_name(&root.join("main.tex")), "main.tex");
        // The name, and the whole path, lead back to the file.
        for shown in [f.root.agent_name(&file), agent_path(&file)] {
            assert_eq!(
                f.root.resolve(Path::new(&shown)),
                Ok(file.clone()),
                "{shown}"
            );
        }
        // A file outside the root is named whole.
        let secret = f.outside.join("secret.txt");
        let shown = f.root.agent_name(&secret);
        assert_eq!(shown, agent_path(&secret));
        assert!(Path::new(&shown).is_absolute(), "{shown}");
        assert_eq!(Path::new(&shown), secret);
        if cfg!(windows) {
            assert!(!agent_path(&file).contains('\\'), "{}", agent_path(&file));
        } else {
            assert_eq!(agent_path(&file), file.display().to_string());
        }
    }

    /// Where `\` is a character of a file name, it stays one.
    #[cfg(unix)]
    #[test]
    fn a_backslash_in_a_file_name_stays_outside_windows() {
        assert_eq!(agent_path(Path::new("odd\\name.tex")), "odd\\name.tex");
        assert_eq!(agent_path(Path::new("/p/a\\b/c.tex")), "/p/a\\b/c.tex");
        assert_eq!(
            agent_text("cannot read /p/a\\b: denied", Path::new("/p/a\\b")),
            "cannot read /p/a\\b: denied"
        );
    }

    /// A path that cannot be resolved is named in the message as the agent is shown paths.
    #[test]
    fn io_errors_name_the_path_for_the_agent() {
        let f = fixture();
        let through_a_file = f.root.path().join("main.tex").join("x.tex");
        match f.root.resolve(Path::new("main.tex/x.tex")) {
            // Unix: a file is not a directory.
            Err(PathError::Io(message)) => {
                assert!(
                    message.starts_with(&format!("{}: ", agent_path(&through_a_file))),
                    "{message}"
                );
            }
            // Windows finds no such path, which is a file that does not exist yet.
            other => assert_eq!(other, Ok(through_a_file)),
        }
    }

    #[test]
    fn errors_display_their_cause() {
        let e = PathError::OutsideProject(PathBuf::from("/etc/passwd"));
        assert_eq!(e.to_string(), "path outside project root: /etc/passwd");
    }
}
