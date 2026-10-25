//! Lemma's GTK application: every widget, buffer and controller lives in this crate.
//!
//! The crate is a library plus the thin `lemma` binary (`main.rs`), so the GTK integration tests in
//! `tests/gtk_*.rs` can drive the same types the app uses. Everything here runs on the GTK main
//! thread.

pub mod agent;
pub mod agent_edits;
pub mod app;
pub mod build_ui;
pub mod changes_panel;
pub mod cli;
pub mod diagnostics_panel;
pub mod document;
pub mod editor;
pub mod file_commands;
pub mod fsutil;
pub mod heap;
pub mod hooks;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod memreport;
pub mod pdf;
pub mod review_persist;
pub mod review_ui;
pub mod saver;
pub mod settings;
pub mod testmode;
pub mod window;
pub mod workspace;

pub mod find_bar;
pub mod git_gutter;
pub mod history_ui;
pub mod labels;
pub mod lint;
pub mod outline_panel;
pub mod palette;
pub mod prefs;
pub mod quick_open;
pub mod quickfix;
pub mod search_panel;
pub mod session;
pub mod shortcuts;
pub mod wordcount;

// M5 (WP5.5): image paste and drops, templates, BibTeX import, auto-save
pub mod autosave;
pub mod bibimport;
pub mod paste;
pub mod templates;

// M5 (WP5.3): latexindent formatting (the environment tools and align helper are in `editor`)
pub mod format;

// M5 (WP5.4): timing measurements on the real display (`LEMMA_PERF`, scripts/perf-wayland.sh)
pub mod perf;

// Copies of unsaved text that a crash, a kill or a logout cannot take away
pub mod recovery;

#[cfg(windows)]
pub mod windows;

/// The GApplication id (ARCHITECTURE §10.5).
pub const APP_ID: &str = "io.github.lemma_editor.Lemma";
