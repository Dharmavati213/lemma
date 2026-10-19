//! WP1.9 acceptance: `index_file` on the fixture project and on targeted snippets.

use lemma_syntax::{
    Citation, EnvPair, FileIndex, Include, IncludeKind, Label, MacroDef, OutlineItem, Reference,
    index_file, index_file_with_theorems,
};
use std::path::PathBuf;

fn fixture(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn item(level: u8, title: &str, line: u32, starred: bool, env: Option<&str>) -> OutlineItem {
    OutlineItem {
        level,
        title: title.to_owned(),
        line,
        starred,
        env: env.map(str::to_owned),
    }
}

fn label(name: &str, line: u32, env: Option<&str>, title: Option<&str>) -> Label {
    Label {
        name: name.to_owned(),
        line,
        env: env.map(str::to_owned),
        title: title.map(str::to_owned),
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn titles(idx: &FileIndex) -> Vec<&str> {
    idx.outline.iter().map(|o| o.title.as_str()).collect()
}

#[test]
fn fixture_main_outline_and_labels() {
    let idx = index_file(&fixture("proj/main.tex"));
    assert!(idx.has_documentclass);
    assert!(idx.has_begin_document);

    let sections: Vec<_> = idx.outline.iter().filter(|o| o.level < 7).collect();
    assert_eq!(sections.len(), 10);
    assert_eq!(*sections[0], item(2, "Introduction", 38, false, None));
    assert_eq!(*sections[3], item(3, "Long computation", 122, false, None));

    let theorems: Vec<_> = idx.outline.iter().filter(|o| o.level == 7).collect();
    let envs: Vec<_> = theorems.iter().map(|o| o.env.as_deref().unwrap()).collect();
    assert_eq!(
        envs,
        ["definition", "theorem", "remark", "proposition", "example"]
    );
    assert_eq!(
        *theorems[1],
        item(7, "fpqc descent", 59, false, Some("theorem"))
    );
    assert_eq!(theorems[0].title, "");

    // The titles agree with what hyperref wrote into main.aux (`\newlabel{x}{{n}{p}{title}…}`).
    let find = |n: &str| idx.labels.iter().find(|l| l.name == n).unwrap().clone();
    assert_eq!(
        find("sec:intro"),
        label("sec:intro", 38, None, Some("Introduction"))
    );
    assert_eq!(
        find("def:descent-datum"),
        label("def:descent-datum", 48, Some("definition"), None)
    );
    assert_eq!(
        find("eq:cocycle"),
        label("eq:cocycle", 53, Some("equation"), None)
    );
    assert_eq!(
        find("thm:fpqc"),
        label("thm:fpqc", 59, Some("theorem"), Some("fpqc descent"))
    );
    assert_eq!(
        find("eq:amitsur"),
        label("eq:amitsur", 72, Some("equation"), Some("Descent"))
    );
    assert_eq!(
        find("eq:adj2"),
        label("eq:adj2", 126, Some("align"), Some("Long computation"))
    );
    assert_eq!(idx.labels.len(), 11);
}

#[test]
fn fixture_main_refs_cites_includes_macros() {
    let idx = index_file(&fixture("proj/main.tex"));

    assert_eq!(idx.refs.len(), 13);
    assert_eq!(
        idx.refs[4],
        Reference {
            cmd: "eqref".into(),
            names: names(&["eq:cocycle"]),
            line: 77
        }
    );
    assert_eq!(idx.cites.len(), 10);
    assert_eq!(
        idx.cites[3],
        Citation {
            cmd: "cite".into(),
            keys: names(&["Stacks"]),
            line: 69
        },
        "\\cite[Tag 023N]{{Stacks}}"
    );

    assert_eq!(
        idx.packages,
        names(&[
            "amsmath",
            "amsthm",
            "amssymb",
            "mathtools",
            "tikz-cd",
            "biblatex",
            "hyperref"
        ])
    );
    let non_packages: Vec<_> = idx
        .includes
        .iter()
        .filter(|i| i.kind != IncludeKind::Package)
        .cloned()
        .collect();
    assert_eq!(
        non_packages,
        vec![
            Include {
                kind: IncludeKind::Bibliography,
                target: "refs.bib".into(),
                line: 6
            },
            Include {
                kind: IncludeKind::Input,
                target: "chapters/yoneda".into(),
                line: 45
            },
        ]
    );

    let theorems: Vec<_> = idx
        .macros
        .iter()
        .filter(|m| m.is_theorem)
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        theorems,
        [
            "theorem",
            "lemma",
            "proposition",
            "corollary",
            "definition",
            "example",
            "remark"
        ]
    );
    let co = idx.macros.iter().find(|m| m.name == "cO").unwrap();
    assert_eq!(co.body.as_deref(), Some("\\mathcal{O}"));
    assert_eq!((co.line, co.nargs, co.is_env), (20, 0, false));

    let document = idx.envs.iter().find(|e| e.name == "document").unwrap();
    assert_eq!((document.begin_line, document.end_line), (28, Some(234)));
    assert!(idx.envs.iter().all(|e| e.end_line.is_some()));
}

#[test]
fn fixture_chapter() {
    let idx = index_file(&fixture("proj/chapters/yoneda.tex"));
    assert!(!idx.has_documentclass);
    assert!(!idx.has_begin_document);
    assert_eq!(idx.outline[0], item(2, "The Yoneda lemma", 0, false, None));
    // `lemma` and `corollary` are declared in main.tex; the default theorem list covers them.
    assert_eq!(idx.outline[1], item(7, "Yoneda", 3, false, Some("lemma")));
    assert_eq!(idx.outline[2], item(7, "", 24, false, Some("corollary")));
    assert_eq!(
        idx.labels,
        vec![
            label("sec:yoneda", 0, None, Some("The Yoneda lemma")),
            label("lem:yoneda", 3, Some("lemma"), Some("Yoneda")),
            label("cor:ff", 24, Some("corollary"), None),
        ]
    );
    assert_eq!(idx.cites[0].keys, names(&["MacLane"]));
}

#[test]
fn outline_koma_starred_short_titles_and_theorems() {
    let text = r"\documentclass{scrbook}
\newtheorem{satz}{Satz}[chapter]
\newtheorem*{claim*}{Claim}
\begin{document}
\addpart{Grundlagen}
\part*{Unnumbered part}
\chapter[Short]{A long
  chapter title}
\addchap{Preface}
\addsec*{Notes}
\section *{Starred \emph{section}\label{sec:starred}}
\subsection[Toc entry]{Sub % comment
}
\subsubsection{Subsub}
\paragraph{Para}
\subparagraph{Subpara}
\minisec{Mini}
\begin{satz}[Haupt]\label{satz:main}
\end{satz}
\begin{claim*}
\end{claim*}
\begin{proof}
\end{proof}
\begin{lemma} [Zorn]
\end{lemma}
\begin{theorem*}
\end{theorem*}
\section{Unclosed title
\section{After}
\end{document}
";
    let idx = index_file(text);
    assert_eq!(
        idx.outline,
        vec![
            item(0, "Grundlagen", 4, true, None),
            item(0, "Unnumbered part", 5, true, None),
            item(1, "A long chapter title", 6, false, None),
            item(1, "Preface", 8, true, None),
            item(2, "Notes", 9, true, None),
            item(2, "Starred \\emph{section}", 10, true, None),
            item(3, "Sub", 11, false, None),
            item(4, "Subsub", 13, false, None),
            item(5, "Para", 14, false, None),
            item(6, "Subpara", 15, false, None),
            item(5, "Mini", 16, true, None),
            item(7, "Haupt", 17, false, Some("satz")),
            item(7, "", 19, true, Some("claim*")),
            item(7, "Zorn", 23, false, Some("lemma")),
            item(7, "", 25, true, Some("theorem*")),
            item(2, "After", 28, false, None),
        ]
    );
    assert_eq!(
        idx.labels[0],
        label("sec:starred", 10, None, Some("Starred \\emph{section}"))
    );
    assert_eq!(
        idx.labels[1],
        label("satz:main", 17, Some("satz"), Some("Haupt"))
    );
}

#[test]
fn theorems_declared_in_another_file() {
    let chapter = "\\begin{satz}[Haupt]\\label{s}\n\\end{satz}\n\\begin{lemma}\\end{lemma}\n";
    let plain = index_file(chapter);
    assert_eq!(titles(&plain), [""], "only the default `lemma`");
    let idx = index_file_with_theorems(chapter, &["satz".to_owned()]);
    assert_eq!(
        idx.outline,
        vec![
            item(7, "Haupt", 0, false, Some("satz")),
            item(7, "", 2, false, Some("lemma")),
        ]
    );
    assert_eq!(idx.labels[0], label("s", 0, Some("satz"), Some("Haupt")));
}

#[test]
fn newenvironment_is_not_theorem_like() {
    let idx = index_file("\\newenvironment{example}{}{}\n\\begin{example}\\end{example}\n");
    assert!(idx.outline.is_empty());
}

#[test]
fn label_env_and_title_follow_groups() {
    let text = r"\section{Results}\label{sec:res}
\begin{figure}
\centering
\caption[Short]{A \textbf{nice} picture\label{fig:in}}
\label{fig:after}
\end{figure}
\begin{equation}\label{eq:after-fig} x \end{equation}
\begin{theorem}[Main]
\begin{align} a \label{eq:in-thm} \end{align}
\end{theorem}
\label{after-thm}
\begin{lemma}\label{lem:untitled}\end{lemma}
";
    let idx = index_file(text);
    assert_eq!(
        idx.labels,
        vec![
            label("sec:res", 0, None, Some("Results")),
            label(
                "fig:in",
                3,
                Some("figure"),
                Some("A \\textbf{nice} picture")
            ),
            label(
                "fig:after",
                4,
                Some("figure"),
                Some("A \\textbf{nice} picture")
            ),
            label("eq:after-fig", 6, Some("equation"), Some("Results")),
            label("eq:in-thm", 8, Some("align"), Some("Main")),
            label("after-thm", 10, None, Some("Results")),
            label("lem:untitled", 11, Some("lemma"), None),
        ]
    );
}

#[test]
fn references_with_cleveref_lists_and_ranges() {
    let text = r"\cref{a,b, c} \Cref*{d}
\crefrange{e}{f} \Crefrange {g} {h}
\cpageref{i,%
  j} \labelcref{k}
\ref{x,y} \eqref{z} \autoref{w} \nameref{v} \pageref{u}
\hyperref[t]{some text} \vref{s} \cref{} \ref{ }
";
    let idx = index_file(text);
    let got: Vec<(&str, Vec<String>, u32)> = idx
        .refs
        .iter()
        .map(|r| (r.cmd.as_str(), r.names.clone(), r.line))
        .collect();
    assert_eq!(
        got,
        vec![
            ("cref", names(&["a", "b", "c"]), 0),
            ("Cref*", names(&["d"]), 0),
            ("crefrange", names(&["e", "f"]), 1),
            ("Crefrange", names(&["g", "h"]), 1),
            ("cpageref", names(&["i", "j"]), 2),
            ("labelcref", names(&["k"]), 3),
            ("ref", names(&["x,y"]), 4),
            ("eqref", names(&["z"]), 4),
            ("autoref", names(&["w"]), 4),
            ("nameref", names(&["v"]), 4),
            ("pageref", names(&["u"]), 4),
            ("hyperref", names(&["t"]), 5),
            ("vref", names(&["s"]), 5),
        ]
    );
}

#[test]
fn citations_biblatex_multicite_and_natbib() {
    let text = r"\cites(See)(and more)[p.~1]{a}[ch.~2]{b,c}{d} rest
\parencites[1]{e}
  {f}
\citep[see][p.~5]{g, h} \citet*{i} \Citeauthor{j} \citealp{k}
\textcite{l} \autocite[12]{m} \Parencite[pre][post]{n}
\nocite{*} \nocite{o,*}
\cite{p,% comment
  q} \footcite{r}
";
    let idx = index_file(text);
    let got: Vec<(&str, Vec<String>, u32)> = idx
        .cites
        .iter()
        .map(|c| (c.cmd.as_str(), c.keys.clone(), c.line))
        .collect();
    assert_eq!(
        got,
        vec![
            ("cites", names(&["a", "b", "c", "d"]), 0),
            ("parencites", names(&["e", "f"]), 1),
            ("citep", names(&["g", "h"]), 3),
            ("citet*", names(&["i"]), 3),
            ("Citeauthor", names(&["j"]), 3),
            ("citealp", names(&["k"]), 3),
            ("textcite", names(&["l"]), 4),
            ("autocite", names(&["m"]), 4),
            ("Parencite", names(&["n"]), 4),
            ("nocite", names(&["o"]), 5),
            ("cite", names(&["p", "q"]), 6),
            ("footcite", names(&["r"]), 7),
        ]
    );
}

#[test]
fn every_include_kind() {
    let text = r#"\input{chapters/a}
\input chapters/b
\include{chapters/c}
\subfile{parts/d}
\import{dir/}{e}
\subimport*{sub}{f.tex}
\includegraphics[width=\linewidth]{img/g.pdf}
\includegraphics*[1][2]{img/h}
\bibliography{refs, more}
\addbibresource[label=x]{bib/i.bib}
\usepackage[utf8]{inputenc}
\usepackage{amsmath,
  amssymb}[2020/01/01]
\RequirePackage{xcolor}
\usepackage{amsmath} % again
\input{"with space"}
\InputIfFileExists{opt}{}{}
"#;
    let idx = index_file(text);
    let got: Vec<(IncludeKind, &str, u32)> = idx
        .includes
        .iter()
        .map(|i| (i.kind, i.target.as_str(), i.line))
        .collect();
    use IncludeKind::*;
    assert_eq!(
        got,
        vec![
            (Input, "chapters/a", 0),
            (Input, "chapters/b", 1),
            (Include, "chapters/c", 2),
            (Subfile, "parts/d", 3),
            (Import, "dir/e", 4),
            (Import, "sub/f.tex", 5),
            (Graphics, "img/g.pdf", 6),
            (Graphics, "img/h", 7),
            (Bibliography, "refs", 8),
            (Bibliography, "more", 8),
            (Bibliography, "bib/i.bib", 9),
            (Package, "inputenc", 10),
            (Package, "amsmath", 11),
            (Package, "amssymb", 11),
            (Package, "xcolor", 13),
            (Package, "amsmath", 14),
            (Input, "with space", 15),
            (Input, "opt", 16),
        ]
    );
    assert_eq!(
        idx.packages,
        names(&["inputenc", "amsmath", "amssymb", "xcolor"])
    );
}

#[test]
fn macro_definition_forms() {
    let text = r"\newcommand{\cO}{\mathcal{O}}
\newcommand*\abs[1]{\lvert #1\rvert}
\renewcommand{\vec}[2][x]{\mathbf{#2}_{#1}}
\providecommand{\N}{\mathbb{N}}
\DeclareRobustCommand{\R}{%
  \mathbb{R}}
\DeclareMathOperator{\Hom}{Hom}
\DeclareMathOperator*{\colim}{colim}
\DeclarePairedDelimiter{\norm}{\lVert}{\rVert}
\def\Z{\mathbb{Z}}
\def\pair#1#2{(#1,#2)}
\let\eps\varepsilon
\let\phi = \varphi
\NewDocumentCommand{\set}{s o m}{\{#3\}}
\newenvironment{myenv}[1]{\begin{center}#1}{\end{center}}
\NewDocumentEnvironment{tbox}{O{red} m}{begin}{end}
\newtheorem{theorem}{Theorem}[section]
\newtheorem{lemma}[theorem]{Lemma}
\newtheorem*{remark}{Remark}
\declaretheorem[name=Conjecture]{conj}
\newcommand{\thmref}[1]{Theorem~\ref{#1}\label{x}\section{y}}
\expandafter\def\csname foo\endcsname{bar}
\newcommand{\incomplete}
";
    let idx = index_file(text);
    let m = |name: &str, nargs: u8, body: Option<&str>, line: u32| MacroDef {
        name: name.into(),
        nargs,
        body: body.map(str::to_owned),
        line,
        is_env: false,
        is_theorem: false,
        is_math_operator: false,
        optional_args: 0,
        optional_default: None,
    };
    // `optional` optional arguments, the first one's default `default`.
    let opt = |def: MacroDef, optional: u8, default: Option<&str>| MacroDef {
        optional_args: optional,
        optional_default: default.map(str::to_owned),
        ..def
    };
    let env = |def: MacroDef| MacroDef {
        is_env: true,
        ..def
    };
    let thm = |def: MacroDef| MacroDef {
        is_env: true,
        is_theorem: true,
        ..def
    };
    let op = |def: MacroDef| MacroDef {
        is_math_operator: true,
        ..def
    };
    assert_eq!(
        idx.macros,
        vec![
            m("cO", 0, Some("\\mathcal{O}"), 0),
            m("abs", 1, Some("\\lvert #1\\rvert"), 1),
            opt(m("vec", 2, Some("\\mathbf{#2}_{#1}"), 2), 1, Some("x")),
            m("N", 0, Some("\\mathbb{N}"), 3),
            m("R", 0, Some("\\mathbb{R}"), 4),
            op(m("Hom", 0, Some("Hom"), 6)),
            op(m("colim", 0, Some("colim"), 7)),
            m("norm", 1, Some("\\lVert#1\\rVert"), 8),
            m("Z", 0, Some("\\mathbb{Z}"), 9),
            m("pair", 2, Some("(#1,#2)"), 10),
            m("eps", 0, Some("\\varepsilon"), 11),
            m("phi", 0, Some("\\varphi"), 12),
            opt(m("set", 3, Some("\\{#3\\}"), 13), 2, None),
            env(m("myenv", 1, Some("\\begin{center}#1"), 14)),
            opt(env(m("tbox", 2, Some("begin"), 15)), 1, Some("red")),
            thm(m("theorem", 0, Some("Theorem"), 16)),
            thm(m("lemma", 0, Some("Lemma"), 17)),
            thm(m("remark", 0, Some("Remark"), 18)),
            thm(m("conj", 0, Some("Conjecture"), 19)),
            m(
                "thmref",
                1,
                Some("Theorem~\\ref{#1}\\label{x}\\section{y}"),
                20
            ),
            m("incomplete", 0, None, 22),
        ]
    );
    // Nothing inside a definition body is indexed.
    assert!(idx.refs.is_empty());
    assert!(idx.labels.is_empty());
    assert!(idx.outline.is_empty());
    assert!(idx.envs.is_empty());
}

#[test]
fn environment_pair_recovery() {
    let text = r"\begin{document}
\begin{itemize}
\item a
\begin{enumerate}
\item b
\end{itemize}
\end{foo}
\begin{proof}
\begin{equation}
\end{equation}
\end{document}
\begin{after}
";
    let idx = index_file(text);
    let pair = |name: &str, begin_line, end_line| EnvPair {
        name: name.into(),
        begin_line,
        end_line,
    };
    assert_eq!(
        idx.envs,
        vec![
            pair("document", 0, Some(10)),
            pair("itemize", 1, Some(5)),
            pair("enumerate", 3, None),
            pair("proof", 7, None),
            pair("equation", 8, Some(9)),
        ]
    );

    let idx = index_file("\\begin{a}\n\\begin{b}\n\\end{b}\n\\begin{c}");
    assert_eq!(
        idx.envs,
        vec![
            pair("a", 0, None),
            pair("b", 1, Some(2)),
            pair("c", 3, None)
        ]
    );
}

#[test]
fn comments_verbatim_and_skipped_blocks() {
    let text = r"% \section{Commented}
\section{Real} % \label{commented}
\% \label{escaped-percent}
\verb|\section{Verb}| \verb*+\label{v}+
\lstinline[language=TeX]{\section{L}} \mintinline{latex}|\section{M}|
\begin{verbatim}
\section{In verbatim}
\end{verbatim}
\iffalse
\section{Hidden}
\fi
\iffalse \section{Hidden2} \else \section{Shown} \fi
\url{http://x.org/%20\section{U}} \href{http://y.org/%41}{\label{in-href-text}}
\begin{comment}
\label{in-comment-env}
\end{comment}
\newif\ifdraft
\iffalse \ifdraft \section{X} \fi \section{Y} \fi
\section{Last}
\end{document}
\section{After end}
";
    let idx = index_file(text);
    assert_eq!(titles(&idx), ["Real", "Shown", "Last"]);
    let labels: Vec<_> = idx.labels.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(labels, ["escaped-percent", "in-href-text"]);
    let envs: Vec<_> = idx
        .envs
        .iter()
        .map(|e| (e.name.as_str(), e.end_line))
        .collect();
    assert_eq!(envs, [("verbatim", Some(7)), ("comment", Some(15))]);
}

#[test]
fn endinput_stops_after_its_line() {
    let idx = index_file("\\section{A}\\endinput \\section{B}\n\\section{C}\n");
    assert_eq!(titles(&idx), ["A", "B"]);
}

#[test]
fn unterminated_constructs_do_not_hide_the_rest() {
    let text =
        "\\iffalse\n\\section{A}\n\\begin{verbatim}\n\\section{B}\n\\label{l\n\n\\section{C}\n";
    let idx = index_file(text);
    assert_eq!(titles(&idx), ["A", "B", "C"]);
    assert!(idx.labels.is_empty());
}

#[test]
fn magic_comes_from_parse_magic() {
    let text = "% !TEX root = ../main.tex\n\\documentclass[../main]{subfiles}\n\\begin{document}\n";
    let idx = index_file(text);
    assert_eq!(idx.magic, lemma_syntax::parse_magic(text));
    assert_eq!(idx.magic.root, Some(PathBuf::from("../main.tex")));
    assert_eq!(idx.magic.subfiles_parent, Some(PathBuf::from("../main")));
    assert!(idx.has_documentclass && idx.has_begin_document);
}

#[test]
fn empty_and_degenerate_inputs() {
    assert_eq!(index_file(""), FileIndex::default());
    let idx = index_file("\\");
    assert!(idx.outline.is_empty());
    let idx = index_file("\\section");
    assert!(idx.outline.is_empty());
    let idx = index_file("\\begin{");
    assert!(idx.envs.is_empty());
    let idx = index_file("é\\section{Ünïcode — title}\\label{ü}");
    assert_eq!(titles(&idx), ["Ünïcode — title"]);
    assert_eq!(idx.labels[0].name, "ü");
}

/// Thousands of unclosed arguments, unterminated `\iffalse`/verbatim blocks or `\end`s of
/// environments that are not open used to make every later command scan to the end of the text
/// again (seconds for 100 KB). Generous bounds, for debug builds; `structure_perf` has the real
/// budget.
#[test]
fn pathological_inputs_are_indexed_in_linear_time() {
    for unit in [
        "\\section{",
        "\\iffalse\n",
        "\\begin{verbatim}\n",
        "\\newcommand{\\a}{",
        "\\url{",
        "\\caption[",
        "\\begin{theorem}[",
    ] {
        let text = unit.repeat(120_000 / unit.len());
        let started = std::time::Instant::now();
        let idx = index_file(&text);
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "{unit:?} × {}: {took:?}",
            120_000 / unit.len()
        );
        assert!(idx.labels.is_empty());
    }
    let text = format!(
        "{}{}",
        "\\begin{foo}\n".repeat(10_000),
        "\\end{bar}\n".repeat(10_000)
    );
    let started = std::time::Instant::now();
    let idx = index_file(&text);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(idx.envs.len(), 10_000);
    assert!(idx.envs.iter().all(|e| e.end_line.is_none()));
}
