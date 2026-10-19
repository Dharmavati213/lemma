//! Table tests for the scanner's context rules (`docs/ARCHITECTURE.md` §5, VimTeX's math rules).
//!
//! Each case is a document with a `‸` marking the cursor; the expected value is the context at
//! the cursor.

use lemma_core::LineIndex;
use lemma_syntax::{ArgKind, Context, LineStates, ScanConfig, StrLines};
use std::sync::Arc;

const CURSOR: char = '‸';

fn context_with(cfg: ScanConfig, doc: &str) -> Context {
    let at = doc.find(CURSOR).expect("the case has a cursor");
    let text = doc.replacen(CURSOR, "", 1);
    let index = LineIndex::new(&text);
    let mut states = LineStates::new(Arc::new(cfg));
    states.rebuild(&StrLines {
        text: &text,
        index: &index,
    });
    let (line, col) = index.position(at);
    states.context_at(line, &text[index.line_range(line)], col)
}

fn context(doc: &str) -> Context {
    context_with(ScanConfig::default(), doc)
}

fn check(cases: &[(&str, Context)]) {
    let mut failures = Vec::new();
    for (doc, expected) in cases {
        let got = context(doc);
        if &got != expected {
            failures.push(format!("{doc:?}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn inline() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

fn display() -> Context {
    Context::Math {
        display: true,
        env: None,
    }
}

fn env(name: &str) -> Context {
    Context::Math {
        display: name != "math" && name != "math*",
        env: Some(name.into()),
    }
}

fn arg(cmd: &str, idx: u8, kind: ArgKind, start: u32) -> Context {
    Context::Arg {
        cmd: cmd.into(),
        idx,
        kind,
        start,
    }
}

#[test]
fn dollar_math() {
    check(&[
        ("plain t‸ext", Context::Text),
        ("‸$x$", Context::Text),
        ("$‸", inline()),
        ("a $x‸ + y$ b", inline()),
        ("a $x$‸ b", Context::Text),
        ("a $x$ b‸", Context::Text),
        ("$x$$y‸$", inline()),
        ("$$x‸$$", display()),
        ("$$x$$‸", Context::Text),
        ("$$\nx‸\n$$", display()),
        ("$$\nx\n$$\n‸", Context::Text),
        // The smart `$` pair: the cursor between the dollars is in inline math.
        ("a $‸$ b", inline()),
        ("a $$‸$$ b", display()),
        ("$a\nb‸$", inline()),
    ]);
}

#[test]
fn paren_and_bracket_math() {
    check(&[
        ("\\(x‸\\)", inline()),
        ("\\(x\\)‸", Context::Text),
        ("\\[x‸\\]", display()),
        ("\\[x\\]‸", Context::Text),
        ("\\[\n  x = y‸\n\\]", display()),
        ("\\[\n  x = y\n\\]\nafter‸", Context::Text),
        // `\\[2pt]` is a line break with an optional argument, not display math.
        ("a \\\\[2pt] b‸", Context::Text),
        // A stray closer changes nothing.
        ("text \\] more‸", Context::Text),
    ]);
}

#[test]
fn every_default_math_environment_and_its_starred_form() {
    let names = [
        "equation",
        "align",
        "alignat",
        "flalign",
        "gather",
        "multline",
        "xalignat",
        "xxalignat",
        "eqnarray",
        "math",
        "displaymath",
        "dmath",
    ];
    let mut cases = Vec::new();
    for name in names {
        for name in [name.to_owned(), format!("{name}*")] {
            cases.push((
                format!("\\begin{{{name}}}\n  x‸ = 1\n\\end{{{name}}}\n"),
                env(&name),
            ));
            cases.push((format!("\\begin{{{name}}} x‸ \\end{{{name}}}"), env(&name)));
            cases.push((
                format!("\\begin{{{name}}}\n  x = 1\n\\end{{{name}}}\nafter‸"),
                Context::Text,
            ));
            cases.push((format!("\\begin{{{name}}}‸"), env(&name)));
        }
    }
    let cases: Vec<(&str, Context)> = cases.iter().map(|(d, c)| (d.as_str(), c.clone())).collect();
    check(&cases);
}

#[test]
fn environments_that_do_not_change_the_mode() {
    check(&[
        // tikzcd is not a math environment (VimTeX)...
        (
            "\\begin{tikzcd}\n  A‸ \\arrow[r] & B\n\\end{tikzcd}",
            Context::Text,
        ),
        // ...but inside \[ \] it is math because of the delimiters.
        (
            "\\[\n\\begin{tikzcd}\n  A‸ \\arrow[r, \"f\"] & B\n\\end{tikzcd}\n\\]",
            display(),
        ),
        // Inner environments keep the mode of the enclosing math.
        (
            "\\begin{equation}\n\\begin{aligned}\n  x‸ &= 1\n\\end{aligned}\n\\end{equation}",
            env("equation"),
        ),
        ("$\\begin{cases} 1 & x‸ \\end{cases}$", inline()),
        (
            "\\begin{theorem}[Main result]\n  Let $X$ be a sch‸eme.\n\\end{theorem}",
            Context::Text,
        ),
        // \end of another math environment does not close this one.
        (
            "\\begin{align}\n\\end{equation}\n x‸\n\\end{align}",
            env("align"),
        ),
        // Unknown \end does not close anything; the matching \end does.
        (
            "\\begin{gather}\n a\n\\end{gather}\n\\end{gather}\n b‸",
            Context::Text,
        ),
    ]);
}

#[test]
fn user_math_environment() {
    let doc = "\\begin{mymath}\n  x‸\n\\end{mymath}";
    assert_eq!(context(doc), Context::Text);
    let mut cfg = ScanConfig::default();
    cfg.math_envs.push("mymath".into());
    assert_eq!(context_with(cfg.clone(), doc), env("mymath"));
    assert_eq!(
        context_with(cfg, "\\begin{mymath*}\n  x‸\n\\end{mymath*}"),
        env("mymath*")
    );
}

#[test]
fn text_inside_math() {
    check(&[
        ("$a \\text{if ‸} b$", Context::TextInMath),
        ("$a \\text{if} b‸$", inline()),
        ("$a \\text{if $x‸$ holds}$", inline()),
        ("$a \\text{if $x$ ho‸lds}$", Context::TextInMath),
        ("$a \\text{if $x$ holds} + b‸$", inline()),
        ("\\[ \\text{a {nested} gro‸up} \\]", Context::TextInMath),
        ("\\[ \\text{a {nested} group} x‸ \\]", display()),
        ("\\[ \\text {spaced‸} \\]", Context::TextInMath),
        // Multi-line text argument inside an environment.
        (
            "\\begin{align}\n  x &= y \\intertext{and then, by the\n  lemma‸,}\n  z &= w\n\\end{align}",
            Context::TextInMath,
        ),
        (
            "\\begin{align}\n  x &= y \\intertext{and then\n  by the lemma,}\n  z‸ &= w\n\\end{align}",
            env("align"),
        ),
        // \tag's argument is text (VimTeX treats it as non-math).
        (
            "\\begin{equation} x \\tag{a‸} \\end{equation}",
            Context::TextInMath,
        ),
        // Outside math, \text is just a command.
        ("\\text{in te‸xt}", Context::Text),
        // \operatorname and \mathrm stay math.
        ("$\\operatorname{Ho‸m}$", inline()),
        ("$\\mathrm{d‸}$", inline()),
    ]);
    let names = [
        "text",
        "textnormal",
        "textrm",
        "textup",
        "texttt",
        "textsf",
        "textsc",
        "textbf",
        "textit",
        "mbox",
        "fbox",
        "intertext",
        "shortintertext",
    ];
    for name in names {
        let doc = format!("$x \\{name}{{wo‸rd}} y$");
        assert_eq!(context(&doc), Context::TextInMath, "{doc}");
        let doc = format!("$x \\{name}{{a $b‸$ c}} y$");
        assert_eq!(context(&doc), inline(), "{doc}");
    }
}

#[test]
fn ensuremath_enters_math() {
    check(&[
        ("a \\ensuremath{x‸} b", inline()),
        ("a \\ensuremath{x^{2}‸} b", inline()),
        ("a \\ensuremath{x^{2}} b‸", Context::Text),
        ("a \\ensuremath{\\text{t‸}} b", Context::TextInMath),
    ]);
}

#[test]
fn verbatim_environments() {
    check(&[
        (
            "\\begin{verbatim}\n$x‸$\n\\end{verbatim}",
            Context::Verbatim,
        ),
        (
            "\\begin{verbatim}\n$x$ % y\n\\end{verbatim}\nte‸xt",
            Context::Text,
        ),
        (
            "\\begin{verbatim}\n$x\n\\end{verbatim}\nte‸xt",
            Context::Text,
        ),
        (
            "\\begin{verbatim*}\n$x‸\n\\end{verbatim*}",
            Context::Verbatim,
        ),
        // Only the exact end marker ends the body.
        (
            "\\begin{verbatim*}\n\\end{verbatim}\n$x‸\n\\end{verbatim*}",
            Context::Verbatim,
        ),
        (
            "\\begin{Verbatim}[numbers=left]\n$x‸\n\\end{Verbatim}",
            Context::Verbatim,
        ),
        (
            "\\begin{lstlisting}[language=TeX]\n\\[ x‸\n\\end{lstlisting}",
            Context::Verbatim,
        ),
        (
            "\\begin{lstlisting}[language=TeX]\n\\[ x\n\\end{lstlisting}\na‸",
            Context::Text,
        ),
        (
            "\\begin{minted}{latex}\n$x‸\n\\end{minted}",
            Context::Verbatim,
        ),
        (
            "\\begin{minted}[linenos]{latex}\n$x\n\\end{minted}\na‸",
            Context::Text,
        ),
        ("\\begin{comment}\n$x‸\n\\end{comment}", Context::Comment),
        ("\\begin{comment}\n$x\n\\end{comment}\na‸", Context::Text),
        // One-line body.
        ("\\begin{verbatim}$x\\end{verbatim} a‸", Context::Text),
        // A verbatim body is not affected by blank lines.
        (
            "\\begin{verbatim}\n\n$x‸\n\\end{verbatim}",
            Context::Verbatim,
        ),
        // Inside math, verbatim still applies.
        (
            "\\begin{verbatim}\n\\begin{equation}\n\\end{verbatim}\nx‸",
            Context::Text,
        ),
    ]);
}

#[test]
fn inline_verbatim() {
    check(&[
        ("\\verb|$| a‸", Context::Text),
        ("\\verb|$‸|", Context::Verbatim),
        ("\\verb|‸", Context::Verbatim),
        ("\\verb+a%b+ x‸", Context::Text),
        ("\\verb*|$ $| x‸", Context::Text),
        ("\\verb!{! x‸", Context::Text),
        ("$\\verb|$| x‸$", inline()),
        ("\\lstinline|$| a‸", Context::Text),
        ("\\lstinline{$} a‸", Context::Text),
        ("\\lstinline{a{$}b} a‸", Context::Text),
        ("\\lstinline[language=C]|$| a‸", Context::Text),
        ("\\lstinline[language=C]|$‸|", Context::Verbatim),
        ("\\mintinline{python}|$| a‸", Context::Text),
        ("\\mintinline{python}{$} a‸", Context::Text),
        ("\\mintinline[linenos]{python}{x = $‸}", Context::Verbatim),
        ("\\url{http://a.org/%7Ex} a‸", Context::Text),
        ("\\href{http://a.org/#x%y}{the $x‸$ link}", inline()),
        // An unclosed \verb ends at the end of the line.
        ("\\verb|$x\n a‸", Context::Text),
    ]);
}

#[test]
fn comments_and_escapes() {
    check(&[
        ("% $x‸", Context::Comment),
        ("text % $x\nmore‸", Context::Text),
        ("text ‸% comment", Context::Text),
        ("text %‸ comment", Context::Comment),
        ("a \\% b $x‸$", inline()),
        ("a \\% b‸", Context::Text),
        ("\\$ x‸", Context::Text),
        ("costs \\$5 and \\$6‸", Context::Text),
        ("$a \\$ b‸$", inline()),
        ("$a \\$ b$ c‸", Context::Text),
        ("$x % comment $\n y‸$", inline()),
        ("$x % comment‸", Context::Comment),
        ("\\\\% comment‸", Context::Comment),
    ]);
}

#[test]
fn blank_line_closes_inline_math() {
    check(&[
        ("$x\ny‸", inline()),
        ("$x\n\ny‸", Context::Text),
        ("$x\n   \ny‸", Context::Text),
        ("$x\n\n‸", Context::Text),
        ("\\(x\n\ny‸", Context::Text),
        // A blank line also ends delimited display math, as TeX's \par does...
        ("\\[x\n\ny‸", Context::Text),
        ("$$x\n\ny‸", Context::Text),
        // ...but not a math environment.
        (
            "\\begin{equation}\nx\n\ny‸\n\\end{equation}",
            env("equation"),
        ),
        // Inside an environment, the blank line ends the inline math nested in \text.
        (
            "\\begin{align}\n\\text{if $x\n\ny‸\n\\end{align}",
            env("align"),
        ),
        // A line with a comment is not blank.
        ("$x\n%\ny‸", inline()),
    ]);
}

#[test]
fn item_labels_and_tikzcd_are_plain() {
    check(&[
        ("\\item[(i)] foo $x‸$", inline()),
        ("\\item[(i)] foo‸", Context::Text),
        ("\\item[i)] foo‸", Context::Text),
        (
            "\\begin{enumerate}[label=(\\roman*)]\n\\item[(ii)] b‸",
            Context::Text,
        ),
        ("\\begin{tikzcd}[row sep=large] A‸", Context::Text),
    ]);
}

#[test]
fn argument_contexts() {
    use ArgKind::{Cite, Env, File, Key, Label, Package, Ref};
    check(&[
        ("\\ref{sec:‸}", arg("ref", 0, Ref, 5)),
        ("See~\\ref{sec:in‸tro}.", arg("ref", 0, Ref, 9)),
        ("\\eqref{eq:a‸}", arg("eqref", 0, Ref, 7)),
        ("\\cref{a,b‸}", arg("cref", 0, Ref, 8)),
        ("\\cref{a, b‸}", arg("cref", 0, Ref, 9)),
        ("\\cref*{a‸}", arg("cref", 0, Ref, 7)),
        ("\\crefrange{a}{b‸}", arg("crefrange", 1, Ref, 14)),
        ("\\cite{‸}", arg("cite", 0, Cite, 6)),
        ("\\cite[p.~5]{key‸}", arg("cite", 0, Cite, 12)),
        ("\\cite[see][p.~5]{key‸}", arg("cite", 0, Cite, 17)),
        ("\\cite[se‸e]{key}", Context::Text),
        ("\\parencite{a,b,c‸}", arg("parencite", 0, Cite, 15)),
        (
            "\\textcites(pre)(post)[a]{x}[b]{y‸}",
            arg("textcites", 1, Cite, 31),
        ),
        ("\\label{‸}", arg("label", 0, Label, 7)),
        ("\\input{chap‸}", arg("input", 0, File, 7)),
        (
            "\\includegraphics[width=\\textwidth]{fi‸}",
            arg("includegraphics", 0, File, 35),
        ),
        (
            "\\includegraphics[wid‸th=2cm]{f}",
            arg("includegraphics", 0, Key, 17),
        ),
        ("\\bibliography{a,b‸}", arg("bibliography", 0, File, 16)),
        ("\\import{dir/}{fi‸}", arg("import", 1, File, 14)),
        ("\\usepackage{ams‸}", arg("usepackage", 0, Package, 12)),
        (
            "\\usepackage[utf8]{inputenc‸}",
            arg("usepackage", 0, Package, 18),
        ),
        (
            "\\usepackage[backend=biber,sty‸]{biblatex}",
            arg("usepackage", 0, Key, 26),
        ),
        (
            "\\documentclass[11pt]{ams‸}",
            arg("documentclass", 0, Package, 21),
        ),
        ("\\begin{equ‸", arg("begin", 0, Env, 7)),
        ("\\begin{equ‸}", arg("begin", 0, Env, 7)),
        ("\\begin{‸}", arg("begin", 0, Env, 7)),
        ("\\end{al‸}", arg("end", 0, Env, 5)),
        ("$x \\end{al‸", arg("end", 0, Env, 8)),
        ("\\begin{tabular}{l‸}", arg("begin", 1, Key, 16)),
        (
            "\\begin{tabularx}{\\linewidth}{lX‸}",
            arg("begin", 2, Key, 29),
        ),
        ("\\begin{figure}[h‸t]", arg("begin", 1, Key, 15)),
        ("\\newtheorem{lem‸}", arg("newtheorem", 0, Env, 12)),
        ("\\hypersetup{colorlinks‸}", arg("hypersetup", 0, Key, 12)),
        (
            "\\hypersetup{colorlinks, urlcolor=bl‸}",
            arg("hypersetup", 0, Key, 24),
        ),
        ("\\setcounter{sec‸}{2}", arg("setcounter", 0, Key, 12)),
        ("\\setcounter{section}{2‸}", Context::Text),
        // Arguments inside math.
        (
            "\\begin{equation}\\label{eq:a‸}",
            arg("label", 0, Label, 23),
        ),
        ("$x = \\eqref{a‸}$", arg("eqref", 0, Ref, 12)),
        ("\\begin{equation}\\label{eq:a} x‸", env("equation")),
        // After the argument the mode returns.
        ("\\cite{a} and‸", Context::Text),
        ("\\label{a} $x‸$", inline()),
        // Text arguments are ordinary text.
        ("\\section{Intro‸duction}", Context::Text),
        ("\\emph{wo‸rd}", Context::Text),
        ("\\begin{theorem}[fpqc des‸cent]", Context::Text),
        ("\\begin{theorem}[$p$-ad‸ic]", Context::Text),
        ("\\begin{theorem}[$p‸$-adic]", inline()),
        (
            "\\begin{proof}[Proof of \\cref{thm‸}]",
            arg("cref", 0, Ref, 29),
        ),
    ]);
}

#[test]
fn multi_line_arguments() {
    use ArgKind::{Cite, Key, Package};
    check(&[
        ("\\cite{a,\n  b‸}", arg("cite", 0, Cite, 2)),
        ("\\cite{a,\n  b, c‸}", arg("cite", 0, Cite, 5)),
        ("\\cite{a,\n  b}\nte‸xt", Context::Text),
        ("\\cite{a, % first\n  b‸}", arg("cite", 0, Cite, 2)),
        (
            "\\hypersetup{\n  colorlinks,\n  urlcolor=blue‸\n}",
            arg("hypersetup", 0, Key, 2),
        ),
        (
            "\\hypersetup{\n  pdftitle={A, B},\n  x‸\n}",
            arg("hypersetup", 0, Key, 2),
        ),
        ("\\hypersetup{\n  colorlinks\n}\nte‸xt", Context::Text),
        (
            "\\usepackage[\n  backend=biber,\n  style=alpha‸betic\n]{biblatex}",
            arg("usepackage", 0, Key, 2),
        ),
        (
            "\\usepackage[\n  backend=biber\n]{bibla‸tex}",
            arg("usepackage", 0, Package, 2),
        ),
        // A blank line ends an unclosed argument.
        ("\\cite{a,\n\nte‸xt", Context::Text),
        // An unclosed \begin name does not carry over.
        ("\\begin{equ\nx‸", Context::Text),
    ]);
}

#[test]
fn command_names() {
    check(&[
        ("\\‸", Context::CommandName { start: 0 }),
        ("a \\‸", Context::CommandName { start: 2 }),
        ("\\alp‸", Context::CommandName { start: 0 }),
        ("\\alpha‸", Context::CommandName { start: 0 }),
        ("\\al‸pha", Context::CommandName { start: 0 }),
        ("$x + \\fr‸", Context::CommandName { start: 5 }),
        ("\\section‸{", Context::CommandName { start: 0 }),
        ("\\section*‸{", Context::Text),
        ("$\\alpha ‸$", inline()),
        ("\\usepackage{\\my‸}", Context::CommandName { start: 12 }),
        ("x \\$‸", Context::Text),
        ("x \\\\‸", Context::Text),
        ("\\cite‸", Context::CommandName { start: 0 }),
        ("% \\alp‸", Context::Comment),
        ("\\verb‸", Context::CommandName { start: 0 }),
    ]);
}

#[test]
fn cursor_past_the_end_and_unknown_lines() {
    let text = "a $x";
    let index = LineIndex::new(text);
    let mut states = LineStates::new(Arc::new(ScanConfig::default()));
    states.rebuild(&StrLines {
        text,
        index: &index,
    });
    assert_eq!(states.context_at(0, text, 99), inline());
    // A line number past the states is scanned from the text level.
    assert_eq!(states.context_at(7, "b", 1), Context::Text);
}

#[test]
fn well_formed_fixtures_return_to_text_level() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures");
    for rel in [
        "proj/main.tex",
        "proj/chapters/yoneda.tex",
        "pdf/test.tex",
        "large.tex",
    ] {
        let text = std::fs::read_to_string(format!("{root}/{rel}")).expect("fixture exists");
        let index = LineIndex::new(&text);
        let mut states = LineStates::new(Arc::new(ScanConfig::default()));
        states.rebuild(&StrLines {
            text: &text,
            index: &index,
        });
        let line_ctx = |l: u32| states.context_at(l, &text[index.line_range(l)], 0);
        let last = index.line_count() - 1;
        assert_eq!(line_ctx(last), Context::Text, "{rel}: end of document");
        for l in 0..index.line_count() {
            let line = &text[index.line_range(l)];
            if line.starts_with("\\section") || line.starts_with("\\begin{document}") {
                assert_eq!(line_ctx(l), Context::Text, "{rel}:{}", l + 1);
            }
            if line.starts_with("\\begin{tikzcd}") {
                // In these fixtures tikzcd always sits inside \[ \].
                assert_eq!(line_ctx(l), display(), "{rel}:{}", l + 1);
            }
        }
    }
}
