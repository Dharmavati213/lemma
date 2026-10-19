// Owner: WP1.7 scanner
//! Line access, scanner configuration and the single-line lexer behind [`LineStates`].
//!
//! The scanner state at a line start is a small stack of [`Frame`]s: open math, `\text{…}`
//! inside math, a tracked command argument, or a verbatim body. [`scan`] walks one line from
//! that stack and leaves the stack in the state for the next line. On the way it can emit
//! [`Token`](crate::Token)s, and it reports what was open where it stopped, which
//! [`LineStates::context_at`] turns into a [`Context`](crate::Context).
//!
//! Rules (VimTeX's, see `docs/ARCHITECTURE.md` §5):
//! - math: `$…$`, `$$…$$`, `\(…\)`, `\[…\]`, the configured math environments (and their starred
//!   forms) and `\ensuremath{…}`; `\text`-like commands switch back to text inside math, where `$`
//!   enters math again; `tikzcd` is not a math environment;
//! - `%` starts a comment; `\$` and `\%` are literal;
//! - verbatim environments, `\verb|…|`, `\lstinline`, `\mintinline` and `\url`/`\href` targets
//!   are not scanned;
//! - a blank line ends inline and delimited display math (`$`, `$$`, `\(`, `\[`), as TeX's
//!   `\par` does, and any unclosed command argument; math environments stay open;
//! - arguments of known commands (references, citations, labels, files, packages, key-value
//!   options) are tracked across lines; `\begin{name}` must be complete on one line.
//!
//! [`LineStates`]: crate::LineStates
//! [`LineStates::context_at`]: crate::LineStates::context_at

use crate::context::{
    ArgClass, ArgKind, COMMANDS, DEFAULT_ENVIRONMENT, ENVIRONMENTS, OptClass, SECTIONING, Shape,
};
use crate::tokens::{Sink, TokenKind};
use lemma_core::LineIndex;
use memchr::memmem;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Random access to the lines of a document (a GTK buffer or a plain string).
pub trait Lines {
    /// Number of lines; at least 1, as in [`LineIndex::line_count`].
    fn line_count(&self) -> u32;
    /// Line `i` without its trailing '\n'.
    fn line(&self, i: u32) -> Cow<'_, str>;
}

/// [`Lines`] over a string and its line index.
#[derive(Clone, Copy, Debug)]
pub struct StrLines<'a> {
    pub text: &'a str,
    /// Must be the index of `text`.
    pub index: &'a LineIndex,
}

impl Lines for StrLines<'_> {
    fn line_count(&self) -> u32 {
        self.index.line_count()
    }

    /// # Panics
    /// If `i >= self.line_count()`, or if `index` is not the index of `text`.
    fn line(&self, i: u32) -> Cow<'_, str> {
        Cow::Borrowed(&self.text[self.index.line_range(i)])
    }
}

/// Math/verbatim rules. `Default` = VimTeX rules (tikzcd is NOT math).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanConfig {
    /// Math environments; the starred variants are implied. Default: equation align alignat flalign
    /// gather multline xalignat xxalignat eqnarray math displaymath dmath. Append the user's list.
    pub math_envs: Vec<String>,
    /// Environments whose body is not scanned; the starred variants are implied. Default: verbatim
    /// Verbatim lstlisting minted comment (the body of `comment` counts as a comment).
    pub verbatim_envs: Vec<String>,
    /// Commands (without backslash) whose arguments are text inside math. Default: text textnormal
    /// textrm textup texttt textsf textsc textbf textit mbox fbox intertext shortintertext tag.
    pub text_in_math_cmds: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        fn names(list: &str) -> Vec<String> {
            list.split_whitespace().map(str::to_owned).collect()
        }
        Self {
            math_envs: names(
                "equation align alignat flalign gather multline xalignat xxalignat eqnarray math \
                 displaymath dmath",
            ),
            verbatim_envs: names("verbatim Verbatim lstlisting minted comment"),
            // `\tag` follows VimTeX, which treats the tag's argument as non-math.
            text_in_math_cmds: names(
                "text textnormal textrm textup texttt textsf textsc textbf textit mbox fbox \
                 intertext shortintertext tag",
            ),
        }
    }
}

/// One level of the scanner state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Frame {
    /// `$…$`.
    Dollar,
    /// `$$…$$`.
    DoubleDollar,
    /// `\(…\)`.
    Paren,
    /// `\[…\]`.
    Bracket,
    /// A math environment; `env` indexes the configured environments in [`Tables`].
    MathEnv { env: u16, star: bool },
    /// `\ensuremath{…}` in text; `depth` counts the braces open inside it.
    Ensure { depth: u8 },
    /// The argument of a `\text`-like command inside math.
    TextInMath { depth: u8 },
    /// An argument of a command with a known signature (`spec` indexes [`Tables::specs`]). For a
    /// brace argument `idx` is its index; for an optional one, the index of the next mandatory one.
    Arg {
        spec: u16,
        idx: u8,
        delim: Delim,
        depth: u8,
    },
    /// The body of a verbatim environment, which ends at `\end{name}`.
    Verbatim { env: u16, star: bool },
}

impl Frame {
    /// The brace depth of frames that close at a `}` (or `]`) of their own.
    fn depth(&self) -> Option<u8> {
        match *self {
            Frame::Ensure { depth } | Frame::TextInMath { depth } | Frame::Arg { depth, .. } => {
                Some(depth)
            }
            _ => None,
        }
    }

    fn depth_mut(&mut self) -> Option<&mut u8> {
        match self {
            Frame::Ensure { depth } | Frame::TextInMath { depth } | Frame::Arg { depth, .. } => {
                Some(depth)
            }
            _ => None,
        }
    }

    /// Whether a blank line (TeX's `\par`) ends this frame.
    fn ends_at_paragraph(&self) -> bool {
        !matches!(self, Frame::MathEnv { .. } | Frame::Verbatim { .. })
    }
}

/// The delimiters of a command argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Delim {
    Brace,
    Bracket,
    Paren,
}

/// How the lexer reads content at the current position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Text,
    Math,
    Arg(ArgKind),
    /// The body of a verbatim environment (the frame that decides the mode).
    Verbatim {
        env: u16,
        star: bool,
    },
}

/// Why a scan stopped; see [`scan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The scan reached its end in ordinary content (the frames decide the context).
    Content,
    /// Inside a `%` comment.
    Comment,
    /// Inside the name of the control sequence whose backslash is at this byte.
    CommandName(usize),
    /// Inside inline verbatim or a verbatim body.
    Verbatim,
    /// Inside the unclosed environment name of `\begin{` (`end == false`) or `\end{`, which
    /// starts at byte `start`.
    EnvName { end: bool, start: usize },
}

/// The result of [`scan`].
pub(crate) struct Scanned {
    pub(crate) stop: Stop,
    /// Byte after the opening delimiter or last list `,` of the innermost tracked argument on
    /// this line (0 when the argument began on an earlier line).
    pub(crate) arg_start: usize,
}

/// A command's argument signature, as named in contexts.
#[derive(Debug)]
pub(crate) struct Spec {
    /// The command reported in `Context::Arg::cmd`.
    pub(crate) name: &'static str,
    shape: &'static Shape,
    /// Index of the first argument `shape` describes: 1 for `\begin{env}`, whose argument 0 is
    /// the environment name.
    first: u8,
}

impl Spec {
    /// The class of mandatory argument `idx`, or `None` past the last one.
    fn arg(&self, idx: u8) -> Option<ArgClass> {
        let i = usize::from(idx.checked_sub(self.first)?);
        match self.shape.args.get(i) {
            Some(&class) => Some(class),
            None if self.shape.repeat => self.shape.args.last().copied(),
            None => None,
        }
    }

    /// Whether a tracked argument comes after `idx`, so an untracked argument at `idx` still needs
    /// a frame to find where it ends.
    fn tracks_after(&self, idx: u8) -> bool {
        let i = usize::from(idx.saturating_sub(self.first));
        self.shape.repeat
            || self
                .shape
                .args
                .iter()
                .skip(i + 1)
                .any(|c| matches!(c, ArgClass::Tracked(_)))
    }

    /// The kind of an argument frame, or `None` for a transparent one (whose content is read in
    /// the surrounding mode).
    pub(crate) fn kind(&self, idx: u8, delim: Delim) -> Option<ArgKind> {
        match delim {
            Delim::Brace => match self.arg(idx) {
                Some(ArgClass::Tracked(kind)) => Some(kind),
                _ => None,
            },
            Delim::Bracket | Delim::Paren => match self.shape.opt {
                Some(OptClass::Key) => Some(ArgKind::Key),
                Some(OptClass::Ref) => Some(ArgKind::Ref),
                _ => None,
            },
        }
    }
}

/// What the lexer does after a control word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Action {
    #[default]
    None,
    Begin,
    End,
    EnsureMath,
    InlineVerbatim(VerbStyle),
}

/// The argument syntax of an inline verbatim command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerbStyle {
    /// `\verb<c>…<c>`.
    Verb,
    /// `\lstinline[opts]<c>…<c>` or `\lstinline[opts]{…}`.
    Listing,
    /// `\mintinline[opts]{lang}<c>…<c>` or `{…}`.
    Minted,
    /// `\url{…}` or `\url<c>…<c>`.
    Url,
    /// `\href{url}`, followed by an ordinary text argument.
    Href,
}

/// Everything the lexer knows about a control word.
#[derive(Clone, Copy, Debug, Default)]
struct CmdInfo {
    /// Index into [`Tables::specs`].
    spec: Option<u16>,
    action: Action,
    sectioning: bool,
    text_in_math: bool,
    /// A `*` right after the name belongs to the command.
    star: bool,
}

/// What `\begin{name}` starts.
#[derive(Clone, Copy, Debug)]
enum EnvKind {
    Math(u16),
    Verbatim(u16),
    Spec(u16),
}

/// A math or verbatim environment from the configuration.
#[derive(Debug)]
struct EnvEntry {
    name: Box<str>,
    /// `\end{name}` and `\end{name*}`, the end markers of a verbatim body.
    end_markers: [memmem::Finder<'static>; 2],
    /// `math`, which is inline.
    inline: bool,
    /// `comment`, whose body is a comment.
    comment: bool,
}

/// Lookup tables built once from a [`ScanConfig`].
#[derive(Debug)]
pub(crate) struct Tables {
    cmds: HashMap<Box<[u8]>, CmdInfo, FxBuild>,
    /// Quick rejection of control words that are not in `cmds`, keyed by length and end letters.
    cmd_filter: Bloom,
    envs: HashMap<Box<[u8]>, EnvKind, FxBuild>,
    env_entries: Vec<EnvEntry>,
    specs: Vec<Spec>,
    default_env_spec: u16,
}

impl Tables {
    pub(crate) fn new(cfg: &ScanConfig) -> Self {
        let mut t = Tables {
            cmds: HashMap::default(),
            cmd_filter: Bloom::default(),
            envs: HashMap::default(),
            env_entries: Vec::new(),
            specs: Vec::new(),
            default_env_spec: 0,
        };
        for (names, shape) in COMMANDS {
            for name in names.split_whitespace() {
                let id = t.add_spec(name, shape, 0);
                let info = t.cmd_entry(name);
                info.spec = Some(id);
                info.star = true;
            }
        }
        for name in SECTIONING.split_whitespace() {
            let info = t.cmd_entry(name);
            info.sectioning = true;
            info.star = true;
        }
        let actions = [
            ("begin", Action::Begin),
            ("end", Action::End),
            ("ensuremath", Action::EnsureMath),
            ("verb", Action::InlineVerbatim(VerbStyle::Verb)),
            ("Verb lstinline", Action::InlineVerbatim(VerbStyle::Listing)),
            ("mintinline mint", Action::InlineVerbatim(VerbStyle::Minted)),
            ("url nolinkurl", Action::InlineVerbatim(VerbStyle::Url)),
            ("href", Action::InlineVerbatim(VerbStyle::Href)),
        ];
        for (names, action) in actions {
            for name in names.split_whitespace() {
                let info = t.cmd_entry(name);
                info.action = action;
                info.star |= name == "verb";
            }
        }
        for name in &cfg.text_in_math_cmds {
            let info = t.cmd_entry(name);
            info.text_in_math = true;
            info.star = true;
        }

        for (names, shape) in ENVIRONMENTS {
            let id = t.add_spec("begin", shape, 1);
            for name in names.split_whitespace() {
                t.envs.insert(name.as_bytes().into(), EnvKind::Spec(id));
            }
        }
        t.default_env_spec = t.add_spec("begin", &DEFAULT_ENVIRONMENT, 1);
        // Configured environments win over the built-in signatures, and math over verbatim.
        for name in &cfg.verbatim_envs {
            if let Some(id) = t.add_env(name) {
                t.envs.insert(name.as_bytes().into(), EnvKind::Verbatim(id));
            }
        }
        for name in &cfg.math_envs {
            if let Some(id) = t.add_env(name) {
                t.envs.insert(name.as_bytes().into(), EnvKind::Math(id));
            }
        }
        t
    }

    fn add_spec(&mut self, name: &'static str, shape: &'static Shape, first: u8) -> u16 {
        let id = u16::try_from(self.specs.len()).expect("fewer than 65536 built-in signatures");
        self.specs.push(Spec { name, shape, first });
        id
    }

    fn cmd_entry(&mut self, name: &str) -> &mut CmdInfo {
        self.cmd_filter.insert(name.as_bytes());
        self.cmds.entry(name.as_bytes().into()).or_default()
    }

    /// Registers a configured environment; `None` once 65536 are registered.
    fn add_env(&mut self, name: &str) -> Option<u16> {
        let id = u16::try_from(self.env_entries.len()).ok()?;
        let marker =
            |star: &str| memmem::Finder::new(&format!("\\end{{{name}{star}}}")).into_owned();
        self.env_entries.push(EnvEntry {
            name: name.into(),
            end_markers: [marker(""), marker("*")],
            inline: name == "math",
            comment: name == "comment",
        });
        Some(id)
    }

    fn cmd(&self, name: &[u8]) -> CmdInfo {
        if !self.cmd_filter.may_contain(name) {
            return CmdInfo::default();
        }
        self.cmds.get(name).copied().unwrap_or_default()
    }

    /// Classifies `\begin{name}`; the bool says whether `name` is the starred form of a
    /// configured environment.
    fn env(&self, name: &[u8]) -> (EnvKind, bool) {
        if let Some(&kind) = self.envs.get(name) {
            return (kind, false);
        }
        if let Some(base) = name.strip_suffix(b"*")
            && let Some(&kind @ (EnvKind::Math(_) | EnvKind::Verbatim(_))) = self.envs.get(base)
        {
            return (kind, true);
        }
        (EnvKind::Spec(self.default_env_spec), false)
    }

    pub(crate) fn spec(&self, id: u16) -> &Spec {
        &self.specs[usize::from(id)]
    }

    /// The name of a math environment as written, e.g. `align*`.
    pub(crate) fn env_name(&self, env: u16, star: bool) -> Box<str> {
        let name = &self.env_entries[usize::from(env)].name;
        if star {
            format!("{name}*").into()
        } else {
            name.clone()
        }
    }

    pub(crate) fn is_inline_env(&self, env: u16) -> bool {
        self.env_entries[usize::from(env)].inline
    }

    pub(crate) fn is_comment_env(&self, env: u16) -> bool {
        self.env_entries[usize::from(env)].comment
    }
}

/// A 512-bit filter over (length, first byte, last byte) of command names.
#[derive(Debug, Default)]
struct Bloom([u64; 8]);

impl Bloom {
    fn slot(name: &[u8]) -> usize {
        let (first, last) = match name {
            [] => (0, 0),
            [first, .., last] => (*first, *last),
            [only] => (*only, *only),
        };
        (name.len() * 31 + usize::from(first) * 7 + usize::from(last)) % 512
    }

    fn insert(&mut self, name: &[u8]) {
        let slot = Self::slot(name);
        self.0[slot / 64] |= 1 << (slot % 64);
    }

    fn may_contain(&self, name: &[u8]) -> bool {
        let slot = Self::slot(name);
        self.0[slot / 64] & (1 << (slot % 64)) != 0
    }
}

/// FxHash: a fast, non-cryptographic hasher for the small keys of the scanner's tables.
#[derive(Clone, Copy, Default)]
pub(crate) struct FxHasher(u64);

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// `BuildHasher` for [`FxHasher`].
pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;

/// Deeper nesting is not tracked; it keeps states small on pathological input.
const MAX_FRAMES: usize = 16;

/// Bytes that end a run of ordinary content.
static SPECIAL: [bool; 256] = {
    let mut table = [false; 256];
    let bytes = b"\\%${}]),&~#^_";
    let mut i = 0;
    while i < bytes.len() {
        table[bytes[i] as usize] = true;
        i += 1;
    }
    table
};

fn is_letter(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'@'
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\x0c')
}

/// Length of the UTF-8 sequence that starts with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xff => 4,
        _ => 1,
    }
}

/// Scans `line[..end]` starting from the state `frames`, which is left as the state at `end`.
///
/// With `end == line.len()` the result is the state at the start of the next line. Whether the
/// line is blank is decided on the whole line.
pub(crate) fn scan<S: Sink>(
    t: &Tables,
    frames: &mut Vec<Frame>,
    line: &[u8],
    end: usize,
    sink: S,
) -> Scanned {
    let mut lexer = Lexer {
        t,
        frames,
        line,
        end: end.min(line.len()),
        pos: 0,
        mode: Mode::Text,
        mode_frame: None,
        expect: None,
        arg_start: 0,
        sink,
    };
    let stop = lexer.run();
    Scanned {
        stop,
        arg_start: lexer.arg_start,
    }
}

struct Lexer<'a, S> {
    t: &'a Tables,
    frames: &'a mut Vec<Frame>,
    line: &'a [u8],
    end: usize,
    pos: usize,
    mode: Mode,
    /// Index of the frame that decides `mode`; `None` at the document's text level.
    mode_frame: Option<usize>,
    /// The command (spec) and argument index whose `{`, `[` or `(` may come next.
    expect: Option<(u16, u8)>,
    arg_start: usize,
    sink: S,
}

impl<S: Sink> Lexer<'_, S> {
    fn run(&mut self) -> Stop {
        if self.line.iter().all(|&b| is_blank(b)) {
            while self.frames.last().is_some_and(Frame::ends_at_paragraph) {
                self.frames.pop();
            }
        }
        self.refresh_mode();
        while self.pos < self.end {
            if let Mode::Verbatim { env, star } = self.mode {
                if let Some(stop) = self.verbatim_body(env, star) {
                    return stop;
                }
                continue;
            }
            let b = self.line[self.pos];
            if self.expect.is_some() && self.expected_arg(b) {
                continue;
            }
            let stop = match b {
                b'%' => {
                    self.take_to(self.end, TokenKind::Comment);
                    Some(Stop::Comment)
                }
                b'\\' => self.backslash(),
                b'$' => {
                    self.dollar();
                    None
                }
                b'{' => {
                    self.open_brace();
                    None
                }
                b'}' => {
                    self.close_brace();
                    None
                }
                b']' => {
                    self.close_optional(Delim::Bracket);
                    None
                }
                b')' => {
                    self.close_optional(Delim::Paren);
                    None
                }
                b',' => {
                    self.comma();
                    None
                }
                b'&' | b'~' | b'#' | b'^' | b'_' => {
                    let kind = match self.mode {
                        Mode::Text => TokenKind::Special,
                        Mode::Math if b == b'&' => TokenKind::Special,
                        _ => self.content_kind(),
                    };
                    self.take(1, kind);
                    None
                }
                _ => {
                    self.plain();
                    None
                }
            };
            if let Some(stop) = stop {
                return stop;
            }
        }
        Stop::Content
    }

    fn refresh_mode(&mut self) {
        let t = self.t;
        for (i, frame) in self.frames.iter().enumerate().rev() {
            self.mode = match *frame {
                Frame::Dollar
                | Frame::DoubleDollar
                | Frame::Paren
                | Frame::Bracket
                | Frame::MathEnv { .. }
                | Frame::Ensure { .. } => Mode::Math,
                Frame::TextInMath { .. } => Mode::Text,
                Frame::Verbatim { env, star } => Mode::Verbatim { env, star },
                Frame::Arg {
                    spec, idx, delim, ..
                } => match t.spec(spec).kind(idx, delim) {
                    Some(kind) => Mode::Arg(kind),
                    None => continue,
                },
            };
            self.mode_frame = Some(i);
            return;
        }
        self.mode = Mode::Text;
        self.mode_frame = None;
    }

    fn push(&mut self, frame: Frame) -> bool {
        if self.frames.len() >= MAX_FRAMES {
            return false;
        }
        self.frames.push(frame);
        self.refresh_mode();
        true
    }

    /// Pops frame `i` and everything above it.
    fn truncate(&mut self, i: usize) {
        self.frames.truncate(i);
        self.expect = None;
        self.refresh_mode();
    }

    fn content_kind(&self) -> TokenKind {
        match self.mode {
            Mode::Text => TokenKind::Text,
            Mode::Math => TokenKind::Math,
            Mode::Arg(kind) => kind.token_kind(),
            Mode::Verbatim { env, .. } if self.t.is_comment_env(env) => TokenKind::Comment,
            Mode::Verbatim { .. } => TokenKind::Verbatim,
        }
    }

    /// Classifies `n` bytes at the current position and moves past them.
    fn take(&mut self, n: usize, kind: TokenKind) {
        self.take_to(self.pos + n, kind);
    }

    fn take_to(&mut self, to: usize, kind: TokenKind) {
        let to = to.min(self.end);
        if to > self.pos {
            self.sink.token(self.pos, to, kind);
            self.pos = to;
        }
    }

    fn peek(&self) -> Option<u8> {
        (self.pos < self.end).then(|| self.line[self.pos])
    }

    /// Moves past blanks, classified as content.
    fn skip_blanks(&mut self) {
        let mut p = self.pos;
        while p < self.end && is_blank(self.line[p]) {
            p += 1;
        }
        self.take_to(p, self.content_kind());
    }

    /// Handles `b` while a command may still receive arguments. Returns false (and stops
    /// expecting) when `b` does not start one.
    fn expected_arg(&mut self, b: u8) -> bool {
        let Some((spec_id, idx)) = self.expect else {
            return false;
        };
        let spec = self.t.spec(spec_id);
        let opens = match b {
            b' ' | b'\t' | b'\r' => {
                self.take(1, self.content_kind());
                return true;
            }
            b'{' => match spec.arg(idx) {
                Some(ArgClass::Tracked(_)) => true,
                Some(ArgClass::Plain) => spec.tracks_after(idx),
                None => false,
            },
            // Before a mandatory argument, or right after an environment without any.
            b'[' => spec.shape.opt.is_some() && (spec.arg(idx).is_some() || idx == spec.first),
            b'(' => spec.shape.parens && spec.arg(idx).is_some(),
            _ => false,
        };
        self.expect = None;
        if !opens {
            return false;
        }
        let (delim, kind) = match b {
            b'{' => (Delim::Brace, TokenKind::Brace),
            b'[' => (Delim::Bracket, TokenKind::Bracket),
            _ => (Delim::Paren, TokenKind::Bracket),
        };
        let frame = Frame::Arg {
            spec: spec_id,
            idx,
            delim,
            depth: 0,
        };
        if self.frames.len() >= MAX_FRAMES {
            return false;
        }
        self.take(1, kind);
        self.push(frame);
        self.arg_start = self.pos;
        true
    }

    /// After an argument closes: expect the command's next argument, if it has one.
    fn resume(&mut self, spec: u16, idx: u8, delim: Delim) {
        let next = match delim {
            Delim::Brace => idx.saturating_add(1),
            Delim::Bracket | Delim::Paren => idx,
        };
        self.expect = self
            .t
            .spec(spec)
            .arg(next)
            .is_some()
            .then_some((spec, next));
    }

    /// A run of ordinary content.
    fn plain(&mut self) {
        let mut p = self.pos + 1;
        let quick = !S::WANTS_TOKENS
            && matches!(
                self.frames.last(),
                None | Some(
                    Frame::Dollar
                        | Frame::DoubleDollar
                        | Frame::Paren
                        | Frame::Bracket
                        | Frame::MathEnv { .. }
                )
            );
        if quick {
            // Only `\`, `$` and `%` can change the state here: braces are not counted at the
            // text level or directly inside math, and nothing else is pending.
            p = memchr::memchr3(b'\\', b'$', b'%', &self.line[p..self.end])
                .map_or(self.end, |k| p + k);
        } else {
            while p < self.end && !SPECIAL[usize::from(self.line[p])] {
                p += 1;
            }
        }
        self.take_to(p, self.content_kind());
    }

    fn dollar(&mut self) {
        let double = self.pos + 1 < self.end && self.line[self.pos + 1] == b'$';
        match self.mode {
            Mode::Text => {
                let (frame, n) = if double {
                    (Frame::DoubleDollar, 2)
                } else {
                    (Frame::Dollar, 1)
                };
                if self.frames.len() < MAX_FRAMES {
                    self.take(n, TokenKind::MathDelim);
                    self.push(frame);
                } else {
                    self.take(n, TokenKind::Text);
                }
            }
            Mode::Math => {
                let i = self.mode_frame.unwrap_or(0);
                match self.frames.get(i) {
                    Some(Frame::Dollar) => {
                        self.take(1, TokenKind::MathDelim);
                        self.truncate(i);
                    }
                    Some(Frame::DoubleDollar) if double => {
                        self.take(2, TokenKind::MathDelim);
                        self.truncate(i);
                    }
                    // A `$` inside `\(…\)`, `\[…\]` or an environment is a TeX error; keep the math.
                    _ => self.take(1, TokenKind::Math),
                }
            }
            Mode::Arg(_) | Mode::Verbatim { .. } => self.take(1, self.content_kind()),
        }
    }

    fn brace_kind(&self) -> TokenKind {
        match self.mode {
            Mode::Text => TokenKind::Brace,
            _ => self.content_kind(),
        }
    }

    fn open_brace(&mut self) {
        let kind = self.brace_kind();
        if let Some(depth) = self.frames.last_mut().and_then(Frame::depth_mut) {
            *depth = depth.saturating_add(1);
        }
        self.take(1, kind);
    }

    fn close_brace(&mut self) {
        let kind = self.brace_kind();
        match self.frames.last_mut() {
            Some(frame) if frame.depth().is_some_and(|d| d > 0) => {
                if let Some(depth) = frame.depth_mut() {
                    *depth -= 1;
                }
                self.take(1, kind);
            }
            Some(Frame::TextInMath { .. } | Frame::Ensure { .. }) => {
                self.take(1, TokenKind::Brace);
                self.frames.pop();
                self.refresh_mode();
            }
            Some(&mut Frame::Arg {
                spec,
                idx,
                delim: Delim::Brace,
                ..
            }) => {
                self.take(1, TokenKind::Brace);
                self.frames.pop();
                self.refresh_mode();
                self.resume(spec, idx, Delim::Brace);
            }
            // A stray `}`.
            _ => self.take(1, kind),
        }
    }

    /// `]` or `)`: closes an optional argument with that delimiter at depth 0.
    fn close_optional(&mut self, closing: Delim) {
        if let Some(&Frame::Arg {
            spec,
            idx,
            delim,
            depth: 0,
        }) = self.frames.last()
            && delim == closing
        {
            self.take(1, TokenKind::Bracket);
            self.frames.pop();
            self.refresh_mode();
            self.resume(spec, idx, delim);
        } else {
            self.take(1, self.content_kind());
        }
    }

    fn comma(&mut self) {
        if let Mode::Arg(kind) = self.mode
            && kind.is_list()
            && matches!(self.frames.last(), Some(Frame::Arg { depth: 0, .. }))
        {
            self.arg_start = self.pos + 1;
        }
        self.take(1, self.content_kind());
    }

    fn backslash(&mut self) -> Option<Stop> {
        let start = self.pos;
        let Some(&c) = self.line.get(start + 1).filter(|_| start + 1 < self.end) else {
            let kind = self.command_kind(CmdInfo::default());
            self.take(1, kind);
            return Some(Stop::CommandName(start));
        };
        if !is_letter(c) {
            return self.control_symbol(c);
        }
        let mut e = start + 2;
        while e < self.end && is_letter(self.line[e]) {
            e += 1;
        }
        let info = self.t.cmd(&self.line[start + 1..e]);
        let kind = self.command_kind(info);
        if e >= self.end {
            self.take_to(e, kind);
            return Some(Stop::CommandName(start));
        }
        if info.star && self.line[e] == b'*' {
            e += 1;
        }
        self.take_to(e, kind);
        self.after_command(info)
    }

    fn command_kind(&self, info: CmdInfo) -> TokenKind {
        match (info.action, self.mode) {
            (Action::Begin, _) => TokenKind::Begin,
            (Action::End, _) => TokenKind::End,
            (_, Mode::Math) => TokenKind::MathCommand,
            _ if info.sectioning => TokenKind::Sectioning,
            _ => TokenKind::Command,
        }
    }

    /// The effect of a control word, now that its name has been consumed.
    fn after_command(&mut self, info: CmdInfo) -> Option<Stop> {
        match info.action {
            Action::Begin => return self.begin_end(false),
            Action::End => return self.begin_end(true),
            // Inside tracked arguments only `\begin`/`\end` are interpreted.
            _ if matches!(self.mode, Mode::Arg(_)) => return None,
            Action::InlineVerbatim(style) => return self.inline_verbatim(style),
            _ => {}
        }
        let group = match self.mode {
            Mode::Text if info.action == Action::EnsureMath => Some(Frame::Ensure { depth: 0 }),
            Mode::Math if info.text_in_math => Some(Frame::TextInMath { depth: 0 }),
            _ => None,
        };
        if group.is_some_and(|frame| self.open_group(frame)) {
            return None;
        }
        if let Some(spec) = info.spec
            && !(self.mode == Mode::Math && self.t.spec(spec).shape.text_only)
        {
            self.expect = Some((spec, 0));
        }
        None
    }

    /// Opens `frame` at a `{` after optional blanks.
    fn open_group(&mut self, frame: Frame) -> bool {
        let mut p = self.pos;
        while p < self.end && is_blank(self.line[p]) {
            p += 1;
        }
        if p >= self.end || self.line[p] != b'{' || self.frames.len() >= MAX_FRAMES {
            return false;
        }
        self.take_to(p, self.content_kind());
        self.take(1, TokenKind::Brace);
        self.push(frame);
        true
    }

    fn control_symbol(&mut self, c: u8) -> Option<Stop> {
        let n = (1 + utf8_len(c)).min(self.end - self.pos);
        let delim = match c {
            b'(' if self.mode == Mode::Text => self.push(Frame::Paren),
            b'[' if self.mode == Mode::Text => self.push(Frame::Bracket),
            b')' => self.pop_delim(Frame::Paren),
            b']' => self.pop_delim(Frame::Bracket),
            _ => false,
        };
        let escaped = matches!(
            c,
            b'$' | b'%' | b'&' | b'#' | b'_' | b'{' | b'}' | b'~' | b'^'
        );
        let kind = match self.mode {
            _ if delim => TokenKind::MathDelim,
            Mode::Math => TokenKind::MathCommand,
            Mode::Text if escaped => TokenKind::Special,
            Mode::Arg(_) if escaped => self.content_kind(),
            _ => TokenKind::Command,
        };
        self.take(n, kind);
        None
    }

    /// `\)` or `\]`: closes the innermost matching delimiter and everything opened inside it.
    fn pop_delim(&mut self, frame: Frame) -> bool {
        match self.frames.iter().rposition(|f| *f == frame) {
            Some(i) => {
                self.truncate(i);
                true
            }
            None => false,
        }
    }

    /// `\begin{name}` / `\end{name}` (the name must be on this line).
    fn begin_end(&mut self, end: bool) -> Option<Stop> {
        let mut p = self.pos;
        while p < self.end && is_blank(self.line[p]) {
            p += 1;
        }
        if p >= self.end || self.line[p] != b'{' {
            return None;
        }
        self.take_to(p, self.content_kind());
        self.take(1, TokenKind::Brace);
        let name_start = self.pos;
        let Some(len) = memchr::memchr(b'}', &self.line[name_start..self.end]) else {
            self.take_to(self.end, TokenKind::EnvName);
            return Some(Stop::EnvName {
                end,
                start: name_start,
            });
        };
        self.take(len, TokenKind::EnvName);
        self.take(1, TokenKind::Brace);
        let name = &self.line[name_start..name_start + len];
        let (kind, star) = self.t.env(name);
        if end {
            if let EnvKind::Math(env) = kind
                && let Some(i) = self
                    .frames
                    .iter()
                    .rposition(|f| *f == Frame::MathEnv { env, star })
            {
                self.truncate(i);
            }
            return None;
        }
        if matches!(self.mode, Mode::Arg(_)) {
            return None;
        }
        match kind {
            EnvKind::Math(env) => {
                self.push(Frame::MathEnv { env, star });
                None
            }
            EnvKind::Verbatim(env) => {
                let stop = self.verbatim_env_args();
                self.push(Frame::Verbatim { env, star });
                stop
            }
            EnvKind::Spec(spec) => {
                self.expect = Some((spec, 1));
                None
            }
        }
    }

    /// `[opts]` and `{lang}` right after `\begin{lstlisting}`, `\begin{minted}` and the like.
    fn verbatim_env_args(&mut self) -> Option<Stop> {
        loop {
            let (close, kind) = match self.peek() {
                Some(b'[') => (b']', TokenKind::Bracket),
                Some(b'{') => (b'}', TokenKind::Brace),
                _ => return None,
            };
            if !self.delimited(close, kind, TokenKind::KeyArg) {
                return Some(Stop::Verbatim);
            }
        }
    }

    /// At an opening delimiter: classifies it, the content up to the first `close` and `close`.
    /// Returns false, with the rest of the line as content, when `close` is not on the line.
    fn delimited(&mut self, close: u8, delim: TokenKind, content: TokenKind) -> bool {
        self.take(1, delim);
        match memchr::memchr(close, &self.line[self.pos..self.end]) {
            Some(len) => {
                self.take(len, content);
                self.take(1, delim);
                true
            }
            None => {
                self.take_to(self.end, content);
                false
            }
        }
    }

    /// The arguments of `\verb`, `\lstinline`, `\mintinline`, `\url` and `\href`.
    fn inline_verbatim(&mut self, style: VerbStyle) -> Option<Stop> {
        use VerbStyle::{Href, Listing, Minted, Url, Verb};
        if matches!(style, Listing | Minted)
            && self.peek() == Some(b'[')
            && !self.delimited(b']', TokenKind::Bracket, TokenKind::KeyArg)
        {
            return Some(Stop::Verbatim);
        }
        if matches!(style, Minted | Url | Href) {
            self.skip_blanks();
        }
        if style == Minted {
            if self.peek() != Some(b'{') {
                return None;
            }
            if !self.delimited(b'}', TokenKind::Brace, TokenKind::KeyArg) {
                return Some(Stop::Verbatim);
            }
        }
        let c = self.peek()?;
        if c == b'{' && style != Verb {
            self.take(1, TokenKind::Brace);
            let mut depth = 0u32;
            let mut p = self.pos;
            while p < self.end {
                match self.line[p] {
                    b'{' => depth += 1,
                    b'}' if depth == 0 => break,
                    b'}' => depth -= 1,
                    _ => {}
                }
                p += 1;
            }
            self.take_to(p, TokenKind::Verbatim);
            if p >= self.end {
                return Some(Stop::Verbatim);
            }
            self.take(1, TokenKind::Brace);
            return None;
        }
        if style == Href || (style == Verb && (c.is_ascii_alphabetic() || c == b'\t')) {
            return None;
        }
        let n = utf8_len(c).min(self.end - self.pos);
        let from = self.pos + n;
        let found = memmem::find(&self.line[from..self.end], &self.line[self.pos..from]);
        match found {
            Some(k) => {
                self.take_to(from + k + n, TokenKind::Verbatim);
                None
            }
            None => {
                self.take_to(self.end, TokenKind::Verbatim);
                Some(Stop::Verbatim)
            }
        }
    }

    /// Verbatim body up to `\end{name}` (exactly as LaTeX's verbatim requires).
    fn verbatim_body(&mut self, env: u16, star: bool) -> Option<Stop> {
        let kind = self.content_kind();
        let marker = &self.t.env_entries[usize::from(env)].end_markers[usize::from(star)];
        let Some(k) = marker.find(&self.line[self.pos..self.end]) else {
            self.take_to(self.end, kind);
            return Some(Stop::Verbatim);
        };
        let at = self.pos + k;
        let len = marker.needle().len();
        self.take_to(at, kind);
        self.take(4, TokenKind::End);
        self.take(1, TokenKind::Brace);
        self.take(len - 6, TokenKind::EnvName);
        self.take(1, TokenKind::Brace);
        // The mode is Verbatim only when `mode_frame` is the verbatim frame.
        if let Some(i) = self.mode_frame {
            self.truncate(i);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{Collect, Discard};

    fn state_after(lines: &[&str]) -> Vec<Frame> {
        let t = Tables::new(&ScanConfig::default());
        let mut frames = Vec::new();
        for line in lines {
            scan(&t, &mut frames, line.as_bytes(), line.len(), Discard);
        }
        frames
    }

    #[test]
    fn frames_follow_math_and_arguments() {
        assert_eq!(state_after(&["a $x"]), [Frame::Dollar]);
        assert_eq!(state_after(&["a $x$ b"]), []);
        assert_eq!(state_after(&["\\[ \\text{if $x"]).len(), 3);
        assert_eq!(state_after(&["\\[ x", ""]), []);
        assert!(matches!(
            state_after(&["\\begin{align*}"])[..],
            [Frame::MathEnv { star: true, .. }]
        ));
        assert!(matches!(
            state_after(&["\\cite{a,"])[..],
            [Frame::Arg {
                delim: Delim::Brace,
                ..
            }]
        ));
    }

    #[test]
    fn nesting_is_capped() {
        let line = "$\\text{".repeat(40);
        assert_eq!(state_after(&[&line]).len(), MAX_FRAMES);
    }

    #[test]
    fn quick_and_token_scans_agree_on_state() {
        let t = Tables::new(&ScanConfig::default());
        let lines = [
            "a $b$ {c} \\cite{x,",
            " y} \\[ \\text{ $z$ } \\] % c",
            "\\begin{verbatim} $ ",
            "\\end{verbatim} \\(a\\) \\verb|$|",
        ];
        let (mut quick, mut full) = (Vec::new(), Vec::new());
        for line in lines {
            let mut out = Vec::new();
            scan(&t, &mut quick, line.as_bytes(), line.len(), Discard);
            scan(
                &t,
                &mut full,
                line.as_bytes(),
                line.len(),
                Collect::new(&mut out),
            );
            assert_eq!(quick, full, "after {line:?}");
        }
    }

    /// Pieces of lines that open, close or neutralise scanner constructs.
    const PIECES: &[&str] = &[
        "$",
        "$$",
        "\\(",
        "\\)",
        "\\[",
        "\\]",
        "{",
        "}",
        "[",
        "]",
        "(",
        ")",
        ",",
        "%",
        "\\%",
        "\\$",
        "\\",
        "\\\\",
        " ",
        "x",
        "é",
        "&",
        "\\text{",
        "\\ensuremath{",
        "\\begin{equation}",
        "\\end{equation}",
        "\\begin{verbatim}",
        "\\end{verbatim}",
        "\\begin{comment}",
        "\\end{comment}",
        "\\begin{lstlisting}[",
        "\\begin{theorem}",
        "\\begin{tabularx}{",
        "\\verb|",
        "|",
        "\\lstinline{",
        "\\url{",
        "\\href{",
        "\\cite",
        "\\cite{",
        "\\cites(",
        "\\cref{",
        "\\label{",
        "\\usepackage[",
        "\\crefrange{",
        "\\textcolor{",
        "\\item",
    ];

    proptest::proptest! {
        /// The quick skip of state-only scans must not change the resulting state.
        #[test]
        fn quick_scan_matches_token_scan(
            lines in proptest::collection::vec(
                proptest::collection::vec(proptest::sample::select(PIECES), 0..14),
                1..8,
            )
        ) {
            let t = Tables::new(&ScanConfig::default());
            let (mut quick, mut full) = (Vec::new(), Vec::new());
            for pieces in &lines {
                let line = pieces.concat();
                let mut out = Vec::new();
                scan(&t, &mut quick, line.as_bytes(), line.len(), Discard);
                scan(&t, &mut full, line.as_bytes(), line.len(), Collect::new(&mut out));
                proptest::prop_assert_eq!(&quick, &full, "after {:?}", line);
                let covered: u32 = out.iter().map(|tok| tok.end - tok.start).sum();
                proptest::prop_assert_eq!(covered as usize, line.len());
            }
        }
    }

    #[test]
    fn bloom_rejects_without_false_negatives() {
        let t = Tables::new(&ScanConfig::default());
        for name in t.cmds.keys() {
            assert!(t.cmd_filter.may_contain(name));
        }
        assert!(t.cmd(b"begin").action == Action::Begin);
        assert!(t.cmd(b"text").text_in_math);
        assert!(t.cmd(b"alpha").spec.is_none());
    }
}
