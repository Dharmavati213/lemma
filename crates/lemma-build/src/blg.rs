//! biber and BibTeX `.blg` parsing.

use crate::paths::normalize;
use lemma_core::{DiagSource, Diagnostic, Severity};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

struct Patterns {
    /// `[139] Utils.pm:480> ERROR - …`
    biber: Regex,
    /// `BibTeX subsystem: /tmp/biber_tmp_x/….utf8, line 37, syntax error: …`
    biber_location: Regex,
    biber_source: Regex,
    bibtex_database: Regex,
    bibtex_warning: Regex,
    /// `--line 12 of file refs.bib`, the location of the warning before it.
    bibtex_warning_location: Regex,
    /// `Illegal end of database file---line 36 of file refs.bib`
    bibtex_error: Regex,
    /// `I found no \citation commands---while reading file main.aux`
    bibtex_error_in_file: Regex,
    bibtex_open: Regex,
}

static RE: LazyLock<Patterns> = LazyLock::new(|| {
    let re = |pattern: &str| Regex::new(pattern).expect("valid regex");
    Patterns {
        biber: re(r"^\[\d+\] [\w:.-]+:\d+> (?P<level>WARN|ERROR|FATAL) - (?P<msg>.*)$"),
        biber_location: re(r"^BibTeX subsystem: (?P<file>.+?), line (?P<line>\d+), (?P<msg>.*)$"),
        biber_source: re(r"Found BibTeX data source '(?P<file>[^']+)'"),
        bibtex_database: re(r"^Database file #\d+: (?P<file>.+)$"),
        bibtex_warning: re(r"^Warning--(?P<msg>.*)$"),
        bibtex_warning_location: re(r"^--line (?P<line>\d+) of file (?P<file>.+)$"),
        bibtex_error: re(r"^(?P<msg>.+?)---line (?P<line>\d+) of file (?P<file>.+)$"),
        bibtex_error_in_file: re(r"^(?P<msg>.+?)---while reading file (?P<file>.+)$"),
        bibtex_open: re(r"^I couldn't open (?:database|style|auxiliary) file (?P<file>.+)$"),
    }
});

/// Parses a biber or BibTeX `.blg`.
///
/// File names are matched against `bib_sources` (by path suffix, then by file name); unmatched
/// names are kept as written, relative to the build directory. biber reports syntax errors in a
/// temporary UTF-8 copy of the `.bib` whose line numbers match the original, so those are
/// mapped to the most recent `Found BibTeX data source`. Messages without a location (a missing
/// entry, for example) are attached to the current data source, else the first of
/// `bib_sources`, else an empty path.
pub fn parse_blg(blg: &str, bib_sources: &[PathBuf]) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    let mut current: Option<PathBuf> = None;
    let lines: Vec<&str> = blg.lines().collect();
    let fallback = |current: &Option<PathBuf>| {
        current
            .clone()
            .or_else(|| bib_sources.first().cloned())
            .unwrap_or_default()
    };
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = RE.biber_source.captures(line) {
            current = Some(source_path(&c["file"], bib_sources));
        } else if let Some(c) = RE.biber.captures(line) {
            let severity = if &c["level"] == "WARN" {
                Severity::Warning
            } else {
                Severity::Error
            };
            let (path, line_no, message) = match RE.biber_location.captures(&c["msg"]) {
                Some(l) => {
                    let file = &l["file"];
                    let path = if is_biber_temp_file(file) {
                        current
                            .clone()
                            .unwrap_or_else(|| source_path(file, bib_sources))
                    } else {
                        source_path(file, bib_sources)
                    };
                    let msg = l["msg"].trim();
                    let msg = msg.strip_prefix("warning: ").unwrap_or(msg);
                    (path, l["line"].parse().ok(), msg.to_owned())
                }
                None => (fallback(&current), None, c["msg"].trim().to_owned()),
            };
            out.push(diagnostic(
                path,
                line_no,
                severity,
                message,
                DiagSource::Biber,
            ));
        } else if let Some(c) = RE.bibtex_database.captures(line) {
            current = Some(source_path(c["file"].trim(), bib_sources));
        } else if let Some(c) = RE.bibtex_warning.captures(line) {
            let location = lines
                .get(i + 1)
                .and_then(|next| RE.bibtex_warning_location.captures(next));
            let (path, line_no) = match location {
                Some(l) => (
                    source_path(l["file"].trim(), bib_sources),
                    l["line"].parse().ok(),
                ),
                None => (fallback(&current), None),
            };
            let message = c["msg"].trim().to_owned();
            out.push(diagnostic(
                path,
                line_no,
                Severity::Warning,
                message,
                DiagSource::Bibtex,
            ));
        } else if let Some(c) = RE.bibtex_error.captures(line) {
            let path = source_path(c["file"].trim(), bib_sources);
            let message = c["msg"].trim().to_owned();
            let line_no = c["line"].parse().ok();
            out.push(diagnostic(
                path,
                line_no,
                Severity::Error,
                message,
                DiagSource::Bibtex,
            ));
        } else if let Some(c) = RE.bibtex_error_in_file.captures(line) {
            let path = PathBuf::from(c["file"].trim());
            let message = c["msg"].trim().to_owned();
            out.push(diagnostic(
                path,
                None,
                Severity::Error,
                message,
                DiagSource::Bibtex,
            ));
        } else if let Some(c) = RE.bibtex_open.captures(line) {
            let path = source_path(c["file"].trim(), bib_sources);
            let message = line.trim().to_owned();
            out.push(diagnostic(
                path,
                None,
                Severity::Error,
                message,
                DiagSource::Bibtex,
            ));
        }
    }
    out
}

fn diagnostic(
    path: PathBuf,
    line: Option<u32>,
    severity: Severity,
    message: String,
    source: DiagSource,
) -> Diagnostic {
    Diagnostic {
        path,
        line,
        severity,
        message,
        context: None,
        source,
    }
}

/// biber decodes each data source into a temporary copy (`/tmp/biber_tmp_XXXX/<hash>.utf8`).
fn is_biber_temp_file(file: &str) -> bool {
    file.contains("biber_tmp_") || file.ends_with(".utf8")
}

/// The entry of `sources` that `name` refers to (`refs` and `refs.bib` both match
/// `/p/refs.bib`), else `name` itself.
fn source_path(name: &str, sources: &[PathBuf]) -> PathBuf {
    let written = normalize(Path::new(name));
    let mut with_bib = written.clone().into_os_string();
    with_bib.push(".bib");
    let with_bib = PathBuf::from(with_bib);
    let by_suffix = sources.iter().find(|s| {
        s.ends_with(&written) || (written.extension().is_none() && s.ends_with(&with_bib))
    });
    let by_name = || {
        let file_name = written.file_name()?;
        sources.iter().find(|s| s.file_name() == Some(file_name))
    };
    by_suffix.or_else(by_name).cloned().unwrap_or(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(diags: &[Diagnostic]) -> Vec<(String, Option<u32>, Severity, &str)> {
        diags
            .iter()
            .map(|d| {
                (
                    d.path.display().to_string(),
                    d.line,
                    d.severity,
                    d.message.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn biber_temp_file_maps_to_data_source() {
        let blg = "[117] bibtex.pm:1519> INFO - Found BibTeX data source 'refs.bib'\n\
                   [139] Utils.pm:480> ERROR - BibTeX subsystem: /tmp/biber_tmp_aW4R/8fe4_39057.utf8, line 37, syntax error: at end of input\n\
                   [140] Biber.pm:132> WARN - BibTeX subsystem: /tmp/biber_tmp_aW4R/8fe4_39057.utf8, line 35, warning: possible runaway string started at line 34\n\
                   [147] Biber.pm:132> WARN - I didn't find a database entry for 'Lurie' (section 0)\n\
                   [140] Biber.pm:134> INFO - WARNINGS: 1\n";
        let sources = [PathBuf::from("/p/other.bib"), PathBuf::from("/p/refs.bib")];
        let diags = parse_blg(blg, &sources);
        assert_eq!(
            brief(&diags),
            [
                (
                    "/p/refs.bib".into(),
                    Some(37),
                    Severity::Error,
                    "syntax error: at end of input"
                ),
                (
                    "/p/refs.bib".into(),
                    Some(35),
                    Severity::Warning,
                    "possible runaway string started at line 34"
                ),
                (
                    "/p/refs.bib".into(),
                    None,
                    Severity::Warning,
                    "I didn't find a database entry for 'Lurie' (section 0)"
                ),
            ]
        );
        assert!(diags.iter().all(|d| d.source == DiagSource::Biber));
    }

    #[test]
    fn biber_without_sources_keeps_names() {
        let blg = "[1] biber:342> INFO - x\n[2] Biber.pm:1> ERROR - Cannot find 'missing.bib'!\n";
        assert_eq!(
            brief(&parse_blg(blg, &[])),
            [(
                "".into(),
                None,
                Severity::Error,
                "Cannot find 'missing.bib'!"
            )]
        );
    }

    #[test]
    fn bibtex_messages() {
        let blg = "Database file #1: refs.bib\n\
                   I was expecting a `,' or a `}'---line 12 of file refs.bib\n : @article{x\n\
                   Warning--string name \"jan\" is undefined\n--line 20 of file refs.bib\n\
                   Warning--I didn't find a database entry for \"Nope\"\n\
                   I couldn't open database file nope.bib\n\
                   I found no \\citation commands---while reading file main.aux\n";
        let diags = parse_blg(blg, &[PathBuf::from("/p/refs.bib")]);
        assert_eq!(
            brief(&diags),
            [
                (
                    "/p/refs.bib".into(),
                    Some(12),
                    Severity::Error,
                    "I was expecting a `,' or a `}'"
                ),
                (
                    "/p/refs.bib".into(),
                    Some(20),
                    Severity::Warning,
                    "string name \"jan\" is undefined"
                ),
                (
                    "/p/refs.bib".into(),
                    None,
                    Severity::Warning,
                    "I didn't find a database entry for \"Nope\""
                ),
                (
                    "nope.bib".into(),
                    None,
                    Severity::Error,
                    "I couldn't open database file nope.bib"
                ),
                (
                    "main.aux".into(),
                    None,
                    Severity::Error,
                    "I found no \\citation commands"
                ),
            ]
        );
        assert!(diags.iter().all(|d| d.source == DiagSource::Bibtex));
    }

    #[test]
    fn source_matching() {
        let sources = [
            PathBuf::from("/p/bib/refs.bib"),
            PathBuf::from("/p/more.bib"),
        ];
        assert_eq!(source_path("bib/refs.bib", &sources), sources[0]);
        assert_eq!(source_path("./bib/refs", &sources), sources[0]);
        assert_eq!(source_path("refs.bib", &sources), sources[0]);
        assert_eq!(source_path("more", &sources), sources[1]);
        assert_eq!(source_path("x/none.bib", &sources), Path::new("x/none.bib"));
    }
}
