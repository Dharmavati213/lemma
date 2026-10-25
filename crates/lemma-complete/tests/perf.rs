//! `perf_complete`: one completion request takes ≤ 3 ms p99 (ARCHITECTURE §7) with every table
//! loaded and a 7k-line project index. Run it in release mode:
//!
//! ```sh
//! cargo test --release -p lemma-complete -- --ignored perf
//! ```
//!
//! With `LEMMA_PERF_CORPUS=<dir>`, the `.tex` files there are indexed too.

use lemma_complete::{Completer, CompletionDb, CompletionRequest, Item, WordList};
use lemma_snippets::SnippetSet;
use lemma_syntax::{ArgKind, Context, ProjectIndex, index_file, parse_aux, scan_bib};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The budget for one request (ARCHITECTURE §7: completion ≤ 3 ms).
const P99_BUDGET: Duration = Duration::from_millis(3);
const ROUNDS: usize = 200;

fn fixture(rel: &str) -> String {
    let path = format!("{}/../../tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A project of about 7k lines: the first 7000 lines of `large.tex`, the `proj` fixture, a
/// preamble with 150 macros and 30 theorems, and 400 extra bibliography entries.
fn project() -> (ProjectIndex, Vec<String>) {
    let mut idx = ProjectIndex::default();
    let large: String = fixture("large.tex")
        .lines()
        .take(7000)
        .collect::<Vec<_>>()
        .join("\n");
    let mut preamble = String::new();
    for i in 0..150 {
        let _ = writeln!(
            preamble,
            "\\newcommand{{\\macro{i}}}{{\\mathcal{{M}}_{{{i}}}}}"
        );
    }
    for i in 0..30 {
        let _ = writeln!(preamble, "\\newtheorem{{theorem{i}}}{{Theorem {i}}}");
    }
    let mut texts = vec![
        large,
        preamble,
        fixture("proj/main.tex"),
        fixture("proj/chapters/yoneda.tex"),
    ];
    if let Ok(dir) = std::env::var("LEMMA_PERF_CORPUS") {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tex")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                texts.push(text);
            }
        }
    }
    for (i, text) in texts.iter().enumerate() {
        idx.update_file(
            PathBuf::from(format!("/p/file{i}.tex")),
            Arc::new(index_file(text)),
        );
    }
    let mut bib = fixture("proj/refs.bib");
    for i in 0..400 {
        let _ = writeln!(
            bib,
            "@article{{key{i},\n  author = {{Author{i}, A. and Other, B.}},\n  title = {{On the \
             structure of {{$X_{i}$}}}},\n  year = {{{}}}\n}}",
            1950 + i % 70
        );
    }
    idx.bibs
        .insert(PathBuf::from("/p/refs.bib"), Arc::new(scan_bib(&bib)));
    idx.aux = Arc::new(parse_aux(&fixture("proj/main.aux")));
    (idx, texts)
}

fn math() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

fn arg(cmd: &str, kind: ArgKind, line: &str) -> Context {
    let start = line.rfind(['{', ',']).map_or(0, |i| i + 1) as u32;
    Context::Arg {
        cmd: cmd.into(),
        idx: 0,
        kind,
        start,
    }
}

#[test]
#[ignore = "performance test: cargo test --release -p lemma-complete -- --ignored perf"]
fn perf_complete() {
    let mut db = CompletionDb::builtin();
    let all: Vec<String> = CompletionDb::available_packages()
        .into_iter()
        .map(String::from)
        .collect();
    db.load_packages(&all);
    let (idx, texts) = project();
    let lines: usize = texts.iter().map(|t| t.lines().count()).sum();
    let dictionary: String = (0..800).map(|i| format!("mathword{i}\n")).collect();
    let mut words = WordList::from_text(&[&dictionary]);
    for text in &texts {
        words.add_project_text(text);
    }
    let snippets = SnippetSet::defaults();
    let mut completer = Completer::new();

    let requests: Vec<(&str, Context, &str)> = vec![
        ("command", math(), r"$\"),
        ("command", math(), r"$\f"),
        ("command", math(), r"$\fr"),
        ("command", math(), r"$\mathb"),
        ("command", math(), r"$\longrightar"),
        ("command", math(), r"$\mac"),
        ("command", Context::Text, r"\"),
        ("command", Context::Text, r"\te"),
        ("command", Context::Text, r"\sec"),
        ("command", Context::CommandName { start: 0 }, r"\c"),
        (
            "environment",
            arg("begin", ArgKind::Env, r"\begin{"),
            r"\begin{",
        ),
        (
            "environment",
            arg("begin", ArgKind::Env, r"\begin{al"),
            r"\begin{al",
        ),
        ("environment", Context::Text, r"\begin{theo"),
        ("label", arg("ref", ArgKind::Ref, r"\ref{"), r"\ref{"),
        (
            "label",
            arg("ref", ArgKind::Ref, r"\ref{sec:1"),
            r"\ref{sec:1",
        ),
        ("citation", arg("cite", ArgKind::Cite, r"\cite{"), r"\cite{"),
        (
            "citation",
            arg("cite", ArgKind::Cite, r"\cite{key1"),
            r"\cite{key1",
        ),
        ("word", Context::Text, "a th"),
        ("word", Context::Text, "a qua"),
        ("word", Context::Text, "a mathword1"),
        ("snippet", Context::Text, "thm"),
    ];
    let mut out: Vec<Item> = Vec::new();
    // Warm up.
    for (_, ctx, line) in &requests {
        let req = CompletionRequest::new(ctx, line);
        completer.complete(&db, &snippets, &idx, &words, &req, &mut out);
        assert!(!out.is_empty(), "no items for {line:?}");
    }
    let mut times: Vec<(Duration, &str)> = Vec::new();
    for _ in 0..ROUNDS {
        for (kind, ctx, line) in &requests {
            let req = CompletionRequest::new(ctx, line);
            let t = Instant::now();
            completer.complete(&db, &snippets, &idx, &words, &req, &mut out);
            times.push((t.elapsed(), kind));
        }
    }
    let percentile = |ts: &mut Vec<Duration>, p: f64| {
        ts.sort();
        ts[((ts.len() as f64 * p).ceil() as usize).clamp(1, ts.len()) - 1]
    };
    for kind in [
        "command",
        "environment",
        "label",
        "citation",
        "word",
        "snippet",
    ] {
        let mut ts: Vec<Duration> = times
            .iter()
            .filter(|(_, k)| *k == kind)
            .map(|(t, _)| *t)
            .collect();
        println!(
            "perf_complete {kind:<12} p50 {:>8.3?}  p99 {:>8.3?}  max {:>8.3?}",
            percentile(&mut ts, 0.5),
            percentile(&mut ts, 0.99),
            ts.last().copied().unwrap_or_default()
        );
    }
    let mut all: Vec<Duration> = times.iter().map(|(t, _)| *t).collect();
    let p99 = percentile(&mut all, 0.99);
    let (commands, environments) = db.counts();
    println!(
        "perf_complete: {} requests, {commands} commands, {environments} environments, \
         {lines} project lines, {} labels, {} bib entries, {} words: p99 {p99:.3?}",
        all.len(),
        idx.all_labels().len(),
        idx.all_bib().len(),
        words.len()
    );
    assert!(lines >= 7000, "the project has {lines} lines");
    assert!(p99 <= P99_BUDGET, "p99 {p99:?} > {P99_BUDGET:?}");
}
