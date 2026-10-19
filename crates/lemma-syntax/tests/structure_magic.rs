//! Magic comments, checked against the regexes and samples in `lemma_syntax::magic`.

use lemma_syntax::{Magic, parse_magic};
use std::path::PathBuf;

fn magic(root: Option<&str>, program: Option<&str>, parent: Option<&str>) -> Magic {
    Magic {
        root: root.map(PathBuf::from),
        program: program.map(str::to_owned),
        subfiles_parent: parent.map(PathBuf::from),
    }
}

#[test]
fn research_samples() {
    let cases = [
        (
            "% !TEX program = lualatex\n\\documentclass[11pt]{amsart}\n",
            magic(None, Some("lualatex"), None),
        ),
        (
            "%!TeX TS-program = xelatex\n% !TeX root = ../main.tex\n\\section{x}\n",
            magic(Some("../main.tex"), Some("xelatex"), None),
        ),
        (
            "% \\documentclass{article}\n\\input{chapters/yoneda}\n  \\include {ch2} % comment\n\\subimport{parts/}{intro}\n",
            magic(None, None, None),
        ),
        (
            "\\documentclass[../main.tex]{subfiles}\n\\begin{document}\n",
            magic(None, None, Some("../main.tex")),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse_magic(text), want, "{text:?}");
    }
}

#[test]
fn root_comment_variants() {
    let root = |text: &str| parse_magic(text).root;
    assert_eq!(
        root("% !TEX root = main.tex"),
        Some(PathBuf::from("main.tex"))
    );
    assert_eq!(
        root("  \t%\t! TeX root=main.tex  \r\n"),
        Some(PathBuf::from("main.tex"))
    );
    assert_eq!(
        root("% !tex ROOT = dir with space/main.tex"),
        Some(PathBuf::from("dir with space/main.tex"))
    );
    assert_eq!(
        root("x % !TEX root = main.tex"),
        None,
        "must start the line"
    );
    assert_eq!(
        root("%% !TEX root = main.tex"),
        None,
        "only one % before the !"
    );
    assert_eq!(root("% TEX root = main.tex"), None, "the ! is required");
    assert_eq!(root("% !TeXroot = main.tex"), None);
    assert_eq!(root("% !TEX root ="), None, "empty value");
    assert_eq!(
        root("% !TEX root = a.tex\n% !TEX root = b.tex"),
        Some(PathBuf::from("a.tex"))
    );
}

#[test]
fn byte_order_mark_before_the_first_line() {
    let m = parse_magic("\u{feff}% !TEX root = main.tex\n");
    assert_eq!(m.root, Some(PathBuf::from("main.tex")));
}

#[test]
fn program_comment_variants() {
    let program = |text: &str| parse_magic(text).program;
    assert_eq!(
        program("% !TEX program = pdflatex"),
        Some("pdflatex".into())
    );
    assert_eq!(
        program("% !TEX TS-program = LuaLaTeX"),
        Some("lualatex".into())
    );
    assert_eq!(program("% !TeX ts-program=xelatex"), Some("xelatex".into()));
    assert_eq!(
        program("% !TEX program = pdflatex -shell-escape"),
        None,
        "letters only"
    );
    assert_eq!(program("% !TEX programme = pdflatex"), None);
}

#[test]
fn subfiles_parent_variants() {
    let parent = |text: &str| parse_magic(text).subfiles_parent;
    assert_eq!(
        parent("\\documentclass [ main ] { subfiles }"),
        Some(PathBuf::from("main"))
    );
    assert_eq!(parent("\\documentclass[main.tex]{article}"), None);
    assert_eq!(parent("% \\documentclass[main.tex]{subfiles}"), None);
    assert_eq!(parent("x % \\documentclass[main.tex]{subfiles}"), None);
    assert_eq!(parent("\\documentclass{subfiles}"), None);
    assert_eq!(parent("\\documentclass[]{subfiles}"), None);
    assert_eq!(
        parent("\\documentclass{article}\n\\documentclass[x]{subfiles}"),
        None,
        "only the first \\documentclass counts"
    );
}
