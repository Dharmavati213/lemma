//! WP1.9 acceptance: `ProjectIndex` queries on the fixture project and a synthetic include graph.

use lemma_syntax::{FileIndex, ProjectIndex, index_file, scan_bib};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proj")
}

/// The fixture project, keyed by `/proj/...` paths (resolution is lexical, so any absolute
/// prefix works).
fn fixture_project() -> ProjectIndex {
    let dir = fixture_dir();
    let read = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap();
    let mut idx = ProjectIndex::default();
    idx.update_file(
        "/proj/main.tex".into(),
        Arc::new(index_file(&read("main.tex"))),
    );
    idx.update_file(
        "/proj/chapters/yoneda.tex".into(),
        Arc::new(index_file(&read("chapters/yoneda.tex"))),
    );
    idx.bibs.insert(
        "/proj/refs.bib".into(),
        Arc::new(scan_bib(&read("refs.bib"))),
    );
    idx
}

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

#[test]
fn fixture_project_queries() {
    let idx = fixture_project();
    let main = Path::new("/proj/main.tex");

    assert_eq!(
        idx.project_files(main),
        paths(&["/proj/main.tex", "/proj/chapters/yoneda.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/proj/chapters/yoneda.tex")),
        paths(&["/proj/main.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/proj/refs.bib")),
        paths(&["/proj/main.tex"])
    );
    assert!(idx.includers_of(main).is_empty());

    let (file, label) = idx.find_label("lem:yoneda").unwrap();
    assert_eq!(file, Path::new("/proj/chapters/yoneda.tex"));
    assert_eq!(label.env.as_deref(), Some("lemma"));
    assert_eq!(label.title.as_deref(), Some("Yoneda"));
    assert!(idx.find_label("nope").is_none());

    let labels = idx.all_labels();
    assert_eq!(labels.len(), 14);
    // Sorted by path: chapters/yoneda.tex before main.tex.
    assert_eq!(labels[0].1.name, "sec:yoneda");
    assert_eq!(labels[3].1.name, "sec:intro");

    let keys: Vec<_> = idx.all_bib().iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, ["SGA1", "Vistoli", "Stacks", "MacLane"]);
    assert_eq!(
        idx.packages(),
        [
            "amsmath",
            "amssymb",
            "amsthm",
            "biblatex",
            "hyperref",
            "mathtools",
            "tikz-cd"
        ]
    );
}

#[test]
fn theorem_names_feed_the_chapter_index() {
    let mut idx = ProjectIndex::default();
    let preamble = "\\newtheorem{satz}{Satz}\\newtheorem{lemma}[satz]{Lemma}\\declaretheorem{bem}";
    idx.update_file("/p/preamble.tex".into(), Arc::new(index_file(preamble)));
    idx.update_file(
        "/p/other.tex".into(),
        Arc::new(index_file("\\newtheorem{satz}{Satz}")),
    );
    assert_eq!(idx.theorem_names(), ["bem", "lemma", "satz"]);
    let chapter =
        lemma_syntax::index_file_with_theorems("\\begin{bem}\\end{bem}", &idx.theorem_names());
    assert_eq!(chapter.outline[0].env.as_deref(), Some("bem"));
}

#[test]
fn update_file_replaces() {
    let mut idx = fixture_project();
    idx.update_file(
        "/proj/chapters/yoneda.tex".into(),
        Arc::new(FileIndex::default()),
    );
    assert!(idx.find_label("lem:yoneda").is_none());
    assert_eq!(idx.files.len(), 2);
}

fn synthetic() -> ProjectIndex {
    let files = [
        (
            "/r/main.tex",
            "\\documentclass{book}\\usepackage{defs}\\begin{document}\n\\input{a}\n\\include{chapters/b}\n\\subfile{parts/c}\n\\import{imp/}{d}\n\\input{missing/x.tex}\n\\bibliography{bib/refs}\n\\includegraphics{fig/plot}\n\\end{document}",
        ),
        ("/r/a.tex", "\\input{chapters/e}\n\\input{a}"),
        ("/r/chapters/b.tex", "\\input{a}\n\\input{./chapters/../a}"),
        (
            "/r/parts/c.tex",
            "\\documentclass[../main]{subfiles}\n\\subfile{sub/x}",
        ),
        ("/r/parts/sub/x.tex", ""),
        (
            "/r/imp/d.tex",
            "\\input{local}\n\\input{chapters/e}\n\\subimport{deeper/}{f}",
        ),
        ("/r/chapters/e.tex", ""),
        ("/other/z.tex", "\\input{a}"),
    ];
    let mut idx = ProjectIndex::default();
    for (path, text) in files {
        idx.update_file(path.into(), Arc::new(index_file(text)));
    }
    idx
}

#[test]
fn project_files_follow_latex_resolution() {
    let idx = synthetic();
    assert_eq!(
        idx.project_files(Path::new("/r/main.tex")),
        paths(&[
            "/r/main.tex",
            "/r/a.tex",
            // \input in a.tex is relative to the root's directory.
            "/r/chapters/e.tex",
            "/r/chapters/b.tex",
            "/r/parts/c.tex",
            // \subfile is relative to the including subfile.
            "/r/parts/sub/x.tex",
            "/r/imp/d.tex",
            // Inside an \import, \input looks in the import directory first ...
            "/r/imp/local.tex",
            // ... and \subimport nests below it.
            "/r/imp/deeper/f.tex",
            // Not indexed: listed but not followed.
            "/r/missing/x.tex",
        ])
    );
    assert_eq!(
        idx.project_files(Path::new("/r/none.tex")),
        paths(&["/r/none.tex"])
    );
}

#[test]
fn includers_use_every_include_kind() {
    let idx = synthetic();
    assert_eq!(
        idx.includers_of(Path::new("/r/a.tex")),
        paths(&["/r/chapters/b.tex", "/r/main.tex"]),
        "a.tex including itself does not count"
    );
    assert_eq!(
        idx.includers_of(Path::new("/r/chapters/e.tex")),
        paths(&["/r/a.tex", "/r/imp/d.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/r/parts/sub/x.tex")),
        paths(&["/r/parts/c.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/r/bib/refs.bib")),
        paths(&["/r/main.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/r/defs.sty")),
        paths(&["/r/main.tex"])
    );
    assert_eq!(
        idx.includers_of(Path::new("/r/fig/plot.png")),
        paths(&["/r/main.tex"])
    );
    assert_eq!(idx.includers_of(Path::new("/r/./imp/../a.tex")).len(), 2);
}
