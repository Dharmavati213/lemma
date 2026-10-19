//! Project search and replace. GUI-free: the Search page of the sidebar
//! (`lemma-app/src/search_panel.rs`) drives it.
//!
//! - **Matching** ([`Query`], [`Matcher`]): plain text or a regular expression (the `regex`
//!   crate), with case and whole-word options, **line by line** with `rg`'s semantics (see
//!   [`query`]): the results equal `rg [-F] [-i] [-w]` on the same files (tested on the
//!   fixtures).
//! - **Scope** ([`Scope`]): the project graph's files first (the caller's include order), then
//!   the other `.tex`, `.bib`, `.sty`, `.cls` and `.md` files under the root ([`EXTENSIONS`]),
//!   skipping hidden directories (`.git`, `.build`), symbolic links and the caller's
//!   `skip_dirs`; binary files (a NUL byte) and files over [`Limits::max_file_bytes`] are
//!   skipped. Open documents are searched in their **live text**, which the caller passes in
//!   ([`Scope::live`]), and the disk for the rest (UTF-8, else ISO-8859-15, as the editor reads
//!   files).
//! - **Results** ([`Searcher::start`]) stream from a thread of their own in batches over a
//!   bounded channel ([`Event`]); a new search of the same [`Searcher`] cancels the previous one
//!   (the latest wins), and so does [`Search::cancel`] or dropping the receiver. Nothing runs
//!   (no thread exists) until the first search.
//! - **Replace** ([`plan_replace`], [`Template`]): the edits per file, one per line with
//!   matches, with capture groups for regular expressions; the app applies them as reviewable
//!   hunks (origin "Replace"). [`preview`] gives the replacement of each match for the replace
//!   preview.

pub mod query;
pub mod replace;
pub mod search;

pub use query::{LineHits, Matcher, Query, QueryError};
pub use replace::{Hit, Replacement, Template, apply, plan_replace, preview};
pub use search::{
    EXTENSIONS, Event, FileMatches, Limits, LineMatch, Scope, Search, Searcher, Summary, decode,
    has_search_extension,
};
