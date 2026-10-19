// Owner: WP1.7 scanner
//! The syntactic context at the cursor, used by snippets, completion and typing helpers.
//!
//! This module also holds the argument signatures of the commands and environments the scanner
//! tracks (references, citations, labels, files, packages, keys), which decide both the
//! [`Context::Arg`] reported at the cursor and the argument token kinds.

use crate::lex::{Frame, Stop, Tables};
use crate::tokens::TokenKind;

/// What a command argument holds, which decides the completion source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// Label names (`\ref`, `\eqref`, `\cref`, …).
    Ref,
    /// Citation keys (`\cite`, natbib and biblatex forms).
    Cite,
    /// A label being defined (`\label`).
    Label,
    /// File names (`\input`, `\include`, `\includegraphics`, `\bibliography`, …).
    File,
    /// Environment names (`\begin`, `\end`, `\newenvironment`, `\newtheorem`).
    Env,
    /// Package or class names (`\usepackage`, `\documentclass`, `\usetikzlibrary`, …).
    Package,
    /// Key-value options or identifiers (`\hypersetup`, `\usepackage[…]`, counters, colours, …).
    Key,
    /// Any other argument. The scanner itself reports arguments it does not classify as the
    /// surrounding mode (`Text`, `Math`, `TextInMath`), never as `Other`.
    Other,
}

impl ArgKind {
    /// Whether the argument is a comma-separated list, so `Context::Arg::start` restarts after
    /// each `,`.
    pub(crate) fn is_list(self) -> bool {
        matches!(
            self,
            ArgKind::Ref | ArgKind::Cite | ArgKind::File | ArgKind::Package | ArgKind::Key
        )
    }

    /// The token kind of the argument's content.
    pub(crate) fn token_kind(self) -> TokenKind {
        match self {
            ArgKind::Ref => TokenKind::RefArg,
            ArgKind::Cite => TokenKind::CiteArg,
            ArgKind::Label => TokenKind::LabelArg,
            ArgKind::File => TokenKind::FileArg,
            ArgKind::Env => TokenKind::EnvName,
            ArgKind::Package => TokenKind::PackageArg,
            ArgKind::Key => TokenKind::KeyArg,
            ArgKind::Other => TokenKind::Text,
        }
    }
}

/// Where the cursor is, following VimTeX's math rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Context {
    Text,
    /// Inline (`display == false`) or display math; `env` names the math environment, if any
    /// (with its `*`, e.g. `align*`).
    Math {
        display: bool,
        env: Option<Box<str>>,
    },
    /// Inside \text{..} etc. within math: behaves as Text for snippets/spelling.
    TextInMath,
    Comment,
    Verbatim,
    /// Cursor is inside a control-sequence name; `start` = byte of the backslash in the line.
    ///
    /// This covers the cursor right after a lone `\` and anywhere after the backslash of a control
    /// word up to the end of its letters. For the mode around the command (e.g. to filter
    /// math-only commands), ask `context_at` for the byte `start`.
    CommandName {
        start: u32,
    },
    /// Cursor is inside argument `idx` (0-based, mandatory args only) of `cmd` (without backslash);
    /// `start` = byte after the opening brace or after the last ',' for list arguments.
    ///
    /// Blanks after that delimiter are skipped, so `line[start..cursor]` is the item typed so
    /// far. When the argument began on an earlier line and this line has no `,` before the cursor,
    /// `start` is the line's first non-blank byte. The arguments of `\begin{env}` after the name
    /// are reported with `cmd == "begin"` and `idx >= 1`. Key-value option brackets (such as
    /// `\usepackage[…]`) are reported with `kind == ArgKind::Key` and `idx` = the number of
    /// mandatory arguments before them.
    Arg {
        cmd: Box<str>,
        idx: u8,
        kind: ArgKind,
        start: u32,
    },
}

impl Context {
    /// Whether this is a math context (where `context = "math"` snippets may fire).
    pub fn is_math(&self) -> bool {
        matches!(self, Context::Math { .. })
    }
}

/// Builds the context where a scan of `line[..byte]` stopped.
///
/// `frames` is the state at `byte` and `arg_start` the lexer's start of the current argument item.
pub(crate) fn resolve(
    t: &Tables,
    frames: &[Frame],
    stop: Stop,
    arg_start: usize,
    line: &[u8],
    byte: usize,
) -> Context {
    match stop {
        Stop::Comment => Context::Comment,
        Stop::CommandName(start) => Context::CommandName {
            start: start as u32,
        },
        Stop::EnvName { end, start } => Context::Arg {
            cmd: if end { "end" } else { "begin" }.into(),
            idx: 0,
            kind: ArgKind::Env,
            start: skip_blanks(line, start, byte) as u32,
        },
        Stop::Verbatim | Stop::Content => mode_context(t, frames, stop, arg_start, line, byte),
    }
}

/// The context decided by the innermost frame that is not transparent.
fn mode_context(
    t: &Tables,
    frames: &[Frame],
    stop: Stop,
    arg_start: usize,
    line: &[u8],
    byte: usize,
) -> Context {
    let verbatim = |env: u16| {
        if t.is_comment_env(env) {
            Context::Comment
        } else {
            Context::Verbatim
        }
    };
    if matches!(stop, Stop::Verbatim) {
        // Inline verbatim, or the body of a verbatim environment.
        return match frames.last() {
            Some(&Frame::Verbatim { env, .. }) => verbatim(env),
            _ => Context::Verbatim,
        };
    }
    for frame in frames.iter().rev() {
        return match *frame {
            Frame::Arg {
                spec, idx, delim, ..
            } => {
                let spec = t.spec(spec);
                let Some(kind) = spec.kind(idx, delim) else {
                    continue;
                };
                Context::Arg {
                    cmd: spec.name.into(),
                    idx,
                    kind,
                    start: skip_blanks(line, arg_start.min(byte), byte) as u32,
                }
            }
            Frame::TextInMath { .. } => Context::TextInMath,
            Frame::Dollar | Frame::Paren | Frame::Ensure { .. } => Context::Math {
                display: false,
                env: None,
            },
            Frame::DoubleDollar | Frame::Bracket => Context::Math {
                display: true,
                env: None,
            },
            Frame::MathEnv { env, star } => Context::Math {
                display: !t.is_inline_env(env),
                env: Some(t.env_name(env, star)),
            },
            Frame::Verbatim { env, .. } => verbatim(env),
        };
    }
    Context::Text
}

/// The first byte in `from..to` that is not a space or tab (or `to`).
fn skip_blanks(line: &[u8], from: usize, to: usize) -> usize {
    let to = to.min(line.len());
    let from = from.min(to);
    line[from..to]
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .map_or(to, |i| from + i)
}

/// How the scanner treats a mandatory argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgClass {
    /// Scanned as an argument of this kind (its own token kind and context).
    Tracked(ArgKind),
    /// Ordinary text or math in the surrounding mode.
    Plain,
}

/// What an optional `[…]` argument holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OptClass {
    /// Key-value options: `KeyArg` tokens and an `ArgKind::Key` context.
    Key,
    /// A label name (`\hyperref[label]{text}`): `RefArg` tokens and an `ArgKind::Ref` context.
    Ref,
    /// Ordinary text (a theorem title, a citation note): the surrounding mode.
    Plain,
}

/// The argument signature of a command or environment.
#[derive(Debug)]
pub(crate) struct Shape {
    /// Optional arguments accepted before each mandatory one, if any.
    pub(crate) opt: Option<OptClass>,
    /// The mandatory arguments, in order.
    pub(crate) args: &'static [ArgClass],
    /// Further `{…}` arguments repeat the last class (biblatex multicite commands).
    pub(crate) repeat: bool,
    /// `(…)` arguments are accepted like optional ones (biblatex multicite global notes).
    pub(crate) parens: bool,
    /// In math the arguments are math (a tikz-cd `\arrow["\eta"]` inside `\[ \]`).
    pub(crate) text_only: bool,
}

impl Shape {
    const fn new(opt: Option<OptClass>, args: &'static [ArgClass]) -> Self {
        Self {
            opt,
            args,
            repeat: false,
            parens: false,
            text_only: false,
        }
    }

    /// [`Shape::new`], outside math only.
    const fn in_text(opt: Option<OptClass>, args: &'static [ArgClass]) -> Self {
        Self {
            text_only: true,
            ..Self::new(opt, args)
        }
    }

    const fn multicite() -> Self {
        Self {
            opt: Some(OptClass::Plain),
            args: &[CITE],
            repeat: true,
            parens: true,
            text_only: false,
        }
    }
}

const REF: ArgClass = ArgClass::Tracked(ArgKind::Ref);
const CITE: ArgClass = ArgClass::Tracked(ArgKind::Cite);
const LABEL: ArgClass = ArgClass::Tracked(ArgKind::Label);
const FILE: ArgClass = ArgClass::Tracked(ArgKind::File);
const ENV: ArgClass = ArgClass::Tracked(ArgKind::Env);
const PACKAGE: ArgClass = ArgClass::Tracked(ArgKind::Package);
const KEY: ArgClass = ArgClass::Tracked(ArgKind::Key);
const PLAIN: ArgClass = ArgClass::Plain;
const KEY_OPT: Option<OptClass> = Some(OptClass::Key);
const REF_OPT: Option<OptClass> = Some(OptClass::Ref);
const PLAIN_OPT: Option<OptClass> = Some(OptClass::Plain);

/// Commands whose arguments are tracked: whitespace-separated names (without backslash) and
/// their shape. A `*` right after any of these names belongs to the command.
pub(crate) static COMMANDS: &[(&str, Shape)] = &[
    // References.
    (
        "ref pageref eqref autoref Autoref nameref Nameref vref Vref fref Fref cref Cref \
         cpageref Cpageref labelcref labelcpageref namecref nameCref lcnamecref zref zpageref \
         subref",
        Shape::new(None, &[REF]),
    ),
    ("vpageref Vpageref", Shape::new(PLAIN_OPT, &[REF])),
    ("zcref zcpageref", Shape::new(KEY_OPT, &[REF])),
    // `\hyperref[label]{text}`.
    ("hyperref", Shape::new(REF_OPT, &[PLAIN])),
    (
        "crefrange Crefrange cpagerefrange Cpagerefrange vrefrange",
        Shape::new(None, &[REF, REF]),
    ),
    // Citations: base LaTeX, natbib and biblatex.
    (
        "cite Cite nocite citep Citep citet Citet citealp Citealp citealt Citealt citeauthor \
         Citeauthor citeyear citeyearpar citetitle Citetitle citedate citeurl citenum \
         parencite Parencite textcite Textcite autocite Autocite footcite Footcite \
         footcitetext smartcite Smartcite supercite fullcite footfullcite notecite Notecite \
         pnotecite Pnotecite fnotecite bibitem",
        Shape::new(PLAIN_OPT, &[CITE]),
    ),
    (
        "cites Cites parencites Parencites footcites footcitetexts smartcites Smartcites \
         textcites Textcites supercites autocites Autocites",
        Shape::multicite(),
    ),
    ("label zlabel", Shape::new(None, &[LABEL])),
    // Files.
    (
        "input include subfile subfileinclude verbatiminput includeonly bibliography \
         addglobalbib",
        Shape::new(None, &[FILE]),
    ),
    (
        "includegraphics includepdf includestandalone includesvg lstinputlisting \
         addbibresource externaldocument",
        Shape::new(KEY_OPT, &[FILE]),
    ),
    (
        "import subimport inputfrom includefrom subinputfrom subincludefrom",
        Shape::new(None, &[FILE, FILE]),
    ),
    ("inputminted", Shape::new(KEY_OPT, &[KEY, FILE])),
    // Packages, classes and libraries.
    (
        "usepackage RequirePackage documentclass LoadClass usetikzlibrary usepgfplotslibrary \
         tcbuselibrary",
        Shape::new(KEY_OPT, &[PACKAGE]),
    ),
    (
        "PassOptionsToPackage PassOptionsToClass",
        Shape::new(None, &[KEY, PACKAGE]),
    ),
    // Environments being defined; a theorem's `[counter]` it shares (`\newtheorem{lem}[thm]{…}`).
    (
        "newenvironment renewenvironment NewDocumentEnvironment RenewDocumentEnvironment",
        Shape::new(None, &[ENV]),
    ),
    (
        "newtheorem renewtheorem",
        Shape::new(KEY_OPT, &[ENV, PLAIN]),
    ),
    ("declaretheorem", Shape::new(KEY_OPT, &[ENV])),
    // Key-value settings and identifiers: counters, styles, colours.
    (
        "hypersetup geometry newgeometry tikzset pgfplotsset sisetup lstset KOMAoptions \
         ExecuteBibliographyOptions pagestyle thispagestyle pagenumbering theoremstyle \
         bibliographystyle newcounter stepcounter refstepcounter value arabic roman Roman \
         alph Alph fnsymbol color pagecolor usetheme usecolortheme usefonttheme \
         useinnertheme useoutertheme",
        Shape::new(None, &[KEY]),
    ),
    (
        "captionsetup setlist setminted",
        Shape::new(KEY_OPT, &[KEY]),
    ),
    (
        "setcounter addtocounter textcolor colorbox",
        Shape::new(None, &[KEY, PLAIN]),
    ),
    ("numberwithin", Shape::new(None, &[KEY, KEY])),
    ("definecolor", Shape::new(None, &[KEY, KEY, KEY])),
    // cleveref's names of a counter (the names themselves are text).
    ("crefname Crefname", Shape::new(None, &[KEY])),
    ("crefalias", Shape::new(None, &[KEY, KEY])),
    // An operator's name is an abbreviation (`coker`, `Hom`), not a word.
    ("DeclareMathOperator", Shape::new(None, &[PLAIN, KEY])),
    // TikZ and tikz-cd options outside math: `\arrow[rr, "f"]`, `\draw[thick, ->]` (a diagram
    // inside `\[ \]` is math, labels included).
    (
        "arrow ar draw fill filldraw node coordinate",
        Shape::in_text(KEY_OPT, &[]),
    ),
];

/// Arguments after `\begin{name}` for known environments (math and verbatim environments are
/// configured in `ScanConfig` instead).
pub(crate) static ENVIRONMENTS: &[(&str, Shape)] = &[
    (
        "figure figure* table table* tikzpicture tikzcd enumerate enumerate* itemize itemize* \
         description description* tcolorbox adjustbox",
        Shape::new(KEY_OPT, &[]),
    ),
    ("tabular array longtable", Shape::new(KEY_OPT, &[KEY])),
    ("tabular* tabularx", Shape::new(KEY_OPT, &[PLAIN, KEY])),
];

/// Every other environment: an optional title such as `\begin{theorem}[Main result]`.
pub(crate) static DEFAULT_ENVIRONMENT: Shape = Shape::new(PLAIN_OPT, &[]);

/// Sectioning commands (`TokenKind::Sectioning`), including KOMA-Script's.
pub(crate) const SECTIONING: &str = "part chapter section subsection subsubsection paragraph \
    subparagraph addpart addchap addsec minisec";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (names, _) in COMMANDS {
            for name in names.split_whitespace() {
                assert!(seen.insert(name), "duplicate command {name}");
            }
        }
        for name in SECTIONING.split_whitespace() {
            assert!(seen.insert(name), "duplicate command {name}");
        }
    }

    #[test]
    fn counters_labels_operator_names_and_tikz_options_are_not_text() {
        use crate::{LineStates, ScanConfig, StrLines};
        use lemma_core::LineIndex;
        let text = "\\newtheorem{lem}[thm]{Lemma}[section]\n\
\\hyperref[sec:a]{the text}\n\
\\crefname{lem}{lemma}{lemmas}\n\
\\DeclareMathOperator*{\\coker}{coker}\n\
A \\arrow[rr, \"f\"] & B \\ar[d] \\draw[thick] (0,0); \\node[above] {top};\n";
        let index = LineIndex::new(text);
        let mut states = LineStates::new(std::sync::Arc::new(ScanConfig::default()));
        states.rebuild(&StrLines {
            text,
            index: &index,
        });
        // The kind of the token at `word` (the first one after `after`).
        let kind = |after: &str, word: &str| {
            let at = text.find(after).unwrap() + after.len();
            let at = at + text[at..].find(word).unwrap();
            let (line, col) = index.position(at);
            let mut tokens = Vec::new();
            states.tokens(line, &text[index.line_range(line)], &mut tokens);
            tokens
                .iter()
                .find(|t| t.start <= col && col < t.end)
                .unwrap()
                .kind
        };
        for (after, word, expected) in [
            ("newtheorem", "thm", TokenKind::KeyArg),
            ("newtheorem", "Lemma", TokenKind::Text),
            ("hyperref", "sec:a", TokenKind::RefArg),
            ("hyperref", "the text", TokenKind::Text),
            ("crefname", "lem", TokenKind::KeyArg),
            ("crefname{lem}", "lemma", TokenKind::Text),
            ("DeclareMathOperator*{\\coker}", "coker", TokenKind::KeyArg),
            ("arrow", "rr", TokenKind::KeyArg),
            ("\\ar", "d", TokenKind::KeyArg),
            ("draw", "thick", TokenKind::KeyArg),
            ("node", "above", TokenKind::KeyArg),
            ("above]", "top", TokenKind::Text),
        ] {
            assert_eq!(kind(after, word), expected, "{word} after {after}");
        }
        // Inside math a diagram's options stay math.
        let display = "\\[ A \\arrow[r, \"\\eta\"] & B \\]";
        let index = LineIndex::new(display);
        let mut math = LineStates::new(std::sync::Arc::new(ScanConfig::default()));
        math.rebuild(&StrLines {
            text: display,
            index: &index,
        });
        let mut tokens = Vec::new();
        math.tokens(0, display, &mut tokens);
        assert!(
            tokens
                .iter()
                .all(|t| !matches!(t.kind, TokenKind::KeyArg | TokenKind::Text)),
            "{tokens:?}"
        );
        // `\hyperref[` completes labels.
        let line = "\\hyperref[sec:a]{the text}";
        assert_eq!(
            states.context_at(1, line, 13),
            Context::Arg {
                cmd: "hyperref".into(),
                idx: 0,
                kind: ArgKind::Ref,
                start: 10,
            }
        );
    }

    #[test]
    fn skip_blanks_stays_in_range() {
        assert_eq!(skip_blanks(b"a,  b", 2, 5), 4);
        assert_eq!(skip_blanks(b"a,  ", 2, 4), 4);
        assert_eq!(skip_blanks(b"ab", 5, 9), 2);
    }

    #[test]
    fn is_math_only_for_math() {
        assert!(
            Context::Math {
                display: false,
                env: None
            }
            .is_math()
        );
        assert!(!Context::TextInMath.is_math());
        assert!(!Context::CommandName { start: 0 }.is_math());
    }
}
