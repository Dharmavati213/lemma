//! Snippet expansion through real GtkSourceView chunks (WP1.8 acceptance, ARCHITECTURE §9
//! risk 9).
//!
//! Every default snippet is expanded with `editor::snippets::expand` on a 4-space-indented
//! line of a real `TexView` + `Document`, then "X" is typed at the first focus position. The
//! buffer must equal the hand-written expansion (first stop filled, mirrors updated live,
//! continuation lines indented), the cursor must sit right after the X, the snippet's chunk
//! focus positions must equal the expected list, the document's `LineIndex` must stay exact,
//! and GLib must log no warnings or criticals.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_snippets`.

mod common;

use gtk::prelude::*;
use lemma_app::document::{self, DocRef, Document};
use lemma_app::editor::TexView;
use lemma_app::editor::snippets::{self, ExpandError, build_gsv_snippet, expand, expand_chunks};
use lemma_config::EditorCfg;
use lemma_core::LineIndex;
use lemma_snippets::{Chunk, Compiled, SnippetMatch, SnippetSet};
use lemma_syntax::Context;
use sourceview5::prelude::*;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    common::run(&[
        ("warnings_are_counted", warnings_are_counted),
        ("chunks_map_onto_gsv_chunks", chunks_map_onto_gsv_chunks),
        (
            "every_default_expands_on_an_indented_line",
            every_default_expands,
        ),
        (
            "undefined_macros_expand_the_alternative_body",
            undefined_macros_expand_the_alternative,
        ),
        (
            "one_undo_restores_the_trigger",
            one_undo_restores_the_trigger,
        ),
        (
            "undo_after_typing_into_a_stop",
            undo_after_typing_into_a_stop,
        ),
        ("manual_prefix_is_replaced", manual_prefix_is_replaced),
        (
            "text_around_the_trigger_is_kept",
            text_around_the_trigger_is_kept,
        ),
        ("tab_indentation_is_copied", tab_indentation_is_copied),
        (
            "nested_expansion_inside_a_stop",
            nested_expansion_inside_a_stop,
        ),
        ("sessions_can_be_ended", sessions_can_be_ended),
        ("stale_match_changes_nothing", stale_match_changes_nothing),
        (
            "read_only_view_changes_nothing",
            read_only_view_changes_nothing,
        ),
    ])
}

// ───────────── Expected expansions ─────────────

/// One default snippet, expanded on the line `INDENT + trigger` with "X" typed at the first
/// focus position (stop 1, or `$0` when the snippet has no numbered stops).
struct Case {
    trigger: &'static str,
    /// The lines that replace `INDENT + trigger`.
    lines: &'static [&'static str],
    /// The focus position of each chunk (−1 = text or mirror).
    focus: &'static [i32],
}

const fn case(
    trigger: &'static str,
    lines: &'static [&'static str],
    focus: &'static [i32],
) -> Case {
    Case {
        trigger,
        lines,
        focus,
    }
}

/// `\begin{env}$1` / `  $0` / `\end{env}`.
const ENV_WITH_TITLE: &[i32] = &[-1, 1, -1, 0, -1];
/// `\begin{env}` / `  $0` / `\end{env}`.
const ENV_BODY: &[i32] = &[-1, 0, -1];
/// Text, `$1`, text, `$0`.
const ONE_STOP: &[i32] = &[-1, 1, -1, 0];
/// Text, `$1`, text, `$2`, text, `$0`.
const TWO_STOPS: &[i32] = &[-1, 1, -1, 2, -1, 0];
/// Text and the implicit `$0` at its end.
const PLAIN: &[i32] = &[-1, 0];

const CASES: &[Case] = &[
    // Environments: manual, line_begin.
    case(
        "thm",
        &[r"    \begin{theorem}X", "      ", r"    \end{theorem}"],
        ENV_WITH_TITLE,
    ),
    case(
        "lem",
        &[r"    \begin{lemma}X", "      ", r"    \end{lemma}"],
        ENV_WITH_TITLE,
    ),
    case(
        "prop",
        &[
            r"    \begin{proposition}X",
            "      ",
            r"    \end{proposition}",
        ],
        ENV_WITH_TITLE,
    ),
    case(
        "cor",
        &[r"    \begin{corollary}X", "      ", r"    \end{corollary}"],
        ENV_WITH_TITLE,
    ),
    case(
        "def",
        &[
            r"    \begin{definition}X",
            "      ",
            r"    \end{definition}",
        ],
        ENV_WITH_TITLE,
    ),
    case(
        "rem",
        &[r"    \begin{remark}X", "      ", r"    \end{remark}"],
        ENV_WITH_TITLE,
    ),
    case(
        "pf",
        &[r"    \begin{proof}X", "      ", r"    \end{proof}"],
        ENV_WITH_TITLE,
    ),
    case(
        "eg",
        &[r"    \begin{example}X", "      ", r"    \end{example}"],
        ENV_WITH_TITLE,
    ),
    case(
        "eq",
        &[r"    \begin{equation}", "      X", r"    \end{equation}"],
        ENV_BODY,
    ),
    case(
        "eqs",
        &[r"    \begin{equation*}", "      X", r"    \end{equation*}"],
        ENV_BODY,
    ),
    case(
        "ali",
        &[r"    \begin{align}", "      X", r"    \end{align}"],
        ENV_BODY,
    ),
    case(
        "alis",
        &[r"    \begin{align*}", "      X", r"    \end{align*}"],
        ENV_BODY,
    ),
    case(
        "gat",
        &[r"    \begin{gather}", "      X", r"    \end{gather}"],
        ENV_BODY,
    ),
    case(
        "beg",
        &[r"    \begin{X}", "      ", r"    \end{X}"],
        &[-1, 1, -1, 0, -1, -1, -1],
    ),
    case(
        "itm",
        &[
            r"    \begin{itemize}",
            r"      \item X",
            r"    \end{itemize}",
        ],
        ENV_BODY,
    ),
    case(
        "enum",
        &[
            r"    \begin{enumerate}",
            r"      \item X",
            r"    \end{enumerate}",
        ],
        ENV_BODY,
    ),
    case("ii", &[r"    \item X"], PLAIN),
    case(
        "fig",
        &[
            r"    \begin{figure}[htbp]",
            r"      \centering",
            r"      \includegraphics[width=X\textwidth]{}",
            r"      \caption{}",
            r"      \label{fig:}",
            r"    \end{figure}",
            "    ",
        ],
        &[-1, 1, -1, 2, -1, 3, -1, 4, -1, 0],
    ),
    // Sections: manual.
    case("sec", &[r"    \section{X}\label{sec:}", "    "], TWO_STOPS),
    case(
        "sub",
        &[r"    \subsection{X}\label{sub:}", "    "],
        TWO_STOPS,
    ),
    case(
        "ssub",
        &[r"    \subsubsection{X}\label{ssub:}", "    "],
        TWO_STOPS,
    ),
    // Text autosnippets.
    case("mk", &["    $X$"], ONE_STOP),
    case("dm", &[r"    \[", "      X", r"    \]", "    "], ONE_STOP),
    // Math autosnippets: structure.
    case("ff", &[r"    \frac{X}{}"], TWO_STOPS),
    case("sq", &[r"    \sqrt{X}"], ONE_STOP),
    case("td", &["    ^{X}"], ONE_STOP),
    case("__", &["    _{X}"], ONE_STOP),
    case("inv", &["    ^{-1}X"], PLAIN),
    case("sum", &[r"    \sum_{X}^{}"], TWO_STOPS),
    case("prod", &[r"    \prod_{X}^{}"], TWO_STOPS),
    case("lim", &[r"    \lim_{X}"], ONE_STOP),
    case("int", &[r"    \int_{X}^{}"], TWO_STOPS),
    case(
        "cas",
        &[r"    \begin{cases}", "      X", r"    \end{cases}"],
        ONE_STOP,
    ),
    case("set", &[r"    \{X\}"], ONE_STOP),
    // Fonts and accents.
    case("cal", &[r"    \mathcal{X}"], ONE_STOP),
    case("bb", &[r"    \mathbb{X}"], ONE_STOP),
    case("mfr", &[r"    \mathfrak{X}"], ONE_STOP),
    case("ol", &[r"    \overline{X}"], ONE_STOP),
    case("hat", &[r"    \hat{X}"], ONE_STOP),
    case("til", &[r"    \tilde{X}"], ONE_STOP),
    case("wt", &[r"    \widetilde{X}"], ONE_STOP),
    case("wh", &[r"    \widehat{X}"], ONE_STOP),
    case("tt", &[r"    \text{X}"], ONE_STOP),
    // Delimiters.
    case("lr(", &[r"    \left( X \right)"], ONE_STOP),
    case("lr[", &[r"    \left[ X \right]"], ONE_STOP),
    case("lr{", &[r"    \left\{ X \right\}"], ONE_STOP),
    case("lr|", &[r"    \left| X \right|"], ONE_STOP),
    // Arrows, relations, symbols.
    case("->", &[r"    \toX"], PLAIN),
    case("=>", &[r"    \impliesX"], PLAIN),
    case("<=", &[r"    \leqX"], PLAIN),
    case(">=", &[r"    \geqX"], PLAIN),
    case("map", &[r"    \mapstoX"], PLAIN),
    case("inj", &[r"    \hookrightarrowX"], PLAIN),
    case("surj", &[r"    \twoheadrightarrowX"], PLAIN),
    case("iff", &[r"    \iffX"], PLAIN),
    case("inn", &[r"    \inX"], PLAIN),
    case("xx", &[r"    \timesX"], PLAIN),
    case("AA", &[r"    \forallX"], PLAIN),
    case("EE", &[r"    \existsX"], PLAIN),
    // Operators and number sets, with the project's macros defined.
    case("hom", &[r"    \HomX"], PLAIN),
    case("spec", &[r"    \SpecX"], PLAIN),
    case("proj", &[r"    \ProjX"], PLAIN),
    case("ker", &[r"    \KerX"], PLAIN),
    case("coker", &[r"    \cokerX"], PLAIN),
    case("ext", &[r"    \ExtX"], PLAIN),
    case("NN", &[r"    \NX"], PLAIN),
    case("ZZ", &[r"    \ZX"], PLAIN),
    case("QQ", &[r"    \QX"], PLAIN),
    case("RR", &[r"    \RX"], PLAIN),
    case("CC", &[r"    \CX"], PLAIN),
    case("KK", &[r"    \KX"], PLAIN),
];

/// The `if_undefined` defaults: (trigger, macro name, expansion when the macro is undefined).
const ALT_CASES: &[(&str, &str, &str)] = &[
    ("hom", "Hom", r"    \operatorname{Hom}X"),
    ("spec", "Spec", r"    \operatorname{Spec}X"),
    ("proj", "Proj", r"    \operatorname{Proj}X"),
    ("ker", "Ker", r"    \operatorname{Ker}X"),
    ("coker", "coker", r"    \operatorname{coker}X"),
    ("ext", "Ext", r"    \operatorname{Ext}X"),
    ("NN", "N", r"    \mathbb{N}X"),
    ("ZZ", "Z", r"    \mathbb{Z}X"),
    ("QQ", "Q", r"    \mathbb{Q}X"),
    ("RR", "R", r"    \mathbb{R}X"),
    ("CC", "C", r"    \mathbb{C}X"),
    ("KK", "K", r"    \mathbb{K}X"),
];

const INDENT: &str = "    ";
const BEFORE: &str = "% before\n";
const AFTER: &str = "\n% after";

// ───────────── Tests ─────────────

fn chunks_map_onto_gsv_chunks() {
    let problems = common::logged_problems();
    let chunks = [
        Chunk::Text(r"\$a$".into()),
        stop(1, ""),
        stop(2, r"\alpha"),
        Chunk::Mirror(1),
        stop(0, ""),
    ];
    let snippet = build_gsv_snippet(&chunks);
    assert_eq!(snippet.n_chunks(), 5);
    let chunk = |i| snippet.nth_chunk(i);

    assert_eq!(chunk(0).text().as_str(), r"\$a$");
    assert!(chunk(0).is_text_set());
    assert_eq!(chunk(0).focus_position(), -1);
    assert!(chunk(0).spec().unwrap_or_default().is_empty());

    assert_eq!(chunk(1).focus_position(), 1);
    assert!(!chunk(1).is_text_set());

    assert_eq!(chunk(2).focus_position(), 2);
    assert_eq!(chunk(2).text().as_str(), r"\alpha");
    assert!(chunk(2).is_text_set());

    assert_eq!(chunk(3).focus_position(), -1);
    assert_eq!(chunk(3).spec().map(String::from), Some("$1".to_owned()));
    assert!(!chunk(3).is_text_set());

    assert_eq!(chunk(4).focus_position(), 0);
    assert!(!chunk(4).is_text_set());

    // Pushed into a view, the literal `\$` and `$` survive and the mirror follows stop 1.
    let fx = Fixture::new("");
    let mut start = fx.buffer().start_iter();
    fx.view.push_snippet(&snippet, Some(&mut start));
    fx.type_text("x");
    assert_eq!(fx.text(), r"\$a$x\alphax");
    fx.assert_line_index_exact();
    common::assert_no_new_problems(problems);
}

fn every_default_expands() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    assert_cases_cover(&set, CASES.iter().map(|c| c.trigger));

    let mut failures = Vec::new();
    for compiled in set.iter() {
        let trigger = compiled.def.trigger.as_str();
        let case = CASES
            .iter()
            .find(|c| c.trigger == trigger)
            .expect("checked by assert_cases_cover");
        let mut asked = Vec::new();
        let got = expand_on_indented_line(compiled, |name| {
            asked.push(name.to_owned());
            true
        });
        let expected_asks: Vec<&str> = compiled.def.required_macro().into_iter().collect();
        failures.extend(got.mismatches(trigger, &case.lines.join("\n"), case.focus));
        if asked != expected_asks {
            failures.push(format!(
                "{trigger}: asked about {asked:?}, expected {expected_asks:?}"
            ));
        }
        if got.selected != first_default(&compiled.chunks) {
            failures.push(format!(
                "{trigger}: pre-selected {:?}, expected {:?}",
                got.selected,
                first_default(&compiled.chunks)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} problems:\n{}",
        failures.len(),
        failures.join("\n")
    );
    common::assert_no_new_problems(problems);
}

fn undefined_macros_expand_the_alternative() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let with_alt: Vec<&Compiled> = set.iter().filter(|c| c.alt.is_some()).collect();
    assert_cases_cover_subset(&with_alt, ALT_CASES.iter().map(|c| c.0));

    let mut failures = Vec::new();
    for compiled in with_alt {
        let trigger = compiled.def.trigger.as_str();
        let &(_, name, expected) = ALT_CASES
            .iter()
            .find(|c| c.0 == trigger)
            .expect("checked by assert_cases_cover_subset");
        let mut asked = Vec::new();
        let got = expand_on_indented_line(compiled, |name| {
            asked.push(name.to_owned());
            false
        });
        failures.extend(got.mismatches(trigger, expected, PLAIN));
        if asked != [name] {
            failures.push(format!(
                "{trigger}: asked about {asked:?}, expected [{name:?}]"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // An alternative body with a tab stop (a user snippet).
    let user = SnippetSet::from_toml(
        r"
        [[snippet]]
        trigger = 'OO'
        body = '\O'
        if_undefined = '\mathcal{O}_{$1}'
        auto = true
        context = 'math'
        ",
    )
    .expect("a valid snippet file");
    let compiled = user.iter().next().expect("one snippet");
    let got = expand_on_indented_line(compiled, |_| false);
    let mismatches = got.mismatches("OO", r"    \mathcal{O}_{X}", ONE_STOP);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    let got = expand_on_indented_line(compiled, |_| true);
    let mismatches = got.mismatches("OO", r"    \OX", PLAIN);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    common::assert_no_new_problems(problems);
}

fn one_undo_restores_the_trigger() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    // (trigger, expansion, context of an autosnippet or `None` for a manual one)
    let cases = [
        ("ff", r"    \frac{}{}", Some(math())),
        (
            "thm",
            "    \\begin{theorem}\n      \n    \\end{theorem}",
            None,
        ),
        ("mk", "    $$", Some(Context::Text)),
    ];
    for (trigger, expanded, auto) in cases {
        let fx = Fixture::new(&format!("{BEFORE}{INDENT}"));
        fx.cursor_to_line_end(1);
        for c in trigger.chars() {
            fx.type_text(&c.to_string());
        }
        let line = format!("{INDENT}{trigger}");
        let m = match &auto {
            Some(ctx) => set
                .auto_match(&line, ctx)
                .unwrap_or_else(|| panic!("{trigger} fires after {line:?}")),
            None => manual_match(&set, trigger, trigger),
        };
        expand(&fx.view, &m, |_| true).expect("expands");
        assert_eq!(fx.text(), format!("{BEFORE}{expanded}"), "{trigger}");

        let buffer = fx.buffer();
        assert!(buffer.can_undo());
        buffer.undo();
        assert_eq!(fx.text(), format!("{BEFORE}{line}"), "{trigger}: one undo");
        assert_eq!(
            fx.cursor(),
            char_len(&format!("{BEFORE}{line}")),
            "{trigger}: the cursor is back after the trigger"
        );
        fx.assert_line_index_exact();

        assert!(buffer.can_redo());
        buffer.redo();
        assert_eq!(fx.text(), format!("{BEFORE}{expanded}"), "{trigger}: redo");
        fx.assert_line_index_exact();
    }
    common::assert_no_new_problems(problems);
}

fn undo_after_typing_into_a_stop() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let env = |name: &str| format!("{BEFORE}    \\begin{{{name}}}\n      \n    \\end{{{name}}}");
    let trigger_line = format!("{BEFORE}{INDENT}beg");

    // A mirrored stop: GtkSourceView rewrites the mirror while the session is active, so the
    // session must end before undo replays the history (see `snippets::end_snippet_sessions`).
    let fx = Fixture::new(&trigger_line);
    fx.cursor_to_line_end(1);
    expand(&fx.view, &manual_match(&set, "beg", "beg"), |_| true).expect("expands");
    for c in "pf".chars() {
        fx.type_text(&c.to_string());
    }
    assert_eq!(fx.text(), env("pf"));
    let buffer = fx.buffer();
    let mut seen = vec![fx.text()];
    while fx.text() != trigger_line {
        assert!(buffer.can_undo(), "stuck at {:?}", fx.text());
        assert!(seen.len() < 10, "too many undo steps: {seen:#?}");
        buffer.undo();
        let text = fx.text();
        assert!(
            [env(""), env("p"), trigger_line.clone()].contains(&text),
            "undo step {} gave {text:?}",
            seen.len()
        );
        fx.assert_line_index_exact();
        seen.push(text);
    }
    // Each keystroke is its own undo step (its mirror update keeps GTK from merging them).
    assert_eq!(seen, [env("pf"), env("p"), env(""), trigger_line.clone()]);
    while buffer.can_redo() {
        buffer.redo();
        fx.assert_line_index_exact();
    }
    assert_eq!(
        fx.text(),
        env("pf"),
        "redo replays the expansion and the typing"
    );

    // Without mirrors: the first undo empties the stop, the second restores the trigger.
    let fx = Fixture::new(&format!("{BEFORE}{INDENT}ff"));
    fx.cursor_to_line_end(1);
    expand(&fx.view, &whole_trigger(compiled(&set, "ff")), |_| true).expect("expands");
    fx.type_text("a");
    assert_eq!(fx.text(), format!("{BEFORE}    \\frac{{a}}{{}}"));
    fx.buffer().undo();
    assert_eq!(fx.text(), format!("{BEFORE}    \\frac{{}}{{}}"));
    fx.buffer().undo();
    assert_eq!(fx.text(), format!("{BEFORE}{INDENT}ff"));
    fx.assert_line_index_exact();
    common::assert_no_new_problems(problems);
}

fn sessions_can_be_ended() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new(&format!("{INDENT}beg"));
    fx.cursor_to_line_end(0);
    let snippet = expand(&fx.view, &manual_match(&set, "beg", "beg"), |_| true).expect("expands");
    assert!(
        snippet.buffer().is_some(),
        "the session is active at stop 1"
    );
    fx.type_text("a");
    let enabled = fx.view.enables_snippets();
    snippets::end_snippet_sessions(&fx.view);
    assert!(snippet.buffer().is_none(), "the session has ended");
    assert_eq!(
        fx.view.enables_snippets(),
        enabled,
        "the setting is restored"
    );
    // Typing no longer updates the mirror.
    fx.type_text("b");
    assert_eq!(fx.text(), "    \\begin{ab}\n      \n    \\end{a}");

    // A snippet without numbered stops ends as soon as it is pushed.
    let fx = Fixture::new("x->");
    fx.cursor_to_line_end(0);
    let snippet =
        expand(&fx.view, &whole_trigger(compiled(&set, "->")), |_| true).expect("expands");
    assert!(snippet.buffer().is_none());
    assert_eq!(fx.text(), r"x\to");
    common::assert_no_new_problems(problems);
}

fn manual_prefix_is_replaced() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new(&format!("{BEFORE}{INDENT}th"));
    fx.cursor_to_line_end(1);
    let m = manual_match(&set, "th", "thm");
    assert_eq!(m.trigger_len, 2);
    expand(&fx.view, &m, |_| true).expect("expands");
    fx.type_text("[Yoneda]");
    assert_eq!(
        fx.text(),
        format!("{BEFORE}    \\begin{{theorem}}[Yoneda]\n      \n    \\end{{theorem}}")
    );
    fx.assert_line_index_exact();
    common::assert_no_new_problems(problems);
}

fn text_around_the_trigger_is_kept() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new("  $α ff$ é\nnext");
    let cursor = fx.buffer().iter_at_line_offset(0, 7).expect("line 0");
    fx.buffer().place_cursor(&cursor);
    let m = set.auto_match("  $α ff", &math()).expect("ff fires");
    expand(&fx.view, &m, |_| true).expect("expands");
    fx.type_text("1");
    assert_eq!(fx.text(), "  $α \\frac{1}{}$ é\nnext");
    assert_eq!(fx.cursor(), char_len(r"  $α \frac{1"));

    let fx = Fixture::new("  $x$ dm tail");
    let cursor = fx.buffer().iter_at_line_offset(0, 8).expect("line 0");
    fx.buffer().place_cursor(&cursor);
    let m = set
        .auto_match("  $x$ dm", &Context::Text)
        .expect("dm fires");
    expand(&fx.view, &m, |_| true).expect("expands");
    fx.type_text("y");
    assert_eq!(fx.text(), "  $x$ \\[\n    y\n  \\]\n   tail");
    fx.assert_line_index_exact();

    // A multibyte trigger: exactly its characters are replaced.
    let user = SnippetSet::from_toml(
        r"
        [[snippet]]
        trigger = 'αα'
        body = '\alpha'
        auto = true
        context = 'math'
        ",
    )
    .expect("a valid snippet file");
    let fx = Fixture::new("é αα");
    fx.cursor_to_line_end(0);
    let m = user.auto_match("é αα", &math()).expect("αα fires");
    assert_eq!(m.trigger_len, 4);
    expand(&fx.view, &m, |_| true).expect("expands");
    assert_eq!(fx.text(), r"é \alpha");
    fx.assert_line_index_exact();
    common::assert_no_new_problems(problems);
}

fn warnings_are_counted() {
    let before = common::logged_problems();
    common::expect_logged_problems(1);
    glib::g_warning!(
        "lemma-test",
        "an expected warning (checks the warning counter)"
    );
    assert_eq!(common::logged_problems(), before + 1);
}

fn tab_indentation_is_copied() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new("\t\tbeg");
    fx.cursor_to_line_end(0);
    let m = manual_match(&set, "beg", "beg");
    expand(&fx.view, &m, |_| true).expect("expands");
    fx.type_text("proof");
    assert_eq!(fx.text(), "\t\t\\begin{proof}\n\t\t  \n\t\t\\end{proof}");

    // A line without indentation stays unindented.
    let fx = Fixture::new("beg");
    fx.cursor_to_line_end(0);
    let m = manual_match(&set, "beg", "beg");
    expand(&fx.view, &m, |_| true).expect("expands");
    assert_eq!(fx.text(), "\\begin{}\n  \n\\end{}");
    common::assert_no_new_problems(problems);
}

fn nested_expansion_inside_a_stop() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let ctx = math();
    let fx = Fixture::new(&format!("{INDENT}ff"));
    fx.cursor_to_line_end(0);
    let m = set.auto_match("    ff", &ctx).expect("ff fires");
    expand(&fx.view, &m, |_| true).expect("expands");
    fx.type_text("f");
    fx.type_text("f");
    let line = fx.line_before_cursor();
    assert_eq!(line, r"    \frac{ff");
    let m = set.auto_match(&line, &ctx).expect("ff fires after `{`");
    expand(&fx.view, &m, |_| true).expect("expands inside the outer stop");
    fx.type_text("a");
    assert_eq!(fx.text(), r"    \frac{\frac{a}{}}{}");
    fx.assert_line_index_exact();
    common::assert_no_new_problems(problems);
}

fn stale_match_changes_nothing() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new(&format!("{INDENT}xx"));
    fx.cursor_to_line_end(0);
    let m = whole_trigger(compiled(&set, "ff"));
    let err = expand(&fx.view, &m, |_| true).expect_err("the trigger is not there");
    assert_eq!(err, ExpandError::TriggerNotBeforeCursor("ff".into()));
    assert_eq!(fx.text(), "    xx");
    assert!(!fx.buffer().can_undo());

    // A `trigger_len` longer than the trigger.
    let bad = SnippetMatch {
        trigger_len: 5,
        ..whole_trigger(compiled(&set, "ff"))
    };
    let err = expand(&fx.view, &bad, |_| true).expect_err("an invalid match");
    assert_eq!(err, ExpandError::TriggerNotBeforeCursor("ff".into()));

    // `expand_chunks` checks the typed text the same way.
    let err = expand_chunks(&fx.view, &[stop(0, "")], "yy").expect_err("not before the cursor");
    assert_eq!(err, ExpandError::TriggerNotBeforeCursor("yy".into()));
    assert_eq!(fx.text(), "    xx");
    common::assert_no_new_problems(problems);
}

fn read_only_view_changes_nothing() {
    let problems = common::logged_problems();
    let set = SnippetSet::defaults();
    let fx = Fixture::new(&format!("{INDENT}ff"));
    fx.cursor_to_line_end(0);
    fx.view.set_editable(false);
    let m = whole_trigger(compiled(&set, "ff"));
    assert_eq!(
        expand(&fx.view, &m, |_| true).expect_err("read-only"),
        ExpandError::NotEditable
    );
    assert_eq!(fx.text(), "    ff");
    common::assert_no_new_problems(problems);
}

// ───────────── Helpers ─────────────

/// What expanding one snippet and typing "X" produced.
struct Expanded {
    /// The selection right after the push (a pre-selected default).
    selected: String,
    /// The buffer after typing.
    text: String,
    /// The cursor after typing, as a char offset.
    cursor: i32,
    /// The focus position of each chunk of the pushed snippet.
    focus: Vec<i32>,
    /// Whether the document's `LineIndex` matched its text after typing.
    line_index_exact: bool,
}

impl Expanded {
    /// Differences from `expected` (the expanded lines; "X" marks the typed text, whose first
    /// occurrence must be just before the cursor) and the expected focus positions.
    fn mismatches(&self, trigger: &str, expected: &str, focus: &[i32]) -> Vec<String> {
        let mut out = Vec::new();
        let text = format!("{BEFORE}{expected}{AFTER}");
        if self.text != text {
            out.push(format!(
                "{trigger}: buffer {:?}, expected {text:?}",
                self.text
            ));
        }
        let x = text.find('X').expect("expected text contains X");
        let cursor = char_len(&text[..=x]);
        if self.cursor != cursor {
            out.push(format!(
                "{trigger}: cursor at {}, expected {cursor} (after the typed X)",
                self.cursor
            ));
        }
        if self.focus != focus {
            out.push(format!(
                "{trigger}: focus positions {:?}, expected {focus:?}",
                self.focus
            ));
        }
        if !self.line_index_exact {
            out.push(format!("{trigger}: the document's LineIndex is stale"));
        }
        out
    }
}

/// Expands `compiled` (whole trigger typed) on the line `INDENT + trigger` between two other
/// lines and types "X" at the first focus position.
fn expand_on_indented_line(
    compiled: &Compiled,
    macro_defined: impl FnOnce(&str) -> bool,
) -> Expanded {
    let trigger = &compiled.def.trigger;
    let fx = Fixture::new(&format!("{BEFORE}{INDENT}{trigger}{AFTER}"));
    fx.cursor_to_line_end(1);
    let snippet = expand(&fx.view, &whole_trigger(compiled), macro_defined)
        .unwrap_or_else(|e| panic!("{trigger}: {e}"));
    let selected = fx.selection();
    fx.type_text("X");
    Expanded {
        selected,
        text: fx.text(),
        cursor: fx.cursor(),
        focus: (0..snippet.n_chunks())
            .map(|i| snippet.nth_chunk(i).focus_position())
            .collect(),
        line_index_exact: fx.line_index_exact(),
    }
}

/// The match for `compiled` with its whole trigger typed (what `auto_match` returns).
fn whole_trigger(compiled: &Compiled) -> SnippetMatch<'_> {
    SnippetMatch {
        def: &compiled.def,
        chunks: &compiled.chunks,
        trigger_len: compiled.def.trigger.len(),
        alt_chunks: compiled.alt.as_deref(),
    }
}

fn compiled<'a>(set: &'a SnippetSet, trigger: &str) -> &'a Compiled {
    set.iter()
        .find(|c| c.def.trigger == trigger)
        .unwrap_or_else(|| panic!("no snippet {trigger:?}"))
}

/// The manual match of `trigger` for `prefix` typed on an indented line in text.
fn manual_match<'a>(set: &'a SnippetSet, prefix: &str, trigger: &str) -> SnippetMatch<'a> {
    set.manual(prefix, &format!("{INDENT}{prefix}"), &Context::Text)
        .into_iter()
        .find(|m| m.def.trigger == trigger)
        .unwrap_or_else(|| panic!("{prefix:?} offers {trigger:?}"))
}

/// The default of the stop the cursor visits first: the lowest numbered stop, else `$0`.
fn first_default(chunks: &[Chunk]) -> &str {
    let stops = chunks.iter().filter_map(|c| match c {
        Chunk::Stop { n, default } => Some((*n, default.as_str())),
        _ => None,
    });
    stops
        .min_by_key(|&(n, _)| if n == 0 { u8::MAX } else { n })
        .map_or("", |(_, default)| default)
}

fn assert_cases_cover<'a>(set: &SnippetSet, triggers: impl Iterator<Item = &'a str>) {
    assert_eq!(set.len(), 71, "the curated defaults");
    let all: Vec<&Compiled> = set.iter().collect();
    assert_cases_cover_subset(&all, triggers);
}

/// The table lists each snippet of `compiled` exactly once and nothing else.
fn assert_cases_cover_subset<'a>(compiled: &[&Compiled], triggers: impl Iterator<Item = &'a str>) {
    let mut table: Vec<&str> = triggers.collect();
    let mut snippets: Vec<&str> = compiled.iter().map(|c| c.def.trigger.as_str()).collect();
    table.sort_unstable();
    snippets.sort_unstable();
    let unique = {
        let mut u = snippets.clone();
        u.dedup();
        u.len()
    };
    assert_eq!(unique, snippets.len(), "default triggers are unique");
    assert_eq!(
        table, snippets,
        "the expected table covers exactly these snippets"
    );
}

fn stop(n: u8, default: &str) -> Chunk {
    Chunk::Stop {
        n,
        default: default.into(),
    }
}

fn math() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

fn char_len(s: &str) -> i32 {
    i32::try_from(s.chars().count()).expect("short text")
}

/// A `TexView` on an open `Document`, as an editor tab has them, inside an unshown window.
struct Fixture {
    doc: DocRef,
    view: TexView,
    window: gtk::Window,
}

impl Fixture {
    fn new(text: &str) -> Self {
        let path = PathBuf::from("/nonexistent/lemma-test/main.tex");
        let buffer = document::new_buffer(&path);
        buffer.set_text(text);
        buffer.set_modified(false);
        let doc = Document::new_open(path, buffer, sourceview5::File::new());
        let view = TexView::new(&doc, &EditorCfg::default()).expect("an open document");
        let window = gtk::Window::new();
        window.set_child(Some(&view));
        Self { doc, view, window }
    }

    fn buffer(&self) -> gtk::TextBuffer {
        self.view.buffer()
    }

    fn text(&self) -> String {
        self.doc.text()
    }

    fn cursor_to_line_end(&self, line: i32) {
        let mut iter = self.buffer().iter_at_line(line).expect("the line exists");
        if !iter.ends_line() {
            iter.forward_to_line_end();
        }
        self.buffer().place_cursor(&iter);
    }

    /// The cursor as a char offset.
    fn cursor(&self) -> i32 {
        let buffer = self.buffer();
        buffer.iter_at_mark(&buffer.get_insert()).offset()
    }

    fn selection(&self) -> String {
        self.buffer()
            .selection_bounds()
            .map(|(a, b)| a.slice(&b).to_string())
            .unwrap_or_default()
    }

    fn line_before_cursor(&self) -> String {
        let buffer = self.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let mut start = cursor;
        start.set_line_offset(0);
        start.slice(&cursor).to_string()
    }

    /// Types `text` the way GtkTextView commits input-method text: one user action that
    /// replaces the selection.
    fn type_text(&self, text: &str) {
        let buffer = self.buffer();
        buffer.begin_user_action();
        buffer.delete_selection(true, true);
        buffer.insert_interactive_at_cursor(text, true);
        buffer.end_user_action();
    }

    fn line_index_exact(&self) -> bool {
        *self.doc.line_index() == LineIndex::new(&self.doc.text())
    }

    fn assert_line_index_exact(&self) {
        assert!(self.line_index_exact(), "the document's LineIndex is stale");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.window.destroy();
    }
}
