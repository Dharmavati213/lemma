// Owner: WP1.9 structure
//! The commands the structure pass reacts to, and the built-in environment lists.

use crate::IncludeKind;

/// How a reference command takes its label names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefForm {
    /// `\ref{a}`: one name, commas included.
    Single,
    /// `\cref{a,b}`: a comma list.
    List,
    /// `\crefrange{a}{b}`.
    Range,
    /// `\hyperref[a]{text}`.
    Hyperref,
}

/// How a citation command takes its keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CiteForm {
    /// `\cite[pre][post]{a,b}`.
    Single,
    /// biblatex `\cites(pre)(post)[pre][post]{a}[pre][post]{b}…`.
    Multi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cmd {
    Section {
        level: u8,
        unnumbered: bool,
    },
    Label,
    Ref(RefForm),
    Cite(CiteForm),
    Begin,
    End,
    /// `\input{f}` or TeX's `\input f`.
    Input,
    /// One braced file argument after optional arguments.
    FileArg(IncludeKind),
    /// `\import{dir}{file}` and its relatives.
    Import,
    Graphics,
    /// `\bibliography{a,b}`.
    Bibliography,
    /// `\addbibresource[opts]{file.bib}`.
    BibResource,
    Package,
    DocumentClass,
    NewCommand,
    Def,
    Let,
    MathOperator,
    PairedDelimiter,
    DocumentCommand,
    NewEnvironment,
    DocumentEnvironment,
    NewTheorem,
    DeclareTheorem,
    NewIf,
    Caption,
    /// `\verb|x|`: a delimiter character only.
    InlineVerb,
    /// `\lstinline[opts]{x}` / `\mintinline[opts]{lang}{x}`: braces or a delimiter character.
    InlineCode {
        lang: bool,
    },
    /// `\url{x}`: the argument is read verbatim.
    Url,
    /// `\href{url}{text}`: only the URL is verbatim.
    Href,
    IfFalse,
    EndInput,
}

/// Sorted by bytes for binary search (checked by a test).
const COMMANDS: &[(&str, Cmd)] = &[
    ("Autocite", Cmd::Cite(CiteForm::Single)),
    ("Autocites", Cmd::Cite(CiteForm::Multi)),
    ("Autoref", Cmd::Ref(RefForm::Single)),
    ("Cite", Cmd::Cite(CiteForm::Single)),
    ("Citealp", Cmd::Cite(CiteForm::Single)),
    ("Citealt", Cmd::Cite(CiteForm::Single)),
    ("Citeauthor", Cmd::Cite(CiteForm::Single)),
    ("Citep", Cmd::Cite(CiteForm::Single)),
    ("Cites", Cmd::Cite(CiteForm::Multi)),
    ("Citet", Cmd::Cite(CiteForm::Single)),
    ("Citetitle", Cmd::Cite(CiteForm::Single)),
    ("Cpageref", Cmd::Ref(RefForm::List)),
    ("Cpagerefrange", Cmd::Ref(RefForm::Range)),
    ("Cref", Cmd::Ref(RefForm::List)),
    ("Crefrange", Cmd::Ref(RefForm::Range)),
    ("DeclareCommandCopy", Cmd::Let),
    ("DeclareDocumentCommand", Cmd::DocumentCommand),
    ("DeclareDocumentEnvironment", Cmd::DocumentEnvironment),
    ("DeclareExpandableDocumentCommand", Cmd::DocumentCommand),
    ("DeclareMathOperator", Cmd::MathOperator),
    ("DeclarePairedDelimiter", Cmd::PairedDelimiter),
    ("DeclarePairedDelimiterX", Cmd::PairedDelimiter),
    ("DeclareRobustCommand", Cmd::NewCommand),
    ("Fref", Cmd::Ref(RefForm::Single)),
    ("InputIfFileExists", Cmd::FileArg(IncludeKind::Input)),
    ("Nameref", Cmd::Ref(RefForm::Single)),
    ("NewCommandCopy", Cmd::Let),
    ("NewDocumentCommand", Cmd::DocumentCommand),
    ("NewDocumentEnvironment", Cmd::DocumentEnvironment),
    ("NewExpandableDocumentCommand", Cmd::DocumentCommand),
    ("Notecite", Cmd::Cite(CiteForm::Single)),
    ("Parencite", Cmd::Cite(CiteForm::Single)),
    ("Parencites", Cmd::Cite(CiteForm::Multi)),
    ("ProvideDocumentCommand", Cmd::DocumentCommand),
    ("ProvideDocumentEnvironment", Cmd::DocumentEnvironment),
    ("ProvideExpandableDocumentCommand", Cmd::DocumentCommand),
    ("Pvolcite", Cmd::Cite(CiteForm::Single)),
    ("RenewCommandCopy", Cmd::Let),
    ("RenewDocumentCommand", Cmd::DocumentCommand),
    ("RenewDocumentEnvironment", Cmd::DocumentEnvironment),
    ("RenewExpandableDocumentCommand", Cmd::DocumentCommand),
    ("RequirePackage", Cmd::Package),
    ("RequirePackageWithOptions", Cmd::Package),
    ("Smartcite", Cmd::Cite(CiteForm::Single)),
    ("Smartcites", Cmd::Cite(CiteForm::Multi)),
    ("Textcite", Cmd::Cite(CiteForm::Single)),
    ("Textcites", Cmd::Cite(CiteForm::Multi)),
    ("Verb", Cmd::InlineVerb),
    ("Volcite", Cmd::Cite(CiteForm::Single)),
    ("Vref", Cmd::Ref(RefForm::Single)),
    ("addbibresource", Cmd::BibResource),
    (
        "addchap",
        Cmd::Section {
            level: 1,
            unnumbered: true,
        },
    ),
    ("addglobalbib", Cmd::BibResource),
    (
        "addpart",
        Cmd::Section {
            level: 0,
            unnumbered: true,
        },
    ),
    (
        "addsec",
        Cmd::Section {
            level: 2,
            unnumbered: true,
        },
    ),
    ("addsectionbib", Cmd::BibResource),
    ("autocite", Cmd::Cite(CiteForm::Single)),
    ("autocites", Cmd::Cite(CiteForm::Multi)),
    ("autopageref", Cmd::Ref(RefForm::Single)),
    ("autoref", Cmd::Ref(RefForm::Single)),
    ("avolcite", Cmd::Cite(CiteForm::Single)),
    ("begin", Cmd::Begin),
    ("bibliography", Cmd::Bibliography),
    ("caption", Cmd::Caption),
    (
        "chapter",
        Cmd::Section {
            level: 1,
            unnumbered: false,
        },
    ),
    ("cite", Cmd::Cite(CiteForm::Single)),
    ("citealp", Cmd::Cite(CiteForm::Single)),
    ("citealt", Cmd::Cite(CiteForm::Single)),
    ("citeauthor", Cmd::Cite(CiteForm::Single)),
    ("citedate", Cmd::Cite(CiteForm::Single)),
    ("citenum", Cmd::Cite(CiteForm::Single)),
    ("citep", Cmd::Cite(CiteForm::Single)),
    ("cites", Cmd::Cite(CiteForm::Multi)),
    ("citet", Cmd::Cite(CiteForm::Single)),
    ("citetitle", Cmd::Cite(CiteForm::Single)),
    ("citeurl", Cmd::Cite(CiteForm::Single)),
    ("citeyear", Cmd::Cite(CiteForm::Single)),
    ("citeyearpar", Cmd::Cite(CiteForm::Single)),
    ("cpageref", Cmd::Ref(RefForm::List)),
    ("cpagerefrange", Cmd::Ref(RefForm::Range)),
    ("cref", Cmd::Ref(RefForm::List)),
    ("crefrange", Cmd::Ref(RefForm::Range)),
    ("declaretheorem", Cmd::DeclareTheorem),
    ("def", Cmd::Def),
    ("documentclass", Cmd::DocumentClass),
    ("documentstyle", Cmd::DocumentClass),
    ("edef", Cmd::Def),
    ("end", Cmd::End),
    ("endinput", Cmd::EndInput),
    ("eqref", Cmd::Ref(RefForm::Single)),
    ("fnotecite", Cmd::Cite(CiteForm::Single)),
    ("footcite", Cmd::Cite(CiteForm::Single)),
    ("footcites", Cmd::Cite(CiteForm::Multi)),
    ("footcitetext", Cmd::Cite(CiteForm::Single)),
    ("footcitetexts", Cmd::Cite(CiteForm::Multi)),
    ("footfullcite", Cmd::Cite(CiteForm::Single)),
    ("fref", Cmd::Ref(RefForm::Single)),
    ("fullcite", Cmd::Cite(CiteForm::Single)),
    ("fvolcite", Cmd::Cite(CiteForm::Single)),
    ("gdef", Cmd::Def),
    ("href", Cmd::Href),
    ("hyperref", Cmd::Ref(RefForm::Hyperref)),
    ("iffalse", Cmd::IfFalse),
    ("import", Cmd::Import),
    ("include", Cmd::FileArg(IncludeKind::Include)),
    ("includefrom", Cmd::Import),
    ("includegraphics", Cmd::Graphics),
    ("includepdf", Cmd::Graphics),
    ("includestandalone", Cmd::FileArg(IncludeKind::Input)),
    ("includesvg", Cmd::Graphics),
    ("input", Cmd::Input),
    ("inputfrom", Cmd::Import),
    ("label", Cmd::Label),
    ("labelcpageref", Cmd::Ref(RefForm::List)),
    ("labelcref", Cmd::Ref(RefForm::List)),
    ("lcnamecref", Cmd::Ref(RefForm::Single)),
    ("lcnamecrefs", Cmd::Ref(RefForm::List)),
    ("let", Cmd::Let),
    ("lstinline", Cmd::InlineCode { lang: false }),
    (
        "minisec",
        Cmd::Section {
            level: 5,
            unnumbered: true,
        },
    ),
    ("mint", Cmd::InlineCode { lang: true }),
    ("mintinline", Cmd::InlineCode { lang: true }),
    ("nameCref", Cmd::Ref(RefForm::Single)),
    ("nameCrefs", Cmd::Ref(RefForm::List)),
    ("namecref", Cmd::Ref(RefForm::Single)),
    ("namecrefs", Cmd::Ref(RefForm::List)),
    ("nameref", Cmd::Ref(RefForm::Single)),
    ("newcommand", Cmd::NewCommand),
    ("newenvironment", Cmd::NewEnvironment),
    ("newif", Cmd::NewIf),
    ("newrobustcmd", Cmd::NewCommand),
    ("newtheorem", Cmd::NewTheorem),
    ("nocite", Cmd::Cite(CiteForm::Single)),
    ("nolinkurl", Cmd::Url),
    ("notecite", Cmd::Cite(CiteForm::Single)),
    ("pageref", Cmd::Ref(RefForm::Single)),
    (
        "paragraph",
        Cmd::Section {
            level: 5,
            unnumbered: false,
        },
    ),
    ("parencite", Cmd::Cite(CiteForm::Single)),
    ("parencites", Cmd::Cite(CiteForm::Multi)),
    (
        "part",
        Cmd::Section {
            level: 0,
            unnumbered: false,
        },
    ),
    ("pnotecite", Cmd::Cite(CiteForm::Single)),
    ("providecommand", Cmd::NewCommand),
    ("providerobustcmd", Cmd::NewCommand),
    ("pvolcite", Cmd::Cite(CiteForm::Single)),
    ("ref", Cmd::Ref(RefForm::Single)),
    ("renewcommand", Cmd::NewCommand),
    ("renewenvironment", Cmd::NewEnvironment),
    ("renewrobustcmd", Cmd::NewCommand),
    (
        "section",
        Cmd::Section {
            level: 2,
            unnumbered: false,
        },
    ),
    ("smartcite", Cmd::Cite(CiteForm::Single)),
    ("smartcites", Cmd::Cite(CiteForm::Multi)),
    ("subfile", Cmd::FileArg(IncludeKind::Subfile)),
    ("subfileinclude", Cmd::FileArg(IncludeKind::Subfile)),
    ("subimport", Cmd::Import),
    ("subincludefrom", Cmd::Import),
    ("subinputfrom", Cmd::Import),
    (
        "subparagraph",
        Cmd::Section {
            level: 6,
            unnumbered: false,
        },
    ),
    ("subref", Cmd::Ref(RefForm::Single)),
    (
        "subsection",
        Cmd::Section {
            level: 3,
            unnumbered: false,
        },
    ),
    (
        "subsubsection",
        Cmd::Section {
            level: 4,
            unnumbered: false,
        },
    ),
    ("supercite", Cmd::Cite(CiteForm::Single)),
    ("supercites", Cmd::Cite(CiteForm::Multi)),
    ("textcite", Cmd::Cite(CiteForm::Single)),
    ("textcites", Cmd::Cite(CiteForm::Multi)),
    ("tvolcite", Cmd::Cite(CiteForm::Single)),
    ("url", Cmd::Url),
    ("usepackage", Cmd::Package),
    ("verb", Cmd::InlineVerb),
    ("volcite", Cmd::Cite(CiteForm::Single)),
    ("vpageref", Cmd::Ref(RefForm::Single)),
    ("vref", Cmd::Ref(RefForm::Single)),
    ("xdef", Cmd::Def),
    ("zcpageref", Cmd::Ref(RefForm::List)),
    ("zcref", Cmd::Ref(RefForm::List)),
    ("zref", Cmd::Ref(RefForm::Single)),
];

/// The structure-relevant meaning of control word `name` (without the backslash).
pub(super) fn lookup(name: &str) -> Option<Cmd> {
    COMMANDS
        .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| COMMANDS[i].1)
}

/// Environment names treated as theorem-like even without a `\newtheorem` in the same file
/// (theorems are usually declared in a preamble file). Sorted for binary search.
const DEFAULT_THEOREMS: &[&str] = &[
    "assumption",
    "axiom",
    "claim",
    "conj",
    "conjecture",
    "construction",
    "convention",
    "cor",
    "corollary",
    "criterion",
    "defi",
    "definition",
    "defn",
    "dfn",
    "example",
    "exercise",
    "fact",
    "hypothesis",
    "lem",
    "lemma",
    "notation",
    "observation",
    "problem",
    "prop",
    "proposition",
    "question",
    "rem",
    "remark",
    "rmk",
    "situation",
    "theorem",
    "thm",
];

/// Whether `name` (without a trailing `*`) is a common theorem-like environment.
pub(super) fn is_default_theorem(name: &str) -> bool {
    let base = name.strip_suffix('*').unwrap_or(name);
    DEFAULT_THEOREMS.binary_search(&base).is_ok()
}

/// Environments whose body TeX reads verbatim; the pass skips to the matching `\end`.
pub(super) fn is_verbatim_env(name: &str) -> bool {
    matches!(
        name,
        "verbatim"
            | "verbatim*"
            | "Verbatim"
            | "Verbatim*"
            | "BVerbatim"
            | "LVerbatim"
            | "VerbatimOut"
            | "spverbatim"
            | "lstlisting"
            | "minted"
            | "comment"
            | "filecontents"
            | "filecontents*"
            | "luacode"
            | "luacode*"
    )
}

/// TeX conditionals that end with `\fi` (for skipping `\iffalse … \fi`), besides `\newif` ones.
pub(super) fn is_primitive_conditional(name: &str) -> bool {
    matches!(
        name,
        "if" | "ifcat"
            | "ifnum"
            | "ifdim"
            | "ifodd"
            | "ifvmode"
            | "ifhmode"
            | "ifmmode"
            | "ifinner"
            | "ifvoid"
            | "ifhbox"
            | "ifvbox"
            | "ifx"
            | "ifeof"
            | "iftrue"
            | "iffalse"
            | "ifcase"
            | "ifdefined"
            | "ifcsname"
            | "iffontchar"
            | "ifincsname"
            | "ifpdfprimitive"
            | "ifabsnum"
            | "ifabsdim"
            | "ifprimitive"
            | "ifpdf"
            | "ifpdftex"
            | "ifluatex"
            | "ifxetex"
            | "ifPDFTeX"
            | "ifLuaTeX"
            | "ifXeTeX"
            | "if@twoside"
            | "if@twocolumn"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_and_unique() {
        for w in COMMANDS.windows(2) {
            assert!(
                w[0].0.as_bytes() < w[1].0.as_bytes(),
                "{} >= {}",
                w[0].0,
                w[1].0
            );
        }
        for w in DEFAULT_THEOREMS.windows(2) {
            assert!(w[0] < w[1], "{} >= {}", w[0], w[1]);
        }
    }

    #[test]
    fn lookup_finds_every_entry() {
        for (name, cmd) in COMMANDS {
            assert_eq!(lookup(name), Some(*cmd));
        }
        assert_eq!(lookup("sectionx"), None);
        assert_eq!(lookup(""), None);
        assert!(is_default_theorem("theorem*"));
        assert!(!is_default_theorem("proof"));
    }
}
