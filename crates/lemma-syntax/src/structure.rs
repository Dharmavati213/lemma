// Owner: WP1.9 structure
//! The per-file structure pass (worker thread, 150 ms after typing stops).
//!
//! [`index_file`] makes one pass over the raw text and does not use the incremental scanner. It
//! looks only at control sequences, and skips comments, verbatim environments, inline verbatim
//! (`\verb`, `\lstinline`, `\url`, …) and `\iffalse … \fi` blocks. Like TeX, it stops reading at
//! `\end{document}` and at the end of the line containing `\endinput`. An `\endinput` inside an
//! open conditional is ignored: it is usually an include guard
//! (`\ifdefined\x\endinput\fi`, `\ifx\x\undefined … \else\endinput\fi`) that TeX does not run on
//! the first load, so the rest of the file still counts.

pub(crate) mod args;
mod commands;

use crate::{Magic, parse_magic};
use args::{
    LineCounter, arg_count, at, brace, bracket, clean, control_sequence, defined_name, group,
    line_end, raw_group, skip_blanks, skip_optionals, skip_space, split_list, star,
    strip_outer_braces, with_text_index,
};
use commands::{
    CiteForm, Cmd, RefForm, is_default_theorem, is_primitive_conditional, is_verbatim_env,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// A sectioning command or theorem-like environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    /// 0 part … 6 subparagraph, 7 theorem-like.
    ///
    /// KOMA-Script's `\addpart`, `\addchap` and `\addsec` share the levels of `\part`, `\chapter`
    /// and `\section`; `\minisec` is level 5.
    pub level: u8,
    /// The (long) title with whitespace collapsed and `\label`, `\index`, `\footnote` and
    /// `\protect` removed; for a theorem, its optional `[title]` or `""`.
    pub title: String,
    /// 0-based line of the command or `\begin`.
    pub line: u32,
    /// Unnumbered: a starred command or environment, or KOMA's `\add…`/`\minisec`.
    pub starred: bool,
    /// The environment name for theorem-like items.
    pub env: Option<String>,
}

/// A `\label{name}`, with the environment and title it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// 0-based line of the `\label`.
    pub line: u32,
    /// The innermost enclosing environment other than `document`.
    pub env: Option<String>,
    /// What `\nameref` would print: the optional title of the enclosing theorem, the caption of the
    /// enclosing float, or else the title of the current section. `None` inside an untitled theorem.
    pub title: Option<String>,
}

/// A reference command (`\ref`, `\eqref`, `\cref{a,b}`, `\crefrange`, …) and its label names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The command without the backslash, with `*` if starred (e.g. `"cref"`).
    pub cmd: String,
    /// Label names in order; comma lists are split for the cleveref family.
    pub names: Vec<String>,
    /// 0-based line of the command.
    pub line: u32,
}

/// A citation command (`\cite`, natbib, biblatex multicites) and its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citation {
    /// The command without the backslash, with `*` if starred (e.g. `"parencites"`).
    pub cmd: String,
    /// Keys in order, over every key group of a multicite; `\nocite{*}`'s `*` is left out.
    pub keys: Vec<String>,
    /// 0-based line of the command.
    pub line: u32,
}

/// A `\begin{name}` and its matching `\end`, if any.
///
/// Pairs are in `\begin` order. On a mismatched `\end{x}` the innermost open `x` is closed and the
/// environments opened inside it stay unclosed (`end_line: None`); an `\end` with no open match is
/// ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvPair {
    pub name: String,
    /// 0-based line of the `\begin`.
    pub begin_line: u32,
    /// 0-based line of the matching `\end`.
    pub end_line: Option<u32>,
}

/// The command that pulled in another file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncludeKind {
    /// `\input`, `\InputIfFileExists`, `\includestandalone`.
    Input,
    /// `\include`.
    Include,
    /// `\subfile`, `\subfileinclude`.
    Subfile,
    /// `\import`, `\subimport`, `\inputfrom`, `\includefrom`, `\subinputfrom`, `\subincludefrom`.
    Import,
    /// `\includegraphics`, `\includesvg`, `\includepdf`.
    Graphics,
    /// One entry of `\bibliography{a,b}`, or `\addbibresource` and friends.
    Bibliography,
    /// One entry of `\usepackage{a,b}` or `\RequirePackage`.
    Package,
}

/// A file reference such as `\input{chapters/a}`; `target` is as written.
///
/// For `\import{dir}{file}` the target is `dir` and `file` joined with `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    pub kind: IncludeKind,
    pub target: String,
    /// 0-based line of the command.
    pub line: u32,
}

/// A macro, environment, theorem or math operator defined in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroDef {
    /// The command name without the backslash (`"cO"` for `\cO`), or the environment name.
    pub name: String,
    /// Number of arguments, optional ones included (`\newcommand{\f}[2][x]` has 2).
    pub nargs: u8,
    /// The replacement text with comments removed and whitespace collapsed. For `\let` it is the
    /// target (`"\varepsilon"`), for an environment its begin code, for a theorem its title.
    pub body: Option<String>,
    /// 0-based line of the defining command.
    pub line: u32,
    pub is_env: bool,
    pub is_theorem: bool,
    pub is_math_operator: bool,
    /// How many of the `nargs` arguments are optional: the `[default]` one of
    /// `\newcommand{\f}[2][x]` (1), the `s`, `o`, `O{…}`, `d…`, `t…`, `e{…}` ones of
    /// `\NewDocumentCommand` (`{s o m}`: 2). A call needs only the other ones, in braces.
    pub optional_args: u8,
    /// The default of an optional first argument: `\newcommand{\f}[2][x]` → `Some("x")`,
    /// `\NewDocumentCommand{\f}{O{x} m}` → `Some("x")`, `{o m}` → `Some("")`.
    pub optional_default: Option<String>,
}

/// Everything the structure pass extracts from one file. Lines are 0-based.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileIndex {
    /// Sections and theorem-like environments in document order.
    pub outline: Vec<OutlineItem>,
    pub labels: Vec<Label>,
    pub refs: Vec<Reference>,
    pub cites: Vec<Citation>,
    pub envs: Vec<EnvPair>,
    pub includes: Vec<Include>,
    pub macros: Vec<MacroDef>,
    /// Packages from `\usepackage`/`\RequirePackage`, without duplicates, in load order.
    pub packages: Vec<String>,
    pub magic: Magic,
    pub has_documentclass: bool,
    pub has_begin_document: bool,
}

/// Runs the structure pass over a whole file.
///
/// Theorem-like environments are those declared in the file itself plus common names such as
/// `theorem`, `lemma` or `definition`; see [`index_file_with_theorems`] for the rest.
pub fn index_file(text: &str) -> FileIndex {
    let mut indexer = Indexer::new(text);
    with_text_index(text.as_bytes(), || indexer.run());
    indexer.finish()
}

/// Like [`index_file`], but also treats `theorems` as theorem-like environments. Pass the
/// project's [`ProjectIndex::theorem_names`](crate::ProjectIndex::theorem_names) so that a
/// chapter whose `\newtheorem`s live in a separate preamble file gets a complete outline.
pub fn index_file_with_theorems(text: &str, theorems: &[String]) -> FileIndex {
    let mut indexer = Indexer::new(text);
    indexer.theorems.extend(theorems.iter().cloned());
    with_text_index(text.as_bytes(), || indexer.run());
    indexer.finish()
}

/// An open environment.
struct Frame {
    /// Index into `FileIndex::envs`.
    env: usize,
    /// The label title before the environment began; TeX groups restore it at the `\end`.
    saved_title: Option<Rc<str>>,
}

struct Indexer<'a> {
    s: &'a str,
    b: &'a [u8],
    lines: LineCounter<'a>,
    out: FileIndex,
    stack: Vec<Frame>,
    /// The current `\nameref` title (see [`Label::title`]).
    title: Option<Rc<str>>,
    /// Theorem-like environments declared in this file.
    theorems: HashSet<String>,
    /// Environments declared with `\newenvironment` (never theorem-like).
    plain_envs: HashSet<String>,
    /// Conditionals declared with `\newif`, for `\iffalse` nesting and [`Self::if_depth`].
    conditionals: HashSet<String>,
    /// How many `\if…` (primitive or `\newif`) are open at the scan position, up to their `\fi`.
    if_depth: u32,
    /// Scanning stops here (`\end{document}`, or `\endinput` outside any conditional).
    limit: usize,
    /// How many frames of [`Self::stack`] each environment name has (so an `\end` of a name
    /// that is not open does not search the whole stack).
    open_names: HashMap<String, usize>,
    /// For verbatim environments whose `\end` was not found after a position: that position
    /// (it is not found after any later one either).
    verbatim_unclosed: HashMap<String, usize>,
    /// The answers of [`Self::skip_if_false`] after an unterminated `\iffalse` (see
    /// [`IfProfile`]).
    if_profile: Option<IfProfile>,
}

/// Where [`Indexer::skip_if_false`] resumes for each conditional after a position, computed in
/// one pass once an `\iffalse` was found unterminated: otherwise each later `\iffalse` would scan
/// to the end of the text again.
struct IfProfile {
    /// The position the pass started from.
    from: usize,
    /// How many `\newif` conditionals were known (more change the answers).
    conditionals: usize,
    /// The position after a conditional's name → where scanning resumes and whether that is in
    /// its `\else` branch.
    answers: HashMap<usize, (usize, bool)>,
}

impl<'a> Indexer<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s,
            b: s.as_bytes(),
            lines: LineCounter::new(s.as_bytes()),
            out: FileIndex::default(),
            stack: Vec::new(),
            title: None,
            theorems: HashSet::new(),
            plain_envs: HashSet::new(),
            conditionals: HashSet::new(),
            if_depth: 0,
            limit: s.len(),
            open_names: HashMap::new(),
            verbatim_unclosed: HashMap::new(),
            if_profile: None,
        }
    }

    fn run(&mut self) {
        let (s, b) = (self.s, self.b);
        let mut i = 0;
        while i < self.limit {
            let Some(off) = memchr::memchr2(b'\\', b'%', &b[i..self.limit]) else {
                break;
            };
            let p = i + off;
            if b[p] == b'%' {
                i = line_end(b, p);
                continue;
            }
            let Some((name, after)) = control_sequence(b, p) else {
                break;
            };
            let name = &s[name];
            i = match commands::lookup(name) {
                Some(cmd) => self.command(cmd, name, p, after).max(after),
                None => {
                    self.track_conditional(name);
                    after
                }
            };
        }
    }

    /// Keeps [`Self::if_depth`] up to date for a control sequence the pass does not handle.
    fn track_conditional(&mut self, name: &str) {
        if name == "fi" {
            self.if_depth = self.if_depth.saturating_sub(1);
        } else if self.is_conditional(name) {
            self.if_depth += 1;
        }
    }

    /// Whether `\name` opens a conditional that ends with `\fi`.
    fn is_conditional(&self, name: &str) -> bool {
        name.starts_with("if")
            && (is_primitive_conditional(name) || self.conditionals.contains(name))
    }

    fn finish(mut self) -> FileIndex {
        self.out.magic = parse_magic(self.s);
        self.out
    }

    fn line(&mut self, pos: usize) -> u32 {
        self.lines.line_at(pos)
    }

    /// Handles one command; returns where scanning resumes (inside an argument that may contain
    /// labels or citations, or after the arguments).
    fn command(&mut self, cmd: Cmd, name: &str, start: usize, after: usize) -> usize {
        match cmd {
            Cmd::Section { level, unnumbered } => self.section(start, after, level, unnumbered),
            Cmd::Label => self.label(start, after),
            Cmd::Ref(form) => self.reference(name, start, after, form),
            Cmd::Cite(form) => self.citation(name, start, after, form),
            Cmd::Begin => self.begin(start, after),
            Cmd::End => self.end(start, after),
            Cmd::Input => self.input(start, after),
            Cmd::FileArg(kind) => self.file_arg(start, after, kind),
            Cmd::Import => self.import(start, after),
            Cmd::Graphics => {
                let (_, j) = star(self.b, after);
                let j = skip_optionals(self.b, j, 2);
                self.single_include(start, after, j, IncludeKind::Graphics)
            }
            Cmd::Bibliography => self.list_include(start, after, after, IncludeKind::Bibliography),
            Cmd::BibResource => {
                let j = skip_optionals(self.b, after, 1);
                self.single_include(start, after, j, IncludeKind::Bibliography)
            }
            Cmd::Package => {
                let j = skip_optionals(self.b, after, 1);
                self.list_include(start, after, j, IncludeKind::Package)
            }
            Cmd::DocumentClass => {
                self.out.has_documentclass = true;
                after
            }
            Cmd::NewCommand => self.new_command(start, after),
            Cmd::Def => self.def(start, after),
            Cmd::Let => self.let_(start, after),
            Cmd::MathOperator => self.math_operator(start, after),
            Cmd::PairedDelimiter => self.paired_delimiter(name, start, after),
            Cmd::DocumentCommand => self.document_command(start, after),
            Cmd::NewEnvironment => self.new_environment(start, after, false),
            Cmd::DocumentEnvironment => self.new_environment(start, after, true),
            Cmd::NewTheorem => self.new_theorem(start, after),
            Cmd::DeclareTheorem => self.declare_theorem(start, after),
            Cmd::NewIf => self.new_if(after),
            Cmd::Caption => self.caption(after),
            Cmd::InlineVerb => {
                let j = if at(self.b, after) == b'*' {
                    after + 1
                } else {
                    after
                };
                skip_delimited(self.b, j).unwrap_or(after)
            }
            Cmd::InlineCode { lang } => self.inline_code(after, lang),
            Cmd::Url => skip_inline_verbatim(self.b, skip_blanks(self.b, after)).unwrap_or(after),
            Cmd::Href => {
                let j = skip_optionals(self.b, after, 1);
                let j = skip_space(self.b, j);
                raw_group(self.b, j).map_or(after, |(_, end)| end)
            }
            Cmd::IfFalse => {
                let (resume, in_else) = self.skip_if_false(after);
                if in_else {
                    // The `\else` branch is read; its `\fi` is still to come.
                    self.if_depth += 1;
                }
                resume
            }
            Cmd::EndInput => {
                if self.if_depth == 0 {
                    self.limit = line_end(self.b, after);
                }
                after
            }
        }
    }

    // ----- outline, labels, references, citations -----

    fn section(&mut self, start: usize, after: usize, level: u8, unnumbered: bool) -> usize {
        let b = self.b;
        let (starred, j) = star(b, after);
        let mut j = skip_space(b, j);
        if at(b, j) == b'[' {
            let Some((_, end)) = bracket(b, j) else {
                return after;
            };
            j = skip_space(b, end);
        }
        let Some((content, _)) = brace(b, j) else {
            return after;
        };
        let title = clean_title(&self.s[content.clone()]);
        let line = self.line(start);
        self.title = Some(Rc::from(title.as_str()));
        self.out.outline.push(OutlineItem {
            level,
            title,
            line,
            starred: starred || unnumbered,
            env: None,
        });
        content.start
    }

    fn label(&mut self, start: usize, after: usize) -> usize {
        let j = skip_space(self.b, after);
        let Some((content, end)) = brace(self.b, j) else {
            return after;
        };
        let name = clean(&self.s[content]);
        if name.is_empty() {
            return end;
        }
        let line = self.line(start);
        let env = self
            .stack
            .iter()
            .rev()
            .map(|f| &self.out.envs[f.env].name)
            .find(|n| *n != "document")
            .cloned();
        let title = self.title.as_deref().map(str::to_owned);
        self.out.labels.push(Label {
            name,
            line,
            env,
            title,
        });
        end
    }

    fn reference(&mut self, name: &str, start: usize, after: usize, form: RefForm) -> usize {
        let (b, s) = (self.b, self.s);
        let (starred, j) = star(b, after);
        let mut names = Vec::new();
        let mut end = after;
        match form {
            RefForm::Single | RefForm::List => {
                let j = skip_space(b, skip_optionals(b, j, 1));
                if let Some((content, e)) = brace(b, j) {
                    if form == RefForm::List {
                        names = split_list(&s[content]);
                    } else {
                        names.push(clean(&s[content]));
                    }
                    end = e;
                }
            }
            RefForm::Range => {
                let mut j = j;
                for _ in 0..2 {
                    let Some((content, e)) = brace(b, skip_space(b, j)) else {
                        break;
                    };
                    names.push(clean(&s[content]));
                    j = e;
                    end = e;
                }
            }
            RefForm::Hyperref => {
                if let Some((content, e)) = bracket(b, skip_space(b, j)) {
                    names.push(clean(&s[content]));
                    end = e;
                }
            }
        }
        names.retain(|n| !n.is_empty());
        if !names.is_empty() {
            let line = self.line(start);
            self.out.refs.push(Reference {
                cmd: starred_name(name, starred),
                names,
                line,
            });
        }
        end
    }

    fn citation(&mut self, name: &str, start: usize, after: usize, form: CiteForm) -> usize {
        let (b, s) = (self.b, self.s);
        let (starred, mut j) = star(b, after);
        let mut keys = Vec::new();
        let mut end = after;
        match form {
            CiteForm::Single => {
                let k = skip_space(b, skip_optionals(b, j, 2));
                if let Some((content, e)) = brace(b, k) {
                    keys = split_list(&s[content]);
                    end = e;
                }
            }
            CiteForm::Multi => {
                // Global (prenote)(postnote), then any number of [pre][post]{keys} groups.
                for _ in 0..2 {
                    let k = skip_space(b, j);
                    if at(b, k) != b'(' {
                        break;
                    }
                    let Some((_, e)) = group(b, k, false) else {
                        break;
                    };
                    j = e;
                }
                loop {
                    let k = skip_space(b, skip_optionals(b, j, 2));
                    let Some((content, e)) = brace(b, k) else {
                        break;
                    };
                    keys.extend(split_list(&s[content]));
                    j = e;
                    end = e;
                }
            }
        }
        keys.retain(|k| k != "*");
        if !keys.is_empty() {
            let line = self.line(start);
            self.out.cites.push(Citation {
                cmd: starred_name(name, starred),
                keys,
                line,
            });
        }
        end
    }

    fn caption(&mut self, after: usize) -> usize {
        let j = skip_space(self.b, skip_optionals(self.b, after, 1));
        let Some((content, _)) = brace(self.b, j) else {
            return after;
        };
        let title = clean_title(&self.s[content.clone()]);
        if !title.is_empty() {
            self.title = Some(Rc::from(title.as_str()));
        }
        content.start
    }

    // ----- environments -----

    fn is_theorem(&self, name: &str) -> bool {
        let base = name.strip_suffix('*').unwrap_or(name);
        if self.plain_envs.contains(name) || self.plain_envs.contains(base) {
            return false;
        }
        self.theorems.contains(name) || self.theorems.contains(base) || is_default_theorem(name)
    }

    fn begin(&mut self, start: usize, after: usize) -> usize {
        let b = self.b;
        let j = skip_space(b, after);
        let Some((content, end)) = brace(b, j) else {
            return after;
        };
        let name = clean(&self.s[content]);
        if name.is_empty() {
            return end;
        }
        let line = self.line(start);
        if name == "document" {
            self.out.has_begin_document = true;
        }
        if is_verbatim_env(&name)
            && self
                .verbatim_unclosed
                .get(&name)
                .is_none_or(|&from| end < from)
        {
            let closing = format!("\\end{{{name}}}");
            if let Some(off) = memchr::memmem::find(&b[end..], closing.as_bytes()) {
                let close_at = end + off;
                let end_line = self.line(close_at);
                self.out.envs.push(EnvPair {
                    name,
                    begin_line: line,
                    end_line: Some(end_line),
                });
                return close_at + closing.len();
            }
            self.verbatim_unclosed.insert(name.clone(), end);
        }
        let theorem = self.is_theorem(&name);
        *self.open_names.entry(name.clone()).or_default() += 1;
        self.stack.push(Frame {
            env: self.out.envs.len(),
            saved_title: self.title.clone(),
        });
        self.out.envs.push(EnvPair {
            name: name.clone(),
            begin_line: line,
            end_line: None,
        });
        if !theorem {
            return end;
        }
        let k = skip_space(b, end);
        let (title, resume) = match bracket(b, k) {
            Some((content, _)) => (clean_title(&self.s[content.clone()]), content.start),
            None => (String::new(), end),
        };
        self.title = (!title.is_empty()).then(|| Rc::from(title.as_str()));
        self.out.outline.push(OutlineItem {
            level: 7,
            title,
            line,
            starred: name.ends_with('*'),
            env: Some(name),
        });
        resume
    }

    fn end(&mut self, start: usize, after: usize) -> usize {
        let j = skip_space(self.b, after);
        let Some((content, end)) = brace(self.b, j) else {
            return after;
        };
        let name = clean(&self.s[content]);
        let line = self.line(start);
        let envs = &mut self.out.envs;
        let open = self.open_names.get(&name).is_some_and(|&n| n > 0);
        if open && let Some(k) = self.stack.iter().rposition(|f| envs[f.env].name == name) {
            envs[self.stack[k].env].end_line = Some(line);
            self.title = self.stack[k].saved_title.take();
            for frame in self.stack.drain(k..) {
                if let Some(n) = self.open_names.get_mut(&envs[frame.env].name) {
                    *n = n.saturating_sub(1);
                }
            }
        }
        if name == "document" {
            self.limit = end;
        }
        end
    }

    // ----- includes and packages -----

    fn push_include(&mut self, start: usize, kind: IncludeKind, target: String) {
        if target.is_empty() {
            return;
        }
        let line = self.line(start);
        self.out.includes.push(Include { kind, target, line });
    }

    /// `\cmd…{file}` where `j` is just before the braced argument.
    fn single_include(&mut self, start: usize, after: usize, j: usize, kind: IncludeKind) -> usize {
        let Some((content, end)) = brace(self.b, skip_space(self.b, j)) else {
            return after;
        };
        let target = unquote(&clean(&self.s[content]));
        self.push_include(start, kind, target);
        end
    }

    /// `\cmd…{a,b}` where `j` is just before the braced list.
    fn list_include(&mut self, start: usize, after: usize, j: usize, kind: IncludeKind) -> usize {
        let Some((content, end)) = brace(self.b, skip_space(self.b, j)) else {
            return after;
        };
        for item in split_list(&self.s[content]) {
            if kind == IncludeKind::Package && !self.out.packages.contains(&item) {
                self.out.packages.push(item.clone());
            }
            self.push_include(start, kind, item);
        }
        end
    }

    fn file_arg(&mut self, start: usize, after: usize, kind: IncludeKind) -> usize {
        let j = skip_optionals(self.b, after, 1);
        self.single_include(start, after, j, kind)
    }

    /// `\input{file}` or the primitive form `\input file`.
    fn input(&mut self, start: usize, after: usize) -> usize {
        let b = self.b;
        if at(b, skip_space(b, after)) == b'{' {
            return self.single_include(start, after, after, IncludeKind::Input);
        }
        let j = skip_blanks(b, after);
        if j == after {
            return after;
        }
        let mut k = j;
        while k < b.len()
            && !matches!(
                b[k],
                b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'%' | b'\\'
            )
        {
            k += 1;
        }
        let target = self.s[j..k].to_owned();
        self.push_include(start, IncludeKind::Input, target);
        k
    }

    fn import(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let (_, j) = star(b, after);
        let Some((dir, e)) = brace(b, skip_space(b, j)) else {
            return after;
        };
        let Some((file, end)) = brace(b, skip_space(b, e)) else {
            return after;
        };
        let (dir, file) = (clean(&s[dir]), clean(&s[file]));
        let target = if dir.is_empty() || dir.ends_with('/') {
            format!("{dir}{file}")
        } else {
            format!("{dir}/{file}")
        };
        self.push_include(start, IncludeKind::Import, target);
        end
    }

    // ----- definitions -----

    fn push_macro(&mut self, start: usize, def: MacroDef) {
        let line = self.line(start);
        self.out.macros.push(MacroDef { line, ..def });
    }

    fn new_command(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let (_, j) = star(b, after);
        let Some((name, mut j)) = defined_name(s, j) else {
            return after;
        };
        let mut nargs = 0;
        let mut default = None;
        if let Some((n, e)) = arg_count(s, j) {
            nargs = n;
            j = e;
            let k = skip_space(b, j);
            if let Some((range, e)) = bracket(b, k) {
                default = Some(clean(&s[range]));
                j = e;
            }
        }
        let (body, end) = long_brace(s, skip_space(b, j)).map_or((None, j), |(t, e)| (Some(t), e));
        let def = MacroDef {
            optional_args: u8::from(default.is_some() && nargs > 0),
            optional_default: default.filter(|_| nargs > 0),
            ..plain_macro(name, nargs, body)
        };
        self.push_macro(start, def);
        end
    }

    /// `\def\name<parameter text>{body}` (also `\gdef`, `\edef`, `\xdef`).
    fn def(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let Some((name, mut k)) = control_sequence(b, skip_space(b, after)) else {
            return after;
        };
        let name = &s[name];
        if name == "csname" {
            return after;
        }
        let limit = (k + 256).min(b.len());
        let mut nargs = 0u8;
        while k < limit && b[k] != b'{' {
            match b[k] {
                b'#' => {
                    if at(b, k + 1).is_ascii_digit() {
                        nargs = nargs.max(at(b, k + 1) - b'0');
                    }
                    k += 2;
                }
                b'\\' => k = control_sequence(b, k).map_or(k + 1, |(_, e)| e),
                b'%' => k = line_end(b, k) + 1,
                _ => k += 1,
            }
        }
        let Some((body, end)) = long_brace(s, k) else {
            return after;
        };
        self.push_macro(start, plain_macro(name, nargs, Some(body)));
        end
    }

    /// `\let\a\b`, `\let\a=\b`, `\NewCommandCopy{\a}{\b}`.
    fn let_(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let Some((name, k)) = defined_name(s, after) else {
            return after;
        };
        let mut k = skip_blanks(b, k);
        if at(b, k) == b'=' {
            k = skip_blanks(b, k + 1);
        }
        let (target, end) = match at(b, k) {
            b'\\' => match control_sequence(b, k) {
                Some((_, e)) => (s[k..e].to_owned(), e),
                None => return after,
            },
            b'{' => match brace(b, k) {
                Some((content, e)) => (clean(&s[content]), e),
                None => return after,
            },
            0 | b' ' | b'\t' | b'\r' | b'\n' => return after,
            _ => match s[k..].chars().next() {
                Some(c) => (c.to_string(), k + c.len_utf8()),
                None => return after,
            },
        };
        self.push_macro(start, plain_macro(name, 0, Some(target)));
        end
    }

    fn math_operator(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let (_, j) = star(b, after);
        let Some((name, j)) = defined_name(s, j) else {
            return after;
        };
        let (body, end) = match brace(b, skip_space(b, j)) {
            Some((content, e)) => (Some(clean(&s[content])), e),
            None => (None, j),
        };
        let def = MacroDef {
            is_math_operator: true,
            ..plain_macro(name, 0, body)
        };
        self.push_macro(start, def);
        end
    }

    /// `\DeclarePairedDelimiter{\abs}{\lvert}{\rvert}` and the `X` variant with `[n]` and a body.
    fn paired_delimiter(&mut self, cmd: &str, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let Some((name, mut j)) = defined_name(s, after) else {
            return after;
        };
        let mut nargs = 1;
        if let Some((n, e)) = arg_count(s, j) {
            nargs = n;
            j = e;
        }
        let mut parts = Vec::new();
        let count = if cmd.ends_with('X') { 3 } else { 2 };
        for _ in 0..count {
            let Some((content, e)) = brace(b, skip_space(b, j)) else {
                break;
            };
            parts.push(clean(&s[content]));
            j = e;
        }
        let body = match parts.as_slice() {
            [left, right] => Some(format!("{left}#1{right}")),
            [left, right, inner] => Some(format!("{left}{inner}{right}")),
            _ => None,
        };
        self.push_macro(start, plain_macro(name, nargs, body));
        j
    }

    /// `\NewDocumentCommand{\name}{spec}{body}` and its relatives.
    fn document_command(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let Some((name, j)) = defined_name(s, after) else {
            return after;
        };
        let Some((spec, j)) = brace(b, skip_space(b, j)) else {
            return after;
        };
        let args = xparse_args(&s[spec]);
        let (body, end) = long_brace(s, skip_space(b, j)).map_or((None, j), |(t, e)| (Some(t), e));
        let def = MacroDef {
            optional_args: args.optional,
            optional_default: args.first_default,
            ..plain_macro(name, args.total, body)
        };
        self.push_macro(start, def);
        end
    }

    /// `\newenvironment{name}[n][default]{begin}{end}` or, with `xparse`,
    /// `\NewDocumentEnvironment{name}{spec}{begin}{end}`.
    fn new_environment(&mut self, start: usize, after: usize, xparse: bool) -> usize {
        let (b, s) = (self.b, self.s);
        let (_, j) = star(b, after);
        let Some((name, mut j)) = brace(b, skip_space(b, j)) else {
            return after;
        };
        let name = clean(&s[name]);
        let mut nargs = 0;
        let (mut optional_args, mut optional_default) = (0, None);
        if xparse {
            if let Some((spec, e)) = brace(b, skip_space(b, j)) {
                let args = xparse_args(&s[spec]);
                (nargs, optional_args, optional_default) =
                    (args.total, args.optional, args.first_default);
                j = e;
            }
        } else if let Some((n, e)) = arg_count(s, j) {
            nargs = n;
            j = e;
            if let Some((range, e)) = bracket(b, skip_space(b, j)) {
                if n > 0 {
                    (optional_args, optional_default) = (1, Some(clean(&s[range])));
                }
                j = e;
            }
        }
        let mut body = None;
        if let Some((begin_code, e)) = long_brace(s, skip_space(b, j)) {
            body = Some(begin_code);
            j = e;
            if let Some((_, e)) = long_brace(s, skip_space(b, j)) {
                j = e;
            }
        }
        if name.is_empty() {
            return j;
        }
        self.theorems.remove(&name);
        self.plain_envs.insert(name.clone());
        let def = MacroDef {
            is_env: true,
            optional_args,
            optional_default,
            ..plain_macro(&name, nargs, body)
        };
        self.push_macro(start, def);
        j
    }

    /// `\newtheorem{name}{Title}`, `\newtheorem{name}[counter]{Title}`,
    /// `\newtheorem{name}{Title}[parent]` and `\newtheorem*`.
    fn new_theorem(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let (_, j) = star(b, after);
        let Some((name, mut j)) = brace(b, skip_space(b, j)) else {
            return after;
        };
        let name = clean(&s[name]);
        if let Some((_, e)) = bracket(b, skip_space(b, j)) {
            j = e;
        }
        let mut title = None;
        if let Some((content, e)) = brace(b, skip_space(b, j)) {
            title = Some(clean(&s[content]));
            j = e;
            if let Some((_, e)) = bracket(b, skip_space(b, j)) {
                j = e;
            }
        }
        if !name.is_empty() {
            self.add_theorem(start, name, title);
        }
        j
    }

    /// thmtools' `\declaretheorem[opts]{name}` or `\declaretheorem{a,b}[opts]`.
    fn declare_theorem(&mut self, start: usize, after: usize) -> usize {
        let (b, s) = (self.b, self.s);
        let mut opts = String::new();
        let mut j = skip_space(b, after);
        if let Some((content, e)) = bracket(b, j) {
            opts.push_str(&s[content]);
            j = skip_space(b, e);
        }
        let Some((names, mut end)) = brace(b, j) else {
            return after;
        };
        if let Some((content, e)) = bracket(b, skip_space(b, end)) {
            opts.push(',');
            opts.push_str(&s[content]);
            end = e;
        }
        let title = key_value(&opts, &["name", "title"]);
        for name in split_list(&s[names]) {
            self.add_theorem(start, name, title.clone());
        }
        end
    }

    fn add_theorem(&mut self, start: usize, name: String, title: Option<String>) {
        self.plain_envs.remove(&name);
        self.theorems.insert(name.clone());
        let def = MacroDef {
            is_env: true,
            is_theorem: true,
            ..plain_macro(&name, 0, title)
        };
        self.push_macro(start, def);
    }

    fn new_if(&mut self, after: usize) -> usize {
        let b = self.b;
        let Some((name, end)) = control_sequence(b, skip_space(b, after)) else {
            return after;
        };
        let name = &self.s[name];
        if name.starts_with("if") {
            self.conditionals.insert(name.to_owned());
        }
        end
    }

    // ----- skipped material -----

    fn inline_code(&self, after: usize, lang: bool) -> usize {
        let b = self.b;
        let mut j = skip_optionals(b, after, 1);
        if lang {
            match brace(b, skip_space(b, j)) {
                Some((_, e)) => j = e,
                None => return after,
            }
        }
        skip_inline_verbatim(b, skip_blanks(b, j)).unwrap_or(after)
    }

    /// Skips `\iffalse … \fi` (resuming after `\else` if there is one at the same depth). Leaves an
    /// unterminated `\iffalse` alone. Returns where scanning resumes and whether that is inside the
    /// `\else` branch (so the conditional is still open).
    fn skip_if_false(&mut self, after: usize) -> (usize, bool) {
        if let Some(profile) = &self.if_profile
            && after >= profile.from
            && profile.conditionals == self.conditionals.len()
            && let Some(&answer) = profile.answers.get(&after)
        {
            return answer;
        }
        let b = self.b;
        let mut depth = 1u32;
        let mut i = after;
        while let Some(off) = memchr::memchr2(b'\\', b'%', &b[i..]) {
            let p = i + off;
            if b[p] == b'%' {
                i = line_end(b, p);
                continue;
            }
            let Some((name, e)) = control_sequence(b, p) else {
                break;
            };
            match &self.s[name] {
                "fi" => {
                    depth -= 1;
                    if depth == 0 {
                        return (e, false);
                    }
                }
                "else" if depth == 1 => return (e, true),
                n if self.is_conditional(n) => depth += 1,
                _ => {}
            }
            i = e;
        }
        // Unterminated: answer every later conditional from one more pass.
        self.if_profile = Some(self.if_profile_from(after));
        (after, false)
    }

    /// The [`IfProfile`] of the text after `from` (a stack of the open conditionals: a `\fi`
    /// closes the innermost, the first `\else` while one is innermost is its `\else`).
    fn if_profile_from(&self, from: usize) -> IfProfile {
        let b = self.b;
        let mut answers = HashMap::new();
        let mut open: Vec<(usize, bool)> = Vec::new();
        let mut i = from;
        while let Some(off) = memchr::memchr2(b'\\', b'%', &b[i..]) {
            let p = i + off;
            if b[p] == b'%' {
                i = line_end(b, p);
                continue;
            }
            let Some((name, e)) = control_sequence(b, p) else {
                break;
            };
            match &self.s[name] {
                "fi" => {
                    if let Some((at, decided)) = open.pop()
                        && !decided
                    {
                        answers.insert(at, (e, false));
                    }
                }
                "else" => {
                    if let Some((at, decided)) = open.last_mut()
                        && !*decided
                    {
                        *decided = true;
                        answers.insert(*at, (e, true));
                    }
                }
                n if self.is_conditional(n) => open.push((e, false)),
                _ => {}
            }
            i = e;
        }
        for (at, decided) in open {
            if !decided {
                answers.insert(at, (at, false));
            }
        }
        IfProfile {
            from,
            conditionals: self.conditionals.len(),
            answers,
        }
    }
}

/// A `{…}` argument at `i` that may span paragraphs (macro bodies), cleaned.
fn long_brace(s: &str, i: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if at(b, i) != b'{' {
        return None;
    }
    let (content, end) = group(b, i, true)?;
    Some((clean(&s[content]), end))
}

/// Skips `|…|` (any delimiter on the same line) or a raw `{…}` group at `i`.
fn skip_inline_verbatim(b: &[u8], i: usize) -> Option<usize> {
    if at(b, i) == b'{' {
        raw_group(b, i).map(|(_, end)| end)
    } else {
        skip_delimited(b, i)
    }
}

/// Skips `<c>…<c>` where `<c>` is the ASCII character at `i`; the closing one must be on the same
/// line (otherwise everything up to the line end is skipped, like LaTeX's error recovery).
fn skip_delimited(b: &[u8], i: usize) -> Option<usize> {
    let d = at(b, i);
    if d == 0 || !d.is_ascii() || d.is_ascii_alphabetic() || d.is_ascii_whitespace() {
        return None;
    }
    let eol = line_end(b, i + 1);
    Some(memchr::memchr(d, &b[i + 1..eol]).map_or(eol, |o| i + 1 + o + 1))
}

fn plain_macro(name: &str, nargs: u8, body: Option<String>) -> MacroDef {
    MacroDef {
        name: name.to_owned(),
        nargs,
        body,
        line: 0,
        is_env: false,
        is_theorem: false,
        is_math_operator: false,
        optional_args: 0,
        optional_default: None,
    }
}

fn starred_name(name: &str, starred: bool) -> String {
    if starred {
        format!("{name}*")
    } else {
        name.to_owned()
    }
}

/// Strips quotes from `"file name"` targets.
fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s)
        .to_owned()
}

/// The value of the first of `keys` in a `key=value` list.
fn key_value(opts: &str, keys: &[&str]) -> Option<String> {
    split_list(opts).into_iter().find_map(|item| {
        let (k, v) = item.split_once('=')?;
        keys.contains(&k.trim())
            .then(|| strip_outer_braces(v.trim()).to_owned())
    })
}

/// Cleans a heading: removes `\label{…}`, `\index{…}`, `\footnote[…]{…}` and `\protect`.
fn clean_title(raw: &str) -> String {
    let s = clean(raw);
    if !s.contains('\\') {
        return s;
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut copied) = (0, 0);
    while let Some(off) = memchr::memchr(b'\\', &b[i..]) {
        let p = i + off;
        let Some((name, e)) = control_sequence(b, p) else {
            break;
        };
        let drop_to = match &s[name] {
            "label" | "index" | "footnote" => {
                brace(b, skip_space(b, skip_optionals(b, e, 1))).map(|(_, end)| end)
            }
            "protect" => Some(skip_blanks(b, e)),
            _ => None,
        };
        match drop_to {
            Some(d) => {
                out.push_str(&s[copied..p]);
                copied = d;
                i = d;
            }
            None => i = e,
        }
    }
    out.push_str(&s[copied..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The arguments an `xparse` spec such as `s o m O{x} e{^_}` declares.
#[derive(Debug, PartialEq, Eq)]
struct XparseArgs {
    /// All of them (as [`MacroDef::nargs`] counts them).
    total: u8,
    /// The optional ones (`s`, `o`, `O`, `d`, `D`, `t`, `e`, `E`).
    optional: u8,
    /// The default of the first argument when it is `o` (`""`) or `O{default}`.
    first_default: Option<String>,
}

/// The arguments of the `xparse` spec `spec` (see [`XparseArgs`]).
fn xparse_args(spec: &str) -> XparseArgs {
    let b = spec.as_bytes();
    let mut n = 0usize;
    let mut optional = 0usize;
    let mut first_default = None;
    let mut i = 0;
    // Skips one token (a character or a control sequence) or a braced group.
    let token = |i: usize| -> usize {
        match at(b, i) {
            b'\\' => control_sequence(b, i).map_or(i + 1, |(_, e)| e),
            b'{' => group(b, i, true).map_or(b.len(), |(_, e)| e),
            _ => i + 1,
        }
    };
    let tokens_in_group = |i: usize| -> (usize, usize) {
        match group(b, i, true) {
            Some((r, e)) if at(b, i) == b'{' => {
                let (mut k, mut count) = (r.start, 0);
                while k < r.end {
                    if !at(b, k).is_ascii_whitespace() {
                        count += 1;
                    }
                    k = token(k);
                }
                (count, e)
            }
            _ => (1, token(i)),
        }
    };
    while i < b.len() {
        let c = b[i];
        let first = n == 0;
        i += 1;
        match c {
            b'm' | b'v' | b'b' | b'l' => n += 1,
            b'o' | b's' => {
                if first && c == b'o' {
                    first_default = Some(String::new());
                }
                n += 1;
                optional += 1;
            }
            b'O' => {
                let k = skip_space(b, i);
                i = token(k);
                if first {
                    let default = match group(b, k, true) {
                        Some((r, _)) if at(b, k) == b'{' => &spec[r],
                        _ => &spec[k..i],
                    };
                    first_default = Some(clean(default));
                }
                n += 1;
                optional += 1;
            }
            b't' => {
                n += 1;
                optional += 1;
                i = token(skip_space(b, i));
            }
            b'r' => {
                n += 1;
                i = token(token(skip_space(b, i)));
            }
            b'd' => {
                n += 1;
                optional += 1;
                i = token(token(skip_space(b, i)));
            }
            b'R' => {
                n += 1;
                i = token(token(token(skip_space(b, i))));
            }
            b'D' => {
                n += 1;
                optional += 1;
                i = token(token(token(skip_space(b, i))));
            }
            b'e' => {
                let (count, e) = tokens_in_group(skip_space(b, i));
                n += count;
                optional += count;
                i = e;
            }
            b'E' => {
                let (count, e) = tokens_in_group(skip_space(b, i));
                n += count;
                optional += count;
                i = token(skip_space(b, e));
            }
            b'u' => {
                n += 1;
                i = token(skip_space(b, i));
            }
            b'>' => i = token(skip_space(b, i)),
            _ => {}
        }
    }
    let total = u8::try_from(n.min(9)).unwrap_or(9);
    XparseArgs {
        total,
        optional: u8::try_from(optional).unwrap_or(9).min(total),
        first_default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xparse_arg_count(spec: &str) -> u8 {
        xparse_args(spec).total
    }

    #[test]
    fn xparse_counts() {
        assert_eq!(xparse_arg_count("m"), 1);
        assert_eq!(xparse_arg_count("s o m"), 3);
        assert_eq!(xparse_arg_count("O{default} m"), 2);
        assert_eq!(xparse_arg_count("D(){x} r[] m"), 3);
        assert_eq!(xparse_arg_count("e{^_}"), 2);
        assert_eq!(xparse_arg_count("E{^_}{{a}{b}} m"), 3);
        assert_eq!(xparse_arg_count("t+ >{\\SplitList{,}} +m"), 2);
        assert_eq!(xparse_arg_count(""), 0);
        let args = |spec: &str| {
            let a = xparse_args(spec);
            (a.total, a.optional, a.first_default)
        };
        assert_eq!(args("m"), (1, 0, None));
        assert_eq!(args("s o m"), (3, 2, None));
        assert_eq!(args("o m"), (2, 1, Some(String::new())));
        assert_eq!(args("O{default} m"), (2, 1, Some("default".into())));
        assert_eq!(args("O{} m m"), (3, 1, Some(String::new())));
        assert_eq!(args("m O{x}"), (2, 1, None));
        assert_eq!(args("D(){x} r[] m"), (3, 1, None));
        assert_eq!(args("e{^_}"), (2, 2, None));
    }

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(
            clean_title("Intro\\label{sec:a} \\protect\\foo  x\n y\\footnote[2]{n}"),
            "Intro \\foo x y"
        );
        assert_eq!(
            clean_title("  $\\mathcal{O}$-modules "),
            "$\\mathcal{O}$-modules"
        );
    }

    #[test]
    fn delimited_skips() {
        assert_eq!(skip_delimited(b"|a%b| x", 0), Some(5));
        assert_eq!(skip_delimited(b"|ab\nx|", 0), Some(3));
        assert_eq!(skip_delimited(b"a|", 0), None);
    }
}
