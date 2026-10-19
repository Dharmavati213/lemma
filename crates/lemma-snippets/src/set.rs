//! Compiled snippet sets and LuaSnip-style trigger matching.

use std::cmp::Reverse;
use std::fmt;

use lemma_syntax::Context;
use serde::Deserialize;

use crate::body::{self, BodyError, Chunk};
use crate::def::{Cond, SnippetDef};

/// The curated defaults, compiled into the binary.
const DEFAULTS_TOML: &str = include_str!("../../../data/snippets.default.toml");

/// Why a snippet file or body was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnippetError {
    /// The file is not valid TOML or does not match the `[[snippet]]` schema (unknown keys included).
    Toml(String),
    /// The trigger is empty, the body or `if_undefined` body is malformed, or `if_undefined` is set
    /// on a body that is not a single control word.
    Body { trigger: String, msg: String },
    /// A `${N:default}` default contains `$` (nested tab stops are not supported).
    DefaultHasDollar { trigger: String },
}

impl fmt::Display for SnippetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(msg) => write!(f, "invalid snippets file: {msg}"),
            Self::Body { trigger, msg } => write!(f, "snippet `{trigger}`: {msg}"),
            Self::DefaultHasDollar { trigger } => {
                write!(
                    f,
                    "snippet `{trigger}`: a tab-stop default may not contain `$`"
                )
            }
        }
    }
}

impl std::error::Error for SnippetError {}

/// The `snippets.toml` file: an array of `[[snippet]]` tables.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnippetFile {
    #[serde(default)]
    snippet: Vec<SnippetDef>,
}

/// A definition with its parsed body.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// The definition as written.
    pub def: SnippetDef,
    /// The parsed `body`.
    pub chunks: Vec<Chunk>,
    /// The parsed `if_undefined` body, if any.
    pub alt: Option<Vec<Chunk>>,
}

impl Compiled {
    /// Validates `def` and parses its bodies.
    fn new(def: SnippetDef) -> Result<Self, SnippetError> {
        let invalid = |msg: String| SnippetError::Body {
            trigger: def.trigger.clone(),
            msg,
        };
        let body_error = |e: BodyError, field: &str| match e {
            BodyError::DefaultHasDollar(_) => SnippetError::DefaultHasDollar {
                trigger: def.trigger.clone(),
            },
            BodyError::Invalid(msg) => invalid(format!("{field}: {msg}")),
        };
        if def.trigger.is_empty() {
            return Err(invalid("empty trigger".into()));
        }
        let chunks = body::parse(&def.body).map_err(|e| body_error(e, "body"))?;
        let alt = match &def.if_undefined {
            None => None,
            Some(alt) => {
                if def.required_macro().is_none() {
                    return Err(invalid(
                        "`if_undefined` needs a body that is a single control word such as `\\N`"
                            .into(),
                    ));
                }
                Some(body::parse(alt).map_err(|e| body_error(e, "if_undefined"))?)
            }
        };
        Ok(Self { def, chunks, alt })
    }

    /// Whether the text before the trigger allows it to expand, ignoring the context:
    /// never right after `\` (the trigger would be part of a control-sequence name, so `\sum`
    /// never expands `sum`), the word boundary of `word_trig`, and `line_begin`.
    fn fits_position(&self, before: &str) -> bool {
        let prev = before.chars().next_back();
        prev != Some('\\')
            && !(self.def.word_trig && prev.is_some_and(is_word_char))
            && (!self.def.line_begin || before.chars().all(char::is_whitespace))
    }

    /// [`Self::fits_position`] and the snippet's [`Cond`] holds in `ctx`.
    fn fits(&self, before: &str, ctx: &Context) -> bool {
        self.fits_position(before) && cond_holds(self.def.context, ctx)
    }

    fn as_match(&self, trigger_len: usize) -> SnippetMatch<'_> {
        SnippetMatch {
            def: &self.def,
            chunks: &self.chunks,
            trigger_len,
            alt_chunks: self.alt.as_deref(),
        }
    }
}

/// LuaSnip's word characters, `[A-Za-z0-9_]`.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `cond` allows a snippet in `ctx`. Nothing expands in comments or verbatim text.
fn cond_holds(cond: Cond, ctx: &Context) -> bool {
    match cond {
        Cond::Any => !matches!(ctx, Context::Comment | Context::Verbatim),
        Cond::Math => ctx.is_math(),
        Cond::Text => matches!(ctx, Context::Text | Context::TextInMath),
    }
}

/// The text before `prefix` when `line_before` ends with it, else all of `line_before`.
fn text_before<'a>(line_before: &'a str, prefix: &str) -> &'a str {
    line_before.strip_suffix(prefix).unwrap_or(line_before)
}

/// An ordered collection of compiled snippets.
#[derive(Clone, Debug)]
pub struct SnippetSet {
    items: Vec<Compiled>,
}

/// A snippet that matches the text before the cursor.
#[derive(Debug)]
pub struct SnippetMatch<'a> {
    /// The matching definition.
    pub def: &'a SnippetDef,
    /// The chunks to expand (not yet indented; see [`crate::with_indent`]).
    pub chunks: &'a [Chunk],
    /// Bytes before the cursor to delete (the trigger).
    pub trigger_len: usize,
    /// The chunks to expand instead when the project lacks [`SnippetDef::required_macro`].
    pub alt_chunks: Option<&'a [Chunk]>,
}

impl SnippetSet {
    /// The 71 curated default snippets (`data/snippets.default.toml`, via include_str!).
    pub fn defaults() -> Self {
        Self::from_toml(DEFAULTS_TOML)
            .expect("data/snippets.default.toml is valid (checked by the crate's tests)")
    }

    /// Parses a snippets.toml file. Every entry is validated; the first invalid one is reported.
    pub fn from_toml(src: &str) -> Result<Self, SnippetError> {
        let file: SnippetFile =
            toml::from_str(src).map_err(|e| SnippetError::Toml(e.to_string()))?;
        let items = file
            .snippet
            .into_iter()
            .map(Compiled::new)
            .collect::<Result<_, _>>()?;
        Ok(Self { items })
    }

    /// User entries replace defaults with the same (trigger, context, auto).
    ///
    /// The user's entries come first, so on an equal rank they win over the remaining defaults.
    pub fn merged(defaults: Self, user: Self) -> Self {
        let key = |d: &SnippetDef| (d.trigger.clone(), d.context, d.auto);
        let mut items = user.items;
        let replaced: Vec<_> = items.iter().map(|c| key(&c.def)).collect();
        items.extend(
            defaults
                .items
                .into_iter()
                .filter(|c| !replaced.contains(&key(&c.def))),
        );
        Self { items }
    }

    /// Number of snippets.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when the set has no snippets.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The compiled snippets in match order (for the completer and expansion tests).
    pub fn iter(&self) -> std::slice::Iter<'_, Compiled> {
        self.items.iter()
    }

    /// Autosnippet check after ONE typed character. `line_before` = line text up to the cursor (inclusive of the typed char).
    /// Rules: never right after `\`; wordTrig (char before trigger not [A-Za-z0-9_] unless at line start);
    /// line_begin (only whitespace before the trigger); Cond::Math = `Context::Math`;
    /// Cond::Text = Text|TextInMath; never in Comment/Verbatim. The longest trigger wins, then the
    /// highest priority, then the earliest entry.
    pub fn auto_match(&self, line_before: &str, ctx: &Context) -> Option<SnippetMatch<'_>> {
        let rank = |c: &Compiled| (c.def.trigger.len(), c.def.priority);
        let mut best: Option<&Compiled> = None;
        for c in self.items.iter().filter(|c| c.def.auto) {
            let Some(before) = line_before.strip_suffix(c.def.trigger.as_str()) else {
                continue;
            };
            // Strictly greater, so the earliest entry wins a tie.
            if c.fits(before, ctx) && best.is_none_or(|b| rank(c) > rank(b)) {
                best = Some(c);
            }
        }
        best.map(|c| c.as_match(c.def.trigger.len()))
    }

    /// Manual snippets for the completion popup / Tab expansion: the non-auto snippets whose
    /// trigger starts with `prefix` (case-sensitive), in descending priority, then entry order.
    /// `line_before` is the line up to the cursor and ends with `prefix`; the position and context
    /// rules of [`Self::auto_match`] apply to the text before `prefix`, so line_begin snippets
    /// require only whitespace there (the trigger is the only token). `trigger_len` is `prefix.len()`.
    pub fn manual(&self, prefix: &str, line_before: &str, ctx: &Context) -> Vec<SnippetMatch<'_>> {
        let before = text_before(line_before, prefix);
        let mut out: Vec<_> = self
            .items
            .iter()
            .filter(|c| !c.def.auto && c.def.trigger.starts_with(prefix) && c.fits(before, ctx))
            .map(|c| c.as_match(prefix.len()))
            .collect();
        out.sort_by_key(|m| Reverse(m.def.priority));
        out
    }

    /// True if `prefix` exactly equals a line_begin trigger valid here (then the popup shows only that snippet).
    /// Checks the manual line_begin snippets' position rules only, not their [`Cond`].
    pub fn is_exact_line_begin_trigger(&self, prefix: &str, line_before: &str) -> bool {
        let before = text_before(line_before, prefix);
        self.items.iter().any(|c| {
            !c.def.auto && c.def.line_begin && c.def.trigger == prefix && c.fits_position(before)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        assert_eq!(SnippetSet::defaults().len(), 71);
    }

    #[test]
    fn errors_display_the_trigger() {
        let e = SnippetSet::from_toml("[[snippet]]\ntrigger = 'x'\nbody = '${1:$}'").unwrap_err();
        assert_eq!(
            e.to_string(),
            "snippet `x`: a tab-stop default may not contain `$`"
        );
    }

    #[test]
    fn text_before_strips_the_prefix() {
        assert_eq!(text_before("  thm", "thm"), "  ");
        assert_eq!(text_before("  ", "thm"), "  ");
        assert_eq!(text_before("x", ""), "x");
    }
}
