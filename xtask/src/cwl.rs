//! A parser for TeXstudio `.cwl` completion word lists.
//!
//! Only what the completer needs is read:
//!
//! - `#include:name` dependencies;
//! - command lines `\name<args>#classifiers` and environment lines `\begin{name}<args>#classifiers`
//!   (`\end{…}` lines are skipped: the environment line describes both);
//! - the classifiers: `*` unusual, `S` hidden, `m` math only, `n` text only, `L0`–`L6`
//!   sectioning, `\math` (an environment that starts math), and the argument classifiers
//!   `r` (reference), `c`/`C` (citation), `l` (label), `i`/`I`/`g`/`b` (file) and `u` (package).
//!
//! `#keyvals:` … `#endkeyvals` and `#ifOption:` … `#endif` blocks, `#repl:` lines, comments and
//! anything that is not a command line (a stray `404: Not Found`, say) are skipped.

/// What a classified argument holds. Mirrors `lemma_syntax::ArgKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArgClass {
    Ref,
    Cite,
    Label,
    File,
    Package,
}

impl ArgClass {
    /// The `lemma_syntax::ArgKind` variant name.
    pub fn variant(self) -> &'static str {
        match self {
            ArgClass::Ref => "Ref",
            ArgClass::Cite => "Cite",
            ArgClass::Label => "Label",
            ArgClass::File => "File",
            ArgClass::Package => "Package",
        }
    }
}

/// The classifiers after `#`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Classifiers {
    /// `*`: shown only after the usual commands.
    pub unusual: bool,
    /// `S`: never offered (syntax checking only).
    pub hidden: bool,
    /// `m`: valid only in math.
    pub math: bool,
    /// `n`: valid only in text.
    pub text: bool,
    /// `L0`–`L6`: a sectioning command.
    pub sectioning: bool,
    /// `\math` among the environment aliases: the environment starts math mode.
    pub env_math: bool,
    /// The argument classifier, if any.
    pub arg: Option<ArgClass>,
}

/// Whether an entry is a command or an environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Command,
    Environment,
}

/// One command or environment line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// The command name without the backslash (`frac`, `section*`) or the environment name.
    pub name: String,
    /// Everything after the name (after `}` for environments), before the classifiers.
    pub rest: String,
    pub cls: Classifiers,
}

/// A parsed `.cwl` file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CwlFile {
    /// `#include:` targets in file order, without duplicates.
    pub includes: Vec<String>,
    /// Command and environment lines in file order.
    pub entries: Vec<Entry>,
}

/// Parses a `.cwl` file. Never fails: lines it does not understand are skipped.
pub fn parse(src: &str) -> CwlFile {
    let mut out = CwlFile::default();
    let mut in_keyvals = false;
    let mut if_depth = 0usize;
    for raw in src.lines() {
        let line = raw.trim_end();
        if in_keyvals {
            in_keyvals = !line.starts_with("#endkeyvals");
            continue;
        }
        if let Some(directive) = line.strip_prefix('#') {
            if directive.starts_with("keyvals:") {
                in_keyvals = true;
            } else if directive.starts_with("ifOption:") {
                if_depth += 1;
            } else if directive.starts_with("endif") {
                if_depth = if_depth.saturating_sub(1);
            } else if let Some(name) = directive.strip_prefix("include:") {
                let name = name.trim();
                if if_depth == 0 && !name.is_empty() && !out.includes.iter().any(|i| i == name) {
                    out.includes.push(name.to_owned());
                }
            }
            continue;
        }
        if if_depth > 0 {
            continue;
        }
        if let Some(entry) = parse_entry(line) {
            out.entries.push(entry);
        }
    }
    out
}

/// Parses one command or environment line; `None` for anything else.
fn parse_entry(line: &str) -> Option<Entry> {
    let body = line.strip_prefix('\\')?;
    let (code, cls) = split_classifiers(body);
    // `\begin{%<env%>}` (a placeholder for the name) is the command `\begin` itself; likewise
    // for `\end`.
    for cmd in ["begin", "end"] {
        if let Some(arg) = code.strip_prefix(cmd).filter(|a| a.starts_with("{%<")) {
            return Some(Entry {
                kind: EntryKind::Command,
                name: cmd.to_owned(),
                rest: arg.to_owned(),
                cls: parse_classifiers(cls),
            });
        }
    }
    if let Some(after) = code.strip_prefix("begin{") {
        let close = after.find('}')?;
        let name = &after[..close];
        if name.is_empty() || name.contains(['%', '\\', '{']) {
            return None;
        }
        return Some(Entry {
            kind: EntryKind::Environment,
            name: name.to_owned(),
            rest: after[close + 1..].to_owned(),
            cls: parse_classifiers(cls),
        });
    }
    if code.starts_with("end{") {
        return None;
    }
    let letters = code
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic() || *b == b'@')
        .count();
    if letters == 0 {
        // Control symbols (`\,`, `\\`) are never completed after a backslash and a letter.
        return None;
    }
    let star = usize::from(code.as_bytes().get(letters) == Some(&b'*'));
    let name = &code[..letters + star];
    Some(Entry {
        kind: EntryKind::Command,
        name: name.to_owned(),
        rest: code[letters + star..].to_owned(),
        cls: parse_classifiers(cls),
    })
}

/// Splits `body` (a line without its leading backslash) at the first `#` outside braces that is
/// not escaped: `(code, classifiers)`.
fn split_classifiers(body: &str) -> (&str, &str) {
    let b = body.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'#' if depth <= 0 && i > 0 => return (&body[..i], &body[i + 1..]),
            _ => {}
        }
        i += 1;
    }
    (body, "")
}

/// Parses the classifier text after `#` (several segments may follow, separated by `#`).
pub fn parse_classifiers(s: &str) -> Classifiers {
    let mut c = Classifiers::default();
    for seg in s.split('#') {
        // `beginEnv*`, `endEnv*` and similar words are TeXstudio internals, not flag letters.
        let letters = seg
            .bytes()
            .take_while(|b| b.is_ascii_alphabetic() || *b == b'*')
            .count();
        if letters >= 5 {
            continue;
        }
        let bytes = seg.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    c.env_math |= seg[i + 1..].split(',').any(|alias| alias == "math");
                    break;
                }
                b'/' | b'%' => break,
                b'*' => c.unusual = true,
                b'S' => c.hidden = true,
                b'm' => c.math = true,
                b'n' => c.text = true,
                b'L' => {
                    if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                        c.sectioning = true;
                        i += 1;
                    }
                }
                b'r' => c.arg = c.arg.or(Some(ArgClass::Ref)),
                b'c' | b'C' => c.arg = c.arg.or(Some(ArgClass::Cite)),
                b'l' => c.arg = c.arg.or(Some(ArgClass::Label)),
                b'i' | b'I' | b'g' | b'b' => c.arg = c.arg.or(Some(ArgClass::File)),
                b'u' => c.arg = c.arg.or(Some(ArgClass::Package)),
                _ => {}
            }
            i += 1;
        }
    }
    c
}

/// The argument signature of an entry, converted to a lemma snippet body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    /// Lemma snippet body syntax (`{$1}{$2}`): every placeholder becomes an empty tab stop, `%|`
    /// becomes `$0`, and a literal `$` is written `${$}`.
    pub snippet: String,
    /// The placeholders for display (`{num}{den}`), empty when there are none.
    pub display: String,
    /// Optional arguments (`[…]`).
    pub optional: usize,
    /// Mandatory arguments (`{…}`).
    pub mandatory: usize,
    /// Picture coordinates `(…)` and beamer overlays `<…>`: variants for special uses.
    pub special: usize,
    /// Literal text such as the `\item` of `\begin{itemize}\item`.
    pub has_literal: bool,
    /// The argument class implied by a placeholder (`%ref`, `%labeldef`, `imagefile`).
    pub arg: Option<ArgClass>,
}

/// Tab stops are single digits in lemma snippet bodies.
const MAX_STOP: usize = 9;

/// Converts the text after a command or environment name (`{%<num%>}{den}`, `[root]{arg}`,
/// `(%|\Bigg)`) into a [`Signature`].
pub fn signature(rest: &str) -> Signature {
    let mut sig = Signature::default();
    let mut next_stop = 1usize;
    let mut i = 0;
    let b = rest.as_bytes();
    while i < b.len() {
        if rest[i..].starts_with("%|") {
            sig.snippet.push_str("$0");
            i += 2;
            continue;
        }
        if rest[i..].starts_with("%<")
            && let Some(len) = rest[i + 2..].find("%>")
        {
            let name = placeholder_name(&rest[i + 2..i + 2 + len]);
            push_stop(&mut sig.snippet, &mut next_stop, "", "");
            sig.display.push_str(&name);
            i += 2 + len + 2;
            continue;
        }
        let c = b[i];
        if let Some(close) = closing(c)
            && let Some(end) = matching(rest, i, c, close)
        {
            let content = &rest[i + 1..end];
            let open_s = &rest[i..i + 1];
            let close_s = &rest[end..end + 1];
            if content.contains('\\') || content.contains("%|") {
                // Literal code such as `\bigg(%|\bigg)`.
                sig.snippet.push_str(open_s);
                sig.snippet.push_str(&literal_snippet(content));
                sig.snippet.push_str(close_s);
                sig.has_literal = true;
            } else {
                let name = placeholder_name(content);
                if sig.arg.is_none() {
                    sig.arg = placeholder_class(content);
                }
                push_stop(&mut sig.snippet, &mut next_stop, open_s, close_s);
                sig.display.push_str(open_s);
                sig.display.push_str(&name);
                sig.display.push_str(close_s);
                match c {
                    b'[' => sig.optional += 1,
                    b'{' => sig.mandatory += 1,
                    _ => sig.special += 1,
                }
            }
            i = end + 1;
            continue;
        }
        let ch = rest[i..].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            sig.has_literal = true;
        }
        push_literal(&mut sig.snippet, ch);
        i += ch.len_utf8();
    }
    sig
}

fn closing(open: u8) -> Option<u8> {
    match open {
        b'{' => Some(b'}'),
        b'[' => Some(b']'),
        b'(' => Some(b')'),
        b'<' => Some(b'>'),
        _ => None,
    }
}

/// The index of the bracket closing the one at `start`, honouring nesting of the same pair.
fn matching(s: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut i = start;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Appends a tab stop wrapped in `open`/`close`, or an empty pair once the stops run out.
fn push_stop(out: &mut String, next: &mut usize, open: &str, close: &str) {
    out.push_str(open);
    if *next <= MAX_STOP {
        out.push('$');
        out.push_str(&next.to_string());
        *next += 1;
    }
    out.push_str(close);
}

fn push_literal(out: &mut String, c: char) {
    if c == '$' {
        out.push_str("${$}");
    } else {
        out.push(c);
    }
}

/// Literal code: `%|` becomes `$0`, `%<…%>` placeholders become `$0` too (one exit point), and
/// `$` is escaped.
fn literal_snippet(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("%|") {
            out.push_str("$0");
            rest = r;
        } else if rest.starts_with("%<")
            && let Some(len) = rest[2..].find("%>")
        {
            out.push_str("$0");
            rest = &rest[2 + len + 2..];
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            push_literal(&mut out, c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The display name of a placeholder: `%<…%>` markers and `%suffix` qualifiers removed.
fn placeholder_name(content: &str) -> String {
    let s = content.replace("%<", "").replace("%>", "");
    let s = s.split('%').next().unwrap_or("").trim();
    s.to_owned()
}

/// The argument class a placeholder implies.
fn placeholder_class(content: &str) -> Option<ArgClass> {
    let suffix = content.rsplit_once('%').map(|(_, s)| s);
    match suffix {
        Some("ref") => Some(ArgClass::Ref),
        Some("labeldef") => Some(ArgClass::Label),
        _ if content.trim() == "imagefile" => Some(ArgClass::File),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_environments_and_includes() {
        let f = parse(
            "# comment\n#include:amstext\n#include:amstext\n\\frac{%<num%:translatable%>}{%<den%>}#m\n\
             \\begin{align}#\\math,array\n\\end{align}\n\\section*{title}#L2\n\\eqref{label}#r\n",
        );
        assert_eq!(f.includes, ["amstext"]);
        assert_eq!(f.entries.len(), 4);
        let frac = &f.entries[0];
        assert_eq!(frac.kind, EntryKind::Command);
        assert_eq!(frac.name, "frac");
        assert!(frac.cls.math);
        let align = &f.entries[1];
        assert_eq!(align.kind, EntryKind::Environment);
        assert_eq!(align.name, "align");
        assert!(align.cls.env_math && !align.cls.math);
        assert_eq!(f.entries[2].name, "section*");
        assert!(f.entries[2].cls.sectioning);
        assert_eq!(f.entries[3].cls.arg, Some(ArgClass::Ref));
    }

    #[test]
    fn placeholder_environment_is_the_begin_command() {
        let f = parse("\\begin{%<env%>}\n\\end{%<env%>}\n\\end{align}\n");
        let got: Vec<_> = f
            .entries
            .iter()
            .map(|e| (e.kind, e.name.as_str(), e.rest.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (EntryKind::Command, "begin", "{%<env%>}"),
                (EntryKind::Command, "end", "{%<env%>}"),
            ]
        );
        assert_eq!(signature("{%<env%>}").snippet, "{$1}");
    }

    #[test]
    fn skips_blocks_and_garbage() {
        let f = parse(
            "404: Not Found\n#keyvals:\\usepackage/x#c\n\\notacommand\n#endkeyvals\n\
             #ifOption:foo\n\\optional\n#include:hidden\n#endif\n#repl:\"a ä\n\
             %<%:TEXSTUDIO-GENERIC-ENVIRONMENT-TEMPLATE%>\n\\,\n\\kept\n",
        );
        assert!(f.includes.is_empty());
        let names: Vec<_> = f.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
    }

    #[test]
    fn classifier_segments() {
        let c = parse_classifiers("*mM");
        assert!(c.unusual && c.math && !c.text);
        let c = parse_classifiers("m\\array");
        assert!(c.math && !c.env_math);
        let c = parse_classifiers("s#%color");
        assert_eq!(c, Classifiers::default());
        let c = parse_classifiers("beginEnv*#%expl3");
        assert_eq!(c, Classifiers::default());
        let c = parse_classifiers("*/%unit");
        assert!(c.unusual && !c.math);
        let c = parse_classifiers("SC");
        assert!(c.hidden);
        assert_eq!(c.arg, Some(ArgClass::Cite));
        let c = parse_classifiers("*L");
        assert!(c.unusual && !c.sectioning);
    }

    #[test]
    fn classifier_split_ignores_escaped_hash_and_braces() {
        assert_eq!(split_classifiers("cmd{a#b}#m"), ("cmd{a#b}", "m"));
        assert_eq!(split_classifiers("left[#m"), ("left[", "m"));
        assert_eq!(split_classifiers("plain"), ("plain", ""));
    }

    #[test]
    fn signatures() {
        let s = signature("{%<num%:translatable%>}{%<den%:translatable%>}");
        assert_eq!(s.snippet, "{$1}{$2}");
        assert_eq!(s.display, "{num}{den}");
        assert_eq!((s.optional, s.mandatory), (0, 2));
        let s = signature("[root]{arg}");
        assert_eq!(s.snippet, "[$1]{$2}");
        assert_eq!((s.optional, s.mandatory, s.special), (1, 1, 0));
        let s = signature("(xdimen,ydimen){text}");
        assert_eq!(s.snippet, "($1){$2}");
        assert_eq!((s.optional, s.mandatory, s.special), (0, 1, 1));
        let s = signature("(%|\\Bigg)");
        assert_eq!(s.snippet, "($0\\Bigg)");
        assert!(s.has_literal);
        let s = signature(" %|");
        assert_eq!(s.snippet, " $0");
        assert!(!s.has_literal);
        let s = signature("[%<label%>] %<description%>");
        assert_eq!(s.snippet, "[$1] $2");
        let s = signature("{key%labeldef}");
        assert_eq!(s.arg, Some(ArgClass::Label));
        assert_eq!(s.display, "{key}");
        let s = signature("\\item");
        assert_eq!(s.snippet, "\\item");
        assert!(s.has_literal);
        let s = signature("$");
        assert_eq!(s.snippet, "${$}");
        let s = signature("{a}{b}{c}{d}{e}{f}{g}{h}{i}{j}");
        assert_eq!(s.snippet, "{$1}{$2}{$3}{$4}{$5}{$6}{$7}{$8}{$9}{}");
        let s = signature("(");
        assert_eq!(s.snippet, "(");
    }
}
