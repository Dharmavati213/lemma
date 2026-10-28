//! The copy of a build's PDF next to the root document (`[build] copy_pdf`). latexmk writes the
//! PDF into the output directory (`.build`, hidden), so the PDF that a file manager, a mail
//! attachment or an upload finds beside `main.tex` would otherwise stay whatever was there
//! before: an older build, or another program's.

use crate::paths::normalize;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Puts a copy of the built `pdf` in the directory of `root` (the root document), under the
/// PDF's own name, unless the file there is `pdf` itself (the output directory is the root's)
/// or already a copy of it: the copy takes the PDF's modification time, so the same size and
/// time mean the same PDF. Blocking: call it on the build thread. The copy is written to a
/// hidden file beside it and renamed over it, so a viewer that has it open reloads a whole PDF,
/// never a half-written one. Returns the copy's path when it wrote one.
pub fn copy_pdf_beside_root(pdf: &Path, root: &Path) -> io::Result<Option<PathBuf>> {
    let pdf = normalize(&std::path::absolute(pdf)?);
    let root = normalize(&std::path::absolute(root)?);
    let (Some(dir), Some(name)) = (root.parent(), pdf.file_name()) else {
        return Ok(None);
    };
    let copy = dir.join(name);
    if copy == pdf {
        return Ok(None);
    }
    let source = fs::metadata(&pdf)?;
    let modified = source.modified()?;
    // Follows a symbolic link, so a link to `pdf` is current too.
    let current = fs::metadata(&copy)
        .is_ok_and(|m| m.len() == source.len() && m.modified().ok() == Some(modified));
    if current {
        return Ok(None);
    }
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = dir.join(temp);
    let written = fs::copy(&pdf, &temp)
        .and_then(|_| File::options().write(true).open(&temp))
        .and_then(|file| file.set_modified(modified))
        .and_then(|()| fs::rename(&temp, &copy));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    Ok(Some(copy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn touch(path: &Path, secs: u64) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::new(secs, 250_000_000))
            .unwrap();
    }

    #[test]
    fn copies_the_pdf_beside_the_root_once_per_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main.tex");
        fs::write(&root, "\\documentclass{article}").unwrap();
        fs::create_dir(dir.path().join(".build")).unwrap();
        let pdf = dir.path().join(".build/main.pdf");
        let copy = dir.path().join("main.pdf");
        // An older PDF beside the root (another program's, or an earlier build's).
        fs::write(&copy, "%PDF-1.5 old").unwrap();
        fs::write(&pdf, "%PDF-1.5 new").unwrap();
        touch(&pdf, 1_800_000_000);

        assert_eq!(
            copy_pdf_beside_root(&pdf, &root).unwrap(),
            Some(copy.clone())
        );
        assert_eq!(fs::read(&copy).unwrap(), b"%PDF-1.5 new");
        let times = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(times(&copy), times(&pdf));
        // Nothing to do while the copy is current (a build that found nothing to do).
        assert_eq!(copy_pdf_beside_root(&pdf, &root).unwrap(), None);

        // The next build, with as many bytes.
        fs::write(&pdf, "%PDF-1.5 two").unwrap();
        touch(&pdf, 1_800_000_060);
        assert_eq!(
            copy_pdf_beside_root(&pdf, &root).unwrap(),
            Some(copy.clone())
        );
        assert_eq!(fs::read(&copy).unwrap(), b"%PDF-1.5 two");

        // No temporary file is left behind.
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [".build", "main.pdf", "main.tex"]);
    }

    #[test]
    fn a_pdf_built_beside_the_root_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main.tex");
        let pdf = dir.path().join("main.pdf");
        fs::write(&pdf, "%PDF-1.5").unwrap();
        // outdir = ".": the log names the PDF through the output directory.
        let named = dir.path().join("./main.pdf");
        assert_eq!(copy_pdf_beside_root(&named, &root).unwrap(), None);
        assert_eq!(fs::read(&pdf).unwrap(), b"%PDF-1.5");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_the_built_pdf_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main.tex");
        fs::create_dir(dir.path().join(".build")).unwrap();
        let pdf = dir.path().join(".build/main.pdf");
        fs::write(&pdf, "%PDF-1.5").unwrap();
        let link = dir.path().join("main.pdf");
        std::os::unix::fs::symlink(".build/main.pdf", &link).unwrap();
        assert_eq!(copy_pdf_beside_root(&pdf, &root).unwrap(), None);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_failed_copy_reports_the_error_and_leaves_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("main.tex");
        fs::create_dir(dir.path().join(".build")).unwrap();
        let pdf = dir.path().join(".build/main.pdf");
        fs::write(&pdf, "%PDF-1.5").unwrap();
        // A directory where the copy goes cannot be replaced.
        fs::create_dir(dir.path().join("main.pdf")).unwrap();
        assert!(copy_pdf_beside_root(&pdf, &root).is_err());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
