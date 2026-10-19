//! [`WordList`]: dictionary words and the words seen in the project.

use lemma_syntax::{LineStates, Lines, ScanConfig, Token, TokenKind};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Words collected from the project's text must have at least this many letters.
const MIN_PROJECT_WORD: usize = 4;

/// Longer runs of letters are not words (base64, hashes).
const MAX_WORD: usize = 48;

/// Word-list entries longer than this (in characters or words) are prose, not entries.
const MAX_ENTRY_CHARS: usize = 64;
const MAX_ENTRY_WORDS: usize = 4;

/// Dictionary words for completion and spell checking, plus what the project's sources use.
///
/// - Word lists (`spell.word_lists`, one word or phrase per line) give
///   completion candidates and [`Self::spell_words`].
/// - LaTeX sources ([`Self::add_project_text`]) give the words of at least four letters in their
///   text, as completion candidates only (never spell words: a misspelling in the project must
///   stay flagged), and the control words they use, which rank higher in command completion
///   ("used in the project").
#[derive(Clone, Debug, Default)]
pub struct WordList {
    /// Word-list entries as written, in order, without duplicates.
    dictionary: Vec<Box<str>>,
    /// Completion candidates: dictionary entries and project words, sorted, without duplicates.
    words: Vec<Box<str>>,
    /// Control words seen in the project, without the backslash.
    commands: HashSet<Box<str>>,
}

impl WordList {
    /// One word or phrase per line (the format of Neovim's spell word lists).
    ///
    /// Blank lines, lines starting with `#` and lines longer than a phrase (more than four words
    /// or 64 characters) are skipped. A text containing a backslash or `$` is a LaTeX source,
    /// not a word list: it is read with [`Self::add_project_text`], so the project's words can be
    /// collected here too (`WordList::from_text(&[dictionaries…, sources…])`).
    pub fn from_text(texts: &[&str]) -> Self {
        let mut seen = HashSet::new();
        let mut dictionary = Vec::new();
        let mut sources = Vec::new();
        for text in texts {
            if text.contains(['\\', '$']) {
                sources.push(*text);
                continue;
            }
            for line in text.lines() {
                let entry = line.trim();
                if entry.is_empty()
                    || entry.starts_with('#')
                    || entry.chars().count() > MAX_ENTRY_CHARS
                    || entry.split_whitespace().count() > MAX_ENTRY_WORDS
                {
                    continue;
                }
                if seen.insert(entry) {
                    dictionary.push(Box::from(entry));
                }
            }
        }
        let mut list = Self {
            dictionary,
            words: Vec::new(),
            commands: HashSet::new(),
        };
        list.rebuild_words(BTreeSet::new());
        for source in sources {
            list.add_project_text(source);
        }
        list
    }

    /// A list built from LaTeX sources only (see [`Self::add_project_text`]).
    pub fn from_project(texts: &[&str]) -> Self {
        let mut list = Self::default();
        for text in texts {
            list.add_project_text(text);
        }
        list
    }

    /// Adds what one LaTeX source uses: the words of at least four letters in its text (not in
    /// math, commands, arguments such as labels, comments or verbatim) and its control words.
    ///
    /// To refresh the project part, start again from a clone of the dictionary list.
    pub fn add_project_text(&mut self, text: &str) {
        let lines: Vec<&str> = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        let src = SliceLines(&lines);
        let mut states = LineStates::new(Arc::new(ScanConfig::default()));
        states.rebuild(&src);
        let mut found = BTreeSet::new();
        let mut tokens: Vec<Token> = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            tokens.clear();
            states.tokens(n as u32, line, &mut tokens);
            for t in &tokens {
                let Some(span) = line.get(t.start as usize..t.end as usize) else {
                    continue;
                };
                match t.kind {
                    TokenKind::Text => collect_words(span, &mut found),
                    TokenKind::Command | TokenKind::MathCommand | TokenKind::Sectioning => {
                        let name = span.strip_prefix('\\').unwrap_or("");
                        let letters = name.bytes().take_while(u8::is_ascii_alphabetic).count();
                        if letters > 0 && !self.commands.contains(&name[..letters]) {
                            self.commands.insert(Box::from(&name[..letters]));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.rebuild_words(found);
    }

    /// Merges `extra` into the sorted candidate list.
    fn rebuild_words(&mut self, extra: BTreeSet<&str>) {
        let mut all: BTreeSet<&str> = self.words.iter().map(AsRef::as_ref).collect();
        all.extend(self.dictionary.iter().map(AsRef::as_ref));
        all.extend(extra);
        self.words = all.into_iter().map(Box::from).collect();
    }

    /// The completion candidates (dictionary entries and project words), sorted.
    pub fn words(&self) -> &[Box<str>] {
        &self.words
    }

    /// Number of completion candidates.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// True when there are no completion candidates.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Whether the project's sources use the control word `name` (without the backslash).
    pub fn uses_command(&self, name: &str) -> bool {
        self.commands.contains(name)
    }

    /// Spell-check words: entries split on spaces and hyphens, single letters dropped, capitalised names also get "'s".
    ///
    /// The same rule as the user's Neovim setup (`math.utf-8.add`): `Barr-Beck` gives `Barr`,
    /// `Barr's`, `Beck` and `Beck's`; `Mac Lane` gives `Mac`, `Mac's`, `Lane` and `Lane's`.
    /// Only word-list entries count, never project words.
    pub fn spell_words(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut add = |w: String| {
            if seen.insert(w.clone()) {
                out.push(w);
            }
        };
        for entry in &self.dictionary {
            for part in entry.split(|c: char| c.is_whitespace() || c == '-') {
                if part.chars().count() < 2 {
                    continue;
                }
                add(part.to_owned());
                if part.chars().next().is_some_and(char::is_uppercase) {
                    add(format!("{part}'s"));
                }
            }
        }
        out
    }
}

/// [`Lines`] over pre-split lines.
struct SliceLines<'a>(&'a [&'a str]);

impl Lines for SliceLines<'_> {
    fn line_count(&self) -> u32 {
        self.0.len().max(1) as u32
    }

    fn line(&self, i: u32) -> Cow<'_, str> {
        Cow::Borrowed(self.0.get(i as usize).copied().unwrap_or(""))
    }
}

/// Adds the words of `text`: runs of letters, which may contain single inner hyphens or
/// apostrophes (`quasi-coherent`, `Grothendieck's`), with at least [`MIN_PROJECT_WORD`] letters.
fn collect_words<'a>(text: &'a str, out: &mut BTreeSet<&'a str>) {
    let mut start = None;
    let mut letters = 0usize;
    let mut last_letter_end = 0usize;
    let finish = |start: usize, end: usize, letters: usize, out: &mut BTreeSet<&'a str>| {
        if (MIN_PROJECT_WORD..=MAX_WORD).contains(&letters) {
            out.insert(&text[start..end]);
        }
    };
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_alphabetic() {
            if start.is_none() {
                start = Some(i);
                letters = 0;
            }
            letters += 1;
            last_letter_end = i + c.len_utf8();
            continue;
        }
        let joiner = matches!(c, '-' | '\'' | '’');
        let next_is_letter = chars.peek().is_some_and(|(_, n)| n.is_alphabetic());
        if joiner && next_is_letter && start.is_some() && last_letter_end == i {
            continue;
        }
        if let Some(s) = start.take() {
            finish(s, last_letter_end, letters, out);
        }
    }
    if let Some(s) = start {
        finish(s, last_letter_end, letters, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_entries() {
        let long = "a phrase that is far too long to be a word-list entry\n";
        let w = WordList::from_text(&[
            "abelian\nBarr-Beck\n\nMac Lane\n",
            "abelian\n# comment\nétale\n",
            long,
        ]);
        let words: Vec<&str> = w.words().iter().map(AsRef::as_ref).collect();
        assert_eq!(words, ["Barr-Beck", "Mac Lane", "abelian", "étale"]);
    }

    #[test]
    fn latex_sources_among_word_lists_give_project_words() {
        let w = WordList::from_text(&[
            "abelian\n",
            "Let $X$ be a scheme.\n\\section{Cohomology}\nLorem ipsum dolor sit amet, consectetur.\n",
        ]);
        let words: Vec<&str> = w.words().iter().map(AsRef::as_ref).collect();
        assert_eq!(
            words,
            [
                "Cohomology",
                "Lorem",
                "abelian",
                "amet",
                "consectetur",
                "dolor",
                "ipsum",
                "scheme"
            ]
        );
        assert_eq!(w.spell_words(), ["abelian"]);
        assert!(w.uses_command("section"));
    }

    #[test]
    fn spell_words_follow_the_neovim_rule() {
        let w = WordList::from_text(&["Barr-Beck\nMac Lane\ndg-category\nx\nabelian\nBarr\n"]);
        assert_eq!(
            w.spell_words(),
            [
                "Barr", "Barr's", "Beck", "Beck's", "Mac", "Mac's", "Lane", "Lane's", "dg",
                "category", "abelian"
            ]
        );
    }

    #[test]
    fn project_words_come_from_text_only() {
        let src = "\\section{Quasi-coherent sheaves}\\label{sec:quasicoherent}\n\
                   Grothendieck's theorem on étale cohomology $\\mathrm{Spec}\\, gives$ % comment words\n\
                   \\begin{verbatim}\nverbatimword\n\\end{verbatim}\nThe Hom functor.\n";
        let w = WordList::from_project(&[src]);
        let words: Vec<&str> = w.words().iter().map(AsRef::as_ref).collect();
        for expected in [
            "Quasi-coherent",
            "sheaves",
            "Grothendieck's",
            "theorem",
            "étale",
        ] {
            assert!(
                words.contains(&expected),
                "{expected} missing from {words:?}"
            );
        }
        for absent in [
            "quasicoherent",
            "Spec",
            "gives",
            "comment",
            "verbatimword",
            "The",
            "Hom",
            "section",
        ] {
            assert!(!words.contains(&absent), "{absent} in {words:?}");
        }
        assert!(w.uses_command("section"));
        assert!(w.uses_command("mathrm"));
        assert!(!w.uses_command("frac"));
        assert!(w.spell_words().is_empty());
    }

    #[test]
    fn words_split_on_punctuation() {
        let mut out = BTreeSet::new();
        collect_words("well-known -dash- it's word. co--op longword'", &mut out);
        let got: Vec<&str> = out.into_iter().collect();
        assert_eq!(got, ["dash", "longword", "well-known", "word"]);
    }

    #[test]
    fn project_text_extends_a_dictionary_clone() {
        let dict = WordList::from_text(&["abelian\n"]);
        let mut both = dict.clone();
        both.add_project_text("Sheaves and stacks.");
        assert_eq!(both.len(), 3);
        assert_eq!(both.spell_words(), ["abelian"]);
        assert_eq!(dict.len(), 1);
        assert!(WordList::default().is_empty());
    }
}
