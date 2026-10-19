//! "Add Citation from arXiv ID or DOI…".
//!
//! `win.add-citation` (main menu, file section; the command palette) opens a dialog: an arXiv ID
//! or a DOI (bare, `doi:`/`arXiv:` prefixed, or as a doi.org / arxiv.org link), the project's
//! `.bib` file to add to (a new `references.bib` in the root when there is none) and whether to
//! insert `\cite{key}` at the cursor. **Add** fetches the entry, and only then is the network
//! used:
//!
//! - **DOI:** `GET https://doi.org/<doi>` with `Accept: application/x-bibtex` (doi.org redirects
//!   to the registrar's BibTeX);
//! - **arXiv:** `GET https://export.arxiv.org/api/query?id_list=<id>` (Atom), converted to a
//!   `@misc` entry with `eprint`, `archivePrefix` and `primaryClass` ([`arxiv_to_bibtex`]).
//!
//! The entry gets a key of the form `lastname2020word` ([`make_key`]), made unique in the file,
//! and is **appended to the `.bib` file as a reviewable hunk** (applied like a restore from
//! History: `Origin::Agent { call: 0 }` outside any agent turn, then saved), so it can be
//! rejected like an agent's change. An entry whose DOI or arXiv ID is already in the file is not
//! added again; its key is used.
//!
//! **Tests** never reach the real network: `LEMMA_DOI_URL` and `LEMMA_ARXIV_URL` replace the two
//! endpoints ([`Endpoints::from_env`]), and the tests serve them from a local fake server.
//!
//! Hooks: [`install`] once per window. Tests: [`show`], [`citation_dialog`], [`add_citation`],
//! [`fetch`] and the pure helpers.

use crate::editor::Editor;
use crate::review_ui;
use crate::window::WindowState;
use crate::workspace::{normalize, walk_project};
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Replaces `https://doi.org` (tests).
pub const DOI_URL_ENV: &str = "LEMMA_DOI_URL";
/// Replaces `https://export.arxiv.org/api/query` (tests).
pub const ARXIV_URL_ENV: &str = "LEMMA_ARXIV_URL";

const DOI_URL: &str = "https://doi.org";
const ARXIV_URL: &str = "https://export.arxiv.org/api/query";
/// Every request gives up after this long.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Larger responses are refused.
const MAX_RESPONSE: u64 = 1 << 20;

/// The action's name.
pub const ACTION: &str = "add-citation";

/// What the user typed, recognised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identifier {
    /// A DOI such as `10.1007/BF02684778`.
    Doi(String),
    /// An arXiv ID such as `2301.01234v2` or `math/0211159`.
    Arxiv(String),
}

impl Identifier {
    /// The ID without an arXiv version, ASCII lower case: what [`find_duplicate`] looks for.
    fn search_key(&self) -> String {
        match self {
            Self::Doi(doi) => doi.to_ascii_lowercase(),
            Self::Arxiv(id) => strip_version(id).to_ascii_lowercase(),
        }
    }
}

/// Recognises an arXiv ID or a DOI, with or without a prefix or link.
pub fn parse_identifier(input: &str) -> Result<Identifier, String> {
    let s = input.trim();
    let lower = s.to_ascii_lowercase();
    let strip = |prefixes: &[&str]| {
        prefixes
            .iter()
            .find(|p| lower.starts_with(*p))
            .map(|p| s[p.len()..].trim())
    };
    let doi_prefixes = [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi.org/",
        "doi:",
    ];
    let arxiv_prefixes = [
        "https://arxiv.org/abs/",
        "http://arxiv.org/abs/",
        "https://arxiv.org/pdf/",
        "http://arxiv.org/pdf/",
        "arxiv.org/abs/",
        "arxiv:",
    ];
    if let Some(doi) = strip(&doi_prefixes) {
        return valid_doi(doi)
            .then(|| Identifier::Doi(doi.to_owned()))
            .ok_or_else(|| format!("“{doi}” is not a DOI"));
    }
    if let Some(id) = strip(&arxiv_prefixes) {
        let id = id.strip_suffix(".pdf").unwrap_or(id);
        return valid_arxiv(id)
            .then(|| Identifier::Arxiv(id.to_owned()))
            .ok_or_else(|| format!("“{id}” is not an arXiv ID"));
    }
    if valid_doi(s) {
        Ok(Identifier::Doi(s.to_owned()))
    } else if valid_arxiv(s) {
        Ok(Identifier::Arxiv(s.to_owned()))
    } else {
        Err("Enter an arXiv ID (2301.01234) or a DOI (10.1000/xyz123)".into())
    }
}

/// `10.<4–9 digits>/<suffix>`, without blanks.
fn valid_doi(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("10.") else {
        return false;
    };
    let Some((registrant, suffix)) = rest.split_once('/') else {
        return false;
    };
    let registrant = registrant.split('.').next().unwrap_or("");
    (4..=9).contains(&registrant.len())
        && registrant.bytes().all(|b| b.is_ascii_digit())
        && !suffix.is_empty()
        && !s.chars().any(char::is_whitespace)
}

/// `YYMM.NNNN(N)` or `archive(.XX)/YYMMNNN`, each with an optional `vN`.
fn valid_arxiv(s: &str) -> bool {
    let id = strip_version(s);
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if let Some((yymm, number)) = id.split_once('.')
        && yymm.len() == 4
        && digits(yymm)
        && (4..=5).contains(&number.len())
        && digits(number)
    {
        return true;
    }
    if let Some((archive, number)) = id.split_once('/') {
        let (name, class) = archive.split_once('.').unwrap_or((archive, ""));
        return !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
            && (class.is_empty()
                || (class.len() == 2 && class.bytes().all(|b| b.is_ascii_uppercase())))
            && number.len() == 7
            && digits(number);
    }
    false
}

/// `id` without a trailing `vN`.
fn strip_version(id: &str) -> &str {
    match id.rfind('v') {
        Some(at)
            if at > 0
                && at + 1 < id.len()
                && id[at + 1..].bytes().all(|b| b.is_ascii_digit())
                && id.as_bytes()[at - 1].is_ascii_digit() =>
        {
            &id[..at]
        }
        _ => id,
    }
}

/// Percent-encodes everything but unreserved characters and `/`.
fn encode_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The two services' base URLs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// doi.org; the DOI is appended as a path.
    pub doi: String,
    /// The arXiv API's query URL; `?id_list=<id>` is appended.
    pub arxiv: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            doi: DOI_URL.into(),
            arxiv: ARXIV_URL.into(),
        }
    }
}

impl Endpoints {
    /// The defaults, each replaced by [`DOI_URL_ENV`] / [`ARXIV_URL_ENV`] when set.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            doi: std::env::var(DOI_URL_ENV).unwrap_or(default.doi),
            arxiv: std::env::var(ARXIV_URL_ENV).unwrap_or(default.arxiv),
        }
    }

    /// The URL fetched for `id`.
    pub fn url(&self, id: &Identifier) -> String {
        match id {
            Identifier::Doi(doi) => {
                format!("{}/{}", self.doi.trim_end_matches('/'), encode_path(doi))
            }
            Identifier::Arxiv(id) => format!("{}?id_list={}", self.arxiv, encode_path(id)),
        }
    }
}

/// Fetches the entry for `id` and returns it as a [`BibEntry`] with a generated key.
/// **Blocking** (network): run it on a worker thread.
pub fn fetch(endpoints: &Endpoints, id: &Identifier) -> Result<BibEntry, String> {
    let url = endpoints.url(id);
    let accept = match id {
        Identifier::Doi(_) => "application/x-bibtex",
        Identifier::Arxiv(_) => "application/atom+xml",
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!(
            "Lemma/",
            env!("CARGO_PKG_VERSION"),
            " (LaTeX editor; citation import)"
        ))
        .build()
        .into();
    let service = match id {
        Identifier::Doi(_) => "doi.org",
        Identifier::Arxiv(_) => "arXiv",
    };
    let mut response = agent
        .get(&url)
        .header("Accept", accept)
        .call()
        .map_err(|e| format!("Cannot reach {service}: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE)
        .read_to_string()
        .map_err(|e| format!("Cannot read the answer of {service}: {e}"))?;
    if !(200..=299).contains(&status) {
        return Err(match id {
            Identifier::Doi(doi) if status == 404 => format!("No record found for DOI {doi}"),
            _ => format!("{service} answered with HTTP status {status}"),
        });
    }
    let mut entry = match id {
        Identifier::Doi(doi) => {
            parse_bibtex(&body).map_err(|e| format!("doi.org sent no BibTeX for {doi}: {e}"))?
        }
        Identifier::Arxiv(arxiv) => arxiv_to_bibtex(&body, arxiv)?,
    };
    if let Some(key) = make_key(&entry) {
        entry.key = key;
    }
    Ok(entry)
}

// ----- BibTeX -----

/// One BibTeX entry: `@kind{key, name = value, …}`. Values are kept as written, delimiters
/// included (`{…}`, `"…"`, a number or a macro such as `jul`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BibEntry {
    pub kind: String,
    pub key: String,
    pub fields: Vec<(String, String)>,
}

impl BibEntry {
    /// The field `name` (case-insensitive), without its outer braces or quotes.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| unwrap_value(v))
    }

    /// The entry as text, one field per line, the `=` signs aligned, ending in a newline.
    pub fn to_bibtex(&self) -> String {
        let width = self.fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let mut out = format!("@{}{{{},\n", self.kind, self.key);
        for (name, value) in &self.fields {
            out.push_str(&format!("  {name:width$} = {value},\n"));
        }
        out.push_str("}\n");
        out
    }
}

/// `value` without one pair of outer braces or quotes.
fn unwrap_value(value: &str) -> &str {
    let v = value.trim();
    let delimited =
        (v.starts_with('{') && v.ends_with('}')) || (v.starts_with('"') && v.ends_with('"'));
    if delimited && v.len() >= 2 {
        return v[1..v.len() - 1].trim();
    }
    v
}

/// Parses the first entry of `text` (`@string`, `@comment` and `@preamble` are skipped).
pub fn parse_bibtex(text: &str) -> Result<BibEntry, String> {
    let mut rest = text;
    loop {
        let at = rest.find('@').ok_or("no entry")?;
        rest = &rest[at + 1..];
        let open = rest.find(['{', '(']).ok_or("no entry")?;
        let kind = rest[..open].trim();
        if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let close = if rest.as_bytes()[open] == b'{' {
            b'}'
        } else {
            b')'
        };
        let body = &rest[open + 1..];
        if ["string", "comment", "preamble"]
            .iter()
            .any(|k| kind.eq_ignore_ascii_case(k))
        {
            continue;
        }
        let comma = body.find(',').ok_or("an entry without fields")?;
        let key = body[..comma].trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err("an entry without a key".into());
        }
        let fields = parse_fields(&body[comma + 1..], close)?;
        return Ok(BibEntry {
            kind: kind.to_ascii_lowercase(),
            key: key.to_owned(),
            fields,
        });
    }
}

/// `name = value` pairs up to the entry's closing delimiter.
fn parse_fields(text: &str, close: u8) -> Result<Vec<(String, String)>, String> {
    let bytes = text.as_bytes();
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            return Err("the entry is not closed".into());
        }
        if bytes[i] == close {
            return Ok(fields);
        }
        let eq = text[i..].find('=').ok_or("a field without a value")? + i;
        let name = text[i..eq].trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b))
        {
            return Err(format!("a malformed field name “{name}”"));
        }
        i = eq + 1;
        let start = i;
        let (mut depth, mut quoted) = (0usize, false);
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 1,
                b'{' => depth += 1,
                b'}' if depth > 0 => depth -= 1,
                b'"' if depth == 0 => quoted = !quoted,
                b',' if depth == 0 && !quoted => break,
                c if c == close && depth == 0 && !quoted => break,
                _ => {}
            }
            i += 1;
        }
        if i >= bytes.len() {
            return Err("the entry is not closed".into());
        }
        let value = text[start..i].trim();
        if value.is_empty() {
            return Err(format!("the field “{name}” has no value"));
        }
        fields.push((name.to_ascii_lowercase(), value.to_owned()));
    }
}

/// The keys of the entries in `bib`.
pub fn existing_keys(bib: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = bib;
    while let Some(at) = rest.find('@') {
        rest = &rest[at + 1..];
        let Some(open) = rest.find(['{', '(']) else {
            break;
        };
        let kind = rest[..open].trim();
        if kind.is_empty()
            || !kind.bytes().all(|b| b.is_ascii_alphanumeric())
            || ["string", "comment", "preamble"]
                .iter()
                .any(|k| kind.eq_ignore_ascii_case(k))
        {
            continue;
        }
        let body = &rest[open + 1..];
        if let Some(comma) = body.find([',', '\n']) {
            let key = body[..comma].trim();
            if !key.is_empty() {
                keys.push(key.to_owned());
            }
        }
    }
    keys
}

/// `key`, or `key` with the first of `a`…`z`, `2`, `3`, … that is not in `existing`
/// (compared case-insensitively, as BibTeX does).
pub fn unique_key(key: &str, existing: &[String]) -> String {
    let taken = |k: &str| existing.iter().any(|e| e.eq_ignore_ascii_case(k));
    if !taken(key) {
        return key.to_owned();
    }
    ('a'..='z')
        .map(|c| format!("{key}{c}"))
        .chain((2..).map(|n| format!("{key}{n}")))
        .find(|k| !taken(k))
        .unwrap_or_else(|| key.to_owned())
}

/// The key of the entry in `bib` that already holds `id` (its DOI, or its arXiv ID without a
/// version).
pub fn find_duplicate(bib: &str, id: &Identifier) -> Option<String> {
    let needle = id.search_key();
    // ASCII lower-casing keeps byte offsets, so a match's offset is valid in `bib` too (Unicode
    // lower-casing does not: `İ` gets longer, the Kelvin sign shorter).
    let lower = bib.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        from = at + needle.len();
        // Whole IDs only: `10.1/ab` is not in `10.1/abc`, nor `2301.0123` in `2301.01234`.
        let after = lower[from..].chars().next();
        let before = lower[..at].chars().next_back();
        let id_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-' | '_');
        if after.is_some_and(|c| id_char(c) && !(matches!(id, Identifier::Arxiv(_)) && c == 'v'))
            || before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.')
        {
            continue;
        }
        // An ID before the first entry (in a comment) is not an entry's.
        let Some(entry_start) = bib[..at].rfind('@') else {
            continue;
        };
        return parse_bibtex(&bib[entry_start..])
            .ok()
            .map(|e| e.key)
            .or_else(|| existing_keys(&bib[entry_start..]).into_iter().next());
    }
    None
}

/// The edit appending `entry` to the text `bib`, after a blank line.
pub fn append_edit(bib: &str, entry: &str) -> TextEdit {
    let sep = if bib.trim().is_empty() || bib.ends_with("\n\n") {
        ""
    } else if bib.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    TextEdit::insert(bib.len(), format!("{sep}{entry}"))
}

/// A key `lastname2020word` from the first author's last name, the year and the first word of
/// the title that is not a short function word; `None` without an author or a year.
pub fn make_key(entry: &BibEntry) -> Option<String> {
    let author = entry.field("author")?;
    let first = author.split(" and ").next()?.trim();
    let last = match first.split_once(',') {
        Some((last, _)) => last.trim(),
        None => first
            .split_whitespace()
            .rev()
            .find(|w| !matches!(w.trim_end_matches('.'), "Jr" | "Sr" | "II" | "III"))?,
    };
    let last = ascii_word(last);
    let year: String = entry
        .field("year")
        .or_else(|| entry.field("date"))?
        .chars()
        .filter(char::is_ascii_digit)
        .take(4)
        .collect();
    if last.is_empty() || year.len() != 4 {
        return None;
    }
    const SKIP: &[&str] = &[
        "a", "an", "the", "on", "of", "and", "in", "for", "to", "with", "at", "by", "from", "some",
        "its", "is", "are", "le", "la", "les", "des", "du", "de", "sur", "une", "un", "der", "die",
        "das", "über", "uber", "ein", "eine", "zur", "zum",
    ];
    let word = entry
        .field("title")
        .unwrap_or("")
        .split_whitespace()
        .filter(|w| !w.starts_with('$') && !w.starts_with('\\'))
        .map(ascii_word)
        .find(|w| !w.is_empty() && !SKIP.contains(&w.as_str()))
        .unwrap_or_default();
    Some(format!("{last}{year}{word}"))
}

/// `s` in lower-case ASCII letters and digits: accents are dropped (`é` → `e`, `ß` → `ss`),
/// TeX accent commands (`\'e`, `{\"o}`) and everything else go.
fn ascii_word(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let folded = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => {
                "a"
            }
            'ç' | 'Ç' | 'č' | 'Č' | 'ć' | 'Ć' => "c",
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' | 'ě' | 'Ě' => "e",
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => "i",
            'ñ' | 'Ñ' | 'ň' | 'ń' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => {
                "o"
            }
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'ů' => "u",
            'ý' | 'ÿ' | 'Ý' => "y",
            'š' | 'Š' | 'ś' => "s",
            'ž' | 'Ž' | 'ź' | 'ż' => "z",
            'ř' | 'Ř' => "r",
            'ł' | 'Ł' => "l",
            'ß' => "ss",
            'æ' | 'Æ' => "ae",
            'œ' | 'Œ' => "oe",
            c if c.is_ascii_alphanumeric() => {
                out.push(c.to_ascii_lowercase());
                continue;
            }
            _ => continue,
        };
        out.push_str(folded);
    }
    out
}

// ----- arXiv -----

/// Converts the arXiv API's Atom answer for `id` into a `@misc` entry (key from
/// [`make_key`], else `arXiv:<id>`).
pub fn arxiv_to_bibtex(atom: &str, id: &str) -> Result<BibEntry, String> {
    let entry = element(atom, "entry").ok_or_else(|| format!("No arXiv record for {id}"))?;
    let entry_id = element(entry, "id").map(xml_text).unwrap_or_default();
    if entry_id.contains("/api/errors") {
        let why = element(entry, "summary").map(xml_text).unwrap_or_default();
        return Err(format!(
            "arXiv: {}",
            if why.is_empty() { "unknown ID" } else { &why }
        ));
    }
    let title = element(entry, "title")
        .map(xml_text)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("No arXiv record for {id}"))?;
    let mut authors = Vec::new();
    let mut rest = entry;
    while let Some(author) = element(rest, "author") {
        if let Some(name) = element(author, "name") {
            authors.push(xml_text(name));
        }
        // `author` is a slice of `rest`: continue after it.
        let end = author.as_ptr() as usize - rest.as_ptr() as usize + author.len();
        rest = &rest[end..];
    }
    let year: String = element(entry, "published")
        .map(xml_text)
        .unwrap_or_default()
        .chars()
        .take(4)
        .collect();
    // The record's ID (with its version) names the article, not what the user typed.
    let eprint = entry_id
        .rsplit_once("/abs/")
        .map_or_else(|| id.to_owned(), |(_, id)| id.to_owned());
    let eprint = strip_version(&eprint).to_owned();
    let class = attribute(entry, "arxiv:primary_category", "term");
    let doi = element(entry, "arxiv:doi").map(xml_text);
    let journal = element(entry, "arxiv:journal_ref").map(xml_text);
    let braced = |s: &str| format!("{{{}}}", bib_text(s));
    let mut fields = vec![
        ("author".to_owned(), braced(&authors.join(" and "))),
        ("title".to_owned(), braced(&title)),
    ];
    if year.len() == 4 {
        fields.push(("year".into(), braced(&year)));
    }
    fields.push(("eprint".into(), braced(&eprint)));
    fields.push(("archiveprefix".into(), "{arXiv}".into()));
    if let Some(class) = class {
        fields.push(("primaryclass".into(), braced(&class)));
    }
    if let Some(doi) = doi.filter(|d| !d.is_empty()) {
        fields.push(("doi".into(), braced(&doi)));
    }
    if let Some(journal) = journal.filter(|j| !j.is_empty()) {
        fields.push(("note".into(), braced(&journal)));
    }
    fields.push(("url".into(), format!("{{https://arxiv.org/abs/{eprint}}}")));
    let mut entry = BibEntry {
        kind: "misc".into(),
        key: format!("arXiv:{eprint}"),
        fields,
    };
    if let Some(key) = make_key(&entry) {
        entry.key = key;
    }
    Ok(entry)
}

/// The inner text of the first `<name>` or `<name …>` element in `xml`.
fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let mut from = 0;
    loop {
        let at = xml[from..].find(&open)? + from;
        let after = &xml[at + open.len()..];
        from = at + open.len();
        match after.chars().next() {
            Some('>') | Some(' ') | Some('\n') | Some('\t') | Some('\r') | Some('/') => {}
            _ => continue,
        }
        let gt = after.find('>')?;
        if after[..gt].ends_with('/') {
            // Empty, and still a slice of `xml` (callers continue after it).
            return Some(&after[gt..gt]);
        }
        let inner = &after[gt + 1..];
        let close = format!("</{name}>");
        return inner.find(&close).map(|end| &inner[..end]);
    }
}

/// The value of `attr` on the first `<name …>` tag in `xml`.
fn attribute(xml: &str, name: &str, attr: &str) -> Option<String> {
    let open = format!("<{name}");
    let at = xml.find(&open)?;
    let tag = &xml[at..at + xml[at..].find('>')?];
    let key = format!("{attr}=");
    let v = &tag[tag.find(&key)? + key.len()..];
    let quote = v.chars().next()?;
    let v = &v[1..];
    Some(xml_unescape(&v[..v.find(quote)?]))
}

/// Element text: entities decoded, runs of blanks (Atom wraps titles) made one space.
fn xml_text(s: &str) -> String {
    xml_unescape(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s` made safe inside a braced BibTeX value: unescaped `&`, `%` and `#` are escaped, and
/// unbalanced braces are dropped.
fn bib_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = '\0';
    for c in s.chars() {
        if matches!(c, '&' | '%' | '#') && prev != '\\' {
            out.push('\\');
        }
        out.push(c);
        prev = c;
    }
    let mut depth = 0i32;
    let balanced = out.chars().all(|c| {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        depth >= 0
    }) && depth == 0;
    if balanced {
        out
    } else {
        out.replace(['{', '}'], "")
    }
}

// ----- applying -----

/// What [`add_citation`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Added {
    /// The entry's key.
    pub key: String,
    /// False when the file already had the entry (nothing was appended).
    pub appended: bool,
}

/// Fetches `input`'s entry and appends it to `bib` as a reviewable hunk; with `cite`, inserts
/// `\cite{key}` at `cite`'s cursor afterwards (a normal edit).
pub async fn add_citation(
    state: &Rc<WindowState>,
    input: &str,
    bib: &Path,
    cite: Option<&Rc<Editor>>,
) -> Result<Added, String> {
    let id = parse_identifier(input)?;
    let bib = normalize(bib);
    let new_file = state.workspace.get(&bib).is_none() && !bib.exists();
    let doc = if new_file {
        state.workspace.create_closed(&bib)
    } else {
        state.workspace.get_or_load(&bib).await?
    };
    let added = match doc.with_text(|text| find_duplicate(text, &id)) {
        Some(key) => Added {
            key,
            appended: false,
        },
        None => {
            let endpoints = Endpoints::from_env();
            let fetched = id.clone();
            let mut entry = gio::spawn_blocking(move || fetch(&endpoints, &fetched))
                .await
                .unwrap_or_else(|_| Err("Fetching the entry failed".into()))?;
            // The text may have changed while the request ran.
            let edit = doc.with_text(|text| {
                entry.key = unique_key(&entry.key, &existing_keys(text));
                append_edit(text, &entry.to_bibtex())
            });
            review_ui::start_review(&doc, new_file);
            review_ui::outside_turn(&doc, || doc.apply_edits(&[edit], Origin::Agent { call: 0 }))
                .map_err(|e| format!("Cannot add the entry to {}: {e}", doc.display_name()))?;
            review_ui::recompute(&doc);
            doc.save()
                .await
                .map_err(|e| format!("Added, but saving {} failed: {e}", doc.display_name()))?;
            Added {
                key: entry.key,
                appended: true,
            }
        }
    };
    if let Some(editor) = cite
        && editor.document().is_open()
    {
        let buffer = editor.buffer();
        buffer.begin_user_action();
        buffer.insert_at_cursor(&format!("\\cite{{{}}}", added.key));
        buffer.end_user_action();
    }
    Ok(added)
}

/// Adds `win.add-citation`, its menu item and its palette name.
pub fn install(state: &Rc<WindowState>) {
    let action = gio::SimpleAction::new(ACTION, None);
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, _| {
        if let Some(state) = weak.upgrade() {
            show(&state);
        }
    });
    state.window.add_action(&action);
    let detailed = format!("win.{ACTION}");
    crate::app::describe_action(
        &detailed,
        "Add Citation from arXiv ID or DOI",
        crate::app::ShortcutGroup::Editing,
        None,
    );
    state
        .header
        .menu_files
        .append(Some("Add _Citation from arXiv ID or DOI…"), Some(&detailed));
}

// ----- the dialog -----

/// The open "Add Citation" dialog.
pub struct CitationDialog {
    state: Weak<WindowState>,
    pub dialog: adw::Dialog,
    pub entry: adw::EntryRow,
    /// The `.bib` files offered, in the combo row's order.
    pub bibs: Vec<PathBuf>,
    pub bib_row: adw::ComboRow,
    pub cite_row: adw::SwitchRow,
    pub add_button: gtk::Button,
    spinner: adw::Spinner,
    error: gtk::Label,
    /// The editor `\cite` goes into.
    editor: Option<Weak<Editor>>,
    busy: Cell<bool>,
}

thread_local! {
    static DIALOGS: RefCell<Vec<Rc<CitationDialog>>> = const { RefCell::new(Vec::new()) };
}

/// The citation dialog open over `state`'s window (tests).
pub fn citation_dialog(state: &WindowState) -> Option<Rc<CitationDialog>> {
    DIALOGS.with_borrow(|list| {
        list.iter()
            .find(|d| std::ptr::eq(d.state.as_ptr(), state))
            .cloned()
    })
}

/// The project's `.bib` files (blocking walk), the ones the active document names first; a new
/// `references.bib` in the root when there is none.
fn bib_files(root: &Path, named: &[String]) -> Vec<PathBuf> {
    let walked = walk_project(root, &[root.join(".build")], 5000, 500);
    let mut bibs: Vec<PathBuf> = walked
        .files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bib")))
        .collect();
    bibs.sort_by_key(|p| {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
        (!named.iter().any(|n| Some(n) == stem.as_ref()), p.clone())
    });
    if bibs.is_empty() {
        bibs.push(root.join("references.bib"));
    }
    bibs
}

/// The bibliography names in `text` (`\bibliography{a,b}`, `\addbibresource{a.bib}`), as stems.
fn named_bibs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for command in ["\\bibliography{", "\\addbibresource{"] {
        let mut rest = text;
        while let Some(at) = rest.find(command) {
            rest = &rest[at + command.len()..];
            let Some(end) = rest.find('}') else { break };
            for name in rest[..end].split(',') {
                let name = name.trim();
                let name = name.rsplit('/').next().unwrap_or(name);
                out.push(name.strip_suffix(".bib").unwrap_or(name).to_owned());
            }
        }
    }
    out
}

/// `win.add-citation`: opens the dialog (or presents the open one). The dialog appears once the
/// project's `.bib` files are listed.
pub fn show(state: &Rc<WindowState>) {
    if let Some(open) = citation_dialog(state) {
        open.dialog.present(Some(&state.window));
        return;
    }
    let state = state.clone();
    glib::spawn_future_local(async move {
        let editor = state.active_editor();
        let named = editor
            .as_ref()
            .map(|e| e.document().with_text(named_bibs))
            .unwrap_or_default();
        let root = state.workspace.root().path().to_path_buf();
        let bibs = gio::spawn_blocking(move || bib_files(&root, &named))
            .await
            .unwrap_or_default();
        if citation_dialog(&state).is_some() || bibs.is_empty() {
            return;
        }
        let d = CitationDialog::new(&state, bibs, editor.as_ref());
        DIALOGS.with_borrow_mut(|list| list.push(d.clone()));
        d.dialog.connect_closed(|dialog| {
            DIALOGS.with_borrow_mut(|list| list.retain(|d| &d.dialog != dialog));
        });
        d.dialog.set_focus(Some(&d.entry));
        d.dialog.present(Some(&state.window));
    });
}

impl CitationDialog {
    fn new(state: &Rc<WindowState>, bibs: Vec<PathBuf>, editor: Option<&Rc<Editor>>) -> Rc<Self> {
        let root = state.workspace.root().path().to_path_buf();
        let entry = adw::EntryRow::builder()
            .title("arXiv ID or DOI")
            .activates_default(true)
            .build();
        let names: Vec<String> = bibs
            .iter()
            .map(|p| {
                let name = p.strip_prefix(&root).unwrap_or(p).display().to_string();
                if p.exists() {
                    name
                } else {
                    format!("{name} (new)")
                }
            })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let bib_row = adw::ComboRow::builder()
            .title("Bibliography")
            .model(&gtk::StringList::new(&names))
            .build();
        let tex_editor = editor.filter(|e| {
            e.document()
                .path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("tex"))
        });
        let cite_row = adw::SwitchRow::builder()
            .title("Insert \\cite at the cursor")
            .active(tex_editor.is_some())
            .sensitive(tex_editor.is_some())
            .build();
        let group = adw::PreferencesGroup::builder()
            .description(
                "The entry is fetched from doi.org or arXiv and added to the bibliography as a \
                 change to review.",
            )
            .build();
        group.add(&entry);
        group.add(&bib_row);
        group.add(&cite_row);
        let spinner = adw::Spinner::builder().visible(false).build();
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["error"])
            .build();
        let status = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        status.append(&spinner);
        status.append(&error);
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        page.append(&group);
        page.append(&status);
        let add_button = gtk::Button::builder()
            .label("_Add")
            .use_underline(true)
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::new();
        header.pack_end(&add_button);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&page));
        let dialog = adw::Dialog::builder()
            .title("Add Citation")
            .content_width(460)
            .child(&toolbar)
            .default_widget(&add_button)
            .build();
        let this = Rc::new(Self {
            state: Rc::downgrade(state),
            dialog,
            entry,
            bibs,
            bib_row,
            cite_row,
            add_button,
            spinner,
            error,
            editor: tex_editor.map(Rc::downgrade),
            busy: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.entry.connect_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                let ok = parse_identifier(&entry.text()).is_ok();
                this.add_button.set_sensitive(ok && !this.busy.get());
                this.show_error(None);
            }
        });
        let weak = Rc::downgrade(&this);
        this.add_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.add();
            }
        });
        this
    }

    /// The `.bib` file chosen.
    pub fn bib(&self) -> Option<PathBuf> {
        self.bibs.get(self.bib_row.selected() as usize).cloned()
    }

    /// The error shown, if any.
    pub fn error_text(&self) -> Option<String> {
        self.error
            .is_visible()
            .then(|| self.error.label().to_string())
    }

    /// True while an entry is being fetched.
    pub fn is_busy(&self) -> bool {
        self.busy.get()
    }

    fn show_error(&self, message: Option<&str>) {
        self.error.set_visible(message.is_some());
        self.error.set_label(message.unwrap_or(""));
    }

    /// Fetches and adds the entry; the dialog closes when it was added and shows the error
    /// otherwise.
    pub fn add(self: &Rc<Self>) {
        let (Some(state), Some(bib)) = (self.state.upgrade(), self.bib()) else {
            return;
        };
        let input = self.entry.text().to_string();
        if let Err(e) = parse_identifier(&input) {
            self.show_error(Some(&e));
            return;
        }
        if self.busy.replace(true) {
            return;
        }
        self.add_button.set_sensitive(false);
        self.spinner.set_visible(true);
        self.show_error(None);
        let editor = self
            .cite_row
            .is_active()
            .then(|| self.editor.as_ref().and_then(Weak::upgrade))
            .flatten();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = add_citation(&state, &input, &bib, editor.as_ref()).await;
            this.busy.set(false);
            this.spinner.set_visible(false);
            this.add_button.set_sensitive(true);
            match result {
                Ok(added) => {
                    this.dialog.close();
                    let name = bib
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let message = if added.appended {
                        format!("Added {} to {name}: review the new entry", added.key)
                    } else {
                        format!("{name} already has this entry as {}", added.key)
                    };
                    // Plain text: file names and keys may contain `&` or `<`.
                    let toast = adw::Toast::builder()
                        .title(&message)
                        .use_markup(false)
                        .build();
                    toast.set_button_label(Some("_Show"));
                    let weak = Rc::downgrade(&state);
                    toast.connect_button_clicked(move |_| {
                        if let Some(state) = weak.upgrade() {
                            let bib = bib.clone();
                            glib::spawn_future_local(async move {
                                if let Err(e) = state.open_file(&bib).await {
                                    state.toast(&e);
                                }
                            });
                        }
                    });
                    state.toasts.add_toast(toast);
                }
                Err(e) => this.show_error(Some(&e)),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    #[test]
    fn identifiers() {
        let doi = |s: &str| Identifier::Doi(s.into());
        let arxiv = |s: &str| Identifier::Arxiv(s.into());
        assert_eq!(
            parse_identifier("10.1007/BF02684778"),
            Ok(doi("10.1007/BF02684778"))
        );
        assert_eq!(
            parse_identifier(" doi:10.1000/x.y(z) "),
            Ok(doi("10.1000/x.y(z)"))
        );
        assert_eq!(
            parse_identifier("https://doi.org/10.4007/annals.2024.1"),
            Ok(doi("10.4007/annals.2024.1"))
        );
        assert_eq!(parse_identifier("2301.01234"), Ok(arxiv("2301.01234")));
        assert_eq!(
            parse_identifier("arXiv:2301.01234v2"),
            Ok(arxiv("2301.01234v2"))
        );
        assert_eq!(parse_identifier("0704.0001"), Ok(arxiv("0704.0001")));
        assert_eq!(parse_identifier("math/0211159"), Ok(arxiv("math/0211159")));
        assert_eq!(
            parse_identifier("math.AG/0211159v1"),
            Ok(arxiv("math.AG/0211159v1"))
        );
        assert_eq!(
            parse_identifier("https://arxiv.org/pdf/2301.01234v1.pdf"),
            Ok(arxiv("2301.01234v1"))
        );
        for bad in [
            "",
            "hello",
            "10.12/x",
            "10.1000/",
            "2301.123",
            "doi:2301.01234",
            "10.1000/a b",
        ] {
            assert!(parse_identifier(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_are_encoded() {
        let e = Endpoints::default();
        assert_eq!(
            e.url(&Identifier::Doi("10.1016/0001-8708(78)90013-0".into())),
            "https://doi.org/10.1016/0001-8708%2878%2990013-0"
        );
        assert_eq!(
            e.url(&Identifier::Arxiv("math/0211159".into())),
            "https://export.arxiv.org/api/query?id_list=math/0211159"
        );
    }

    const CROSSREF: &str = " @article{Grothendieck_1957, title={Sur quelques points d’algèbre homologique, I}, volume={9}, ISSN={0040-8735}, url={http://dx.doi.org/10.2748/tmj/1178244839}, DOI={10.2748/tmj/1178244839}, number={2}, journal={Tohoku Mathematical Journal}, publisher={Mathematical Institute, Tohoku University}, author={Grothendieck, Alexander}, year={1957}, month=jan, pages={119–221} }\n";

    #[test]
    fn crossref_bibtex_is_reformatted() {
        let mut entry = parse_bibtex(CROSSREF).unwrap();
        assert_eq!(entry.kind, "article");
        assert_eq!(entry.key, "Grothendieck_1957");
        assert_eq!(entry.field("doi"), Some("10.2748/tmj/1178244839"));
        assert_eq!(entry.field("month"), Some("jan"));
        assert_eq!(
            make_key(&entry).as_deref(),
            Some("grothendieck1957quelques")
        );
        entry.key = make_key(&entry).unwrap();
        let text = entry.to_bibtex();
        assert!(
            text.starts_with("@article{grothendieck1957quelques,\n  title     = {Sur"),
            "{text}"
        );
        assert!(text.contains("\n  month     = jan,\n"), "{text}");
        assert!(text.ends_with("}\n"));
        assert_eq!(parse_bibtex(&text).unwrap(), entry);
    }

    #[test]
    fn bibtex_parsing_edge_cases() {
        let e = parse_bibtex("@string{x = \"y\"}\n@Book{k, title = \"A {B}, C\", year = 2001}")
            .unwrap();
        assert_eq!(e.kind, "book");
        assert_eq!(e.field("title"), Some("A {B}, C"));
        assert_eq!(e.field("year"), Some("2001"));
        assert!(parse_bibtex("<html>not found</html>").is_err());
        assert!(parse_bibtex("@article{k, title = {open").is_err());
    }

    #[test]
    fn keys_and_duplicates() {
        let bib = "@article{smith2020, doi = {10.1000/ABC}}\n@misc{Jones2021a,\n eprint = {2301.01234}}\n@comment{x}\n";
        assert_eq!(existing_keys(bib), ["smith2020", "Jones2021a"]);
        let keys = existing_keys(bib);
        assert_eq!(unique_key("new", &keys), "new");
        assert_eq!(unique_key("Smith2020", &keys), "Smith2020a");
        assert_eq!(unique_key("jones2021", &[]), "jones2021");
        assert_eq!(
            find_duplicate(bib, &Identifier::Doi("10.1000/abc".into())).as_deref(),
            Some("smith2020")
        );
        assert_eq!(
            find_duplicate(bib, &Identifier::Doi("10.1000/ab".into())),
            None
        );
        assert_eq!(
            find_duplicate(bib, &Identifier::Arxiv("2301.01234v3".into())).as_deref(),
            Some("Jones2021a")
        );
        assert_eq!(
            find_duplicate(bib, &Identifier::Arxiv("2301.0123".into())),
            None
        );
    }

    /// Unicode lower-casing changes byte lengths (`İ` gets longer, the Kelvin sign shorter): the
    /// offsets of a match must still be valid in the original text.
    #[test]
    fn regress5_bib_dup_unicode_offsets() {
        let ist = "İ".repeat(20);
        let bib = format!(
            "@book{{ist, publisher={{{ist}}}}}\n@article{{right, doi={{10.1000/x}}}}\n@article{{wrong, title={{t}}}}\n"
        );
        assert_eq!(
            find_duplicate(&bib, &Identifier::Doi("10.1000/x".into())).as_deref(),
            Some("right")
        );
        // A shifted offset inside a multi-byte character must not panic.
        let bib = format!(
            "@book{{a, publisher={{{ist}}}}}\n@article{{b, doi={{10.1000/y}}, title={{şşşşşşşş}}}}"
        );
        assert_eq!(
            find_duplicate(&bib, &Identifier::Doi("10.1000/Y".into())).as_deref(),
            Some("b")
        );
        // Characters that get shorter (the Kelvin sign) before the entry.
        let bib = "% \u{212A}\u{212A}\u{212A}\n@misc{k, eprint={2301.01234}}\n";
        assert_eq!(
            find_duplicate(bib, &Identifier::Arxiv("2301.01234".into())).as_deref(),
            Some("k")
        );
        // An ID in a comment before the first entry does not hide a later duplicate.
        let bib = "% see 10.1000/z\n@article{later, doi={10.1000/z}}\n";
        assert_eq!(
            find_duplicate(bib, &Identifier::Doi("10.1000/z".into())).as_deref(),
            Some("later")
        );
    }

    #[test]
    fn appending_leaves_one_blank_line() {
        let e = "@misc{k,\n}\n";
        assert_eq!(append_edit("", e).text, e);
        assert_eq!(append_edit("@a{x,}\n", e).text, format!("\n{e}"));
        assert_eq!(append_edit("@a{x,}", e).text, format!("\n\n{e}"));
        assert_eq!(append_edit("@a{x,}\n\n", e).text, e);
        assert_eq!(append_edit("@a{x,}", e).range, 6..6);
    }

    pub(super) const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <title type="html">ArXiv Query: id_list=2301.01234</title>
  <entry>
    <id>http://arxiv.org/abs/2301.01234v2</id>
    <published>2023-01-03T18:00:00Z</published>
    <title>On the Étale Cohomology of
      Schemes &amp; Stacks with 100% $\mathbb{Z}$-coefficients</title>
    <summary>Abstract.</summary>
    <author><name>Jean-Pierre Müller</name></author>
    <author><name>Ada Lovelace</name><arxiv:affiliation>X</arxiv:affiliation></author>
    <arxiv:doi>10.1000/xyz</arxiv:doi>
    <arxiv:journal_ref>J. Alg. Geom. 1 (2024)</arxiv:journal_ref>
    <arxiv:primary_category term="math.AG" scheme="http://arxiv.org/schemas/atom"/>
  </entry>
</feed>"#;

    #[test]
    fn arxiv_atom_becomes_misc() {
        let e = arxiv_to_bibtex(ATOM, "2301.01234").unwrap();
        assert_eq!(e.kind, "misc");
        assert_eq!(e.key, "muller2023etale");
        assert_eq!(
            e.field("author"),
            Some("Jean-Pierre Müller and Ada Lovelace")
        );
        assert_eq!(
            e.field("title"),
            Some(
                "On the Étale Cohomology of Schemes \\& Stacks with 100\\% $\\mathbb{Z}$-coefficients"
            )
        );
        assert_eq!(e.field("year"), Some("2023"));
        assert_eq!(e.field("eprint"), Some("2301.01234"));
        assert_eq!(e.field("archiveprefix"), Some("arXiv"));
        assert_eq!(e.field("primaryclass"), Some("math.AG"));
        assert_eq!(e.field("doi"), Some("10.1000/xyz"));
        assert_eq!(e.field("note"), Some("J. Alg. Geom. 1 (2024)"));
        assert_eq!(e.field("url"), Some("https://arxiv.org/abs/2301.01234"));
        assert_eq!(parse_bibtex(&e.to_bibtex()).unwrap(), e);
        let error = r#"<feed><entry><id>http://arxiv.org/api/errors#incorrect_id_format_for_1</id><title>Error</title><summary>incorrect id format for 1</summary></entry></feed>"#;
        assert!(
            arxiv_to_bibtex(error, "1")
                .unwrap_err()
                .contains("incorrect id format")
        );
        assert!(arxiv_to_bibtex("<feed></feed>", "2301.99999").is_err());
    }

    /// Serves `answers` (status, body) to successive connections on a local port and returns
    /// its base URL and the request lines and `Accept` headers it saw.
    fn fake_server(answers: Vec<(u16, String)>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut seen = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if line.starts_with("GET") || line.to_ascii_lowercase().starts_with("accept:") {
                        seen.push_str(line.trim_end());
                        seen.push('|');
                    }
                }
                let _ = tx.send(seen);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
                let _ = stream.read(&mut [0; 1]);
            }
        });
        (base, rx)
    }

    #[test]
    fn fetch_from_a_fake_server() {
        let (base, seen) = fake_server(vec![
            (200, CROSSREF.into()),
            (404, "DOI Not Found".into()),
            (200, ATOM.into()),
            (500, "oops".into()),
        ]);
        let endpoints = Endpoints {
            doi: base.clone(),
            arxiv: format!("{base}/api/query"),
        };
        let e = fetch(
            &endpoints,
            &Identifier::Doi("10.2748/tmj/1178244839".into()),
        )
        .unwrap();
        assert_eq!(e.key, "grothendieck1957quelques");
        let request = seen.recv().unwrap();
        assert!(
            request.contains("GET /10.2748/tmj/1178244839 "),
            "{request}"
        );
        assert!(request.contains("application/x-bibtex"), "{request}");
        let err = fetch(&endpoints, &Identifier::Doi("10.1000/none".into())).unwrap_err();
        assert!(err.contains("No record found"), "{err}");
        let e = fetch(&endpoints, &Identifier::Arxiv("2301.01234".into())).unwrap();
        assert_eq!(e.key, "muller2023etale");
        let _ = seen.recv().unwrap();
        let request = seen.recv().unwrap();
        assert!(
            request.contains("GET /api/query?id_list=2301.01234 "),
            "{request}"
        );
        let err = fetch(&endpoints, &Identifier::Arxiv("2301.01234".into())).unwrap_err();
        assert!(err.contains("500"), "{err}");
    }

    #[test]
    fn named_bibliographies() {
        assert_eq!(
            named_bibs("\\bibliography{refs, extra/more}\n\\addbibresource{lit.bib}"),
            ["refs", "more", "lit"]
        );
    }
}
