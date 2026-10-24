//! Regression tests for SyncTeX path matching defects found in the M1 review (area C), on
//! hand-written synctex files (the same format as `tests/paths.rs`).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use lemma_synctex::{Scanner, SyncTarget};

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

/// A canonical working directory nested deep inside a fresh temporary directory, so that the
/// scanner's search above the PDF never leaves the temporary directory.
fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = lemma_core::canonicalize(tmp.path())
        .unwrap()
        .join("w/x/y/z");
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

/// C3: `build.outdir = "../build"` puts the PDF outside TeX's working directory although the
/// project was not moved. A same-named file above the output directory (here `<dir>/main.tex`
/// next to the project) must not be taken for the root: the recorded working directory still
/// holds the root file, so its recorded paths are right.
#[test]
fn regress_outdir_outside_project_reanchors_at_unrelated_file() {
    let (_tmp, dir) = tempdir();
    let proj = dir.join("proj");
    touch(&proj, &["main.tex", "chapters/intro.tex"]);
    // An unrelated document one level above the output directory.
    touch(&dir, &["main.tex"]);
    let pdf = dir.join("build/main.pdf");
    write_synctex(
        &pdf,
        &[
            (1, format!("{}/./main.tex", proj.display())),
            (2, format!("{}/./chapters/intro.tex", proj.display())),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&pdf).unwrap();
    println!("{s:?}");
    assert_eq!(
        forward(&mut s, &proj.join("main.tex"), 10),
        [node_box(0)],
        "forward search from the real root finds its box"
    );
    assert_eq!(
        inverse(&mut s, 0),
        target(proj.join("main.tex"), 10),
        "inverse search opens the real root, not the unrelated {}",
        dir.join("main.tex").display()
    );
    assert!(
        forward(&mut s, &dir.join("main.tex"), 10).is_empty(),
        "the unrelated file has no boxes"
    );
}

/// C5: `\input{link}` where `link.tex` is a symlink to `real/chap.tex`. TeX records
/// `<cwd>/./link.tex`; the editor sends the canonical path `real/chap.tex`. Inverse search maps
/// the text to `real/chap.tex`, so forward search from that file must find the same boxes.
#[test]
#[cfg(unix)]
fn regress_forward_through_renamed_symlink() {
    let (_tmp, dir) = tempdir();
    touch(&dir, &["main.tex", "real/chap.tex"]);
    std::os::unix::fs::symlink(dir.join("real/chap.tex"), dir.join("link.tex")).unwrap();
    write_synctex(
        &dir.join("main.pdf"),
        &[
            (1, format!("{}/./main.tex", dir.display())),
            (2, format!("{}/./link.tex", dir.display())),
        ],
        &[(1, 10), (2, 5)],
    );
    let mut s = Scanner::open(&dir.join("main.pdf")).unwrap();
    // Inverse search resolves the symlink to its target ...
    assert_eq!(inverse(&mut s, 1), target(dir.join("real/chap.tex"), 5));
    // ... so forward search from the target (what the editor sends) must match it.
    assert_eq!(
        forward(&mut s, &dir.join("real/chap.tex"), 5),
        [node_box(1)],
        "forward search from the symlink's target"
    );
}
