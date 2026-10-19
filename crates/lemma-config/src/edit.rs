//! Minimal in-place editing of a TOML config file's text.

/// A value [`set_value`] can write.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConfigValue<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    /// An array of strings (`math_envs`, `word_lists`, `preview.envs`).
    Strings(&'a [String]),
    /// An array of integers (`lint.disabled`).
    Ints(&'a [i64]),
}

impl ConfigValue<'_> {
    fn to_toml(self) -> toml::Value {
        match self {
            Self::Bool(b) => toml::Value::Boolean(b),
            Self::Int(i) => toml::Value::Integer(i),
            Self::Float(f) => toml::Value::Float(f),
            Self::Str(s) => toml::Value::String(s.to_owned()),
            Self::Strings(list) => toml::Value::Array(
                list.iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
            Self::Ints(list) => {
                toml::Value::Array(list.iter().map(|i| toml::Value::Integer(*i)).collect())
            }
        }
    }
}

/// Sets `key = "value"` in table `[section]` of the TOML text `src` and returns the new text.
///
/// Everything else (comments, blank lines, key order, other values) stays as written:
/// - an existing `key = …` line of the section is replaced (a trailing comment goes with it);
/// - otherwise the key is added after the section's last key;
/// - a missing section is appended at the end.
///
/// The value is written as a TOML basic string. Fails (and changes nothing) when the result would
/// not hold `section.key = value`: the text is not valid TOML, or the section is defined in a way
/// this line editor does not follow (dotted keys, an inline table, a multi-line value).
pub fn set_string(src: &str, section: &str, key: &str, value: &str) -> Result<String, String> {
    set_value(src, section, key, ConfigValue::Str(value))
}

/// [`set_string`] for any [`ConfigValue`] (Preferences writes booleans, numbers and lists this
/// way). An existing value spanning several lines (a multi-line array) is refused, as are the
/// other shapes [`set_string`] refuses.
pub fn set_value(
    src: &str,
    section: &str,
    key: &str,
    value: ConfigValue<'_>,
) -> Result<String, String> {
    let value = value.to_toml();
    let line = format!("{} = {}", toml_key(key), value);
    let lines: Vec<&str> = src.lines().collect();
    let found = find_key(&lines, section, key);
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    match found {
        Found::Key(i) => out[i] = line,
        Found::Section { last: i } => out.insert(i + 1, line),
        Found::Nothing => {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(format!("[{}]", toml_key(section)));
            out.push(line);
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    check(&text, section, key, Some(&value))?;
    Ok(text)
}

/// Removes `key` from table `[section]` of the TOML text `src` (it takes its default again) and
/// returns the new text; the text is returned unchanged when the key is not set. Comments and
/// the other lines stay as written. Fails like [`set_value`] when the key is set in a way this
/// line editor does not follow.
pub fn remove_key(src: &str, section: &str, key: &str) -> Result<String, String> {
    let lines: Vec<&str> = src.lines().collect();
    let Found::Key(i) = find_key(&lines, section, key) else {
        // Not on a line of its own: either unset (fine) or set some other way (refused).
        check(src, section, key, None)?;
        return Ok(src.to_owned());
    };
    let mut out: Vec<&str> = lines.clone();
    out.remove(i);
    let mut text = out.join("\n");
    text.push('\n');
    check(&text, section, key, None)?;
    Ok(text)
}

/// Where a key is in a config file's lines.
enum Found {
    /// The line of `key = …` in the section.
    Key(usize),
    /// The section exists without the key; `last` is its header or last key line.
    Section {
        last: usize,
    },
    Nothing,
}

fn find_key(lines: &[&str], section: &str, key: &str) -> Found {
    let mut in_section = false;
    let mut section_found = false;
    // Index of the existing key line, and the line after which a new key goes.
    let mut existing = None;
    let mut insert_after = None;
    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        if let Some(name) = table_header(trimmed) {
            in_section = name == section;
            if in_section {
                section_found = true;
                insert_after = Some(i);
            }
            continue;
        }
        if !in_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        insert_after = Some(i);
        if line_key(trimmed).is_some_and(|k| k == key) {
            existing = Some(i);
        }
    }
    match (existing, insert_after) {
        (Some(i), _) => Found::Key(i),
        (None, Some(last)) if section_found => Found::Section { last },
        _ => Found::Nothing,
    }
}

/// Verifies that `text` parses and holds `section.key = value` (`None`: the key is not set).
fn check(text: &str, section: &str, key: &str, value: Option<&toml::Value>) -> Result<(), String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let found = table.get(section).and_then(|s| s.get(key));
    if found == value {
        Ok(())
    } else if value.is_some() {
        Err(format!("could not set {section}.{key} in this file"))
    } else {
        Err(format!("could not remove {section}.{key} from this file"))
    }
}

/// The table name of a `[name]` header line (not of an `[[array]]` header).
fn table_header(line: &str) -> Option<String> {
    let line = strip_comment(line).trim();
    if line.starts_with("[[") {
        return Some(String::new());
    }
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    Some(unquote(inner))
}

/// The key of a `key = value` line.
fn line_key(line: &str) -> Option<String> {
    let (key, _) = line.split_once('=')?;
    Some(unquote(key.trim()))
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
        .to_owned()
}

/// `line` without a `#` comment outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '#') => return &line[..i],
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    line
}

/// A bare key when possible, else a quoted one.
fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_owned()
    } else {
        toml::Value::String(key.to_owned()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_key_and_keeps_everything_else() {
        let src = "# my settings\n[editor]\nvim = false # no vim\n\n[agent]\n# detected\nnode = \"/old/node\"  # auto\nclaude = \"/c\"\n";
        let out = set_string(src, "agent", "node", "/new/node").unwrap();
        assert_eq!(
            out,
            "# my settings\n[editor]\nvim = false # no vim\n\n[agent]\n# detected\nnode = \"/new/node\"\nclaude = \"/c\"\n"
        );
    }

    #[test]
    fn adds_a_key_after_the_sections_last_key() {
        let src = "[agent]\nmodel = \"opus\"\n\n# build things\n[build]\non_save = false\n";
        let out = set_string(src, "agent", "claude", "/bin/claude").unwrap();
        assert_eq!(
            out,
            "[agent]\nmodel = \"opus\"\nclaude = \"/bin/claude\"\n\n# build things\n[build]\non_save = false\n"
        );
        let out = set_string("[agent]\n", "agent", "node", "/n").unwrap();
        assert_eq!(out, "[agent]\nnode = \"/n\"\n");
    }

    #[test]
    fn appends_a_missing_section() {
        assert_eq!(
            set_string("", "agent", "node", "/n").unwrap(),
            "[agent]\nnode = \"/n\"\n"
        );
        assert_eq!(
            set_string("[editor]\nvim = true", "agent", "node", "/n").unwrap(),
            "[editor]\nvim = true\n\n[agent]\nnode = \"/n\"\n"
        );
    }

    #[test]
    fn escapes_values_and_ignores_other_sections() {
        let src = "[spell]\nnode = \"x\"\n[agent]\n";
        let out = set_string(src, "agent", "node", "C:\\path \"q\"").unwrap();
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(parsed["agent"]["node"].as_str(), Some("C:\\path \"q\""));
        assert_eq!(parsed["spell"]["node"].as_str(), Some("x"));
        let out = set_string(
            "[keys]\n\"win.save\" = \"<Control>s\"\n",
            "keys",
            "win.save",
            "",
        )
        .unwrap();
        assert_eq!(out, "[keys]\n\"win.save\" = \"\"\n");
        assert_eq!(crate::parse(&out).unwrap().keys.bindings["win.save"], "");
    }

    #[test]
    fn writes_booleans_numbers_and_lists() {
        let src = "# prefs\n[editor]\nvim = true # on\nfont = \"Mono 11\"\n\n[pdf]\n";
        let out = set_value(src, "editor", "vim", ConfigValue::Bool(false)).unwrap();
        assert_eq!(
            out,
            "# prefs\n[editor]\nvim = false\nfont = \"Mono 11\"\n\n[pdf]\n"
        );
        let out = set_value(&out, "preview", "delay_ms", ConfigValue::Int(450)).unwrap();
        assert!(out.ends_with("\n[preview]\ndelay_ms = 450\n"), "{out}");
        let envs = ["tikzcd".to_owned(), "tikz picture".to_owned()];
        let out = set_value(&out, "preview", "envs", ConfigValue::Strings(&envs)).unwrap();
        let out = set_value(&out, "lint", "disabled", ConfigValue::Ints(&[1, 8])).unwrap();
        let out = set_value(&out, "pdf", "thumbnails", ConfigValue::Bool(true)).unwrap();
        let c = crate::parse(&out).unwrap();
        assert!(!c.editor.vim);
        assert_eq!(c.editor.font, "Mono 11");
        assert_eq!(c.preview.delay_ms, 450);
        assert_eq!(c.preview.envs, envs);
        assert_eq!(c.lint.disabled, [1, 8]);
        assert!(c.pdf.thumbnails);
        assert!(out.starts_with("# prefs\n"), "comments stay: {out}");
        let out = set_value("", "x", "f", ConfigValue::Float(1.5)).unwrap();
        assert_eq!(out, "[x]\nf = 1.5\n");
        // A multi-line array cannot be replaced line by line.
        let src = "[editor]\nmath_envs = [\n  \"a\",\n]\n";
        assert!(set_value(src, "editor", "math_envs", ConfigValue::Strings(&[])).is_err());
    }

    #[test]
    fn removes_a_key_and_keeps_everything_else() {
        let src = "# top\n[editor]\nvim = true # mine\nwrap = false\n\n[pdf]\nthumbnails = true\n";
        let out = remove_key(src, "editor", "vim").unwrap();
        assert_eq!(
            out,
            "# top\n[editor]\nwrap = false\n\n[pdf]\nthumbnails = true\n"
        );
        assert!(
            !crate::parse(&out).unwrap().editor.vim,
            "back to the default (off since M5)"
        );
        assert_eq!(remove_key(&out, "editor", "vim").unwrap(), out, "unset");
        assert_eq!(remove_key("", "editor", "vim").unwrap(), "");
        // Set through an inline table: not this editor's business.
        assert!(remove_key("editor = { vim = false }\n", "editor", "vim").is_err());
        assert!(remove_key("[editor\n", "editor", "vim").is_err());
    }

    #[test]
    fn refuses_what_it_cannot_edit() {
        assert!(set_string("[agent\n", "agent", "node", "/n").is_err());
        // An inline table defines the section without a header.
        assert!(set_string("agent = { node = \"/x\" }\n", "agent", "node", "/n").is_err());
        // A table header inside an array of tables is not the section.
        let src = "[[prompt]]\nname = \"a\"\n";
        let out = set_string(src, "agent", "node", "/n").unwrap();
        assert!(out.starts_with(src), "{out}");
    }
}
