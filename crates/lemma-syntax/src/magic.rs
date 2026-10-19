// Owner: WP1.9 structure
//! Magic comments: `% !TEX root`, `% !TEX program` and the subfiles parent.
//!
//! The rules follow these regexes, tested against real projects:
//! - root: `(?im)^[ \t]*%[ \t]*!\s*TeX\s+root\s*=\s*(?P<r>.+?)\s*$`
//! - program: `(?im)^[ \t]*%[ \t]*!\s*TeX\s+(?:TS-)?program\s*=\s*(?P<p>[A-Za-z]+)\s*$`
//! - subfiles: `(?m)^[^%\n]*?\\documentclass\s*\[(?P<parent>[^\]]+)\]\s*\{subfiles\}`
//!
//! Unlike the regexes, a magic comment must fit on one line.

use crate::structure::args::{at, brace, bracket, is_letter, skip_space};
use std::path::PathBuf;

/// Build hints found in a file's magic comments and preamble.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Magic {
    /// `% !TEX root = ...`, as written (relative to the file's directory).
    pub root: Option<PathBuf>,
    /// `% !TEX program = ...` (e.g. "lualatex"), lowercased.
    pub program: Option<String>,
    /// `\documentclass[<parent>]{subfiles}`, as written.
    pub subfiles_parent: Option<PathBuf>,
}

/// Reads the magic comments of `text`. The first occurrence of each wins.
pub fn parse_magic(text: &str) -> Magic {
    const DOCUMENTCLASS: &str = "\\documentclass";
    let mut magic = Magic::default();
    let mut class_seen = false;
    let mut offset = 0;
    for line in text.split('\n') {
        // A byte-order mark can only precede the first line.
        let trimmed = line.trim_start_matches(['\u{feff}', ' ', '\t']);
        if let Some(comment) = trimmed.strip_prefix('%') {
            read_magic_comment(comment, &mut magic);
        } else if !class_seen
            && let Some(p) = line.find(DOCUMENTCLASS)
            && !line[..p].contains('%')
        {
            class_seen = true;
            let after = offset + p + DOCUMENTCLASS.len();
            if !is_letter(at(text.as_bytes(), after)) {
                magic.subfiles_parent = subfiles_parent(text, after);
            }
        }
        offset += line.len() + 1;
    }
    magic
}

/// Applies one comment (the text after its `%`) to `magic` if it is `! TeX root|program = …`.
fn read_magic_comment(comment: &str, magic: &mut Magic) {
    let Some(rest) = comment.trim_start_matches([' ', '\t']).strip_prefix('!') else {
        return;
    };
    let Some(rest) = strip_prefix_ignore_case(rest.trim_start(), "tex") else {
        return;
    };
    let spaced = rest.trim_start();
    if spaced.len() == rest.len() {
        return;
    }
    if let Some(value) = strip_prefix_ignore_case(spaced, "root").and_then(assignment) {
        if magic.root.is_none() && !value.is_empty() {
            magic.root = Some(PathBuf::from(value));
        }
        return;
    }
    let program = strip_prefix_ignore_case(spaced, "ts-program")
        .or_else(|| strip_prefix_ignore_case(spaced, "program"))
        .and_then(assignment);
    if let Some(value) = program
        && magic.program.is_none()
        && !value.is_empty()
        && value.bytes().all(|c| c.is_ascii_alphabetic())
    {
        magic.program = Some(value.to_ascii_lowercase());
    }
}

/// `\s*=\s*value\s*` → `value`.
fn assignment(s: &str) -> Option<&str> {
    Some(s.trim_start().strip_prefix('=')?.trim())
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// The `[parent]` of `\documentclass[parent]{subfiles}`, reading from just after `\documentclass`.
fn subfiles_parent(text: &str, after: usize) -> Option<PathBuf> {
    let b = text.as_bytes();
    let (options, end) = bracket(b, skip_space(b, after))?;
    let (class, _) = brace(b, skip_space(b, end))?;
    let parent = text[options].trim();
    (text[class].trim() == "subfiles" && !parent.is_empty()).then(|| PathBuf::from(parent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_forms() {
        let mut m = Magic::default();
        read_magic_comment(" !TeXroot = main.tex", &mut m);
        assert_eq!(m.root, None, "TeX and root must be separated by whitespace");
        read_magic_comment(" ! TeX rooted = x", &mut m);
        assert_eq!(m.root, None);
        read_magic_comment(" !TeX  ROOT =  ../main.tex \r", &mut m);
        assert_eq!(m.root, Some(PathBuf::from("../main.tex")));
        read_magic_comment("!tex program = pdflatex -shell-escape", &mut m);
        assert_eq!(m.program, None);
        read_magic_comment("! TEX TS-program = XeLaTeX", &mut m);
        assert_eq!(m.program.as_deref(), Some("xelatex"));
    }
}
