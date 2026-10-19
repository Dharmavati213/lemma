//! The search thread: scope order and filters, live text over the disk, streaming in batches,
//! the match cap, cancellation and "latest wins".

#![allow(clippy::single_range_in_vec_init)] // match ranges are compared as lists

use lemma_search::{Event, FileMatches, Limits, Query, Scope, Search, Searcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-search-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(dir: &Path, file: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn large_tex() -> Arc<str> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/large.tex");
    Arc::from(std::fs::read_to_string(path).unwrap())
}

fn rel(root: &Path, files: &[FileMatches]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            f.path.strip_prefix(root).map_or_else(
                |_| f.path.display().to_string(),
                |p| p.display().to_string(),
            )
        })
        .collect()
}

#[test]
fn scope_order_filters_and_live_text() {
    let root = temp_dir("scope");
    let outside = temp_dir("scope-outside");
    write(&root, "main.tex", "disk needle\n");
    write(&root, "chapters/a.tex", "needle a\n");
    write(&root, "b.bib", "@book{needle}\n");
    write(&root, "notes.md", "a needle\n");
    write(&root, "style.sty", "\\def\\needle{}\n");
    write(&root, "x.cls", "needle\n");
    write(&root, "z.txt", "needle in a text file\n");
    write(&root, "UPPER.TEX", "NEEDLE\n");
    write(&root, ".hidden/h.tex", "needle\n");
    write(&root, ".git/x.tex", "needle\n");
    write(&root, "build/out.tex", "needle\n");
    write(&root, "sub/deep/d.tex", "needle\n");
    write(&root, "binary.tex", b"needle\0\x01\x02");
    write(&root, "big.tex", "needle ".repeat(400));
    write(&root, "latin.tex", b"caf\xe9 needle \xa4\n");
    write(&outside, "o.tex", "needle outside\n");
    std::os::unix::fs::symlink(root.join("main.tex"), root.join("link.tex")).unwrap();

    let scope = Scope {
        root: Some(root.clone()),
        files: vec![
            root.join("chapters/a.tex"),
            root.join("main.tex"),
            outside.join("o.tex"),
        ],
        live: vec![
            (
                root.join("main.tex"),
                Arc::from("live\nlive needle\nneedle"),
            ),
            (root.join("z.txt"), Arc::from("live needle")),
            (root.join("unsaved.tex"), Arc::from("needle")),
        ],
        skip_dirs: vec![root.join("build")],
    };
    let limits = Limits {
        max_file_bytes: 1000,
        ..Limits::default()
    };
    let matcher = Query::plain("needle").matcher().unwrap();
    let (files, summary) = Searcher::new().start(matcher, scope, limits).wait();
    let summary = summary.unwrap();
    assert_eq!(
        rel(&root, &files),
        [
            // The project graph, in the given order (outside the root too).
            "chapters/a.tex",
            "main.tex",
            &outside.join("o.tex").display().to_string(),
            // The walk: the root's files by name (byte order), then its directories.
            "UPPER.TEX",
            "b.bib",
            "latin.tex",
            "notes.md",
            "style.sty",
            "x.cls",
            "sub/deep/d.tex",
            // Live documents the rest did not reach.
            "z.txt",
            "unsaved.tex",
        ]
    );
    let main = &files[1];
    assert!(main.live, "main.tex is searched in its live text");
    assert_eq!(main.matches, 2);
    assert_eq!(
        main.lines
            .iter()
            .map(|l| (l.line, l.offset, l.text.as_str()))
            .collect::<Vec<_>>(),
        [(1, 5, "live needle"), (2, 17, "needle")]
    );
    assert!(!files[0].live);
    // ISO-8859-15 is decoded.
    assert_eq!(files[5].lines[0].text, "café needle €");
    assert_eq!(files[5].lines[0].ranges, [6..12]);
    assert_eq!(summary.skipped_binary, 1);
    assert_eq!(summary.skipped_large, 1);
    assert_eq!(summary.files_with_matches, files.len());
    assert_eq!(summary.matches, 13);
    assert!(!summary.capped && !summary.walk_truncated);
}

#[test]
fn lines_are_shown_without_crlf_and_long_lines_are_cut() {
    let root = temp_dir("lines");
    let long = format!(
        "{}needle{}needle{}",
        "a".repeat(3000),
        "é".repeat(900),
        "b".repeat(50)
    );
    let scope = Scope {
        live: vec![
            (root.join("crlf.tex"), Arc::from("x\r\nneedle\r\n")),
            (root.join("long.tex"), Arc::from(long.as_str())),
        ],
        ..Scope::default()
    };
    let matcher = Query::regex(r"needle\r?").matcher().unwrap();
    let (files, _) = Searcher::new()
        .start(matcher, scope, Limits::default())
        .wait();
    let crlf = &files[0].lines[0];
    assert_eq!((crlf.line, crlf.text.as_str()), (1, "needle"));
    // The `\r` the match took is not shown.
    assert_eq!(crlf.ranges, [0..6]);
    let cut = &files[1].lines[0];
    assert_eq!(cut.matches, 2);
    assert_eq!(cut.first, 3000..3006);
    assert!(
        cut.text_start > 2800 && cut.text_start < 3000,
        "{}",
        cut.text_start
    );
    assert!(cut.cut_end);
    assert!(cut.text.len() <= Limits::default().line_bytes + 4);
    assert_eq!(cut.ranges, [3000 - cut.text_start..3006 - cut.text_start]);
    assert_eq!(&cut.text[cut.ranges[0].clone()], "needle");
}

#[test]
fn results_stream_in_batches_and_the_cap_ends_the_search() {
    let root = temp_dir("batches");
    for i in 0..10 {
        write(
            &root,
            &format!("f{i}.tex"),
            "x\n".repeat(3) + &"needle\n".repeat(10),
        );
    }
    let scope = Scope {
        root: Some(root.clone()),
        ..Scope::default()
    };
    let matcher = Query::plain("needle").matcher().unwrap();
    // One file per batch.
    let limits = Limits {
        batch_lines: 1,
        ..Limits::default()
    };
    let search = Searcher::new().start(matcher.clone(), scope.clone(), limits);
    let mut batches = Vec::new();
    let mut done = None;
    while let Ok(event) = search.events().recv_blocking() {
        match event {
            Event::Files(files) => batches.push(files.len()),
            Event::Done(s) => done = Some(s),
        }
    }
    assert_eq!(batches, [1; 10]);
    assert_eq!(done.unwrap().matches, 100);

    // The cap: the search stops at the line that reaches it.
    let limits = Limits {
        max_matches: 25,
        ..Limits::default()
    };
    let (files, summary) = Searcher::new().start(matcher, scope, limits).wait();
    let summary = summary.unwrap();
    assert!(summary.capped);
    assert_eq!(files.len(), 3);
    assert_eq!(summary.matches, 25);
    assert_eq!(
        files
            .iter()
            .map(|f| (f.matches, f.complete))
            .collect::<Vec<_>>(),
        [(10, true), (10, true), (5, false)]
    );
    assert_eq!(files[2].lines.last().unwrap().line, 7);
}

/// A scope that takes a while: 40 copies of `large.tex` (60k lines) as live documents.
fn slow_scope(root: &Path) -> Scope {
    let text = large_tex();
    Scope {
        live: (0..40)
            .map(|i| (root.join(format!("copy{i}.tex")), text.clone()))
            .collect(),
        ..Scope::default()
    }
}

/// Reads `search`'s events until the channel closes; true when `Done` came.
fn drain(search: &Search) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut done = false;
    while let Ok(event) = search.events().recv_blocking() {
        assert!(Instant::now() < deadline, "the search ends");
        done |= matches!(event, Event::Done(_));
    }
    done
}

#[test]
fn the_latest_search_wins() {
    let root = temp_dir("latest");
    let searcher = Searcher::new();
    let limits = Limits {
        max_matches: usize::MAX,
        batch_lines: 16,
        ..Limits::default()
    };
    let matcher = Query::plain("the").matcher().unwrap();
    let first = searcher.start(matcher.clone(), slow_scope(&root), limits.clone());
    assert!(matches!(
        first.events().recv_blocking(),
        Ok(Event::Files(_))
    ));
    assert!(first.is_current());
    // A new search cancels the first, which ends without `Done` even though nobody reads it
    // until then (it never waits on a full channel once cancelled).
    let small = Scope {
        live: vec![(root.join("a.tex"), Arc::from("the end"))],
        ..Scope::default()
    };
    let second = searcher.start(matcher.clone(), small, limits.clone());
    assert!(!first.is_current());
    assert!(second.is_current());
    let started = Instant::now();
    assert!(!drain(&first), "a cancelled search sends no Done");
    assert!(started.elapsed() < Duration::from_secs(10));
    let (files, summary) = second.wait();
    assert_eq!(files.len(), 1);
    assert_eq!(summary.unwrap().matches, 1);

    // `cancel` stops the current search; cancelling an old one does nothing.
    let third = searcher.start(matcher.clone(), slow_scope(&root), limits.clone());
    first.cancel();
    assert!(third.is_current());
    assert!(matches!(
        third.events().recv_blocking(),
        Ok(Event::Files(_))
    ));
    third.cancel();
    assert!(!third.is_current());
    assert!(!drain(&third));
    // So does the searcher's.
    let fourth = searcher.start(matcher, slow_scope(&root), limits);
    searcher.cancel();
    assert!(!drain(&fourth));
}

#[test]
fn a_dropped_receiver_stops_the_search() {
    let root = temp_dir("dropped");
    let searcher = Searcher::new();
    let matcher = Query::plain("the").matcher().unwrap();
    let limits = Limits {
        max_matches: usize::MAX,
        batch_lines: 16,
        ..Limits::default()
    };
    let search = searcher.start(matcher.clone(), slow_scope(&root), limits.clone());
    assert!(matches!(
        search.events().recv_blocking(),
        Ok(Event::Files(_))
    ));
    drop(search);
    // The searcher is still usable, and the dropped search's thread does not hold it up.
    let small = Scope {
        live: vec![(root.join("a.tex"), Arc::from("the"))],
        ..Scope::default()
    };
    let (files, summary) = searcher.start(matcher, small, limits).wait();
    assert_eq!(files.len(), 1);
    assert!(summary.is_some());
}

#[test]
fn ten_thousand_matches_on_large_tex_are_fast() {
    let root = temp_dir("large");
    let scope = Scope {
        live: vec![(root.join("large.tex"), large_tex())],
        ..Scope::default()
    };
    let matcher = Query::plain("e").matcher().unwrap();
    let started = Instant::now();
    let (files, summary) = Searcher::new()
        .start(matcher, scope, Limits::default())
        .wait();
    let summary = summary.unwrap();
    // The cap cuts the file at the line that reaches it.
    assert_eq!(files.len(), 1);
    assert!(!files[0].complete);
    assert!(
        (20_000..20_100).contains(&summary.matches),
        "{}",
        summary.matches
    );
    assert!(summary.capped);
    eprintln!(
        "{} matches on {} lines in {:?}",
        summary.matches,
        summary.lines,
        started.elapsed()
    );
}
