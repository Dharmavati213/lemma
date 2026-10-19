//! `str_replace`, `minimal_edits` and `unified_diff`.

use lemma_core::{TextEdit, apply_all};
use lemma_review::{EditError, minimal_edits, str_replace, unified_diff};
use proptest::prelude::*;
use std::path::PathBuf;

fn replaced(current: &str, edits: &[TextEdit]) -> String {
    let mut s = current.to_owned();
    apply_all(&mut s, edits);
    s
}

// ---- str_replace -------------------------------------------------------------------------------

#[test]
fn str_replace_errors() {
    assert_eq!(
        str_replace("abc", "x", "y", false),
        Err(EditError::NotFound)
    );
    assert_eq!(str_replace("abc", "x", "y", true), Err(EditError::NotFound));
    assert_eq!(str_replace("abc", "", "y", false), Err(EditError::Empty));
    assert_eq!(
        str_replace("a b a b a", "a", "c", false),
        Err(EditError::NotUnique(3))
    );
    // Occurrences are counted without overlap.
    assert_eq!(
        str_replace("aaaa", "aa", "b", false),
        Err(EditError::NotUnique(2))
    );
    assert_eq!(str_replace("aaa", "aa", "b", false).map(|e| e.len()), Ok(1));
}

#[test]
fn str_replace_single_is_trimmed() {
    let cur = "\\begin{align}\n  a &= b \\\\\n\\end{align}\n";
    let edits = str_replace(cur, "a &= b", "a &= c", false).unwrap();
    assert_eq!(
        edits,
        [TextEdit {
            range: 21..22,
            text: "c".into()
        }]
    );
    assert_eq!(replaced(cur, &edits), cur.replace("a &= b", "a &= c"));

    // Pure insertion and pure deletion after trimming.
    let edits = str_replace("x = 1;", "x = 1", "x = 10", false).unwrap();
    assert_eq!(edits, [TextEdit::insert(5, "0")]);
    let edits = str_replace("hello big world", "hello big world", "hello world", false).unwrap();
    assert_eq!(edits, [TextEdit::delete(6..10)]);
    // Identical replacement: nothing to do.
    assert_eq!(str_replace("abc", "b", "b", false), Ok(vec![]));
}

#[test]
fn str_replace_all_is_descending() {
    let cur = "\\alpha + \\alpha^2 = \\alpha";
    let edits = str_replace(cur, "\\alpha", "\\beta", true).unwrap();
    assert_eq!(edits.len(), 3);
    assert!(edits.windows(2).all(|w| w[1].range.end <= w[0].range.start));
    // "\\alpha" → "\\beta" shares the leading backslash and the final "a".
    assert_eq!(
        edits[0],
        TextEdit {
            range: 21..25,
            text: "bet".into()
        }
    );
    assert_eq!(replaced(cur, &edits), "\\beta + \\beta^2 = \\beta");
    // Adjacent matches.
    let edits = str_replace("abab", "ab", "aXb", true).unwrap();
    assert_eq!(edits, [TextEdit::insert(3, "X"), TextEdit::insert(1, "X")]);
    assert_eq!(replaced("abab", &edits), "aXbaXb");
}

#[test]
fn str_replace_multibyte() {
    // 'é' (C3 A9) and 'è' (C3 A8) share their first byte: trimming must not split it.
    let cur = "café crème";
    let edits = str_replace(cur, "café", "cafè", false).unwrap();
    assert_eq!(
        edits,
        [TextEdit {
            range: 3..5,
            text: "è".into()
        }]
    );
    assert_eq!(replaced(cur, &edits), "cafè crème");
    // 'Ä' (C3 84) and 'Ą' (C4 84) share their last byte.
    let edits = str_replace("xÄy", "Ä", "Ą", false).unwrap();
    assert_eq!(
        edits,
        [TextEdit {
            range: 1..3,
            text: "Ą".into()
        }]
    );
    let edits = str_replace("∑∏∑", "∑", "∫", true).unwrap();
    assert_eq!(replaced("∑∏∑", &edits), "∫∏∫");
    for e in &edits {
        assert!("∑∏∑".is_char_boundary(e.range.start) && "∑∏∑".is_char_boundary(e.range.end));
    }
}

// ---- minimal_edits -----------------------------------------------------------------------------

#[test]
fn minimal_edits_examples() {
    assert_eq!(minimal_edits("a\nb\n", "a\nb\n"), vec![]);
    let edits = minimal_edits("a\nb\nc\n", "a\nB\nc\n");
    assert_eq!(
        edits,
        [TextEdit {
            range: 2..3,
            text: "B".into()
        }]
    );
    let edits = minimal_edits("1\n2\n3\n4\n5\n6\n7\n8\n", "1\nX\n3\n4\n5\n6\n8\n");
    assert_eq!(
        edits,
        [
            TextEdit::delete(12..14),
            TextEdit {
                range: 2..3,
                text: "X".into()
            }
        ]
    );
    assert_eq!(minimal_edits("", "new\n"), [TextEdit::insert(0, "new\n")]);
    assert_eq!(minimal_edits("old\n", ""), [TextEdit::delete(0..4)]);
    assert_eq!(
        minimal_edits("a\r\nb\r\n", "a\nb\r\n"),
        [TextEdit::delete(1..2)]
    );
}

fn text() -> impl Strategy<Value = String> {
    let line = prop::sample::select(vec![
        "",
        "a",
        "b",
        "\\x",
        "é",
        "∑ x",
        "\r",
        "a b",
        "\\begin{x}",
        "%",
    ]);
    (prop::collection::vec(line, 0..12), any::<bool>()).prop_map(|(lines, nl)| {
        let mut s = lines.join("\n");
        if nl && !s.is_empty() {
            s.push('\n');
        }
        s
    })
}

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    ProptestConfig::with_cases(cases)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn minimal_edits_round_trip(a in text(), b in text()) {
        let edits = minimal_edits(&a, &b);
        prop_assert!(edits.windows(2).all(|w| w[1].range.end <= w[0].range.start));
        prop_assert_eq!(replaced(&a, &edits), b);
    }

    #[test]
    fn str_replace_matches_std_replace(
        cur in text(),
        old in "[ab\\\\é]{1,3}",
        new in "[abé]{0,3}",
    ) {
        match str_replace(&cur, &old, &new, true) {
            Ok(edits) => prop_assert_eq!(replaced(&cur, &edits), cur.replace(&old, &new)),
            Err(e) => prop_assert_eq!(e, EditError::NotFound),
        }
    }
}

// ---- unified_diff ------------------------------------------------------------------------------

/// Compares with `tests/golden/<name>.diff`; `UPDATE_GOLDEN=1` rewrites it.
fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.diff"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("read golden file");
    assert_eq!(actual, expected, "golden {name} differs");
}

#[test]
fn unified_diff_golden() {
    let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let new = old
        .replace("line 2\n", "line two\n")
        .replace("line 5\n", "")
        .replace("line 17\n", "line 17\nline 17b\n");
    golden("context", &unified_diff("chapters/a.tex", &old, &new));
    golden(
        "no_newline",
        &unified_diff("main.tex", "a\nb\nc", "a\nb\nC"),
    );
    golden(
        "new_file",
        &unified_diff("new.tex", "", "\\section{New}\nText.\n"),
    );
    golden("deleted_file", &unified_diff("gone.tex", "x\n", ""));
    assert_eq!(unified_diff("same.tex", "a\n", "a\n"), "");
}

#[test]
fn unified_diff_merges_close_changes() {
    let old: String = (1..=12).map(|i| format!("{i}\n")).collect();
    let new = old.replace("3\n", "three\n").replace("9\n", "nine\n");
    let diff = unified_diff("f", &old, &new);
    // Lines 3 and 9 are 5 apart (≤ 2 × 3 context): one hunk.
    assert_eq!(diff.matches("@@").count(), 2, "{diff}");
    assert!(diff.contains("@@ -1,12 +1,12 @@"), "{diff}");
    let new = old.replace("2\n", "two\n").replace("10\n", "ten\n");
    let diff = unified_diff("f", &old, &new);
    assert_eq!(diff.matches("@@").count(), 4, "{diff}");
}
