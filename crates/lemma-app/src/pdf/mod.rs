//! The PDF preview pane (ARCHITECTURE §6): [`pdf_view::PdfView`] draws the pages,
//! [`pdf_controller::PdfController`] connects it to the PDF thread, the toolbar and SyncTeX.
//!
//! M3 (WP3.4): [`links`] (hover previews, jumps and Back, web links), [`search`], [`sidebar`]
//! (the strip of thumbnails and bookmarks, drawn with [`page_picture`]) and [`detach`] (a
//! window of its own).

pub mod detach;
mod geometry;
pub mod links;
pub mod page_picture;
pub mod pdf_controller;
pub mod pdf_view;
pub mod search;
pub mod sidebar;
mod texture_cache;
