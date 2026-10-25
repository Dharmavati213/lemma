//! `perf_word_list_project`: building the completion words of a generated 300-file project.
//! Run it in release mode:
//!
//! ```sh
//! cargo test --release -p lemma-complete --test perf_words -- --ignored perf
//! ```

use lemma_complete::WordList;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Files of the generated project: the app reads at most 300 from disk.
const FILES: usize = 300;
const LINES: usize = 400;

/// A deterministic word generator (a linear congruential generator over syllables).
struct Words(u64);

impl Words {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A word of 2–4 syllables from `seed` (the same seed, the same word).
    fn word(seed: u64) -> String {
        const SYLLABLES: [&str; 24] = [
            "ka", "lo", "mi", "ne", "ru", "sa", "te", "vo", "zi", "ba", "de", "fu", "go", "hi",
            "ja", "ku", "li", "mo", "nu", "pe", "qui", "ro", "si", "tu",
        ];
        // splitmix64's finalizer spreads neighbouring seeds over all syllables.
        let mut s = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        s = (s ^ (s >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        s = (s ^ (s >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        s ^= s >> 31;
        let n = 2 + (s % 3);
        let mut out = String::new();
        for _ in 0..n {
            s /= 24;
            out.push_str(SYLLABLES[(s % 24) as usize]);
        }
        out
    }
}

/// `count` files of [`LINES`] lines: prose from a shared vocabulary of 3000 words and 150 words
/// of each file's own (names, terms), with some commands and math.
fn project(count: usize) -> Vec<String> {
    let mut rng = Words(42);
    (0..count)
        .map(|f| {
            let mut text = format!("\\section{{Part {f}}}\\label{{sec:{f}}}\n");
            for line in 0..LINES {
                for _ in 0..12 {
                    let seed = if rng.next().is_multiple_of(8) {
                        1_000_000 + (f as u64) * 150 + rng.next() % 150
                    } else {
                        rng.next() % 3000
                    };
                    text.push_str(&Words::word(seed));
                    text.push(' ');
                }
                if line.is_multiple_of(10) {
                    let _ = write!(text, "$\\mathcal{{F}}_{line}$ \\cite{{key{line}}}");
                }
                text.push_str(".\n");
            }
            text
        })
        .collect()
}

fn time(texts: &[String], build: impl Fn(&[String]) -> WordList) -> (Duration, WordList) {
    let mut best = Duration::MAX;
    let mut list = WordList::default();
    for _ in 0..3 {
        let t = Instant::now();
        list = build(texts);
        best = best.min(t.elapsed());
    }
    (best, list)
}

#[test]
#[ignore = "performance test: cargo test --release -p lemma-complete --test perf_words -- --ignored perf"]
fn perf_word_list_project() {
    let dictionary: String = (0..800).map(|i| format!("mathword{i}\n")).collect();
    let base = WordList::from_text(&[&dictionary]);
    let one_by_one = |texts: &[String]| {
        let mut words = base.clone();
        for text in texts {
            words.add_project_text(text);
        }
        words
    };
    let together = |texts: &[String]| {
        let mut words = base.clone();
        words.add_project_texts(texts);
        words
    };
    let all = project(FILES);
    let lines: usize = all.iter().map(|t| t.lines().count()).sum();
    let (sequential, expected) = time(&all, one_by_one);
    let (batch, list) = time(&all, together);
    let (half, _) = time(&all[..FILES / 2], together);
    println!(
        "perf_word_list_project: {FILES} files, {lines} lines, {} words: one text at a time \
         {sequential:.1?}, all at once {batch:.1?} ({} files: {half:.1?})",
        list.len(),
        FILES / 2
    );
    assert_eq!(list.words(), expected.words());
    assert!(list.len() > 20_000, "{} words", list.len());
    // Linear in the files: twice the files take at most 2.5 times as long.
    assert!(
        batch.as_secs_f64() <= 2.5 * half.as_secs_f64(),
        "{FILES} files {batch:?}, {} files {half:?}",
        FILES / 2
    );
}
