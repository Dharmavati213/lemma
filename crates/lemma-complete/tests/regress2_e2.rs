//! Regression tests for verified area-E2 defects of the second review pass (lemma-complete).
//! Each test fails until its defect is fixed.

use lemma_complete::{Completer, CompletionDb, CompletionRequest, Insert, Item, WordList};
use lemma_snippets::{Chunk, SnippetSet};
use lemma_syntax::{Context, ProjectIndex, index_file};
use std::path::PathBuf;
use std::sync::Arc;

fn insert_text(item: &Item) -> String {
    match &item.insert {
        Insert::Text(t) => t.clone(),
        Insert::Snippet(chunks) => chunks
            .iter()
            .map(|c| match c {
                Chunk::Text(t) => t.clone(),
                Chunk::Stop { n, .. } | Chunk::Mirror(n) => format!("${n}"),
            })
            .collect(),
    }
}

/// E2-5: a user macro whose first argument is optional (`\newcommand{\norm}[2][]{…}`) must not
/// complete as two mandatory groups: `\norm{x}{2}` passes `x` as #2 and typesets a stray `2`.
#[test]
fn regress2_optional_first_argument_is_not_a_mandatory_group() {
    let text = "\\documentclass{article}\n\
                \\newcommand{\\norm}[2][]{\\left\\| #2 \\right\\|_{#1}}\n\
                \\begin{document}\n\\end{document}\n";
    let mut idx = ProjectIndex::default();
    idx.update_file(PathBuf::from("/p/main.tex"), Arc::new(index_file(text)));
    let db = CompletionDb::builtin();
    let snippets = SnippetSet::defaults();
    let words = WordList::default();
    let mut completer = Completer::new();
    let line = "$x = \\nor";
    let ctx = Context::Math {
        display: false,
        env: None,
    };
    let req = CompletionRequest::new(&ctx, line);
    let mut out = Vec::new();
    completer.complete(&db, &snippets, &idx, &words, &req, &mut out);
    let norm = out.iter().find(|i| i.label == "\\norm").unwrap_or_else(|| {
        panic!(
            "no \\norm in {:?}",
            out.iter().map(|i| &i.label).collect::<Vec<_>>()
        )
    });
    let inserted = insert_text(norm);
    assert!(
        !inserted.starts_with("\\norm{$1}{$2}"),
        "the optional first argument completes as a mandatory group: {inserted:?}"
    );
}
