//! WP1.9 performance: `perf_index` < 5 ms on 7k lines.
//!
//! Run with `cargo test --release -p lemma-syntax --test structure_perf -- --ignored perf`.
//! `LEMMA_PERF_CORPUS` (a `.tex` file or a directory of them) adds the user's own documents.

use lemma_syntax::{index_file, scan_bib};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BUDGET_7K_LINES: Duration = Duration::from_millis(5);

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The median time of `runs` calls, after one warm-up call.
fn median(runs: usize, mut f: impl FnMut()) -> Duration {
    f();
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    times.sort();
    times[runs / 2]
}

/// Debug builds are much slower; the budgets are for `--release`.
fn budget(lines: usize) -> Duration {
    let scaled = BUDGET_7K_LINES.mul_f64((lines as f64 / 7000.0).max(1.0));
    if cfg!(debug_assertions) {
        eprintln!("note: debug build, budget x10 (run with --release for the real check)");
        scaled * 10
    } else {
        scaled
    }
}

/// About 7000 lines in the shape of a real paper: the fixture's body and chapter repeated, with
/// the preamble once.
fn paper_7k() -> String {
    let main = read(&fixtures().join("proj/main.tex"));
    let chapter = read(&fixtures().join("proj/chapters/yoneda.tex"));
    let (preamble, rest) = main.split_once("\\begin{document}").unwrap();
    let body = rest.split("\\end{document}").next().unwrap();
    let mut doc = format!("{preamble}\\begin{{document}}");
    while doc.lines().count() < 7000 {
        doc.push_str(body);
        doc.push_str(&chapter);
    }
    doc.push_str("\\end{document}\n");
    doc
}

#[test]
#[ignore = "perf: run with --release -- --ignored perf"]
fn perf_index() {
    let doc = paper_7k();
    let lines = doc.lines().count();
    let idx = index_file(&doc);
    assert!(idx.labels.len() > 300 && idx.outline.len() > 300);
    let t = median(21, || {
        std::hint::black_box(index_file(std::hint::black_box(&doc)));
    });
    eprintln!("perf_index: {lines} lines, {} KB: {t:?}", doc.len() / 1024);
    assert!(t < budget(lines), "{t:?} for {lines} lines");

    let large = read(&fixtures().join("large.tex"));
    let first_7k: String = large.split_inclusive('\n').take(7000).collect();
    let t = median(21, || {
        std::hint::black_box(index_file(std::hint::black_box(&first_7k)));
    });
    eprintln!(
        "perf_index: large.tex first 7000 lines, {} KB: {t:?}",
        first_7k.len() / 1024
    );
    assert!(t < budget(7000), "{t:?}");

    let all_lines = large.lines().count();
    let t = median(5, || {
        std::hint::black_box(index_file(std::hint::black_box(&large)));
    });
    eprintln!(
        "perf_index: large.tex {all_lines} lines, {} KB: {t:?}",
        large.len() / 1024
    );
    assert!(t < budget(all_lines), "{t:?}");
}

#[test]
#[ignore = "perf: run with --release -- --ignored perf"]
fn perf_index_corpus() {
    let Some(corpus) = std::env::var_os("LEMMA_PERF_CORPUS").map(PathBuf::from) else {
        eprintln!("SKIP: LEMMA_PERF_CORPUS is not set");
        return;
    };
    let files: Vec<PathBuf> = if corpus.is_dir() {
        let mut v: Vec<_> = std::fs::read_dir(&corpus)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "tex"))
            .collect();
        v.sort();
        v
    } else {
        vec![corpus]
    };
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().count();
        let t = median(11, || {
            std::hint::black_box(index_file(std::hint::black_box(&text)));
        });
        eprintln!("perf_index: {} ({lines} lines): {t:?}", path.display());
        assert!(t < budget(lines), "{}: {t:?}", path.display());
    }
}

#[test]
#[ignore = "perf: run with --release -- --ignored perf"]
fn perf_scan_bib() {
    let Some(path) = std::process::Command::new("kpsewhich")
        .arg("tugboat.bib")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: tugboat.bib not found");
        return;
    };
    let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
    let t = median(5, || {
        std::hint::black_box(scan_bib(std::hint::black_box(&text)));
    });
    eprintln!("perf_scan_bib: tugboat.bib {} KB: {t:?}", text.len() / 1024);
    // The research measured 611 ms for the `biblatex` crate on this file.
    assert!(t < Duration::from_millis(100) * if cfg!(debug_assertions) { 10 } else { 1 });
}

/// Broken or generated files with thousands of unclosed arguments, unterminated `\iffalse` or
/// verbatim environments, or `\end`s of environments that are not open: every such command used
/// to scan to the end of the text again (seconds for a few hundred KB). The pass must stay
/// linear: twice the text, at most about twice the time, and within 200 ms per MB.
#[test]
#[ignore = "perf: run with --release -- --ignored perf"]
fn perf_index_pathological() {
    let shapes: &[(&str, &str)] = &[
        ("verbatim", "\\begin{verbatim}\n"),
        ("iffalse", "\\iffalse\n"),
        ("section", "\\section{"),
        ("label", "\\label{"),
        ("newcommand body", "\\newcommand{\\a}{"),
        ("newcommand count", "\\newcommand\\a["),
        ("url", "\\url{"),
        ("hyperref", "\\hyperref["),
        ("caption", "\\caption["),
        ("document command", "\\NewDocumentCommand{\\a}{"),
        ("theorem title", "\\begin{theorem}["),
        ("declaretheorem", "\\declaretheorem["),
        ("import", "\\import{"),
    ];
    let per_mb = Duration::from_millis(200) * if cfg!(debug_assertions) { 10 } else { 1 };
    let time = |text: &str| {
        median(3, || {
            std::hint::black_box(index_file(std::hint::black_box(text)));
        })
    };
    let mut cases: Vec<(String, String, String)> = shapes
        .iter()
        .map(|(name, unit)| {
            let n = 240_000 / unit.len();
            (name.to_string(), unit.repeat(n), unit.repeat(2 * n))
        })
        .collect();
    // `\end`s of an environment that is not open, after many open ones.
    cases.push((
        "end not open".into(),
        format!(
            "{}{}",
            "\\begin{foo}\n".repeat(10_000),
            "\\end{bar}\n".repeat(10_000)
        ),
        format!(
            "{}{}",
            "\\begin{foo}\n".repeat(20_000),
            "\\end{bar}\n".repeat(20_000)
        ),
    ));
    for (name, small, large) in &cases {
        let (t1, t2) = (time(small), time(large));
        let budget = per_mb.mul_f64(large.len() as f64 / (1 << 20) as f64);
        eprintln!(
            "perf_index_pathological: {name}: {} KB {t1:?}, {} KB {t2:?}",
            small.len() / 1024,
            large.len() / 1024
        );
        assert!(t2 < budget, "{name}: {t2:?} for {} KB", large.len() / 1024);
        assert!(
            t2 < t1 * 3 + Duration::from_millis(2),
            "{name}: not linear ({t1:?} → {t2:?})"
        );
    }
}
