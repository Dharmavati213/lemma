//! TeX `.log` parsing for pdfTeX, LuaTeX and XeTeX logs written with `-file-line-error`.
//!
//! The regexes were checked against real logs of all three engines (`tests/fixtures/logs`).

use crate::paths::{resolve, with_tex_extension};
use lemma_core::{DiagSource, Diagnostic, Severity};
use regex::{Captures, Regex};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What one TeX run's `.log` says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogReport {
    /// Errors, warnings and bad boxes in log order, without exact duplicates.
    pub diagnostics: Vec<Diagnostic>,
    /// The log asks for another pass (`Rerun to get …`, `Label(s) may have changed`,
    /// `Please (re)run Biber`, …).
    pub rerun: bool,
    /// `Output written on <file> (<n> pages, …)`: the output file (resolved against the working
    /// directory; `.xdv` for XeTeX) and its page count.
    pub output: Option<(PathBuf, u32)>,
    /// TeX stopped early: an emergency stop or `==> Fatal error occurred`.
    pub fatal: bool,
}

/// TeX's default `max_print_line`. Logs wrapped at this width are unwrapped before parsing.
const DEFAULT_WRAP: usize = 79;
/// How many lines after an error header may hold its `l.<n>` context.
const ERROR_LOOKAHEAD: usize = 12;
/// How many lines after an engine warning may hold its `l.<n>` line.
const WARNING_LOOKAHEAD: usize = 5;

struct Patterns {
    /// `./main.tex:66: Undefined control sequence.`
    file_line_error: Regex,
    /// `l.66 is an \equivalnce`
    line_number: Regex,
    /// `LaTeX Warning: …`, `Package hyperref Warning: …`, `Class amsart Warning: …`,
    /// `LaTeX Font Warning: …`, `Module x Warning: …`.
    warning: Regex,
    /// `(hyperref)                removing …`: a continuation line of a package message.
    continuation: Regex,
    input_line: Regex,
    /// `Overfull \hbox (136.78185pt too wide) in paragraph at lines 33--35`
    badbox: Regex,
    runaway: Regex,
    /// `*** (job aborted, no legal \end found)`
    emergency_reason: Regex,
    output: Regex,
    /// `pdfTeX warning (ext4): …`, possibly in the middle of a line.
    engine_warning: Regex,
    /// LuaTeX's backend: `warning  (pdf backend): …`
    backend_warning: Regex,
    package_error: Regex,
    rerun: Regex,
    /// End-of-run summaries that repeat the individual warnings.
    summary: Regex,
}

impl Patterns {
    fn new() -> Self {
        let re = |pattern: &str| Regex::new(pattern).expect("valid regex");
        Patterns {
            file_line_error: re(
                r"^(?P<file>(?:\.{1,2}/|/)[^:]*|[^\s:/(][^:]*\.(?:tex|sty|cls|ltx|def|cfg|clo|fd|bbl|aux|toc|lof|lot|out|nav|snm|vrb|bib|lbx|cbx|bbx|dbx|ldf|tikz|pgf|lua)):(?P<line>\d+): (?P<msg>.*)$",
            ),
            line_number: re(r"^l\.(?P<line>\d+) (?P<pre>.*)$"),
            warning: re(
                r"^(?:(?P<cls>Package|Class|Module) (?P<name>\S+)|LaTeX3?(?: [A-Za-z]+)?) (?P<kind>[Ww]arning|Error): (?P<msg>.*)$",
            ),
            continuation: re(r"^\((?P<name>[A-Za-z0-9@_.-]+)\)\s+(?P<msg>.*)$"),
            input_line: re(r"on input line (?P<line>\d+)"),
            badbox: re(
                r"^(?:Over|Under)full \\[hv]box \((?:badness \d+|-?[\d.]+pt too (?:wide|high|deep))\) (?:in paragraph at lines (?P<a>\d+)--\d+|in alignment at lines (?P<c>\d+)--\d+|detected at line (?P<e>\d+)|has occurred while \\output is active)",
            ),
            runaway: re(r"^Runaway (?P<what>argument|definition|preamble|text)\?$"),
            emergency_reason: re(r"^\*\*\* \((?P<why>.+)\)$"),
            output: re(
                r"^Output written on (?P<file>.+?) \((?P<pages>\d+) pages?(?:, \d+ bytes)?\)\.$",
            ),
            engine_warning: re(
                r"(?P<engine>pdfTeX|LuaTeX|XeTeX) warning(?: \([^)]*\))?: (?P<msg>.*)$",
            ),
            backend_warning: re(r"^warning\s+\([^)]*\): (?P<msg>.*)$"),
            package_error: re(r"^(?P<cls>Package|Class|Module) (?P<name>\S+) Error: (?P<msg>.*)$"),
            rerun: re(
                r"Rerun to get|Rerun LaTeX|Label\(s\) may have changed|Please \(re\)run|Please rerun|Citation\(s\) may have changed|Table widths have changed",
            ),
            summary: re(
                r"^(?:There were (?:undefined references|undefined citations|multiply-defined labels)|Some font shapes were not available)",
            ),
        }
    }
}

static RE: LazyLock<Patterns> = LazyLock::new(Patterns::new);

/// Parses a TeX log. Built for `max_print_line=10000` logs; logs wrapped at TeX's default 79
/// columns are detected and unwrapped first.
///
/// `workdir` is TeX's working directory (the root document's directory). `exists` decides
/// whether a `(name` token in the log is a real file; it gets the name as written (relative to
/// `workdir`, or absolute) and is injectable for tests.
///
/// How the diagnostics are built:
/// - **Paths** are resolved against `workdir` and normalized. Messages without a `file:line:`
///   prefix get the innermost open file from the log's `(file … )` nesting, or the root document
///   when no file is open.
/// - **Errors** take their line from `file:line:` or the following `l.<n>`; `context` is
///   `<text before the error> <HERE> <text after>` from the `l.<n>` lines. `LaTeX Error: ` is
///   dropped from messages and `Package x Error: m` becomes `Package x: m`.
/// - A **missing file** (`! LaTeX Error: File … not found.`) takes its line from the
///   `Emergency stop` that follows. An emergency stop is only reported on its own when no other
///   error came before it.
/// - A **runaway argument** is merged into the error after it, with `context`
///   `Runaway argument? <text>`. TeX's own location, if any, is where it noticed the problem
///   (the end of the paragraph or of an `\input` file); [`locate_runaways`](crate::locate_runaways)
///   moves the error to where the runaway text starts in the sources.
/// - **Warnings** (`LaTeX`, `Package x`, `Class x`, `Module x`, `LaTeX Font`, engine warnings)
///   join their continuation lines and take the line from `on input line N`. Package and class
///   messages are prefixed `Package x: ` / `Class x: `.
/// - **Bad boxes** keep TeX's message and take the first line of `at lines A--B` or
///   `detected at line N`.
/// - Rerun requests set [`LogReport::rerun`] instead of producing diagnostics. The end-of-run
///   summaries `There were undefined references`, `There were multiply-defined labels` and
///   `Some font shapes were not available` repeat earlier warnings and are dropped.
pub fn parse_log(log: &str, workdir: &Path, exists: &dyn Fn(&Path) -> bool) -> LogReport {
    let lines = logical_lines(log);
    let mut parser = Parser::new(workdir, exists);
    let mut i = 0;
    while i < lines.len() {
        i = parser.step(&lines, i);
    }
    parser.finish()
}

/// The log's lines; a log wrapped at 79 columns has its wrapped lines joined back together.
fn logical_lines(log: &str) -> Vec<Cow<'_, str>> {
    let raw: Vec<&str> = log.lines().collect();
    if !is_wrapped(&raw) {
        return raw.into_iter().map(Cow::Borrowed).collect();
    }
    let mut out: Vec<Cow<'_, str>> = Vec::with_capacity(raw.len());
    let mut it = raw.into_iter();
    while let Some(first) = it.next() {
        // The second line of TeX's error context is cut at `error_line` (79 by default) and is
        // never wrapped, even when it is exactly 79 characters long.
        let after_context = out.last().is_some_and(|l| is_context_first_line(l));
        if !fills_wrap_width(first) || after_context {
            out.push(Cow::Borrowed(first));
            continue;
        }
        let mut joined = first.to_owned();
        let mut last = first;
        while fills_wrap_width(last) {
            let Some(next) = it.next() else {
                break;
            };
            joined.push_str(next);
            last = next;
        }
        out.push(Cow::Owned(joined));
    }
    out
}

/// TeX breaks a line after exactly `max_print_line` characters. A wrapped log has many lines of
/// exactly 79 characters and almost none longer (only the banner and the memory statistics).
fn is_wrapped(lines: &[&str]) -> bool {
    let body = lines.get(1..).unwrap_or_default();
    let full = body.iter().filter(|l| fills_wrap_width(l)).count();
    let long = body.iter().filter(|l| l.len() > DEFAULT_WRAP + 1).count();
    full >= 3 && long * 4 <= full
}

/// pdfTeX counts bytes; LuaTeX and XeTeX may count characters.
fn fills_wrap_width(line: &str) -> bool {
    line.len() == DEFAULT_WRAP || line.chars().count() == DEFAULT_WRAP
}

/// The first line of a TeX error context: `l.110 Exactness \alpha` or `<inserted text> …`.
fn is_context_first_line(line: &str) -> bool {
    let numbered = line
        .strip_prefix("l.")
        .and_then(|rest| rest.split_once(' '))
        .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    numbered || (line.starts_with('<') && line.contains("> "))
}

/// An error's first line: `file:line: message` or `! message`.
struct ErrorHeader<'l> {
    file: Option<&'l str>,
    line: Option<u32>,
    message: &'l str,
}

impl<'l> ErrorHeader<'l> {
    fn parse(line: &'l str) -> Option<Self> {
        if let Some(message) = line.strip_prefix("! ") {
            return Some(ErrorHeader {
                file: None,
                line: None,
                message,
            });
        }
        if !line.contains(": ") {
            return None;
        }
        let c = RE.file_line_error.captures(line)?;
        Some(ErrorHeader {
            file: Some(c.name("file")?.as_str()),
            line: c["line"].parse().ok(),
            message: c.name("msg")?.as_str(),
        })
    }
}

struct Parser<'a> {
    workdir: &'a Path,
    exists: &'a dyn Fn(&Path) -> bool,
    /// Distinct files seen in the log; the stack refers to them by index.
    files: Vec<PathBuf>,
    /// `(token` → file index, or `None` if the token is not a file.
    known: HashMap<String, Option<usize>>,
    /// Open `(`s: a file, or `None` for a parenthesis that is not a file.
    stack: Vec<Option<usize>>,
    /// The first file opened at the top level: the root document.
    root: Option<usize>,
    /// The job named on the `**` line, for logs that never opened a file.
    job: Option<PathBuf>,
    report: LogReport,
    /// `Runaway argument? <text>` waiting for the error on the next line.
    runaway: Option<String>,
    /// Index of the last diagnostic if it is an error without a line (an emergency stop right
    /// after it may supply one).
    unlocated_error: Option<usize>,
    saw_error: bool,
}

impl<'a> Parser<'a> {
    fn new(workdir: &'a Path, exists: &'a dyn Fn(&Path) -> bool) -> Self {
        Parser {
            workdir,
            exists,
            files: Vec::new(),
            known: HashMap::new(),
            stack: Vec::new(),
            root: None,
            job: None,
            report: LogReport::default(),
            runaway: None,
            unlocated_error: None,
            saw_error: false,
        }
    }

    /// Handles the message starting at `lines[i]`; returns the index of the next line to read.
    fn step(&mut self, lines: &[Cow<'_, str>], i: usize) -> usize {
        let line = lines[i].as_ref();
        if let Some(header) = ErrorHeader::parse(line) {
            return self.error(lines, i, &header);
        }
        if let Some(c) = RE.runaway.captures(line) {
            return self.runaway(lines, i, &c["what"]);
        }
        if let Some(c) = RE.warning.captures(line) {
            return self.warning(lines, i, &c);
        }
        if let Some(c) = RE.badbox.captures(line) {
            return self.badbox(lines, i, &c);
        }
        if line.contains("warning") {
            let found = RE
                .engine_warning
                .captures(line)
                .map(|c| {
                    (
                        c.get(0).map_or(0, |m| m.start()),
                        c["engine"].to_owned(),
                        c["msg"].to_owned(),
                    )
                })
                .or_else(|| {
                    RE.backend_warning
                        .captures(line)
                        .map(|c| (0, "LuaTeX".to_owned(), c["msg"].to_owned()))
                });
            if let Some((start, engine, msg)) = found {
                self.track_files(&line[..start]);
                return self.engine_warning(lines, i, &engine, &msg);
            }
        }
        if let Some(c) = RE.output.captures(line) {
            let file = c["file"].trim_matches('"');
            let pages = c["pages"].parse().unwrap_or(0);
            self.report.output = Some((resolve(self.workdir, Path::new(file)), pages));
            return i + 1;
        }
        if RE.emergency_reason.is_match(line) || line.contains("==> Fatal error occurred") {
            self.report.fatal = true;
            return i + 1;
        }
        if line == "No pages of output." {
            let path = self.current_file();
            self.push(Severity::Warning, path, None, line.to_owned(), None);
            return i + 1;
        }
        if self.job.is_none()
            && let Some(command) = line.strip_prefix("**")
        {
            self.job = job_file(self.workdir, command);
        }
        self.track_files(line);
        i + 1
    }

    fn error(&mut self, lines: &[Cow<'_, str>], i: usize, header: &ErrorHeader<'_>) -> usize {
        let mut message = header.message.trim().to_owned();
        if message.starts_with("==> Fatal error occurred") {
            self.report.fatal = true;
            return i + 1;
        }
        let mut j = i + 1;
        while let Some(c) = lines.get(j).and_then(|t| RE.continuation.captures(t)) {
            push_words(&mut message, &c["msg"]);
            j += 1;
        }
        let mut line = header.line;
        let mut context = None;
        let mut reason = None;
        let mut next_header = None;
        let mut context_line = None;
        let end = (j + ERROR_LOOKAHEAD).min(lines.len());
        while j < end {
            let t = lines[j].as_ref();
            if ErrorHeader::parse(t).is_some() {
                next_header = Some(j);
                break;
            }
            if let Some(c) = RE.line_number.captures(t) {
                if line.is_none() {
                    line = c["line"].parse().ok();
                }
                context = Some(tex_context(&c["pre"], lines.get(j + 1).map(AsRef::as_ref)));
                context_line = Some(j);
                break;
            }
            if let Some(c) = RE.emergency_reason.captures(t) {
                reason = Some(c["why"].to_owned());
            }
            j += 1;
        }
        let next = match (next_header, context_line) {
            (Some(h), _) => h,
            (None, Some(k)) => skip_block(lines, k),
            (None, None) => skip_block(lines, i + 1),
        };
        let path = match header.file {
            Some(file) => resolve(self.workdir, Path::new(file)),
            None => self.current_file(),
        };
        let runaway = self.runaway.take();
        if message == "Emergency stop." {
            self.emergency_stop(path, line, context, reason);
            return next;
        }
        let index = self.report.diagnostics.len();
        self.push(
            Severity::Error,
            path,
            line,
            error_message(message),
            runaway.or(context),
        );
        self.unlocated_error = line.is_none().then_some(index);
        next
    }

    /// An emergency stop usually just ends the run after another error. It locates that error
    /// when TeX gave none (a missing `\input` file), and is reported only when it is the first.
    fn emergency_stop(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        context: Option<String>,
        reason: Option<String>,
    ) {
        self.report.fatal = true;
        if let Some(k) = self.unlocated_error.take() {
            let d = &mut self.report.diagnostics[k];
            if line.is_some() {
                d.path = path;
                d.line = line;
                if d.context.is_none() {
                    d.context = context;
                }
            }
            return;
        }
        if self.saw_error {
            return;
        }
        let message = match reason {
            Some(why) => format!("Emergency stop ({why})."),
            None => "Emergency stop.".to_owned(),
        };
        self.push(Severity::Error, path, line, message, context);
    }

    /// `Runaway argument?` is followed by the text TeX had read and then by the error itself.
    fn runaway(&mut self, lines: &[Cow<'_, str>], i: usize, what: &str) -> usize {
        let text = lines.get(i + 1).map_or("", |t| t.trim_end());
        let runaway = format!("Runaway {what}? {text}");
        let error_follows = lines
            .get(i + 2)
            .is_some_and(|t| ErrorHeader::parse(t).is_some());
        if error_follows {
            self.runaway = Some(runaway);
        } else {
            let path = self.current_file();
            let message = format!("Runaway {what}?");
            self.push(Severity::Error, path, None, message, Some(runaway));
        }
        (i + 2).min(lines.len())
    }

    fn warning(&mut self, lines: &[Cow<'_, str>], i: usize, c: &Captures<'_>) -> usize {
        let mut text = c["msg"].trim().to_owned();
        let mut k = i + 1;
        while let Some(t) = lines.get(k).map(AsRef::as_ref) {
            if t.trim().is_empty() {
                break;
            }
            if let Some(cc) = RE.continuation.captures(t) {
                push_words(&mut text, &cc["msg"]);
            } else if t.starts_with([' ', '\t']) && !t.trim_start().starts_with('(') {
                push_words(&mut text, t);
            } else {
                break;
            }
            k += 1;
        }
        if RE.rerun.is_match(&text) {
            self.report.rerun = true;
            return k;
        }
        if RE.summary.is_match(&text) {
            return k;
        }
        let line = RE
            .input_line
            .captures(&text)
            .and_then(|m| m["line"].parse().ok());
        let message = match (c.name("cls"), c.name("name")) {
            (Some(cls), Some(name)) => format!("{} {}: {text}", cls.as_str(), name.as_str()),
            _ => text,
        };
        let severity = if c["kind"].eq_ignore_ascii_case("error") {
            Severity::Error
        } else {
            Severity::Warning
        };
        let path = self.current_file();
        self.push(severity, path, line, message, None);
        k
    }

    fn badbox(&mut self, lines: &[Cow<'_, str>], i: usize, c: &Captures<'_>) -> usize {
        let line = ["a", "c", "e"]
            .iter()
            .find_map(|n| c.name(n))
            .and_then(|m| m.as_str().parse().ok());
        let message = c.get(0).map_or("", |m| m.as_str()).to_owned();
        let path = self.current_file();
        self.push(Severity::BadBox, path, line, message, None);
        // The box contents that follow may contain unbalanced parentheses.
        skip_block(lines, i + 1)
    }

    /// `pdfTeX warning (ext4): …` may be followed by TeX context with an `l.<n>` line.
    fn engine_warning(
        &mut self,
        lines: &[Cow<'_, str>],
        i: usize,
        engine: &str,
        msg: &str,
    ) -> usize {
        let mut line = None;
        let mut next = i + 1;
        let end = (i + 1 + WARNING_LOOKAHEAD).min(lines.len());
        for j in i + 1..end {
            let t = lines[j].as_ref();
            if t.trim().is_empty() || ErrorHeader::parse(t).is_some() {
                break;
            }
            if let Some(c) = RE.line_number.captures(t) {
                line = c["line"].parse().ok();
                next = (j + 2).min(lines.len());
                break;
            }
        }
        let path = self.current_file();
        let message = format!("{engine}: {}", msg.trim());
        self.push(Severity::Warning, path, line, message, None);
        next
    }

    fn push(
        &mut self,
        severity: Severity,
        path: PathBuf,
        line: Option<u32>,
        message: String,
        context: Option<String>,
    ) {
        if severity == Severity::Error {
            self.saw_error = true;
        }
        self.unlocated_error = None;
        self.report.diagnostics.push(Diagnostic {
            path,
            line,
            severity,
            message,
            context,
            source: DiagSource::Tex,
        });
    }

    /// Follows `(file` and `)` on a line of ordinary log output.
    fn track_files(&mut self, line: &str) {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => {
                    let (token, len) = file_token(&line[i + 1..]);
                    if let Some(id) = self.file_id(token) {
                        if self.stack.is_empty() && self.root.is_none() {
                            self.root = Some(id);
                        }
                        self.stack.push(Some(id));
                        i += 1 + len;
                        continue;
                    }
                    self.stack.push(None);
                }
                b')' => {
                    self.stack.pop();
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn file_id(&mut self, token: &str) -> Option<usize> {
        if token.is_empty() || !token.contains(['.', '/']) {
            return None;
        }
        if let Some(&id) = self.known.get(token) {
            return id;
        }
        let id = (self.exists)(Path::new(token)).then(|| {
            self.files.push(resolve(self.workdir, Path::new(token)));
            self.files.len() - 1
        });
        self.known.insert(token.to_owned(), id);
        id
    }

    /// The innermost open file, else the root document.
    fn current_file(&self) -> PathBuf {
        self.stack
            .iter()
            .rev()
            .find_map(|f| *f)
            .or(self.root)
            .map(|id| self.files[id].clone())
            .or_else(|| self.job.clone())
            .unwrap_or_default()
    }

    fn finish(mut self) -> LogReport {
        let mut seen = HashSet::new();
        self.report.diagnostics.retain(|d| {
            seen.insert((
                d.path.clone(),
                d.line,
                d.severity,
                d.message.clone(),
                d.context.clone(),
            ))
        });
        self.report
    }
}

/// The name after `(`: up to a delimiter, or between quotes for names with spaces. Returns the
/// name and how many bytes it spans.
fn file_token(rest: &str) -> (&str, usize) {
    if let Some(quoted) = rest.strip_prefix('"') {
        return match quoted.find('"') {
            Some(end) => (&quoted[..end], end + 2),
            None => (quoted, rest.len()),
        };
    }
    let end = rest
        .find([')', '(', ' ', '[', '{', '"', '<'])
        .unwrap_or(rest.len());
    (&rest[..end], end)
}

/// The file named on the `**` command line (`**main.tex`, `**"my file.tex"`, `**\input x`).
fn job_file(workdir: &Path, command: &str) -> Option<PathBuf> {
    let command = command.trim();
    let name = match command.strip_suffix('"') {
        Some(quoted) => quoted.rsplit_once('"')?.1,
        None => command.split_whitespace().last()?,
    };
    (!name.is_empty() && !name.starts_with(['&', '\\']))
        .then(|| resolve(workdir, &with_tex_extension(PathBuf::from(name))))
}

/// `pre <HERE> after` from the two `l.<n>` lines. `after` is the second line without its
/// indentation; a trailing `^^M` (TeX's end-of-line character) is dropped.
fn tex_context(pre: &str, after_line: Option<&str>) -> String {
    let after = after_line
        .filter(|t| t.is_empty() || t.starts_with([' ', '\t']))
        .map_or("", str::trim);
    let after = after.strip_suffix("^^M").unwrap_or(after).trim_end();
    let context = format!("{pre} <HERE> {after}");
    context.trim_end().to_owned()
}

/// The first blank line at or after `from`: where a message block ends.
fn skip_block(lines: &[Cow<'_, str>], from: usize) -> usize {
    (from..lines.len())
        .find(|&k| lines[k].trim().is_empty())
        .unwrap_or(lines.len())
}

/// Appends a continuation line to a message, separated by one space.
fn push_words(message: &mut String, more: &str) {
    let more = more.trim();
    if more.is_empty() {
        return;
    }
    if !message.is_empty() {
        message.push(' ');
    }
    message.push_str(more);
}

/// Drops the `LaTeX Error: ` prefix; `Package x Error: m` becomes `Package x: m`.
fn error_message(message: String) -> String {
    if let Some(rest) = message.strip_prefix("LaTeX Error: ") {
        return rest.to_owned();
    }
    if let Some(c) = RE.package_error.captures(&message) {
        return format!("{} {}: {}", &c["cls"], &c["name"], &c["msg"]);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(log: &str) -> LogReport {
        parse_log(log, Path::new("/w"), &|p: &Path| {
            p.extension()
                .is_some_and(|e| e == "tex" || e == "sty" || e == "aux")
        })
    }

    fn located(r: &LogReport) -> Vec<(String, Option<u32>, Severity, &str)> {
        r.diagnostics
            .iter()
            .map(|d| {
                (
                    // `/` on Windows too, where the expected paths below are built with `\`.
                    d.path.to_string_lossy().replace('\\', "/"),
                    d.line,
                    d.severity,
                    d.message.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn file_stack_attributes_warnings() {
        let log = "(./main.tex (/usr/share/x.sty (3.1.11a) )\n\
                   (./ch/a.tex\n\nLaTeX Warning: Citation `x' on page 1 undefined on input line 3.\n\n)\n\n\
                   LaTeX Warning: Reference `y' on page 1 undefined on input line 9.\n\n)\n";
        assert_eq!(
            located(&parse(log)),
            [
                (
                    "/w/ch/a.tex".into(),
                    Some(3),
                    Severity::Warning,
                    "Citation `x' on page 1 undefined on input line 3."
                ),
                (
                    "/w/main.tex".into(),
                    Some(9),
                    Severity::Warning,
                    "Reference `y' on page 1 undefined on input line 9."
                ),
            ]
        );
    }

    /// Windows: MiKTeX quotes the paths of its own files, with backslashes in them; TeX Live
    /// writes them with a drive and forward slashes.
    #[test]
    fn windows_file_names() {
        let log = "(./main.tex (\"C:\\Program Files\\MiKTeX\\tex/latex/base\\article.cls\" \
                   (C:/texlive/2026/texmf-dist/tex/latex/base/size10.sty))\n\
                   (./ch/a.tex\n\nLaTeX Warning: Citation `x' on page 1 undefined on input line 3.\n\n)\n\n\
                   LaTeX Warning: Reference `y' on page 1 undefined on input line 9.\n\n)\n";
        assert_eq!(
            located(&parse(log)),
            [
                (
                    "/w/ch/a.tex".to_owned(),
                    Some(3),
                    Severity::Warning,
                    "Citation `x' on page 1 undefined on input line 3."
                ),
                (
                    "/w/main.tex".to_owned(),
                    Some(9),
                    Severity::Warning,
                    "Reference `y' on page 1 undefined on input line 9."
                ),
            ]
        );
    }

    #[test]
    fn quoted_file_names_with_spaces() {
        let log =
            "(\"./my file.tex\" (./a (b).tex\n\nLaTeX Warning: W1.\n\n)\nLaTeX Warning: W2.\n\n";
        let r = parse(log);
        assert_eq!(r.diagnostics[0].path, Path::new("/w/my file.tex"));
        assert_eq!(r.diagnostics[1].path, Path::new("/w/my file.tex"));
    }

    #[test]
    fn error_context_and_help_block() {
        let log = "(./main.tex\n./main.tex:66: Undefined control sequence.\nl.66 is an \\equivalnce\n\
                   \x20                      of categories.\nThe control sequence (e.g., `\\hobx'\n\n)\n";
        let r = parse(log);
        let d = &r.diagnostics[0];
        assert_eq!(
            (d.line, d.message.as_str()),
            (Some(66), "Undefined control sequence.")
        );
        assert_eq!(
            d.context.as_deref(),
            Some("is an \\equivalnce <HERE> of categories.")
        );
        assert!(!r.fatal);
    }

    #[test]
    fn runaway_context_wins_over_tex_context() {
        let log = "(./main.tex (./sec.tex)\nRunaway argument?\n{unclosed text \\par until the end. \n\
                   ./main.tex:4: File ended while scanning use of \\emph .\n<inserted text> \n                \\par \n\
                   l.4 \\input{sec}\n               \nI suspect you have forgotten a `}', causing me\n\n)\n";
        let r = parse(log);
        let d = &r.diagnostics[0];
        assert_eq!(
            (d.line, d.message.as_str()),
            (Some(4), "File ended while scanning use of \\emph .")
        );
        assert_eq!(
            d.context.as_deref(),
            Some("Runaway argument? {unclosed text \\par until the end.")
        );
        assert_eq!(r.diagnostics.len(), 1);
    }

    #[test]
    fn package_error_with_continuation() {
        let log = "(./main.tex\n./main.tex:5: Package babel Error: Unknown option `x'. Either you misspelled it\n\
                   (babel)                or the language definition file x.ldf was not found.\n\n\
                   See the babel package documentation for explanation.\nl.5 \\begin{document}\n\n)\n";
        let r = parse(log);
        assert_eq!(
            r.diagnostics[0].message,
            "Package babel: Unknown option `x'. Either you misspelled it or the language definition file x.ldf was not found."
        );
        assert_eq!(r.diagnostics[0].line, Some(5));
    }

    #[test]
    fn standalone_emergency_stop_and_no_output() {
        let log = "**doc\n! Emergency stop.\n<*> doc\n\n*** (job aborted, no legal \\end found)\n\nNo pages of output.\n";
        let r = parse(log);
        assert!(r.fatal);
        assert_eq!(
            located(&r),
            [
                (
                    "/w/doc.tex".into(),
                    None,
                    Severity::Error,
                    "Emergency stop (job aborted, no legal \\end found)."
                ),
                (
                    "/w/doc.tex".into(),
                    None,
                    Severity::Warning,
                    "No pages of output."
                ),
            ]
        );
    }

    #[test]
    fn engine_warning_mid_line_with_line_number() {
        let log = "(./main.tex [1{/var/lib/pdftex.map}pdfTeX warning (ext4): destination with the same identifier (name{page.1}) has been already used, duplicate ignored\n\
                   <to be read again> \n                   \\relax \nl.18 \\end{document}\n                    ]\n\n\
                   warning  (pdf backend): ignoring duplicate destination with the name 'page.1'\n)\n";
        let r = parse(log);
        assert_eq!(
            located(&r),
            [
                (
                    "/w/main.tex".into(),
                    Some(18),
                    Severity::Warning,
                    "pdfTeX: destination with the same identifier (name{page.1}) has been already used, duplicate ignored"
                ),
                (
                    "/w/main.tex".into(),
                    None,
                    Severity::Warning,
                    "LuaTeX: ignoring duplicate destination with the name 'page.1'"
                ),
            ]
        );
    }

    #[test]
    fn rerun_and_summaries_are_not_diagnostics() {
        let log = "(./main.tex\n\nLaTeX Warning: Label(s) may have changed. Rerun to get cross-references right.\n\n\
                   LaTeX Warning: There were undefined references.\n\n\
                   Package biblatex Warning: The following entry could not be found\n(biblatex)                in the database:\n(biblatex)                Lurie\n(biblatex)                Please verify the spelling and rerun\n(biblatex)                LaTeX afterwards.\n\n)\n";
        let r = parse(log);
        assert!(r.rerun);
        assert_eq!(
            located(&r),
            [(
                "/w/main.tex".into(),
                None,
                Severity::Warning,
                "Package biblatex: The following entry could not be found in the database: Lurie Please verify the spelling and rerun LaTeX afterwards."
            )]
        );
        // "rerun LaTeX afterwards" in a hint is not a rerun request.
        let hint_only = log.replace(
            "Label(s) may have changed. Rerun to get cross-references right.",
            "x.",
        );
        assert!(!parse(&hint_only).rerun);
    }

    #[test]
    fn badbox_variants() {
        let log = "(./main.tex\nUnderfull \\vbox (badness 10000) has occurred while \\output is active []\n\n\
                   Overfull \\hbox (2.0pt too wide) in alignment at lines 7--9\n []\n\n)\n";
        assert_eq!(
            located(&parse(log)),
            [
                (
                    "/w/main.tex".into(),
                    None,
                    Severity::BadBox,
                    "Underfull \\vbox (badness 10000) has occurred while \\output is active"
                ),
                (
                    "/w/main.tex".into(),
                    Some(7),
                    Severity::BadBox,
                    "Overfull \\hbox (2.0pt too wide) in alignment at lines 7--9"
                ),
            ]
        );
    }

    #[test]
    fn exact_duplicates_are_dropped() {
        let warning = "\nUnderfull \\hbox (badness 10000) in paragraph at lines 5--5\n []\n\n";
        let r = parse(&format!("(./main.tex{warning}{warning})"));
        assert_eq!(r.diagnostics.len(), 1);
    }

    /// Writes `line` the way TeX does with `max_print_line=79`: a newline after every 79th
    /// character, so a line of exactly 79 characters is followed by an empty one.
    fn tex_wrap(line: &str) -> String {
        let chunks: Vec<&str> = line
            .as_bytes()
            .chunks(DEFAULT_WRAP)
            .map(|c| std::str::from_utf8(c).expect("ascii"))
            .collect();
        let mut out = chunks.join("\n");
        if !line.is_empty() && line.len().is_multiple_of(DEFAULT_WRAP) {
            out.push('\n');
        }
        out
    }

    #[test]
    fn unwraps_79_column_logs() {
        let exactly_79 = format!("({})", "x".repeat(77));
        let logical: [&str; 11] = [
            "This is pdfTeX, Version 3.141592653-2.6-1.40.29 (TeX Live 2026/Arch Linux) (preloaded format=pdflatex)",
            "(./main.tex (/usr/share/texmf-dist/tex/latex/some/long/path/to/a/package/file/named/thing.sty)",
            "(./chapters/a-very-long-chapter-file-name-that-goes-on-for-a-while/sec.tex",
            "",
            "Class amsart Warning: Abstract should precede \\maketitle in AMS document classes; reported on input line 32.",
            "",
            ")",
            &exactly_79,
            "LaTeX Warning: Reference `thm:nonexistent' on page 1 undefined on input line 44.",
            "",
            ")",
        ];
        // The banner line is never wrapped by TeX.
        let log: Vec<String> = logical
            .iter()
            .enumerate()
            .map(|(i, l)| if i == 0 { (*l).to_owned() } else { tex_wrap(l) })
            .collect();
        let log = log.join("\n");
        assert!(log.lines().any(|l| l == "."), "the test must split `44.`");
        let lines: Vec<String> = logical_lines(&log)
            .into_iter()
            .map(Cow::into_owned)
            .collect();
        assert_eq!(lines, logical);

        let r = parse(&log);
        assert_eq!(
            located(&r),
            [
                (
                    "/w/chapters/a-very-long-chapter-file-name-that-goes-on-for-a-while/sec.tex"
                        .into(),
                    Some(32),
                    Severity::Warning,
                    "Class amsart: Abstract should precede \\maketitle in AMS document classes; reported on input line 32."
                ),
                (
                    "/w/main.tex".into(),
                    Some(44),
                    Severity::Warning,
                    "Reference `thm:nonexistent' on page 1 undefined on input line 44."
                ),
            ]
        );
    }

    #[test]
    fn full_width_error_context_is_not_joined() {
        let filler = "y".repeat(DEFAULT_WRAP);
        let after = format!(
            "{}_n follows since filtered colimits commute with finite...",
            " ".repeat(22)
        );
        assert_eq!(after.len(), DEFAULT_WRAP);
        let log = format!(
            "banner\n{filler}\n\n{filler}\n\n{filler}\n\n(./main.tex\n./main.tex:110: Missing $ inserted.\n\
             l.110 Exactness \\alpha\n{after}\nI've inserted a begin-math/end-math symbol since I think\n\n)\n"
        );
        let r = parse(&log);
        assert_eq!(
            r.diagnostics[0].context.as_deref(),
            Some(
                "Exactness \\alpha <HERE> _n follows since filtered colimits commute with finite..."
            )
        );
    }

    #[test]
    fn unwrapped_logs_are_left_alone() {
        let log = format!("banner\n{}\nnext\n", "x".repeat(79));
        assert_eq!(logical_lines(&log).len(), 3);
    }

    #[test]
    fn output_line_variants() {
        for (line, file, pages) in [
            (
                "Output written on .build/main.pdf (5 pages, 223399 bytes).",
                "/w/.build/main.pdf",
                5,
            ),
            (
                "Output written on main.xdv (1 page, 9 bytes).",
                "/w/main.xdv",
                1,
            ),
            // XeTeX writing the PDF itself gives no size.
            (
                "Output written on w_xelatex.pdf (5 pages).",
                "/w/w_xelatex.pdf",
                5,
            ),
            (
                "Output written on \"my doc.pdf\" (2 pages, 10 bytes).",
                "/w/my doc.pdf",
                2,
            ),
        ] {
            assert_eq!(
                parse(line).output,
                Some((PathBuf::from(file), pages)),
                "{line}"
            );
        }
    }

    #[test]
    fn contexts() {
        assert_eq!(
            tex_context("\\input{x}", Some("         ^^M")),
            "\\input{x} <HERE>"
        );
        assert_eq!(
            tex_context("\\end{proof}", Some("           ")),
            "\\end{proof} <HERE>"
        );
        assert_eq!(tex_context("a", Some("Help text")), "a <HERE>");
        assert_eq!(tex_context("a", None), "a <HERE>");
    }

    #[test]
    fn job_names() {
        let w = Path::new("/w");
        assert_eq!(job_file(w, "main.tex"), Some(PathBuf::from("/w/main.tex")));
        assert_eq!(
            job_file(w, "\"my doc\""),
            Some(PathBuf::from("/w/my doc.tex"))
        );
        assert_eq!(job_file(w, "&pdflatex"), None);
    }
}
