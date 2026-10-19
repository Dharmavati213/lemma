//! Snippet bodies: parsing into chunks and indenting continuation lines.

use std::fmt;

/// A piece of a parsed body, mapped one-to-one onto a GtkSourceSnippetChunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Literal text: `set_text` + `set_text_set(true)`, focus -1.
    Text(String),
    /// First occurrence of tab stop `n`; `default` is pre-selected text ("" = none). n = 0 is the exit.
    Stop { n: u8, default: String },
    /// Later occurrence of `n`: GtkSourceSnippetChunk with spec "$n", focus -1.
    Mirror(u8),
}

/// Why a body was rejected; [`parse_body`] reports it as a message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodyError {
    /// The default of tab stop `n` contains a `$` (nested tab stops are not supported).
    DefaultHasDollar(u8),
    /// Any other problem.
    Invalid(String),
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultHasDollar(n) => write!(f, "the default of ${n} contains `$`"),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}

/// Parses a body; appends `Stop { n: 0 }` at the end if the body has no `$0`.
///
/// - `$N`, `${N}` and `${N:default}` with a single digit N are tab stops; the first occurrence of
///   N is a [`Chunk::Stop`], later ones are [`Chunk::Mirror`]s. Only the first may have a default,
///   and `$0` may appear once.
/// - A default ends at the `}` that balances the `${`; a backslash keeps the next character from
///   counting as a brace (`${1:\}}` has the default `\}`). Defaults may not contain `$`.
/// - `${$}` is a literal `$`; any other `$` is literal (`$$1` is `$` then `$1`; `${10}` is text).
/// - Backslashes are copied verbatim. Adjacent text is merged into one [`Chunk::Text`].
pub fn parse_body(body: &str) -> Result<Vec<Chunk>, String> {
    parse(body).map_err(|e| e.to_string())
}

/// [`parse_body`] with a typed error.
pub(crate) fn parse(body: &str) -> Result<Vec<Chunk>, BodyError> {
    let mut out = ChunkBuilder::default();
    let mut rest = body;
    while let Some(dollar) = rest.find('$') {
        out.text(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let (piece, used) = piece_after_dollar(after)?;
        match piece {
            Piece::Dollar => out.text("$"),
            Piece::Stop(n, default) => out.stop(n, default)?,
        }
        rest = &after[used..];
    }
    out.text(rest);
    Ok(out.finish())
}

/// What a `$` starts.
enum Piece<'a> {
    /// A literal `$`.
    Dollar,
    /// Tab stop `n` with its default ("" = none).
    Stop(u8, &'a str),
}

/// Reads the syntax after a `$`; returns the piece and how many bytes of `after` it spans.
fn piece_after_dollar(after: &str) -> Result<(Piece<'_>, usize), BodyError> {
    let bytes = after.as_bytes();
    let digit = |i: usize| {
        bytes
            .get(i)
            .filter(|b| b.is_ascii_digit())
            .map(|b| b - b'0')
    };
    if let Some(n) = digit(0) {
        return Ok((Piece::Stop(n, ""), 1));
    }
    if after.starts_with("{$}") {
        return Ok((Piece::Dollar, 3));
    }
    if bytes.first() == Some(&b'{')
        && let Some(n) = digit(1)
    {
        match bytes.get(2) {
            Some(b'}') => return Ok((Piece::Stop(n, ""), 3)),
            Some(b':') => {
                let len = default_len(&after[3..]).ok_or_else(|| {
                    BodyError::Invalid(format!("the default of ${n} has no closing `}}`"))
                })?;
                let default = &after[3..3 + len];
                if default.contains('$') {
                    return Err(BodyError::DefaultHasDollar(n));
                }
                return Ok((Piece::Stop(n, default), 3 + len + 1));
            }
            _ => {}
        }
    }
    Ok((Piece::Dollar, 0))
}

/// Byte length of a `${N:` default: up to the `}` at brace depth 0, skipping `\x` pairs.
fn default_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' if depth == 0 => return Some(i),
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Accumulates chunks, merging adjacent text and turning repeated stops into mirrors.
#[derive(Default)]
struct ChunkBuilder {
    chunks: Vec<Chunk>,
    seen: [bool; 10],
}

impl ChunkBuilder {
    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        match self.chunks.last_mut() {
            Some(Chunk::Text(t)) => t.push_str(s),
            _ => self.chunks.push(Chunk::Text(s.to_owned())),
        }
    }

    fn stop(&mut self, n: u8, default: &str) -> Result<(), BodyError> {
        let seen = &mut self.seen[usize::from(n)];
        if !*seen {
            *seen = true;
            self.chunks.push(Chunk::Stop {
                n,
                default: default.to_owned(),
            });
            return Ok(());
        }
        if n == 0 {
            return Err(BodyError::Invalid("`$0` may appear only once".into()));
        }
        if !default.is_empty() {
            return Err(BodyError::Invalid(format!(
                "${n} repeats with a default; only its first occurrence may have one"
            )));
        }
        self.chunks.push(Chunk::Mirror(n));
        Ok(())
    }

    fn finish(mut self) -> Vec<Chunk> {
        if !self.seen[0] {
            self.chunks.push(Chunk::Stop {
                n: 0,
                default: String::new(),
            });
        }
        self.chunks
    }
}

/// Prefix every '\n' inside Text chunks with `indent` (the current line's leading whitespace).
/// Required: GtkSourceView does not indent snippet continuation lines.
///
/// Stop defaults are indented the same way. A line left empty inside a chunk (`"\n\n"`) gets no
/// indent, so expansions do not leave trailing whitespace; a line that a following stop or mirror
/// fills is indented.
pub fn with_indent(chunks: &[Chunk], indent: &str) -> Vec<Chunk> {
    chunks
        .iter()
        .map(|chunk| match chunk {
            Chunk::Text(t) => Chunk::Text(indent_lines(t, indent)),
            Chunk::Stop { n, default } => Chunk::Stop {
                n: *n,
                default: indent_lines(default, indent),
            },
            Chunk::Mirror(n) => Chunk::Mirror(*n),
        })
        .collect()
}

/// Inserts `indent` after each '\n' of `s` unless another '\n' follows immediately.
fn indent_lines(s: &str, indent: &str) -> String {
    let mut out = String::with_capacity(s.len() + indent.len() * s.matches('\n').count());
    let mut lines = s.split('\n').peekable();
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    while let Some(line) = lines.next() {
        out.push('\n');
        if !line.is_empty() || lines.peek().is_none() {
            out.push_str(indent);
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Chunk {
        Chunk::Text(s.into())
    }

    fn stop(n: u8) -> Chunk {
        Chunk::Stop {
            n,
            default: String::new(),
        }
    }

    fn stop_with(n: u8, default: &str) -> Chunk {
        Chunk::Stop {
            n,
            default: default.into(),
        }
    }

    fn parsed(body: &str) -> Vec<Chunk> {
        parse_body(body).unwrap_or_else(|e| panic!("{body:?}: {e}"))
    }

    #[test]
    fn double_dollar_is_a_literal_dollar_before_a_stop() {
        assert_eq!(parsed("$$1$$0"), [text("$"), stop(1), text("$"), stop(0)]);
    }

    #[test]
    fn braced_dollar_is_literal() {
        assert_eq!(
            parsed("${$}$1${$}$0"),
            [text("$"), stop(1), text("$"), stop(0)]
        );
        assert_eq!(parsed("a${$}1b"), [text("a$1b"), stop(0)]);
    }

    #[test]
    fn default_value() {
        assert_eq!(
            parsed(r"\includegraphics[width=${1:0.8}\textwidth]{$2}"),
            [
                text(r"\includegraphics[width="),
                stop_with(1, "0.8"),
                text(r"\textwidth]{"),
                stop(2),
                text("}"),
                stop(0),
            ]
        );
    }

    #[test]
    fn default_with_balanced_braces_and_escaped_brace() {
        assert_eq!(
            parsed(r"${1:\frac{a}{b}}x"),
            [stop_with(1, r"\frac{a}{b}"), text("x"), stop(0)]
        );
        assert_eq!(
            parsed(r"${1:\}}x"),
            [stop_with(1, r"\}"), text("x"), stop(0)]
        );
        assert_eq!(parsed("${1:}"), [stop(1), stop(0)]);
    }

    #[test]
    fn braced_stop() {
        assert_eq!(parsed("${1}a${0}"), [stop(1), text("a"), stop(0)]);
    }

    #[test]
    fn repeated_stop_is_a_mirror() {
        assert_eq!(
            parsed("\\begin{$1}\n  $0\n\\end{$1}"),
            [
                text("\\begin{"),
                stop(1),
                text("}\n  "),
                stop(0),
                text("\n\\end{"),
                Chunk::Mirror(1),
                text("}"),
            ]
        );
        assert_eq!(
            parsed("${1:x}-${1}-$1"),
            [
                stop_with(1, "x"),
                text("-"),
                Chunk::Mirror(1),
                text("-"),
                Chunk::Mirror(1),
                stop(0)
            ]
        );
    }

    #[test]
    fn implicit_exit_stop_is_appended() {
        assert_eq!(
            parsed(r"\frac{$1}{$2}"),
            [
                text(r"\frac{"),
                stop(1),
                text("}{"),
                stop(2),
                text("}"),
                stop(0)
            ]
        );
        assert_eq!(parsed("^{-1}"), [text("^{-1}"), stop(0)]);
        assert_eq!(parsed(""), [stop(0)]);
        assert_eq!(parsed("$0x"), [stop(0), text("x")]);
    }

    #[test]
    fn other_dollars_and_backslashes_are_literal() {
        assert_eq!(parsed(r"\$a$"), [text(r"\$a$"), stop(0)]);
        assert_eq!(parsed("${x}$"), [text("${x}$"), stop(0)]);
        assert_eq!(
            parsed("${10}^{$1}"),
            [text("${10}^{"), stop(1), text("}"), stop(0)]
        );
        assert_eq!(parsed("$10"), [stop(1), text("0"), stop(0)]);
        assert_eq!(parsed("${1"), [text("${1"), stop(0)]);
        assert_eq!(parsed(r"\\ \n $"), [text(r"\\ \n $"), stop(0)]);
    }

    #[test]
    fn multibyte_text_is_kept() {
        assert_eq!(
            parsed("α$1β${1}γ"),
            [
                text("α"),
                stop(1),
                text("β"),
                Chunk::Mirror(1),
                text("γ"),
                stop(0)
            ]
        );
        assert_eq!(parsed(r"${1:\é{ü}}"), [stop_with(1, r"\é{ü}"), stop(0)]);
    }

    #[test]
    fn errors() {
        assert_eq!(parse("${1:$2}"), Err(BodyError::DefaultHasDollar(1)));
        assert_eq!(parse("${1:a${$}}"), Err(BodyError::DefaultHasDollar(1)));
        assert!(matches!(parse("${1:abc"), Err(BodyError::Invalid(_))));
        assert!(matches!(parse(r"${1:\}"), Err(BodyError::Invalid(_))));
        assert!(matches!(parse("$0$0"), Err(BodyError::Invalid(_))));
        assert!(matches!(parse("$1${1:x}"), Err(BodyError::Invalid(_))));
        assert_eq!(
            parse_body("${2:$}"),
            Err("the default of $2 contains `$`".to_owned())
        );
    }

    #[test]
    fn with_indent_indents_continuation_lines() {
        let chunks = parsed("\\[\n  $1\n\\]\n$0");
        assert_eq!(
            with_indent(&chunks, "    "),
            [
                text("\\[\n      "),
                stop(1),
                text("\n    \\]\n    "),
                stop(0)
            ]
        );
    }

    #[test]
    fn with_indent_leaves_single_lines_and_mirrors_alone() {
        let chunks = parsed(r"\frac{$1}{$1}");
        assert_eq!(with_indent(&chunks, "\t"), chunks);
        assert_eq!(with_indent(&chunks, ""), chunks);
    }

    #[test]
    fn with_indent_skips_blank_lines_inside_a_chunk() {
        assert_eq!(
            with_indent(&[text("a\n\nb\n")], "  "),
            [text("a\n\n  b\n  ")]
        );
    }

    #[test]
    fn with_indent_indents_defaults() {
        assert_eq!(
            with_indent(&[stop_with(1, "x\ny")], "  "),
            [stop_with(1, "x\n  y")]
        );
    }
}
