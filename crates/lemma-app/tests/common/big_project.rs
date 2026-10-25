//! A generated book for the performance tests of the project-wide features (the label checks,
//! the Outline): `main.tex` with `\input`s of `chapters` chapter files in `c/`, each with a
//! chapter heading and `labels` labels on sections, equations, theorems, lemmas and figures,
//! and prose after each that refers to labels anywhere in the book and cites entries of an
//! `entries`-entry `refs.bib`; plus a `.build/main.aux` that numbers every label and lists the
//! headings and figures, as a build would. 300 chapters of 66 labels and 10 000 entries make
//! about 142k lines, 20k labels, 30k references and 40k citations.

use super::Rng;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// What [`write`] wrote.
#[derive(Clone, Copy, Debug)]
pub struct BigProject {
    pub chapters: usize,
    pub labels: usize,
    pub refs: usize,
    pub cites: usize,
    pub entries: usize,
    pub lines: usize,
}

const SYLLABLES: [&str; 20] = [
    "ka", "lo", "mi", "ne", "ru", "sa", "ti", "vo", "ze", "pra", "sto", "qua", "len", "dor", "fis",
    "gra", "hum", "jex", "bri", "cul",
];

fn word(rng: &mut Rng) -> String {
    (0..2 + rng.below(3))
        .map(|_| SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize])
        .collect()
}

fn words(rng: &mut Rng, n: usize) -> String {
    (0..n).map(|_| word(rng)).collect::<Vec<_>>().join(" ")
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The `j`-th label of chapter `c` and the environment (or heading) it is in.
fn label(c: usize, j: usize) -> (&'static str, String) {
    let kind = ["sec", "eq", "thm", "lem", "fig"][j % 5];
    (kind, format!("{kind}:{c}:{j}"))
}

/// Writes the book into `dir` (deterministic for a `seed`).
pub fn write(dir: &Path, chapters: usize, labels: usize, entries: usize, seed: u64) -> BigProject {
    let mut rng = Rng::new(seed);
    std::fs::create_dir_all(dir.join("c")).unwrap();
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    let mut bib = String::new();
    for i in 0..entries {
        let _ = write!(
            bib,
            "@article{{key{i:05},\n  author = {{{}, {} and {}, {}}},\n  title = {{{}}},\n  \
             journal = {{{}}},\n  year = {{{}}},\n  pages = {{{i}--{}}},\n}}\n\n",
            capitalized(&word(&mut rng)),
            capitalized(&word(&mut rng)),
            capitalized(&word(&mut rng)),
            capitalized(&word(&mut rng)),
            capitalized(&words(&mut rng, 6)),
            capitalized(&words(&mut rng, 2)),
            1950 + i % 70,
            i + 10,
        );
    }
    std::fs::write(dir.join("refs.bib"), bib).unwrap();
    let mut aux = String::from("\\relax\n");
    let (mut refs, mut cites, mut lines) = (0, 0, 0);
    for c in 0..chapters {
        let mut text = String::new();
        let title = capitalized(&words(&mut rng, 2));
        let _ = writeln!(text, "\\chapter{{{title}}}\\label{{ch:{c}}}");
        let _ = writeln!(
            aux,
            "\\@writefile{{toc}}{{\\contentsline {{chapter}}{{\\numberline {{{}}}{title}}}{{{}}}{{}}}}",
            c + 1,
            c * 10 + 1
        );
        let _ = writeln!(
            aux,
            "\\newlabel{{ch:{c}}}{{{{{}}}{{{}}}}}",
            c + 1,
            c * 10 + 1
        );
        let (mut section, mut theorem, mut equation, mut figure) = (0, 0, 0, 0);
        for j in 0..labels {
            let (kind, name) = label(c, j);
            let number = match kind {
                "sec" => {
                    section += 1;
                    let title = capitalized(&words(&mut rng, 3));
                    let _ = writeln!(text, "\\section{{{title}}}\\label{{{name}}}");
                    let _ = writeln!(
                        aux,
                        "\\@writefile{{toc}}{{\\contentsline {{section}}{{\\numberline {{{}.{section}}}{title}}}{{{}}}{{}}}}",
                        c + 1,
                        c * 10 + 1
                    );
                    format!("{}.{section}", c + 1)
                }
                "eq" => {
                    equation += 1;
                    let _ = write!(
                        text,
                        "\\begin{{equation}}\n  a_{{{j}}} + b^{{2}} = \\sum_{{n}} c_n \\label{{{name}}}\n\\end{{equation}}\n"
                    );
                    format!("{}.{equation}", c + 1)
                }
                "thm" | "lem" => {
                    theorem += 1;
                    let env = if kind == "thm" { "theorem" } else { "lemma" };
                    let _ = write!(
                        text,
                        "\\begin{{{env}}}\\label{{{name}}}\n  {}.\n\\end{{{env}}}\n\\begin{{proof}}\n  {}.\n\\end{{proof}}\n",
                        capitalized(&words(&mut rng, 12)),
                        capitalized(&words(&mut rng, 15)),
                    );
                    format!("{}.{theorem}", c + 1)
                }
                _ => {
                    figure += 1;
                    let caption = capitalized(&words(&mut rng, 5));
                    let _ = write!(
                        text,
                        "\\begin{{figure}}[h]\n  \\centering\n  \\fbox{{{}}}\n  \\caption{{{caption}}}\\label{{{name}}}\n\\end{{figure}}\n",
                        words(&mut rng, 2),
                    );
                    let _ = writeln!(
                        aux,
                        "\\@writefile{{lof}}{{\\contentsline {{figure}}{{\\numberline {{{}.{figure}}}{{\\ignorespaces {caption}}}}}{{{}}}{{}}}}",
                        c + 1,
                        c * 10 + 1
                    );
                    format!("{}.{figure}", c + 1)
                }
            };
            let _ = writeln!(
                aux,
                "\\newlabel{{{name}}}{{{{{number}}}{{{}}}}}",
                c * 10 + 1
            );
            // A paragraph of prose with references anywhere in the book and citations.
            let mut sentences = Vec::new();
            for _ in 0..3 {
                let n = 10 + rng.below(9) as usize;
                let mut sentence = capitalized(&words(&mut rng, n));
                let r = rng.below(100);
                if r < 50 {
                    let target = label(
                        rng.below(chapters as u64) as usize,
                        rng.below(labels as u64) as usize,
                    );
                    let _ = write!(sentence, " by \\ref{{{}}}", target.1);
                    refs += 1;
                }
                if r > 30 {
                    let _ = write!(sentence, " \\cite{{key{:05}}}", rng.below(entries as u64));
                    cites += 1;
                }
                sentences.push(sentence + ".");
            }
            let _ = write!(
                text,
                "{} {}\n{}\n\n",
                sentences[0], sentences[1], sentences[2]
            );
        }
        lines += text.lines().count();
        std::fs::write(dir.join(format!("c/c{c:04}.tex")), text).unwrap();
    }
    let mut main = String::from(
        "\\documentclass{book}\n\\usepackage{amsmath,amssymb,amsthm}\n\\usepackage{hyperref}\n\
         \\newtheorem{theorem}{Theorem}[chapter]\n\\newtheorem{lemma}[theorem]{Lemma}\n\
         \\newcommand{\\R}{\\mathbb{R}}\n\\begin{document}\n\\tableofcontents\n",
    );
    for c in 0..chapters {
        let _ = writeln!(main, "\\input{{c/c{c:04}}}");
    }
    main.push_str("\\bibliographystyle{plain}\n\\bibliography{refs}\n\\end{document}\n");
    lines += main.lines().count();
    std::fs::write(dir.join("main.tex"), main).unwrap();
    std::fs::write(dir.join(".build/main.aux"), aux).unwrap();
    BigProject {
        chapters,
        labels: chapters * (labels + 1),
        refs,
        cites,
        entries,
        lines,
    }
}

/// The first line of prose of a chapter's `text` at or after the 0-based line `from` (a
/// paragraph's line that has no reference), wrapping around to the start.
pub fn prose_line(text: &str, from: usize) -> u32 {
    let lines: Vec<&str> = text.lines().collect();
    let prose = |i: &usize| {
        let line = lines[*i];
        line.len() > 40 && !line.starts_with(['\\', ' ']) && !line.contains("\\ref")
    };
    (from..lines.len())
        .find(prose)
        .or_else(|| (0..lines.len()).find(prose))
        .and_then(|line| u32::try_from(line).ok())
        .expect("a line of prose")
}

/// How long `f` kept the calling thread: its wall time less the time the thread waited for a
/// CPU meanwhile, which a loaded machine adds (see `common::timed_iteration`).
pub fn on_cpu(f: impl FnOnce()) -> Duration {
    let started = Instant::now();
    let waited_before = super::run_queue_wait();
    f();
    let waited = super::run_queue_wait()
        .zip(waited_before)
        .map_or(Duration::ZERO, |(after, before)| {
            after.saturating_sub(before)
        });
    started.elapsed().saturating_sub(waited)
}

/// The median and the largest of `times`, in milliseconds, as `median/max ms`.
pub fn median_max(times: &mut [Duration]) -> String {
    times.sort();
    format!(
        "{:.2}/{:.2} ms",
        times[times.len() / 2].as_secs_f64() * 1e3,
        times[times.len() - 1].as_secs_f64() * 1e3
    )
}

/// The load average (`/proc/loadavg`'s first three fields), for the printed timings.
pub fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}
