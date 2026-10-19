//! LaTeX scanner (line states, tokens, context) and structure pass. No GTK.
//!
//! The modules are split by owner, and everything is re-exported at the crate root:
//! - scanner: `lex`, `tokens`, `context`, `state`;
//! - structure pass: `structure`, `magic`, `bib`, `aux`, `project`.

mod aux;
mod bib;
mod context;
mod lex;
mod magic;
mod project;
mod state;
mod structure;
mod tokens;

pub use aux::{AuxInfo, aux_inputs, parse_aux};
pub use bib::{BibEntry, scan_bib};
pub use context::{ArgKind, Context};
pub use lex::{Lines, ScanConfig, StrLines};
pub use magic::{Magic, parse_magic};
pub use project::ProjectIndex;
pub use state::LineStates;
pub use structure::{
    Citation, EnvPair, FileIndex, Include, IncludeKind, Label, MacroDef, OutlineItem, Reference,
    index_file, index_file_with_theorems,
};
pub use tokens::{Token, TokenKind};
