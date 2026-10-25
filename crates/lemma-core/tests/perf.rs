//! Performance tests. Run with `cargo test --release -p lemma-core -- --ignored perf`.
//!
//! They read `tests/fixtures/large.tex` (60k lines).

use lemma_core::{LineIndex, TextEdit};
use std::path::Path;
use std::time::{Duration, Instant};

fn large() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/large.tex");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every `x` of `text` replaced by `yy` as a buffer reports it: a delete and an insert per match,
/// from the last match up (`Document::apply_edits`) or from the first one down (GtkSourceView's
/// replace all). Returns the changes in order and the final text.
fn replace_x(text: &str, down: bool) -> (Vec<TextEdit>, String) {
    let at: Vec<usize> = text.match_indices('x').map(|(i, _)| i).collect();
    let mut changes = Vec::with_capacity(2 * at.len());
    if down {
        for &a in at.iter().rev() {
            changes.push(TextEdit::delete(a..a + 1));
            changes.push(TextEdit::insert(a, "yy"));
        }
    } else {
        for (k, &a) in at.iter().enumerate() {
            // Each earlier match grew the text by one byte.
            let a = a + k;
            changes.push(TextEdit::delete(a..a + 1));
            changes.push(TextEdit::insert(a, "yy"));
        }
    }
    (changes, text.replace('x', "yy"))
}

/// The line index through the 40 000 changes of a replace all of 20 000 matches in `large.tex`,
/// either way round: ≤ 50 ms each (6–17 ms on a loaded machine; 0.23 s while it moved every line
/// start after each change).
#[test]
#[ignore = "performance test; run in release with --ignored perf"]
fn perf_line_index_replace_all_large_tex() {
    let text = large();
    for down in [true, false] {
        let (changes, after) = replace_x(&text, down);
        assert_eq!(changes.len(), 40_000);
        let mut index = LineIndex::new(&text);
        let started = Instant::now();
        for e in &changes {
            std::hint::black_box(index.line_delta(e));
            index.apply(e);
        }
        let took = started.elapsed();
        assert_eq!(index, LineIndex::new(&after));
        let way = if down {
            "from the end"
        } else {
            "from the start"
        };
        eprintln!("perf_line_index_replace_all_large_tex, {way}: {took:?}");
        if !cfg!(debug_assertions) {
            assert!(took < Duration::from_millis(50), "{way}: {took:?}");
        }
    }
}
