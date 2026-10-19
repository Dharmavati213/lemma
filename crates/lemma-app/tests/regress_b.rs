//! Regression tests for verified M1 defects in the Problems list's data (review area B) that
//! need no display. Each test fails until its defect is fixed. GTK behaviour is covered by
//! `gtk_regress_b.rs`.

use lemma_app::diagnostics_panel::problems_from;
use lemma_core::{DiagSource, Diagnostic, Severity};
use std::path::{Path, PathBuf};

fn diag(path: &Path, line: Option<u32>, severity: Severity, message: &str) -> Diagnostic {
    Diagnostic {
        path: path.to_path_buf(),
        line,
        severity,
        message: message.into(),
        context: None,
        source: DiagSource::Tex,
    }
}

/// A project directory with `main.tex` and `chapters/a.tex` (canonical paths).
fn project(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress-b-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("chapters")).expect("mkdir");
    std::fs::write(dir.join("main.tex"), "x\n").expect("write");
    std::fs::write(dir.join("chapters/a.tex"), "x\n").expect("write");
    dir.canonicalize().expect("canonical")
}

/// B5: with `[build] outdir = "."` (output next to the sources) every source file lies "under
/// the output directory", so every diagnostic is treated as generated: it loses its line and is
/// moved to the root (no gutter mark, no underline, no line in the list).
#[test]
fn regress_outdir_dot_keeps_lines_of_source_diagnostics() {
    let dir = project("outdir-dot");
    let root = dir.join("main.tex");
    let chapter = dir.join("chapters/a.tex");
    let diagnostics = vec![
        diag(
            &root,
            Some(66),
            Severity::Error,
            "Undefined control sequence.",
        ),
        diag(&chapter, Some(3), Severity::Warning, "Reference undefined"),
    ];
    for outdir in [".", "./"] {
        let problems = problems_from(&diagnostics, &root, outdir);
        let got: Vec<(PathBuf, Option<u32>)> =
            problems.iter().map(|p| (p.path.clone(), p.line)).collect();
        assert_eq!(
            got,
            vec![(root.clone(), Some(66)), (chapter.clone(), Some(3))],
            "outdir {outdir:?}: {problems:#?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// B17: the build-stopping error in an `\input` chapter is listed after all of the root's
/// warnings, so "Show" in the "Build failed: 1 error" toast opens a page whose visible top holds
/// only warnings.
#[test]
fn regress_errors_are_listed_before_other_files_warnings() {
    let dir = project("error-order");
    let root = dir.join("main.tex");
    let chapter = dir.join("chapters/a.tex");
    let mut diagnostics: Vec<Diagnostic> = (1..=26)
        .map(|i| {
            diag(
                &root,
                Some(i * 2),
                Severity::Warning,
                &format!("Citation 'k{i}' undefined"),
            )
        })
        .collect();
    diagnostics.push(diag(
        &chapter,
        Some(5),
        Severity::Error,
        "Undefined control sequence.",
    ));
    let problems = problems_from(&diagnostics, &root, ".build");
    let first_error = problems
        .iter()
        .position(|p| p.severity == Severity::Error)
        .expect("the error is listed");
    assert_eq!(
        first_error, 0,
        "the only error is row {first_error}, after {first_error} warnings"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
