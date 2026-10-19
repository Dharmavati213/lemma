//! chktex as a lint source.
//!
//! - [`run`] pipes a document's text into `chktex -q -v0 -I0 -S TabSize=1 -p NAME -f FORMAT`
//!   (blocking; the app runs it on its jobs thread after a save). The text goes through stdin, so
//!   the warnings describe exactly the text that was saved, and `-p` names the file in them.
//!   `-I0` keeps chktex from following `\input` (each file is linted when it is saved), and
//!   `TabSize=1` makes chktex's columns byte columns (it expands tabs to `TabSize` columns).
//!   A missing `chktex` is [`RunError::NotFound`]: the app then disables the lint quietly.
//! - [`parse`] reads the machine-readable [`FORMAT`] into [`LintWarning`]s; [`to_diagnostic`]
//!   makes a `lemma_core::Diagnostic` with `source: DiagSource::Lint`.
//! - [`is_shown`] applies `[lint] disabled` / `[lint] enabled` on top of
//!   [`DEFAULT_SILENCED`], the noisiest warnings, which Lemma hides unless enabled.
//! - `% chktex N` comments are honoured by chktex itself; [`suppress_on_line`] computes the edit
//!   that adds one.

use lemma_core::{DiagSource, Diagnostic, Severity, TextEdit};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// chktex's `-f` format: line, column, length, warning number, kind and message, separated by
/// `:` (the message comes last because it may contain colons).
pub const FORMAT: &str = "%l:%c:%d:%n:%k:%m\n";

/// How long one chktex run may take before it is killed.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Larger outputs are cut here (a pathological file can produce hundreds of thousands of
/// warnings).
const MAX_OUTPUT_BYTES: usize = 4 << 20;

/// Warnings Lemma hides unless `[lint] enabled` lists them. They were chosen by running chktex
/// 1.7.9 on the test fixtures and on the user's own corpus (432 files, about 138k lines,
/// counted by warning number only): these are the most frequent warnings that are false
/// positives in mathematical writing.
///
/// | # | Message | Why it is silenced |
/// |---|---|---|
/// | 1 | Command terminated with space | ~6800 hits in the corpus: text macros built to be followed by a space (`\og`, `\ie`, user macros with `\xspace`), `\hbox to`; the real cases (`\LaTeX is`) are rare |
/// | 3 | You should enclose the previous parenthesis with `{}` | ~6100 hits: standard notation such as `(g\circ f)^*`, `(\cF^+)^+` (154 of the fixtures' 437 warnings) |
/// | 9 | `]` expected, found `)` | ~1000 hits: half-open intervals `[0,1)`, `[a,b)` |
/// | 10 | Solo `)` found | ~3500 hits: French-style intervals `]0,1[` and enumerations `a)`, `b)` |
/// | 17 | Number of `(` doesn't match the number of `)` | the file-level echo of 9 and 10 |
/// | 18 | Use either `` or '' as an alternative to `"` | every one of the fixtures' 240 hits is a tikz-cd label, `\arrow[r, "f"]` |
/// | 24 | Delete this space to maintain correct pagereferences | ~2400 hits, nearly all `\section{…} \label{…}`, where the space is harmless |
/// | 36 | You should put a space in front of parenthesis | `map(s)`, `Theorem 1.2(a)` |
///
/// Kept on purpose although frequent: 45 (`$$ … $$`, deprecated in LaTeX), 25 (`x_ij`,
/// `2^10`: real bugs), 2, 8, 12 and 13 (typographic spacing and dashes).
pub const DEFAULT_SILENCED: &[u32] = &[1, 3, 9, 10, 17, 18, 24, 36];

/// How chktex classifies a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LintKind {
    Warning,
    Error,
    Message,
}

/// One chktex warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintWarning {
    /// 1-based line.
    pub line: u32,
    /// 1-based byte column within the line (with `TabSize=1`).
    pub column: u32,
    /// Length of the offending text in bytes; 0 for warnings about the whole file.
    pub length: u32,
    /// chktex's warning number (`% chktex N`, `-n N`).
    pub number: u32,
    pub kind: LintKind,
    pub message: String,
}

impl LintWarning {
    /// True for warnings about the file as a whole (warning 17 at the end of the file): no
    /// column to underline and no line to put a `% chktex N` on.
    pub fn is_file_level(&self) -> bool {
        self.length == 0
    }

    /// The byte range of the warning within `line_text` (its line without the line break),
    /// clamped to the line and to character boundaries; `None` for file-level warnings.
    pub fn byte_range(&self, line_text: &str) -> Option<std::ops::Range<usize>> {
        if self.is_file_level() {
            return None;
        }
        let len = line_text.len();
        let mut start = (self.column.max(1) as usize - 1).min(len);
        let mut end = start.saturating_add(self.length as usize).min(len);
        while !line_text.is_char_boundary(start) {
            start -= 1;
        }
        while !line_text.is_char_boundary(end) {
            end += 1;
        }
        Some(start..end)
    }
}

/// Why chktex could not lint a text.
#[derive(Debug)]
pub enum RunError {
    /// The program does not exist: the lint is off until the app is restarted.
    NotFound,
    /// It ran longer than [`TIMEOUT`] and was killed.
    TimedOut,
    /// Any other failure (it could not start, or it failed without printing warnings).
    Failed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::NotFound => f.write_str("chktex is not installed"),
            RunError::TimedOut => write!(f, "chktex ran longer than {} s", TIMEOUT.as_secs()),
            RunError::Failed(e) => write!(f, "chktex failed: {e}"),
        }
    }
}

/// The arguments of a run whose warnings name the file `name` (the text comes on stdin).
pub fn args(name: &str) -> Vec<OsString> {
    [
        "-q",
        "-v0",
        "-I0",
        "-S",
        "TabSize=1",
        "-p",
        name,
        "-f",
        FORMAT,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

/// Runs `program` (normally `chktex`) on `text` in the working directory `cwd` (where a project's
/// `.chktexrc` is found), naming the file `name` in its output. Blocking: run it off the main
/// thread. Returns every warning, unfiltered, sorted by line and column.
pub fn run(
    program: &Path,
    text: &str,
    cwd: &Path,
    name: &str,
) -> Result<Vec<LintWarning>, RunError> {
    run_command(command(program, cwd, name), text)
}

/// The command [`run`] runs (tests change its environment, e.g. `HOME`, so a user's
/// `~/.chktexrc` does not change their results).
pub fn command(program: &Path, cwd: &Path, name: &str) -> Command {
    let mut command = Command::new(program);
    command.args(args(name)).current_dir(cwd);
    command
}

/// [`run`] with a prepared [`command`].
pub fn run_command(mut command: Command, text: &str) -> Result<Vec<LintWarning>, RunError> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => RunError::NotFound,
            _ => RunError::Failed(e.to_string()),
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = text.as_bytes().to_vec();
    // chktex reads everything before it reports, so the text is written from its own thread
    // while this one reads (a large file would otherwise fill both pipes).
    let writer = thread::spawn(move || {
        // A chktex that exits early closes the pipe: not an error of ours.
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout
            .by_ref()
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut out);
        // Drain the rest so chktex never blocks on a full pipe.
        let _ = io::copy(&mut stdout, &mut io::sink());
        out
    });
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = thread::spawn(move || {
        let mut err = Vec::new();
        let _ = stderr.by_ref().take(64 << 10).read_to_end(&mut err);
        let _ = io::copy(&mut stderr, &mut io::sink());
        err
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = writer.join();
                let _ = reader.join();
                let _ = errors.join();
                return Err(RunError::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(RunError::Failed(e.to_string())),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = errors.join().unwrap_or_default();
    let out = String::from_utf8_lossy(&out);
    let warnings = parse(&out);
    // 0: nothing found, 2: warnings, 3: errors. Anything else without output is a failure
    // (an unreadable chktexrc, a crash).
    if warnings.is_empty() && !matches!(status.code(), Some(0 | 2 | 3)) {
        let err = String::from_utf8_lossy(&err);
        let detail = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(RunError::Failed(
            format!("{status} {detail}").trim().to_owned(),
        ));
    }
    Ok(warnings)
}

/// Parses chktex output written with [`FORMAT`]: one warning per line; other lines are ignored.
/// The warnings come back sorted by line, column and number (chktex reports some passes out of
/// order), without exact duplicates.
pub fn parse(output: &str) -> Vec<LintWarning> {
    let mut warnings: Vec<LintWarning> = output.lines().filter_map(parse_line).collect();
    warnings.sort_by_key(|w| (w.line, w.column, w.number));
    warnings.dedup();
    warnings
}

fn parse_line(line: &str) -> Option<LintWarning> {
    let mut fields = line.splitn(6, ':');
    let line_no: u32 = fields.next()?.trim().parse().ok()?;
    let column: u32 = fields.next()?.trim().parse().ok()?;
    let length: u32 = fields.next()?.trim().parse().ok()?;
    let number: u32 = fields.next()?.trim().parse().ok()?;
    let kind = match fields.next()?.trim() {
        "Warning" => LintKind::Warning,
        "Error" => LintKind::Error,
        "Message" => LintKind::Message,
        _ => return None,
    };
    let message = fields.next()?.trim().to_owned();
    (line_no > 0).then_some(LintWarning {
        line: line_no,
        column,
        length,
        number,
        kind,
        message,
    })
}

/// Whether warning `number` is shown: not in `disabled`, and either not silenced by default or
/// listed in `enabled`. `disabled` wins over `enabled`.
pub fn is_shown(number: u32, disabled: &[u32], enabled: &[u32]) -> bool {
    !disabled.contains(&number)
        && (!DEFAULT_SILENCED.contains(&number) || enabled.contains(&number))
}

/// The warning as a Problems entry of the file `path`: `DiagSource::Lint`, severity `Warning`
/// (`Info` for chktex's messages), the message prefixed with nothing (the number goes into the
/// context, which the Problems row shows as "chktex 18").
pub fn to_diagnostic(warning: &LintWarning, path: &Path) -> Diagnostic {
    Diagnostic {
        path: PathBuf::from(path),
        line: Some(warning.line),
        severity: match warning.kind {
            LintKind::Message => Severity::Info,
            LintKind::Warning | LintKind::Error => Severity::Warning,
        },
        message: warning.message.clone(),
        context: Some(code_label(warning.number)),
        source: DiagSource::Lint,
    }
}

/// "chktex 18": how a warning number is shown.
pub fn code_label(number: u32) -> String {
    format!("chktex {number}")
}

/// The warning number of a lint diagnostic made by [`to_diagnostic`] (from its context).
pub fn code_of(context: Option<&str>) -> Option<u32> {
    context?.strip_prefix("chktex ")?.trim().parse().ok()
}

/// The edit that silences warning `number` on the line `line_text` (without its line break),
/// which starts at byte `line_start` of the document: ` chktex N` appended to a comment that
/// already mentions chktex, else ` % chktex N` at the end of the line (chktex reads only the
/// first number after each `chktex`, so several warnings need several mentions). `None` when the
/// line already silences that number.
pub fn suppress_on_line(line_text: &str, line_start: usize, number: u32) -> Option<TextEdit> {
    let content = line_text.strip_suffix('\r').unwrap_or(line_text);
    let comment = comment_start(content);
    if let Some(at) = comment {
        let already = content[at..]
            .match_indices("chktex")
            .filter_map(|(i, m)| {
                let rest = content[at + i + m.len()..].trim_start();
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().ok()
            })
            .any(|n| n == number);
        if already {
            return None;
        }
    }
    let trimmed_end = content.trim_end_matches([' ', '\t']).len();
    let mentions_chktex = comment.is_some_and(|at| content[at..].contains("chktex"));
    let text = if mentions_chktex {
        format!(" chktex {number}")
    } else if comment.is_some() {
        format!(" % chktex {number}")
    } else if trimmed_end == 0 {
        format!("% chktex {number}")
    } else {
        format!(" % chktex {number}")
    };
    Some(TextEdit {
        range: line_start + trimmed_end..line_start + content.len(),
        text,
    })
}

/// The byte offset of the `%` that starts a comment in `line`, if any (`\%` is a percent sign;
/// `\\%` is a line break followed by a comment).
pub fn comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut backslashes = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\\' => backslashes += 1,
            b'%' if backslashes.is_multiple_of(2) => return Some(i),
            _ => backslashes = 0,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_format() {
        let out = "3:13:1:12:Warning:Interword spacing (`\\ ') should perhaps be used.\n\
                   3:11:1:26:Warning:You ought to remove spaces in front of punctuation.\n\
                   junk line\n\
                   2:1:4:9:Error:`x' expected, found `y'.\n\
                   9:1:0:17:Warning:Number of `(' doesn't match the number of `)'!\n";
        let w = parse(out);
        assert_eq!(
            w.iter()
                .map(|w| (w.line, w.column, w.length, w.number, w.kind))
                .collect::<Vec<_>>(),
            [
                (2, 1, 4, 9, LintKind::Error),
                (3, 11, 1, 26, LintKind::Warning),
                (3, 13, 1, 12, LintKind::Warning),
                (9, 1, 0, 17, LintKind::Warning),
            ]
        );
        assert_eq!(w[0].message, "`x' expected, found `y'.");
        assert_eq!(
            w[2].message,
            "Interword spacing (`\\ ') should perhaps be used."
        );
        assert!(w[3].is_file_level());
    }

    #[test]
    fn ranges_are_clamped_to_characters() {
        let w = |column, length| LintWarning {
            line: 1,
            column,
            length,
            number: 1,
            kind: LintKind::Warning,
            message: String::new(),
        };
        let line = "Ünï (a)x";
        assert_eq!(w(10, 1).byte_range(line), Some(9..10));
        assert_eq!(w(2, 1).byte_range(line), Some(0..2));
        assert_eq!(w(40, 3).byte_range(line), Some(10..10));
        assert_eq!(w(1, 0).byte_range(line), None);
    }

    #[test]
    fn filters() {
        assert!(!is_shown(18, &[], &[]));
        assert!(is_shown(18, &[], &[18]));
        assert!(!is_shown(18, &[18], &[18]));
        assert!(is_shown(2, &[], &[]));
        assert!(!is_shown(2, &[2], &[]));
    }

    #[test]
    fn suppression_comments() {
        let edit = |line: &str, n| {
            suppress_on_line(line, 100, n).map(|e| {
                let mut text = line.to_owned();
                TextEdit {
                    range: e.range.start - 100..e.range.end - 100,
                    text: e.text,
                }
                .apply(&mut text);
                text
            })
        };
        assert_eq!(
            edit("a \"x\" b", 18).as_deref(),
            Some("a \"x\" b % chktex 18")
        );
        assert_eq!(
            edit("a \"x\" b   ", 18).as_deref(),
            Some("a \"x\" b % chktex 18")
        );
        assert_eq!(
            edit("a \"x\" b % note", 18).as_deref(),
            Some("a \"x\" b % note % chktex 18")
        );
        assert_eq!(
            edit("a \"x\" b % chktex 3", 18).as_deref(),
            Some("a \"x\" b % chktex 3 chktex 18")
        );
        assert_eq!(edit("a \"x\" b % chktex 18", 18), None);
        assert_eq!(edit("a % chktex 3 chktex 18", 18), None);
        assert_eq!(
            edit("50\\% of \"x\"", 18).as_deref(),
            Some("50\\% of \"x\" % chktex 18")
        );
        assert_eq!(
            edit("a\\\\% c", 18).as_deref(),
            Some("a\\\\% c % chktex 18")
        );
        assert_eq!(
            edit("a \"x\"\r", 18).as_deref(),
            Some("a \"x\" % chktex 18\r")
        );
        assert_eq!(edit("", 18).as_deref(), Some("% chktex 18"));
    }

    #[test]
    fn diagnostics_and_codes() {
        let w = LintWarning {
            line: 4,
            column: 2,
            length: 1,
            number: 26,
            kind: LintKind::Warning,
            message: "You ought to remove spaces in front of punctuation.".into(),
        };
        let d = to_diagnostic(&w, Path::new("/p/main.tex"));
        assert_eq!(d.source, DiagSource::Lint);
        assert_eq!(d.line, Some(4));
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(code_of(d.context.as_deref()), Some(26));
        assert_eq!(code_of(Some("in x.sty")), None);
    }
}
