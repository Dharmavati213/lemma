//! Performance tests (ARCHITECTURE §7, §10.7). Run with
//! `cargo test --release -p lemma-review -- --ignored perf`.
//!
//! They read `tests/fixtures/large.tex` (60k lines) and, when `LEMMA_PERF_CORPUS` names a
//! directory, every `.tex` file in it.

use lemma_core::{Origin, TextEdit};
use lemma_review::FileReview;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn large() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/large.tex");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn corpus() -> Vec<(PathBuf, String)> {
    let Some(dir) = std::env::var_os("LEMMA_PERF_CORPUS") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tex"))
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
        .collect()
}

/// Median, minimum and maximum of `runs` timings of `f`.
fn measure(runs: usize, mut f: impl FnMut()) -> (Duration, Duration, Duration) {
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    times.sort();
    (times[runs / 2], times[0], times[runs - 1])
}

/// `text` with the middle line changed by the agent: a review with that one hunk.
fn one_hunk_review(text: &str) -> (FileReview, String) {
    let mut review = FileReview::new(text.to_owned());
    let mut current = text.to_owned();
    let middle = text.len() / 2;
    let at = text[..middle].rfind('\n').map_or(0, |i| i + 1);
    let e = TextEdit::insert(at, "% agent was here ");
    review.on_edit(&e, Origin::Agent { call: 1 });
    e.apply(&mut current);
    (review, current)
}

#[test]
#[ignore = "performance test; run in release with --ignored perf"]
fn perf_recompute_one_hunk() {
    let mut inputs = vec![(PathBuf::from("large.tex"), large())];
    inputs.extend(corpus());
    for (path, text) in inputs {
        let (mut review, current) = one_hunk_review(&text);
        let (median, min, max) = measure(25, || {
            review.recompute(&current);
        });
        assert_eq!(review.hunks().len(), 1);
        assert!(!review.hunks()[0].words.is_empty());
        eprintln!(
            "perf_recompute_one_hunk {} ({} lines): median {median:?}, min {min:?}, max {max:?}",
            path.display(),
            text.lines().count()
        );
        assert!(
            median < Duration::from_millis(2),
            "{}: {median:?}",
            path.display()
        );
    }
}

#[test]
#[ignore = "performance test; run in release with --ignored perf"]
fn perf_recompute_281_hunks() {
    // 7023 lines with every 25th line changed, as in the research benchmark.
    let base: String = large().split_inclusive('\n').take(7023).collect();
    let current: String = base
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            if i % 25 == 0 && i / 25 < 281 {
                line.replacen("with", "WITH", 1)
                    .replace('\n', " % edited\n")
            } else {
                line.to_owned()
            }
        })
        .collect();
    let mut review = FileReview::new(base.clone());
    let e = TextEdit {
        range: 0..base.len(),
        text: current.clone(),
    };
    review.on_edit(&e, Origin::Reload);
    let (median, min, max) = measure(15, || {
        review.recompute(&current);
    });
    assert_eq!(
        review.hunks().len(),
        281,
        "the 8 ms timeout coarsened the diff"
    );
    eprintln!("perf_recompute_281_hunks (7023 lines): median {median:?}, min {min:?}, max {max:?}");
    assert!(median < Duration::from_millis(8), "{median:?}");
}

#[test]
#[ignore = "performance test; run in release with --ignored perf"]
fn perf_on_edit_keystroke() {
    // Typing outside the hunk folds into the 2.2 MB baseline; inside it grows the hunk.
    let text = large();
    let (mut review, mut current) = one_hunk_review(&text);
    review.recompute(&current);
    let hunk = review.hunks()[0].cur_bytes.clone();
    let mut times = Vec::new();
    for i in 0..2000 {
        let at = if i % 2 == 0 {
            100 + i
        } else {
            hunk.start + 2 + i
        };
        let e = TextEdit::insert(at, "x");
        let t = Instant::now();
        review.on_edit(&e, Origin::User);
        times.push(t.elapsed());
        e.apply(&mut current);
    }
    times.sort();
    let (median, p99) = (times[times.len() / 2], times[times.len() * 99 / 100]);
    eprintln!("perf_on_edit_keystroke (large.tex): median {median:?}, p99 {p99:?}");
    let t = Instant::now();
    review.recompute(&current);
    eprintln!("  recompute after typing: {:?}", t.elapsed());
    assert_eq!(review.hunks().len(), 1);
    assert!(p99 < Duration::from_millis(2), "{p99:?}");
}
