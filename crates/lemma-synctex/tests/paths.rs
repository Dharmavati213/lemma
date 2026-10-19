//! How recorded input names are matched to files on disk, on hand-written synctex files.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use lemma_synctex::{Scanner, SyncTarget};

/// A recorded path prefix that does not exist on any machine running the tests.
const GONE: &str = "/nonexistent-lemma-synctex/proj";

/// Baseline (bp) of the `k`-th node written by [`write_synctex`].
fn baseline(k: usize) -> f32 {
    100.0 + 50.0 * k as f32
}

/// Writes `pdf` (empty) and its uncompressed `.synctex` file: the given `Input:` records and,
/// for each `(tag, line)` node, one 300×10 bp hbox on page 1 with its baseline at
/// [`baseline`]`(k)`.
fn write_synctex(pdf: &Path, inputs: &[(i32, String)], nodes: &[(i32, u32)]) {
    const SP_PER_BP: f64 = 65781.76;
    let sp = |bp: f64| (bp * SP_PER_BP).round() as i64;
    let mut s = String::from("SyncTeX Version:1\n");
    for (tag, name) in inputs {
        writeln!(s, "Input:{tag}:{name}").unwrap();
    }
    s.push_str("Output:pdf\nMagnification:1000\nUnit:1\nX Offset:0\nY Offset:0\nContent:\n{1\n");
    writeln!(
        s,
        "[1,1:{},{}:{},{},0",
        sp(72.0),
        sp(720.0),
        sp(468.0),
        sp(648.0)
    )
    .unwrap();
    for (k, (tag, line)) in nodes.iter().enumerate() {
        let (h, v) = (sp(72.0), sp(baseline(k).into()));
        writeln!(
            s,
            "({tag},{line}:{h},{v}:{},{},{}",
            sp(300.0),
            sp(8.0),
            sp(2.0)
        )
        .unwrap();
        writeln!(s, "g{tag},{line}:{h},{v}\n)").unwrap();
    }
    s.push_str("]\n}1\nPostamble:\nCount:1\nPost scriptum:\n");
    fs::create_dir_all(pdf.parent().unwrap()).unwrap();
    fs::write(pdf.with_extension("synctex"), s).unwrap();
    fs::write(pdf, b"").unwrap();
}

fn touch(dir: &Path, files: &[&str]) {
    for f in files {
        let p = dir.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"%\n").unwrap();
    }
}

/// A canonical working directory inside a fresh temporary directory (the recorded names below
/// embed it). It is nested deep enough that the scanner's search for a moved project's root
/// never looks outside the temporary directory, e.g. at a stray `/tmp/main.tex`.
fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().canonicalize().unwrap().join("w/x/y/z");
    fs::create_dir_all(&path).unwrap();
    (tmp, path)
}

/// The page-1 box of the `k`-th node, as `(v_top, height)`.
fn node_box(k: usize) -> (f32, f32) {
    (baseline(k) - 8.0, 10.0)
}

/// The boxes of `forward` as `(v_top, height)`, rounded to 0.01 bp.
fn forward(s: &mut Scanner, file: &Path, line: u32) -> Vec<(f32, f32)> {
    s.forward(file, line)
        .iter()
        .map(|b| {
            assert_eq!(b.page, 1);
            (
                (b.v_top * 100.0).round() / 100.0,
                (b.height * 100.0).round() / 100.0,
            )
        })
        .collect()
}

/// Inverse search in the middle of the `k`-th node.
fn inverse(s: &mut Scanner, k: usize) -> Option<SyncTarget> {
    s.inverse(1, 200.0, baseline(k) - 4.0)
}

fn target(file: impl Into<PathBuf>, line: u32) -> Option<SyncTarget> {
    Some(SyncTarget {
        file: file.into(),
        line,
    })
}

#[test]
fn box_geometry_follows_the_contract() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex"]);
    write_synctex(
        &dir.join("main.pdf"),
        &[(1, format!("{}/./main.tex", dir.display()))],
        &[(1, 10)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    let b = s.forward(&dir.join("main.tex"), 10);
    assert_eq!(b.len(), 1);
    // v_top = baseline − height, height = height + depth, width = |width|.
    let close = |a: f32, b: f32| (a - b).abs() < 0.01;
    assert!(close(b[0].h, 72.0) && close(b[0].width, 300.0), "{b:?}");
    assert!(close(b[0].v_top, 92.0) && close(b[0].height, 10.0), "{b:?}");
}

#[test]
fn moved_project_is_reanchored_at_the_pdf_directory() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex", "chapters/intro.tex"]);
    write_synctex(
        &dir.join("main.pdf"),
        &[
            (1, format!("{GONE}/./main.tex")),
            (2, format!("{GONE}/./chapters/intro.tex")),
            (3, "/usr/share/texmf-dist/tex/latex/base/article.cls".into()),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    assert_eq!(forward(&mut s, &dir.join("main.tex"), 10), [node_box(0)]);
    assert_eq!(
        forward(&mut s, &dir.join("chapters/intro.tex"), 5),
        [node_box(1)]
    );
    assert_eq!(inverse(&mut s, 0), target(dir.join("main.tex"), 10));
    assert_eq!(
        inverse(&mut s, 1),
        target(dir.join("chapters/intro.tex"), 5)
    );
}

#[test]
fn moved_project_with_an_output_directory() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex", "chapters/intro.tex"]);
    write_synctex(
        &dir.join(".build/main.pdf"),
        &[
            (1, format!("{GONE}/./main.tex")),
            (2, format!("{GONE}/./chapters/intro.tex")),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&dir.join(".build/main.pdf")).unwrap();
    assert_eq!(
        forward(&mut s, &dir.join("chapters/intro.tex"), 5),
        [node_box(1)]
    );
    assert_eq!(inverse(&mut s, 0), target(dir.join("main.tex"), 10));
}

#[test]
fn a_moved_root_is_searched_only_a_few_levels_above_the_pdf() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex"]);
    let pdf = dir.join("1/2/3/4/main.pdf");
    write_synctex(&pdf, &[(1, format!("{GONE}/./main.tex"))], &[(1, 10)]);
    let mut s = Scanner::open(&pdf).unwrap();
    assert_eq!(inverse(&mut s, 0), target(format!("{GONE}/main.tex"), 10));
}

#[test]
fn recorded_paths_are_trusted_when_the_pdf_lies_below_the_build_directory() {
    let (_tmp, dir) = tempdir();
    let proj = dir.join("proj");
    // A stray copy inside the output directory must not shadow the real source.
    touch(&proj, &["main.tex", ".build/main.tex"]);
    write_synctex(
        &proj.join(".build/main.pdf"),
        &[(1, format!("{}/./main.tex", proj.display()))],
        &[(1, 10)],
    );
    let mut s = Scanner::open(&proj.join(".build/main.pdf")).unwrap();
    assert_eq!(inverse(&mut s, 0), target(proj.join("main.tex"), 10));
    assert_eq!(forward(&mut s, &proj.join("main.tex"), 10), [node_box(0)]);
    assert!(forward(&mut s, &proj.join(".build/main.tex"), 10).is_empty());
}

#[test]
fn recorded_paths_are_trusted_when_the_output_directory_lies_elsewhere() {
    // `build.outdir` pointing outside the project (here `<dir>/out/latex`), with an unrelated
    // `main.tex` above the output directory: the project was not moved, so its recorded paths
    // are right. (The decoy lacks the project's other sources, so it is no copy of it.)
    let (_tmp, dir) = tempdir();
    let proj = dir.join("proj");
    touch(&proj, &["main.tex", "chapters/intro.tex"]);
    touch(&dir, &["out/main.tex"]);
    let pdf = dir.join("out/latex/main.pdf");
    write_synctex(
        &pdf,
        &[
            (1, format!("{}/./main.tex", proj.display())),
            (2, format!("{}/./chapters/intro.tex", proj.display())),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&pdf).unwrap();
    assert_eq!(forward(&mut s, &proj.join("main.tex"), 10), [node_box(0)]);
    assert!(forward(&mut s, &dir.join("out/main.tex"), 10).is_empty());
    assert_eq!(inverse(&mut s, 0), target(proj.join("main.tex"), 10));
    assert_eq!(
        inverse(&mut s, 1),
        target(proj.join("chapters/intro.tex"), 5)
    );
}

#[test]
fn copied_project_maps_to_the_copy_not_the_original() {
    let (_tmp, dir) = tempdir();
    let (original, copy) = (dir.join("a"), dir.join("b"));
    touch(&original, &["main.tex"]);
    touch(&copy, &["main.tex"]);
    write_synctex(
        &copy.join("main.pdf"),
        &[(1, format!("{}/./main.tex", original.display()))],
        &[(1, 10)],
    );
    let mut s = Scanner::open(&copy.join("main.pdf")).unwrap();
    assert_eq!(forward(&mut s, &copy.join("main.tex"), 10), [node_box(0)]);
    assert!(forward(&mut s, &original.join("main.tex"), 10).is_empty());
    assert_eq!(inverse(&mut s, 0), target(copy.join("main.tex"), 10));
}

#[test]
fn other_files_with_the_same_name_never_match() {
    let (_tmp, dir) = tempdir();
    touch(
        &dir,
        &[
            "main.tex",
            "chapters/intro.tex",
            "sub/main.tex",
            "other/intro.tex",
        ],
    );
    write_synctex(
        &dir.join("main.pdf"),
        &[
            (1, format!("{}/./main.tex", dir.display())),
            (2, format!("{}/./chapters/intro.tex", dir.display())),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    assert!(forward(&mut s, &dir.join("sub/main.tex"), 10).is_empty());
    assert!(forward(&mut s, &dir.join("other/intro.tex"), 5).is_empty());
    assert_eq!(
        forward(&mut s, &dir.join("chapters/intro.tex"), 5),
        [node_box(1)]
    );
}

#[test]
fn stale_synctex_matches_by_the_longest_unambiguous_suffix() {
    // Only the PDF and its synctex file are left; none of the recorded sources exist.
    let (_tmp, dir) = tempdir();
    write_synctex(
        &dir.join("main.pdf"),
        &[
            (1, format!("{GONE}/./main.tex")),
            (2, format!("{GONE}/./a/intro.tex")),
            (3, format!("{GONE}/./b/intro.tex")),
        ],
        &[(1, 10), (2, 5), (3, 5)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    assert_eq!(forward(&mut s, &dir.join("main.tex"), 10), [node_box(0)]);
    assert_eq!(forward(&mut s, &dir.join("a/intro.tex"), 5), [node_box(1)]);
    assert_eq!(forward(&mut s, &dir.join("b/intro.tex"), 5), [node_box(2)]);
    assert!(forward(&mut s, &dir.join("c/intro.tex"), 5).is_empty());
    assert_eq!(inverse(&mut s, 1), target(format!("{GONE}/a/intro.tex"), 5));
}

#[test]
fn relative_input_names() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex", "chapters/intro.tex"]);
    write_synctex(
        &dir.join(".build/main.pdf"),
        &[(1, "main.tex".into()), (2, "./chapters/intro.tex".into())],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&dir.join(".build/main.pdf")).unwrap();
    assert_eq!(forward(&mut s, &dir.join("main.tex"), 10), [node_box(0)]);
    assert_eq!(
        forward(&mut s, &dir.join("chapters/intro.tex"), 5),
        [node_box(1)]
    );
    assert_eq!(
        inverse(&mut s, 1),
        target(dir.join("chapters/intro.tex"), 5)
    );
}

#[test]
fn a_file_recorded_under_two_names_gives_the_boxes_of_both() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex", "chapters/intro.tex"]);
    write_synctex(
        &dir.join("main.pdf"),
        &[
            (1, format!("{}/./main.tex", dir.display())),
            (2, format!("{}/./chapters/intro.tex", dir.display())),
            (3, "chapters/intro.tex".into()),
        ],
        &[(2, 5), (3, 5)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    assert_eq!(
        forward(&mut s, &dir.join("chapters/intro.tex"), 5),
        [node_box(0), node_box(1)]
    );
}

#[test]
fn symlinked_paths_match_their_target() {
    let (_tmp, dir) = tempdir();
    let proj = dir.join("proj");
    touch(&proj, &["main.tex"]);
    std::os::unix::fs::symlink(&proj, dir.join("link")).unwrap();
    write_synctex(
        &proj.join("main.pdf"),
        &[(1, format!("{}/./main.tex", proj.display()))],
        &[(1, 10)],
    );
    let mut s = Scanner::open(&dir.join("link/main.pdf")).unwrap();
    assert_eq!(
        forward(&mut s, &dir.join("link/main.tex"), 10),
        [node_box(0)]
    );
    assert_eq!(inverse(&mut s, 0), target(proj.join("main.tex"), 10));

    // `..` after a symlink leads to the parent of its target, as in the file system.
    touch(&proj, &["chapters/x.tex"]);
    fs::create_dir(dir.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(proj.join("chapters"), dir.join("elsewhere/ch")).unwrap();
    assert_eq!(
        forward(&mut s, &dir.join("elsewhere/ch/../main.tex"), 10),
        [node_box(0)]
    );
}

#[test]
fn unreadable_synctex_gives_none() {
    let (_tmp, dir) = tempdir();
    fs::write(dir.join("garbage.synctex"), "not a synctex file\n").unwrap();
    fs::write(dir.join("empty.synctex"), "").unwrap();
    fs::write(dir.join("broken.synctex.gz"), "not gzip").unwrap();
    for pdf in ["garbage.pdf", "empty.pdf", "broken.pdf", "absent.pdf"] {
        assert!(Scanner::open(&dir.join(pdf)).is_none(), "{pdf}");
    }
}
