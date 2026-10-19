//! Token kinds per line: highlighting classes and the `no-spell-check` split.

use lemma_core::LineIndex;
use lemma_syntax::{LineStates, ScanConfig, StrLines, Token, TokenKind};
use std::sync::Arc;

use TokenKind::*;

fn states(text: &str) -> (LineStates, LineIndex) {
    let index = LineIndex::new(text);
    let mut states = LineStates::new(Arc::new(ScanConfig::default()));
    states.rebuild(&StrLines {
        text,
        index: &index,
    });
    (states, index)
}

/// The tokens of every line of `text`, as (span text, kind).
fn spans(text: &str) -> Vec<Vec<(String, TokenKind)>> {
    let (states, index) = states(text);
    (0..index.line_count())
        .map(|l| {
            let line = &text[index.line_range(l)];
            let mut out = Vec::new();
            states.tokens(l, line, &mut out);
            out.iter()
                .map(|t| (line[t.start as usize..t.end as usize].to_owned(), t.kind))
                .collect()
        })
        .collect()
}

fn line_spans(line: &str) -> Vec<(String, TokenKind)> {
    spans(line).remove(0)
}

fn expect(line: &str, expected: &[(&str, TokenKind)]) {
    let got = line_spans(line);
    let expected: Vec<(String, TokenKind)> = expected
        .iter()
        .map(|(s, k)| ((*s).to_owned(), *k))
        .collect();
    assert_eq!(got, expected, "tokens of {line:?}");
}

/// Checks that tokens are sorted, adjacent, cover the line, and never repeat a kind.
fn assert_tiles(line: &str, tokens: &[Token]) {
    let mut at = 0;
    let mut prev = None;
    for t in tokens {
        assert_eq!(t.start, at, "gap or overlap in {line:?}: {tokens:?}");
        assert!(t.end > t.start, "empty token in {line:?}: {tokens:?}");
        assert_ne!(
            Some(t.kind),
            prev,
            "unmerged tokens in {line:?}: {tokens:?}"
        );
        assert!(
            line.is_char_boundary(t.end as usize),
            "{line:?}: {tokens:?}"
        );
        prev = Some(t.kind);
        at = t.end;
    }
    assert_eq!(
        at as usize,
        line.len(),
        "tokens do not cover {line:?}: {tokens:?}"
    );
}

#[test]
fn sectioning_and_labels() {
    expect(
        "\\section{Introduction}\\label{sec:intro}",
        &[
            ("\\section", Sectioning),
            ("{", Brace),
            ("Introduction", Text),
            ("}", Brace),
            ("\\label", Command),
            ("{", Brace),
            ("sec:intro", LabelArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\subsection*{Notes}",
        &[
            ("\\subsection*", Sectioning),
            ("{", Brace),
            ("Notes", Text),
            ("}", Brace),
        ],
    );
}

#[test]
fn inline_math() {
    expect(
        "Let $X$ be a scheme.",
        &[
            ("Let ", Text),
            ("$", MathDelim),
            ("X", Math),
            ("$", MathDelim),
            (" be a scheme.", Text),
        ],
    );
    expect(
        "$\\mathcal{F}_{n}$",
        &[
            ("$", MathDelim),
            ("\\mathcal", MathCommand),
            ("{F}_{n}", Math),
            ("$", MathDelim),
        ],
    );
    expect(
        "$a \\text{if $b$} c$",
        &[
            ("$", MathDelim),
            ("a ", Math),
            ("\\text", MathCommand),
            ("{", Brace),
            ("if ", Text),
            ("$", MathDelim),
            ("b", Math),
            ("$", MathDelim),
            ("}", Brace),
            (" c", Math),
            ("$", MathDelim),
        ],
    );
    expect(
        "\\(x\\) and \\[y\\]",
        &[
            ("\\(", MathDelim),
            ("x", Math),
            ("\\)", MathDelim),
            (" and ", Text),
            ("\\[", MathDelim),
            ("y", Math),
            ("\\]", MathDelim),
        ],
    );
    expect(
        "$a & b$",
        &[
            ("$", MathDelim),
            ("a ", Math),
            ("&", Special),
            (" b", Math),
            ("$", MathDelim),
        ],
    );
}

#[test]
fn environments() {
    expect(
        "\\begin{equation}\\label{eq:a} x = 1 \\end{equation}",
        &[
            ("\\begin", Begin),
            ("{", Brace),
            ("equation", EnvName),
            ("}", Brace),
            ("\\label", MathCommand),
            ("{", Brace),
            ("eq:a", LabelArg),
            ("}", Brace),
            (" x = 1 ", Math),
            ("\\end", End),
            ("{", Brace),
            ("equation", EnvName),
            ("}", Brace),
        ],
    );
    expect(
        "\\begin{theorem}[fpqc descent]",
        &[
            ("\\begin", Begin),
            ("{", Brace),
            ("theorem", EnvName),
            ("}", Brace),
            ("[", Bracket),
            ("fpqc descent", Text),
            ("]", Bracket),
        ],
    );
    expect(
        "\\begin{tabular}{lcr}",
        &[
            ("\\begin", Begin),
            ("{", Brace),
            ("tabular", EnvName),
            // Adjacent tokens of one kind are merged.
            ("}{", Brace),
            ("lcr", KeyArg),
            ("}", Brace),
        ],
    );
}

#[test]
fn arguments() {
    expect(
        "\\cite[Tag 023N]{Stacks}",
        &[
            ("\\cite", Command),
            ("[", Bracket),
            ("Tag 023N", Text),
            ("]", Bracket),
            ("{", Brace),
            ("Stacks", CiteArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\usepackage[backend=biber]{biblatex}",
        &[
            ("\\usepackage", Command),
            ("[", Bracket),
            ("backend=biber", KeyArg),
            ("]", Bracket),
            ("{", Brace),
            ("biblatex", PackageArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\input{chapters/yoneda}",
        &[
            ("\\input", Command),
            ("{", Brace),
            ("chapters/yoneda", FileArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\cref{a,b}",
        &[
            ("\\cref", Command),
            ("{", Brace),
            ("a,b", RefArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\hypersetup{pdftitle={A \\& B}}",
        &[
            ("\\hypersetup", Command),
            ("{", Brace),
            ("pdftitle={A \\& B}", KeyArg),
            ("}", Brace),
        ],
    );
    expect(
        "\\emph{word}",
        &[
            ("\\emph", Command),
            ("{", Brace),
            ("word", Text),
            ("}", Brace),
        ],
    );
}

#[test]
fn item_labels_have_no_error() {
    expect(
        "\\item[(i)] foo",
        &[("\\item", Command), ("[(i)] foo", Text)],
    );
}

#[test]
fn comments_verbatim_and_escapes() {
    expect("text % $x$", &[("text ", Text), ("% $x$", Comment)]);
    expect(
        "a \\% b \\$ c",
        &[
            ("a ", Text),
            ("\\%", Special),
            (" b ", Text),
            ("\\$", Special),
            (" c", Text),
        ],
    );
    expect(
        "\\verb|$x$| y",
        &[("\\verb", Command), ("|$x$|", Verbatim), (" y", Text)],
    );
    expect(
        "\\lstinline[language=C]{x%y} z",
        &[
            ("\\lstinline", Command),
            ("[", Bracket),
            ("language=C", KeyArg),
            ("]", Bracket),
            ("{", Brace),
            ("x%y", Verbatim),
            ("}", Brace),
            (" z", Text),
        ],
    );
    expect(
        "\\url{http://a.org/%7E}",
        &[
            ("\\url", Command),
            ("{", Brace),
            ("http://a.org/%7E", Verbatim),
            ("}", Brace),
        ],
    );
    expect(
        "Section~3 \\& more",
        &[
            ("Section", Text),
            ("~", Special),
            ("3 ", Text),
            ("\\&", Special),
            (" more", Text),
        ],
    );
    let doc = spans(
        "\\begin{verbatim}\n$x$ % y\n\\end{verbatim} z\n\\begin{comment}\nnote\n\\end{comment}",
    );
    assert_eq!(doc[1], [("$x$ % y".to_owned(), Verbatim)]);
    assert_eq!(
        doc[2],
        [
            ("\\end".to_owned(), End),
            ("{".to_owned(), Brace),
            ("verbatim".to_owned(), EnvName),
            ("}".to_owned(), Brace),
            (" z".to_owned(), Text),
        ]
    );
    assert_eq!(doc[4], [("note".to_owned(), Comment)]);
}

#[test]
fn multi_line_constructs_keep_their_kind() {
    let doc = spans("\\cite{a,\n  b}\n\\[\n x \\text{if\n y} \\]\n");
    assert_eq!(
        doc[1],
        [("  b".to_owned(), CiteArg), ("}".to_owned(), Brace),]
    );
    assert_eq!(doc[3][0], (" x ".to_owned(), Math));
    assert_eq!(doc[4][0], (" y".to_owned(), Text));
    assert_eq!(doc[4][1], ("}".to_owned(), Brace));
    assert_eq!(doc[4][2], (" ".to_owned(), Math));
}

#[test]
fn no_spell_is_everything_but_text() {
    let all = [
        Text,
        Command,
        Sectioning,
        Begin,
        End,
        EnvName,
        Brace,
        Bracket,
        Special,
        MathDelim,
        Math,
        MathCommand,
        Comment,
        Verbatim,
        LabelArg,
        RefArg,
        CiteArg,
        FileArg,
        PackageArg,
        KeyArg,
    ];
    for kind in all {
        assert_eq!(kind.no_spell(), kind != Text, "{kind:?}");
    }
}

#[test]
fn tokens_tile_every_line_of_the_fixtures() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures");
    for rel in ["proj/main.tex", "proj/chapters/yoneda.tex", "pdf/test.tex"] {
        let text = std::fs::read_to_string(format!("{root}/{rel}")).expect("fixture exists");
        let (states, index) = states(&text);
        for l in 0..index.line_count() {
            let line = &text[index.line_range(l)];
            let mut out = Vec::new();
            states.tokens(l, line, &mut out);
            assert_tiles(line, &out);
        }
    }
    for line in [
        "",
        "   ",
        "\\",
        "a\\",
        "$",
        "\\verb",
        "\\verb|",
        "\\begin{",
        "\\begin{x",
        "é \\é $ü$ \\verb€x€ y",
        "\\lstinline[",
        "\\mintinline{",
        "\\url{a{b}",
        "}}}]]))",
    ] {
        let (states, _) = states(line);
        let mut out = Vec::new();
        states.tokens(0, line, &mut out);
        assert_tiles(line, &out);
    }
}

#[test]
fn tokens_append_without_merging_into_earlier_lines() {
    let (states, _) = states("a\nb");
    let mut out = Vec::new();
    states.tokens(0, "a", &mut out);
    states.tokens(1, "b", &mut out);
    assert_eq!(out.len(), 2);
}
