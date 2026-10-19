//! `parse_log` and `parse_blg` on the real logs in `tests/fixtures/logs/<variant>/` (pdfTeX,
//! LuaTeX, XeTeX, and 79-column `main.79.log` copies), plus `perf_parse`.

use lemma_build::{LogReport, locate_runaways, parse_blg, parse_log};
use lemma_core::{DiagSource, Diagnostic, Severity};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn fixtures() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    fs::canonicalize(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

fn variant_dir(variant: &str) -> PathBuf {
    let logs = fixtures().join("logs").join(variant);
    if logs.is_dir() {
        logs
    } else {
        fixtures().join(variant)
    }
}

/// Files a build writes next to `main.tex`. The fixtures were copied without them, but TeX
/// opened them during the build.
const BUILD_PRODUCTS: &[&str] = &["aux", "bbl", "out", "toc", "lof", "lot", "nav", "snm"];

/// A machine-independent `exists`: project files must exist in the fixture directory; absolute
/// names (the TeX installation) are taken as files, so the results do not depend on the local
/// TeX Live.
fn fixture_exists(dir: &Path) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| {
        if p.is_absolute() {
            p.extension().is_some()
        } else {
            dir.join(p).is_file()
                || p.extension()
                    .is_some_and(|e| BUILD_PRODUCTS.iter().any(|b| e == *b))
        }
    }
}

fn read(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The project's `.tex` files, as `Build::wait` would list them from the `.fls`.
fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
    ["main.tex", "chapters/yoneda.tex"]
        .iter()
        .map(|f| dir.join(f))
        .filter(|p| p.is_file())
        .map(|p| {
            let text = read(&p);
            (p, text)
        })
        .collect()
}

/// `parse_log`, then runaway location, as `Build::wait` does.
fn parsed(variant: &str, log: &str) -> (LogReport, PathBuf) {
    let dir = variant_dir(variant);
    let mut report = parse_log(&read(&dir.join(log)), &dir, &fixture_exists(&dir));
    locate_runaways(&mut report, &sources(&dir));
    (report, dir)
}

fn rel(path: &Path, dir: &Path) -> String {
    path.strip_prefix(dir).unwrap_or(path).display().to_string()
}

fn line(d: &Diagnostic) -> String {
    d.line.map_or_else(|| "?".to_owned(), |l| l.to_string())
}

fn render(report: &LogReport, dir: &Path) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "rerun: {}\nfatal: {}", report.rerun, report.fatal);
    match &report.output {
        Some((path, pages)) => {
            let _ = writeln!(s, "output: {} ({pages} pages)", rel(path, dir));
        }
        None => s.push_str("output: none\n"),
    }
    for d in &report.diagnostics {
        let _ = writeln!(
            s,
            "{:?} {}:{} {}",
            d.severity,
            rel(&d.path, dir),
            line(d),
            d.message
        );
        if let Some(context) = &d.context {
            let _ = writeln!(s, "    | {context}");
        }
        assert_eq!(d.source, DiagSource::Tex);
    }
    s
}

fn render_blg(diagnostics: &[Diagnostic], dir: &Path) -> String {
    let mut s = String::new();
    for d in diagnostics {
        let _ = writeln!(
            s,
            "{:?} {:?} {}:{} {}",
            d.source,
            d.severity,
            rel(&d.path, dir),
            line(d),
            d.message
        );
    }
    s
}

/// Every `.log` fixture: (variant, file).
const LOGS: &[(&str, &str)] = &[
    ("bibtex", "main.log"),
    ("errininput", "main.log"),
    ("errininput", "main.79.log"),
    ("errininput", "wide.log"),
    ("errininput_lualatex", "main.log"),
    ("errininput_xelatex", "main.log"),
    ("missingbrace", "main.log"),
    ("missingbrace", "main.79.log"),
    ("missingdollar", "main.log"),
    ("missingdollar", "main.79.log"),
    ("missinginput", "main.log"),
    ("missinginput", "main.79.log"),
    ("overfull", "main.log"),
    ("overfull", "main.79.log"),
    ("runaway", "main.log"),
    ("runaway", "main.79.log"),
    ("undefcite", "main.log"),
    ("undefcite", "main.79.log"),
    ("undefcs", "main.log"),
    ("undefcs", "main.79.log"),
    ("undefref", "main.log"),
    ("undefref", "main.79.log"),
    ("warns", "main.log"),
    ("warns", "run1.log"),
    ("warns", "run2.log"),
    ("warns_lualatex", "main.log"),
    ("warns_lualatex", "run1.log"),
    ("warns_lualatex", "run2.log"),
    ("warns_wrapped", "main.log"),
    ("warns_wrapped", "run1.log"),
    ("warns_wrapped", "run2.log"),
    ("warns_xelatex", "main.log"),
    ("warns_xelatex", "run1.log"),
    ("warns_xelatex", "run2.log"),
    ("wide", "main.log"),
    ("wide", "w_lualatex.log"),
    ("wide", "w_xelatex.log"),
    ("proj", "main.log"),
];

/// Every `.blg` fixture.
const BLGS: &[(&str, &str)] = &[
    ("bibtex", "main.blg"),
    ("errininput", "main.blg"),
    ("missingdollar", "main.blg"),
    ("overfull", "main.blg"),
    ("undefcite", "main.blg"),
    ("undefcs", "main.blg"),
    ("undefref", "main.blg"),
    ("warns", "main.blg"),
    ("warns", "broken_biber.blg"),
    ("warns_lualatex", "main.blg"),
    ("warns_wrapped", "main.blg"),
    ("warns_xelatex", "main.blg"),
    ("wide", "main.blg"),
    ("proj", "main.blg"),
];

fn snapshot_name(variant: &str, file: &str) -> String {
    format!("{variant}__{}", file.replace('.', "_"))
}

#[test]
fn snapshots_of_every_log() {
    let mut failures = Vec::new();
    for (variant, log) in LOGS {
        let (report, dir) = parsed(variant, log);
        let name = format!("log__{}", snapshot_name(variant, log));
        let rendered = render(&report, &dir);
        // Run every snapshot even if one fails, so a parser change shows all its effects.
        let result = std::panic::catch_unwind(|| insta::assert_snapshot!(name, rendered));
        if result.is_err() {
            failures.push(format!("{variant}/{log}"));
        }
    }
    assert!(failures.is_empty(), "snapshot mismatches: {failures:?}");
}

#[test]
fn snapshots_of_every_blg() {
    let mut failures = Vec::new();
    for (variant, blg) in BLGS {
        let dir = variant_dir(variant);
        let diagnostics = parse_blg(&read(&dir.join(blg)), &[dir.join("refs.bib")]);
        let name = format!("blg__{}", snapshot_name(variant, blg));
        let rendered = render_blg(&diagnostics, &dir);
        let result = std::panic::catch_unwind(|| insta::assert_snapshot!(name, rendered));
        if result.is_err() {
            failures.push(format!("{variant}/{blg}"));
        }
    }
    assert!(failures.is_empty(), "snapshot mismatches: {failures:?}");
}

#[test]
fn fixture_lists_are_complete() {
    let mut logs = Vec::new();
    let mut blgs = Vec::new();
    let root = fixtures().join("logs");
    for variant in fs::read_dir(&root).expect("fixtures/logs") {
        let variant = variant.expect("entry").path();
        for file in fs::read_dir(&variant).expect("variant dir") {
            let path = file.expect("entry").path();
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            let variant = variant
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            match path.extension().and_then(|e| e.to_str()) {
                Some("log") => logs.push((variant, name)),
                Some("blg") => blgs.push((variant, name)),
                _ => {}
            }
        }
    }
    for (variant, file) in &logs {
        assert!(
            LOGS.contains(&(variant, file)),
            "{variant}/{file} has no snapshot"
        );
    }
    for (variant, file) in &blgs {
        assert!(
            BLGS.contains(&(variant, file)),
            "{variant}/{file} has no snapshot"
        );
    }
}

/// (path relative to the variant, line, message) of each diagnostic with `severity`.
fn find(report: &LogReport, dir: &Path, severity: Severity) -> Vec<(String, Option<u32>, String)> {
    report
        .diagnostics
        .iter()
        .filter(|d| d.severity == severity)
        .map(|d| (rel(&d.path, dir), d.line, d.message.clone()))
        .collect()
}

fn errors_of(variant: &str, log: &str) -> (Vec<(String, Option<u32>, String)>, LogReport) {
    let (report, dir) = parsed(variant, log);
    (find(&report, &dir, Severity::Error), report)
}

fn entry(path: &str, line: u32, message: &str) -> (String, Option<u32>, String) {
    (path.to_owned(), Some(line), message.to_owned())
}

#[test]
fn error_in_input_file_all_engines() {
    for (variant, log) in [
        ("errininput", "main.log"),
        ("errininput", "main.79.log"),
        ("errininput", "wide.log"),
        ("errininput_lualatex", "main.log"),
        ("errininput_xelatex", "main.log"),
    ] {
        let (errors, report) = errors_of(variant, log);
        assert_eq!(
            errors,
            [entry(
                "chapters/yoneda.tex",
                26,
                "Undefined control sequence."
            )],
            "{variant}/{log}"
        );
        let error = report
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error)
            .expect("error");
        let context = error.context.as_deref().expect("context");
        assert!(
            context.ends_with("is fully \\faithfull <HERE> faithful."),
            "{context}"
        );
        assert!(report.output.is_some() && !report.fatal);
    }
    // error_line=254 shows the whole source line.
    let (report, _) = parsed("errininput", "wide.log");
    let context = report.diagnostics.iter().find_map(|d| d.context.clone());
    assert_eq!(
        context.as_deref(),
        Some(
            "The Yoneda embedding $h\\colon\\cC\\to\\mathrm{PSh}(\\cC)$ is fully \\faithfull <HERE> faithful."
        )
    );
}

#[test]
fn missing_input_takes_the_emergency_stop_line() {
    for log in ["main.log", "main.79.log"] {
        let (errors, report) = errors_of("missinginput", log);
        assert_eq!(
            errors,
            [entry(
                "main.tex",
                46,
                "File `chapters/yonedaa.tex' not found."
            )],
            "{log}"
        );
        assert!(report.fatal && report.output.is_none());
    }
}

#[test]
fn runaway_arguments_are_located() {
    for (variant, line, message) in [
        (
            "runaway",
            41,
            "File ended while scanning use of \\blx@citeargs@iii.",
        ),
        (
            "missingbrace",
            66,
            "File ended while scanning use of \\emph .",
        ),
    ] {
        for log in ["main.log", "main.79.log"] {
            let (errors, report) = errors_of(variant, log);
            assert_eq!(
                errors,
                [entry("main.tex", line, message)],
                "{variant}/{log}"
            );
            assert!(report.fatal && report.output.is_none());
        }
    }
}

#[test]
fn other_errors() {
    let (errors, _) = errors_of("undefcs", "main.log");
    assert_eq!(
        errors,
        [entry("main.tex", 66, "Undefined control sequence.")]
    );
    let (errors, _) = errors_of("missingdollar", "main.log");
    assert_eq!(
        errors,
        [
            entry("main.tex", 110, "Missing $ inserted."),
            entry(
                "main.tex",
                111,
                "Command \\end{proof} invalid in math mode."
            ),
            entry("main.tex", 111, "Missing $ inserted."),
        ]
    );
}

#[test]
fn warns_has_the_seven_research_entries() {
    let expected = [
        (
            Severity::Warning,
            "main.aux",
            None,
            "Label `sec:intro' multiply defined.",
        ),
        (
            Severity::Warning,
            "main.tex",
            Some(32),
            "Class amsart: Abstract should precede \\maketitle in AMS document classes; reported on input line 32.",
        ),
        (
            Severity::Warning,
            "chapters/yoneda.tex",
            Some(29),
            "Citation 'Lurie' on page 2 undefined on input line 29.",
        ),
        (
            Severity::Warning,
            "main.tex",
            Some(93),
            "Package hyperref: Token not allowed in a PDF string (Unicode): removing `\\alpha' on input line 93.",
        ),
        (
            Severity::Warning,
            "main.tex",
            Some(94),
            "Font shape `OT1/cmr/bx/sc' undefined using `OT1/cmr/bx/n' instead on input line 94.",
        ),
        (
            Severity::BadBox,
            "main.tex",
            Some(95),
            "Underfull \\hbox (badness 10000) in paragraph at lines 95--95",
        ),
        (
            Severity::BadBox,
            "main.tex",
            Some(98),
            "Overfull \\vbox (10.0pt too high) detected at line 98",
        ),
    ];
    for (variant, log) in [("warns", "main.log"), ("warns_wrapped", "main.log")] {
        let (report, dir) = parsed(variant, log);
        for (severity, path, line, message) in expected {
            let found = report.diagnostics.iter().any(|d| {
                d.severity == severity
                    && rel(&d.path, &dir) == path
                    && d.line == line
                    && d.message == message
            });
            assert!(
                found,
                "{variant}/{log}: missing {severity:?} {path}:{line:?} {message}"
            );
        }
        assert!(find(&report, &dir, Severity::Error).is_empty());
    }
}

#[test]
fn rerun_detection() {
    for variant in ["warns", "warns_lualatex", "warns_wrapped", "warns_xelatex"] {
        let (report, _) = parsed(variant, "run1.log");
        assert!(report.rerun, "{variant}/run1.log");
    }
    // Both ask nothing more of LaTeX: biber ran, and the citation hint is not a rerun request.
    assert!(!parsed("undefref", "main.log").0.rerun);
    assert!(!parsed("undefcite", "main.log").0.rerun);
}

#[test]
fn output_written() {
    let (report, dir) = parsed("warns", "main.log");
    let (path, pages) = report.output.expect("output");
    assert_eq!((rel(&path, &dir).as_str(), pages), ("main.pdf", 6));
    let (report, dir) = parsed("errininput_xelatex", "main.log");
    let (path, pages) = report.output.expect("output");
    assert_eq!((rel(&path, &dir).as_str(), pages), ("main.xdv", 5));
}

#[test]
fn biber_and_bibtex_blg() {
    let dir = variant_dir("warns");
    let bib = dir.join("refs.bib");
    let broken = parse_blg(
        &read(&dir.join("broken_biber.blg")),
        std::slice::from_ref(&bib),
    );
    let error = broken
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("biber error");
    assert_eq!(
        (error.path.as_path(), error.line, error.source),
        (bib.as_path(), Some(37), DiagSource::Biber)
    );
    let missing = parse_blg(&read(&dir.join("main.blg")), std::slice::from_ref(&bib));
    assert!(missing.iter().any(|d| {
        d.severity == Severity::Warning
            && d.message
                .starts_with("I didn't find a database entry for 'Lurie'")
    }));

    let dir = variant_dir("bibtex");
    let bib = dir.join("refs.bib");
    let bibtex = parse_blg(&read(&dir.join("main.blg")), std::slice::from_ref(&bib));
    let error = bibtex
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("bibtex error");
    assert_eq!(
        (
            error.path.as_path(),
            error.line,
            error.message.as_str(),
            error.source
        ),
        (
            bib.as_path(),
            Some(36),
            "Illegal end of database file",
            DiagSource::Bibtex
        )
    );
    assert!(
        bibtex
            .iter()
            .any(|d| d.message == "I didn't find a database entry for \"Nope\"")
    );
}

/// Release-only timing: `cargo test --release -p lemma-build -- --ignored perf`.
#[test]
#[ignore]
fn perf_parse() {
    const RUNS: usize = 30;
    for (variant, log) in LOGS {
        let dir = variant_dir(variant);
        let text = read(&dir.join(log));
        if text.len() < 30_000 {
            continue;
        }
        // The same file-system check Build::wait uses.
        let exists = |p: &Path| dir.join(p).is_file();
        let mut times: Vec<f64> = (0..RUNS)
            .map(|_| {
                let start = Instant::now();
                std::hint::black_box(parse_log(&text, &dir, &exists));
                start.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let median = times[RUNS / 2];
        let per_35kb = median * 35_000.0 / text.len() as f64;
        println!(
            "perf_parse {variant}/{log}: {} bytes, median {median:.2} ms ({per_35kb:.2} ms per 35 KB)",
            text.len()
        );
        assert!(
            per_35kb < 5.0,
            "{variant}/{log}: {per_35kb:.2} ms per 35 KB"
        );
    }
}
