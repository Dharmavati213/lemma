//! Plain-English explanations of the most common TeX and LaTeX errors.
//!
//! [`explain`] matches a parsed log message (as [`crate::parse_log`] words it: `LaTeX Error: `
//! dropped, `Package x Error: m` as `Package x: m`) against a table of 28 errors and 5 common
//! warnings. Each match gives a one-sentence [`Explanation`] for the Problems row's tooltip and
//! expander, and, where a fix is safe, a [`FixHint`] saying what the app's quick fix
//! (`lemma-app/src/quickfix.rs`) should do. The table was checked against the logs of real
//! `pdflatex` runs (`tests/data/explain/`, one per error) and the fixtures' logs.
//!
//! The rest of the module are the GUI-free pieces of those fixes: locating TeX's error point in
//! a source line ([`error_point`]), the math-only token run around it ([`math_run`]), edit
//! distances ([`edit_distance`], [`closest`]) and the edits themselves (`*_edit` functions),
//! each in document byte offsets.

use lemma_core::TextEdit;
use std::ops::Range;

/// The errors and warnings the table knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    UndefinedControlSequence,
    MissingDollar,
    EnvironmentUndefined,
    MismatchedEnd,
    FileNotFound,
    MissingBeginDocument,
    ExtraBraceOrDollar,
    TooManyBraces,
    MissingBrace,
    ExtraAlignmentTab,
    MisplacedAlignmentTab,
    DoubleScript,
    DisplayMathEnd,
    ParagraphEnded,
    FileEnded,
    NoLineToEnd,
    AlreadyDefined,
    MissingItem,
    OnlyInPreamble,
    UnicodeCharacter,
    InvalidInMathMode,
    MissingNumber,
    IllegalUnit,
    OptionClash,
    NotInOuterParMode,
    LeftRight,
    ArgumentExtraBrace,
    IllegalParameter,
    // Warnings.
    ReferenceUndefined,
    CitationUndefined,
    LabelMultiplyDefined,
    OverfullBox,
    UnderfullBox,
}

impl ErrorKind {
    /// Every kind, errors first.
    pub const ALL: [ErrorKind; 33] = [
        Self::UndefinedControlSequence,
        Self::MissingDollar,
        Self::EnvironmentUndefined,
        Self::MismatchedEnd,
        Self::FileNotFound,
        Self::MissingBeginDocument,
        Self::ExtraBraceOrDollar,
        Self::TooManyBraces,
        Self::MissingBrace,
        Self::ExtraAlignmentTab,
        Self::MisplacedAlignmentTab,
        Self::DoubleScript,
        Self::DisplayMathEnd,
        Self::ParagraphEnded,
        Self::FileEnded,
        Self::NoLineToEnd,
        Self::AlreadyDefined,
        Self::MissingItem,
        Self::OnlyInPreamble,
        Self::UnicodeCharacter,
        Self::InvalidInMathMode,
        Self::MissingNumber,
        Self::IllegalUnit,
        Self::OptionClash,
        Self::NotInOuterParMode,
        Self::LeftRight,
        Self::ArgumentExtraBrace,
        Self::IllegalParameter,
        Self::ReferenceUndefined,
        Self::CitationUndefined,
        Self::LabelMultiplyDefined,
        Self::OverfullBox,
        Self::UnderfullBox,
    ];

    /// True for the warnings at the end of the table.
    pub fn is_warning(self) -> bool {
        matches!(
            self,
            Self::ReferenceUndefined
                | Self::CitationUndefined
                | Self::LabelMultiplyDefined
                | Self::OverfullBox
                | Self::UnderfullBox
        )
    }
}

/// What a quick fix can do about an error (the app decides whether it applies to the text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixHint {
    /// Wrap the math-only token run at TeX's error point in `$…$` ([`wrap_in_math_edit`]).
    WrapInMath,
    /// `\name` (without the backslash) is undefined: load the package that defines it, or use
    /// the closest known macro.
    UndefinedCommand { name: String },
    /// The environment `name` is undefined: load its package, declare it with `\newtheorem`
    /// (theorem-like names), or use the closest known environment.
    UndefinedEnvironment { name: String },
    /// `\begin{begin}` was closed by `\end{end}`: change the `\end` ([`fix_end_edit`]).
    MismatchedEnd { begin: String, end: String },
    /// The file `name` (as TeX reports it) does not exist: use the closest existing name.
    FileNotFound { name: String },
    /// The document has no `\begin{document}`: insert one where text starts.
    MissingBeginDocument,
}

/// A matched error: its kind, a one-sentence explanation and maybe a fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    pub kind: ErrorKind,
    /// One plain-English sentence (no markup).
    pub text: String,
    pub fix: Option<FixHint>,
}

/// Explains a log message (`context` is the diagnostic's `pre <HERE> post` TeX context, used to
/// name the undefined command). `None` for messages the table does not know.
pub fn explain(message: &str, context: Option<&str>) -> Option<Explanation> {
    let m = message.trim();
    let explained = |kind, text: String, fix| Some(Explanation { kind, text, fix });
    if m.starts_with("Undefined control sequence") {
        let name = context.and_then(undefined_name);
        let text = match &name {
            Some(name) => format!(
                "TeX does not know the command \\{name}: it is misspelled, or it comes from a \
                 package that is not loaded."
            ),
            None => "TeX does not know a command on this line: it is misspelled, or it comes \
                     from a package that is not loaded."
                .to_owned(),
        };
        let fix = name.map(|name| FixHint::UndefinedCommand { name });
        return explained(ErrorKind::UndefinedControlSequence, text, fix);
    }
    if m.starts_with("Missing $ inserted") {
        return explained(
            ErrorKind::MissingDollar,
            "Something that only works in math (such as _, ^ or \\alpha) appeared in text, so \
             TeX started a formula on its own; put the formula between $ and $."
                .to_owned(),
            Some(FixHint::WrapInMath),
        );
    }
    if let Some(name) = between(m, "Environment ", " undefined") {
        return explained(
            ErrorKind::EnvironmentUndefined,
            format!(
                "LaTeX does not know the environment “{name}”: it is misspelled, or the package \
                 (or the \\newtheorem) that defines it is missing."
            ),
            Some(FixHint::UndefinedEnvironment {
                name: name.to_owned(),
            }),
        );
    }
    if let Some(rest) = m.strip_prefix("\\begin{")
        && let Some((begin, rest)) = rest.split_once('}')
        && let Some(end) = rest
            .find("ended by \\end{")
            .map(|i| &rest[i + "ended by \\end{".len()..])
            .and_then(|r| r.split_once('}'))
            .map(|(e, _)| e)
    {
        let line = between(rest.trim_start(), "on input line ", " ended");
        if begin == "document" {
            return explained(
                ErrorKind::MismatchedEnd,
                format!(
                    "LaTeX found \\end{{{end}}} without a matching \\begin{{{end}}}; this is \
                     often a consequence of an earlier error about “{end}”."
                ),
                None,
            );
        }
        let opened = line.map_or_else(String::new, |l| format!(" opened on line {l}"));
        return explained(
            ErrorKind::MismatchedEnd,
            format!(
                "The environment “{begin}”{opened} is closed by \\end{{{end}}}; the names of \
                 \\begin and \\end must match."
            ),
            Some(FixHint::MismatchedEnd {
                begin: begin.to_owned(),
                end: end.to_owned(),
            }),
        );
    }
    if let Some(name) = m
        .strip_prefix("File `")
        .and_then(|r| r.split_once("' not found"))
        .map(|(n, _)| n)
    {
        return explained(
            ErrorKind::FileNotFound,
            format!(
                "TeX cannot find the file “{name}”: check its name and folder (paths are \
                 relative to the main document)."
            ),
            Some(FixHint::FileNotFound {
                name: name.to_owned(),
            }),
        );
    }
    if let Some(name) = m
        .strip_prefix("I can't find file `")
        .and_then(|r| r.split_once('\''))
        .map(|(n, _)| n)
    {
        let generated = [
            ".aux", ".toc", ".lof", ".lot", ".out", ".bbl", ".nav", ".snm",
        ]
        .iter()
        .any(|e| name.ends_with(e));
        if generated {
            return explained(
                ErrorKind::FileNotFound,
                format!(
                    "TeX could not read its own file “{name}”; this follows from an earlier \
                     error (often a missing \\begin{{document}})."
                ),
                None,
            );
        }
        return explained(
            ErrorKind::FileNotFound,
            format!(
                "TeX cannot find the file “{name}”: check its name and folder (paths are \
                 relative to the main document)."
            ),
            Some(FixHint::FileNotFound {
                name: name.to_owned(),
            }),
        );
    }
    if m.starts_with("Missing \\begin{document}") {
        return explained(
            ErrorKind::MissingBeginDocument,
            "Text was found before \\begin{document}: a stray character in the preamble, or \
             \\begin{document} itself is missing."
                .to_owned(),
            Some(FixHint::MissingBeginDocument),
        );
    }
    if m.starts_with("Extra }, or forgotten") {
        return explained(
            ErrorKind::ExtraBraceOrDollar,
            "A closing brace } has no matching {, or the $ that should close a formula before \
             it is missing."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Too many }'s") {
        return explained(
            ErrorKind::TooManyBraces,
            "There is a closing brace } without a matching opening brace {.".to_owned(),
            None,
        );
    }
    if m.starts_with("Missing } inserted") || m.starts_with("Missing { inserted") {
        return explained(
            ErrorKind::MissingBrace,
            "TeX expected a brace here: a group or formula is not closed properly, usually \
             because a { or } is missing."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Extra alignment tab has been changed") {
        return explained(
            ErrorKind::ExtraAlignmentTab,
            "A row of this table or alignment has more & separators than it has columns; add a \
             column to the column specification or remove an &."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Misplaced alignment tab character") {
        return explained(
            ErrorKind::MisplacedAlignmentTab,
            "& separates the columns of tables and alignments and cannot appear in ordinary \
             text; write \\& for an ampersand."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Double superscript") || m.starts_with("Double subscript") {
        let which = if m.contains("superscript") {
            "superscripts, as in x^2^3; write x^{2^3} or {x^2}^3"
        } else {
            "subscripts, as in x_1_2; write x_{1_2} or {x_1}_2"
        };
        return explained(
            ErrorKind::DoubleScript,
            format!("A symbol has two {which}."),
            None,
        );
    }
    if m.starts_with("Display math should end with $$") {
        return explained(
            ErrorKind::DisplayMathEnd,
            "A display formula opened with $$ is closed by a single $ (or the other way \
             round); write displays as \\[ … \\]."
                .to_owned(),
            None,
        );
    }
    if let Some(cmd) = between(m, "Paragraph ended before ", " was complete") {
        let cmd = shown_command(cmd);
        return explained(
            ErrorKind::ParagraphEnded,
            format!(
                "The argument of {cmd} contains a blank line, or its closing brace }} is \
                 missing."
            ),
            None,
        );
    }
    if m.starts_with("Runaway argument") || m.starts_with("Runaway text") {
        return explained(
            ErrorKind::ParagraphEnded,
            "An argument was never closed: TeX read on to a blank line or the end of the file \
             looking for its closing brace }."
                .to_owned(),
            None,
        );
    }
    if let Some(cmd) = m.strip_prefix("File ended while scanning use of ") {
        let cmd = shown_command(cmd.trim_end_matches(['.', ' ']));
        return explained(
            ErrorKind::FileEnded,
            format!(
                "The file ended inside the argument of {cmd}: a closing brace }} is missing \
                 (the problem starts where that argument begins)."
            ),
            None,
        );
    }
    if m.starts_with("There's no line here to end") {
        return explained(
            ErrorKind::NoLineToEnd,
            "A line break \\\\ comes where no line has started (at the start of a paragraph or \
             right after a list); use a blank line or \\vspace instead."
                .to_owned(),
            None,
        );
    }
    if let Some(cmd) = between(m, "Command ", " already defined") {
        return explained(
            ErrorKind::AlreadyDefined,
            format!(
                "\\newcommand cannot define {cmd}, because it already exists; choose another \
                 name, or use \\renewcommand to replace it."
            ),
            None,
        );
    }
    if m.starts_with("Something's wrong--perhaps a missing \\item")
        || m.starts_with("Lonely \\item")
    {
        return explained(
            ErrorKind::MissingItem,
            "List environments (itemize, enumerate, description) need \\item before their \
             text, and \\item only works inside such a list."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Can be used only in preamble") {
        return explained(
            ErrorKind::OnlyInPreamble,
            "This command (such as \\usepackage) only works before \\begin{document}; move it \
             to the preamble."
                .to_owned(),
            None,
        );
    }
    if let Some(rest) = m.strip_prefix("Unicode character ") {
        let ch = rest.split_whitespace().next().unwrap_or("this character");
        return explained(
            ErrorKind::UnicodeCharacter,
            format!(
                "pdfLaTeX has no glyph for the character {ch}; type the LaTeX command for it, \
                 or build with LuaLaTeX or XeLaTeX."
            ),
            None,
        );
    }
    if let Some(cmd) = between(m, "Command ", " invalid in math mode") {
        return explained(
            ErrorKind::InvalidInMathMode,
            format!(
                "{cmd} only works in text but appears inside a formula; often a closing $ is \
                 missing before it."
            ),
            None,
        );
    }
    if m.starts_with("Missing number, treated as zero") {
        return explained(
            ErrorKind::MissingNumber,
            "TeX expected a number or a length here; a common cause is a [ right after \\\\, \
             which TeX reads as an optional spacing argument."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Illegal unit of measure") {
        return explained(
            ErrorKind::IllegalUnit,
            "A length is missing its unit (pt, cm, mm, em, ex, …), as in \\vspace{2}.".to_owned(),
            None,
        );
    }
    if let Some(package) = m
        .strip_prefix("Option clash for package ")
        .map(|p| p.trim_end_matches('.'))
    {
        return explained(
            ErrorKind::OptionClash,
            format!(
                "The package {package} is loaded twice with different options (perhaps once by \
                 the class or another package); load it once with all its options."
            ),
            None,
        );
    }
    if m.starts_with("Not in outer par mode") {
        return explained(
            ErrorKind::NotInOuterParMode,
            "A figure, table or \\marginpar is inside a box, minipage or footnote, where floats \
             cannot go; move it out, or use the [H] placement of the float package."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Missing \\right. inserted")
        || m.starts_with("Extra \\right")
        || m.starts_with("Missing delimiter")
    {
        return explained(
            ErrorKind::LeftRight,
            "\\left and \\right must come in pairs within one formula (and one line of an \
             alignment); use \\left. or \\right. for an invisible side."
                .to_owned(),
            None,
        );
    }
    if let Some(cmd) = between(m, "Argument of ", " has an extra }") {
        let cmd = shown_command(cmd);
        return explained(
            ErrorKind::ArgumentExtraBrace,
            format!("A }} ends the argument of {cmd} before it began; usually a {{ is missing."),
            None,
        );
    }
    if let Some(cmd) = m
        .strip_prefix("Illegal parameter number in definition of ")
        .map(|c| c.trim_end_matches('.'))
    {
        return explained(
            ErrorKind::IllegalParameter,
            format!(
                "The definition of {cmd} uses #1, #2, … beyond its number of arguments, or a # \
                 that should be written \\#."
            ),
            None,
        );
    }
    // Warnings.
    if m.starts_with("Reference ") && m.contains(" undefined") {
        let label = quoted(m).unwrap_or("this label");
        return explained(
            ErrorKind::ReferenceUndefined,
            format!(
                "There is no \\label{{{label}}} in the project, or the document needs one more \
                 build to resolve it."
            ),
            None,
        );
    }
    if m.starts_with("Citation ") && m.contains(" undefined") {
        let key = quoted(m).unwrap_or("this key");
        return explained(
            ErrorKind::CitationUndefined,
            format!(
                "The key {key} is in no bibliography file the document reads, or BibTeX or \
                 biber has not run since it was added."
            ),
            None,
        );
    }
    if m.starts_with("Label ") && m.contains("multiply defined") {
        let label = quoted(m).unwrap_or("this label");
        return explained(
            ErrorKind::LabelMultiplyDefined,
            format!(
                "Two \\label commands use the name {label}, so references to it may point to \
                 the wrong place."
            ),
            None,
        );
    }
    if m.starts_with("Overfull \\hbox") {
        return explained(
            ErrorKind::OverfullBox,
            "A line sticks out into the margin because TeX found no good place to break it; \
             rephrase, allow hyphenation with \\-, or break a long formula."
                .to_owned(),
            None,
        );
    }
    if m.starts_with("Underfull \\hbox") {
        return explained(
            ErrorKind::UnderfullBox,
            "A line is spaced too loosely, often because of \\\\ or \\newline at the end of a \
             paragraph or a very short line; usually harmless."
                .to_owned(),
            None,
        );
    }
    None
}

/// `s` between `prefix` (at its start) and the first `suffix` after it.
fn between<'a>(s: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(prefix)?;
    rest.find(suffix).map(|i| &rest[..i])
}

/// The first name quoted as `x' or 'x' in `s`.
fn quoted(s: &str) -> Option<&str> {
    let start = s.find(['`', '\''])? + 1;
    let len = s[start..].find('\'')?;
    Some(&s[start..start + len]).filter(|n| !n.is_empty())
}

/// How a command from a message is shown: LaTeX's internal names (`\text@command`,
/// `\blx@citeargs@iii`) as "a command".
fn shown_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.contains('@') || cmd.is_empty() {
        "a command".to_owned()
    } else {
        cmd.to_owned()
    }
}

// ----- the error point -----

/// The parts of a TeX context `pre <HERE> post`.
pub fn split_context(context: &str) -> Option<(&str, &str)> {
    let (pre, post) = context.split_once(" <HERE>")?;
    Some((pre, post.trim_start()))
}

/// The undefined control sequence of an "Undefined control sequence" error: the last one TeX
/// read before the error point (without the backslash).
pub fn undefined_name(context: &str) -> Option<String> {
    let (pre, _) = split_context(context)?;
    let pre = pre.trim_end();
    let at = pre.rfind('\\')?;
    let name = &pre[at + 1..];
    let letters = name
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '@')
        .count();
    if letters == 0 {
        // A control symbol (`\#`) or a backslash at the very end.
        let symbol = name.chars().next()?;
        return (name.chars().count() == 1).then(|| symbol.to_string());
    }
    // The name must reach the error point (TeX stops right after it).
    (letters == name.len()).then(|| name.to_owned())
}

/// Where TeX stopped in `line_text` (one source line, without its line break), from the context
/// `pre <HERE> post` of an error on that line: the byte offset right after `pre`. `pre` may be
/// cut at its start (`...`) and `post` at its end; both must match the line.
pub fn error_point(line_text: &str, context: &str) -> Option<usize> {
    let (pre, post) = split_context(context)?;
    let pre = pre.strip_prefix("...").unwrap_or(pre);
    let post = post.strip_suffix("...").unwrap_or(post);
    // TeX's `^^M` marks the end of the line.
    let post = post.strip_suffix("^^M").unwrap_or(post).trim();
    let post_head: String = post.chars().take(24).collect();
    if pre.trim().is_empty() {
        // The error is at the start of the line (after its indentation).
        let indent = line_text.len() - line_text.trim_start().len();
        return line_text[indent..]
            .starts_with(post_head.as_str())
            .then_some(indent);
    }
    let mut found = None;
    for (at, _) in line_text.match_indices(pre) {
        let point = at + pre.len();
        let rest = line_text[point..].trim_start();
        if post_head.is_empty() || rest.starts_with(post_head.as_str()) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(point);
        }
    }
    found
}

/// The run of math-only material around byte `point` of `line_text` that a "Missing $
/// inserted" error is about: the brace-balanced, space-free stretch containing `point` (not
/// crossing an unmatched brace or a `$`), widened over operators and short operands on both
/// sides (`x^2 + y^2 = 1`) and without trailing punctuation.
///
/// TeX reports the error right after the math-only token it read, so the run is found only
/// when `point` follows `_`, `^` or a control word that `is_math_command` (given the name
/// without its backslash) calls math-only; anything else (the `$` TeX inserts at the end of a
/// paragraph it put into math, a text command) has no safe fix.
pub fn math_run(
    line_text: &str,
    point: usize,
    is_math_command: &dyn Fn(&str) -> bool,
) -> Option<Range<usize>> {
    let bytes = line_text.as_bytes();
    let point = point.min(bytes.len());
    if !line_text.is_char_boundary(point) || !after_math_token(&line_text[..point], is_math_command)
    {
        return None;
    }
    let mut start = word_start(bytes, point)?;
    let mut end = word_end(bytes, point);
    // Trailing punctuation and an unmatched closing parenthesis belong to the text.
    loop {
        let run = &line_text[start..end];
        let unmatched_close = (run.ends_with(')')
            && run.matches('(').count() < run.matches(')').count())
            || (run.ends_with(']') && run.matches('[').count() < run.matches(']').count());
        if (run.ends_with(['.', ',', ';', ':', '!', '?']) && end > point) || unmatched_close {
            end -= 1;
        } else {
            break;
        }
    }
    // A leading unmatched opening parenthesis too.
    while start < end {
        let run = &line_text[start..end];
        if (run.starts_with('(') && run.matches('(').count() > run.matches(')').count())
            || (run.starts_with('[') && run.matches('[').count() > run.matches(']').count())
        {
            start += 1;
        } else {
            break;
        }
    }
    if start >= end || point < start || point > end {
        return None;
    }
    // Widen over `op operand` pairs to the right and `operand op` pairs to the left.
    loop {
        let rest = &line_text[end..];
        let Some((op, operand)) = next_two_words(rest) else {
            break;
        };
        if !is_operator(op.1) || !is_operand(operand.1, is_math_command) {
            break;
        }
        end += operand.0.end;
        let run_end = trim_punctuation(&line_text[..end]);
        end = run_end;
    }
    loop {
        let before = &line_text[..start];
        let Some((operand, op)) = previous_two_words(before) else {
            break;
        };
        if !is_operator(op.1) || !is_operand(operand.1, is_math_command) {
            break;
        }
        start = operand.0.start;
    }
    Some(start..end)
}

fn trim_punctuation(text: &str) -> usize {
    text.trim_end_matches(['.', ',', ';', ':', '!', '?']).len()
}

/// Start of the space-free, brace-balanced stretch that ends at or contains `point`.
fn word_start(bytes: &[u8], point: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = point;
    while i > 0 {
        let b = bytes[i - 1];
        match b {
            b' ' | b'\t' | b'$' => break,
            b'}' => depth += 1,
            b'{' if depth == 0 => break,
            b'{' => depth -= 1,
            _ => {}
        }
        i -= 1;
    }
    Some(i)
}

fn word_end(bytes: &[u8], point: usize) -> usize {
    let mut depth = 0i32;
    let mut i = point;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'$' | b'\r' | b'%' => break,
            b'{' => depth += 1,
            b'}' if depth == 0 => break,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Whether `before` (a line up to TeX's error point) ends with `_`, `^` or a math-only control
/// word.
fn after_math_token(before: &str, is_math_command: &dyn Fn(&str) -> bool) -> bool {
    if before.ends_with(['_', '^']) {
        return true;
    }
    let letters = before
        .bytes()
        .rev()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    let name_start = before.len() - letters;
    letters > 0 && before[..name_start].ends_with('\\') && is_math_command(&before[name_start..])
}

/// Whether `run` holds math: `_`, `^`, or a math-only control word.
fn is_mathy(run: &str, is_math_command: &dyn Fn(&str) -> bool) -> bool {
    run.contains(['_', '^'])
        || run.match_indices('\\').any(|(at, _)| {
            let name: String = run[at + 1..]
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect();
            !name.is_empty() && is_math_command(&name)
        })
}

const OPERATORS: &[&str] = &[
    "+",
    "-",
    "=",
    "<",
    ">",
    "/",
    "|",
    "\\le",
    "\\ge",
    "\\leq",
    "\\geq",
    "\\neq",
    "\\to",
    "\\cdot",
    "\\times",
    "\\in",
    "\\subset",
    "\\subseteq",
    "\\approx",
    "\\sim",
    "\\cong",
    "\\simeq",
    "\\equiv",
    "\\mapsto",
    "\\pm",
    "\\circ",
    "\\otimes",
    "\\oplus",
    "\\cap",
    "\\cup",
];

fn is_operator(word: &str) -> bool {
    OPERATORS.contains(&word)
}

/// A word that can be the other side of an operator: math material, a number, or one or two
/// letters (`x`, `ab`).
fn is_operand(word: &str, is_math_command: &dyn Fn(&str) -> bool) -> bool {
    let word = word.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    !word.is_empty()
        && !word.contains('$')
        && (is_mathy(word, is_math_command)
            || word.chars().all(|c| c.is_ascii_digit() || c == '.')
            || (word.chars().count() <= 2 && word.chars().all(|c| c.is_ascii_alphanumeric()))
            || word.starts_with(['(', '[']) && word.chars().any(|c| c.is_ascii_alphanumeric()))
}

/// The next two space-separated words of `text` with their byte ranges in `text`.
#[allow(clippy::type_complexity)]
fn next_two_words(text: &str) -> Option<((Range<usize>, &str), (Range<usize>, &str))> {
    let mut words = words(text);
    Some((words.next()?, words.next()?))
}

#[allow(clippy::type_complexity)]
fn previous_two_words(text: &str) -> Option<((Range<usize>, &str), (Range<usize>, &str))> {
    let all: Vec<_> = words(text).collect();
    // The run starts right after a space: the last word must end at the end of `text` minus
    // the spaces.
    if all.len() < 2 {
        return None;
    }
    let n = all.len();
    Some((all[n - 2].clone(), all[n - 1].clone()))
}

fn words(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let mut offset = 0;
    text.split(' ').filter_map(move |w| {
        let start = offset;
        offset += w.len() + 1;
        (!w.is_empty()).then_some((start..start + w.len(), w))
    })
}

/// The quick fix of "Missing $ inserted": `$` around the math run at the error point of the
/// line `line_text` (starting at byte `line_start` of the document; see [`math_run`]).
/// Descending edits.
pub fn wrap_in_math_edit(
    line_text: &str,
    line_start: usize,
    context: &str,
    is_math_command: &dyn Fn(&str) -> bool,
) -> Option<Vec<TextEdit>> {
    let point = error_point(line_text, context)?;
    let run = math_run(line_text, point, is_math_command)?;
    Some(vec![
        TextEdit::insert(line_start + run.end, "$"),
        TextEdit::insert(line_start + run.start, "$"),
    ])
}

// ----- names -----

/// The optimal string alignment distance between `a` and `b` (insertions, deletions,
/// substitutions and transpositions of adjacent characters each cost 1).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// The largest edit distance [`closest`] accepts for a name of this many characters: 1 up to
/// four characters, 2 up to eight, 3 beyond (a typo, not a different word).
pub fn max_distance(len: usize) -> usize {
    match len {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    }
}

/// The candidate closest to `name` by [`edit_distance`], within [`max_distance`]; ties go to
/// the earlier candidate. `name` itself never counts.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = max_distance(name.chars().count());
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if c == name || c.len().abs_diff(name.len()) > limit {
            continue;
        }
        let d = edit_distance(name, c);
        if d <= limit && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

// ----- edits -----

/// Lines of `text` with their byte ranges (without the line break).
fn lines_with_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    text.split('\n').map(move |line| {
        let start = offset;
        offset += line.len() + 1;
        (start, line)
    })
}

/// `line` up to its comment (see [`crate::chktex::comment_start`]).
fn code_part(line: &str) -> &str {
    crate::chktex::comment_start(line).map_or(line, |at| &line[..at])
}

/// Byte offset of the `\begin{document}` of `text` (outside comments), if any.
pub fn begin_document(text: &str) -> Option<usize> {
    lines_with_offsets(text).find_map(|(start, line)| {
        code_part(line)
            .find("\\begin{document}")
            .map(|at| start + at)
    })
}

/// Whether the preamble of `text` loads `package` (`\usepackage[…]{a,package}` or
/// `\RequirePackage`).
pub fn loads_package(text: &str, package: &str) -> bool {
    let preamble = &text[..begin_document(text).unwrap_or(text.len())];
    lines_with_offsets(preamble).any(|(_, line)| {
        let code = code_part(line);
        ["\\usepackage", "\\RequirePackage"].iter().any(|cmd| {
            code.match_indices(cmd).any(|(at, _)| {
                let rest = &code[at + cmd.len()..];
                let rest = skip_optional(rest.trim_start());
                rest.strip_prefix('{')
                    .and_then(|r| r.split_once('}'))
                    .is_some_and(|(names, _)| names.split(',').any(|n| n.trim() == package))
            })
        })
    })
}

fn skip_optional(s: &str) -> &str {
    match s.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(s, |(_, r)| r.trim_start()),
        None => s,
    }
}

/// Where a new preamble line goes: after the last line holding `\usepackage` (else
/// `\documentclass`) before `\begin{document}`. The byte offset of the start of the line after
/// it. `None` without `\documentclass`.
fn preamble_insertion_point(text: &str, after: &[&str]) -> Option<usize> {
    let end = begin_document(text).unwrap_or(text.len());
    let mut class_line = None;
    let mut last = None;
    for (start, line) in lines_with_offsets(&text[..end]) {
        let code = code_part(line);
        if code.contains("\\documentclass") {
            class_line = Some(start + line.len());
        }
        if after.iter().any(|a| code.contains(a)) {
            last = Some(start + line.len());
        }
    }
    let line_end = last.or(class_line)?;
    // A `\documentclass` or `\usepackage` spanning several lines ends where its braces close;
    // keep it simple and refuse when the line leaves a brace open.
    Some((line_end + 1).min(text.len()))
}

/// The edit that loads `package` in the preamble of the root document `text`: a
/// `\usepackage{package}` line after the last `\usepackage` (else after `\documentclass`).
/// `None` when it is loaded already or there is no `\documentclass`.
pub fn usepackage_edit(text: &str, package: &str) -> Option<TextEdit> {
    if loads_package(text, package) {
        return None;
    }
    let at = preamble_insertion_point(text, &["\\usepackage", "\\RequirePackage"])?;
    let mut line = format!("\\usepackage{{{package}}}\n");
    if at == text.len() && !text.ends_with('\n') {
        line.insert(0, '\n');
    }
    Some(TextEdit::insert(at, line))
}

/// The edit that declares the theorem-like environment `env` (`lemma` → `\newtheorem{lemma}
/// [shared]{Lemma}`) in the preamble of `text`, after its last `\newtheorem` (else after the
/// last `\usepackage`). `shared` is an existing theorem-like environment whose counter it
/// shares. `None` without `\documentclass`.
pub fn newtheorem_edit(text: &str, env: &str, shared: Option<&str>) -> Option<TextEdit> {
    let has_theorems = {
        let end = begin_document(text).unwrap_or(text.len());
        text[..end]
            .lines()
            .any(|l| code_part(l).contains("\\newtheorem"))
    };
    let after: &[&str] = if has_theorems {
        &["\\newtheorem"]
    } else {
        &["\\usepackage", "\\RequirePackage"]
    };
    let at = preamble_insertion_point(text, after)?;
    let title = theorem_title(env);
    let counter = shared.map_or_else(String::new, |s| format!("[{s}]"));
    Some(TextEdit::insert(
        at,
        format!("\\newtheorem{{{env}}}{counter}{{{title}}}\n"),
    ))
}

/// Commands of common packages that have no completion table (the app looks in its cwl tables
/// first): command name (without the backslash) → package.
pub const COMMAND_PACKAGES: &[(&str, &str)] = &[
    ("AxiomC", "bussproofs"),
    ("BinaryInfC", "bussproofs"),
    ("UnaryInfC", "bussproofs"),
    ("TrinaryInfC", "bussproofs"),
    ("bcancel", "cancel"),
    ("blindtext", "blindtext"),
    ("bm", "bm"),
    ("bra", "braket"),
    ("braket", "braket"),
    ("cancel", "cancel"),
    ("captionof", "caption"),
    ("captionsetup", "caption"),
    ("ding", "pifont"),
    ("doublespacing", "setspace"),
    ("enquote", "csquotes"),
    ("euro", "eurosym"),
    ("fancyfoot", "fancyhdr"),
    ("fancyhead", "fancyhdr"),
    ("fancyhf", "fancyhdr"),
    ("hhline", "hhline"),
    ("hl", "soul"),
    ("ifthenelse", "ifthen"),
    ("includepdf", "pdfpages"),
    ("inferrule", "mathpartir"),
    ("ket", "braket"),
    ("lipsum", "lipsum"),
    ("listoftodos", "todonotes"),
    ("marginnote", "marginnote"),
    ("mathbbm", "bbm"),
    ("mathds", "dsfont"),
    ("missingfigure", "todonotes"),
    ("multirow", "multirow"),
    ("nameref", "nameref"),
    ("newcolumntype", "array"),
    ("onehalfspacing", "setspace"),
    ("setstretch", "setspace"),
    ("sout", "ulem"),
    ("subcaption", "subcaption"),
    ("subcaptionbox", "subcaption"),
    ("titleformat", "titlesec"),
    ("titlespacing", "titlesec"),
    ("todo", "todonotes"),
    ("uline", "ulem"),
    ("url", "url"),
    ("uwave", "ulem"),
    ("xcancel", "cancel"),
    ("xspace", "xspace"),
    ("xymatrix", "xy"),
];

/// Environments of common packages that have no completion table: name → package.
pub const ENVIRONMENT_PACKAGES: &[(&str, &str)] = &[
    ("algorithm", "algorithm"),
    ("appendices", "appendix"),
    ("comment", "comment"),
    ("doublespace", "setspace"),
    ("framed", "framed"),
    ("landscape", "pdflscape"),
    ("longtable", "longtable"),
    ("mdframed", "mdframed"),
    ("multicols", "multicol"),
    ("multicols*", "multicol"),
    ("onehalfspace", "setspace"),
    ("prooftree", "bussproofs"),
    ("spacing", "setspace"),
    ("subfigure", "subcaption"),
    ("subtable", "subcaption"),
    ("tabularx", "tabularx"),
    ("wrapfigure", "wrapfig"),
    ("wraptable", "wrapfig"),
];

/// Theorem-like environment names a `\newtheorem` fix may declare, with their titles.
pub const THEOREM_NAMES: &[(&str, &str)] = &[
    ("theorem", "Theorem"),
    ("thm", "Theorem"),
    ("lemma", "Lemma"),
    ("lem", "Lemma"),
    ("proposition", "Proposition"),
    ("prop", "Proposition"),
    ("corollary", "Corollary"),
    ("cor", "Corollary"),
    ("definition", "Definition"),
    ("defn", "Definition"),
    ("conjecture", "Conjecture"),
    ("claim", "Claim"),
    ("remark", "Remark"),
    ("rem", "Remark"),
    ("example", "Example"),
    ("exercise", "Exercise"),
    ("notation", "Notation"),
    ("observation", "Observation"),
    ("question", "Question"),
    ("fact", "Fact"),
];

fn theorem_title(env: &str) -> String {
    let base = env.trim_end_matches('*');
    THEOREM_NAMES.iter().find(|(n, _)| *n == base).map_or_else(
        || {
            let mut c = base.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        },
        |(_, t)| (*t).to_owned(),
    )
}

/// Replaces `\from` by `\to` everywhere on the line `line_text` (starting at byte
/// `line_start`), where it is a whole control word (not followed by a letter, not `\\from`).
/// Descending edits; empty when it does not occur.
pub fn replace_command_edits(
    line_text: &str,
    line_start: usize,
    from: &str,
    to: &str,
) -> Vec<TextEdit> {
    let needle = format!("\\{from}");
    let code = code_part(line_text);
    let mut edits: Vec<TextEdit> = code
        .match_indices(&needle)
        .filter(|(at, _)| {
            let after = code[at + needle.len()..].chars().next();
            let whole = !from.chars().all(|c| c.is_ascii_alphabetic())
                || !after.is_some_and(|c| c.is_ascii_alphabetic() || c == '@');
            let escaped = code[..*at].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1;
            whole && !escaped
        })
        .map(|(at, _)| TextEdit {
            range: line_start + at + 1..line_start + at + needle.len(),
            text: to.to_owned(),
        })
        .collect();
    edits.reverse();
    edits
}

/// The quick fix of `\begin{begin}` ended by `\end{end}`: the name in the first `\end{end}` on
/// the line `line_text` (at byte `line_start`) becomes `begin`.
pub fn fix_end_edit(
    line_text: &str,
    line_start: usize,
    begin: &str,
    end: &str,
) -> Option<TextEdit> {
    let code = code_part(line_text);
    let at = find_env_command(code, "end", end)?;
    Some(TextEdit {
        range: line_start + at.clone().start..line_start + at.end,
        text: begin.to_owned(),
    })
}

/// The byte range of the name in the first `\cmd{name}` (spaces allowed before `{`) of `text`.
fn find_env_command(text: &str, cmd: &str, name: &str) -> Option<Range<usize>> {
    let needle = format!("\\{cmd}");
    text.match_indices(&needle).find_map(|(at, _)| {
        let rest = &text[at + needle.len()..];
        let skipped = rest.len() - rest.trim_start().len();
        let rest = rest.trim_start().strip_prefix('{')?;
        let inner = rest.split_once('}')?.0;
        (inner.trim() == name).then(|| {
            let start = at + needle.len() + skipped + 1 + (inner.len() - inner.trim_start().len());
            start..start + name.len()
        })
    })
}

/// Renames the environment `old` to `new`: the first `\begin{old}` at or after byte `from` of
/// `text` (on that line) and the `\end{old}` that closes it (counting nested `old`
/// environments). Descending edits; `None` when either is missing.
pub fn rename_environment_edits(
    text: &str,
    from: usize,
    old: &str,
    new: &str,
) -> Option<Vec<TextEdit>> {
    let line_end = text[from..].find('\n').map_or(text.len(), |i| from + i);
    let begin = find_env_command(code_part(&text[from..line_end]), "begin", old)?;
    let begin = from + begin.start..from + begin.end;
    let mut depth = 0usize;
    for (start, line) in lines_with_offsets(&text[begin.end..]) {
        let code = code_part(line);
        let mut events: Vec<(usize, bool, Range<usize>)> = Vec::new();
        let mut scan = |cmd: &str, opens: bool| {
            let mut pos = 0;
            while let Some(r) = find_env_command(&code[pos..], cmd, old) {
                events.push((pos + r.start, opens, pos + r.start..pos + r.end));
                pos += r.end;
            }
        };
        scan("begin", true);
        scan("end", false);
        events.sort_by_key(|e| e.0);
        for (_, opens, range) in events {
            if opens {
                depth += 1;
            } else if depth == 0 {
                let base = begin.end + start;
                return Some(vec![
                    TextEdit {
                        range: base + range.start..base + range.end,
                        text: new.to_owned(),
                    },
                    TextEdit {
                        range: begin.clone(),
                        text: new.to_owned(),
                    },
                ]);
            } else {
                depth -= 1;
            }
        }
    }
    None
}

/// The quick fix of a missing `\begin{document}` in `text`: `\begin{document}` on its own line
/// before the 1-based line `line` where text started. `None` when `text` has a
/// `\begin{document}` (the error is then a stray character in the preamble) or no
/// `\documentclass`.
pub fn begin_document_edit(text: &str, line: u32) -> Option<TextEdit> {
    if begin_document(text).is_some() || !text.contains("\\documentclass") {
        return None;
    }
    let (start, _) = lines_with_offsets(text).nth(line.checked_sub(1)? as usize)?;
    Some(TextEdit::insert(start, "\\begin{document}\n"))
}

/// The argument of a file command (`\input{chapters/intor}`) on the line `line_text` (at
/// `line_start`) that names `requested` (as TeX reports it, maybe with an added `.tex`): its
/// byte range in the document and its text.
pub fn file_argument(
    line_text: &str,
    line_start: usize,
    requested: &str,
) -> Option<(Range<usize>, String)> {
    let code = code_part(line_text);
    let wanted = requested.strip_suffix(".tex").unwrap_or(requested);
    let mut pos = 0;
    while let Some(open) = code[pos..].find('{') {
        let start = pos + open + 1;
        let Some(len) = code[start..].find('}') else {
            break;
        };
        let arg = code[start..start + len].trim();
        let bare = arg.strip_suffix(".tex").unwrap_or(arg);
        if bare == wanted && !arg.is_empty() {
            let lead = code[start..].len() - code[start..].trim_start().len();
            let s = line_start + start + lead;
            return Some((s..s + arg.len(), arg.to_owned()));
        }
        pos = start + len + 1;
    }
    None
}

/// The file name that should replace the argument `written` (as in the source) when the file
/// `found` (relative, with its extension) is the closest existing one: keeps the extension
/// only when `written` had one.
pub fn replacement_name(written: &str, found: &str) -> String {
    let has_extension = std::path::Path::new(written)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains('.'));
    if has_extension {
        return found.to_owned();
    }
    match found.rsplit_once('.') {
        Some((stem, _)) if !stem.ends_with('/') && !stem.is_empty() => stem.to_owned(),
        _ => found.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[TextEdit]) -> String {
        let mut s = text.to_owned();
        lemma_core::apply_all(&mut s, edits);
        s
    }

    #[test]
    fn distances() {
        assert_eq!(edit_distance("lamda", "lambda"), 1);
        assert_eq!(edit_distance("alpah", "alpha"), 1);
        assert_eq!(edit_distance("equivalnce", "equiv"), 5);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(
            closest("lamda", ["alpha", "lambda", "Lambda"]),
            Some("lambda")
        );
        assert_eq!(closest("equivalnce", ["equiv", "equal"]), None);
        assert_eq!(closest("beign", ["begin", "being"]), Some("begin"));
        assert_eq!(closest("x", ["x"]), None);
    }

    #[test]
    fn undefined_names() {
        assert_eq!(
            undefined_name("is an \\equivalnce <HERE> of categories.").as_deref(),
            Some("equivalnce")
        );
        assert_eq!(
            undefined_name("Hello \\foo world and \\lamda <HERE> .").as_deref(),
            Some("lamda")
        );
        assert_eq!(undefined_name("x \\# <HERE> y").as_deref(), Some("#"));
        assert_eq!(undefined_name("\\foo{a} <HERE> b"), None);
        assert_eq!(undefined_name("no command <HERE> b"), None);
    }

    #[test]
    fn math_runs() {
        let math = |name: &str| ["alpha", "beta", "le", "cdot"].contains(&name);
        let run = |line: &str, context: &str| {
            let p = error_point(line, context)?;
            math_run(line, p, &math).map(|r| line[r].to_owned())
        };
        assert_eq!(
            run(
                "Exactness \\alpha_n follows since filtered colimits commute with finite limits.",
                "Exactness \\alpha <HERE> _n follows since filtered colimits commute with finite..."
            )
            .as_deref(),
            Some("\\alpha_n")
        );
        assert_eq!(
            run(
                "The value x_1 is positive.",
                "The value x_ <HERE> 1 is positive."
            )
            .as_deref(),
            Some("x_1")
        );
        assert_eq!(
            run("so x^2 + y^2 = 1, done", "so x^ <HERE> 2 + y^2 = 1, done").as_deref(),
            Some("x^2 + y^2 = 1")
        );
        assert_eq!(
            run("see \\emph{x_1} here", "see \\emph{x_ <HERE> 1} here").as_deref(),
            Some("x_1")
        );
        assert_eq!(
            run("(see f(x)_i).", "(see f(x)_ <HERE> i).").as_deref(),
            Some("f(x)_i")
        );
        assert_eq!(run("\\end{proof}", "\\end{proof} <HERE>"), None);
        assert_eq!(run("plain words", "plain <HERE> words"), None);
        assert_eq!(run("a \\emph{b} c", "a \\emph <HERE> {b} c"), None);
        assert_eq!(
            run("where a \\le b_1 holds", "where a \\le b_ <HERE> 1 holds").as_deref(),
            Some("a \\le b_1")
        );
        assert_eq!(
            run("  \\beta^2 at the start", " <HERE> \\beta^2 at the start"),
            None
        );
    }

    #[test]
    fn package_and_theorem_edits() {
        let doc = "\\documentclass{article}\n\\usepackage{amsmath}\n% \\usepackage{x}\n\\begin{document}\nHi\n\\end{document}\n";
        let e = usepackage_edit(doc, "amssymb").unwrap();
        assert_eq!(
            apply(doc, &[e]),
            "\\documentclass{article}\n\\usepackage{amsmath}\n\\usepackage{amssymb}\n% \\usepackage{x}\n\\begin{document}\nHi\n\\end{document}\n"
        );
        assert_eq!(usepackage_edit(doc, "amsmath"), None);
        assert!(loads_package(
            "\\documentclass{a}\n\\usepackage[x]{b, amssymb}\n",
            "amssymb"
        ));
        let bare = "\\documentclass{article}\n\\begin{document}\n";
        assert_eq!(
            apply(bare, &[usepackage_edit(bare, "xcolor").unwrap()]),
            "\\documentclass{article}\n\\usepackage{xcolor}\n\\begin{document}\n"
        );
        let thm = "\\documentclass{article}\n\\newtheorem{theorem}{Theorem}[section]\n\\begin{document}\n";
        assert_eq!(
            apply(
                thm,
                &[newtheorem_edit(thm, "lemma", Some("theorem")).unwrap()]
            ),
            "\\documentclass{article}\n\\newtheorem{theorem}{Theorem}[section]\n\\newtheorem{lemma}[theorem]{Lemma}\n\\begin{document}\n"
        );
        assert_eq!(usepackage_edit("no class", "x"), None);
    }

    #[test]
    fn command_and_environment_edits() {
        let line = "Hello \\lamda and \\lamdas, \\\\lamda \\lamda.";
        let edits = replace_command_edits(line, 10, "lamda", "lambda");
        assert_eq!(edits.len(), 2);
        let mut s = format!("{}{line}", "x".repeat(10));
        lemma_core::apply_all(&mut s, &edits);
        assert_eq!(&s[10..], "Hello \\lambda and \\lamdas, \\\\lamda \\lambda.");

        let e = fix_end_edit(
            "\\end {enumerate} % \\end{enumerate}",
            0,
            "itemize",
            "enumerate",
        )
        .unwrap();
        assert_eq!(
            apply("\\end {enumerate} % \\end{enumerate}", &[e]),
            "\\end {itemize} % \\end{enumerate}"
        );

        let doc = "\\begin{equaiton}\nx \\begin{equaiton} y \\end{equaiton}\n\\end{equaiton}\nz";
        let edits = rename_environment_edits(doc, 0, "equaiton", "equation").unwrap();
        assert_eq!(
            apply(doc, &edits),
            "\\begin{equation}\nx \\begin{equaiton} y \\end{equaiton}\n\\end{equation}\nz"
        );
        assert_eq!(rename_environment_edits("\\begin{x}\n", 0, "x", "y"), None);
    }

    #[test]
    fn begin_document_and_files() {
        let doc = "\\documentclass{article}\n\\usepackage{amsmath}\nSome text.\n\\end{document}\n";
        let e = begin_document_edit(doc, 3).unwrap();
        assert_eq!(
            apply(doc, &[e]),
            "\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\nSome text.\n\\end{document}\n"
        );
        assert_eq!(
            begin_document_edit("\\documentclass{a}\n\\begin{document}\nx", 2),
            None
        );

        let (range, written) =
            file_argument("  \\input{chapters/intor} % x", 100, "chapters/intor.tex").unwrap();
        assert_eq!((range, written.as_str()), (109..123, "chapters/intor"));
        assert_eq!(
            replacement_name("chapters/intor", "chapters/intro.tex"),
            "chapters/intro"
        );
        assert_eq!(replacement_name("fig.pdf", "figs.pdf"), "figs.pdf");
        assert_eq!(replacement_name("figs/plott", "figs/plot.png"), "figs/plot");
    }

    #[test]
    fn explanations_name_things() {
        let e = explain(
            "Undefined control sequence.",
            Some("is an \\equivalnce <HERE> of"),
        )
        .unwrap();
        assert_eq!(
            e.fix,
            Some(FixHint::UndefinedCommand {
                name: "equivalnce".into()
            })
        );
        assert!(e.text.contains("\\equivalnce"));
        let e = explain(
            "\\begin{itemize} on input line 3 ended by \\end{enumerate}.",
            None,
        )
        .unwrap();
        assert_eq!(
            e.fix,
            Some(FixHint::MismatchedEnd {
                begin: "itemize".into(),
                end: "enumerate".into()
            })
        );
        assert!(e.text.contains("line 3"));
        let e = explain("\\begin{document} ended by \\end{theorem}.", None).unwrap();
        assert_eq!((e.kind, e.fix), (ErrorKind::MismatchedEnd, None));
        let e = explain("File `chapters/intor.tex' not found.", None).unwrap();
        assert_eq!(
            e.fix,
            Some(FixHint::FileNotFound {
                name: "chapters/intor.tex".into()
            })
        );
        assert_eq!(
            explain("Paragraph ended before \\text@command was complete.", None)
                .unwrap()
                .text,
            "The argument of a command contains a blank line, or its closing brace } is missing."
        );
        assert_eq!(explain("Something entirely new.", None), None);
    }
}
