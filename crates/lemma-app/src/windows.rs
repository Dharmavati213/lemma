//! Windows: the process environment, set by `main` before any other thread exists.
//!
//! `lemma.exe` is a GUI program (`windows_subsystem = "windows"`), so Windows gives it no
//! console of its own. Two things are set up here:
//!
//! - **The console of a terminal that started Lemma.** [`prepare_environment`] attaches to it,
//!   so `lemma --help`, `--version` and the log (`LEMMA_LOG`) appear there. Started from the
//!   Start menu or Explorer there is none, and nothing is printed.
//! - **PATH** gets the usual directories of the programs Lemma runs, when they exist and are not
//!   on it yet ([`complete_path`]): TeX Live (`C:\texlive\<year>\bin\windows`), MiKTeX (for the
//!   user or for everyone), Strawberry Perl (which MiKTeX's `latexmk` needs), Node.js and npm's
//!   global packages, Git, and `%USERPROFILE%\.local\bin` (Claude Code's installer). Their
//!   installers normally add them to PATH; this covers a Lemma started before Explorer took
//!   the change in.
//!
//! GTK and its libraries find their data by themselves: they live in the bundle's `bin`, and
//! look for `share` and `lib` next to it (see `packaging/windows/`).

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Sets up the environment (see the module docs). Call it first thing in `main`: it changes
/// environment variables, which is only sound while the process has a single thread.
pub fn prepare_environment() {
    attach_parent_console();
    complete_path();
}

/// Attaches to the console of the process that started Lemma, if it has one (a terminal).
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain system call; it fails harmlessly when the parent has no console.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Appends the [usual directories](usual_dirs) that exist and are not on PATH yet to PATH.
fn complete_path() {
    let inherited = env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = env::split_paths(&inherited).collect();
    let known = |dirs: &[PathBuf], dir: &Path| {
        dirs.iter()
            .any(|d| d.as_os_str().eq_ignore_ascii_case(dir.as_os_str()))
    };
    for dir in usual_dirs(&|name| env::var_os(name)) {
        if dir.is_dir() && !known(&dirs, &dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = env::join_paths(&dirs)
        && joined != inherited
    {
        set_var("PATH", joined);
    }
}

/// Where the programs Lemma runs usually are, from the environment `var` (see the module docs).
/// TeX Live's newest year comes first.
fn usual_dirs(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let dir = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let drive = var("SystemDrive")
        .map(|d| PathBuf::from(format!("{}\\", d.to_string_lossy())))
        .unwrap_or_else(|| PathBuf::from(r"C:\"));
    let mut dirs = texlive_bins(&drive.join("texlive"));
    if let Some(local) = dir("LOCALAPPDATA") {
        dirs.push(local.join(r"Programs\MiKTeX\miktex\bin\x64"));
    }
    for programs in [dir("ProgramFiles"), dir("ProgramW6432")]
        .into_iter()
        .flatten()
    {
        dirs.push(programs.join(r"MiKTeX\miktex\bin\x64"));
        dirs.push(programs.join("nodejs"));
        dirs.push(programs.join(r"Git\cmd"));
    }
    dirs.push(drive.join(r"Strawberry\perl\bin"));
    if let Some(roaming) = dir("APPDATA") {
        dirs.push(roaming.join("npm"));
    }
    if let Some(home) = dir("USERPROFILE") {
        dirs.push(home.join(r".local\bin"));
    }
    dirs
}

/// The `bin\windows` directories (`bin\win64` and `bin\win32` before TeX Live 2023) of the TeX
/// Live installations under `texlive` (`C:\texlive\2026`, …), newest year first.
fn texlive_bins(texlive: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(texlive) else {
        return Vec::new();
    };
    let mut years: Vec<(u32, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let year = e.file_name().to_str()?.parse::<u32>().ok()?;
            Some((year, e.path()))
        })
        .collect();
    years.sort_by_key(|(year, _)| std::cmp::Reverse(*year));
    years
        .into_iter()
        .flat_map(|(_, dir)| ["windows", "win64", "win32"].map(|b| dir.join("bin").join(b)))
        .collect()
}

fn set_var(name: &str, value: impl AsRef<OsStr>) {
    // SAFETY: only `prepare_environment` calls this, and `main` calls that before any other
    // thread exists, so nothing reads the environment concurrently.
    unsafe { env::set_var(name, value) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usual_dirs_come_from_the_environment() {
        let vars = |name: &str| -> Option<std::ffi::OsString> {
            match name {
                "SystemDrive" => Some("D:".into()),
                "LOCALAPPDATA" => Some(r"C:\Users\u\AppData\Local".into()),
                "APPDATA" => Some(r"C:\Users\u\AppData\Roaming".into()),
                "ProgramFiles" => Some(r"C:\Program Files".into()),
                "USERPROFILE" => Some(r"C:\Users\u".into()),
                _ => None,
            }
        };
        let dirs = usual_dirs(&vars);
        for expected in [
            r"C:\Users\u\AppData\Local\Programs\MiKTeX\miktex\bin\x64",
            r"C:\Program Files\MiKTeX\miktex\bin\x64",
            r"C:\Program Files\nodejs",
            r"C:\Program Files\Git\cmd",
            r"D:\Strawberry\perl\bin",
            r"C:\Users\u\AppData\Roaming\npm",
            r"C:\Users\u\.local\bin",
        ] {
            assert!(
                dirs.contains(&PathBuf::from(expected)),
                "{expected}: {dirs:?}"
            );
        }
    }

    #[test]
    fn newest_texlive_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        for year in ["2024", "2026", "tlpkg"] {
            std::fs::create_dir(dir.path().join(year)).unwrap();
        }
        let bins = texlive_bins(dir.path());
        assert_eq!(bins[0], dir.path().join(r"2026\bin\windows"));
        assert_eq!(bins[3], dir.path().join(r"2024\bin\windows"));
        assert_eq!(bins.len(), 6);
    }
}
