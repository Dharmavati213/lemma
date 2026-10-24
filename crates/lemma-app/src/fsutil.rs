//! File system helpers whose details differ between Unix and Windows: private files and
//! directories, opening a file that may be a FIFO, and executable files.
//!
//! Windows has no permission bits. Lemma's files there live in the user's profile (`%APPDATA%`,
//! `%LOCALAPPDATA%`), whose access control list already keeps other users out, so "private"
//! adds nothing; there are no FIFOs in the file system, and a file is a program by its extension
//! (`.exe`, `.cmd` and the rest of `PATHEXT`).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// `OpenOptions` whose newly created file only the user can read and write (mode 0600).
pub fn private_file_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// A recursive `DirBuilder` whose new directories only the user can enter (mode 0700).
pub fn private_dir_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
}

/// Opens `path` for reading. On Unix with `O_NONBLOCK`: opening a FIFO would otherwise wait for a
/// writer (it does not change reads from a regular file).
pub fn open_nonblocking(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(path)
}

/// Whether `path` is a file the user may run: one with an execute bit on Unix; on Windows one
/// whose extension is in `PATHEXT` (npm puts a `sh` script without one next to each `.cmd`).
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(windows)]
    {
        has_program_extension(path) && path.is_file()
    }
}

/// Whether the extension of `path` is one of `PATHEXT`'s.
#[cfg(windows)]
fn has_program_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        program_extensions()
            .iter()
            .any(|x| x.eq_ignore_ascii_case(e))
    })
}

/// The program `path` names, if there is one: `path` itself when it is executable, or on Windows
/// (where `node` means `node.exe` and `npm` means `npm.cmd`) `path` with the first extension of
/// `PATHEXT` that names a file, when `path` has none of them.
pub fn program_file(path: &Path) -> Option<PathBuf> {
    if is_executable(path) {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    {
        if !has_program_extension(path) {
            for extension in program_extensions() {
                let mut name = path.as_os_str().to_owned();
                name.push(".");
                name.push(&extension);
                let candidate = PathBuf::from(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// The extensions of programs, from `PATHEXT` (`.COM;.EXE;.BAT;.CMD` when it is unset), without
/// their dots.
#[cfg(windows)]
fn program_extensions() -> Vec<String> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    pathext
        .split(';')
        .map(|e| e.trim().trim_start_matches('.').to_owned())
        .filter(|e| !e.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let tool = dir.path().join("tool");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(program_file(&tool), Some(tool));
            assert_eq!(program_file(&plain), None);
        }
        #[cfg(windows)]
        {
            let tool = dir.path().join("tool.cmd");
            std::fs::write(&tool, "@echo off\r\n").unwrap();
            assert_eq!(program_file(&dir.path().join("tool")), Some(tool.clone()));
            assert_eq!(program_file(&tool), Some(tool));
            assert_eq!(program_file(&plain), None);
            // npm's shell script for Git Bash, next to `tool.cmd`, is not a program here.
            std::fs::write(dir.path().join("tool"), "#!/bin/sh\n").unwrap();
            assert_eq!(
                program_file(&dir.path().join("tool")),
                Some(dir.path().join("tool.cmd"))
            );
        }
        assert_eq!(program_file(&dir.path().join("missing")), None);
    }
}
