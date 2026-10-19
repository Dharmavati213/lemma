//! Agent messages as Pango markup (WP2.5): pulldown-cmark (no default features) → blocks for
//! selectable `gtk::Label`s, code blocks separately (monospace, with a copy button).
//!
//! # LaTeX stays verbatim
//!
//! Messages about a LaTeX document are full of backslashes, braces, `$`, `*` and `_`. Markdown
//! would eat some of them (`\{` → `{`, `\\` → `\`, `$x^*$ and $y^*$` → emphasis), so:
//!
//! - every backslash is doubled before parsing and halved again in raw text (code, math, HTML),
//!   so Markdown's backslash escapes never remove one: the text shows exactly what the agent
//!   wrote;
//! - `$…$` and `$$…$$` are parsed as math (no emphasis inside) and shown verbatim, never
//!   rendered; `\(…\)` and `\[…\]` are protected the same way (they are turned into marked
//!   `$`-math for the parser and back into themselves in the output);
//! - a single `~` is a tie (`$f$~and~$g$`, `Lemma~\ref{a}`), never strikethrough: only `~~…~~`
//!   strikes text through;
//! - Pango's special characters (`<`, `>`, `&`, quotes) are escaped everywhere, and a U+0000
//!   becomes U+FFFD (as CommonMark requires; GLib's C strings would cut the text there).
//!
//! The output is a list of [`Block`]s; [`render`] is pure and cheap (the panel calls it once per
//! frame for a message that is still streaming and updates only the blocks that changed).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::borrow::Cow;
use std::fmt::Write as _;

/// One block of a rendered message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Pango markup (a paragraph, a heading, an HTML block).
    Markup(String),
    /// A fenced or indented code block: its info string's first word (language) and verbatim
    /// text (without the final newline).
    Code { lang: Option<String>, text: String },
    /// One list item (or the part of an item before or after a nested list): its nesting depth
    /// (0 = top level), its marker (`•`, `◦`, `1.`; empty for the continuation of an item after
    /// a nested list) and its content as markup (paragraphs separated by a blank line). Task
    /// list items start with `☐ ` or `☑ `.
    Item {
        depth: u8,
        marker: String,
        markup: String,
    },
    /// A block quote, flattened to markup (paragraphs separated by a blank line, list items as
    /// `• ` lines, a table as one line per row with the header in bold, in its place).
    Quote(String),
    /// A table: the header cells and the body rows, as markup.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A thematic break.
    Rule,
}

/// Renders a Markdown message (see the module docs).
pub fn render(markdown: &str) -> Vec<Block> {
    let markdown = without_nul(markdown);
    let tildes = protect_tildes(&markdown);
    let protected = protect_math(&tildes);
    let source = double_backslashes(&protected);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let mut renderer = Renderer::default();
    for event in Parser::new_ext(&source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// Renders a message that grows while it streams, re-parsing only what changed. The text is cut
/// into pieces at blank lines outside code fences and HTML blocks that end only at their closer
/// (`<!--`, `<pre>`, `<script>`, …) that are followed by an unindented line (where a Markdown
/// block always ends) other than an ordered list item (which may continue the numbering of a
/// list above it), and the blocks of the pieces that did not change are reused; so a frame's
/// work stays proportional to the last paragraph, not to the whole answer. The result equals
/// [`render`] of the whole text (messages with link reference definitions, which reach across
/// pieces, are rendered whole).
#[derive(Debug, Default)]
pub struct Incremental {
    pieces: Vec<(String, Vec<Block>)>,
}

/// What changed since the last [`Incremental::update`].
#[derive(Debug, PartialEq, Eq)]
pub struct Update {
    /// The first `unchanged` blocks are the same as before.
    pub unchanged: usize,
    /// The blocks after them.
    pub tail: Vec<Block>,
}

impl Incremental {
    /// The blocks of `text` (see the type's docs).
    pub fn render(&mut self, text: &str) -> Vec<Block> {
        let Update { unchanged, tail } = self.update(text);
        let mut out: Vec<Block> = self
            .pieces
            .iter()
            .flat_map(|(_, blocks)| blocks.iter())
            .take(unchanged)
            .cloned()
            .collect();
        out.extend(tail);
        out
    }

    /// Renders `text` and says which blocks changed since the last call (none before the first
    /// call): a caller that keeps widgets for the blocks updates only the tail.
    ///
    /// While the text only grows (streaming), the complete pieces are compared (not re-scanned)
    /// and only the rest is cut and parsed, so a frame costs a `memcmp` of the old text plus the
    /// work on its last paragraph.
    pub fn update(&mut self, text: &str) -> Update {
        // The complete pieces (all but the last) that still start the text.
        let complete = self.pieces.len().saturating_sub(1);
        let mut kept = 0;
        let mut offset = 0;
        for (piece, _) in &self.pieces[..complete] {
            if !text[offset..].starts_with(piece.as_str()) {
                break;
            }
            offset += piece.len();
            kept += 1;
        }
        let rest = &text[offset..];
        if has_reference_definitions(rest) || (kept == 0 && has_reference_definitions(text)) {
            self.pieces.clear();
            return Update {
                unchanged: 0,
                tail: render(text),
            };
        }
        let pieces = split_pieces(rest);
        // The first piece of the rest may be the old last piece, unchanged.
        let extra = usize::from(
            kept == complete
                && self.pieces.len() > complete
                && pieces
                    .first()
                    .is_some_and(|p| *p == self.pieces[complete].0),
        );
        let same = kept + extra;
        let unchanged = self.pieces[..same].iter().map(|(_, b)| b.len()).sum();
        self.pieces.truncate(same);
        let mut tail = Vec::new();
        for piece in &pieces[extra..] {
            let blocks = render(piece);
            tail.extend(blocks.iter().cloned());
            self.pieces.push(((*piece).to_owned(), blocks));
        }
        Update { unchanged, tail }
    }
}

/// `text` cut after the blank lines (outside code fences and HTML blocks of CommonMark's types
/// 1–5) that precede an unindented line that does not start an ordered list item.
fn split_pieces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    let mut fence: Option<(char, usize)> = None;
    // The closer of an open HTML block that blank lines do not end.
    let mut html: Option<&'static str> = None;
    let mut after_blank = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let blank = line.trim().is_empty();
        if fence.is_none()
            && html.is_none()
            && after_blank
            && !blank
            && !line.starts_with([' ', '\t'])
            && !is_ordered_item(line)
            && !may_become_ordered_item(line)
            && offset > start
        {
            pieces.push(&text[start..offset]);
            start = offset;
        }
        if let Some(closer) = html {
            if line.to_ascii_lowercase().contains(closer) {
                html = None;
            }
        } else {
            let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
            let fence_len =
                fence_char.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
            match fence {
                Some((c, len))
                    if indent < 4
                        && fence_char == Some(c)
                        && fence_len >= len
                        && trimmed[fence_len..].trim().is_empty() =>
                {
                    fence = None;
                }
                None if indent < 4 && fence_len >= 3 => {
                    fence = fence_char.map(|c| (c, fence_len));
                }
                None if indent < 4 => html = open_html_block(trimmed),
                _ => {}
            }
        }
        after_blank = blank && fence.is_none() && html.is_none();
        offset += line.len();
    }
    if start < text.len() || pieces.is_empty() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// True when `line` starts with an ordered list marker (`1.`, `12)`) followed by a space, a tab
/// or the line's end.
fn is_ordered_item(line: &str) -> bool {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return false;
    }
    let rest = &line.as_bytes()[digits..];
    matches!(rest.first(), Some(b'.' | b')'))
        && matches!(rest.get(1), None | Some(b' ' | b'\t' | b'\n' | b'\r'))
}

/// True for the unfinished last line of a streaming text that may still turn into an ordered
/// list item (`1` before its `.` arrives): a cut before it would not be final.
fn may_become_ordered_item(line: &str) -> bool {
    !line.ends_with('\n')
        && (1..=9).contains(&line.len())
        && line.bytes().all(|b| b.is_ascii_digit())
}

/// When `line` (without its indentation) starts an HTML block that only its closer ends
/// (CommonMark's types 1–5: `<pre`, `<script`, `<style`, `<textarea`, `<!--`, `<?`, `<!X`,
/// `<![CDATA[`) and the closer is not on the same line: the closer, in lowercase.
fn open_html_block(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    let (opener, closer) = if lower.starts_with("<!--") {
        (4, "-->")
    } else if lower.starts_with("<![cdata[") {
        (9, "]]>")
    } else if lower.starts_with("<?") {
        (2, "?>")
    } else if lower.starts_with("<!")
        && lower.as_bytes().get(2).is_some_and(u8::is_ascii_alphabetic)
    {
        (2, ">")
    } else {
        let (tag, closer) = [
            ("<pre", "</pre>"),
            ("<script", "</script>"),
            ("<style", "</style>"),
            ("<textarea", "</textarea>"),
        ]
        .into_iter()
        .find(|(tag, _)| {
            lower.starts_with(tag)
                && matches!(
                    lower.as_bytes().get(tag.len()),
                    None | Some(b' ' | b'\t' | b'>' | b'\n' | b'\r')
                )
        })?;
        (tag.len(), closer)
    };
    (!lower[opener..].contains(closer)).then_some(closer)
}

/// True when `text` has a line like `[label]: destination` (a link reference definition).
fn has_reference_definitions(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('[') && line.contains("]:")
    })
}

/// Escapes `text` for Pango markup (a U+0000 becomes U+FFFD: GLib reads C strings).
pub fn escape(text: &str) -> String {
    glib::markup_escape_text(&without_nul(text)).to_string()
}

/// `text` with every U+0000 replaced by U+FFFD, which GLib's C strings (labels, markup) can
/// hold: a NUL would end the text there in a release build and panic in a debug one.
pub fn without_nul(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', "\u{FFFD}"))
    } else {
        Cow::Borrowed(text)
    }
}

// ----- the tie protection pre-pass -----

/// Marks a single `~` (a LaTeX tie), so that the parser's single-tilde strikethrough leaves it
/// alone; [`restore_math`] turns it back.
const TIE: char = '\u{E004}';

/// Replaces every `~` that is not next to another `~` by [`TIE`]: `~~…~~` still strikes text
/// through, and fences (`~~~`) are runs too. Text that already contains [`TIE`] is left alone.
fn protect_tildes(src: &str) -> Cow<'_, str> {
    if !src.contains('~') || src.contains(TIE) {
        return Cow::Borrowed(src);
    }
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let lone = c == '~'
            && (i == 0 || chars[i - 1] != '~')
            && chars.get(i + 1).is_none_or(|next| *next != '~');
        out.push(if lone { TIE } else { c });
    }
    Cow::Owned(out)
}

// ----- the math protection pre-pass -----

/// Marks `\(` (as `$` + this), …
const OPEN_INLINE: char = '\u{E000}';
/// … `\)` (this + `$`), …
const CLOSE_INLINE: char = '\u{E001}';
/// … `\[` (`$$` + this) …
const OPEN_DISPLAY: char = '\u{E002}';
/// … and `\]` (this + `$$`).
const CLOSE_DISPLAY: char = '\u{E003}';

/// Rewrites `\(…\)` as `$⟨E000⟩…⟨E001⟩$` and `\[…\]` as `$$⟨E002⟩…⟨E003⟩$$` outside fenced code
/// blocks and code spans, so that the parser treats them as math; [`restore_math`] turns them
/// back. A delimiter preceded by another backslash (`\\(` is a line break and a parenthesis) does
/// not count, and an opener without a closer in the same paragraph stays as it is. Text that
/// already contains one of the marker characters is left alone.
fn protect_math(src: &str) -> Cow<'_, str> {
    if !src.contains(['\\'])
        || src.contains([OPEN_INLINE, CLOSE_INLINE, OPEN_DISPLAY, CLOSE_DISPLAY])
    {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len() + 16);
    let mut fence: Option<(char, usize)> = None;
    let mut paragraph = String::new();
    for line in src.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let fence_len = fence_char.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        match fence {
            Some((c, len)) => {
                out.push_str(line);
                if indent < 4
                    && fence_char == Some(c)
                    && fence_len >= len
                    && trimmed[fence_len..].trim().is_empty()
                {
                    fence = None;
                }
                continue;
            }
            None if indent < 4 && fence_len >= 3 => {
                out.push_str(&protect_paragraph(&std::mem::take(&mut paragraph)));
                out.push_str(line);
                fence = fence_char.map(|c| (c, fence_len));
                continue;
            }
            None => {}
        }
        if line.trim().is_empty() {
            out.push_str(&protect_paragraph(&std::mem::take(&mut paragraph)));
            out.push_str(line);
        } else {
            paragraph.push_str(line);
        }
    }
    out.push_str(&protect_paragraph(&paragraph));
    Cow::Owned(out)
}

/// [`protect_math`] for one paragraph (no blank lines, no fences).
fn protect_paragraph(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                // A code span: a run of n backticks up to the next run of exactly n.
                let run = run_len(bytes, i, b'`');
                i = match find_backtick_run(bytes, i + run, run) {
                    Some(end) => end + run,
                    None => i + run,
                };
            }
            b'\\' => {
                let run = run_len(bytes, i, b'\\');
                let at = i + run - 1;
                let next = bytes.get(at + 1).copied();
                if run % 2 == 1 && matches!(next, Some(b'(' | b'[')) {
                    let display = next == Some(b'[');
                    let close = if display { b']' } else { b')' };
                    if let Some(end) = find_closer(bytes, at + 2, close) {
                        out.push_str(&text[copied..at]);
                        let (open, shut) = if display {
                            ("$$\u{E002}", "\u{E003}$$")
                        } else {
                            ("$\u{E000}", "\u{E001}$")
                        };
                        out.push_str(open);
                        out.push_str(&text[at + 2..end]);
                        out.push_str(shut);
                        i = end + 2;
                        copied = i;
                        continue;
                    }
                }
                i += run;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

fn run_len(bytes: &[u8], at: usize, b: u8) -> usize {
    bytes[at..].iter().take_while(|x| **x == b).count()
}

/// The start of the next run of exactly `len` backticks at or after `from`.
fn find_backtick_run(bytes: &[u8], from: usize, len: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = run_len(bytes, i, b'`');
            if run == len {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// The position of the backslash of the next unescaped `\)` / `\]` (`close`) at or after
/// `from`.
fn find_closer(bytes: &[u8], from: usize, close: u8) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let run = run_len(bytes, i, b'\\');
            let at = i + run - 1;
            if run % 2 == 1 && bytes.get(at + 1) == Some(&close) {
                return Some(at);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// Undoes [`protect_math`] and [`protect_tildes`] in rendered text.
fn restore_math(text: &str) -> Cow<'_, str> {
    if !text.contains([OPEN_INLINE, CLOSE_INLINE, OPEN_DISPLAY, CLOSE_DISPLAY, TIE]) {
        return Cow::Borrowed(text);
    }
    let text = text
        .replace("$$\u{E002}", "\\[")
        .replace("\u{E003}$$", "\\]")
        .replace("$\u{E000}", "\\(")
        .replace("\u{E001}$", "\\)")
        .replace(OPEN_DISPLAY, "\\[")
        .replace(CLOSE_DISPLAY, "\\]")
        .replace(OPEN_INLINE, "\\(")
        .replace(CLOSE_INLINE, "\\)")
        .replace(TIE, "~");
    Cow::Owned(text)
}

/// Doubles every backslash, so that Markdown's escapes give back the original text.
fn double_backslashes(text: &str) -> Cow<'_, str> {
    if text.contains('\\') {
        Cow::Owned(text.replace('\\', "\\\\"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Undoes [`double_backslashes`] in raw text (code, math, HTML), which the parser leaves alone.
fn halve_backslashes(text: &str) -> Cow<'_, str> {
    if text.contains("\\\\") {
        Cow::Owned(text.replace("\\\\", "\\"))
    } else {
        Cow::Borrowed(text)
    }
}

// ----- the renderer -----

/// An open list.
struct List {
    /// The next number of an ordered list.
    next: Option<u64>,
}

/// An open list item.
struct Item {
    depth: u8,
    marker: String,
    markup: String,
    /// The item (or its first part) was emitted already, because a nested list or a code block
    /// followed it.
    emitted: bool,
}

/// A table being collected.
#[derive(Default)]
struct TableState {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// The row being read.
    row: Vec<String>,
}

#[derive(Default)]
struct Renderer {
    blocks: Vec<Block>,
    /// The paragraph, heading or table cell being built.
    leaf: String,
    lists: Vec<List>,
    items: Vec<Item>,
    /// Depth of block quotes, and the markup of the outermost one.
    quote_depth: usize,
    quote: String,
    /// Inside a block quote: the next leaf is a list item line, and whether the last one was.
    quote_item: bool,
    quote_prev_item: bool,
    /// The code block being collected: its language and text.
    code: Option<(Option<String>, String)>,
    /// An HTML block being collected.
    html: Option<String>,
    /// The table being collected.
    table: Option<TableState>,
    /// A link's or image's alt text is being collected (images show their alt text in italics).
    image_depth: usize,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        if let Some((_, text)) = &mut self.code {
            match event {
                Event::Text(t) => text.push_str(&halve_backslashes(&t)),
                Event::End(TagEnd::CodeBlock) => self.end_code_block(),
                _ => {}
            }
            return;
        }
        if let Some(html) = &mut self.html {
            match event {
                Event::Html(t) | Event::Text(t) => html.push_str(&halve_backslashes(&t)),
                Event::End(TagEnd::HtmlBlock) => {
                    let text = self.html.take().unwrap_or_default();
                    let text = text.trim_end_matches('\n');
                    self.leaf.push_str(&escape(text));
                    self.flush_leaf();
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.leaf.push_str(&escape(&t)),
            Event::Code(t) => {
                let _ = write!(
                    self.leaf,
                    "<span font_family=\"monospace\" background=\"#888888\" bgalpha=\"22%\">{}</span>",
                    escape(&halve_backslashes(&t))
                );
            }
            Event::InlineMath(t) => {
                let _ = write!(self.leaf, "${}$", escape(&halve_backslashes(&t)));
            }
            Event::DisplayMath(t) => {
                let _ = write!(self.leaf, "$${}$$", escape(&halve_backslashes(&t)));
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                self.leaf.push_str(&escape(&halve_backslashes(&t)));
            }
            Event::FootnoteReference(t) => {
                let _ = write!(self.leaf, "[^{}]", escape(&t));
            }
            Event::SoftBreak | Event::HardBreak => self.leaf.push('\n'),
            Event::Rule => {
                self.flush_leaf();
                if self.quote_depth > 0 {
                    self.push_quote("―――", "\n\n");
                } else {
                    self.emit_open_item();
                    self.blocks.push(Block::Rule);
                }
            }
            Event::TaskListMarker(done) => {
                self.leaf.push_str(if done { "☑ " } else { "☐ " });
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_leaf();
                let size = match level {
                    HeadingLevel::H1 => " size=\"x-large\"",
                    HeadingLevel::H2 => " size=\"large\"",
                    _ => "",
                };
                let _ = write!(self.leaf, "<span weight=\"bold\"{size}>");
            }
            Tag::BlockQuote(_) => {
                self.flush_leaf();
                if self.quote_depth == 0 {
                    self.emit_open_item();
                }
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_leaf();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|l| !l.is_empty())
                        .map(|l| restore_math(&halve_backslashes(l)).into_owned()),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush_leaf();
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.flush_leaf();
                // A nested list ends the visible part of the item that contains it.
                self.emit_open_item();
                self.lists.push(List { next: start });
            }
            Tag::Item => {
                self.flush_leaf();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut().and_then(|l| l.next.as_mut()) {
                    Some(n) => {
                        let marker = format!("{n}.");
                        *n += 1;
                        marker
                    }
                    None => ["•", "◦", "▪"][depth.min(2)].to_owned(),
                };
                if self.quote_depth > 0 {
                    let indent = "  ".repeat(depth);
                    let _ = write!(self.leaf, "{indent}{marker} ");
                    self.quote_item = true;
                } else {
                    self.items.push(Item {
                        depth: u8::try_from(depth).unwrap_or(u8::MAX),
                        marker,
                        markup: String::new(),
                        emitted: false,
                    });
                }
            }
            Tag::Table(_) => {
                self.flush_leaf();
                self.emit_open_item();
                self.table = Some(TableState::default());
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.leaf.push_str("<i>"),
            Tag::Strong => self.leaf.push_str("<b>"),
            Tag::Strikethrough => self.leaf.push_str("<s>"),
            Tag::Superscript => self.leaf.push_str("<sup>"),
            Tag::Subscript => self.leaf.push_str("<sub>"),
            Tag::Link { dest_url, .. } => {
                let _ = write!(
                    self.leaf,
                    "<a href=\"{}\">",
                    escape(&restore_math(&dest_url))
                );
            }
            Tag::Image { .. } => {
                self.image_depth += 1;
                self.leaf.push_str("<i>");
            }
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.flush_leaf(),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_leaf(),
            TagEnd::Heading(_) => {
                self.leaf.push_str("</span>");
                self.flush_leaf();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_leaf();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    self.quote_prev_item = false;
                    let quote = std::mem::take(&mut self.quote);
                    if !quote.is_empty() {
                        self.blocks
                            .push(Block::Quote(restore_math(&quote).into_owned()));
                    }
                }
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::HtmlBlock => {}
            TagEnd::List(_) => {
                self.flush_leaf();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_leaf();
                self.quote_item = false;
                if self.quote_depth == 0
                    && let Some(item) = self.items.pop()
                    && (!item.emitted || !item.markup.is_empty())
                {
                    self.blocks.push(Block::Item {
                        depth: item.depth,
                        marker: if item.emitted {
                            String::new()
                        } else {
                            item.marker
                        },
                        markup: restore_math(&item.markup).into_owned(),
                    });
                }
            }
            TagEnd::Table => {
                let Some(table) = self.table.take() else {
                    return;
                };
                if self.quote_depth > 0 {
                    // Inside a quote the table is part of the quote's text, in its place: the
                    // header in bold, a row per line, cells separated by a bar.
                    let mut lines = Vec::with_capacity(table.rows.len() + 1);
                    if !table.header.is_empty() {
                        let header: Vec<String> =
                            table.header.iter().map(|h| format!("<b>{h}</b>")).collect();
                        lines.push(header.join(" │ "));
                    }
                    lines.extend(table.rows.iter().map(|row| row.join(" │ ")));
                    self.quote_prev_item = false;
                    self.push_quote(&lines.join("\n"), "\n\n");
                } else {
                    self.blocks.push(Block::Table {
                        header: table.header,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = restore_math(&std::mem::take(&mut self.leaf)).into_owned();
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis => self.leaf.push_str("</i>"),
            TagEnd::Strong => self.leaf.push_str("</b>"),
            TagEnd::Strikethrough => self.leaf.push_str("</s>"),
            TagEnd::Superscript => self.leaf.push_str("</sup>"),
            TagEnd::Subscript => self.leaf.push_str("</sub>"),
            TagEnd::Link => self.leaf.push_str("</a>"),
            TagEnd::Image => {
                self.image_depth = self.image_depth.saturating_sub(1);
                self.leaf.push_str("</i>");
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => self.flush_leaf(),
        }
    }

    fn end_code_block(&mut self) {
        let Some((lang, text)) = self.code.take() else {
            return;
        };
        let text = restore_math(text.strip_suffix('\n').unwrap_or(&text)).into_owned();
        if self.quote_depth > 0 {
            let markup = format!("<tt>{}</tt>", escape(&text));
            self.push_quote(&markup, "\n\n");
            return;
        }
        self.emit_open_item();
        self.blocks.push(Block::Code { lang, text });
    }

    /// Emits the open list item's content so far (before a nested list, a code block, a table
    /// or a quote), so blocks keep their order. Later content of the item is a continuation.
    fn emit_open_item(&mut self) {
        if self.quote_depth > 0 {
            return;
        }
        if let Some(item) = self.items.last_mut()
            && (!item.emitted || !item.markup.is_empty())
        {
            let marker = if item.emitted {
                String::new()
            } else {
                item.marker.clone()
            };
            self.blocks.push(Block::Item {
                depth: item.depth,
                marker,
                markup: restore_math(&std::mem::take(&mut item.markup)).into_owned(),
            });
            item.emitted = true;
        }
    }

    /// Moves the finished leaf to its container: the block quote, the open list item, or the
    /// list of blocks.
    fn flush_leaf(&mut self) {
        if self.table.is_some() {
            // Cells are collected at their end.
            return;
        }
        let leaf = std::mem::take(&mut self.leaf);
        let trimmed = leaf.trim_end_matches('\n');
        if trimmed.is_empty() {
            return;
        }
        if self.quote_depth > 0 {
            let sep = if self.quote_item && self.quote_prev_item {
                "\n"
            } else {
                "\n\n"
            };
            self.quote_prev_item = self.quote_item;
            self.push_quote(trimmed, sep);
        } else if let Some(item) = self.items.last_mut() {
            if !item.markup.is_empty() {
                item.markup.push_str("\n\n");
            }
            item.markup.push_str(trimmed);
        } else {
            self.blocks
                .push(Block::Markup(restore_math(trimmed).into_owned()));
        }
    }

    fn push_quote(&mut self, markup: &str, sep: &str) {
        if !self.quote.is_empty() {
            self.quote.push_str(sep);
        }
        self.quote.push_str(markup);
    }

    fn finish(mut self) -> Vec<Block> {
        if let Some((lang, text)) = self.code.take() {
            // An unterminated fence at the end of a streaming message.
            self.blocks.push(Block::Code {
                lang,
                text: restore_math(text.strip_suffix('\n').unwrap_or(&text)).into_owned(),
            });
        }
        self.flush_leaf();
        while self.quote_depth > 0 {
            self.end(TagEnd::BlockQuote(None));
        }
        self.blocks
    }
}

/// Plain text of a block's markup (tags removed, entities decoded), for tests and accessibility.
pub fn plain_text(markup: &str) -> String {
    match pango::parse_markup(&without_links(markup), '\0') {
        Ok((_, text, _)) => text.to_string(),
        Err(_) => markup.to_owned(),
    }
}

/// True when `markup` is valid label markup: Pango markup plus GtkLabel's `<a href="…">` links
/// (a label logs a warning for invalid markup).
pub fn is_valid_markup(markup: &str) -> bool {
    pango::parse_markup(&without_links(markup), '\0').is_ok()
}

/// `markup` without its `<a …>` and `</a>` tags (GtkLabel handles those before Pango sees the
/// text). Attribute values are escaped, so a tag ends at the first `>`.
fn without_links(markup: &str) -> Cow<'_, str> {
    if !markup.contains("<a") && !markup.contains("</a>") {
        return Cow::Borrowed(markup);
    }
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let is_link =
            tail.starts_with("<a ") || tail.starts_with("<a>") || tail.starts_with("</a>");
        match (is_link, tail.find('>')) {
            (true, Some(end)) => rest = &tail[end + 1..],
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(text: &str) -> Vec<Block> {
        let blocks = render(text);
        for block in &blocks {
            let markups: Vec<&String> = match block {
                Block::Markup(m) | Block::Quote(m) | Block::Item { markup: m, .. } => vec![m],
                Block::Table { header, rows } => {
                    header.iter().chain(rows.iter().flatten()).collect()
                }
                Block::Code { .. } | Block::Rule => vec![],
            };
            for m in markups {
                assert!(is_valid_markup(m), "invalid markup {m:?} from {text:?}");
            }
        }
        blocks
    }

    fn m(s: &str) -> Block {
        Block::Markup(s.into())
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        assert_eq!(
            md("# Title\n\nSome *emphasis*, **strong**, ~~gone~~ and `code`.\n\n### Small"),
            [
                m("<span weight=\"bold\" size=\"x-large\">Title</span>"),
                m(
                    "Some <i>emphasis</i>, <b>strong</b>, <s>gone</s> and <span \
                   font_family=\"monospace\" background=\"#888888\" bgalpha=\"22%\">code</span>."
                ),
                m("<span weight=\"bold\">Small</span>"),
            ]
        );
        assert_eq!(
            md("## Two"),
            [m("<span weight=\"bold\" size=\"large\">Two</span>")]
        );
    }

    #[test]
    fn escaping_of_pango_characters_and_dollars() {
        assert_eq!(md("a < b & c > d"), [m("a &lt; b &amp; c &gt; d")]);
        assert_eq!(md("It costs $5 & more"), [m("It costs $5 &amp; more")]);
        assert_eq!(md("$a<b$ and $$x & y$$"), [m("$a&lt;b$ and $$x &amp; y$$")]);
        assert_eq!(md("`<tt> & </tt>`").len(), 1);
        assert_eq!(
            plain_text(match &md("`<tt> & $x$`")[0] {
                Block::Markup(s) => s,
                b => panic!("{b:?}"),
            }),
            "<tt> & $x$"
        );
        // Raw HTML is shown, not interpreted.
        assert_eq!(md("x <b>bold</b> y"), [m("x &lt;b&gt;bold&lt;/b&gt; y")]);
        assert_eq!(
            md("<div>\nhi\n</div>"),
            [m("&lt;div&gt;\nhi\n&lt;/div&gt;")]
        );
    }

    #[test]
    fn latex_is_verbatim() {
        let cases = [
            r"Use \{ and \} for braces, \\ for a line break and \_ for an underscore.",
            r"Inline $x^*$ and $y^*$, also \(a*b*c\) and \(\alpha_1, \beta_2\).",
            r"The sum $\sum_{i=1}^n a_i$ is \emph{finite}.",
            r"An escaped \$ stays, as does \% and \#.",
            r"Write \textbf{bold} or \[ x * y * z \] inline.",
        ];
        for case in cases {
            let blocks = md(case);
            assert_eq!(blocks.len(), 1, "{case}: {blocks:?}");
            let Block::Markup(markup) = &blocks[0] else {
                panic!("{blocks:?}");
            };
            assert_eq!(plain_text(markup), case, "markup {markup:?}");
        }
        // Display math on its own lines.
        let text = "Consider\n\\[\n  \\int_0^1 f(x)\\,dx = 1 * 2 * 3\n\\]\nwhich holds.";
        let blocks = md(text);
        let Block::Markup(markup) = &blocks[0] else {
            panic!("{blocks:?}");
        };
        assert_eq!(plain_text(markup), text);
        // `\\(` is a line break and a parenthesis, not math.
        assert_eq!(
            plain_text(match &md(r"a \\(b*c*) d")[0] {
                Block::Markup(s) => s,
                b => panic!("{b:?}"),
            }),
            r"a \\(b<i>c</i>) d".replace("<i>", "").replace("</i>", "")
        );
    }

    #[test]
    fn code_blocks_are_verbatim_with_language() {
        let text = "Here:\n\n```latex\n\\begin{align}\n  a &= b \\\\\n  \\(c\\) & < d\n\\end{align}\n```\n\nDone.";
        assert_eq!(
            md(text),
            [
                m("Here:"),
                Block::Code {
                    lang: Some("latex".into()),
                    text: "\\begin{align}\n  a &= b \\\\\n  \\(c\\) & < d\n\\end{align}".into()
                },
                m("Done."),
            ]
        );
        assert_eq!(
            md("    indented \\code\n"),
            [Block::Code {
                lang: None,
                text: "indented \\code".into()
            }]
        );
        // An unterminated fence while streaming.
        assert_eq!(
            md("```\npartial \\x"),
            [Block::Code {
                lang: None,
                text: "partial \\x".into()
            }]
        );
        // `\(` inside a code span is not math.
        assert_eq!(
            plain_text(match &md(r"see `\(x\)` here")[0] {
                Block::Markup(s) => s,
                b => panic!("{b:?}"),
            }),
            r"see \(x\) here"
        );
    }

    #[test]
    fn lists_nested_ordered_and_tasks() {
        let text = "- one\n- two\n  - inner *a*\n  - inner b\n- three\n\n3. third\n4. fourth\n\n- [ ] todo\n- [x] done";
        let item = |depth: u8, marker: &str, markup: &str| Block::Item {
            depth,
            marker: marker.into(),
            markup: markup.into(),
        };
        assert_eq!(
            md(text),
            [
                item(0, "•", "one"),
                item(0, "•", "two"),
                item(1, "◦", "inner <i>a</i>"),
                item(1, "◦", "inner b"),
                item(0, "•", "three"),
                item(0, "3.", "third"),
                item(0, "4.", "fourth"),
                item(0, "•", "☐ todo"),
                item(0, "•", "☑ done"),
            ]
        );
        // A loose item with two paragraphs and a code block.
        let text =
            "1. first para\n\n   second para\n\n   ```\n   code\n   ```\n\n   after\n2. next";
        assert_eq!(
            md(text),
            [
                item(0, "1.", "first para\n\nsecond para"),
                Block::Code {
                    lang: None,
                    text: "code".into()
                },
                item(0, "", "after"),
                item(0, "2.", "next"),
            ]
        );
    }

    #[test]
    fn links_quotes_tables_and_rules() {
        assert_eq!(
            md("See [the docs](https://example.org/a?b=1&c=\"2\") now."),
            [m(
                "See <a href=\"https://example.org/a?b=1&amp;c=&quot;2&quot;\">the docs</a> now."
            )]
        );
        assert_eq!(
            md("<https://x.org>"),
            [m("<a href=\"https://x.org\">https://x.org</a>")]
        );
        assert_eq!(
            md("> quoted *text*\n>\n> - a\n> - b"),
            [Block::Quote("quoted <i>text</i>\n\n• a\n• b".into())]
        );
        assert_eq!(
            md("| a | b |\n|---|---|\n| 1 | $x<y$ |\n| 2 | **3** |"),
            [Block::Table {
                header: vec!["a".into(), "b".into()],
                rows: vec![
                    vec!["1".into(), "$x&lt;y$".into()],
                    vec!["2".into(), "<b>3</b>".into()]
                ],
            }]
        );
        assert_eq!(
            md("above\n\n---\n\nbelow"),
            [m("above"), Block::Rule, m("below")]
        );
        assert_eq!(md("![alt text](x.png)"), [m("<i>alt text</i>")]);
    }

    #[test]
    fn breaks_and_empty_input() {
        assert!(md("").is_empty());
        assert!(md("\n\n").is_empty());
        assert_eq!(md("line one\nline two"), [m("line one\nline two")]);
        assert_eq!(md("hard  \nbreak"), [m("hard\nbreak")]);
    }

    #[test]
    fn streaming_prefixes_are_valid() {
        let text = "# Plan\n\nI will **fix** the `\\frac` in $\\alpha_1 < \\beta$:\n\n1. edit \
                    [main.tex](main.tex)\n2. compile\n\n```latex\n\\begin{equation}\nx & y\n\\end{equation}\n```\n\
                    > note\n\n| a | b |\n|--|--|\n| c | d |\n";
        let mut end = 0;
        while end <= text.len() {
            if text.is_char_boundary(end) {
                md(&text[..end]);
            }
            end += 1;
        }
    }

    /// The per-frame cost of re-rendering a long streaming answer (ARCHITECTURE §7: a frame's
    /// main-thread work stays well under 8 ms): the whole 100 kB message once, then a frame after
    /// each new chunk. Run with `cargo test --release -- --ignored perf`.
    #[test]
    #[ignore]
    fn perf_render_long_message() {
        let paragraph = "Let $f\\colon X \\to Y$ be **proper** and `\\pi_*` exact; then \\(R^i f_* F = 0\\) \
                         for $i > 0$ by [the lemma](main.tex:12).\n\n- one\n- two\n\n```latex\n\\begin{align}\n a &= b\n\\end{align}\n```\n\n";
        let mut text = paragraph.repeat(100_000 / paragraph.len() + 1);
        let t = std::time::Instant::now();
        let whole = render(&text);
        let full = t.elapsed();
        assert!(whole.len() > 1000);
        let mut incremental = Incremental::default();
        incremental.render(&text);
        let mut frames = Vec::new();
        for i in 0..200 {
            text.push_str(if i % 20 == 19 {
                "\n\n"
            } else {
                "more words, $x_i$ and `y`. "
            });
            let t = std::time::Instant::now();
            let update = incremental.update(&text);
            frames.push(t.elapsed());
            assert!(update.unchanged + update.tail.len() >= whole.len());
            assert!(update.tail.len() <= 2, "only the last piece changes");
        }
        frames.sort();
        let p99 = frames[frames.len() * 99 / 100];
        println!(
            "100 kB: whole render {full:?}; per frame while streaming p99 {p99:?}, max {:?}",
            frames.last().unwrap()
        );
        assert!(p99 < std::time::Duration::from_millis(2), "{p99:?}");
    }

    #[test]
    fn incremental_equals_whole() {
        let texts = [
            "# Plan\n\nFirst *para*.\n\nSecond with $x$.\n",
            "1. one\n\n2. two\n\n   indented continuation\n\n3. three\n",
            "- a\n- b\n\n- c\n\n  - nested\n\nafter\n",
            "Text\n\n```latex\n\\begin{align}\n\na &= b\n\n\\end{align}\n```\n\nDone.\n",
            "> q1\n\n> q2\n>\n> q3\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\nend",
            "    code\n\n    more\n\nplain\n\n~~~\nfenced\n\n~~~\n",
            "See [x][r].\n\n[r]: https://example.org\n",
            "Display\n\\[\n a\n\\]\n\nand \\(b\\) and $$c$$.\n\n<div>\n\nhtml\n\n</div>\n",
            // A loose list numbered `1.` throughout, and HTML blocks across blank lines.
            "1. one\n\n1. two\n\n1. three\n\nafter\n\n7) x\n\n7) y\n",
            "<pre>\nx\n\ny\n</pre>\n\nnext\n\n<!-- a comment\n\nspanning -->\n\nend <?x\n\n?>\n",
            "<script type=\"x\">\na\n\nb\n</script>\n\n<!DOCTYPE\n\nhtml>\n\nlast\n",
        ];
        for text in texts {
            let mut incremental = Incremental::default();
            let mut previous: Vec<Block> = Vec::new();
            let mut end = 0;
            while end <= text.len() {
                if text.is_char_boundary(end) {
                    let prefix = &text[..end];
                    let Update { unchanged, tail } = incremental.update(prefix);
                    let mut rebuilt = previous[..unchanged].to_vec();
                    rebuilt.extend(tail);
                    let whole = render(prefix);
                    assert_eq!(rebuilt, whole, "{prefix:?}");
                    previous = whole;
                }
                end += 1;
            }
        }
        // Any change, not only appending: shrinking, editing the middle, replacing. The blocks
        // the update reports as unchanged plus its tail are the new text's blocks.
        let mut incremental = Incremental::default();
        let mut previous: Vec<Block> = Vec::new();
        for text in [
            texts[0],
            texts[1],
            &texts[1][..10],
            "# Plan\n\nFirst *para*.\n\nChanged middle.\n\nThird.\n",
            "# Plan\n\nFirst *para*.\n\nSecond with $x$.\n",
            "",
            texts[6],
            texts[3],
        ] {
            let Update { unchanged, tail } = incremental.update(text);
            let whole = render(text);
            let mut rebuilt = previous[..unchanged].to_vec();
            rebuilt.extend(tail);
            assert_eq!(rebuilt, whole, "{text:?}");
            assert_eq!(incremental.render(text), whole, "{text:?}");
            previous = whole;
        }
        assert_eq!(
            split_pieces("a\n\nb\n\n c\n\nd"),
            ["a\n\n", "b\n\n c\n\n", "d"]
        );
        assert_eq!(split_pieces(""), [""]);
    }

    #[test]
    fn ties_nul_and_tables_in_quotes() {
        // A single `~` is a LaTeX tie, never strikethrough; `~~…~~` strikes through.
        assert_eq!(
            md("So $f$~and~$g$, by Lemmas~\\ref{a}~and~\\ref{b}."),
            [m("So $f$~and~$g$, by Lemmas~\\ref{a}~and~\\ref{b}.")]
        );
        let Block::Markup(markup) = &md("`a~b` and )~or~( and ~~gone~~")[0] else {
            panic!()
        };
        assert!(markup.contains("<s>gone</s>"), "{markup}");
        assert_eq!(plain_text(markup), "a~b and )~or~( and gone");
        assert_eq!(
            md("~~~\na ~ b\n~~~"),
            [Block::Code {
                lang: None,
                text: "a ~ b".into()
            }]
        );
        // A NUL never reaches GLib.
        assert_eq!(md("a\0b"), [m("a\u{FFFD}b")]);
        assert_eq!(escape("x\0<"), "x\u{FFFD}&lt;");
        // A table in a quote stays in its place, inside the quote.
        assert_eq!(
            md("> intro\n>\n> | a | b |\n> |---|---|\n> | 1 | 2 |\n>\n> outro"),
            [Block::Quote(
                "intro\n\n<b>a</b> │ <b>b</b>\n1 │ 2\n\noutro".into()
            )]
        );
    }

    #[test]
    fn marker_characters_in_the_input_disable_protection() {
        let text = "odd \u{E000} char and \\(x\\)";
        let Block::Markup(markup) = &md(text)[0] else {
            panic!()
        };
        assert!(plain_text(markup).contains("\\(x\\)"));
    }
}
