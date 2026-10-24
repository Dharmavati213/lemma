//! `lemma_build::chktex` with the real chktex on the fixtures (WP3.7): the parsed warnings of
//! every distinct fixture `.tex` file are a golden (`snapshots/chktex__fixtures.snap`); `%
//! chktex N` comments silence a warning; a missing program is `RunError::NotFound`. Every test
//! prints `SKIP` and passes when chktex is not installed.
//!
//! chktex runs with `HOME` and `XDG_CONFIG_HOME` pointing at an empty directory, so a user's
//! `~/.chktexrc` cannot change the golden; it depends on chktex 1.7.9 and the system
//! `chktexrc` of TeX Live.

use lemma_build::chktex::{self, DEFAULT_SILENCED, LintKind, RunError};
use lemma_core::{DiagSource, fnv1a64};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have_chktex() -> bool {
    let found = Command::new("chktex")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("ChkTeX"));
    if !found {
        eprintln!("SKIP: chktex not found");
    }
    found
}

fn fixtures() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    lemma_core::canonicalize(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

/// chktex on `text` in `cwd`, isolated from the user's configuration.
fn lint(text: &str, cwd: &Path, name: &str) -> Result<Vec<chktex::LintWarning>, RunError> {
    let home = tempfile::tempdir().unwrap();
    let mut command = chktex::command(Path::new("chktex"), cwd, name);
    command
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("CHKTEXRC");
    chktex::run_command(command, text)
}

/// Every `.tex` file of the fixtures except `large.tex`, without duplicates (the log variants
/// share most of their sources), sorted.
fn fixture_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "tex")
                && path.file_name().is_some_and(|n| n != "large.tex")
            {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&fixtures(), &mut files);
    files.sort();
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(fnv1a64(&[&fs::read(f).unwrap()])));
    files
}

#[test]
fn golden_of_the_fixtures() {
    if !have_chktex() {
        return;
    }
    let root = fixtures();
    let mut out = String::new();
    let mut silenced: BTreeMap<u32, usize> = BTreeMap::new();
    let mut total = 0;
    for file in fixture_files() {
        let text = fs::read_to_string(&file).unwrap();
        let rel = file.strip_prefix(&root).unwrap().display().to_string();
        let warnings = lint(&text, file.parent().unwrap(), &rel).expect("chktex runs");
        total += warnings.len();
        let lines: Vec<&str> = text.split('\n').collect();
        for w in &warnings {
            let shown = chktex::is_shown(w.number, &[], &[]);
            if !shown {
                *silenced.entry(w.number).or_default() += 1;
            }
            // The underlined text, to check the byte columns.
            let marked = lines
                .get(w.line as usize - 1)
                .and_then(|l| w.byte_range(l).map(|r| l[r].to_owned()))
                .map_or_else(|| "(file)".to_owned(), |t| format!("{t:?}"));
            let kind = match w.kind {
                LintKind::Warning => "W",
                LintKind::Error => "E",
                LintKind::Message => "M",
            };
            let hidden = if shown { "" } else { " (silenced)" };
            writeln!(
                out,
                "{rel}:{}:{} {kind}{}{hidden} {marked} {}",
                w.line, w.column, w.number, w.message
            )
            .unwrap();
        }
    }
    writeln!(
        out,
        "-- {total} warnings; silenced by default: {silenced:?}"
    )
    .unwrap();
    insta::assert_snapshot!("fixtures", out);
}

#[test]
fn byte_columns_with_tabs_and_utf8() {
    if !have_chktex() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let text = "\tÜnï (a)x and ...\n";
    let warnings = lint(text, dir.path(), "x.tex").unwrap();
    let line = text.lines().next().unwrap();
    let marked: Vec<(u32, &str)> = warnings
        .iter()
        .map(|w| (w.number, &line[w.byte_range(line).unwrap()]))
        .collect();
    assert!(marked.contains(&(36, "x")), "{marked:?}");
    assert!(marked.contains(&(11, "...")), "{marked:?}");
}

#[test]
fn comments_silence_warnings() {
    if !have_chktex() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let numbers = |text: &str| -> Vec<u32> {
        lint(text, dir.path(), "x.tex")
            .unwrap()
            .iter()
            .map(|w| w.number)
            .collect()
    };
    // Every warning of a line, silenced one after the other.
    let mut line = "See \\cite{x}, and so on ... here".to_owned();
    let mut found = numbers(&format!("{line}\n"));
    found.sort_unstable();
    found.dedup();
    assert!(found.len() >= 2, "{found:?}");
    for n in &found {
        let edit = chktex::suppress_on_line(&line, 0, *n).unwrap();
        edit.apply(&mut line);
        assert!(!numbers(&format!("{line}\n")).contains(n), "{line}");
        assert_eq!(chktex::suppress_on_line(&line, 0, *n), None);
    }
    assert!(numbers(&format!("{line}\n")).is_empty(), "{line}");
    assert!(line.starts_with("See \\cite{x}, and so on ... here % chktex "));
    // A second warning on a line that already silences one.
    let two = "Say \"q\" here % chktex 2\n";
    let warnings = lint(two, dir.path(), "x.tex").unwrap();
    assert!(
        !warnings.is_empty() && warnings.iter().all(|w| w.number == 18),
        "{warnings:?}"
    );
    let edit = chktex::suppress_on_line(two.trim_end(), 0, 18).unwrap();
    let mut both = two.trim_end().to_owned();
    edit.apply(&mut both);
    assert_eq!(both, "Say \"q\" here % chktex 2 chktex 18");
    assert!(numbers(&format!("{both}\n")).is_empty());
}

#[test]
fn diagnostics_and_a_missing_program() {
    let dir = tempfile::tempdir().unwrap();
    let missing = chktex::run(
        Path::new("/nonexistent/chktex-for-lemma"),
        "x",
        dir.path(),
        "x.tex",
    );
    assert!(matches!(missing, Err(RunError::NotFound)), "{missing:?}");
    if !have_chktex() {
        return;
    }
    let warnings = lint("so on ... here\n", dir.path(), "x.tex").unwrap();
    let d = chktex::to_diagnostic(&warnings[0], Path::new("/p/x.tex"));
    assert_eq!(d.source, DiagSource::Lint);
    assert_eq!(
        chktex::code_of(d.context.as_deref()),
        Some(warnings[0].number)
    );
    assert!(DEFAULT_SILENCED.windows(2).all(|w| w[0] < w[1]));
}
