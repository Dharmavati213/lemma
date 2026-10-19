//! [`CompletionRequest`]: what is being completed, and which source answers it.

use crate::CompletionDb;
use lemma_syntax::{ArgKind, Context};

/// The text and context being completed.
///
/// Build it with [`CompletionRequest::new`], which derives `prefix` from the context, or fill
/// the fields directly. `line_before` must end with `prefix`.
///
/// **Which context to pass.** `ctx` is the context at the cursor (`Document::context_at`).
/// Inside a command name the scanner reports `Context::CommandName { start }`, which says
/// nothing about math: pass the context at `start` instead (the mode around the command) so that
/// math-only commands are hidden in text and text-only ones in math. With `CommandName` itself,
/// commands of every mode are offered. Likewise for `\begin{…`: with the scanner's `Context::Arg`
/// every environment is offered; with the context at the backslash of `\begin`, environments
/// that start math (`align`) are hidden in math and math-only ones (`pmatrix`) in text. After
/// `\end{` every environment is offered whatever the context: it closes what is open.
#[derive(Clone, Copy, Debug)]
pub struct CompletionRequest<'a> {
    pub ctx: &'a Context,
    /// Text being completed (without the leading backslash for commands).
    pub prefix: &'a str,
    /// The line up to the cursor.
    pub line_before: &'a str,
}

impl<'a> CompletionRequest<'a> {
    /// A request whose `prefix` is derived from `ctx` and `line_before`:
    ///
    /// - in an argument (`Context::Arg`), the item typed so far: `line_before[start..]`;
    /// - in a command name, or right after an unescaped `\` and letters, those letters;
    /// - inside the braces of a command on this line, an item that looks like a label, key or
    ///   path (letters and digits with `:`, `-`, `.`, `_` or `/`: `\refeq{eq:co`), so commands
    ///   the scanner does not classify can still complete labels or files (see
    ///   [`CompletionDb::arg_kind`]);
    /// - otherwise the letters and digits before the cursor (a word or snippet trigger).
    pub fn new(ctx: &'a Context, line_before: &'a str) -> Self {
        let prefix = match ctx {
            Context::Arg { start, .. } => line_before.get(*start as usize..).unwrap_or(""),
            Context::CommandName { .. } => command_prefix(line_before).unwrap_or(""),
            _ => command_prefix(line_before)
                .or_else(|| {
                    arg_item(line_before)
                        .map(|(_, start)| &line_before[start..])
                        .filter(|item| is_key_like(item))
                })
                .unwrap_or_else(|| word_prefix(line_before)),
        };
        Self {
            ctx,
            prefix,
            line_before,
        }
    }

    /// Whether a command name is being completed: the prefix is letters after an unescaped `\`
    /// (outside an argument).
    pub fn is_command(&self) -> bool {
        !matches!(self.ctx, Context::Arg { .. })
            && self.prefix.bytes().all(|b| b.is_ascii_alphabetic())
            && self
                .line_before
                .strip_suffix(self.prefix)
                .is_some_and(ends_with_unescaped_backslash)
    }

    /// The byte offset in `line_before` where the text replaced by an accepted item starts:
    /// before the backslash for commands, before the prefix otherwise.
    pub fn replace_start(&self) -> usize {
        let start = self.line_before.len().saturating_sub(self.prefix.len());
        if self.is_command() {
            start.saturating_sub(1)
        } else {
            start
        }
    }
}

/// Whether the mode around the cursor is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModeCtx {
    Math,
    Text,
    Unknown,
}

/// The source that answers a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// Nothing is completed here (comments, verbatim, label definitions, key-value options).
    None,
    Command(ModeCtx),
    /// Environment names after `\begin{` or `\end{`.
    Environment(ModeCtx),
    /// Label names for a reference command (`cmd` without the backslash).
    Ref(&'a str),
    Cite,
    File(&'a str),
    Package(&'a str),
    /// Manual snippets, and in text the dictionary and project words.
    Word(ModeCtx),
}

/// Decides the source for `req`.
pub(crate) fn target<'a>(db: &CompletionDb, req: &CompletionRequest<'a>) -> Target<'a> {
    let mode = match req.ctx {
        Context::Comment | Context::Verbatim => return Target::None,
        Context::CommandName { .. } => return Target::Command(ModeCtx::Unknown),
        Context::Arg { cmd, kind, .. } => return arg_target(cmd, *kind),
        Context::Math { .. } => ModeCtx::Math,
        Context::Text | Context::TextInMath => ModeCtx::Text,
    };
    if req.is_command() {
        return Target::Command(mode);
    }
    if let Some((cmd, start)) = arg_item(req.line_before)
        && req.line_before.len() - start == req.prefix.len()
    {
        // `\end{` closes whatever is open, also an environment that starts math inside it.
        if cmd == "end" {
            return Target::Environment(ModeCtx::Unknown);
        }
        if cmd == "begin" {
            return Target::Environment(mode);
        }
        if let Some(kind) = db.arg_kind(cmd) {
            let t = arg_target(cmd, kind);
            if t != Target::None {
                return t;
            }
        }
    }
    Target::Word(mode)
}

fn arg_target(cmd: &str, kind: ArgKind) -> Target<'_> {
    match kind {
        ArgKind::Env if cmd == "begin" || cmd == "end" => Target::Environment(ModeCtx::Unknown),
        ArgKind::Ref => Target::Ref(cmd),
        ArgKind::Cite => Target::Cite,
        ArgKind::File => Target::File(cmd),
        ArgKind::Package => Target::Package(cmd),
        _ => Target::None,
    }
}

/// Whether `s` ends with a backslash that is not itself escaped (`\\` is a line break).
fn ends_with_unescaped_backslash(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// The letters after a trailing unescaped `\` + letters (`"a \fr"` → `"fr"`, `"\"` → `""`).
fn command_prefix(line_before: &str) -> Option<&str> {
    let letters = line_before
        .bytes()
        .rev()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    let (before, prefix) = line_before.split_at(line_before.len() - letters);
    ends_with_unescaped_backslash(before).then_some(prefix)
}

/// Letters and digits with at least one of the separators of labels, keys and paths
/// (`eq:cocycle`, `quasi-co`, `fig/plot.pdf`), or a starred name (`align*`).
fn is_key_like(item: &str) -> bool {
    let separator = |c: char| matches!(c, ':' | '-' | '.' | '_' | '/');
    let (body, starred) = match item.strip_suffix('*') {
        Some(body) => (body, true),
        None => (item, false),
    };
    (starred || body.chars().any(separator))
        && body.chars().all(|c| c.is_alphanumeric() || separator(c))
}

/// The letters and digits before the cursor.
fn word_prefix(line_before: &str) -> &str {
    let start = line_before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric())
        .last()
        .map_or(line_before.len(), |(i, _)| i);
    &line_before[start..]
}

/// Bytes that end an argument item when scanning backwards.
fn ends_item(b: u8) -> bool {
    matches!(
        b,
        b'{' | b'}' | b',' | b'\\' | b'$' | b'%' | b' ' | b'\t' | b'[' | b']'
    )
}

/// When the cursor is inside the braces of `\cmd[…]{a, b, item` on this line: the command
/// name (without the backslash, with its `*`) and the byte where the item starts.
pub(crate) fn arg_item(line_before: &str) -> Option<(&str, usize)> {
    let b = line_before.as_bytes();
    let mut i = b.len();
    while i > 0 && !ends_item(b[i - 1]) {
        i -= 1;
    }
    let item_start = i;
    // Back over blanks and earlier list items to the opening brace.
    loop {
        while i > 0 && matches!(b[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        match b.get(i.checked_sub(1)?)? {
            b'{' => break,
            b',' => {
                i -= 1;
                while i > 0 && !ends_item(b[i - 1]) {
                    i -= 1;
                }
            }
            _ => return None,
        }
    }
    let mut j = i - 1;
    // Optional arguments between the name and the brace.
    while j > 0 && b[j - 1] == b']' {
        let open = b[..j - 1].iter().rposition(|&c| c == b'[')?;
        if b[open..j - 1].contains(&b'{') {
            return None;
        }
        j = open;
    }
    let name_end = j;
    if j > 0 && b[j - 1] == b'*' {
        j -= 1;
    }
    let letters = b[..j]
        .iter()
        .rev()
        .take_while(|c| c.is_ascii_alphabetic())
        .count();
    if letters == 0 {
        return None;
    }
    let name_start = j - letters;
    ends_with_unescaped_backslash(&line_before[..name_start])
        .then(|| (&line_before[name_start..name_end], item_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn math() -> Context {
        Context::Math {
            display: false,
            env: None,
        }
    }

    #[test]
    fn prefixes() {
        let m = math();
        let r = CompletionRequest::new(&m, r"$a+\fr");
        assert_eq!(r.prefix, "fr");
        assert!(r.is_command());
        assert_eq!(r.replace_start(), 3);
        let r = CompletionRequest::new(&m, r"$\");
        assert_eq!(r.prefix, "");
        assert!(r.is_command());
        assert_eq!(r.replace_start(), 1);
        let t = Context::Text;
        let r = CompletionRequest::new(&t, r"a\\fr");
        assert_eq!(r.prefix, "fr");
        assert!(!r.is_command());
        assert_eq!(r.replace_start(), 3);
        let r = CompletionRequest::new(&t, "  thm");
        assert_eq!(r.prefix, "thm");
        assert!(!r.is_command());
        let r = CompletionRequest::new(&t, "the étale");
        assert_eq!(r.prefix, "étale");
        let r = CompletionRequest::new(&t, "end. ");
        assert_eq!(r.prefix, "");
        let r = CompletionRequest::new(&t, r"see \refeq{eq:co");
        assert_eq!(r.prefix, "eq:co");
        assert_eq!(r.replace_start(), 11);
        let r = CompletionRequest::new(&t, r"\textbf{word");
        assert_eq!(r.prefix, "word");
        let r = CompletionRequest::new(&t, r"\emph{quasi-co");
        assert_eq!(r.prefix, "quasi-co");
        let r = CompletionRequest::new(&m, r"\frac{a+b");
        assert_eq!(r.prefix, "b");
    }

    #[test]
    fn context_prefixes() {
        let line = r"see \cref{sec:a, thm:ma";
        let ctx = Context::Arg {
            cmd: "cref".into(),
            idx: 0,
            kind: ArgKind::Ref,
            start: 17,
        };
        let r = CompletionRequest::new(&ctx, line);
        assert_eq!(r.prefix, "thm:ma");
        assert!(!r.is_command());
        assert_eq!(r.replace_start(), 17);
        let ctx = Context::CommandName { start: 2 };
        let r = CompletionRequest::new(&ctx, r"$ \alp");
        assert_eq!(r.prefix, "alp");
        assert!(r.is_command());
        assert_eq!(r.replace_start(), 2);
        // A context that does not match the line never replaces text before the cursor.
        let r = CompletionRequest::new(&ctx, "$ x+");
        assert_eq!(r.prefix, "");
        assert!(!r.is_command());
        assert_eq!(r.replace_start(), 4);
        let ctx = Context::Arg {
            cmd: "ref".into(),
            idx: 0,
            kind: ArgKind::Ref,
            start: 99,
        };
        assert_eq!(CompletionRequest::new(&ctx, r"\ref{").prefix, "");
    }

    #[test]
    fn arg_items() {
        assert_eq!(arg_item(r"\refeq{eq:co"), Some(("refeq", 7)));
        assert_eq!(arg_item(r"\cite[p.~3]{a,b, c"), Some(("cite", 17)));
        assert_eq!(arg_item(r"\cref*{"), Some(("cref*", 7)));
        assert_eq!(arg_item(r"\\{x"), None);
        assert_eq!(arg_item(r"{x"), None);
        assert_eq!(arg_item(r"\frac{a}{b"), None);
        assert_eq!(arg_item(r"\ref{a}"), None);
        assert_eq!(arg_item(r"x \ref{a} b"), None);
        assert_eq!(arg_item(""), None);
    }

    #[test]
    fn targets() {
        let db = CompletionDb::builtin();
        let t = Context::Text;
        let m = math();
        fn target_of<'a>(db: &CompletionDb, ctx: &'a Context, line: &'a str) -> Target<'a> {
            target(db, &CompletionRequest::new(ctx, line))
        }
        let target_of = |ctx, line| target_of(&db, ctx, line);
        assert_eq!(target_of(&m, r"\fr"), Target::Command(ModeCtx::Math));
        assert_eq!(target_of(&t, r"\fr"), Target::Command(ModeCtx::Text));
        assert_eq!(target_of(&t, "wor"), Target::Word(ModeCtx::Text));
        assert_eq!(target_of(&Context::Comment, r"\fr"), Target::None);
        assert_eq!(target_of(&t, r"\eqref{eq:a"), Target::Ref("eqref"));
        let env = Context::Arg {
            cmd: "begin".into(),
            idx: 0,
            kind: ArgKind::Env,
            start: 7,
        };
        assert_eq!(
            target_of(&env, r"\begin{al"),
            Target::Environment(ModeCtx::Unknown)
        );
        assert_eq!(
            target_of(&m, r"\begin{pm"),
            Target::Environment(ModeCtx::Math)
        );
        // `\end{` is not filtered by mode: inside `align` it must offer `align`.
        assert_eq!(
            target_of(&t, r"\end{align*"),
            Target::Environment(ModeCtx::Unknown)
        );
        assert_eq!(
            target_of(&m, r"\end{al"),
            Target::Environment(ModeCtx::Unknown)
        );
        let label = Context::Arg {
            cmd: "label".into(),
            idx: 0,
            kind: ArgKind::Label,
            start: 7,
        };
        assert_eq!(target_of(&label, r"\label{x"), Target::None);
    }
}
