//! SyncTeX for Lemma: a safe wrapper over the system libsynctex 2.0.
//!
//! [`Scanner::forward`] maps a source line to boxes in the PDF, and [`Scanner::inverse`] maps a
//! point in the PDF back to a source line. A `Scanner` is owned by one thread at a time (the PDF
//! thread) and is reopened whenever the `.synctex.gz` file changes.
//!
//! All coordinates are PDF big points (bp, 1/72 in) with the origin at the top-left corner of
//! the page and y growing downwards; pages are 1-based.
#![warn(missing_docs)]

pub mod ffi;
mod inputs;
mod scanner;

use std::path::PathBuf;

pub use scanner::Scanner;

/// A rectangle in the PDF produced by a source line. Rect = (h, v_top, width, height).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncBox {
    /// The 1-based page.
    pub page: u32,
    /// Left edge in bp.
    pub h: f32,
    /// Top edge in bp, measured down from the top of the page.
    pub v_top: f32,
    /// Width in bp (never negative).
    pub width: f32,
    /// Height in bp: height above the baseline plus depth below it.
    pub height: f32,
}

/// A source location found by inverse search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncTarget {
    /// The source file (see [`Scanner::inverse`]).
    pub file: PathBuf,
    /// The 1-based line.
    pub line: u32,
}
