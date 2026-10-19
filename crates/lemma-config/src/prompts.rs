//! Agent quick actions (QoL P1): prompt templates from `data/prompts.default.toml`, merged with
//! the user's `prompts.toml` (same directory as `config.toml`).
//!
//! ```toml
//! [[prompt]]
//! name = "Proofread"              # shown in menus; `:Agent proofread` finds it by its slug
//! key = "<Control><Alt>p"         # optional GTK accelerator that runs it
//! context = "selection"           # "selection" (default), "diagnostic" or "file"
//! needs_selection = true          # offered only while text is selected
//! prompt = '''
//! Proofread the following passage from {file} (from line {line}) …
//!
//! {selection}
//! '''
//! ```
//!
//! The prompt text may use `{selection}`, `{file}`, `{line}` and `{diagnostic}` ([`fill_prompt`]);
//! any other brace group (`\begin{align}`) is left alone. A user template with the name of a
//! default replaces it; one with an empty `prompt` removes it.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const DEFAULTS_TOML: &str = include_str!("../../../data/prompts.default.toml");

/// What a template works on, which decides what its variables hold (see [`PromptVars`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptContext {
    /// The editor's selection (or the current paragraph without one).
    #[default]
    Selection,
    /// A build diagnostic (the Problems list's "Ask agent to fix").
    Diagnostic,
    /// The whole active file.
    File,
}

/// One quick action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptTemplate {
    /// Unique (case-insensitively) display name, e.g. `"Fix this error"`.
    pub name: String,
    /// Optional GTK accelerator (`"<Control><Alt>p"`) that runs the action.
    #[serde(default)]
    pub key: Option<String>,
    /// The prompt text with `{selection}`, `{file}`, `{line}`, `{diagnostic}` placeholders.
    /// Empty in a user file: removes the default of the same name.
    pub prompt: String,
    /// Offered only while the editor has a selection.
    #[serde(default)]
    pub needs_selection: bool,
    #[serde(default)]
    pub context: PromptContext,
}

impl PromptTemplate {
    /// The name in lower case with every run of other characters than letters and digits
    /// turned into one `-` ("Fix this error" → `fix-this-error`), for `:Agent <name>`.
    pub fn slug(&self) -> String {
        slug(&self.name)
    }

    /// [`fill_prompt`] of this template's prompt.
    pub fn fill(&self, vars: &PromptVars<'_>) -> String {
        fill_prompt(&self.prompt, vars)
    }
}

fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptFile {
    #[serde(default, rename = "prompt")]
    prompts: Vec<PromptTemplate>,
}

/// Parses a prompts file (`[[prompt]]` tables). Names must be non-empty and unique (ignoring
/// case); the error names the problem and its location.
pub fn parse_prompts(src: &str) -> Result<Vec<PromptTemplate>, String> {
    let file: PromptFile = toml::from_str(src).map_err(|e| e.to_string())?;
    let mut seen = HashSet::new();
    for t in &file.prompts {
        if t.name.trim().is_empty() {
            return Err("a prompt has an empty name".into());
        }
        if !seen.insert(t.name.to_lowercase()) {
            return Err(format!("the prompt name {:?} is used twice", t.name));
        }
    }
    Ok(file.prompts)
}

/// The six default quick actions (`data/prompts.default.toml`): Fix this error, Explain,
/// Proofread, Make this proof more detailed, Convert to align, Translate to English.
pub fn default_prompts() -> Vec<PromptTemplate> {
    parse_prompts(DEFAULTS_TOML).expect("data/prompts.default.toml is valid")
}

/// The defaults with the user's templates applied: a user template replaces the default of the
/// same name (ignoring case) in place, others are appended in order, and templates with an empty
/// prompt are dropped (so an empty prompt removes a default).
pub fn merge_prompts(
    defaults: Vec<PromptTemplate>,
    user: Vec<PromptTemplate>,
) -> Vec<PromptTemplate> {
    let mut merged = defaults;
    for t in user {
        match merged
            .iter_mut()
            .find(|d| d.name.to_lowercase() == t.name.to_lowercase())
        {
            Some(slot) => *slot = t,
            None => merged.push(t),
        }
    }
    merged.retain(|t| !t.prompt.trim().is_empty());
    merged
}

/// The template named `query`: by name (ignoring case) or by [`PromptTemplate::slug`]
/// (`proofread`, `fix-this-error`).
pub fn find_prompt<'a>(templates: &'a [PromptTemplate], query: &str) -> Option<&'a PromptTemplate> {
    let query = query.trim();
    let lower = query.to_lowercase();
    let wanted = slug(query);
    templates
        .iter()
        .find(|t| t.name.to_lowercase() == lower)
        .or_else(|| {
            templates
                .iter()
                .find(|t| !wanted.is_empty() && t.slug() == wanted)
        })
}

/// The values of a template's placeholders.
///
/// | Placeholder | `selection` context | `diagnostic` context | `file` context |
/// |---|---|---|---|
/// | `{selection}` | the selected text (or the paragraph) | the lines around the error, numbered | empty (the file goes along as a resource) |
/// | `{file}` | path relative to the project root | the file of the diagnostic | the file |
/// | `{line}` | 1-based first line of the selection | the diagnostic's line | the cursor line |
/// | `{diagnostic}` | empty | the message (with TeX's context) | empty |
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptVars<'a> {
    pub selection: &'a str,
    pub file: &'a str,
    /// 1-based; `None` is written as `?`.
    pub line: Option<u32>,
    pub diagnostic: &'a str,
}

/// Replaces `{selection}`, `{file}`, `{line}` and `{diagnostic}` in `template` in one pass, so
/// placeholders inside the inserted values are not expanded again. Every other `{…}` is kept.
pub fn fill_prompt(template: &str, vars: &PromptVars<'_>) -> String {
    let line = vars.line.map_or_else(|| "?".to_owned(), |l| l.to_string());
    let mut out = String::with_capacity(template.len() + vars.selection.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let value = [
            ("{selection}", vars.selection),
            ("{file}", vars.file),
            ("{line}", line.as_str()),
            ("{diagnostic}", vars.diagnostic),
        ]
        .into_iter()
        .find(|(name, _)| tail.starts_with(name));
        match value {
            Some((name, value)) => {
                out.push_str(value);
                rest = &tail[name.len()..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_defaults() {
        let d = default_prompts();
        let names: Vec<&str> = d.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Fix this error",
                "Explain",
                "Proofread",
                "Make this proof more detailed",
                "Convert to align",
                "Translate to English"
            ]
        );
        let fix = &d[0];
        assert_eq!(fix.context, PromptContext::Diagnostic);
        assert!(!fix.needs_selection);
        for placeholder in ["{diagnostic}", "{file}", "{line}", "{selection}"] {
            assert!(fix.prompt.contains(placeholder), "{placeholder}");
        }
        for t in &d[1..] {
            assert_eq!(t.context, PromptContext::Selection, "{}", t.name);
            assert!(t.needs_selection, "{}", t.name);
            assert!(t.prompt.contains("{selection}"), "{}", t.name);
        }
        // Every action that changes text says how; Explain must not change anything.
        for t in d.iter().filter(|t| t.name != "Explain") {
            assert!(t.prompt.contains("mcp__lemma__edit"), "{}", t.name);
        }
        assert!(d[1].prompt.contains("Do not edit"));
        assert!(d[2].prompt.contains("keep every formula"));
    }

    #[test]
    fn fill_replaces_only_known_placeholders_once() {
        let vars = PromptVars {
            selection: "a {file} b \\begin{align}",
            file: "ch/one.tex",
            line: Some(12),
            diagnostic: "Undefined control sequence",
        };
        let out = fill_prompt(
            "{diagnostic} in {file}:{line}: {selection} {unknown} \\end{align} {",
            &vars,
        );
        assert_eq!(
            out,
            "Undefined control sequence in ch/one.tex:12: a {file} b \\begin{align} {unknown} \
             \\end{align} {"
        );
        assert_eq!(fill_prompt("line {line}", &PromptVars::default()), "line ?");
        assert_eq!(fill_prompt("", &vars), "");
        assert_eq!(fill_prompt("{{line}}", &vars), "{12}");
    }

    #[test]
    fn parse_errors() {
        assert!(parse_prompts("").unwrap().is_empty());
        let e = parse_prompts("[[prompt]]\nname = \"x\"\n").unwrap_err();
        assert!(e.contains("prompt"), "{e}");
        let e = parse_prompts("[[prompt]]\nname = \"x\"\nprompt = \"p\"\ncolor = 1\n").unwrap_err();
        assert!(e.contains("color"), "{e}");
        let e = parse_prompts(
            "[[prompt]]\nname = \"X\"\nprompt = \"p\"\n[[prompt]]\nname = \"x\"\nprompt = \"q\"\n",
        )
        .unwrap_err();
        assert!(e.contains("twice"), "{e}");
        let e = parse_prompts("[[prompt]]\nname = \" \"\nprompt = \"p\"\n").unwrap_err();
        assert!(e.contains("empty name"), "{e}");
        let e = parse_prompts("[[prompt]]\nname = \"x\"\nprompt = \"p\"\ncontext = \"page\"\n")
            .unwrap_err();
        assert!(e.contains("page") || e.contains("variant"), "{e}");
    }

    #[test]
    fn user_templates_replace_add_and_remove() {
        let user = parse_prompts(
            "[[prompt]]\nname = \"proofread\"\nprompt = \"Mine: {selection}\"\nkey = \"<Control>p\"\n\
             [[prompt]]\nname = \"Explain\"\nprompt = \"\"\n\
             [[prompt]]\nname = \"Shorten\"\nprompt = \"Shorten {selection}\"\nneeds_selection = true\n",
        )
        .unwrap();
        let merged = merge_prompts(default_prompts(), user);
        let names: Vec<&str> = merged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Fix this error",
                "proofread",
                "Make this proof more detailed",
                "Convert to align",
                "Translate to English",
                "Shorten"
            ]
        );
        assert_eq!(merged[1].prompt, "Mine: {selection}");
        assert_eq!(merged[1].key.as_deref(), Some("<Control>p"));
        assert_eq!(merged[1].context, PromptContext::Selection);
    }

    #[test]
    fn slugs_and_lookup() {
        let d = default_prompts();
        assert_eq!(d[0].slug(), "fix-this-error");
        assert_eq!(d[3].slug(), "make-this-proof-more-detailed");
        assert_eq!(slug("  Ünïcode -- name! "), "ünïcode-name");
        assert_eq!(find_prompt(&d, "proofread").unwrap().name, "Proofread");
        assert_eq!(find_prompt(&d, "PROOFREAD").unwrap().name, "Proofread");
        assert_eq!(
            find_prompt(&d, "fix this error").unwrap().name,
            "Fix this error"
        );
        assert_eq!(
            find_prompt(&d, "convert-to-align").unwrap().name,
            "Convert to align"
        );
        assert_eq!(
            find_prompt(&d, "Convert to   align!").unwrap().name,
            "Convert to align"
        );
        assert!(find_prompt(&d, "make the coffee").is_none());
        assert!(find_prompt(&d, "").is_none());
        assert!(find_prompt(&d, " -- ").is_none());
    }

    #[test]
    fn templates_round_trip_through_toml() {
        #[derive(Serialize)]
        struct Out<'a> {
            prompt: &'a [PromptTemplate],
        }
        let d = default_prompts();
        let text = toml::to_string(&Out { prompt: &d }).unwrap();
        assert_eq!(parse_prompts(&text).unwrap(), d);
    }
}
