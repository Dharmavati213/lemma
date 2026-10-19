//! WP1.4 acceptance tests on `tests/fixtures/proj`.
//!
//! The fixture was built in another directory, so its synctex file records absolute paths that
//! do not point into this repository (and need not exist).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lemma_synctex::{Scanner, SyncBox};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/proj")
        .canonicalize()
        .expect("tests/fixtures/proj exists")
}

fn open_fixture() -> (PathBuf, Scanner) {
    let dir = fixture();
    let scanner = Scanner::open(&dir.join("main.pdf")).expect("fixture synctex opens");
    (dir, scanner)
}

fn near(b: &SyncBox, page: u32, h: f32, v_top: f32, width: f32, height: f32) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() <= 0.1;
    b.page == page
        && close(b.h, h)
        && close(b.v_top, v_top)
        && close(b.width, width)
        && close(b.height, height)
}

/// `target` relative to the current directory.
fn relative_from_cwd(target: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let common = cwd.ancestors().find(|a| target.starts_with(a)).unwrap();
    let ups = cwd.strip_prefix(common).unwrap().components().count();
    let mut rel: PathBuf = std::iter::repeat_n("..", ups).collect();
    rel.push(target.strip_prefix(common).unwrap());
    rel
}

#[test]
fn forward_main_line_66() {
    let (dir, mut s) = open_fixture();
    let boxes = s.forward(&dir.join("main.tex"), 66);
    assert!(
        boxes
            .iter()
            .any(|b| near(b, 2, 126.67, 415.05, 358.66, 9.70)),
        "{boxes:?}"
    );
    assert!(boxes.iter().all(|b| b.page == 2), "{boxes:?}");
}

#[test]
fn forward_into_an_input_file() {
    let (dir, mut s) = open_fixture();
    let boxes = s.forward(&dir.join("chapters/yoneda.tex"), 26);
    assert!(!boxes.is_empty());
    assert!(boxes.iter().all(|b| b.page == 2), "{boxes:?}");
}

#[test]
fn identical_boxes_are_reported_once() {
    // libsynctex returns five nodes of the same hbox for this line.
    let (dir, mut s) = open_fixture();
    let boxes = s.forward(&dir.join("chapters/yoneda.tex"), 26);
    assert_eq!(boxes.len(), 1, "{boxes:?}");
    assert!(
        near(&boxes[0], 2, 126.50, 228.48, 358.83, 15.68),
        "{boxes:?}"
    );
}

#[test]
fn forward_reports_every_page_of_a_line() {
    // The paragraph starting at line 109 is broken across pages 2 and 3.
    let (dir, mut s) = open_fixture();
    let boxes = s.forward(&dir.join("main.tex"), 109);
    assert!(boxes.iter().any(|b| b.page == 2), "{boxes:?}");
    assert!(boxes.iter().any(|b| b.page == 3), "{boxes:?}");
}

#[test]
fn inverse_main_and_input_file() {
    let (dir, mut s) = open_fixture();
    let t = s.inverse(2, 200.0, 420.0).expect("a line at (200, 420)");
    assert!(t.file.ends_with("main.tex"), "{t:?}");
    assert_eq!(t.line, 66);
    let t = s.inverse(2, 200.0, 240.0).expect("a line at (200, 240)");
    assert!(t.file.ends_with("chapters/yoneda.tex"), "{t:?}");
    assert_eq!(t.line, 25);
    // The recorded build directory is not an ancestor of the fixture, so the inputs are
    // re-anchored at the PDF's directory, where the sources are.
    assert_eq!(t.file, dir.join("chapters/yoneda.tex"));
}

#[test]
fn inverse_of_a_forward_box_returns_the_line() {
    let (dir, mut s) = open_fixture();
    let b = s.forward(&dir.join("main.tex"), 66)[0];
    let t = s
        .inverse(b.page, b.h + b.width / 2.0, b.v_top + b.height / 2.0)
        .expect("a line inside the box");
    assert_eq!((t.file, t.line), (dir.join("main.tex"), 66));
}

#[test]
fn missing_data_gives_nothing() {
    assert!(Scanner::open(Path::new("/nonexistent/lemma/main.pdf")).is_none());
    // A PDF without synctex data next to it.
    assert!(Scanner::open(&fixture().join("../pdf/test.pdf")).is_none());

    let (dir, mut s) = open_fixture();
    assert!(s.forward(&dir.join("nonexistent.tex"), 3).is_empty());
    assert!(s.forward(&dir.join("refs.bib"), 3).is_empty());
    assert!(s.forward(&dir.join("main.tex"), 0).is_empty());
    assert!(s.forward(Path::new(""), 66).is_empty());
    assert!(s.inverse(0, 200.0, 420.0).is_none());
    assert!(s.inverse(99, 200.0, 420.0).is_none());
    assert!(s.inverse(2, f32::NAN, 420.0).is_none());
    assert!(s.inverse(2, 200.0, f32::INFINITY).is_none());
}

#[test]
fn relative_paths_are_resolved_against_the_current_directory() {
    let dir = fixture();
    let rel = relative_from_cwd(&dir);
    assert!(rel.is_relative());
    let mut s = Scanner::open(&rel.join("main.pdf")).expect("opens by a relative path");
    assert_eq!(s.forward(&rel.join("main.tex"), 66).len(), 1);
    let t = s.inverse(2, 200.0, 420.0).expect("a line at (200, 420)");
    assert_eq!(t.file, dir.join("main.tex"));
}

#[test]
fn a_copied_project_maps_to_the_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().canonicalize().unwrap().join("copy");
    fs::create_dir_all(copy.join("chapters")).unwrap();
    for f in [
        "main.pdf",
        "main.synctex.gz",
        "main.tex",
        "chapters/yoneda.tex",
    ] {
        fs::copy(fixture().join(f), copy.join(f)).unwrap();
    }
    let mut s = Scanner::open(&copy.join("main.pdf")).expect("copied synctex opens");
    let boxes = s.forward(&copy.join("main.tex"), 66);
    assert!(
        boxes
            .iter()
            .any(|b| near(b, 2, 126.67, 415.05, 358.66, 9.70)),
        "{boxes:?}"
    );
    assert!(!s.forward(&copy.join("chapters/yoneda.tex"), 26).is_empty());
    let t = s.inverse(2, 200.0, 240.0).expect("a line at (200, 240)");
    assert_eq!((t.file, t.line), (copy.join("chapters/yoneda.tex"), 25));
    // The repository's fixture is now a different file with the same name.
    assert!(s.forward(&fixture().join("main.tex"), 66).is_empty());
}

#[test]
fn scanners_move_between_threads_and_run_concurrently() {
    fn assert_send<T: Send>() {}
    assert_send::<Scanner>();

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (dir, mut s) = open_fixture();
            std::thread::spawn(move || {
                for line in 1..=200 {
                    s.forward(&dir.join("main.tex"), line);
                    s.inverse(2, 200.0, 420.0);
                }
                s.forward(&dir.join("main.tex"), 66)
            })
        })
        .collect();
    for t in threads {
        let boxes = t.join().expect("query thread");
        assert!(
            boxes
                .iter()
                .any(|b| near(b, 2, 126.67, 415.05, 358.66, 9.70))
        );
    }
}

#[test]
#[ignore = "perf: cargo test --release -- --ignored perf"]
fn perf_queries() {
    let (dir, mut s) = open_fixture();
    let (main, yoneda) = (dir.join("main.tex"), dir.join("chapters/yoneda.tex"));
    let t = Instant::now();
    for i in 0..1000u32 {
        let boxes = if i % 2 == 0 {
            s.forward(&main, 30 + i % 200)
        } else {
            s.forward(&yoneda, 1 + i % 29)
        };
        std::hint::black_box(boxes);
    }
    let forward = t.elapsed();
    let t = Instant::now();
    for i in 0..1000u32 {
        let target = s.inverse(1 + i % 5, (100 + i % 350) as f32, (80 + i % 620) as f32);
        std::hint::black_box(target);
    }
    let inverse = t.elapsed();
    println!("1000 forward: {forward:?}; 1000 inverse: {inverse:?}");
    assert!(forward < Duration::from_millis(100), "{forward:?}");
    assert!(inverse < Duration::from_millis(100), "{inverse:?}");
}
