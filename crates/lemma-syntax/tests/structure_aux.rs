//! WP1.9 acceptance: `.aux` label numbers (plain, hyperref and cleveref forms).

use lemma_syntax::{aux_inputs, parse_aux};
use std::path::PathBuf;

fn pair(number: &str, page: &str) -> (String, String) {
    (number.to_owned(), page.to_owned())
}

#[test]
fn fixture_main_aux() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proj/main.aux");
    let aux = parse_aux(&std::fs::read_to_string(path).unwrap());
    // 14 labels; amsart's `\newlabel{tocindent0}{13.99pt}` lines are not labels.
    assert_eq!(aux.labels.len(), 14);
    assert!(!aux.labels.keys().any(|k| k.starts_with("tocindent")));
    assert_eq!(aux.labels["sec:intro"], pair("1", "1"));
    assert_eq!(aux.labels["lem:yoneda"], pair("2.1", "1"));
    assert_eq!(aux.labels["thm:fpqc"], pair("3.2", "2"));
    assert_eq!(aux.labels["eq:cocycle"], pair("1", "2"));
    assert_eq!(aux.labels["eq:adj3"], pair("5", "3"));
    assert_eq!(aux.labels["prop:sheafification"], pair("4.1", "3"));
}

/// Written by pdflatex with hyperref + cleveref and an `\include`d chapter.
const CLEVEREF_AUX: &str = r"\relax
\providecommand\hyper@newdestlabel[2]{}
\@writefile{toc}{\contentsline {section}{\numberline {1}Intro}{1}{section.1}\protected@file@percent }
\newlabel{sec:intro}{{1}{1}{Intro}{section.1}{}}
\newlabel{sec:intro@cref}{{[section][1][]1}{[1][1][]1}{}{}{}}
\newlabel{thm:main}{{1.1}{1}{Main}{theorem.1.1}{}}
\newlabel{thm:main@cref}{{[theorem][1][1]1.1}{[1][1][]1}{}{}{}}
\newlabel{eq:a}{{{$\ast $}}{1}{Intro}{AMS.1}{}}
\newlabel{eq:a@cref}{{[equation][2147483647][]{$\ast $}}{[1][1][]1}{}{}{}}
\newlabel{it:one}{{1}{1}{Intro}{Item.1}{}}
\@input{chap.aux}
\@input{parts/two.aux}
\gdef \@abspage@last{3}
";

#[test]
fn hyperref_and_cleveref_forms() {
    let aux = parse_aux(CLEVEREF_AUX);
    let names: Vec<_> = aux.labels.keys().map(String::as_str).collect();
    assert_eq!(names, ["eq:a", "it:one", "sec:intro", "thm:main"]);
    assert_eq!(aux.labels["thm:main"], pair("1.1", "1"));
    assert_eq!(aux.labels["eq:a"], pair("$\\ast $", "1"));
    assert_eq!(aux_inputs(CLEVEREF_AUX), ["chap.aux", "parts/two.aux"]);
}

#[test]
fn plain_form_and_redefinition() {
    let aux = parse_aux(
        "\\newlabel{a}{{1}{2}}\n\\newlabel{b}{{3}}\n\\newlabel{a}{{4}{5}}\n\\newlabel{c}\n",
    );
    assert_eq!(aux.labels.len(), 2);
    assert_eq!(aux.labels["a"], pair("4", "5"), "the last definition wins");
    assert_eq!(aux.labels["b"], pair("3", ""));
    assert!(parse_aux("").labels.is_empty());
    assert!(aux_inputs("").is_empty());
}
