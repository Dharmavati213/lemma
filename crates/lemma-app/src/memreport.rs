//! `--mem-report`: the process's memory from `/proc/self/smaps_rollup` (ARCHITECTURE §7).
//!
//! The report is printed on stdout in the `smaps_rollup` line format, which `scripts/rss.sh`
//! reads (it takes the last `Rss:` and `Pss:` lines). It must be taken before any snapshot,
//! because rendering a texture adds tens of MB.

use std::io::{self, Write};

const ROLLUP: &str = "/proc/self/smaps_rollup";

/// Memory figures in kB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemReport {
    pub rss_kb: u64,
    pub pss_kb: u64,
    pub anon_kb: u64,
}

impl MemReport {
    /// Reads this process's figures.
    pub fn read() -> io::Result<Self> {
        let text = std::fs::read_to_string(ROLLUP)?;
        Self::parse(&text)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no Rss/Pss in smaps_rollup"))
    }

    /// Parses `smaps_rollup` text; `None` unless both `Rss:` and `Pss:` are present.
    pub fn parse(text: &str) -> Option<Self> {
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
        };
        Some(Self {
            rss_kb: field("Rss:")?,
            pss_kb: field("Pss:")?,
            anon_kb: field("Anonymous:").unwrap_or(0),
        })
    }

    /// The report as printed: a `[mem] label` header and one line per figure.
    pub fn format(&self, label: &str) -> String {
        format!(
            "[mem] {label}\nRss:       {:>8} kB\nPss:       {:>8} kB\nAnonymous: {:>8} kB\n",
            self.rss_kb, self.pss_kb, self.anon_kb
        )
    }
}

/// Reads and prints the report on stdout; failures go to the log.
pub fn print_report(label: &str) {
    match MemReport::read() {
        Ok(report) => {
            let mut out = io::stdout().lock();
            // A closed stdout must not abort a measurement run.
            let _ = out.write_all(report.format(label).as_bytes());
            let _ = out.flush();
        }
        Err(e) => tracing::error!("--mem-report: cannot read {ROLLUP}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
55d0c0a00000-7ffd4b5f2000 ---p 00000000 00:00 0                          [rollup]
Rss:              98500 kB
Pss:              45012 kB
Pss_Dirty:        20000 kB
Pss_Anon:         19000 kB
Shared_Clean:     50000 kB
Anonymous:        23000 kB
Swap:                 0 kB
";

    #[test]
    fn parses_rollup_text() {
        let r = MemReport::parse(SAMPLE).unwrap();
        assert_eq!(
            r,
            MemReport {
                rss_kb: 98500,
                pss_kb: 45012,
                anon_kb: 23000
            }
        );
    }

    #[test]
    fn missing_fields() {
        assert_eq!(MemReport::parse("Pss: 1 kB\n"), None);
        assert_eq!(
            MemReport::parse("Rss: 1 kB\nPss: 2 kB\n").unwrap().anon_kb,
            0
        );
    }

    #[test]
    fn format_uses_the_rollup_line_format() {
        let text = MemReport {
            rss_kb: 98500,
            pss_kb: 45012,
            anon_kb: 23000,
        }
        .format("idle");
        assert!(text.starts_with("[mem] idle\n"));
        // rss.sh: the number is the second whitespace-separated field of the `Rss:` line.
        let rss = text.lines().find(|l| l.starts_with("Rss:")).unwrap();
        assert_eq!(
            rss.split_whitespace().collect::<Vec<_>>(),
            ["Rss:", "98500", "kB"]
        );
        assert_eq!(MemReport::parse(&text), MemReport::parse(SAMPLE));
    }

    #[test]
    fn reads_this_process() {
        let r = MemReport::read().expect("smaps_rollup is readable on Linux");
        assert!(r.rss_kb > 0 && r.pss_kb > 0);
    }
}
