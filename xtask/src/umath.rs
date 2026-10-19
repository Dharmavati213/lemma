//! Unicode glyphs for LaTeX math commands.
//!
//! Two inputs, both optional:
//!
//! - `data/raw/umath_map.tsv`: the unicode-math table (`unicode-math-table.tex`) mapped to the
//!   commands a pdfLaTeX user types (made by `data/raw/umath*.py`). Columns: `codepoint`,
//!   `glyph`, `unicode_math_name`, `class`, `pdflatex_command`, `package`.
//! - `data/raw/unimathsymbols.txt`: Günter Milde's table (`code^char^latex^unicode-math^class^
//!   category^requirements^comments`), not committed. Only its LaTeX column is used, to add
//!   glyphs for commands the first table lacks. Lines that are not in this format are ignored.

/// One typable symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub codepoint: u32,
    pub glyph: char,
    /// The unicode-math class (`mathrel`, `mathaccent`, …), empty when unknown.
    pub class: String,
    /// The command as typed: `\alpha`, `\mathbb{Z}`.
    pub command: String,
    /// `kernel`, `amssymb`, `amsmath`, `stmaryrd`, `mathtools`, `mathrsfs`, or empty when unknown.
    pub package: String,
}

/// Commands the unicode-math mapping invents that do not exist in pdfLaTeX.
const NOT_LATEX: &[&str] = &["\\omicron"];

/// Parses `umath_map.tsv`.
pub fn parse_map(src: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    for line in src.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        let [cp, glyph, _name, class, command, package] = cols[..] else {
            continue;
        };
        let Some(codepoint) = cp
            .strip_prefix("U+")
            .and_then(|h| u32::from_str_radix(h, 16).ok())
        else {
            continue;
        };
        let Some(glyph) = char::from_u32(codepoint).filter(|c| glyph.starts_with(*c)) else {
            continue;
        };
        if command.is_empty() || NOT_LATEX.contains(&command) {
            continue;
        }
        // "kernel (upright glyph; italic in output)" is still the kernel.
        let package = package.split(' ').next().unwrap_or("");
        out.push(Symbol {
            codepoint,
            glyph,
            class: class.to_owned(),
            command: command.to_owned(),
            package: package.to_owned(),
        });
    }
    out
}

/// Parses the LaTeX column of `unimathsymbols.txt`; lines in any other format are skipped.
pub fn parse_unimathsymbols(src: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    for line in src.lines() {
        if line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('^').collect();
        if cols.len() < 8 {
            continue;
        }
        let Ok(codepoint) = u32::from_str_radix(cols[0], 16) else {
            continue;
        };
        let Some(glyph) = char::from_u32(codepoint).filter(|c| cols[1].starts_with(*c)) else {
            continue;
        };
        let command = cols[2].trim();
        if !command.starts_with('\\') || NOT_LATEX.contains(&command) {
            continue;
        }
        // Requirements such as `amssymb` or `-fourier` (a conflict); the first plain name wins.
        let package = cols[6]
            .split_whitespace()
            .find(|r| !r.starts_with('-') && !r.starts_with('='))
            .unwrap_or("kernel");
        out.push(Symbol {
            codepoint,
            glyph,
            class: String::new(),
            command: command.to_owned(),
            package: package.to_owned(),
        });
    }
    out
}

/// Whether a glyph cannot be shown on its own (combining accents).
pub fn is_combining(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// Whether a symbol of this class takes an argument (`\hat{x}`, `\overbrace{x}`).
pub fn takes_argument(class: &str) -> bool {
    matches!(
        class,
        "mathaccent"
            | "mathaccentwide"
            | "mathbotaccent"
            | "mathbotaccentwide"
            | "mathover"
            | "mathunder"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_rows() {
        let src = "codepoint\tglyph\tunicode_math_name\tclass\tpdflatex_command\tpackage\n\
                   U+03B1\tα\tmupalpha\tmathalpha\t\\alpha\tkernel (upright glyph; italic in output)\n\
                   U+2124\tℤ\tBbbZ\tmathalpha\t\\mathbb{Z}\tamssymb\n\
                   U+0021\t!\tmathexclam\tmathclose\t\tunicode-math only\n\
                   U+03BF\tο\tmupomicron\tmathalpha\t\\omicron\tkernel\n\
                   garbage\n";
        let rows = parse_map(src);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].command, "\\alpha");
        assert_eq!(rows[0].glyph, 'α');
        assert_eq!(rows[0].package, "kernel");
        assert_eq!(rows[1].command, "\\mathbb{Z}");
        assert_eq!(rows[1].codepoint, 0x2124);
    }

    #[test]
    fn unimathsymbols_rows_and_html() {
        let src = "# comment\n\
                   02260^≠^\\neq^\\ne^R^mathrel^^NOT EQUAL TO\n\
                   025A1^□^\\square^\\square^N^mathord^amssymb -fourier^WHITE SQUARE\n\
                   <html lang=\"en\">\n";
        let rows = parse_unimathsymbols(src);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].command, "\\neq");
        assert_eq!(rows[0].package, "kernel");
        assert_eq!(rows[1].package, "amssymb");
        assert!(parse_unimathsymbols("<!DOCTYPE html>\n<html>\n").is_empty());
    }

    #[test]
    fn combining_marks() {
        assert!(is_combining('\u{0302}'));
        assert!(is_combining('\u{20D7}'));
        assert!(!is_combining('α'));
    }
}
