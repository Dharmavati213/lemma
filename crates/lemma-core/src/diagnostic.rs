use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How serious a diagnostic is. Ordered from most to least severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    /// Overfull/underfull box warnings.
    BadBox,
    Info,
}

/// The tool whose output a diagnostic was parsed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiagSource {
    /// The TeX engine's `.log`.
    Tex,
    /// biber's `.blg`.
    Biber,
    /// BibTeX's `.blg`.
    Bibtex,
    /// makeindex / xindy.
    Index,
    /// chktex warnings (M3, WP3.7; added by WP3.0 so WP3.2 and WP3.7 do not both edit this
    /// enum).
    Lint,
    /// Undefined references and citations, found from the project index and `.aux` (M3,
    /// WP3.2).
    Labels,
}

/// One problem reported by a build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Absolute path, or relative to the build working directory if the file could not be resolved.
    pub path: PathBuf,
    /// 1-based line, if known.
    pub line: Option<u32>,
    pub severity: Severity,
    pub message: String,
    /// TeX context: text before the error point, `<HERE>`, text after.
    pub context: Option<String>,
    pub source: DiagSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities_sort_most_severe_first() {
        let mut v = vec![
            Severity::Info,
            Severity::BadBox,
            Severity::Error,
            Severity::Warning,
        ];
        v.sort();
        assert_eq!(
            v,
            [
                Severity::Error,
                Severity::Warning,
                Severity::BadBox,
                Severity::Info
            ]
        );
    }
}
