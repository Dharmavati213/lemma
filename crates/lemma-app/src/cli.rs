//! Command-line options (a small hand-written parser; GApplication never sees the arguments).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Printed for `--help` and after a usage error.
pub const USAGE: &str = "\
Usage: lemma [[+LINE] FILE[:LINE[:COLUMN]]]... [OPTIONS]

FILE is a document or a project directory (`.` for the current one). Without it, Lemma
reopens the project and file it had open last. The first FILE decides the project (its
directory, or the directory itself); only the first can be a directory. The others open in
tabs after it, and the first document is shown. +LINE before a FILE, or :LINE after it, puts
the cursor on that line (:LINE:COLUMN, on that character), counted from 1.

Options:
  --snapshot PNG          render the window to PNG once the first frame is drawn
  --quit-after-ms N       quit N milliseconds after start (after the snapshot, if any)
  --mem-report            print /proc/self/smaps_rollup (Rss, Pss) before any snapshot
  --demo NAME             run a scripted demo: review, pdf, agent, completion, outline,
                          hover-ref, math-preview, pdf-search, palette, search, prefs,
                          pairs or templates
  --apply-as-agent JSON   apply the edits in JSON as if an agent made them
  --build                 build the document (as F5 does) and wait for the result
  --sidebar PAGE          show a sidebar page at the end: outline, changes, problems or
                          search
  --restore-session       restore the project's session (without FILE, reopen the last
                          one) in a scripted run too
  --agent-scenario JSON   run lemma-fake-agent with this scenario instead of Claude
                          (the binary is found next to lemma or through LEMMA_FAKE_AGENT)
  -h, --help              show this help
  -V, --version           show the version
";

/// A scripted demo selected with `--demo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demo {
    Review,
    Pdf,
    Agent,
    Completion,
    Outline,
    HoverRef,
    MathPreview,
    PdfSearch,
    Palette,
    Search,
    Prefs,
    /// WP3.9: delimiter pairs (the `q-pairs.png` snapshot).
    Pairs,
    /// WP5.5: the "New Document from Template" dialog.
    Templates,
}

impl Demo {
    /// Every demo, in `--help` order.
    pub const ALL: [Demo; 13] = [
        Self::Review,
        Self::Pdf,
        Self::Agent,
        Self::Completion,
        Self::Outline,
        Self::HoverRef,
        Self::MathPreview,
        Self::PdfSearch,
        Self::Palette,
        Self::Search,
        Self::Prefs,
        Self::Pairs,
        Self::Templates,
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.name() == name)
    }

    /// The name accepted by `--demo`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Pdf => "pdf",
            Self::Agent => "agent",
            Self::Completion => "completion",
            Self::Outline => "outline",
            Self::HoverRef => "hover-ref",
            Self::MathPreview => "math-preview",
            Self::PdfSearch => "pdf-search",
            Self::Palette => "palette",
            Self::Search => "search",
            Self::Prefs => "prefs",
            Self::Pairs => "pairs",
            Self::Templates => "templates",
        }
    }
}

/// A sidebar page selected with `--sidebar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarPage {
    Outline,
    Changes,
    Problems,
    Search,
}

impl SidebarPage {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "outline" => Self::Outline,
            "changes" => Self::Changes,
            "problems" => Self::Problems,
            "search" => Self::Search,
            _ => return None,
        })
    }

    /// The name accepted by `--sidebar`, which is also the page's name in the sidebar stack.
    pub fn name(self) -> &'static str {
        match self {
            Self::Outline => "outline",
            Self::Changes => "changes",
            Self::Problems => "problems",
            Self::Search => "search",
        }
    }
}

/// A FILE argument: the path, and the place to put the cursor (`+LINE FILE`, `FILE:LINE`,
/// `FILE:LINE:COLUMN`; counted from 1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileArg {
    /// Relative to the current directory.
    pub path: PathBuf,
    pub line: Option<u32>,
    /// A character of `line`.
    pub column: Option<u32>,
}

impl FileArg {
    /// `path` without a place.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            line: None,
            column: None,
        }
    }
}

/// Everything the command line can ask for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The documents to open, in order. The first decides the project root, and it alone may
    /// be a directory (the project itself).
    pub files: Vec<FileArg>,
    pub snapshot: Option<PathBuf>,
    /// Measured from process start.
    pub quit_after: Option<Duration>,
    pub mem_report: bool,
    pub demo: Option<Demo>,
    pub apply_as_agent: Option<PathBuf>,
    /// Build the document's project (as F5 does) and wait for the result before the snapshot.
    pub build: bool,
    /// Show this sidebar page once the other scripted steps are done.
    pub sidebar: Option<SidebarPage>,
    /// A `lemma-fake-agent` scenario: the agent is the fake agent playing it (tests, demos).
    /// It does not make the run scripted.
    pub agent_scenario: Option<PathBuf>,
    /// Restore the project's session although the run is scripted (M3), and without a FILE
    /// reopen the last session's project and file. It does not make the run scripted.
    pub restore_session: bool,
}

impl Options {
    /// The first FILE, which decides the project root.
    pub fn first_file(&self) -> Option<&Path> {
        self.files.first().map(|f| f.path.as_path())
    }

    /// True when the run is scripted (tests, screenshots, measurements): the scripted steps run,
    /// animations are off and errors go to stderr. Prompts are suppressed only when the run also
    /// quits by itself (see [`Options::quits_by_itself`]).
    pub fn is_scripted(&self) -> bool {
        self.snapshot.is_some()
            || self.quit_after.is_some()
            || self.mem_report
            || self.demo.is_some()
            || self.apply_as_agent.is_some()
            || self.build
            || self.sidebar.is_some()
    }

    /// True when the run quits by itself (`--quit-after-ms`), so nobody can answer a prompt.
    /// Without it, a scripted run leaves a normal interactive window behind.
    pub fn quits_by_itself(&self) -> bool {
        self.quit_after.is_some()
    }

    /// Parses the arguments after the program name. Options take their value either as the next
    /// argument or after `=`; `--` ends option parsing (and `+LINE`: what follows is a FILE).
    ///
    /// It looks at the FILEs on disk: `FILE:LINE` is a place only when no file has that very
    /// name, and a FILE after the first must not be a directory.
    pub fn parse<I>(args: I) -> Result<Parsed, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut opts = Options::default();
        let mut args = args.into_iter();
        let mut only_files = false;
        // A `+LINE` waiting for its FILE.
        let mut line: Option<(u32, String)> = None;
        while let Some(arg) = args.next() {
            let text = arg.to_str();
            let is_option = !only_files && text.is_some_and(|t| t.starts_with('-') && t != "-");
            if !is_option {
                if let Some(n) = text.filter(|_| !only_files).and_then(plus_line) {
                    line = Some((n, arg.to_string_lossy().into_owned()));
                    continue;
                }
                let mut file = file_arg(arg);
                if let Some((n, _)) = line.take() {
                    // `+LINE` wins over the FILE's own `:LINE`.
                    file.line = Some(n);
                    file.column = None;
                }
                if !opts.files.is_empty() && file.path.is_dir() {
                    let shown = file.path.to_string_lossy().into_owned();
                    return Err(CliError::DirectoryNotFirst(shown));
                }
                opts.files.push(file);
                continue;
            }
            let text = text.unwrap_or_default();
            let (name, inline) = match text.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n, Some(OsString::from(v))),
                _ => (text, None),
            };
            let mut value = |option: &'static str| {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .ok_or(CliError::MissingValue(option))
            };
            match name {
                "--" => only_files = true,
                "-h" | "--help" => return Ok(Parsed::Help),
                "-V" | "--version" => return Ok(Parsed::Version),
                "--snapshot" => opts.snapshot = Some(value("--snapshot")?.into()),
                "--apply-as-agent" => opts.apply_as_agent = Some(value("--apply-as-agent")?.into()),
                "--agent-scenario" => opts.agent_scenario = Some(value("--agent-scenario")?.into()),
                "--quit-after-ms" => {
                    let v = value("--quit-after-ms")?;
                    let ms = v
                        .to_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .ok_or_else(|| bad_value("--quit-after-ms", &v))?;
                    opts.quit_after = Some(Duration::from_millis(ms));
                }
                "--demo" => {
                    let v = value("--demo")?;
                    let demo = v
                        .to_str()
                        .and_then(Demo::from_name)
                        .ok_or_else(|| bad_value("--demo", &v))?;
                    opts.demo = Some(demo);
                }
                "--sidebar" => {
                    let v = value("--sidebar")?;
                    let page = v
                        .to_str()
                        .and_then(SidebarPage::from_name)
                        .ok_or_else(|| bad_value("--sidebar", &v))?;
                    opts.sidebar = Some(page);
                }
                "--mem-report" if inline.is_none() => opts.mem_report = true,
                "--build" if inline.is_none() => opts.build = true,
                "--restore-session" if inline.is_none() => opts.restore_session = true,
                _ => return Err(CliError::UnknownOption(text.to_owned())),
            }
        }
        if let Some((_, text)) = line {
            return Err(CliError::LineWithoutFile(text));
        }
        Ok(Parsed::Run(opts))
    }
}

/// The line of a `+LINE` argument (`+42`).
fn plus_line(text: &str) -> Option<u32> {
    number(text.strip_prefix('+')?)
}

/// A line or column number: digits only. Counting starts at 1 (0 is taken as 1), and a number
/// too large for a `u32` is the end.
fn number(digits: &str) -> Option<u32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<u32>().unwrap_or(u32::MAX).max(1))
}

/// A FILE argument: `FILE`, or `FILE:LINE` / `FILE:LINE:COLUMN` when no file has that very name
/// (a colon is legal in a file name on Unix).
fn file_arg(arg: OsString) -> FileArg {
    let place = arg
        .to_str()
        .filter(|_| !Path::new(&arg).exists())
        .and_then(split_place);
    match place {
        Some((path, line, column)) => FileArg {
            path: PathBuf::from(path),
            line: Some(line),
            column,
        },
        None => FileArg::new(arg),
    }
}

/// `FILE:LINE` or `FILE:LINE:COLUMN` split into its parts; `None` for anything else.
fn split_place(text: &str) -> Option<(&str, u32, Option<u32>)> {
    let (rest, last) = text.rsplit_once(':')?;
    let last = number(last)?;
    if let Some((path, line)) = rest.rsplit_once(':')
        && let Some(line) = number(line)
        && !path.is_empty()
    {
        return Some((path, line, Some(last)));
    }
    (!rest.is_empty()).then_some((rest, last, None))
}

fn bad_value(option: &'static str, value: &OsString) -> CliError {
    CliError::BadValue {
        option,
        value: value.to_string_lossy().into_owned(),
    }
}

/// The outcome of [`Options::parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Options),
    Help,
    Version,
}

/// A usage error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliError {
    UnknownOption(String),
    MissingValue(&'static str),
    BadValue {
        option: &'static str,
        value: String,
    },
    /// A `+LINE` with no FILE after it.
    LineWithoutFile(String),
    /// A directory as a FILE other than the first.
    DirectoryNotFirst(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(o) => write!(f, "unknown option {o}"),
            Self::MissingValue(o) => write!(f, "{o} needs a value"),
            Self::BadValue { option, value } => write!(f, "invalid value {value:?} for {option}"),
            Self::LineWithoutFile(a) => write!(f, "{a} must come before a FILE"),
            Self::DirectoryNotFirst(a) => {
                write!(
                    f,
                    "only the first FILE can be a directory (the project): {a}"
                )
            }
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, CliError> {
        Options::parse(args.iter().map(OsString::from))
    }

    fn run(args: &[&str]) -> Options {
        match parse(args) {
            Ok(Parsed::Run(o)) => o,
            other => panic!("{args:?} -> {other:?}"),
        }
    }

    #[test]
    fn no_arguments() {
        let o = run(&[]);
        assert_eq!(o, Options::default());
        assert!(!o.is_scripted());
        assert!(!o.quits_by_itself());
        let o = run(&["--sidebar", "changes"]);
        assert!(o.is_scripted() && !o.quits_by_itself());
    }

    #[test]
    fn file_and_every_option() {
        let o = run(&[
            "main.tex",
            "--snapshot",
            "out.png",
            "--quit-after-ms",
            "1500",
            "--mem-report",
            "--demo",
            "pdf",
            "--apply-as-agent",
            "edits.json",
            "--build",
            "--sidebar",
            "problems",
            "--agent-scenario",
            "basic.json",
            "--restore-session",
        ]);
        assert_eq!(o.files, [FileArg::new("main.tex")]);
        assert_eq!(o.agent_scenario, Some(PathBuf::from("basic.json")));
        assert_eq!(o.snapshot, Some(PathBuf::from("out.png")));
        assert_eq!(o.quit_after, Some(Duration::from_millis(1500)));
        assert!(o.mem_report);
        assert_eq!(o.demo, Some(Demo::Pdf));
        assert_eq!(o.apply_as_agent, Some(PathBuf::from("edits.json")));
        assert!(o.build);
        assert_eq!(o.sidebar, Some(SidebarPage::Problems));
        assert!(o.restore_session);
        assert!(o.is_scripted());
        assert!(o.quits_by_itself());
    }

    #[test]
    fn options_before_the_file_and_inline_values() {
        let o = run(&["--quit-after-ms=10", "--snapshot=a b.png", "x.tex"]);
        assert_eq!(o.quit_after, Some(Duration::from_millis(10)));
        assert_eq!(o.snapshot, Some(PathBuf::from("a b.png")));
        assert_eq!(o.files, [FileArg::new("x.tex")]);
    }

    #[test]
    fn several_files_in_order() {
        let o = run(&["b.tex", "a.tex", "--build", "chapters/c.tex"]);
        let files: Vec<&Path> = o.files.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(files, ["b.tex", "a.tex", "chapters/c.tex"].map(Path::new));
        assert_eq!(o.first_file(), Some(Path::new("b.tex")));
        assert!(o.build);
        assert_eq!(run(&[]).first_file(), None);
        // The same file twice is opened once (by the app), not refused.
        assert_eq!(run(&["a.tex", "a.tex"]).files.len(), 2);
    }

    #[test]
    fn plus_line_goes_with_the_next_file() {
        let at = |path: &str, line: u32| FileArg {
            line: Some(line),
            ..FileArg::new(path)
        };
        assert_eq!(run(&["+12", "a.tex"]).files, [at("a.tex", 12)]);
        assert_eq!(
            run(&["a.tex", "+3", "b.tex", "c.tex"]).files,
            [FileArg::new("a.tex"), at("b.tex", 3), FileArg::new("c.tex")]
        );
        // Options may come between; the later `+LINE` wins; lines count from 1.
        assert_eq!(
            run(&["+1", "+7", "--build", "a.tex"]).files,
            [at("a.tex", 7)]
        );
        assert_eq!(run(&["+0", "a.tex"]).files, [at("a.tex", 1)]);
        assert_eq!(
            run(&["+99999999999", "a.tex"]).files,
            [at("a.tex", u32::MAX)]
        );
        // Not a line: a file whose name starts with `+`; after `--`, a file.
        assert_eq!(run(&["+notes.tex"]).files, [FileArg::new("+notes.tex")]);
        assert_eq!(run(&["+"]).files, [FileArg::new("+")]);
        assert_eq!(
            run(&["--", "+5", "a.tex"]).files,
            [FileArg::new("+5"), FileArg::new("a.tex")]
        );
        // `+LINE` wins over the FILE's own place.
        assert_eq!(run(&["+5", "a.tex:7:2"]).files, [at("a.tex", 5)]);
        assert_eq!(
            parse(&["a.tex", "+5"]),
            Err(CliError::LineWithoutFile("+5".into()))
        );
        assert_eq!(
            parse(&["+5", "--build"]),
            Err(CliError::LineWithoutFile("+5".into()))
        );
        assert_eq!(
            CliError::LineWithoutFile("+5".into()).to_string(),
            "+5 must come before a FILE"
        );
    }

    #[test]
    fn line_and_column_after_a_file() {
        let place = |path: &str, line: u32, column: Option<u32>| FileArg {
            line: Some(line),
            column,
            ..FileArg::new(path)
        };
        let missing = "lemma-cli-test-missing.tex";
        assert_eq!(
            run(&[&format!("{missing}:12")]).files,
            [place(missing, 12, None)]
        );
        assert_eq!(
            run(&[&format!("{missing}:12:5")]).files,
            [place(missing, 12, Some(5))]
        );
        assert_eq!(
            run(&["dir/a:b.tex:3"]).files,
            [place("dir/a:b.tex", 3, None)]
        );
        // Anything else is a file name.
        for name in [
            "a.tex:", "a.tex:x", "a.tex:1x", ":12", "a.tex:-3", "a.tex:+3",
        ] {
            assert_eq!(run(&["--", name]).files, [FileArg::new(name)], "{name}");
        }
    }

    /// A file whose name ends in `:12` (a colon cannot be in a Windows file name) is opened as
    /// it is; a directory may only be the first FILE.
    #[cfg(unix)]
    #[test]
    fn files_on_disk() {
        let dir = std::env::temp_dir().join(format!("lemma-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let odd = dir.join("notes.tex:12");
        std::fs::write(&odd, "x\n").unwrap();
        let odd = odd.to_str().unwrap();
        assert_eq!(run(&[odd]).files, [FileArg::new(odd)]);
        let main = dir.join("main.tex");
        let main = main.to_str().unwrap();
        let sub = dir.join("sub");
        let sub = sub.to_str().unwrap();
        let o = run(&[sub, main]);
        assert_eq!(o.first_file(), Some(Path::new(sub)));
        assert_eq!(o.files.len(), 2);
        assert_eq!(
            parse(&[main, sub]),
            Err(CliError::DirectoryNotFirst(sub.into()))
        );
        assert!(
            CliError::DirectoryNotFirst(sub.into())
                .to_string()
                .starts_with("only the first FILE can be a directory")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_scenario_alone_is_not_scripted() {
        let o = run(&["--agent-scenario=t/basic.json", "main.tex"]);
        assert_eq!(o.agent_scenario, Some(PathBuf::from("t/basic.json")));
        assert!(!o.is_scripted() && !o.quits_by_itself());
        assert_eq!(
            parse(&["--agent-scenario"]),
            Err(CliError::MissingValue("--agent-scenario"))
        );
        assert!(USAGE.contains("--agent-scenario"));
        assert!(USAGE.contains("LEMMA_FAKE_AGENT"));
    }

    #[test]
    fn every_demo_name() {
        for d in Demo::ALL {
            assert_eq!(run(&["--demo", d.name()]).demo, Some(d));
            assert!(USAGE.contains(d.name()), "{} is in --help", d.name());
        }
        let names: std::collections::BTreeSet<&str> = Demo::ALL.iter().map(|d| d.name()).collect();
        assert_eq!(names.len(), Demo::ALL.len());
        for name in [
            "outline",
            "hover-ref",
            "math-preview",
            "pdf-search",
            "palette",
            "search",
            "prefs",
            "pairs",
        ] {
            assert!(names.contains(name), "{name}");
        }
    }

    #[test]
    fn restore_session_is_a_flag_that_does_not_script() {
        let o = run(&["main.tex", "--restore-session"]);
        assert!(o.restore_session);
        assert!(!o.is_scripted() && !o.quits_by_itself());
        assert!(!run(&["main.tex"]).restore_session);
        assert_eq!(
            parse(&["--restore-session=yes"]),
            Err(CliError::UnknownOption("--restore-session=yes".into()))
        );
        assert!(USAGE.contains("--restore-session"));
    }

    #[test]
    fn every_sidebar_page() {
        for p in [
            SidebarPage::Outline,
            SidebarPage::Changes,
            SidebarPage::Problems,
            SidebarPage::Search,
        ] {
            assert_eq!(run(&["--sidebar", p.name()]).sidebar, Some(p));
        }
        use crate::window::{PAGE_CHANGES, PAGE_OUTLINE, PAGE_PROBLEMS, PAGE_SEARCH};
        assert_eq!(SidebarPage::Outline.name(), PAGE_OUTLINE);
        assert_eq!(SidebarPage::Changes.name(), PAGE_CHANGES);
        assert_eq!(SidebarPage::Problems.name(), PAGE_PROBLEMS);
        assert_eq!(SidebarPage::Search.name(), PAGE_SEARCH);
        assert_eq!(
            parse(&["--sidebar", "agent"]),
            Err(CliError::BadValue {
                option: "--sidebar",
                value: "agent".into()
            })
        );
    }

    #[test]
    fn double_dash_allows_files_starting_with_a_dash() {
        assert_eq!(run(&["--", "-odd.tex"]).files, [FileArg::new("-odd.tex")]);
        assert_eq!(run(&["-"]).files, [FileArg::new("-")]);
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&["--help"]), Ok(Parsed::Help));
        assert_eq!(parse(&["x.tex", "-h"]), Ok(Parsed::Help));
        assert_eq!(parse(&["-V"]), Ok(Parsed::Version));
    }

    #[test]
    fn usage_errors() {
        assert_eq!(
            parse(&["--frobnicate"]),
            Err(CliError::UnknownOption("--frobnicate".into()))
        );
        assert_eq!(
            parse(&["--snapshot"]),
            Err(CliError::MissingValue("--snapshot"))
        );
        assert_eq!(
            parse(&["--quit-after-ms", "soon"]),
            Err(CliError::BadValue {
                option: "--quit-after-ms",
                value: "soon".into()
            })
        );
        assert_eq!(
            parse(&["--demo", "everything"]),
            Err(CliError::BadValue {
                option: "--demo",
                value: "everything".into()
            })
        );
        assert_eq!(
            parse(&["--mem-report=yes"]),
            Err(CliError::UnknownOption("--mem-report=yes".into()))
        );
        assert_eq!(
            parse(&["--build=now"]),
            Err(CliError::UnknownOption("--build=now".into()))
        );
        assert!(parse(&["--quit-after-ms", "-5"]).is_err());
        let msg = CliError::MissingValue("--demo").to_string();
        assert_eq!(msg, "--demo needs a value");
    }
}
