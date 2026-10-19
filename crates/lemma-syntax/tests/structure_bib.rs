//! WP1.9 acceptance: the tolerant `.bib` scanner on the fixture, on TeX Live's font.bib and
//! tugboat.bib (found with `kpsewhich`; skipped if TeX Live is missing) and on malformed input.

use lemma_syntax::{BibEntry, scan_bib};
use std::path::PathBuf;
use std::process::Command;

fn entry(
    key: &str,
    kind: &str,
    author: Option<&str>,
    year: Option<&str>,
    title: Option<&str>,
    line: u32,
) -> BibEntry {
    BibEntry {
        key: key.into(),
        kind: kind.into(),
        author: author.map(str::to_owned),
        year: year.map(str::to_owned),
        title: title.map(str::to_owned),
        line,
    }
}

fn kpsewhich(name: &str) -> Option<String> {
    let out = Command::new("kpsewhich").arg(name).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if path.is_empty() {
        eprintln!("SKIP: kpsewhich {name} found nothing");
        return None;
    }
    std::fs::read(&path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Lines starting with `@type` minus `@string`/`@preamble`/`@comment`: an independent count of
/// the entries in a well-formed file that puts every `@` at the start of a line.
fn entries_by_line_count(text: &str) -> usize {
    text.lines()
        .filter_map(|l| l.strip_prefix('@'))
        .map(|rest| {
            rest.split(['{', '('])
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|kind| !matches!(kind.as_str(), "string" | "preamble" | "comment"))
        .count()
}

#[test]
fn fixture_refs_bib() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/proj/refs.bib");
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        scan_bib(&text),
        vec![
            entry(
                "SGA1",
                "book",
                Some("Grothendieck, Alexander"),
                Some("1971"),
                Some("Rev{\\^e}tements {\\'e}tales et groupe fondamental (SGA 1)"),
                0
            ),
            entry(
                "Vistoli",
                "incollection",
                Some("Vistoli, Angelo"),
                Some("2005"),
                Some("Grothendieck topologies, fibered categories and descent theory"),
                8
            ),
            entry(
                "Stacks",
                "misc",
                Some("The Stacks project authors"),
                Some("2026"),
                Some("The Stacks project"),
                18
            ),
            entry(
                "MacLane",
                "book",
                Some("Mac Lane, Saunders"),
                Some("1998"),
                Some("Categories for the Working Mathematician"),
                24
            ),
        ]
    );
}

#[test]
fn font_bib_despite_preamble_concatenation() {
    let Some(text) = kpsewhich("font.bib") else {
        return;
    };
    // font.bib opens with an `@Preamble{"…" # "…" # …}` that the `biblatex` crate rejects.
    assert!(text.contains("@Preamble{\n    \"\\ifx"));
    let entries = scan_bib(&text);
    assert_eq!(entries.len(), entries_by_line_count(&text));
    assert!(entries.len() > 1000);
    let first = &entries[0];
    assert_eq!(first.key, "Dearborn:1785:SRS");
    assert_eq!(first.kind, "book");
    assert_eq!(first.author.as_deref(), Some("Benjamin Dearborn"));
    assert_eq!(first.year.as_deref(), Some("1785"));
    assert_eq!(
        text.lines().nth(first.line as usize),
        Some("@Book{Dearborn:1785:SRS,")
    );
}

#[test]
fn tugboat_bib() {
    let Some(text) = kpsewhich("tugboat.bib") else {
        return;
    };
    let entries = scan_bib(&text);
    assert_eq!(entries.len(), entries_by_line_count(&text));
    assert_eq!(
        entries.len(),
        5196,
        "the count the research measured with the biblatex crate"
    );
    for e in entries.iter().step_by(97) {
        let line = text.lines().nth(e.line as usize).unwrap();
        assert!(line.starts_with('@') && line.contains(&e.key), "{line}");
    }
}

#[test]
fn malformed_entries_are_skipped() {
    let text = r#"@article{good1, author = {A}, year = 2001, title = "T1"}
@article{bad1, author = {Unclosed, year = 2002,
@book{good2, title = {T2}, date = {2019-03-01}, editor = {E}}
@misc{bad2 title = {missing comma}}
@misc{good3, title = {{Protected}}, author = "{The} Team"}
@string{jn = "Journal"}
@STRING(sp = { Springer })
@article(good4, title = jn # " of " # {Things}, year = "1999", publisher = sp)
@comment{ @article{commented, title={x}} }
@preamble{ "\def\x{y}" # "z" }
junk @ text, e-mail me@example.org
@misc{good5,}
@misc{good6}
@misc{bad3, title = {x} year = 2000}
@misc{good7,
  title = {Multi
           line   title},
  YEAR = 2020,
}
@misc{bad4, title = {x}
"#;
    let entries = scan_bib(text);
    let keys: Vec<_> = entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "good1", "good2", "good3", "good4", "good5", "good6", "good7"
        ]
    );
    assert_eq!(
        entries[0],
        entry("good1", "article", Some("A"), Some("2001"), Some("T1"), 0)
    );
    assert_eq!(
        entries[1],
        entry("good2", "book", Some("E"), Some("2019"), Some("T2"), 2)
    );
    assert_eq!(entries[2].title.as_deref(), Some("Protected"));
    assert_eq!(entries[2].author.as_deref(), Some("{The} Team"));
    assert_eq!(entries[3].title.as_deref(), Some("Journal of Things"));
    assert_eq!(entries[3].line, 7);
    assert_eq!(entries[5], entry("good6", "misc", None, None, None, 12));
    assert_eq!(entries[6].title.as_deref(), Some("Multi line title"));
    assert_eq!(entries[6].year.as_deref(), Some("2020"));
}

#[test]
fn comment_lines_between_fields() {
    let text = "@article{k,\n  % author = {Commented Out},\n  title = {T}, % trailing\n  url = {a%20b},\n  % last\n}\n";
    let entries = scan_bib(text);
    assert_eq!(
        entries,
        vec![entry("k", "article", None, None, Some("T"), 0)]
    );
}

#[test]
fn empty_and_garbage() {
    assert!(scan_bib("").is_empty());
    assert!(scan_bib("@").is_empty());
    assert!(scan_bib("@@@{{{").is_empty());
    assert!(scan_bib("no entries here\n").is_empty());
    assert_eq!(
        scan_bib("@misc{k, title = {é — ü}}")[0].title.as_deref(),
        Some("é — ü")
    );
}
