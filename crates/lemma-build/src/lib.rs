//! The build pipeline: root-document and engine detection, the latexmk runner, the build queue,
//! and `.log`/`.blg` parsing.
//!
//! Everything here is GUI-free. [`Build::wait`] blocks, so the app runs each build on its own
//! thread and sends the [`BuildOutcome`] back to the main thread.

mod blg;
// M3 (WP3.7): chktex lint and plain-English error explanations (stubs created by WP3.0).
pub mod chktex;
mod detect;
pub mod explain;
mod paths;
mod pdf_copy;
pub mod preamble;
mod process;
mod queue;
mod request;
mod runaway;
mod texlog;

pub use blg::parse_blg;
pub use detect::{Engine, RootGuess, RootSource, detect_engine, detect_root};
pub use pdf_copy::copy_pdf_beside_root;
pub use process::{Build, BuildOutcome, BuildStatus, Canceller, PdfStamp};
pub use queue::BuildQueue;
pub use request::{BuildRequest, clean_command_line, command_line};
pub use runaway::{locate_runaway, locate_runaways};
pub use texlog::{LogReport, parse_log};
