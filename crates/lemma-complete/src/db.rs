//! [`CompletionDb`]: the loaded package tables, merged.

use crate::tables::{self, GenCmd, GenEnv, Package};
use crate::{CmdEntry, EnvEntry};
use lemma_snippets::{Chunk, parse_body};
use lemma_syntax::ArgKind;
use std::collections::HashMap;

/// The word lists [`CompletionDb::builtin`] loads (with the packages they include): the LaTeX
/// kernel, amsmath (with amstext, amsopn, amsbsy), amssymb (with amsfonts) and amsthm.
pub const BUILTIN_PACKAGES: &[&str] = &[
    "latex-document",
    "latex-dev",
    "amsmath",
    "amssymb",
    "amsthm",
];

/// `#include` chains deeper than this are not followed.
const MAX_INCLUDE_DEPTH: usize = 16;

/// A command of a loaded package.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DbCmd {
    pub(crate) cmd: &'static GenCmd,
    /// Loaded by [`CompletionDb::builtin`] (L1, ranked +4).
    pub(crate) builtin: bool,
}

/// An environment of a loaded package.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DbEnv {
    pub(crate) env: &'static GenEnv,
    pub(crate) builtin: bool,
}

/// L1 tables plus the tables of the packages loaded with [`Self::load_packages`].
///
/// A name defined by several loaded packages keeps the definition of the package loaded first,
/// and a package's own definitions win over those of the packages it includes.
#[derive(Clone, Debug)]
pub struct CompletionDb {
    loaded: Vec<&'static Package>,
    pub(crate) commands: Vec<DbCmd>,
    /// `commands[i].cmd.entry.name`, for the matcher.
    pub(crate) command_names: Vec<&'static str>,
    pub(crate) environments: Vec<DbEnv>,
    pub(crate) environment_names: Vec<&'static str>,
    command_index: HashMap<&'static str, usize>,
    environment_index: HashMap<&'static str, usize>,
    args: HashMap<&'static str, ArgKind>,
}

impl Default for CompletionDb {
    fn default() -> Self {
        Self::builtin()
    }
}

impl CompletionDb {
    /// The built-in L1 tables: [`BUILTIN_PACKAGES`] and the packages they include.
    pub fn builtin() -> Self {
        let mut db = Self::empty();
        for name in BUILTIN_PACKAGES {
            db.load(name, true, 0);
        }
        db
    }

    /// A database without any tables.
    pub fn empty() -> Self {
        Self {
            loaded: Vec::new(),
            commands: Vec::new(),
            command_names: Vec::new(),
            environments: Vec::new(),
            environment_names: Vec::new(),
            command_index: HashMap::new(),
            environment_index: HashMap::new(),
            args: HashMap::new(),
        }
    }

    /// Adds the generated tables of these packages and of the packages they include.
    ///
    /// Names are package names as in `\usepackage` (`ProjectIndex::packages`); a document class
    /// is named `class-<name>` (`class-amsart`). Unknown names and packages already loaded are
    /// ignored, so calling this again with the project's current list is cheap. A package that
    /// is not part of the core is decompressed the first time any database loads it.
    pub fn load_packages(&mut self, names: &[String]) {
        for name in names {
            self.load(name.trim(), false, 0);
        }
    }

    fn load(&mut self, name: &str, builtin: bool, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH || self.is_loaded(name) {
            return;
        }
        let Some(pkg) = tables::package(name) else {
            return;
        };
        self.loaded.push(pkg);
        for cmd in pkg.commands {
            let name = cmd.entry.name;
            if !self.command_index.contains_key(name) {
                self.command_index.insert(name, self.commands.len());
                self.commands.push(DbCmd { cmd, builtin });
                self.command_names.push(name);
            }
        }
        for env in pkg.environments {
            let name = env.entry.name;
            if !self.environment_index.contains_key(name) {
                self.environment_index.insert(name, self.environments.len());
                self.environments.push(DbEnv { env, builtin });
                self.environment_names.push(name);
            }
        }
        for &(cmd, kind) in pkg.args {
            self.args.entry(cmd).or_insert(kind);
        }
        for inc in pkg.includes {
            self.load(inc, builtin, depth + 1);
        }
    }

    /// Whether the tables of `package` are loaded.
    pub fn is_loaded(&self, package: &str) -> bool {
        self.loaded.iter().any(|p| p.name == package)
    }

    /// The loaded packages, in load order.
    pub fn loaded_packages(&self) -> Vec<&'static str> {
        self.loaded.iter().map(|p| p.name).collect()
    }

    /// Every package that has tables (what [`Self::load_packages`] can load), sorted.
    pub fn available_packages() -> Vec<&'static str> {
        tables::packages()
    }

    /// Whether `package`'s tables are static (the core packages that [`Self::builtin`] loads,
    /// with their includes) rather than compressed.
    pub fn is_static_package(package: &str) -> bool {
        tables::is_static(package)
    }

    /// The packages whose compressed tables were decompressed so far in this process (by
    /// [`Self::load_packages`] in any database), in order. Each is decompressed once.
    pub fn decompressed_packages() -> Vec<&'static str> {
        tables::decompressed()
    }

    /// The size in bytes of the compressed tables embedded in the binary.
    pub fn packed_size() -> usize {
        tables::packed_size()
    }

    /// The number of distinct commands and environments loaded.
    pub fn counts(&self) -> (usize, usize) {
        (self.commands.len(), self.environments.len())
    }

    /// True when no tables are loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// Every loaded command, in load order.
    pub fn commands(&self) -> impl Iterator<Item = &'static CmdEntry> + '_ {
        self.commands.iter().map(|c| &c.cmd.entry)
    }

    /// Every loaded environment, in load order.
    pub fn environments(&self) -> impl Iterator<Item = &'static EnvEntry> + '_ {
        self.environments.iter().map(|e| &e.env.entry)
    }

    /// The loaded command `name` (without the backslash).
    pub fn command(&self, name: &str) -> Option<&'static CmdEntry> {
        self.command_index
            .get(name)
            .map(|&i| &self.commands[i].cmd.entry)
    }

    /// The loaded environment `name`.
    pub fn environment(&self, name: &str) -> Option<&'static EnvEntry> {
        self.environment_index
            .get(name)
            .map(|&i| &self.environments[i].env.entry)
    }

    /// What the argument of command `cmd` (without the backslash) holds, according to the word
    /// lists' classifiers: references, citations, labels, files or packages. A starred name
    /// falls back to the unstarred one. The completion provider uses it to open the popup after
    /// `{` for commands the scanner does not classify (`\autoref`, `\textcite`, …).
    pub fn arg_kind(&self, cmd: &str) -> Option<ArgKind> {
        self.args
            .get(cmd)
            .or_else(|| self.args.get(cmd.strip_suffix('*')?))
            .copied()
    }

    /// The whole environment as a snippet: `\begin{name}`, the environment's
    /// [`EnvEntry::body`] (a plain indented `$0` line for environments without a table) and
    /// `\end{name}`. Not indented yet (`lemma_snippets::with_indent`).
    pub fn env_snippet(&self, name: &str) -> Vec<Chunk> {
        let body = self.environment(name).map_or("\n  $0\n", |e| e.body);
        let escaped = name.replace('$', "${$}");
        let src = format!("\\begin{{{escaped}}}{body}\\end{{{escaped}}}");
        parse_body(&src).unwrap_or_else(|_| {
            vec![
                Chunk::Text(format!("\\begin{{{name}}}\n  ")),
                Chunk::Stop {
                    n: 0,
                    default: String::new(),
                },
                Chunk::Text(format!("\n\\end{{{name}}}")),
            ]
        })
    }
}

/// The class of the document: `name` in `\documentclass[…]{name}` (the first one outside
/// comments, before `\begin{document}`). Load its table with
/// [`CompletionDb::load_packages`] as `class-<name>`.
pub fn document_class(text: &str) -> Option<&str> {
    const CMD: &str = "\\documentclass";
    let mut from = 0;
    while let Some(found) = text[from..].find(CMD) {
        let at = from + found;
        from = at + CMD.len();
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        if in_comment(&text[line_start..at]) {
            continue;
        }
        if text[..at].contains("\\begin{document}") {
            return None;
        }
        let mut rest = skip_blanks(&text[from..]);
        if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue; // `\documentclassx`
        }
        if let Some(options) = rest.strip_prefix('[') {
            rest = skip_blanks(&options[options.find(']')? + 1..]);
        }
        let arg = rest.strip_prefix('{')?;
        let name = arg[..arg.find('}')?].trim();
        return (!name.is_empty()).then_some(name);
    }
    None
}

/// Whether text that ends a line prefix contains an unescaped `%` (the rest is a comment).
fn in_comment(line_prefix: &str) -> bool {
    let b = line_prefix.as_bytes();
    b.iter().enumerate().any(|(i, &c)| {
        c == b'%' && b[..i].iter().rev().take_while(|&&p| p == b'\\').count() % 2 == 0
    })
}

/// `s` without leading whitespace and `%` comments.
fn skip_blanks(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        match s.strip_prefix('%') {
            Some(comment) => s = comment.find('\n').map_or("", |i| &comment[i + 1..]),
            None => return s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_classes() {
        assert_eq!(
            document_class("\\documentclass[11pt]{amsart}\n"),
            Some("amsart")
        );
        assert_eq!(
            document_class(
                "% \\documentclass{book}\n\\documentclass[\n  a4paper, % paper\n]{ article }"
            ),
            Some("article")
        );
        assert_eq!(document_class("\\documentclass{}"), None);
        assert_eq!(document_class("\\begin{document}\\documentclass{x}"), None);
        assert_eq!(
            document_class("50\\% \\documentclass{memoir}"),
            Some("memoir")
        );
        assert_eq!(document_class("no class"), None);
        let mut db = CompletionDb::builtin();
        db.load_packages(&[format!(
            "class-{}",
            document_class("\\documentclass{amsart}").unwrap_or_default()
        )]);
        assert!(db.command("subjclass").is_some());
    }

    #[test]
    fn builtin_loads_the_kernel_and_ams_packages_with_their_includes() {
        let db = CompletionDb::builtin();
        let loaded = db.loaded_packages();
        for p in [
            "latex-document",
            "latex-dev",
            "amsmath",
            "amstext",
            "amsopn",
            "amsbsy",
            "amssymb",
            "amsfonts",
            "amsthm",
        ] {
            assert!(loaded.contains(&p), "{p} not loaded: {loaded:?}");
        }
        assert!(!db.is_loaded("hyperref"));
        assert!(db.command("frac").is_some());
        assert!(db.command("text").is_some());
        assert!(db.command("mathbb").is_some());
        assert!(db.command("operatorname").is_some());
        assert!(db.command("alpha").is_some_and(|c| c.glyph == Some('α')));
        assert!(db.environment("align*").is_some_and(|e| e.math));
        assert!(db.environment("proof").is_some());
        assert!(db.command("cref").is_none());
    }

    #[test]
    fn load_packages_follows_includes_and_ignores_unknown_names() {
        let mut db = CompletionDb::builtin();
        let (cmds, _) = db.counts();
        db.load_packages(&["tikz-cd".into(), "cleveref".into(), "nonexistent".into()]);
        assert!(db.is_loaded("tikz-cd"));
        assert!(db.is_loaded("tikzlibrarycd"));
        assert!(db.is_loaded("tikz"));
        assert!(db.environment("tikzcd").is_some());
        assert!(db.command("cref").is_some());
        assert!(db.counts().0 > cmds);
        let again = db.counts();
        db.load_packages(&["cleveref".into()]);
        assert_eq!(db.counts(), again);
    }

    #[test]
    fn earlier_packages_win() {
        let mut db = CompletionDb::builtin();
        db.load_packages(&["natbib".into()]);
        // natbib only lists `\cite[prefix][postfix]{keylist}`; the kernel's `\cite{…}` stays.
        assert_eq!(db.command("cite").map(|c| c.snippet), Some(r"\cite{$1}"));
    }

    #[test]
    fn arg_kinds() {
        let mut db = CompletionDb::builtin();
        assert_eq!(db.arg_kind("ref"), Some(ArgKind::Ref));
        assert_eq!(db.arg_kind("eqref"), Some(ArgKind::Ref));
        assert_eq!(db.arg_kind("cite"), Some(ArgKind::Cite));
        assert_eq!(db.arg_kind("input"), Some(ArgKind::File));
        assert_eq!(db.arg_kind("frac"), None);
        assert_eq!(db.arg_kind("cref"), None);
        db.load_packages(&["cleveref".into(), "graphicx".into()]);
        assert_eq!(db.arg_kind("cref"), Some(ArgKind::Ref));
        assert_eq!(db.arg_kind("cref*"), Some(ArgKind::Ref));
        assert_eq!(db.arg_kind("includegraphics"), Some(ArgKind::File));
    }

    #[test]
    fn environment_snippets() {
        let db = CompletionDb::builtin();
        let text = |chunks: &[Chunk]| -> String {
            chunks
                .iter()
                .map(|c| match c {
                    Chunk::Text(t) => t.clone(),
                    Chunk::Stop { n, .. } => format!("${n}"),
                    Chunk::Mirror(n) => format!("${n}"),
                })
                .collect()
        };
        assert_eq!(
            text(&db.env_snippet("itemize")),
            "\\begin{itemize}\n  \\item $0\n\\end{itemize}"
        );
        assert_eq!(
            text(&db.env_snippet("alignat")),
            "\\begin{alignat}{$1}\n  $0\n\\end{alignat}"
        );
        assert_eq!(
            text(&db.env_snippet("theorem")),
            "\\begin{theorem}\n  $0\n\\end{theorem}"
        );
    }
}
