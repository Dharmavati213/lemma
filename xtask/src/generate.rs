//! `gen-completion`: builds `crates/lemma-complete/src/generated.rs` and
//! `crates/lemma-complete/src/packages.zst` from the completion data.
//!
//! The core packages (the closure of [`CORE_PACKAGES`] over `#include:`, what
//! `CompletionDb::builtin` loads) become static tables in `generated.rs`. Every other package is
//! one zstd frame in `packages.zst` (its rows in the format of [`encode`]), embedded with
//! `include_bytes!` and indexed by `PACKED` (name → offset, length and `#include:` dependencies);
//! `CompletionDb::load_packages` decompresses only the packages a project loads.
//!
//! Inputs (relative to the workspace root):
//!
//! | Path | What |
//! |---|---|
//! | `data/raw/cwl/*.cwl` | TeXstudio word lists (GPL-3.0), one package per file |
//! | `data/completion/cwl/*.cwl` | Lemma's supplements in the same format, merged into the package of the same name |
//! | `data/raw/umath_map.tsv`, `data/raw/unimathsymbols.txt` (optional) | glyphs, and math symbols the word lists lack |
//! | `data/completion/glyphs.tsv` | hand-checked glyphs (`\cmd<TAB>glyph`), overriding the tables |
//! | `data/completion/common.txt` | the most used commands and environments, ranked |
//! | `data/completion/text-only.txt` | commands that are invalid in math |
//! | `data/completion/math-ok.txt` | commands and environments of no particular mode that belong in formulas |
//! | `data/completion/names.txt` | package, class and TikZ library names |
//!
//! The output only depends on these files: maps are ordered and nothing time- or
//! machine-dependent is written, so `--check` can compare it byte for byte.

use crate::cwl::{self, ArgClass, Classifiers, EntryKind};
use crate::umath::{self, Symbol};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Where the generated tables go, relative to the workspace root.
pub const OUTPUT: &str = "crates/lemma-complete/src/generated.rs";

/// Where the packed (compressed) tables go, relative to the workspace root.
pub const PACKED_OUTPUT: &str = "crates/lemma-complete/src/packages.zst";

/// The packages of `CompletionDb::builtin` (`lemma_complete::BUILTIN_PACKAGES`): these and the
/// packages they include stay static tables, so a project that uses only them decompresses
/// nothing.
const CORE_PACKAGES: &[&str] = &[
    "latex-document",
    "latex-dev",
    "amsmath",
    "amssymb",
    "amsthm",
];

/// Separators of the packed format (see [`encode`]); no word list contains them.
const SECTION: char = '\u{1d}';
const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';

/// What `gen-completion` writes.
#[derive(Debug, PartialEq, Eq)]
pub struct Generated {
    /// `generated.rs`.
    pub rust: String,
    /// `packages.zst`.
    pub packed: Vec<u8>,
}

/// The package that receives kernel symbols (it is part of `CompletionDb::builtin`).
const KERNEL_PACKAGE: &str = "latex-document";

/// Size of each bonus tier of `common.txt`.
const COMMON_TIER: usize = 32;

/// Word lists of math packages: their commands and environments of no particular mode belong in
/// formulas (`\boxed`, `\text`, `\eqref`, `tikzcd`, `\SI`, `\color`).
const MATH_PACKAGES: &[&str] = &[
    "amsbsy",
    "amscd",
    "amsfonts",
    "amsmath",
    "amsopn",
    "amssymb",
    "amstext",
    "bm",
    "esint",
    "latexsym",
    "mathrsfs",
    "mathtools",
    "siunitx",
    "stmaryrd",
    "tikz-cd",
    "tikzlibrarycd",
    "xcolor",
];

/// Argument names (as the word lists write them) that take math: a command of no particular mode
/// with one belongs in formulas (`\ensuremath{formula}`, `\lefteqn{formula}`). (`{arg}` is too
/// general: `\thmname{arg}`.)
const MATH_ARGUMENTS: &[&str] = &[
    "formula",
    "math",
    "symbol",
    "expression",
    "num",
    "den",
    "above",
    "below",
    "superscript",
    "subscript",
];

/// One line of a word list (or a symbol from the unicode-math table).
#[derive(Clone, Debug)]
struct Variant {
    /// The arguments after the name (see [`cwl::Entry::rest`]).
    rest: String,
    cls: Classifiers,
    /// Position in the package, across its files: the first variant wins ties.
    order: usize,
}

#[derive(Default)]
struct PackageModel {
    includes: Vec<String>,
    commands: BTreeMap<String, Vec<Variant>>,
    environments: BTreeMap<String, Vec<Variant>>,
    next_order: usize,
}

impl PackageModel {
    fn add_file(&mut self, file: cwl::CwlFile) {
        for inc in file.includes {
            if !self.includes.contains(&inc) {
                self.includes.push(inc);
            }
        }
        for entry in file.entries {
            let map = match entry.kind {
                EntryKind::Command => &mut self.commands,
                EntryKind::Environment => &mut self.environments,
            };
            let variants = map.entry(entry.name).or_default();
            if variants
                .iter()
                .any(|v| v.rest == entry.rest && v.cls == entry.cls)
            {
                continue;
            }
            variants.push(Variant {
                rest: entry.rest,
                cls: entry.cls,
                order: self.next_order,
            });
            self.next_order += 1;
        }
    }

    fn is_empty(&self) -> bool {
        self.includes.is_empty() && self.commands.is_empty() && self.environments.is_empty()
    }
}

/// Where a symbol of the unicode-math table is defined, as a word-list package name.
fn symbol_package(package: &str) -> Option<&'static str> {
    match package {
        "kernel" => Some(KERNEL_PACKAGE),
        "amssymb" => Some("amssymb"),
        "amsmath" => Some("amsmath"),
        "stmaryrd" => Some("stmaryrd"),
        "mathtools" => Some("mathtools"),
        "latexsym" => Some("latexsym"),
        _ => None,
    }
}

/// Reads every input below `root` and returns the generated Rust source and packed tables.
pub fn generate(root: &Path) -> Result<Generated, String> {
    let read = |rel: &str| -> Result<String, String> {
        fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel}: {e}"))
    };
    let read_optional = |rel: &str| fs::read_to_string(root.join(rel)).unwrap_or_default();

    // 1. Word lists, then Lemma's supplements merged into the package of the same name.
    let mut packages: BTreeMap<String, PackageModel> = BTreeMap::new();
    for dir in ["data/raw/cwl", "data/completion/cwl"] {
        for (name, path) in cwl_files(&root.join(dir))? {
            let src = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let src = String::from_utf8_lossy(&src);
            packages.entry(name).or_default().add_file(cwl::parse(&src));
        }
    }
    packages.retain(|_, p| !p.is_empty());

    // 2. Glyphs: the unicode-math tables (lowest code point wins), then the hand-checked list.
    let mut symbols = umath::parse_map(&read("data/raw/umath_map.tsv")?);
    symbols.extend(umath::parse_unimathsymbols(&read_optional(
        "data/raw/unimathsymbols.txt",
    )));
    let glyphs = glyph_table(&symbols, &read("data/completion/glyphs.tsv")?)?;

    // 3. Math symbols that no word list defines become commands of their package.
    add_symbols(&mut packages, &symbols);

    let text_only = word_set(&read("data/completion/text-only.txt")?);
    let math_ok = parse_math_ok(&read("data/completion/math-ok.txt")?)?;
    let common = parse_common(&read("data/completion/common.txt")?)?;
    let names = parse_names(&read("data/completion/names.txt")?, &packages)?;

    let core = core_closure(&packages);
    let mut out = String::new();
    emit_header(&mut out);
    let mut statics = Vec::new();
    let mut index = Vec::new();
    let mut packed = Vec::new();
    for (name, model) in &packages {
        let rows = package_rows(name, model, &glyphs, &text_only, &math_ok);
        if core.contains(name.as_str()) {
            statics.push(emit_static(&mut out, name, &rows));
            continue;
        }
        let frame = ruzstd::encoding::compress_to_vec(
            encode(name, &rows)?.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        index.push((name, packed.len(), frame.len(), &model.includes));
        packed.extend_from_slice(&frame);
    }
    out.push_str("/// The core packages' static tables, sorted by name.\n");
    out.push_str("pub(crate) static PACKAGES: &[&Package] = &[\n");
    for s in &statics {
        let _ = writeln!(out, "    &{s}::PKG,");
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// Every other package: a zstd frame of [`PACKED_BYTES`] each, sorted by name.\n",
    );
    out.push_str("pub(crate) static PACKED: &[Packed] = &[\n");
    for (name, at, len, includes) in &index {
        let includes: Vec<String> = includes.iter().map(|i| format!("{i:?}")).collect();
        let _ = writeln!(
            out,
            "    Packed {{ name: {name:?}, at: {at}, len: {len}, includes: &[{}] }},",
            includes.join(", ")
        );
    }
    out.push_str("];\n\n");
    out.push_str("/// The packed tables (`packages.zst`, written with this file).\n");
    out.push_str("pub(crate) static PACKED_BYTES: &[u8] = include_bytes!(\"packages.zst\");\n\n");

    out.push_str("/// Glyphs of math commands (`\\alpha`, `\\mathbb{Z}`), sorted by command.\n");
    out.push_str("pub(crate) static GLYPHS: &[(&str, char)] = &[\n");
    for (cmd, glyph) in &glyphs {
        let _ = writeln!(out, "    ({cmd:?}, {glyph:?}),");
    }
    out.push_str("];\n\n");

    for (ident, what, map) in [
        (
            "COMMON_COMMANDS",
            "commands (without the backslash)",
            &common.commands,
        ),
        ("COMMON_ENVIRONMENTS", "environments", &common.environments),
    ] {
        let _ = writeln!(
            out,
            "/// The most used {what} (`data/completion/common.txt`): name, ranking bonus and\n\
             /// rank (0 = most used), sorted by name."
        );
        let _ = writeln!(out, "pub(crate) static {ident}: &[(&str, i8, u16)] = &[");
        for (key, (bonus, rank)) in map {
            let _ = writeln!(out, "    ({key:?}, {bonus}, {rank}),");
        }
        out.push_str("];\n\n");
    }

    for (ident, doc, list) in [
        (
            "PACKAGE_NAMES",
            "Package names for `\\usepackage`, sorted.",
            &names.packages,
        ),
        (
            "CLASS_NAMES",
            "Class names for `\\documentclass`, sorted.",
            &names.classes,
        ),
        (
            "TIKZ_LIBRARIES",
            "TikZ library names for `\\usetikzlibrary`, sorted.",
            &names.tikz,
        ),
    ] {
        let _ = writeln!(out, "/// {doc}");
        let _ = writeln!(out, "pub(crate) static {ident}: &[&str] = &[");
        for n in list {
            let _ = writeln!(out, "    {n:?},");
        }
        out.push_str("];\n\n");
    }
    // Exactly one trailing newline.
    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(Generated { rust: out, packed })
}

/// The names of [`CORE_PACKAGES`] and of the packages they include, transitively.
fn core_closure(packages: &BTreeMap<String, PackageModel>) -> BTreeSet<&str> {
    let mut core = BTreeSet::new();
    let mut todo: Vec<&str> = CORE_PACKAGES.to_vec();
    while let Some(name) = todo.pop() {
        let Some((key, model)) = packages.get_key_value(name) else {
            continue;
        };
        if core.insert(key.as_str()) {
            todo.extend(model.includes.iter().map(String::as_str));
        }
    }
    core
}

/// One package's rows as text, the format `lemma_complete::tables` decodes: four sections
/// separated by U+001D (the `#include:` names, the commands, the environments, the classified
/// arguments); records end with U+001E and their fields are separated by U+001F.
///
/// - command: name, snippet, mode (`A`, `M`, `T`), glyph (empty for none), signature, flags;
/// - environment: name, body, `1` when it starts math (else `0`), mode, flags;
/// - argument: command, kind (`R`ef, `C`ite, `L`abel, `F`ile, `P`ackage).
///
/// The flags are `u` (unusual) and `t` (text-leaning), in that order.
fn encode(package: &str, rows: &Rows) -> Result<String, String> {
    let mut s = String::new();
    let field = |s: &mut String, f: &str, last: bool| -> Result<(), String> {
        if f.contains([SECTION, RECORD, FIELD]) {
            return Err(format!(
                "{package}: {f:?} contains a separator of the packed format"
            ));
        }
        s.push_str(f);
        s.push(if last { RECORD } else { FIELD });
        Ok(())
    };
    let flags = |unusual: bool, leaning: bool| -> &'static str {
        match (unusual, leaning) {
            (false, false) => "",
            (true, false) => "u",
            (false, true) => "t",
            (true, true) => "ut",
        }
    };
    for inc in &rows.includes {
        field(&mut s, inc, true)?;
    }
    s.push(SECTION);
    for c in &rows.commands {
        field(&mut s, &c.name, false)?;
        field(&mut s, &c.snippet, false)?;
        field(&mut s, c.mode, false)?;
        field(
            &mut s,
            &c.glyph.map(String::from).unwrap_or_default(),
            false,
        )?;
        field(&mut s, &c.sig, false)?;
        field(&mut s, flags(c.unusual, c.leaning), true)?;
    }
    s.push(SECTION);
    for e in &rows.environments {
        field(&mut s, &e.name, false)?;
        field(&mut s, &e.body, false)?;
        field(&mut s, if e.math { "1" } else { "0" }, false)?;
        field(&mut s, e.mode, false)?;
        field(&mut s, flags(e.unusual, e.leaning), true)?;
    }
    s.push(SECTION);
    for (cmd, class) in &rows.args {
        field(&mut s, cmd, false)?;
        field(&mut s, &class.variant()[..1], true)?;
    }
    Ok(s)
}

/// The `.cwl` files of `dir` as `(package name, path)`, sorted by name. A missing directory has none.
fn cwl_files(dir: &Path) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    let Ok(read) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in read {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_some_and(|e| e == "cwl")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            out.push((stem.to_owned(), path));
        }
    }
    out.sort();
    Ok(out)
}

/// Command → glyph. Combining accents are left out; `\mathcal{X}` borrows `\mathscr{X}`'s glyph.
fn glyph_table(symbols: &[Symbol], supplement: &str) -> Result<BTreeMap<String, char>, String> {
    let mut best: BTreeMap<String, (u32, char)> = BTreeMap::new();
    for s in symbols {
        if umath::is_combining(s.glyph) {
            continue;
        }
        let slot = best
            .entry(s.command.clone())
            .or_insert((s.codepoint, s.glyph));
        if s.codepoint < slot.0 {
            *slot = (s.codepoint, s.glyph);
        }
    }
    let mut glyphs: BTreeMap<String, char> = best.into_iter().map(|(k, (_, g))| (k, g)).collect();
    for (n, line) in supplement.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (cmd, glyph) = line.split_once('\t').ok_or_else(|| {
            format!(
                "data/completion/glyphs.tsv:{}: expected `\\cmd<TAB>glyph`",
                n + 1
            )
        })?;
        let mut chars = glyph.chars();
        let (Some(g), None) = (chars.next(), chars.next()) else {
            return Err(format!(
                "data/completion/glyphs.tsv:{}: the glyph must be one character",
                n + 1
            ));
        };
        if !cmd.starts_with('\\') {
            return Err(format!(
                "data/completion/glyphs.tsv:{}: the command must start with `\\`",
                n + 1
            ));
        }
        glyphs.insert(cmd.to_owned(), g);
    }
    let script: Vec<(String, char)> = glyphs
        .iter()
        .filter_map(|(cmd, g)| {
            let letter = cmd.strip_prefix("\\mathscr{")?.strip_suffix('}')?;
            (letter.len() == 1 && letter.bytes().all(|b| b.is_ascii_uppercase()))
                .then(|| (format!("\\mathcal{{{letter}}}"), *g))
        })
        .collect();
    for (cmd, g) in script {
        glyphs.entry(cmd).or_insert(g);
    }
    Ok(glyphs)
}

/// Adds the bare math symbols (`\alpha`) that no word list defines to their package.
fn add_symbols(packages: &mut BTreeMap<String, PackageModel>, symbols: &[Symbol]) {
    let defined: BTreeSet<String> = packages
        .values()
        .flat_map(|p| p.commands.keys().cloned())
        .collect();
    let mut added: BTreeMap<(&'static str, String), String> = BTreeMap::new();
    for s in symbols {
        let Some(name) = s.command.strip_prefix('\\') else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphabetic()) {
            continue;
        }
        let Some(package) = symbol_package(&s.package) else {
            continue;
        };
        if defined.contains(name) {
            continue;
        }
        let rest = if umath::takes_argument(&s.class) {
            "{arg}"
        } else {
            ""
        };
        added
            .entry((package, name.to_owned()))
            .or_insert_with(|| rest.to_owned());
    }
    for ((package, name), rest) in added {
        let model = packages.entry(package.to_owned()).or_default();
        let order = model.next_order;
        model.next_order += 1;
        model.commands.entry(name).or_default().push(Variant {
            rest,
            cls: Classifiers {
                math: true,
                ..Classifiers::default()
            },
            order,
        });
    }
}

/// `math-ok.txt`: `\command` and `{environment}` names (see [`text_leaning`]).
#[derive(Debug, Default)]
struct MathOk {
    commands: BTreeSet<String>,
    environments: BTreeSet<String>,
}

fn parse_math_ok(src: &str) -> Result<MathOk, String> {
    let mut out = MathOk::default();
    for (n, line) in src.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("");
        for word in line.split_whitespace() {
            if let Some(cmd) = word.strip_prefix('\\').filter(|c| !c.is_empty()) {
                out.commands.insert(cmd.to_owned());
            } else if let Some(env) = word
                .strip_prefix('{')
                .and_then(|w| w.strip_suffix('}'))
                .filter(|e| !e.is_empty())
            {
                out.environments.insert(env.to_owned());
            } else {
                return Err(format!(
                    "data/completion/math-ok.txt:{}: expected `\\command` or `{{environment}}`, \
                     found `{word}`",
                    n + 1
                ));
            }
        }
    }
    Ok(out)
}

/// Whether a command (or environment) that the word lists offer in text and math alike belongs
/// in text rather than in formulas: none of the signs that it belongs in formulas holds (a glyph,
/// a math package, a math argument name in `display`, an entry in `math-ok.txt`). The completer
/// ranks such entries far down in math (`\frame`, `\footref`, `\figurename`, `itemize`).
fn text_leaning(package: &str, listed: bool, glyph: bool, display: &str) -> bool {
    let math_argument = MATH_ARGUMENTS
        .iter()
        .any(|name| display.contains(&format!("{{{name}}}")));
    !(glyph || listed || math_argument || MATH_PACKAGES.contains(&package))
}

/// Whitespace-separated names, `#` comments allowed.
fn word_set(src: &str) -> BTreeSet<String> {
    src.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(str::split_whitespace)
        .map(|w| w.trim_start_matches('\\').to_owned())
        .collect()
}

/// The bonuses of `common.txt`.
#[derive(Debug, Default)]
struct Common {
    /// Name without the backslash → (bonus, rank).
    commands: BTreeMap<String, (i8, u16)>,
    environments: BTreeMap<String, (i8, u16)>,
}

/// `common.txt`: `\name` or `{env}` per line, most used first.
fn parse_common(src: &str) -> Result<Common, String> {
    let mut out = Common::default();
    let mut rank = 0usize;
    for (n, line) in src.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let command = line.strip_prefix('\\').filter(|c| !c.is_empty());
        let environment = line
            .strip_prefix('{')
            .and_then(|l| l.strip_suffix('}'))
            .filter(|e| !e.is_empty());
        let (map, key) = match (command, environment) {
            (Some(c), _) => (&mut out.commands, c),
            (None, Some(e)) => (&mut out.environments, e),
            (None, None) => {
                return Err(format!(
                    "data/completion/common.txt:{}: expected `\\command` or `{{environment}}`",
                    n + 1
                ));
            }
        };
        let bonus = 4 - (rank / COMMON_TIER).min(3) as i8;
        let rank16 = u16::try_from(rank).map_err(|_| "common.txt: too many entries".to_owned())?;
        if map.insert(key.to_owned(), (bonus, rank16)).is_some() {
            return Err(format!(
                "data/completion/common.txt:{}: duplicate `{line}`",
                n + 1
            ));
        }
        rank += 1;
    }
    Ok(out)
}

struct Names {
    packages: BTreeSet<String>,
    classes: BTreeSet<String>,
    tikz: BTreeSet<String>,
}

/// `names.txt` sections, plus every word list that is a package or a class.
fn parse_names(src: &str, packages: &BTreeMap<String, PackageModel>) -> Result<Names, String> {
    let mut names = Names {
        packages: BTreeSet::new(),
        classes: BTreeSet::new(),
        tikz: BTreeSet::new(),
    };
    let mut section = None;
    for (n, line) in src.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = match line {
                "[packages]" => Some(0),
                "[classes]" => Some(1),
                "[tikzlibraries]" => Some(2),
                _ => {
                    return Err(format!(
                        "data/completion/names.txt:{}: unknown section {line}",
                        n + 1
                    ));
                }
            };
            continue;
        }
        let set = match section {
            Some(0) => &mut names.packages,
            Some(1) => &mut names.classes,
            Some(2) => &mut names.tikz,
            _ => {
                return Err(format!(
                    "data/completion/names.txt:{}: a name before any section",
                    n + 1
                ));
            }
        };
        set.extend(line.split_whitespace().map(str::to_owned));
    }
    for name in packages.keys() {
        if let Some(class) = name.strip_prefix("class-") {
            names.classes.insert(class.to_owned());
        } else if let Some(lib) = name.strip_prefix("tikzlibrary") {
            names.tikz.insert(lib.to_owned());
        } else if !name.starts_with("latex-") {
            names.packages.insert(name.clone());
        }
    }
    Ok(names)
}

fn emit_header(out: &mut String) {
    out.push_str(
        "// @generated by `cargo run -p xtask -- gen-completion`. Do not edit by hand: change the\n\
         // inputs and regenerate; `cargo run -p xtask -- gen-completion --check` runs in CI.\n\
         //\n\
         // Inputs: data/raw/cwl/*.cwl (TeXstudio completion word lists, GPL-3.0),\n\
         // data/raw/umath_map.tsv (unicode-math symbols mapped to pdfLaTeX commands) and\n\
         // data/completion/* (Lemma's supplements). Lemma is GPL-3.0-or-later.\n\
         \n\
         use crate::Mode::{Any as A, MathOnly as M, TextOnly as T};\n\
         use crate::tables::{Package, Packed, c, e};\n\
         use lemma_syntax::ArgKind as K;\n\n",
    );
}

/// The Rust identifier of a package's module.
fn module_ident(name: &str) -> String {
    let mut s = String::from("p_");
    for ch in name.chars() {
        s.push(if ch.is_ascii_alphanumeric() { ch } else { '_' });
    }
    s
}

fn mode_ident(math: bool, text: bool) -> &'static str {
    if math {
        "M"
    } else if text {
        "T"
    } else {
        "A"
    }
}

/// A command of a package, as the tables hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CmdRow {
    name: String,
    snippet: String,
    /// `A`, `M` or `T` (`Mode::Any`, `MathOnly`, `TextOnly`).
    mode: &'static str,
    glyph: Option<char>,
    sig: String,
    unusual: bool,
    leaning: bool,
}

/// An environment of a package, as the tables hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvRow {
    name: String,
    body: String,
    math: bool,
    mode: &'static str,
    unusual: bool,
    leaning: bool,
}

/// One package's tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Rows {
    includes: Vec<String>,
    commands: Vec<CmdRow>,
    environments: Vec<EnvRow>,
    args: BTreeMap<String, ArgClass>,
}

/// The tables of one package: the canonical variant, mode and flags of each name.
fn package_rows(
    name: &str,
    model: &PackageModel,
    glyphs: &BTreeMap<String, char>,
    text_only: &BTreeSet<String>,
    math_ok: &MathOk,
) -> Rows {
    let mut rows = Rows {
        includes: model.includes.clone(),
        ..Rows::default()
    };
    for (cmd, variants) in &model.commands {
        let shown: Vec<&Variant> = variants.iter().filter(|v| !v.cls.hidden).collect();
        // The plainest variant: usual, without picture or overlay arguments, with the fewest
        // optional arguments, first in the file.
        let Some(canon) = shown.iter().copied().min_by_key(|v| {
            let sig = cwl::signature(&v.rest);
            (v.cls.unusual, sig.special, sig.optional, v.order)
        }) else {
            continue;
        };
        let sig = cwl::signature(&canon.rest);
        let math = shown.iter().any(|v| v.cls.math);
        let text = !math
            && (shown.iter().any(|v| v.cls.text || v.cls.sectioning)
                || text_only.contains(cmd.as_str())
                || (cmd.starts_with("text") && sig.mandatory == 0));
        let unusual = shown.iter().all(|v| v.cls.unusual);
        if let Some(class) = shown
            .iter()
            .find_map(|v| v.cls.arg.or_else(|| cwl::signature(&v.rest).arg))
        {
            rows.args.insert(cmd.clone(), class);
        }
        let glyph = glyphs
            .get(&format!("\\{}", cmd.trim_end_matches('*')))
            .copied();
        let leaning = !math
            && !text
            && text_leaning(
                name,
                math_ok.commands.contains(cmd.as_str()),
                glyph.is_some(),
                &sig.display,
            );
        rows.commands.push(CmdRow {
            name: cmd.clone(),
            snippet: format!("\\{cmd}{}", sig.snippet),
            mode: mode_ident(math, text),
            glyph,
            sig: sig.display,
            unusual,
            leaning,
        });
    }
    for (env, variants) in &model.environments {
        let shown: Vec<&Variant> = variants.iter().filter(|v| !v.cls.hidden).collect();
        // As for commands, but a variant with a body (`\begin{itemize}\item`) is preferred.
        let Some(canon) = shown.iter().copied().min_by_key(|v| {
            let sig = cwl::signature(&v.rest);
            (
                v.cls.unusual,
                sig.special,
                sig.optional,
                !sig.has_literal,
                v.order,
            )
        }) else {
            continue;
        };
        let env_math = shown.iter().any(|v| v.cls.env_math);
        let math_only = shown.iter().any(|v| v.cls.math);
        let unusual = shown.iter().all(|v| v.cls.unusual);
        let leaning = !math_only
            && !env_math
            && text_leaning(name, math_ok.environments.contains(env.as_str()), false, "");
        rows.environments.push(EnvRow {
            name: env.clone(),
            body: env_body(&canon.rest),
            math: env_math,
            mode: mode_ident(math_only, env_math && !math_only),
            unusual,
            leaning,
        });
    }
    rows
}

/// Emits one package module of static tables; returns its identifier.
fn emit_static(out: &mut String, name: &str, rows: &Rows) -> String {
    let ident = module_ident(name);
    let _ = writeln!(out, "mod {ident} {{");
    out.push_str("    use super::*;\n");
    let _ = writeln!(out, "    const P: &str = {name:?};");
    out.push_str("    pub(crate) static PKG: Package = Package {\n        name: P,\n");
    let includes: Vec<String> = rows.includes.iter().map(|i| format!("{i:?}")).collect();
    let _ = writeln!(out, "        includes: &[{}],", includes.join(", "));
    out.push_str("        commands: &[\n");
    for c in &rows.commands {
        let glyph = c
            .glyph
            .map_or("None".to_owned(), |g| format!("Some({g:?})"));
        let _ = writeln!(
            out,
            "            c(P, {:?}, {:?}, {}, {glyph}, {:?}, {}, {}),",
            c.name, c.snippet, c.mode, c.sig, c.unusual, c.leaning,
        );
    }
    out.push_str("        ],\n        environments: &[\n");
    for e in &rows.environments {
        let _ = writeln!(
            out,
            "            e(P, {:?}, {:?}, {}, {}, {}, {}),",
            e.name, e.body, e.math, e.mode, e.unusual, e.leaning,
        );
    }
    out.push_str("        ],\n        args: &[");
    if rows.args.is_empty() {
        out.push_str("],\n");
    } else {
        out.push('\n');
        for (cmd, class) in &rows.args {
            let _ = writeln!(out, "            ({cmd:?}, K::{}),", class.variant());
        }
        out.push_str("        ],\n");
    }
    out.push_str("    };\n}\n\n");
    ident
}

/// What goes between `\begin{name}` and `\end{name}`: the arguments as tab stops on the
/// `\begin` line, then an indented content line (with the list's `\item`, if the word list gives
/// one) holding `$0`.
fn env_body(rest: &str) -> String {
    let split = top_level_backslash(rest).unwrap_or(rest.len());
    let (args, tail) = rest.split_at(split);
    let mut args = cwl::signature(args).snippet;
    // The content line holds the exit stop; a `%|` among the arguments would be a second one.
    args = args.replace("$0", "");
    let tail = cwl::signature(tail.trim()).snippet.replace("$0", "");
    let tail = tail.trim();
    let mut body = args;
    body.push_str("\n  ");
    if !tail.is_empty() {
        body.push_str(tail);
        body.push(' ');
    }
    body.push_str("$0\n");
    body
}

/// The byte of the first `\` outside any bracket pair.
fn top_level_backslash(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => depth -= 1,
            b'\\' if depth <= 0 => return Some(i),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_bodies() {
        assert_eq!(env_body(""), "\n  $0\n");
        assert_eq!(env_body("{ncols}"), "{$1}\n  $0\n");
        assert_eq!(env_body("\\item"), "\n  \\item $0\n");
        assert_eq!(env_body("[options%keyvals]{cols}"), "[$1]{$2}\n  $0\n");
    }

    #[test]
    fn common_tiers() {
        let src: String = (0..70).map(|i| format!("\\c{i}\n")).collect();
        let common = parse_common(&src).expect("valid");
        assert_eq!(common.commands["c0"], (4, 0));
        assert_eq!(common.commands["c31"], (4, 31));
        assert_eq!(common.commands["c32"], (3, 32));
        assert_eq!(common.commands["c69"], (2, 69));
        assert!(parse_common("frac\n").is_err());
        assert!(parse_common("{}\n").is_err());
        assert!(parse_common("\\a\n\\a\n").is_err());
        let common = parse_common("\\a\n{align*} # env\n").expect("valid");
        assert_eq!(common.commands["a"], (4, 0));
        assert_eq!(common.environments["align*"], (4, 1));
    }

    #[test]
    fn glyph_table_prefers_low_codepoints_and_adds_mathcal() {
        let sym = |cp: u32, cmd: &str| Symbol {
            codepoint: cp,
            glyph: char::from_u32(cp).expect("valid"),
            class: String::new(),
            command: cmd.to_owned(),
            package: "kernel".to_owned(),
        };
        let symbols = [
            sym(0x1D6FC, "\\alpha"),
            sym(0x3B1, "\\alpha"),
            sym(0x302, "\\hat"),
            sym(0x1D49C, "\\mathscr{A}"),
        ];
        let g = glyph_table(&symbols, "# hand-checked\n\\to\t→\n").expect("valid");
        assert_eq!(g["\\alpha"], 'α');
        assert!(!g.contains_key("\\hat"));
        assert_eq!(g["\\mathcal{A}"], '𝒜');
        assert_eq!(g["\\to"], '→');
        assert!(glyph_table(&[], "\\to\t→→\n").is_err());
        assert!(glyph_table(&[], "to\t→\n").is_err());
    }

    #[test]
    fn text_leaning_commands() {
        // \frame{text} of the kernel: no sign that it belongs in formulas.
        assert!(text_leaning("latex-document", false, false, "{text}"));
        // Listed in math-ok.txt, with a glyph, from a math package, a formula argument.
        assert!(!text_leaning("latex-document", true, false, "{length}"));
        assert!(!text_leaning("latex-document", false, true, ""));
        assert!(!text_leaning("amstext", false, false, "{text}"));
        assert!(!text_leaning("latex-document", false, false, "{formula}"));
        assert!(!text_leaning("latex-document", false, false, "[pos]{math}"));
        assert!(text_leaning("amsthm", false, false, "{arg}"));
        assert!(text_leaning(
            "latex-document",
            false,
            false,
            "{argument list}"
        ));
        let ok = parse_math_ok("# c\n\\quad \\tag*\n{array} # env\n").expect("valid");
        assert!(ok.commands.contains("quad") && ok.commands.contains("tag*"));
        assert!(ok.environments.contains("array"));
        assert!(parse_math_ok("quad\n").is_err());
    }

    #[test]
    fn module_identifiers() {
        assert_eq!(module_ident("tikz-cd"), "p_tikz_cd");
        assert_eq!(module_ident("class-amsart"), "p_class_amsart");
    }
}
