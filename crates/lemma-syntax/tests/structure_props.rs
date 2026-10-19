//! WP1.9 robustness: the structure parsers never panic on arbitrary TeX-like input, and every
//! reported line points at the command that produced it.

use lemma_syntax::{aux_inputs, index_file, parse_aux, parse_magic, scan_bib};
use proptest::prelude::*;

const FRAGMENTS: &[&str] = &[
    "\\section",
    "\\section*",
    "\\addchap",
    "\\begin",
    "\\end",
    "{",
    "}",
    "[",
    "]",
    "(",
    ")",
    "%",
    "\n",
    "\n\n",
    " ",
    "\\label",
    "\\cite",
    "\\cites",
    "\\cref",
    "\\crefrange",
    "\\hyperref",
    "\\verb",
    "|",
    "+",
    "\\iffalse",
    "\\fi",
    "\\else",
    "\\newif",
    "\\ifx",
    "\\newcommand",
    "\\def",
    "#1",
    "\\let",
    "=",
    "\\input",
    "\\import",
    "\\subfile",
    "\\usepackage",
    "\\caption",
    "\\newtheorem",
    "\\declaretheorem",
    "\\DeclarePairedDelimiterX",
    "\\NewDocumentCommand",
    "\\newenvironment",
    "\\documentclass",
    "subfiles",
    "% !TEX root = ",
    "% !TeX program = ",
    "\\lstinline",
    "\\url",
    "\\href",
    "\\endinput",
    "\\\\",
    "\\%",
    "\\",
    "x",
    "a,b",
    "é",
    "—",
    "theorem",
    "verbatim",
    "document",
    "lemma",
    "*",
    ",",
    "\"",
    "#",
    "@",
    "@string",
    "@comment",
    "@preamble",
    "@book",
    "\\newlabel",
    "\\@input",
    "author",
    "year",
    "date",
    "title",
];

fn tex() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(FRAGMENTS), 0..160).prop_map(|v| v.concat())
}

/// 2000 cases unless `PROPTEST_CASES` says otherwise.
fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(2000);
    ProptestConfig::with_cases(cases)
}

fn line(text: &str, n: u32) -> &str {
    text.split('\n')
        .nth(n as usize)
        .unwrap_or_else(|| panic!("line {n} out of range"))
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn index_file_reports_consistent_lines(text in tex()) {
        let idx = index_file(&text);
        for o in &idx.outline {
            prop_assert!(o.level <= 7);
            let cmd = if o.level == 7 { "\\begin" } else { "\\" };
            prop_assert!(line(&text, o.line).contains(cmd));
        }
        for l in &idx.labels {
            prop_assert!(!l.name.is_empty());
            prop_assert!(line(&text, l.line).contains("\\label"));
        }
        for r in &idx.refs {
            prop_assert!(!r.names.is_empty());
            let cmd = format!("\\{}", r.cmd.trim_end_matches('*'));
            prop_assert!(line(&text, r.line).contains(&cmd));
        }
        for c in &idx.cites {
            prop_assert!(!c.keys.is_empty() && c.keys.iter().all(|k| k != "*"));
            let cmd = format!("\\{}", c.cmd.trim_end_matches('*'));
            prop_assert!(line(&text, c.line).contains(&cmd));
        }
        for e in &idx.envs {
            prop_assert!(line(&text, e.begin_line).contains("\\begin"));
            if let Some(end) = e.end_line {
                prop_assert!(end >= e.begin_line);
                prop_assert!(line(&text, end).contains("\\end"));
            }
        }
        for i in &idx.includes {
            prop_assert!(!i.target.is_empty());
            line(&text, i.line);
        }
        for m in &idx.macros {
            prop_assert!(m.nargs <= 9);
            line(&text, m.line);
        }
        prop_assert_eq!(&idx.magic, &parse_magic(&text));
    }

    #[test]
    fn bib_and_aux_never_panic(text in tex()) {
        for e in scan_bib(&text) {
            prop_assert!(!e.key.is_empty() && !e.key.contains(char::is_whitespace));
            prop_assert!(line(&text, e.line).contains('@'));
        }
        let aux = parse_aux(&text);
        prop_assert!(aux.labels.keys().all(|k| !k.ends_with("@cref")));
        aux_inputs(&text);
    }

    #[test]
    fn arbitrary_unicode_never_panics(text in "\\PC{0,200}") {
        index_file(&text);
        scan_bib(&text);
        parse_aux(&text);
        parse_magic(&text);
    }
}
