//! M5 WP5.3: completion for every package. The core packages are static tables; every other
//! package is a zstd frame that is decompressed only when a project loads it.
//!
//! One test per process state: the checks run in order in one test, because the decompressed
//! packages are process-wide. The budget of 5 ms per package is checked in release:
//!
//! ```sh
//! cargo test --release -p lemma-complete --test packages -- --ignored perf
//! ```

use lemma_complete::{Completer, CompletionDb, CompletionRequest, Item, WordList};
use lemma_snippets::SnippetSet;
use lemma_syntax::{ArgKind, Context, ProjectIndex, index_file};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Budget for the compressed tables in the binary (binary growth ≤ 3 MB).
const PACKED_BUDGET: usize = 3 << 20;
/// Budget for loading one package (decompressing it and adding its tables), release build.
const LOAD_BUDGET: Duration = Duration::from_millis(5);

fn index_of(text: &str) -> ProjectIndex {
    let mut idx = ProjectIndex::default();
    idx.update_file(PathBuf::from("/p/main.tex"), Arc::new(index_file(text)));
    idx
}

fn complete(db: &CompletionDb, idx: &ProjectIndex, ctx: &Context, line: &str) -> Vec<Item> {
    let req = CompletionRequest::new(ctx, line);
    let mut items = Vec::new();
    Completer::new().complete(
        db,
        &SnippetSet::defaults(),
        idx,
        &WordList::default(),
        &req,
        &mut items,
    );
    items
}

fn env_arg(line: &str) -> Context {
    Context::Arg {
        cmd: "begin".into(),
        idx: 0,
        kind: ArgKind::Env,
        start: (line.rfind('{').unwrap() + 1) as u32,
    }
}

fn math() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

#[test]
fn packages_load_lazily_and_on_demand() {
    // 1. Only core packages: nothing is decompressed (idle memory as before M5).
    let core = CompletionDb::builtin();
    for p in core.loaded_packages() {
        assert!(CompletionDb::is_static_package(p), "{p} is not static");
    }
    let plain = index_of("\\documentclass{article}\n\\usepackage{amsmath,amssymb}\n");
    let mut db = core.clone();
    db.load_packages(&plain.packages());
    db.load_packages(&["class-article".into()]);
    assert_eq!(CompletionDb::decompressed_packages(), Vec::<&str>::new());

    // 2. A non-core package's completion appears only after `\usepackage`.
    let with_cd = index_of("\\usepackage{tikz-cd}\n\\usepackage{siunitx}\n");
    let line = r"\begin{tikzc";
    let before = complete(&db, &plain, &env_arg(line), line);
    assert!(!before.iter().any(|i| i.label == "tikzcd"), "{before:?}");
    let line_si = r"$\qty";
    assert!(
        !complete(&db, &plain, &math(), line_si)
            .iter()
            .any(|i| i.label == r"\qty")
    );
    db.load_packages(&with_cd.packages());
    let after = complete(&db, &with_cd, &env_arg(line), line);
    assert_eq!(after[0].label, "tikzcd");
    assert_eq!(after[0].detail.as_deref(), Some("tikz-cd"));
    assert!(
        complete(&db, &with_cd, &math(), line_si)
            .iter()
            .any(|i| i.label == r"\qty")
    );
    let decompressed = CompletionDb::decompressed_packages();
    for p in ["tikz-cd", "tikz", "tikzlibrarycd", "siunitx"] {
        assert!(decompressed.contains(&p), "{p}: {decompressed:?}");
    }
    assert!(!decompressed.contains(&"biblatex"));
    // The core database is unchanged.
    assert!(core.environment("tikzcd").is_none());

    // 3. Every package decompresses, and fast enough even in a debug build.
    let all: Vec<String> = CompletionDb::available_packages()
        .into_iter()
        .map(String::from)
        .collect();
    let mut slowest = (Duration::ZERO, String::new());
    for name in &all {
        let mut db = CompletionDb::builtin();
        let t = Instant::now();
        db.load_packages(std::slice::from_ref(name));
        let took = t.elapsed();
        assert!(db.is_loaded(name), "{name}");
        if took > slowest.0 {
            slowest = (took, name.clone());
        }
    }
    assert!(slowest.0 < Duration::from_millis(100), "{slowest:?}");

    // 4. The compressed tables fit the binary budget.
    assert!(CompletionDb::packed_size() > 0);
    assert!(
        CompletionDb::packed_size() <= PACKED_BUDGET,
        "{} bytes",
        CompletionDb::packed_size()
    );
}

/// Loading each package for the first time (decompressing it, its includes and adding the
/// tables) takes ≤ 5 ms in a release build. Prints the times.
#[test]
#[ignore = "performance test: cargo test --release -p lemma-complete --test packages -- --ignored perf"]
fn perf_package_load() {
    let mut times: Vec<(Duration, String)> = Vec::new();
    for name in CompletionDb::available_packages() {
        if CompletionDb::is_static_package(name) {
            continue;
        }
        let mut db = CompletionDb::builtin();
        let t = Instant::now();
        db.load_packages(&[name.to_owned()]);
        times.push((t.elapsed(), name.to_owned()));
    }
    times.sort();
    let (worst, name) = times.last().cloned().unwrap_or_default();
    let total: Duration = times.iter().map(|t| t.0).sum();
    println!(
        "{} packed packages ({} bytes): slowest first load {name} {worst:?}, all {total:?}",
        times.len(),
        CompletionDb::packed_size()
    );
    assert!(worst <= LOAD_BUDGET, "{name}: {worst:?}");
}
