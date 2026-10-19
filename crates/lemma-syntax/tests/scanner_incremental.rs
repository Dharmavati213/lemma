//! Property test: after random LaTeX insertions and deletions, `LineStates::apply_edit` leaves
//! exactly the states of a fresh `rebuild`, and lines outside the returned range keep their
//! tokens.
//!
//! Runs 256 cases of up to 12 edits each by default; `PROPTEST_CASES` overrides the count.

use lemma_core::{LineIndex, TextEdit};
use lemma_syntax::{LineStates, ScanConfig, StrLines, Token};
use proptest::prelude::*;
use proptest::sample::Index;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

/// Fragments that open, close or neutralise scanner constructs.
const FRAGMENTS: &[&str] = &[
    "$",
    "$$",
    "\\(",
    "\\)",
    "\\[",
    "\\]",
    "{",
    "}",
    "[",
    "]",
    "(",
    ")",
    ",",
    "%",
    "\\%",
    "\\$",
    "\\",
    "\\\\",
    "\n",
    "\n\n",
    "\n  \n",
    " ",
    "x",
    "é",
    "\\text{",
    "\\textbf{",
    "\\ensuremath{",
    "\\begin{equation}",
    "\\end{equation}",
    "\\begin{align*}",
    "\\end{align*}",
    "\\begin{math}",
    "\\end{math}",
    "\\begin{verbatim}",
    "\\end{verbatim}",
    "\\begin{comment}\n",
    "\\end{comment}",
    "\\begin{lstlisting}[language=C]\n",
    "\\end{lstlisting}",
    "\\begin{tikzcd}",
    "\\end{tikzcd}",
    "\\begin{theorem}[",
    "\\begin{tabular}{",
    "\\begin{",
    "\\end{",
    "\\verb|",
    "\\verb|$|",
    "|",
    "\\lstinline{",
    "\\mintinline{py}|",
    "\\url{",
    "\\cite{",
    "\\cite[p.~",
    "\\cites(",
    "\\cref{a,",
    "\\label{",
    "\\ref{",
    "\\usepackage[",
    "\\hypersetup{\n",
    "\\item[(i)] ",
    "\\section{",
    "\\tag{",
    "\\intertext{",
];

fn fixtures() -> &'static [String] {
    static TEXTS: OnceLock<Vec<String>> = OnceLock::new();
    TEXTS.get_or_init(|| {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures");
        let read = |rel: &str| std::fs::read_to_string(format!("{root}/{rel}")).expect("fixture");
        let large: String = read("large.tex").split_inclusive('\n').take(150).collect();
        vec![
            read("proj/main.tex"),
            read("proj/chapters/yoneda.tex"),
            read("pdf/test.tex"),
            read("logs/missingdollar/main.tex"),
            read("logs/missingbrace/main.tex"),
            large,
            RULES.to_owned(),
        ]
    })
}

/// One of every construct the scanner tracks.
const RULES: &str = r#"\documentclass[11pt]{amsart}
\usepackage[
  backend=biber,
  style=alphabetic
]{biblatex}
\hypersetup{
  colorlinks,
  pdftitle={A, B}
}
\begin{document}
Text $a \text{if $b$ then} c$ and $$d$$ and \(e\) and \[f\].
\begin{align*}
  x &= y \intertext{and, by
  the lemma,}
  z &= w \tag{$*$}
\end{align*}
\cite{a,
  b} \cref{x,y} \crefrange{p}{q} \cites(pre)[a]{x}[b]{y}
\begin{verbatim}
$ % \end{verbatim*}
\end{verbatim}
\verb|$| \lstinline[language=C]{x%y} \mintinline{py}|$| \url{a%b}
\begin{tikzcd} A \arrow[r] & B \end{tikzcd}
\[ \begin{tikzcd} A \arrow[r, "f"] & B \end{tikzcd} \]
\item[(i)] \ensuremath{x^{2}} costs \$5, 10\% more.
\begin{comment}
$x
\end{comment}
\begin{theorem}[Main $p$-adic result \cref{a}]
$x

y
\end{theorem}
\begin{tabularx}{\linewidth}{lX} a & b \end{tabularx}
\end{document}
"#;

#[derive(Clone, Debug)]
enum Op {
    Insert(Index, &'static str),
    Delete(Index, usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<Index>(), proptest::sample::select(FRAGMENTS)).prop_map(|(at, s)| Op::Insert(at, s)),
        1 => (any::<Index>(), 1usize..40).prop_map(|(at, n)| Op::Delete(at, n)),
    ]
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn to_edit(text: &str, op: &Op) -> TextEdit {
    match op {
        Op::Insert(at, s) => TextEdit::insert(floor_boundary(text, at.index(text.len() + 1)), *s),
        Op::Delete(at, n) => {
            let a = floor_boundary(text, at.index(text.len() + 1));
            let b = floor_boundary(text, a + n);
            TextEdit::delete(a..b)
        }
    }
}

fn line_tokens(states: &LineStates, text: &str, index: &LineIndex) -> Vec<Vec<Token>> {
    (0..index.line_count())
        .map(|l| {
            let mut out = Vec::new();
            states.tokens(l, &text[index.line_range(l)], &mut out);
            out
        })
        .collect()
}

fn rebuilt(cfg: &Arc<ScanConfig>, text: &str, index: &LineIndex) -> LineStates {
    let mut fresh = LineStates::new(cfg.clone());
    fresh.rebuild(&StrLines { text, index });
    fresh
}

fn check_edits(fixture: usize, ops: &[Op]) -> Result<(), TestCaseError> {
    let cfg = Arc::new(ScanConfig::default());
    let mut text = fixtures()[fixture].clone();
    let mut index = LineIndex::new(&text);
    let mut states = rebuilt(&cfg, &text, &index);
    for op in ops {
        let edit = to_edit(&text, op);
        let before = line_tokens(&states, &text, &index);
        let (first, removed, added) = index.line_delta(&edit);
        edit.apply(&mut text);
        index.apply(&edit);
        let range: Range<u32> = states.apply_edit(
            &StrLines {
                text: &text,
                index: &index,
            },
            first,
            removed,
            added,
        );

        let fresh = rebuilt(&cfg, &text, &index);
        let after = line_tokens(&states, &text, &index);
        if states != fresh {
            let fresh_tokens = line_tokens(&fresh, &text, &index);
            let line = (0..after.len()).find(|&l| after[l] != fresh_tokens[l]);
            return Err(TestCaseError::fail(format!(
                "states differ from a rebuild after {edit:?} (delta {first},{removed},{added}; \
                 returned {range:?}); first line with different tokens: {line:?}\n{text}"
            )));
        }
        // The edited lines are always re-tagged.
        prop_assert_eq!(range.start, first);
        prop_assert!(range.end >= (first + added).min(index.line_count()));
        for (l, tokens) in after.iter().enumerate() {
            let l = l as u32;
            let old = if l < range.start {
                l
            } else if l >= range.end {
                l + removed - added
            } else {
                continue;
            };
            prop_assert_eq!(
                tokens,
                &before[old as usize],
                "line {} (old {}) changed outside {:?} after {:?}",
                l,
                old,
                range,
                edit
            );
        }
    }
    Ok(())
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    #[test]
    fn incremental_equals_rebuild(
        fixture in 0..7usize,
        ops in proptest::collection::vec(op(), 1..12),
    ) {
        check_edits(fixture, &ops)?;
    }
}

#[test]
fn typing_a_dollar_flips_the_rest_of_the_paragraph_only() {
    let cfg = Arc::new(ScanConfig::default());
    let mut text = String::from("a b\nc d\ne f\n\ng h\n");
    let mut index = LineIndex::new(&text);
    let mut states = rebuilt(&cfg, &text, &index);
    let edit = TextEdit::insert(2, "$");
    let (first, removed, added) = index.line_delta(&edit);
    edit.apply(&mut text);
    index.apply(&edit);
    let range = states.apply_edit(
        &StrLines {
            text: &text,
            index: &index,
        },
        first,
        removed,
        added,
    );
    // Lines 1–3 change state; the blank line 3 closes the math, so line 4 is unchanged.
    assert_eq!(range, 0..4);
    assert!(states == rebuilt(&cfg, &text, &index));
}

#[test]
fn typing_inside_a_line_rescans_one_line() {
    let cfg = Arc::new(ScanConfig::default());
    let mut text = fixtures()[0].clone();
    let mut index = LineIndex::new(&text);
    let mut states = rebuilt(&cfg, &text, &index);
    let at = text.find("faithfully flat").expect("fixture text");
    let edit = TextEdit::insert(at, "very ");
    let (first, removed, added) = index.line_delta(&edit);
    edit.apply(&mut text);
    index.apply(&edit);
    let range = states.apply_edit(
        &StrLines {
            text: &text,
            index: &index,
        },
        first,
        removed,
        added,
    );
    assert_eq!(range, first..first + 1);
}

#[test]
fn deleting_whole_lines() {
    let cfg = Arc::new(ScanConfig::default());
    let mut text = String::from("\\[\nx\n\\]\ny\n");
    let mut index = LineIndex::new(&text);
    let mut states = rebuilt(&cfg, &text, &index);
    // Delete the line "\]\n": line_delta reports (2, 2, 1).
    let edit = TextEdit::delete(5..8);
    let (first, removed, added) = index.line_delta(&edit);
    assert_eq!((first, removed, added), (2, 2, 1));
    edit.apply(&mut text);
    index.apply(&edit);
    let range = states.apply_edit(
        &StrLines {
            text: &text,
            index: &index,
        },
        first,
        removed,
        added,
    );
    assert_eq!(range, 2..4);
    assert!(states == rebuilt(&cfg, &text, &index));
    // Explicit pure removal (added = 0) of the last line.
    let mut states = rebuilt(&cfg, "a\nb", &LineIndex::new("a\nb"));
    let text = "a";
    let index = LineIndex::new(text);
    let range = states.apply_edit(
        &StrLines {
            text,
            index: &index,
        },
        1,
        1,
        0,
    );
    assert_eq!(range, 1..1);
    assert_eq!(states.line_count(), 1);
}
