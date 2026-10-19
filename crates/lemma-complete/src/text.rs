//! Display helpers for completion details: TeX markup in `.bib` fields, author lists, truncation.

/// Precomposed letters for TeX accents: (accent, base letters, accented letters).
const ACCENTS: &[(char, &str, &str)] = &[
    ('\'', "aeiouyAEIOUYcnszCNSZ", "áéíóúýÁÉÍÓÚÝćńśźĆŃŚŹ"),
    ('`', "aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
    ('^', "aeiouAEIOU", "âêîôûÂÊÎÔÛ"),
    ('"', "aeiouyAEIOUY", "äëïöüÿÄËÏÖÜŸ"),
    ('~', "anoANO", "ãñõÃÑÕ"),
    ('=', "aeiouAEIOU", "āēīōūĀĒĪŌŪ"),
    ('.', "zZeE", "żŻėĖ"),
    ('c', "cCsS", "çÇşŞ"),
    ('v', "cCsSzZrReEnN", "čČšŠžŽřŘěĚňŇ"),
    ('u', "aAgG", "ăĂğĞ"),
    ('H', "oOuU", "őŐűŰ"),
    ('r', "aAuU", "åÅůŮ"),
    ('k', "aAeE", "ąĄęĘ"),
];

/// Control words that stand for a letter.
const LETTERS: &[(&str, &str)] = &[
    ("ss", "ß"),
    ("o", "ø"),
    ("O", "Ø"),
    ("ae", "æ"),
    ("AE", "Æ"),
    ("oe", "œ"),
    ("OE", "Œ"),
    ("aa", "å"),
    ("AA", "Å"),
    ("l", "ł"),
    ("L", "Ł"),
    ("i", "ı"),
    ("j", "ȷ"),
];

fn accented(accent: char, base: char) -> Option<char> {
    let (_, from, to) = ACCENTS.iter().find(|(a, _, _)| *a == accent)?;
    let i = from.chars().position(|c| c == base)?;
    to.chars().nth(i)
}

/// Plain text from TeX markup: accents become letters (`{\'e}` → é, `\"o` → ö), braces and
/// other control words disappear, `~` is a space and whitespace collapses.
pub(crate) fn detex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' | '}' => i += 1,
            '~' => {
                out.push(' ');
                i += 1;
            }
            '\\' => {
                i += 1;
                let Some(&next) = chars.get(i) else { break };
                if next.is_ascii_alphabetic() {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let accent = word.chars().next().filter(|_| word.len() == 1);
                    if let Some(a) = accent.filter(|a| "cvuHrk".contains(*a))
                        && let Some((base, used)) = accent_argument(&chars[i..])
                        && let Some(letter) = accented(a, base)
                    {
                        out.push(letter);
                        i += used;
                    } else if let Some((_, letter)) = LETTERS.iter().find(|(w, _)| *w == word) {
                        out.push_str(letter);
                        while chars.get(i) == Some(&' ') {
                            i += 1;
                        }
                    }
                } else if "'`^\"~=.".contains(next) {
                    i += 1;
                    if let Some((base, used)) = accent_argument(&chars[i..]) {
                        out.push(accented(next, base).unwrap_or(base));
                        i += used;
                    }
                } else {
                    // `\&`, `\%`, `\_`, `\$`, `\ `: the character itself.
                    out.push(next);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    collapse_whitespace(&out)
}

/// The letter an accent applies to: `e`, `{e}`, ` e` or `{\i}`; returns it and the chars used.
fn accent_argument(rest: &[char]) -> Option<(char, usize)> {
    let mut i = 0;
    while rest.get(i) == Some(&' ') {
        i += 1;
    }
    match rest.get(i)? {
        '{' => {
            let close = rest[i..].iter().position(|&c| c == '}')? + i;
            let inner: String = rest[i + 1..close].iter().collect();
            let base = match inner.trim() {
                "\\i" => 'i',
                "\\j" => 'j',
                s => {
                    let mut cs = s.chars();
                    match (cs.next(), cs.next()) {
                        (Some(c), None) => c,
                        _ => return None,
                    }
                }
            };
            Some((base, close + 1))
        }
        '\\' => {
            // `\'\i`
            match rest.get(i + 1)? {
                'i' => Some(('i', i + 2)),
                'j' => Some(('j', i + 2)),
                _ => None,
            }
        }
        &c if c.is_alphabetic() => Some((c, i + 1)),
        _ => None,
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits `s` at ` and ` outside braces.
fn split_names(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            c if depth == 0
                && c.is_ascii_whitespace()
                && s[i..].len() > 5
                && s[i + 1..].starts_with("and")
                && b.get(i + 4).is_some_and(u8::is_ascii_whitespace) =>
            {
                out.push(s[start..i].trim());
                i += 4;
                start = i;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].trim());
    out.retain(|n| !n.is_empty());
    out
}

/// The family name of one BibTeX name: `Mac Lane, Saunders` → `Mac Lane`,
/// `Saunders Mac Lane` → `Lane`, `{The Stacks project authors}` → as written.
///
/// A name whose last word starts with a lowercase letter is taken whole: it is an institution
/// whose protecting braces the `.bib` scanner removed (`The Stacks project authors`).
fn family_name(name: &str) -> String {
    let name = name.trim();
    if name.starts_with('{') && name.ends_with('}') && !name[1..name.len() - 1].contains('}') {
        return detex(name);
    }
    let mut depth = 0i32;
    for (i, b) in name.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => return detex(&name[..i]),
            _ => {}
        }
    }
    let mut depth = 0i32;
    let mut last = 0;
    for (i, b) in name.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b' ' if depth == 0 => last = i + 1,
            _ => {}
        }
    }
    if last > 0 && name[last..].starts_with(|c: char| c.is_lowercase()) {
        return detex(name);
    }
    detex(&name[last..])
}

/// `A and B` → `A and B`, three or more names (or `and others`) → `A et al.`, using family
/// names.
pub(crate) fn short_authors(authors: &str) -> String {
    let names = split_names(authors);
    let others = names.last().is_some_and(|n| *n == "others");
    let names: Vec<String> = names
        .iter()
        .filter(|n| **n != "others")
        .map(|n| family_name(n))
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [a] if !others => a.clone(),
        [a, b] if !others => format!("{a} and {b}"),
        [a, ..] => format!("{a} et al."),
    }
}

/// `s` cut to at most `max` characters, with `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_owned(),
        Some(_) => {
            let cut: String = s.chars().take(max.saturating_sub(1)).collect();
            format!("{}…", cut.trim_end())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detex_accents_and_markup() {
        assert_eq!(
            detex(r"Rev{\^e}tements {\'e}tales et groupe fondamental (SGA 1)"),
            "Revêtements étales et groupe fondamental (SGA 1)"
        );
        assert_eq!(detex(r#"K\"ahler"#), "Kähler");
        assert_eq!(detex(r"\v{C}ech"), "Čech");
        assert_eq!(detex(r"Mitchell-B{\'e}nabou"), "Mitchell-Bénabou");
        assert_eq!(detex(r"Erd\H{o}s"), "Erdős");
        assert_eq!(detex(r#"G\"{o}del"#), "Gödel");
        assert_eq!(detex(r"Stra\ss e"), "Straße");
        assert_eq!(detex(r#"Na\"{\i}ve \emph{set} theory"#), "Naïve set theory");
        assert_eq!(detex(r"A \& B~C"), "A & B C");
        assert_eq!(detex(r"\'{}"), "");
        assert_eq!(detex("x\\"), "x");
    }

    #[test]
    fn authors() {
        assert_eq!(short_authors("Grothendieck, Alexander"), "Grothendieck");
        assert_eq!(short_authors("Mac Lane, Saunders"), "Mac Lane");
        assert_eq!(
            short_authors("{The Stacks project authors}"),
            "The Stacks project authors"
        );
        assert_eq!(
            short_authors("Mac Lane, Saunders and Moerdijk, Ieke"),
            "Mac Lane and Moerdijk"
        );
        assert_eq!(short_authors("Saunders Mac Lane"), "Lane");
        assert_eq!(
            short_authors("The Stacks project authors"),
            "The Stacks project authors"
        );
        assert_eq!(short_authors("A, B and C, D and E, F"), "A et al.");
        assert_eq!(short_authors("A, B and others"), "A et al.");
        assert_eq!(short_authors("Ab{and}on, X"), "Abandon");
        assert_eq!(short_authors(""), "");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("a longer title here", 10), "a longer…");
        assert_eq!(truncate("ééééééé", 4), "ééé…");
    }
}
