//! Helpers shared by the integration tests.
#![allow(dead_code)] // each test binary uses a subset

use lemma_snippets::{Chunk, SnippetMatch, SnippetSet};
use lemma_syntax::{ArgKind, Context};

pub fn text() -> Context {
    Context::Text
}

pub fn math() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

pub fn align() -> Context {
    Context::Math {
        display: true,
        env: Some("align".into()),
    }
}

pub fn arg() -> Context {
    Context::Arg {
        cmd: "label".into(),
        idx: 0,
        kind: ArgKind::Label,
        start: 7,
    }
}

/// Every context that is neither math nor text.
pub fn other_contexts() -> [Context; 4] {
    [
        Context::Comment,
        Context::Verbatim,
        Context::CommandName { start: 0 },
        arg(),
    ]
}

/// Renders chunks back into body syntax, so stops, mirrors and the implicit `$0` are visible.
pub fn to_body(chunks: &[Chunk]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        match chunk {
            Chunk::Text(t) => out.push_str(&t.replace('$', "${$}")),
            Chunk::Stop { n, default } if default.is_empty() => out.push_str(&format!("${n}")),
            Chunk::Stop { n, default } => out.push_str(&format!("${{{n}:{default}}}")),
            Chunk::Mirror(n) => out.push_str(&format!("${n}")),
        }
    }
    out
}

/// The trigger of the autosnippet that fires, if any.
pub fn fired<'a>(set: &'a SnippetSet, line_before: &str, ctx: &Context) -> Option<&'a str> {
    set.auto_match(line_before, ctx)
        .map(|m| m.def.trigger.as_str())
}

/// The triggers of the manual matches, in order.
pub fn triggers<'a>(matches: &[SnippetMatch<'a>]) -> Vec<&'a str> {
    matches.iter().map(|m| m.def.trigger.as_str()).collect()
}
