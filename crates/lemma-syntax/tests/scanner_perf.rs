//! Scanner performance budgets (`docs/ARCHITECTURE.md` §7, §10.7 WP1.7). Ignored by default:
//!
//! ```sh
//! cargo test --release -p lemma-syntax --test scanner_perf -- --ignored perf
//! ```
//!
//! `LEMMA_PERF_CORPUS` may name a directory of `.tex` files (your own corpus); each is held to
//! the same full-scan budget.

use lemma_core::{LineIndex, TextEdit};
use lemma_syntax::{LineStates, ScanConfig, StrLines};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REBUILD_BUDGET: Duration = Duration::from_millis(15);
const CONTEXT_BUDGET: Duration = Duration::from_micros(50);
const KEYSTROKE_BUDGET: Duration = Duration::from_millis(2);

fn fixture(rel: &str) -> String {
    let path = format!("{}/../../tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).expect("fixture exists")
}

fn corpus() -> Vec<(PathBuf, String)> {
    let Some(dir) = std::env::var_os("LEMMA_PERF_CORPUS") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, String)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "tex"))
        .filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            Some((p, String::from_utf8_lossy(&bytes).into_owned()))
        })
        .collect();
    files.sort();
    files
}

/// The median of `runs` timings of `f`.
fn median(runs: usize, mut f: impl FnMut()) -> Duration {
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

fn new_states() -> LineStates {
    LineStates::new(Arc::new(ScanConfig::default()))
}

#[test]
#[ignore]
fn perf_rebuild_large() {
    let mut docs = vec![(
        PathBuf::from("tests/fixtures/large.tex"),
        fixture("large.tex"),
    )];
    docs.extend(corpus());
    for (path, text) in &docs {
        let index = LineIndex::new(text);
        let src = StrLines {
            text,
            index: &index,
        };
        let mut states = new_states();
        let t = median(9, || states.rebuild(&src));
        eprintln!(
            "rebuild {}: {} lines, {} KB in {t:?}",
            path.display(),
            index.line_count(),
            text.len() / 1024
        );
        assert!(
            t < REBUILD_BUDGET,
            "rebuild of {} took {t:?}",
            path.display()
        );
    }
}

#[test]
#[ignore]
fn perf_context_at() {
    let mut docs = vec![
        (
            PathBuf::from("tests/fixtures/large.tex"),
            fixture("large.tex"),
        ),
        (
            PathBuf::from("tests/fixtures/proj/main.tex"),
            fixture("proj/main.tex"),
        ),
    ];
    docs.extend(corpus());
    let mut worst = Duration::ZERO;
    for (path, text) in &docs {
        let index = LineIndex::new(text);
        let mut states = new_states();
        states.rebuild(&StrLines {
            text,
            index: &index,
        });
        let lines = index.line_count();
        let step = (lines / 2000).max(1);
        for l in (0..lines).step_by(step as usize) {
            let line = &text[index.line_range(l)];
            // The end of the line is the most expensive cursor position.
            let byte = line.len() as u32;
            const REPS: u32 = 50;
            let t = Instant::now();
            for _ in 0..REPS {
                std::hint::black_box(states.context_at(l, std::hint::black_box(line), byte));
            }
            let per_call = t.elapsed() / REPS;
            worst = worst.max(per_call);
            assert!(
                per_call < CONTEXT_BUDGET,
                "context_at {}:{} took {per_call:?}",
                path.display(),
                l + 1
            );
        }
    }
    eprintln!("context_at: worst mean per line {worst:?}");
}

#[test]
#[ignore]
fn perf_apply_edit_keystroke() {
    let mut text = fixture("large.tex");
    let mut index = LineIndex::new(&text);
    let mut states = new_states();
    states.rebuild(&StrLines {
        text: &text,
        index: &index,
    });
    let mut worst = Duration::ZERO;
    let mut at = text.len() / 2;
    for c in "typing $x$ and \\cite{key} here\n".chars() {
        let edit = TextEdit::insert(at, c.to_string());
        let (first, removed, added) = index.line_delta(&edit);
        edit.apply(&mut text);
        index.apply(&edit);
        let t = Instant::now();
        states.apply_edit(
            &StrLines {
                text: &text,
                index: &index,
            },
            first,
            removed,
            added,
        );
        worst = worst.max(t.elapsed());
        at += c.len_utf8();
    }
    eprintln!("apply_edit per keystroke on large.tex: worst {worst:?}");
    assert!(worst < KEYSTROKE_BUDGET, "keystroke rescan took {worst:?}");
}
