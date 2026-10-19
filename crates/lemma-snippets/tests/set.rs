//! SnippetSet behaviour on small hand-written sets: TOML validation, merging, match ordering,
//! manual matches and `if_undefined` alternatives.

mod common;

use common::{align, arg, fired, math, text, to_body, triggers};
use lemma_snippets::{Chunk, SnippetError, SnippetSet, parse_body, with_indent};
use lemma_syntax::Context;

fn set(src: &str) -> SnippetSet {
    SnippetSet::from_toml(src).unwrap_or_else(|e| panic!("{e}"))
}

fn one(fields: &str) -> String {
    format!("[[snippet]]\n{fields}\n")
}

fn body_error(src: &str) -> String {
    match SnippetSet::from_toml(src) {
        Err(SnippetError::Body { msg, .. }) => msg,
        other => panic!("expected a body error, got {other:?}"),
    }
}

// ───── from_toml ─────

#[test]
fn empty_file_is_an_empty_set() {
    let s = set("");
    assert_eq!(s.len(), 0);
    assert!(s.is_empty());
    assert!(s.auto_match("ff", &math()).is_none());
}

#[test]
fn invalid_toml_and_unknown_keys_are_toml_errors() {
    for src in [
        "[[snippet]]\ntrigger = 'a'\nbody = ",
        &one("trigger = 'a'\nbody = 'b'\nwordtrig = false"),
        &one("trigger = 'a'\nbody = 'b'\ncontext = 'maths'"),
        &one("trigger = 'a'"),
        "[[snippets]]\ntrigger = 'a'\nbody = 'b'\n",
    ] {
        assert!(
            matches!(SnippetSet::from_toml(src), Err(SnippetError::Toml(_))),
            "{src:?}"
        );
    }
}

#[test]
fn empty_trigger_is_rejected() {
    assert_eq!(
        body_error(&one("trigger = ''\nbody = 'b'")),
        "empty trigger"
    );
}

#[test]
fn malformed_bodies_are_rejected_with_the_trigger() {
    let src = one("trigger = 'x'\nbody = '${1:abc'");
    match SnippetSet::from_toml(&src) {
        Err(SnippetError::Body { trigger, msg }) => {
            assert_eq!(trigger, "x");
            assert!(msg.starts_with("body: "), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    assert!(body_error(&one("trigger = 'x'\nbody = '$0$0'")).contains("only once"));
}

#[test]
fn a_dollar_in_a_default_is_its_own_error() {
    for src in [
        one("trigger = 'x'\nbody = '${1:$2}'"),
        one("trigger = 'x'\nbody = '\\N'\nif_undefined = '${1:$}'"),
    ] {
        assert_eq!(
            SnippetSet::from_toml(&src).unwrap_err(),
            SnippetError::DefaultHasDollar {
                trigger: "x".into()
            }
        );
    }
}

#[test]
fn if_undefined_needs_a_single_control_word_body() {
    let msg = body_error(&one(
        "trigger = 'x'\nbody = '\\frac{$1}'\nif_undefined = 'y'",
    ));
    assert!(msg.contains("if_undefined"), "{msg}");
    let msg = body_error(&one("trigger = 'x'\nbody = '\\N'\nif_undefined = '${1:a'"));
    assert!(msg.starts_with("if_undefined: "), "{msg}");
}

#[test]
fn errors_display_their_cause() {
    let e = SnippetSet::from_toml(&one("trigger = ''\nbody = 'b'")).unwrap_err();
    assert_eq!(e.to_string(), "snippet ``: empty trigger");
    let e = SnippetSet::from_toml("x = ").unwrap_err();
    assert!(e.to_string().starts_with("invalid snippets file: "), "{e}");
}

// ───── merged ─────

const USER_FF: &str = "[[snippet]]
trigger = 'ff'
body = '\\dfrac{$1}{$2}'
auto = true
context = 'math'
";

#[test]
fn user_entries_replace_defaults_with_the_same_key() {
    let merged = SnippetSet::merged(SnippetSet::defaults(), set(USER_FF));
    assert_eq!(merged.len(), 71);
    let m = merged.auto_match("x ff", &math()).expect("ff fires");
    assert_eq!(to_body(m.chunks), r"\dfrac{$1}{$2}$0");
    assert_eq!(merged.iter().filter(|c| c.def.trigger == "ff").count(), 1);
    // Untouched defaults are kept.
    assert_eq!(fired(&merged, "x sq", &math()), Some("sq"));
    assert_eq!(merged.manual("thm", "thm", &text()).len(), 1);
}

#[test]
fn a_different_context_or_auto_flag_adds_instead_of_replacing() {
    let user = set("[[snippet]]
trigger = 'ff'
body = 'any-ff'
auto = true

[[snippet]]
trigger = 'ff'
body = 'manual-ff'
context = 'math'

[[snippet]]
trigger = 'qq'
body = '\\quad'
auto = true
context = 'math'
");
    let merged = SnippetSet::merged(SnippetSet::defaults(), user);
    assert_eq!(merged.len(), 74);
    // Same rank as the default math `ff`: the user's entry comes first and wins.
    let m = merged.auto_match("x ff", &math()).expect("fires");
    assert_eq!(to_body(m.chunks), "any-ff$0");
    // In text only the user's `any` snippet qualifies.
    assert_eq!(
        to_body(merged.auto_match("x ff", &text()).expect("fires").chunks),
        "any-ff$0"
    );
    assert_eq!(fired(&merged, "x qq", &math()), Some("qq"));
    assert_eq!(triggers(&merged.manual("ff", "x ff", &math())), ["ff"]);
}

// ───── auto_match ordering and rules ─────

#[test]
fn longest_trigger_wins_before_priority() {
    let s = set("[[snippet]]
trigger = 'b'
body = 'short'
auto = true
word_trig = false
priority = 100

[[snippet]]
trigger = 'ab'
body = 'long'
auto = true
");
    let m = s.auto_match(" ab", &text()).expect("fires");
    assert_eq!(m.def.trigger, "ab");
    assert_eq!(m.trigger_len, 2);
    // When the longer trigger cannot fire, the shorter one does.
    assert_eq!(fired(&s, "xab", &text()), Some("b"));
}

#[test]
fn priority_then_entry_order_break_ties() {
    let s = set("[[snippet]]
trigger = 'ab'
body = 'first'
auto = true

[[snippet]]
trigger = 'ab'
body = 'second'
auto = true
context = 'text'

[[snippet]]
trigger = 'ab'
body = 'third'
auto = true
context = 'math'
priority = 1
");
    assert_eq!(
        to_body(s.auto_match("ab", &text()).expect("fires").chunks),
        "first$0"
    );
    assert_eq!(
        to_body(s.auto_match("ab", &math()).expect("fires").chunks),
        "third$0"
    );
}

#[test]
fn line_begin_autosnippets_need_only_whitespace_before() {
    let s = set(&one(
        "trigger = 'xx'\nbody = 'X'\nauto = true\nline_begin = true",
    ));
    for line in ["xx", "  xx", "\t xx"] {
        assert_eq!(fired(&s, line, &text()), Some("xx"), "{line:?}");
    }
    for line in ["a xx", "(xx", r"\ xx"] {
        assert_eq!(fired(&s, line, &text()), None, "{line:?}");
    }
}

#[test]
fn any_context_snippets_fire_anywhere_but_comments_and_verbatim() {
    let s = set(&one(
        "trigger = '...'\nbody = '\\dots'\nauto = true\nword_trig = false",
    ));
    let fires = [
        text(),
        math(),
        align(),
        Context::TextInMath,
        Context::CommandName { start: 0 },
        arg(),
    ];
    for ctx in fires {
        assert_eq!(fired(&s, "a...", &ctx), Some("..."), "{ctx:?}");
    }
    for ctx in [Context::Comment, Context::Verbatim] {
        assert_eq!(fired(&s, "a...", &ctx), None, "{ctx:?}");
    }
}

#[test]
fn manual_snippets_never_fire_automatically() {
    let s = set(&one("trigger = 'zz'\nbody = 'Z'"));
    assert_eq!(fired(&s, "zz", &text()), None);
}

#[test]
fn only_the_end_of_line_before_counts() {
    let s = SnippetSet::defaults();
    assert_eq!(fired(&s, "x ff ", &math()), None);
    assert_eq!(fired(&s, "x f", &math()), None);
    assert_eq!(fired(&s, "", &math()), None);
}

// ───── manual ─────

#[test]
fn manual_matches_by_prefix_and_reports_the_prefix_length() {
    let s = SnippetSet::defaults();
    let m = s.manual("e", "e", &text());
    assert_eq!(triggers(&m), ["eg", "eq", "eqs", "enum"]);
    assert!(m.iter().all(|m| m.trigger_len == 1));
    assert_eq!(triggers(&s.manual("su", "Then su", &text())), ["sub"]);
    assert!(s.manual("thx", "thx", &text()).is_empty());
}

#[test]
fn manual_with_an_empty_prefix_lists_by_priority() {
    let s = SnippetSet::defaults();
    let at_line_start = s.manual("", "", &text());
    assert_eq!(at_line_start.len(), 21);
    assert!(at_line_start[..18].iter().all(|m| m.def.priority == 2000));
    assert_eq!(triggers(&at_line_start[18..]), ["sec", "sub", "ssub"]);
    // Mid-line only the sections qualify.
    assert_eq!(
        triggers(&s.manual("", "Text ", &text())),
        ["sec", "sub", "ssub"]
    );
}

#[test]
fn manual_respects_context_word_boundary_and_backslash() {
    let s = SnippetSet::defaults();
    assert_eq!(triggers(&s.manual("sec", "see sec", &math())), ["sec"]);
    assert!(s.manual("sec", "% sec", &Context::Comment).is_empty());
    assert!(s.manual("sec", "x", &Context::Verbatim).is_empty());
    assert!(s.manual("sec", r"\sec", &math()).is_empty());
    assert!(s.manual("sec", "xsec", &text()).is_empty());
    // Autosnippets are not offered.
    assert!(s.manual("ff", "x ff", &math()).is_empty());

    let math_only = set(&one("trigger = 'mat'\nbody = 'M'\ncontext = 'math'"));
    assert_eq!(triggers(&math_only.manual("ma", "x ma", &math())), ["mat"]);
    assert!(math_only.manual("ma", "x ma", &text()).is_empty());
    let text_only = set(&one("trigger = 'txt'\nbody = 'T'\ncontext = 'text'"));
    assert_eq!(
        triggers(&text_only.manual("t", "t", &Context::TextInMath)),
        ["txt"]
    );
    assert!(text_only.manual("t", "t", &math()).is_empty());
}

#[test]
fn manual_line_begin_needs_only_whitespace_before_the_prefix() {
    let s = SnippetSet::defaults();
    assert_eq!(triggers(&s.manual("beg", "  beg", &text())), ["beg"]);
    assert!(s.manual("thm", r"\begin{thm", &text()).is_empty());
    assert!(s.manual("ii", "a ii", &text()).is_empty());
    // A line_before that does not end with the prefix is the text before it.
    assert_eq!(triggers(&s.manual("thm", "  ", &text())), ["thm"]);
}

#[test]
fn exact_line_begin_trigger() {
    let s = SnippetSet::defaults();
    assert!(s.is_exact_line_begin_trigger("eq", "  eq"));
    assert!(s.is_exact_line_begin_trigger("ii", "\tii"));
    assert!(!s.is_exact_line_begin_trigger("th", "th"));
    assert!(!s.is_exact_line_begin_trigger("", ""));
    assert!(!s.is_exact_line_begin_trigger("thm", "a thm"));
    assert!(
        !s.is_exact_line_begin_trigger("sec", "sec"),
        "not a line_begin snippet"
    );
    assert!(!s.is_exact_line_begin_trigger("ff", "ff"), "autosnippet");
}

// ───── if_undefined / alt ─────

#[test]
fn if_undefined_snippets_carry_the_alternative() {
    let s = SnippetSet::defaults();
    let m = s.auto_match("x NN", &math()).expect("NN fires");
    assert_eq!(m.def.required_macro(), Some("N"));
    assert_eq!(m.chunks, [Chunk::Text(r"\N".into()), stop0()]);
    assert_eq!(
        m.alt_chunks,
        Some(&[Chunk::Text(r"\mathbb{N}".into()), stop0()][..])
    );

    let m = s.auto_match("x hom", &math()).expect("hom fires");
    assert_eq!(m.def.required_macro(), Some("Hom"));
    assert_eq!(
        m.alt_chunks.map(to_body).as_deref(),
        Some(r"\operatorname{Hom}$0")
    );

    let m = s.auto_match("x ff", &math()).expect("ff fires");
    assert_eq!(m.def.required_macro(), None);
    assert_eq!(m.alt_chunks, None);
}

#[test]
fn every_if_undefined_default_names_its_body_macro() {
    let s = SnippetSet::defaults();
    let with_alt: Vec<_> = s.iter().filter(|c| c.def.if_undefined.is_some()).collect();
    assert_eq!(with_alt.len(), 12);
    for c in with_alt {
        let name = c.def.required_macro().expect("a control word");
        assert_eq!(format!("\\{name}"), c.def.body);
        let alt = c.alt.as_deref().expect("compiled alternative");
        assert_eq!(
            Some(alt),
            parse_body(c.def.if_undefined.as_deref().unwrap_or_default())
                .ok()
                .as_deref()
        );
    }
}

#[test]
fn user_if_undefined_snippet() {
    let s = set(&one(
        "trigger = 'FF'\nbody = '\\F'\nif_undefined = '\\mathbb{F}_{${1:p}}'\nauto = true\ncontext = 'math'",
    ));
    let m = s.auto_match("x FF", &math()).expect("fires");
    assert_eq!(m.def.required_macro(), Some("F"));
    assert_eq!(
        m.alt_chunks.map(to_body).as_deref(),
        Some(r"\mathbb{F}_{${1:p}}$0")
    );
}

// ───── expansion helpers on the defaults ─────

#[test]
fn defaults_indent_for_an_indented_line() {
    let s = SnippetSet::defaults();
    let m = s.manual("itm", "    itm", &text()).pop().expect("itm");
    assert_eq!(
        to_body(&with_indent(m.chunks, "    ")),
        "\\begin{itemize}\n      \\item $0\n    \\end{itemize}"
    );
    let m = s.manual("beg", "beg", &text()).pop().expect("beg");
    assert_eq!(
        with_indent(m.chunks, "  "),
        [
            Chunk::Text(r"\begin{".into()),
            Chunk::Stop {
                n: 1,
                default: String::new()
            },
            Chunk::Text("}\n    ".into()),
            stop0(),
            Chunk::Text("\n  \\end{".into()),
            Chunk::Mirror(1),
            Chunk::Text("}".into()),
        ]
    );
    let m = s.manual("fig", "fig", &text()).pop().expect("fig");
    assert!(m.chunks.contains(&Chunk::Stop {
        n: 1,
        default: "0.8".into()
    }));
}

fn stop0() -> Chunk {
    Chunk::Stop {
        n: 0,
        default: String::new(),
    }
}
