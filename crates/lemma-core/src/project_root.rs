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
    fn io(path: &Path, err: impl fmt::Display) -> Self {
        Self::Io(format!("{}: {err}", path.display()))
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
        let root = fs::canonicalize(dir)?;
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
    /// refused because the OS could not resolve it either. Symbolic links, including dangling ones,
    /// are followed, so a link that points outside the root is refused.
    pub fn resolve(&self, p: &Path) -> Result<PathBuf, PathError> {
        let resolved = canonicalize_lenient(&self.0.join(p), 0)?;
        if resolved.starts_with(&self.0) {
            Ok(resolved)
        } else {
            Err(PathError::OutsideProject(resolved))
        }
    }

    /// `p` relative to the root, if it lies under it. Purely lexical: `p` should already be
    /// canonical (e.g. returned by [`resolve`](Self::resolve)).
    pub fn relative<'a>(&self, p: &'a Path) -> Option<&'a Path> {
        p.strip_prefix(&self.0).ok()
    }
}

/// Like `fs::canonicalize`, but the trailing components may not exist yet. `path` is absolute.
fn canonicalize_lenient(path: &Path, hops: u32) -> Result<PathBuf, PathError> {
    match fs::canonicalize(path) {
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

    let base = match fs::canonicalize(&existing) {
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let outside = fs::canonicalize(outside).unwrap();
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
        // `..` inside a directory that does not exist cannot be resolved by the OS either.
        assert!(refused(f.root.resolve(Path::new("newdir/../x.tex"))));
        assert!(refused(f.root.resolve(Path::new("newdir/../../outside/x"))));
    }

    #[test]
    fn sibling_with_common_prefix_is_outside() {
        let f = fixture();
        let sibling = f.root.path().with_file_name("proj2");
        fs::create_dir_all(&sibling).unwrap();
        assert!(refused(f.root.resolve(&sibling.join("x.tex"))));
    }

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

    #[test]
    fn errors_display_their_cause() {
        let e = PathError::OutsideProject(PathBuf::from("/etc/passwd"));
        assert_eq!(e.to_string(), "path outside project root: /etc/passwd");
    }
}
