//! Snippet model with LuaSnip semantics. Bodies are parsed into chunks; the GUI builds
//! GtkSourceSnippet objects chunk by chunk (never via Snippet::new_parsed).
//!
//! - [`SnippetDef`] is one `[[snippet]]` table of `snippets.toml`;
//! - [`parse_body`] turns a body into [`Chunk`]s and [`with_indent`] indents continuation lines;
//! - [`SnippetSet`] holds compiled snippets and decides which one a trigger expands
//!   ([`SnippetSet::auto_match`] for autosnippets, [`SnippetSet::manual`] for Tab and completion).

mod body;
mod def;
mod set;

pub use body::{Chunk, parse_body, with_indent};
pub use def::{Cond, SnippetDef};
pub use set::{Compiled, SnippetError, SnippetMatch, SnippetSet};
