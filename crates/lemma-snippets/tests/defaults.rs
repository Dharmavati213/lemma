//! The 71 curated defaults: counts, one fire test per autosnippet, and the fire / no-fire
//! table.

mod common;

use common::{align, fired, math, other_contexts, text, to_body, triggers};
use lemma_snippets::{Cond, SnippetSet};
use lemma_syntax::Context;

/// Math autosnippets: trigger, body and `if_undefined` body in canonical body syntax.
const MATH_AUTOS: &[(&str, &str, Option<&str>)] = &[
    ("ff", r"\frac{$1}{$2}$0", None),
    ("sq", r"\sqrt{$1}$0", None),
    ("td", "^{$1}$0", None),
    ("__", "_{$1}$0", None),
    ("inv", "^{-1}$0", None),
    ("sum", r"\sum_{$1}^{$2}$0", None),
    ("prod", r"\prod_{$1}^{$2}$0", None),
    ("lim", r"\lim_{$1}$0", None),
    ("int", r"\int_{$1}^{$2}$0", None),
    ("cas", "\\begin{cases}\n  $1\n\\end{cases}$0", None),
    ("set", r"\{$1\}$0", None),
    ("cal", r"\mathcal{$1}$0", None),
    ("bb", r"\mathbb{$1}$0", None),
    ("mfr", r"\mathfrak{$1}$0", None),
    ("ol", r"\overline{$1}$0", None),
    ("hat", r"\hat{$1}$0", None),
    ("til", r"\tilde{$1}$0", None),
    ("wt", r"\widetilde{$1}$0", None),
    ("wh", r"\widehat{$1}$0", None),
    ("tt", r"\text{$1}$0", None),
    ("lr(", r"\left( $1 \right)$0", None),
    ("lr[", r"\left[ $1 \right]$0", None),
    ("lr{", r"\left\{ $1 \right\}$0", None),
    ("lr|", r"\left| $1 \right|$0", None),
    ("->", r"\to$0", None),
    ("=>", r"\implies$0", None),
    ("<=", r"\leq$0", None),
    (">=", r"\geq$0", None),
    ("map", r"\mapsto$0", None),
    ("inj", r"\hookrightarrow$0", None),
    ("surj", r"\twoheadrightarrow$0", None),
    ("iff", r"\iff$0", None),
    ("inn", r"\in$0", None),
    ("xx", r"\times$0", None),
    ("AA", r"\forall$0", None),
    ("EE", r"\exists$0", None),
    ("hom", r"\Hom$0", Some(r"\operatorname{Hom}$0")),
    ("spec", r"\Spec$0", Some(r"\operatorname{Spec}$0")),
    ("proj", r"\Proj$0", Some(r"\operatorname{Proj}$0")),
    ("ker", r"\Ker$0", Some(r"\operatorname{Ker}$0")),
    ("coker", r"\coker$0", Some(r"\operatorname{coker}$0")),
    ("ext", r"\Ext$0", Some(r"\operatorname{Ext}$0")),
    ("NN", r"\N$0", Some(r"\mathbb{N}$0")),
    ("ZZ", r"\Z$0", Some(r"\mathbb{Z}$0")),
    ("QQ", r"\Q$0", Some(r"\mathbb{Q}$0")),
    ("RR", r"\R$0", Some(r"\mathbb{R}$0")),
    ("CC", r"\C$0", Some(r"\mathbb{C}$0")),
    ("KK", r"\K$0", Some(r"\mathbb{K}$0")),
];

/// Text autosnippets, as in [`MATH_AUTOS`].
const TEXT_AUTOS: &[(&str, &str, Option<&str>)] = &[
    ("mk", "${$}$1${$}$0", None),
    ("dm", "\\[\n  $1\n\\]\n$0", None),
];

/// The autosnippets that expand inside a word (`word_trig = false`).
const IN_WORD: &[&str] = &[
    "td", "inv", "__", "lr(", "lr[", "lr{", "lr|", "->", "=>", "<=", ">=",
];

const LINE_BEGIN_ENVS: &[&str] = &[
    "thm", "lem", "prop", "cor", "def", "rem", "pf", "eg", "eq", "eqs", "ali", "alis", "gat",
    "beg", "itm", "enum", "ii", "fig",
];

const SECTIONS: &[&str] = &["sec", "sub", "ssub"];

fn sorted<'a>(it: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut v: Vec<_> = it.into_iter().collect();
    v.sort_unstable();
    v
}

#[test]
fn defaults_load_exactly_71_in_four_groups() {
    let set = SnippetSet::defaults();
    assert_eq!(set.len(), 71);
    assert!(!set.is_empty());
    assert_eq!(set.iter().count(), 71);

    let group = |pred: &dyn Fn(&lemma_snippets::SnippetDef) -> bool| {
        sorted(
            set.iter()
                .filter(|c| pred(&c.def))
                .map(|c| c.def.trigger.as_str()),
        )
    };
    let line_begin_manual = group(&|d| !d.auto && d.line_begin);
    let other_manual = group(&|d| !d.auto && !d.line_begin);
    let text_auto = group(&|d| d.auto && d.context == Cond::Text);
    let math_auto = group(&|d| d.auto && d.context == Cond::Math);
    assert_eq!(line_begin_manual.len(), 18);
    assert_eq!(other_manual.len(), 3);
    assert_eq!(text_auto.len(), 2);
    assert_eq!(math_auto.len(), 48);
    assert_eq!(18 + 3 + 2 + 48, set.len(), "the groups cover every snippet");

    assert_eq!(line_begin_manual, sorted(LINE_BEGIN_ENVS.iter().copied()));
    assert_eq!(other_manual, sorted(SECTIONS.iter().copied()));
    assert_eq!(text_auto, sorted(TEXT_AUTOS.iter().map(|r| r.0)));
    assert_eq!(math_auto, sorted(MATH_AUTOS.iter().map(|r| r.0)));
}

#[test]
fn manual_snippets_have_the_documented_flags() {
    let set = SnippetSet::defaults();
    for c in set.iter().filter(|c| !c.def.auto) {
        let d = &c.def;
        assert_eq!(d.context, Cond::Any, "{}", d.trigger);
        assert!(d.word_trig, "{}", d.trigger);
        assert_eq!(
            d.priority,
            if d.line_begin { 2000 } else { 0 },
            "{}",
            d.trigger
        );
        assert!(d.description.is_some(), "{}", d.trigger);
    }
}

#[test]
fn only_the_listed_autosnippets_expand_inside_words() {
    let set = SnippetSet::defaults();
    let in_word = sorted(
        set.iter()
            .filter(|c| !c.def.word_trig)
            .map(|c| c.def.trigger.as_str()),
    );
    assert_eq!(in_word, sorted(IN_WORD.iter().copied()));
}

/// One fire test per default autosnippet: it fires in its context with the expected body.
#[test]
fn every_default_autosnippet_fires_with_its_body() {
    let set = SnippetSet::defaults();
    let rows = MATH_AUTOS
        .iter()
        .map(|r| (r, math()))
        .chain(TEXT_AUTOS.iter().map(|r| (r, text())));
    let mut failures = Vec::new();
    for (&(trigger, body, alt), ctx) in rows {
        for lead in ["", "a + ", "(", "  "] {
            let line = format!("{lead}{trigger}");
            match set.auto_match(&line, &ctx) {
                Some(m) if m.def.trigger == trigger => {
                    assert_eq!(m.trigger_len, trigger.len(), "{line:?}");
                    assert_eq!(to_body(m.chunks), body, "{line:?}");
                    assert_eq!(m.alt_chunks.map(to_body).as_deref(), alt, "{line:?}");
                    assert_eq!(m.def.required_macro().is_some(), alt.is_some(), "{line:?}");
                }
                other => failures.push(format!(
                    "{line:?} in {ctx:?} -> {:?}",
                    other.map(|m| m.def.trigger.clone())
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "did not fire:\n{}",
        failures.join("\n")
    );
}

#[test]
fn math_autosnippets_fire_only_in_math() {
    let set = SnippetSet::defaults();
    for &(trigger, ..) in MATH_AUTOS {
        let line = format!("a {trigger}");
        assert_eq!(
            fired(&set, &line, &align()),
            Some(trigger),
            "{line:?} in align"
        );
        for ctx in [text(), Context::TextInMath]
            .into_iter()
            .chain(other_contexts())
        {
            assert_eq!(fired(&set, &line, &ctx), None, "{line:?} in {ctx:?}");
        }
    }
}

#[test]
fn text_autosnippets_fire_only_in_text() {
    let set = SnippetSet::defaults();
    for &(trigger, ..) in TEXT_AUTOS {
        let line = format!("Let {trigger}");
        assert_eq!(
            fired(&set, &line, &Context::TextInMath),
            Some(trigger),
            "{line:?}"
        );
        for ctx in [math(), align()].into_iter().chain(other_contexts()) {
            assert_eq!(fired(&set, &line, &ctx), None, "{line:?} in {ctx:?}");
        }
    }
}

#[test]
fn no_autosnippet_fires_right_after_a_backslash() {
    let set = SnippetSet::defaults();
    for &(trigger, ..) in MATH_AUTOS {
        let line = format!(r"a \{trigger}");
        assert_eq!(fired(&set, &line, &math()), None, "{line:?}");
    }
    for &(trigger, ..) in TEXT_AUTOS {
        let line = format!(r"a \{trigger}");
        assert_eq!(fired(&set, &line, &text()), None, "{line:?}");
    }
}

#[test]
fn word_boundary_applies_unless_word_trig_is_off() {
    let set = SnippetSet::defaults();
    let all = MATH_AUTOS
        .iter()
        .map(|r| (r.0, math()))
        .chain(TEXT_AUTOS.iter().map(|r| (r.0, text())));
    for (trigger, ctx) in all {
        for lead in ["x", "X", "9", "_"] {
            let line = format!("{lead}{trigger}");
            let expected = IN_WORD.contains(&trigger).then_some(trigger);
            assert_eq!(fired(&set, &line, &ctx), expected, "{line:?}");
        }
        for lead in [")", "}", "^", "α", "\t"] {
            let line = format!("{lead}{trigger}");
            assert_eq!(fired(&set, &line, &ctx), Some(trigger), "{line:?}");
        }
    }
}

// ───── Fire / no-fire table ─────

#[test]
fn table_xff_does_not_fire() {
    assert_eq!(fired(&SnippetSet::defaults(), "xff", &math()), None);
}

#[test]
fn table_x_space_ff_fires() {
    assert_eq!(fired(&SnippetSet::defaults(), "x ff", &math()), Some("ff"));
}

#[test]
fn table_paren_ff_fires() {
    assert_eq!(fired(&SnippetSet::defaults(), ")ff", &math()), Some("ff"));
}

#[test]
fn table_xtd_fires_in_word() {
    let set = SnippetSet::defaults();
    let m = set
        .auto_match("xtd", &math())
        .expect("td fires inside a word");
    assert_eq!(m.def.trigger, "td");
    assert_eq!(m.trigger_len, 2);
}

#[test]
fn table_backslash_sum_does_not_fire() {
    let set = SnippetSet::defaults();
    let typed = r"\sum";
    for end in 1..=typed.len() {
        assert_eq!(
            fired(&set, &typed[..end], &math()),
            None,
            "{:?}",
            &typed[..end]
        );
    }
    assert_eq!(fired(&set, "x sum", &math()), Some("sum"));
}

#[test]
fn table_mathbb_typed_letter_by_letter_never_fires_bb() {
    let set = SnippetSet::defaults();
    let typed = r"x + \mathbb";
    for end in 1..=typed.len() {
        assert_eq!(
            fired(&set, &typed[..end], &math()),
            None,
            "{:?}",
            &typed[..end]
        );
    }
    assert_eq!(fired(&set, "x + bb", &math()), Some("bb"));
}

#[test]
fn table_a_double_underscore_fires() {
    assert_eq!(fired(&SnippetSet::defaults(), "a__", &math()), Some("__"));
}

#[test]
fn table_mk_in_a_comment_does_not_fire() {
    let set = SnippetSet::defaults();
    assert_eq!(fired(&set, "% mk", &Context::Comment), None);
    assert_eq!(fired(&set, "Let mk", &text()), Some("mk"));
}

#[test]
fn table_mk_in_math_does_not_fire() {
    assert_eq!(fired(&SnippetSet::defaults(), "x mk", &math()), None);
}

#[test]
fn table_ff_in_text_does_not_fire() {
    assert_eq!(fired(&SnippetSet::defaults(), "the ff", &text()), None);
}

#[test]
fn table_thm_after_only_spaces_is_a_manual_match() {
    let set = SnippetSet::defaults();
    for line in ["thm", "    thm", "\tthm"] {
        let m = set.manual("thm", line, &text());
        assert_eq!(triggers(&m), ["thm"], "{line:?}");
        assert_eq!(m[0].trigger_len, 3);
        assert!(set.is_exact_line_begin_trigger("thm", line), "{line:?}");
    }
}

#[test]
fn table_x_thm_has_no_line_begin_match() {
    let set = SnippetSet::defaults();
    assert!(set.manual("thm", "x thm", &text()).is_empty());
    assert!(!set.is_exact_line_begin_trigger("thm", "x thm"));
}

#[test]
fn thm_is_not_an_autosnippet() {
    assert_eq!(fired(&SnippetSet::defaults(), "thm", &text()), None);
}
