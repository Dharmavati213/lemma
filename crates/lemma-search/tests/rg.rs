//! WP3.6 acceptance: search results equal an `rg` run on the fixtures (plain, regex, case,
//! whole word).
//!
//! Every case is compared with `rg --json` over all of `tests/fixtures` when `rg` is installed,
//! and always with `tests/golden/rg-proj.txt` (`rg` over `tests/fixtures/proj`, recorded with
//! `UPDATE_GOLDEN=1 cargo test -p lemma-search --test rg` on a machine with `rg`).

use lemma_search::{Limits, Query, Scope, Searcher};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Case {
    name: &'static str,
    pattern: &'static str,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "plain",
        pattern: "sheaf",
        regex: false,
        case_sensitive: true,
        whole_word: false,
    },
    Case {
        name: "plain-ignore-case",
        pattern: "Sheaf",
        regex: false,
        case_sensitive: false,
        whole_word: false,
    },
    Case {
        name: "plain-backslash",
        pattern: r"\mathcal{",
        regex: false,
        case_sensitive: true,
        whole_word: false,
    },
    Case {
        name: "regex",
        pattern: r"\\(begin|end)\{(theorem|lemma|proof)\}",
        regex: true,
        case_sensitive: true,
        whole_word: false,
    },
    Case {
        name: "regex-ignore-case",
        pattern: r"\\(eq)?ref\{[^}]*\}",
        regex: true,
        case_sensitive: false,
        whole_word: false,
    },
    Case {
        name: "regex-anchors",
        pattern: r"^\s*\\(sub)*section",
        regex: true,
        case_sensitive: true,
        whole_word: false,
    },
    Case {
        name: "word",
        pattern: "of",
        regex: false,
        case_sensitive: true,
        whole_word: true,
    },
    Case {
        name: "word-ignore-case",
        pattern: "the",
        regex: false,
        case_sensitive: false,
        whole_word: true,
    },
    Case {
        name: "word-command",
        pattern: r"\cF",
        regex: false,
        case_sensitive: true,
        whole_word: true,
    },
    Case {
        name: "regex-word",
        pattern: r"[a-z]+ed",
        regex: true,
        case_sensitive: true,
        whole_word: true,
    },
    Case {
        name: "regex-unicode-class",
        pattern: r"\w+s\b",
        regex: true,
        case_sensitive: false,
        whole_word: false,
    },
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn golden_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/rg-proj.txt")
}

fn query(case: &Case) -> Query {
    Query {
        pattern: case.pattern.to_owned(),
        regex: case.regex,
        case_sensitive: case.case_sensitive,
        whole_word: case.whole_word,
    }
}

/// Our matches under `root`: `rel/path:line:start-end` (1-based line, byte columns of the
/// line), like [`rg`] gives them.
fn ours(root: &Path, case: &Case) -> BTreeSet<String> {
    let matcher = query(case).matcher().unwrap();
    let scope = Scope {
        root: Some(root.to_path_buf()),
        ..Scope::default()
    };
    let limits = Limits {
        max_matches: usize::MAX,
        line_bytes: usize::MAX / 2,
        ..Limits::default()
    };
    let (files, summary) = Searcher::new().start(matcher, scope, limits).wait();
    let summary = summary.expect("the search finished");
    let mut out = BTreeSet::new();
    for file in &files {
        let rel = file.path.strip_prefix(root).unwrap().display().to_string();
        for line in &file.lines {
            assert_eq!(line.text_start, 0);
            assert_eq!(line.ranges.len(), line.matches);
            for r in &line.ranges {
                out.insert(format!("{rel}:{}:{}-{}", line.line + 1, r.start, r.end));
            }
        }
    }
    assert_eq!(summary.matches, out.len(), "{}: counted matches", case.name);
    out
}

/// `rg`'s matches under `root` (`None` when `rg` is not installed).
fn rg(root: &Path, case: &Case) -> Option<BTreeSet<String>> {
    let mut cmd = Command::new("rg");
    cmd.current_dir(root)
        .args([
            "--json",
            "--no-config",
            "--no-ignore",
            "-g",
            "*.{tex,bib,sty,cls,md}",
        ])
        .arg(if case.case_sensitive { "-s" } else { "-i" });
    if !case.regex {
        cmd.arg("-F");
    }
    if case.whole_word {
        cmd.arg("-w");
    }
    cmd.arg("-e").arg(case.pattern).arg(".");
    let output = cmd.output().ok()?;
    assert!(
        output.status.code().is_some_and(|c| c <= 1),
        "rg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut out = BTreeSet::new();
    for line in output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
    {
        let v: serde_json::Value = serde_json::from_slice(line).unwrap();
        if v["type"] != "match" {
            continue;
        }
        let data = &v["data"];
        let path = data["path"]["text"].as_str().unwrap();
        let path = path.strip_prefix("./").unwrap_or(path);
        let number = data["line_number"].as_u64().unwrap();
        for m in data["submatches"].as_array().unwrap() {
            out.insert(format!(
                "{path}:{number}:{}-{}",
                m["start"].as_u64().unwrap(),
                m["end"].as_u64().unwrap()
            ));
        }
    }
    Some(out)
}

fn read_golden() -> BTreeMap<String, BTreeSet<String>> {
    let text = std::fs::read_to_string(golden_file()).expect("the golden file");
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            current = Some(name.to_owned());
            out.entry(name.to_owned()).or_default();
        } else if !line.is_empty() && !line.starts_with("# ") {
            out.get_mut(current.as_ref().expect("a case heading"))
                .unwrap()
                .insert(line.to_owned());
        }
    }
    out
}

#[test]
fn results_equal_rg_on_all_fixtures() {
    let root = fixtures().canonicalize().unwrap();
    let mut compared = 0;
    for case in CASES {
        let Some(expected) = rg(&root, case) else {
            eprintln!("rg is not installed: compared with the golden file only");
            return;
        };
        let got = ours(&root, case);
        assert!(!expected.is_empty(), "{}: rg finds something", case.name);
        if got != expected {
            let missing: Vec<_> = expected.difference(&got).take(10).collect();
            let extra: Vec<_> = got.difference(&expected).take(10).collect();
            panic!(
                "{}: {} matches, rg {}; missing {missing:?}, extra {extra:?}",
                case.name,
                got.len(),
                expected.len()
            );
        }
        compared += got.len();
    }
    eprintln!("{compared} matches equal rg's");
}

#[test]
fn results_equal_the_recorded_rg_run_on_proj() {
    let root = fixtures().join("proj").canonicalize().unwrap();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let mut text = String::from(
            "# rg --json --no-config --no-ignore -g '*.{tex,bib,sty,cls,md}' over \
             tests/fixtures/proj: path:line:start-end per match (tests/rg.rs)\n",
        );
        for case in CASES {
            let found = rg(&root, case).expect("rg is installed");
            text.push_str(&format!("## {}\n", case.name));
            for m in found {
                text.push_str(&m);
                text.push('\n');
            }
        }
        std::fs::create_dir_all(golden_file().parent().unwrap()).unwrap();
        std::fs::write(golden_file(), text).unwrap();
    }
    let golden = read_golden();
    for case in CASES {
        let expected = golden
            .get(case.name)
            .unwrap_or_else(|| panic!("{} is in the golden file", case.name));
        assert_eq!(&ours(&root, case), expected, "{}", case.name);
    }
}
