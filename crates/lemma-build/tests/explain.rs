//! `lemma_build::explain` on real logs (WP3.7): every error of the fixtures' logs
//! (`tests/fixtures/logs/*`) and of one real `pdflatex` run per error class
//! (`tests/data/explain/<case>/main.{tex,log}`) is explained, with the right fix hint, and the
//! quick-fix edits computed from them produce the right text.

use lemma_build::explain::{self, ErrorKind, FixHint};
use lemma_build::parse_log;
use lemma_core::{Diagnostic, LineIndex, Severity, TextEdit, apply_all};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/logs");
    fs::canonicalize(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

fn data() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/explain")
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .into_owned()
}

/// Every diagnostic of `dir/log` (absolute names count as existing files; project files must
/// exist in `dir`).
fn diagnostics(dir: &Path, log: &str) -> Vec<Diagnostic> {
    let exists = |p: &Path| {
        if p.is_absolute() {
            p.extension().is_some()
        } else {
            dir.join(p).is_file() || p.extension().is_some_and(|e| e == "aux" || e == "bbl")
        }
    };
    parse_log(&read(&dir.join(log)), dir, &exists).diagnostics
}

fn errors(dir: &Path, log: &str) -> Vec<Diagnostic> {
    diagnostics(dir, log)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

fn kind(d: &Diagnostic) -> Option<ErrorKind> {
    explain::explain(&d.message, d.context.as_deref()).map(|e| e.kind)
}

/// The math-only commands of the tests (the app asks the completion tables).
fn math(name: &str) -> bool {
    ["alpha", "beta", "frac", "sum", "le"].contains(&name)
}

#[test]
fn every_fixture_error_is_explained() {
    let expected: &[(&str, &str, &[ErrorKind])] = &[
        (
            "undefcs",
            "main.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "undefcs",
            "main.79.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "errininput",
            "main.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "errininput",
            "main.79.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "errininput",
            "wide.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "errininput_lualatex",
            "main.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "errininput_xelatex",
            "main.log",
            &[ErrorKind::UndefinedControlSequence],
        ),
        (
            "missingdollar",
            "main.log",
            &[
                ErrorKind::MissingDollar,
                ErrorKind::InvalidInMathMode,
                ErrorKind::MissingDollar,
            ],
        ),
        (
            "missingdollar",
            "main.79.log",
            &[
                ErrorKind::MissingDollar,
                ErrorKind::InvalidInMathMode,
                ErrorKind::MissingDollar,
            ],
        ),
        ("missinginput", "main.log", &[ErrorKind::FileNotFound]),
        ("missinginput", "main.79.log", &[ErrorKind::FileNotFound]),
        ("missingbrace", "main.log", &[ErrorKind::FileEnded]),
        ("missingbrace", "main.79.log", &[ErrorKind::FileEnded]),
        ("runaway", "main.log", &[ErrorKind::FileEnded]),
        ("runaway", "main.79.log", &[ErrorKind::FileEnded]),
    ];
    for (variant, log, kinds) in expected {
        let errors = errors(&fixtures().join(variant), log);
        let got: Vec<Option<ErrorKind>> = errors.iter().map(kind).collect();
        let want: Vec<Option<ErrorKind>> = kinds.iter().copied().map(Some).collect();
        assert_eq!(got, want, "{variant}/{log}");
    }
    // No other fixture log has errors this table does not know.
    for entry in fs::read_dir(fixtures()).unwrap() {
        let dir = entry.unwrap().path();
        for log in fs::read_dir(&dir).unwrap() {
            let log = log.unwrap().path();
            if log.extension().is_some_and(|e| e == "log") {
                let name = log.file_name().unwrap().to_str().unwrap().to_owned();
                for e in errors(&dir, &name) {
                    assert!(
                        kind(&e).is_some(),
                        "{}: unexplained error {:?}",
                        log.display(),
                        e.message
                    );
                }
            }
        }
    }
}

#[test]
fn fixture_warnings_are_explained() {
    let found = |variant: &str| -> HashSet<ErrorKind> {
        diagnostics(&fixtures().join(variant), "main.log")
            .iter()
            .filter_map(kind)
            .collect()
    };
    assert!(found("undefref").contains(&ErrorKind::ReferenceUndefined));
    assert!(found("undefcite").contains(&ErrorKind::CitationUndefined));
    assert!(found("overfull").contains(&ErrorKind::OverfullBox));
    let warns = found("warns");
    assert!(
        warns.contains(&ErrorKind::LabelMultiplyDefined),
        "{warns:?}"
    );
}

/// (case directory, the kind of its first error).
const CASES: &[(&str, ErrorKind)] = &[
    ("undefcs", ErrorKind::UndefinedControlSequence),
    ("missingdollar", ErrorKind::MissingDollar),
    ("envundef", ErrorKind::EnvironmentUndefined),
    ("envtypo", ErrorKind::EnvironmentUndefined),
    ("mismatch", ErrorKind::MismatchedEnd),
    ("filenotfound", ErrorKind::FileNotFound),
    ("graphicnotfound", ErrorKind::FileNotFound),
    ("nobegindoc", ErrorKind::MissingBeginDocument),
    ("extrabrace", ErrorKind::ExtraBraceOrDollar),
    ("toomany", ErrorKind::TooManyBraces),
    ("missingrbrace", ErrorKind::MissingBrace),
    ("missinglbrace", ErrorKind::ArgumentExtraBrace),
    ("extratab", ErrorKind::ExtraAlignmentTab),
    ("misplacedtab", ErrorKind::MisplacedAlignmentTab),
    ("doublesup", ErrorKind::DoubleScript),
    ("doublesub", ErrorKind::DoubleScript),
    ("displayend", ErrorKind::DisplayMathEnd),
    ("parended", ErrorKind::ParagraphEnded),
    ("fileended", ErrorKind::FileEnded),
    ("nolinetoend", ErrorKind::NoLineToEnd),
    ("alreadydefined", ErrorKind::AlreadyDefined),
    ("missingitem", ErrorKind::MissingItem),
    ("lonelyitem", ErrorKind::MissingItem),
    ("onlypreamble", ErrorKind::OnlyInPreamble),
    ("unicode", ErrorKind::UnicodeCharacter),
    ("invalidinmath", ErrorKind::MissingDollar),
    ("missingnumber", ErrorKind::MissingNumber),
    ("illegalunit", ErrorKind::IllegalUnit),
    ("optionclash", ErrorKind::OptionClash),
    ("outerpar", ErrorKind::NotInOuterParMode),
    ("missingright", ErrorKind::LeftRight),
    ("extraright", ErrorKind::LeftRight),
    ("paramnumber", ErrorKind::IllegalParameter),
];

#[test]
fn every_error_class_matches_a_real_log() {
    let mut seen = HashSet::new();
    for (case, want) in CASES {
        let dir = data().join(case);
        let errors = errors(&dir, "main.log");
        let first = errors
            .first()
            .unwrap_or_else(|| panic!("{case}: no error parsed"));
        let explanation = explain::explain(&first.message, first.context.as_deref())
            .unwrap_or_else(|| panic!("{case}: {:?} is not explained", first.message));
        assert_eq!(explanation.kind, *want, "{case}: {:?}", first.message);
        assert!(
            explanation.text.ends_with('.') && !explanation.text.contains('\n'),
            "{case}: one sentence: {:?}",
            explanation.text
        );
        // Every error of these logs is explained, consequences included.
        for e in &errors {
            assert!(kind(e).is_some(), "{case}: unexplained {:?}", e.message);
        }
        seen.insert(*want);
    }
    // Every error kind of the table has a real log (the warnings come from the fixtures);
    // InvalidInMathMode comes from the missingdollar fixture.
    for kind in ErrorKind::ALL {
        if !kind.is_warning() && kind != ErrorKind::InvalidInMathMode {
            assert!(seen.contains(&kind), "no real log for {kind:?}");
        }
    }
    assert!(ErrorKind::ALL.iter().filter(|k| !k.is_warning()).count() >= 20);
}

/// The first error of `case` and its fix hint.
fn first_error(case: &str) -> (Diagnostic, Option<FixHint>, String) {
    let dir = data().join(case);
    let d = errors(&dir, "main.log").into_iter().next().unwrap();
    let hint = explain::explain(&d.message, d.context.as_deref()).and_then(|e| e.fix);
    (d, hint, read(&dir.join("main.tex")))
}

/// The 1-based `line` of `text` with its start offset.
fn line_of(text: &str, line: u32) -> (usize, &str) {
    let index = LineIndex::new(text);
    let start = index.line_start(line - 1);
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    (start, &text[start..end])
}

fn applied(text: &str, edits: &[TextEdit]) -> String {
    let mut s = text.to_owned();
    apply_all(&mut s, edits);
    s
}

#[test]
fn quick_fix_edits_on_real_errors() {
    // Missing $ inserted: the math-only run is wrapped.
    let (d, hint, text) = first_error("missingdollar");
    assert_eq!(hint, Some(FixHint::WrapInMath));
    let (start, line) = line_of(&text, d.line.unwrap());
    let edits = explain::wrap_in_math_edit(line, start, d.context.as_deref().unwrap(), &math)
        .expect("a math run");
    assert!(applied(&text, &edits).contains("The value $x_1$ is positive."));

    // The same on the fixture: `\alpha_n` (and nothing for the `$` TeX inserts at the end).
    let dir = fixtures().join("missingdollar");
    let text = read(&dir.join("main.tex"));
    let found = errors(&dir, "main.log");
    let (start, line) = line_of(&text, found[0].line.unwrap());
    let edits =
        explain::wrap_in_math_edit(line, start, found[0].context.as_deref().unwrap(), &math)
            .unwrap();
    assert!(applied(&text, &edits).contains("Exactness $\\alpha_n$ follows"));
    let (start, line) = line_of(&text, found[2].line.unwrap());
    assert_eq!(
        explain::wrap_in_math_edit(line, start, found[2].context.as_deref().unwrap(), &math),
        None
    );

    // Undefined control sequence: the name, and the closest macro on the line.
    let (d, hint, text) = first_error("undefcs");
    assert_eq!(hint, Some(FixHint::UndefinedCommand { name: "foo".into() }));
    let second = errors(&data().join("undefcs"), "main.log")[1].clone();
    let Some(FixHint::UndefinedCommand { name }) =
        explain::explain(&second.message, second.context.as_deref()).and_then(|e| e.fix)
    else {
        panic!("no hint");
    };
    assert_eq!(name, "lamda");
    let best = explain::closest(&name, ["alpha", "lambda", "Lambda", "label"]).unwrap();
    let (start, line) = line_of(&text, d.line.unwrap());
    let edits = explain::replace_command_edits(line, start, &name, best);
    assert!(applied(&text, &edits).contains("Hello \\foo world and \\lambda."));

    // Mismatched \end.
    let (d, hint, text) = first_error("mismatch");
    let Some(FixHint::MismatchedEnd { begin, end }) = hint else {
        panic!("{hint:?}");
    };
    let (start, line) = line_of(&text, d.line.unwrap());
    let edit = explain::fix_end_edit(line, start, &begin, &end).unwrap();
    assert_eq!(
        applied(&text, &[edit]),
        text.replace("\\end{enumerate}", "\\end{itemize}")
    );

    // An undefined environment that is a typo: renamed at \begin and \end.
    let (d, hint, text) = first_error("envtypo");
    let Some(FixHint::UndefinedEnvironment { name }) = hint else {
        panic!("{hint:?}");
    };
    let best = explain::closest(&name, ["equation", "eqnarray", "enumerate"]).unwrap();
    let (start, _) = line_of(&text, d.line.unwrap());
    let edits = explain::rename_environment_edits(&text, start, &name, best).unwrap();
    assert_eq!(applied(&text, &edits), text.replace("equaiton", "equation"));

    // A theorem-like environment: declared.
    let (_, hint, text) = first_error("envundef");
    let Some(FixHint::UndefinedEnvironment { name }) = hint else {
        panic!("{hint:?}");
    };
    let edit = explain::newtheorem_edit(&text, &name, None).unwrap();
    assert!(applied(&text, &[edit]).starts_with(
        "\\documentclass{article}\n\\newtheorem{theorem}{Theorem}\n\\begin{document}"
    ));

    // File not found: the argument as written, and the closest file.
    let (d, hint, text) = first_error("filenotfound");
    let Some(FixHint::FileNotFound { name }) = hint else {
        panic!("{hint:?}");
    };
    assert_eq!(name, "chapters/intor.tex");
    let (start, line) = line_of(&text, d.line.unwrap());
    let (range, written) = explain::file_argument(line, start, &name).unwrap();
    assert_eq!(written, "chapters/intor");
    let found = explain::closest("intor", ["intro", "other"]).unwrap();
    let edit = TextEdit {
        range,
        text: explain::replacement_name(&written, &format!("chapters/{found}.tex")),
    };
    assert!(applied(&text, &[edit]).contains("\\input{chapters/intro}"));

    let (d, hint, text) = first_error("graphicnotfound");
    assert_eq!(
        hint,
        Some(FixHint::FileNotFound {
            name: "figs/plott".into()
        })
    );
    let (start, line) = line_of(&text, d.line.unwrap());
    assert!(explain::file_argument(line, start, "figs/plott").is_some());

    // Missing \begin{document}: inserted where the text starts.
    let (d, hint, text) = first_error("nobegindoc");
    assert_eq!(hint, Some(FixHint::MissingBeginDocument));
    let edit = explain::begin_document_edit(&text, d.line.unwrap()).unwrap();
    assert!(applied(&text, &[edit]).contains("\\begin{document}\nSome text in the preamble."));
}
