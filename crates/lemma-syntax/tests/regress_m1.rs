//! Regression tests for verified M1 review findings in the language crates (area E).
//! Each test reproduces one confirmed defect; a control assertion first checks that the same
//! body is indexed correctly without the construct that triggers the bug.

use lemma_syntax::{FileIndex, IncludeKind, index_file, scan_bib};

fn macro_names(idx: &FileIndex) -> Vec<&str> {
    idx.macros.iter().map(|m| m.name.as_str()).collect()
}

fn has_input(idx: &FileIndex, target: &str) -> bool {
    idx.includes
        .iter()
        .any(|i| i.kind == IncludeKind::Input && i.target == target)
}

const GUARDED_BODY: &str = "\\newcommand{\\R}{\\mathbb{R}}\n\
                            \\DeclareMathOperator{\\Hom}{Hom}\n\
                            \\newtheorem{thm}{Theorem}\n\
                            \\input{more-macros}\n";

/// E1: an include guard puts `\endinput` in a conditional branch that TeX skips on the first
/// load, so TeX reads (and defines) everything after it. The structure pass must not stop there.
#[test]
fn regress_endinput_in_else_branch_of_include_guard() {
    // Control: without the guard the body is indexed.
    let plain = index_file(GUARDED_BODY);
    assert_eq!(macro_names(&plain), ["R", "Hom", "thm"]);
    assert!(has_input(&plain, "more-macros"));

    let text = format!(
        "\\ifx\\macrosloaded\\undefined\n\
         \\def\\macrosloaded{{}}\n\
         \\else\n\
         \\expandafter\\endinput\n\
         \\fi\n\
         {GUARDED_BODY}"
    );
    let idx = index_file(&text);
    assert_eq!(
        macro_names(&idx),
        ["macrosloaded", "R", "Hom", "thm"],
        "definitions after a guarded \\endinput (never executed on the first load) are missing"
    );
    assert!(
        has_input(&idx, "more-macros"),
        "\\input after a guarded \\endinput is missing; got {:?}",
        idx.includes
    );
}

/// E1: the one-line guard `\ifdefined\x\endinput\fi` (true branch, skipped on the first load).
#[test]
fn regress_endinput_in_one_line_ifdefined_guard() {
    let text = format!("\\ifdefined\\mymacros\\endinput\\fi\n\\def\\mymacros{{}}\n{GUARDED_BODY}");
    let idx = index_file(&text);
    assert_eq!(
        macro_names(&idx),
        ["mymacros", "R", "Hom", "thm"],
        "definitions after a one-line `\\ifdefined…\\endinput\\fi` guard are missing"
    );
    assert!(has_input(&idx, "more-macros"));
}

/// E1: conditional depth is tracked through `\fi`, the `\else` of a skipped `\iffalse` and
/// `\newif` conditionals; an `\endinput` outside every conditional still stops the pass.
#[test]
fn regress_endinput_depth_tracking() {
    // Closed guard, then an unconditional `\endinput`: the pass stops after its line.
    let idx = index_file(
        "\\ifdefined\\x\\endinput\\fi\n\\def\\x{}\n\\endinput\n\\newcommand{\\after}{}\n",
    );
    assert_eq!(macro_names(&idx), ["x"]);

    // `\iffalse … \else` leaves the conditional open until its `\fi`.
    let idx = index_file(
        "\\iffalse\\def\\no{}\\else\\endinput\\fi\n\\def\\a{}\n\\endinput\n\\def\\b{}\n",
    );
    assert_eq!(macro_names(&idx), ["a"]);

    // A `\newif` conditional counts like a primitive one.
    let idx = index_file(
        "\\newif\\ifloaded\n\\ifloaded\\endinput\\fi\n\\loadedtrue\n\\def\\c{}\n\\endinput\n\\def\\d{}\n",
    );
    assert_eq!(macro_names(&idx), ["c"]);

    // A stray `\fi` does not make the depth negative.
    let idx = index_file("\\fi\n\\def\\e{}\n\\endinput\n\\def\\f{}\n");
    assert_eq!(macro_names(&idx), ["e"]);
}

/// E2: the parenthesised `@comment(` without a closing `)` must not swallow later entries either.
#[test]
fn regress_unclosed_paren_comment_keeps_later_entries() {
    let text = "@comment( TODO\n@article{a1, title = {First}}\n@book{b2, title = {T}}\n";
    let keys: Vec<String> = scan_bib(text).into_iter().map(|e| e.key).collect();
    assert_eq!(keys, ["a1", "b2"]);
}

/// E2: BibTeX treats `@comment` as junk up to the next `@`, and the contract (§10.3) says
/// `scan_bib` resumes at the next '@'. An unbalanced brace in an `@comment` must not swallow
/// every later entry.
#[test]
fn regress_unbalanced_comment_keeps_later_entries() {
    let entries = "@article{a1, title = {First}, year = 2001}\n\
                   @book{b2, author = \"Doe\", title = {T}}\n";
    let keys = |text: &str| -> Vec<String> { scan_bib(text).into_iter().map(|e| e.key).collect() };
    // Control: a balanced comment is skipped and the entries are kept.
    assert_eq!(
        keys(&format!(
            "@comment{{ jabref-meta: x {{balanced}} }}\n{entries}"
        )),
        ["a1", "b2"]
    );

    let text = format!("@comment{{ jabref-meta: x {{unbalanced }}\n{entries}");
    assert_eq!(
        keys(&text),
        ["a1", "b2"],
        "entries after an unbalanced @comment were dropped"
    );
}
