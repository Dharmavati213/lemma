//! The TeX sources of a preview: the root's preamble (what the format is built from) and the
//! file each preview compiles.
//!
//! A preview file typesets the formula into box 0 and ships it out as a page of its own size
//! (plus a margin for anything that sticks out), so the page holds nothing but the formula:
//!
//! ```text
//! \endofdump                      (format route: ends mylatexformat's preamble skipping)
//! \nofiles                        (no .aux, .out, .toc …)
//! \makeatletter … \makeatother    (the definitions from outside the preamble)
//! \begin{document}
//! …                               (equation numbers off, the shipout macro)
//! \setbox0=\vbox{\hsize=\linewidth\noindent      or   \setbox0=\hbox{%
//! <the formula, as written>
//! \par}                                               }%
//! \lemmapreviewship
//! \end{document}
//! ```
//!
//! The full route (no format) starts with the whole preamble instead of `\endofdump`. Errors on
//! lines from [`Body::formula_line`] on are the formula's; earlier ones (a definition that
//! already exists, a package hook) are not shown.

use crate::formula::FormulaKind;

/// The preamble used when the document has no root (a chapter opened on its own).
pub const DEFAULT_PREAMBLE: &str = "\\documentclass{article}
\\usepackage{amsmath,amssymb}
\\IfFileExists{mathtools.sty}{\\usepackage{mathtools}}{}
\\IfFileExists{tikz-cd.sty}{\\usepackage{tikz-cd}}{}
";

/// How a preview is compiled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `pdflatex -fmt` with the preamble's format.
    Format,
    /// The whole preamble compiled every time (no format: fontspec, LuaTeX, XeTeX, or a
    /// preamble mylatexformat cannot dump).
    Full,
}

/// A preview's source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub text: String,
    /// The 1-based line of the file where the formula starts.
    pub formula_line: u32,
}

/// The preamble of a complete document: the text before its first `\begin{document}` outside a
/// comment, and the text after it (the body). `None` when there is no `\begin{document}`.
pub fn split_preamble(text: &str) -> Option<(&str, &str)> {
    const BEGIN: &str = "\\begin{document}";
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let code = &line[..comment_start(line)];
        if let Some(k) = code.find(BEGIN) {
            let at = offset + k;
            return Some((&text[..at], &text[at + BEGIN.len()..]));
        }
        offset += line.len();
    }
    None
}

/// The byte where `line`'s comment starts (an unescaped `%`), or its length.
fn comment_start(line: &str) -> usize {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'%' => return i,
            _ => i += 1,
        }
    }
    line.len()
}

/// The file mylatexformat builds the format from: the preamble and an empty document.
pub fn format_source(preamble: &str) -> String {
    let mut s = String::with_capacity(preamble.len() + 40);
    s.push_str(preamble);
    if !preamble.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("\\begin{document}\n\\end{document}\n");
    s
}

/// The preview file for `formula` (of kind `kind`), with `definitions` (from
/// [`crate::defs::definitions`]) before `\begin{document}`. `preamble` is the whole preamble for
/// [`Route::Full`] and ignored for [`Route::Format`].
pub fn body(
    route: Route,
    preamble: &str,
    definitions: &str,
    kind: &FormulaKind,
    formula: &str,
) -> Body {
    let mut s = String::with_capacity(preamble.len() + definitions.len() + formula.len() + 1024);
    match route {
        Route::Format => s.push_str("\\endofdump\n"),
        Route::Full => {
            s.push_str(preamble);
            if !preamble.ends_with('\n') {
                s.push('\n');
            }
        }
    }
    s.push_str("\\nofiles\n");
    s.push_str(definitions);
    if !definitions.is_empty() && !definitions.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(SETUP);
    let open = if kind.is_block() {
        "\\setbox0=\\vbox{\\hsize=\\linewidth\\noindent\n"
    } else {
        "\\setbox0=\\hbox{%\n"
    };
    s.push_str(open);
    let formula_line = s.matches('\n').count() as u32 + 1;
    // A `%` right after the closing delimiter: an inline formula gets no space after it.
    s.push_str(formula.trim_end_matches(['\n', '\r']));
    s.push_str(if kind.is_block() {
        "\n\\par}%\n"
    } else {
        "%\n}%\n"
    });
    s.push_str("\\lemmapreviewship\n\\end{document}\n");
    Body {
        text: s,
        formula_line,
    }
}

/// After `\begin{document}` (packages redefine these in their begin-document hooks): no equation
/// numbers or tags (the preview's would be wrong), labels ignored (also cleveref's
/// `\label[type]{…}`; amsmath complains about several in one row of a starred environment), and
/// the shipout of box 0 on a page of its size plus a margin (1 in around, ½ in above and below),
/// with TeX's origin at the corner.
const SETUP: &str = "\\begin{document}%
\\makeatletter
\\let\\@eqnnum\\@empty
\\def\\tagform@#1{}%
\\def\\lemmapreview@label{\\@ifnextchar[\\lemmapreview@labelopt\\@gobble}%
\\def\\lemmapreview@labelopt[#1]#2{}%
\\let\\label\\lemmapreview@label
\\let\\label@in@display\\lemmapreview@label
\\def\\lemmapreviewship{%
  \\hoffset=-1in \\voffset=-1in
  \\dimen0=\\dimexpr\\wd0+2in\\relax
  \\dimen2=\\dimexpr\\ht0+\\dp0+1in\\relax
  \\ifdefined\\pdfpagewidth \\pdfpagewidth=\\dimen0 \\pdfpageheight=\\dimen2
  \\else \\pagewidth=\\dimen0 \\pageheight=\\dimen2 \\fi
  \\shipout\\vbox{\\kern.5in\\hbox{\\kern1in\\box0}}}%
\\makeatother
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preambles_split_at_the_first_real_begin_document() {
        let text = "\\documentclass{article}\n% \\begin{document} in a comment\n\\usepackage{x}\n\
\\begin{document}\nHello\n\\begin{document}\n";
        let (pre, body) = split_preamble(text).unwrap();
        assert!(pre.ends_with("\\usepackage{x}\n"), "{pre:?}");
        assert_eq!(body, "\nHello\n\\begin{document}\n");
        assert_eq!(
            split_preamble("a 50\\% \\begin{document}x").map(|p| p.0),
            Some("a 50\\% ")
        );
        assert_eq!(split_preamble("\\section{x}\n$a$\n"), None);
        assert_eq!(split_preamble("x % y \\begin{document}"), None);
    }

    #[test]
    fn format_bodies_start_with_endofdump() {
        let b = body(Route::Format, "ignored", "", &FormulaKind::Inline, "$x$");
        assert!(
            b.text
                .starts_with("\\endofdump\n\\nofiles\n\\begin{document}%\n")
        );
        assert!(!b.text.contains("ignored"));
        let lines: Vec<&str> = b.text.lines().collect();
        assert_eq!(lines[b.formula_line as usize - 1], "$x$%");
        assert_eq!(lines[b.formula_line as usize - 2], "\\setbox0=\\hbox{%");
        assert!(
            b.text
                .ends_with("}%\n\\lemmapreviewship\n\\end{document}\n")
        );
    }

    #[test]
    fn full_bodies_start_with_the_preamble_and_keep_definitions() {
        let defs = "\\makeatletter\n\\providecommand{\\x}{y}\n\\makeatother\n";
        let formula = "\\begin{align*}\na&=b\\\\\nc&=d\n\\end{align*}";
        let b = body(
            Route::Full,
            "\\documentclass{amsart}",
            defs,
            &FormulaKind::Display,
            formula,
        );
        assert!(
            b.text
                .starts_with("\\documentclass{amsart}\n\\nofiles\n\\makeatletter\n")
        );
        let lines: Vec<&str> = b.text.lines().collect();
        let at = b.formula_line as usize - 1;
        assert_eq!(lines[at], "\\begin{align*}");
        assert_eq!(lines[at + 3], "\\end{align*}");
        assert_eq!(lines[at + 4], "\\par}%");
        assert_eq!(
            lines[at - 1],
            "\\setbox0=\\vbox{\\hsize=\\linewidth\\noindent"
        );
        // The definitions come before \begin{document}.
        assert!(
            b.text.find("\\providecommand").unwrap() < b.text.find("\\begin{document}").unwrap()
        );
    }

    #[test]
    fn format_sources_end_the_document() {
        assert_eq!(
            format_source("\\documentclass{article}"),
            "\\documentclass{article}\n\\begin{document}\n\\end{document}\n"
        );
    }
}
