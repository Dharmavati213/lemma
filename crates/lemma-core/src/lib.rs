//! Shared GUI-free types for Lemma.
//!
//! Conventions used throughout the workspace:
//! - text positions are **UTF-8 byte offsets** into the document text;
//! - lines are 0-based here and 1-based only in the UI, diagnostics and MCP;
//! - a line ends at `'\n'`, so a text has `'\n'`-count + 1 lines (the same as `GtkTextBuffer`).

mod diagnostic;
mod edit;
mod hash;
mod latest_slot;
mod line_index;
#[cfg(windows)]
mod poppler_exports;
pub mod process;
mod project_root;

pub use diagnostic::{DiagSource, Diagnostic, Severity};
pub use edit::{Origin, TextEdit, apply_all};
pub use hash::fnv1a64;
pub use latest_slot::LatestSlot;
pub use line_index::LineIndex;
pub use project_root::{PathError, ProjectRoot, canonicalize};
