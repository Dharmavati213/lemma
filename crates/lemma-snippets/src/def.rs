//! The `[[snippet]]` table of `snippets.toml`.

use serde::Deserialize;

/// Where a snippet may expand. No snippet ever expands in a comment or verbatim text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cond {
    /// Anywhere except comments and verbatim text.
    #[default]
    Any,
    /// Only in math (`Context::Math`).
    Math,
    /// Only in text: `Context::Text`, or text inside math such as `\text{..}` (`Context::TextInMath`).
    Text,
}

/// One `[[snippet]]` table in snippets.toml.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetDef {
    /// The text that expands the snippet; never empty.
    pub trigger: String,
    /// Body syntax: `$N`, `${N}`, `${N:default}` (N = 0..9) are tab stops; a repeated N is a mirror;
    /// `${$}` is a literal `$`; any other `$` is literal; backslash is literal (no escapes).
    pub body: String,
    /// Expands as soon as the trigger is typed, without Tab.
    #[serde(default)]
    pub auto: bool,
    /// Where the snippet may expand.
    #[serde(default)]
    pub context: Cond,
    /// The trigger must not follow a word character `[A-Za-z0-9_]`.
    #[serde(default = "yes")]
    pub word_trig: bool,
    /// Only whitespace may precede the trigger on the line.
    #[serde(default)]
    pub line_begin: bool,
    /// Breaks ties between autosnippet triggers of the same length; higher wins. Manual matches
    /// are listed by descending priority.
    #[serde(default)]
    pub priority: i32,
    /// Shown in the completion popup.
    #[serde(default)]
    pub description: Option<String>,
    /// When set, `body` is a single control sequence such as `\N`; if the project does not define
    /// that macro, this body (e.g. `\mathbb{N}`) is expanded instead.
    #[serde(default)]
    pub if_undefined: Option<String>,
}

fn yes() -> bool {
    true
}

impl SnippetDef {
    /// The macro that `body` names when `if_undefined` is set, without the backslash
    /// (`\N` -> `N`). The app picks the body to expand from the project's macros.
    ///
    /// `None` when `if_undefined` is unset or `body` is not a single control word.
    pub fn required_macro(&self) -> Option<&str> {
        self.if_undefined.as_ref()?;
        control_word(&self.body)
    }
}

/// The name of `s` when `s` is exactly one control word (`\` followed by ASCII letters).
fn control_word(s: &str) -> Option<&str> {
    let name = s.strip_prefix('\\')?;
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphabetic())).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(body: &str, if_undefined: Option<&str>) -> SnippetDef {
        SnippetDef {
            trigger: "t".into(),
            body: body.into(),
            auto: false,
            context: Cond::Any,
            word_trig: true,
            line_begin: false,
            priority: 0,
            description: None,
            if_undefined: if_undefined.map(Into::into),
        }
    }

    #[test]
    fn required_macro_strips_the_backslash() {
        assert_eq!(def(r"\N", Some(r"\mathbb{N}")).required_macro(), Some("N"));
        assert_eq!(def(r"\Hom", Some("x")).required_macro(), Some("Hom"));
    }

    #[test]
    fn required_macro_needs_if_undefined_and_a_control_word() {
        assert_eq!(def(r"\N", None).required_macro(), None);
        for body in [r"\frac{$1}", r"\N ", "N", r"\", r"\,", r"\N\Z", r"\N1"] {
            assert_eq!(def(body, Some("x")).required_macro(), None, "{body:?}");
        }
    }

    #[test]
    fn serde_defaults() {
        let d: SnippetDef = toml::from_str("trigger = 'a'\nbody = 'b'").expect("valid");
        let expected = SnippetDef {
            trigger: "a".into(),
            ..def("b", None)
        };
        assert_eq!(d, expected);
    }

    #[test]
    fn context_is_lowercase() {
        let d: SnippetDef =
            toml::from_str("trigger = 'a'\nbody = 'b'\ncontext = 'math'").expect("valid");
        assert_eq!(d.context, Cond::Math);
        assert!(
            toml::from_str::<SnippetDef>("trigger = 'a'\nbody = 'b'\ncontext = 'Math'").is_err()
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(
            toml::from_str::<SnippetDef>("trigger = 'a'\nbody = 'b'\nwordtrig = false").is_err()
        );
    }
}
