//! The user's macro definitions outside the preamble (in the document body or in a chapter file),
//! copied into a preview's own preamble so formulas that use them render.
//!
//! The preamble's definitions are in the format already; these are the others: `\newcommand`
//! and its relatives, `\DeclareMathOperator`, `\DeclarePairedDelimiter`, the xparse commands,
//! `\newenvironment`, `\def` and `\let`. Comments and verbatim environments are skipped. Each
//! definition is copied as written, except that one that might already exist does not fail:
//! `\newcommand` becomes `\providecommand`, `\DeclareMathOperator{\X}{t}` becomes
//! `\providecommand{\X}{\operatorname{t}}` and `\NewDocumentCommand` becomes
//! `\ProvideDocumentCommand`. (A definition that fails anyway only logs an error before the
//! formula, which the preview ignores.)

/// The most text [`definitions`] returns.
pub const MAX_DEFINITIONS: usize = 64 * 1024;
/// Longer single definitions are skipped.
const MAX_DEFINITION: usize = 8 * 1024;

/// How a definition command's arguments look.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `{\name}` or `\name`, optional `[…]`s, `{body}` (`\newcommand`).
    Command,
    /// `{\name}{text}` (`\DeclareMathOperator`).
    Operator,
    /// `{\name}` and `n` more brace groups, optional `[…]`s allowed between them.
    NameAndGroups(u8),
    /// `{name}`, optional `[…]`s, then `n` brace groups (`\newenvironment`).
    EnvAndGroups(u8),
    /// `\name`, parameter text, `{body}` (`\def`).
    Def,
    /// `\name`, optional `=`, one token (`\let`).
    Let,
}

const COMMANDS: &[(&str, Shape)] = &[
    ("newcommand", Shape::Command),
    ("renewcommand", Shape::Command),
    ("providecommand", Shape::Command),
    ("DeclareRobustCommand", Shape::Command),
    ("DeclareMathOperator", Shape::Operator),
    ("DeclarePairedDelimiter", Shape::NameAndGroups(2)),
    ("DeclarePairedDelimiterX", Shape::NameAndGroups(3)),
    ("NewDocumentCommand", Shape::NameAndGroups(2)),
    ("RenewDocumentCommand", Shape::NameAndGroups(2)),
    ("ProvideDocumentCommand", Shape::NameAndGroups(2)),
    ("DeclareDocumentCommand", Shape::NameAndGroups(2)),
    ("newenvironment", Shape::EnvAndGroups(2)),
    ("renewenvironment", Shape::EnvAndGroups(2)),
    ("NewDocumentEnvironment", Shape::EnvAndGroups(3)),
    ("RenewDocumentEnvironment", Shape::EnvAndGroups(3)),
    ("def", Shape::Def),
    ("gdef", Shape::Def),
    ("edef", Shape::Def),
    ("xdef", Shape::Def),
    ("let", Shape::Let),
];

const VERBATIM_ENVS: &[&str] = &[
    "verbatim",
    "verbatim*",
    "Verbatim",
    "lstlisting",
    "minted",
    "comment",
];

/// The definitions in `text` (see the module docs), one per line, wrapped in
/// `\makeatletter`…`\makeatother` (as most definitions that use `@` are written); empty when
/// there are none. At most [`MAX_DEFINITIONS`] bytes.
pub fn definitions(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => i = line_end(b, i),
            b'\\' => {
                let (name, after) = control_word(text, i);
                if name.is_empty() {
                    i = (i + 2).min(b.len());
                    continue;
                }
                if name == "begin"
                    && let Some((env, body)) = group(text, skip_space(b, after))
                    && VERBATIM_ENVS.contains(&env.trim())
                {
                    let marker = format!("\\end{{{}}}", env.trim());
                    i = text[body..]
                        .find(&marker)
                        .map_or(b.len(), |k| body + k + marker.len());
                    continue;
                }
                match COMMANDS.iter().find(|(n, _)| *n == name) {
                    Some((name, shape)) => match parse(text, after, *shape) {
                        Some(end) => {
                            if end - i <= MAX_DEFINITION {
                                let def = rewrite(name, &text[after..end]);
                                if out.len() + def.len() + 1 > MAX_DEFINITIONS {
                                    break;
                                }
                                out.push_str(&def);
                                out.push('\n');
                            }
                            i = end;
                        }
                        None => i = after,
                    },
                    None => i = after,
                }
            }
            _ => i += 1,
        }
    }
    if out.is_empty() {
        return out;
    }
    format!("\\makeatletter\n{out}\\makeatother\n")
}

/// The definition `\name<args>` as it goes into the preview.
fn rewrite(name: &str, args: &str) -> String {
    match name {
        "newcommand" => format!("\\providecommand{args}"),
        "NewDocumentCommand" => format!("\\ProvideDocumentCommand{args}"),
        "DeclareMathOperator" => {
            let b = args.as_bytes();
            let mut i = 0;
            let star = b.first() == Some(&b'*');
            if star {
                i += 1;
            }
            let i = skip_space(b, i);
            let Some((cs, after)) = name_arg(args, i) else {
                return format!("\\DeclareMathOperator{args}");
            };
            let rest = args[after..].trim_start();
            let star = if star { "*" } else { "" };
            format!("\\providecommand{{{cs}}}{{\\operatorname{star}{rest}}}")
        }
        _ => format!("\\{name}{args}"),
    }
}

/// The end of the arguments of a definition command whose name ends at `i`, or `None` when they
/// do not look like `shape`.
fn parse(text: &str, i: usize, shape: Shape) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = i;
    if b.get(i) == Some(&b'*') && !matches!(shape, Shape::Def | Shape::Let) {
        i += 1;
    }
    i = skip_space(b, i);
    match shape {
        Shape::Command => {
            let (_, after) = name_arg(text, i)?;
            i = optionals(text, after);
            let (_, end) = group(text, i)?;
            Some(end)
        }
        Shape::Operator => {
            let (_, after) = name_arg(text, i)?;
            let (_, end) = group(text, skip_space(b, after))?;
            Some(end)
        }
        Shape::NameAndGroups(n) => {
            let (_, mut at) = name_arg(text, i)?;
            for _ in 0..n {
                at = optionals(text, at);
                let (_, end) = group(text, at)?;
                at = end;
            }
            Some(at)
        }
        Shape::EnvAndGroups(n) => {
            let (_, mut at) = group(text, i)?;
            for _ in 0..n {
                at = optionals(text, at);
                let (_, end) = group(text, at)?;
                at = end;
            }
            Some(at)
        }
        Shape::Def => {
            let (name, after) = control_word(text, i);
            if b.get(i) != Some(&b'\\') || (name.is_empty() && after <= i + 1) {
                return None;
            }
            // Parameter text: anything up to the body's brace (not across a blank line).
            let mut j = after;
            while j < b.len() && b[j] != b'{' {
                if b[j] == b'%' || (b[j] == b'\n' && b.get(j + 1) == Some(&b'\n')) {
                    return None;
                }
                j += 1;
            }
            let (_, end) = group(text, j)?;
            Some(end)
        }
        Shape::Let => {
            if b.get(i) != Some(&b'\\') {
                return None;
            }
            let (_, mut j) = control_word(text, i);
            j = skip_space(b, j);
            if b.get(j) == Some(&b'=') {
                j = skip_space(b, j + 1);
            }
            match b.get(j)? {
                b'\\' => {
                    let (_, end) = control_word(text, j);
                    Some(end.max(j + 2).min(b.len()))
                }
                b'\n' | b'%' => None,
                _ => Some(j + text[j..].chars().next()?.len_utf8()),
            }
        }
    }
}

/// A command name argument: `{\name}` or `\name` at `i`: the name with its backslash, and the end.
fn name_arg(text: &str, i: usize) -> Option<(&str, usize)> {
    let b = text.as_bytes();
    match b.get(i)? {
        b'{' => {
            let (inner, end) = group(text, i)?;
            let name = inner.trim();
            name.starts_with('\\').then_some((name, end))
        }
        b'\\' => {
            let (name, end) = control_word(text, i);
            let end = if name.is_empty() {
                (i + 2).min(b.len())
            } else {
                end
            };
            Some((&text[i..end], end))
        }
        _ => None,
    }
}

/// Skips `[…]` groups (and blanks) from `i`.
fn optionals(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    let mut i = skip_space(b, i);
    while b.get(i) == Some(&b'[') {
        let mut depth = 0u32;
        let mut j = i;
        let mut closed = None;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 1,
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                b']' if depth == 0 => {
                    closed = Some(j + 1);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        match closed {
            Some(end) => i = skip_space(b, end),
            None => return i,
        }
    }
    i
}

/// The brace group at `i` (blanks before it skipped): its inner text and the end. Braces escaped
/// with a backslash and comments do not count.
fn group(text: &str, i: usize) -> Option<(&str, usize)> {
    let b = text.as_bytes();
    let i = skip_space(b, i);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    let mut depth = 0u32;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'%' => {
                j = line_end(b, j);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&text[i + 1..j], j + 1));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The control word at the backslash `i`: its name (letters and `@`) and the end. An empty name
/// for a control symbol (`\\`, `\{`).
fn control_word(text: &str, i: usize) -> (&str, usize) {
    let b = text.as_bytes();
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'@') {
        j += 1;
    }
    (&text[i + 1..j], j)
}

/// Skips blanks and single newlines (and comments) from `i`.
fn skip_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'%' => i = line_end(b, i),
            _ => break,
        }
    }
    i
}

/// The index after the end of the line holding `i` (after its `\n`).
fn line_end(b: &[u8], i: usize) -> usize {
    memchr_newline(&b[i..]).map_or(b.len(), |k| i + k + 1)
}

fn memchr_newline(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(text: &str) -> Vec<String> {
        let all = definitions(text);
        if all.is_empty() {
            return Vec::new();
        }
        let inner = all
            .strip_prefix("\\makeatletter\n")
            .and_then(|s| s.strip_suffix("\\makeatother\n"))
            .expect("wrapped");
        inner.lines().map(str::to_owned).collect()
    }

    #[test]
    fn commands_and_operators() {
        let text = "\\newcommand{\\cF}{\\mathcal{F}}\nText \\renewcommand*\\phi{\\varphi} more.\n\
\\newcommand{\\pair}[2][x]{\\langle #1, #2\\rangle}\n\\DeclareMathOperator{\\Hom}{Hom}\n\
\\DeclareMathOperator*{\\colim}{colim}\n\\providecommand\\Z{\\mathbb{Z}}\n";
        assert_eq!(
            defs(text),
            [
                "\\providecommand{\\cF}{\\mathcal{F}}",
                "\\renewcommand*\\phi{\\varphi}",
                "\\providecommand{\\pair}[2][x]{\\langle #1, #2\\rangle}",
                "\\providecommand{\\Hom}{\\operatorname{Hom}}",
                "\\providecommand{\\colim}{\\operatorname*{colim}}",
                "\\providecommand\\Z{\\mathbb{Z}}",
            ]
        );
    }

    #[test]
    fn def_let_xparse_and_environments() {
        let text = "\\def\\norm#1{\\lVert #1\\rVert}\n\\let\\eps=\\varepsilon\n\\let\\to\\rightarrow\n\
\\NewDocumentCommand{\\abs}{m}{\\lvert #1\\rvert}\n\\DeclarePairedDelimiter{\\ceil}{\\lceil}{\\rceil}\n\
\\newenvironment{pf}[1][Proof]{\\textit{#1.}}{\\qed}\n";
        assert_eq!(
            defs(text),
            [
                "\\def\\norm#1{\\lVert #1\\rVert}",
                "\\let\\eps=\\varepsilon",
                "\\let\\to\\rightarrow",
                "\\ProvideDocumentCommand{\\abs}{m}{\\lvert #1\\rvert}",
                "\\DeclarePairedDelimiter{\\ceil}{\\lceil}{\\rceil}",
                "\\newenvironment{pf}[1][Proof]{\\textit{#1.}}{\\qed}",
            ]
        );
    }

    #[test]
    fn multi_line_bodies_keep_their_text() {
        let text = "\\newcommand{\\big}{%\n  \\left( % a comment with } brace\n  x\\right)}\nafter";
        assert_eq!(
            definitions(text),
            "\\makeatletter\n\\providecommand{\\big}{%\n  \\left( % a comment with } brace\n  \
             x\\right)}\n\\makeatother\n"
        );
    }

    #[test]
    fn comments_verbatim_and_lookalikes_are_skipped() {
        let text = "% \\newcommand{\\a}{A}\n\\verb|x| \\begin{verbatim}\n\\newcommand{\\b}{B}\n\
\\end{verbatim}\n\\newcommandx{\\c}{C} \\\\newcommand \\definecolor{x}\n\\newcommand{\\d}{D}";
        assert_eq!(defs(text), ["\\providecommand{\\d}{D}"]);
        assert_eq!(definitions("no definitions here"), "");
    }

    #[test]
    fn broken_definitions_are_skipped() {
        let text = "\\newcommand{\\a}{unclosed\n\\newcommand{\\b}{B}\n\\def x{y}\n\\let\n";
        // An unclosed body is skipped; the definitions after it are still found.
        assert_eq!(defs(text), ["\\providecommand{\\b}{B}"]);
        let text = "\\newcommand{a}{A}\n\\newcommand{\\b}{B}\n";
        assert_eq!(defs(text), ["\\providecommand{\\b}{B}"]);
    }

    #[test]
    fn the_output_is_capped() {
        let one = "\\newcommand{\\x}{y}\n";
        let text = one.repeat(MAX_DEFINITIONS / one.len() + 100);
        assert!(definitions(&text).len() <= MAX_DEFINITIONS + 40);
        let huge = format!(
            "\\newcommand{{\\h}}{{{}}}\n\\newcommand{{\\k}}{{K}}",
            "x".repeat(10_000)
        );
        assert_eq!(defs(&huge), ["\\providecommand{\\k}{K}"]);
    }

    #[test]
    fn utf8_is_kept() {
        let found = defs("\\let\\é=ö \\newcommand{\\R}{ℝ} \\def\\ü{x}");
        assert_eq!(
            found.last().map(String::as_str),
            Some("\\providecommand{\\R}{ℝ}")
        );
    }
}
