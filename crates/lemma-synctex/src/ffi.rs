//! Raw bindings to the system libsynctex 2.0 (`/usr/include/synctex/synctex_parser.h`).
//!
//! Only the functions [`Scanner`](crate::Scanner) needs are declared, with the exact C signatures.
//! All dimensions returned by the `*_visible_*` functions are PDF big points (1/72 in) with the
//! origin at the top-left corner of the page and y growing downwards.
#![allow(non_camel_case_types)]

use std::marker::{PhantomData, PhantomPinned};
use std::os::raw::{c_char, c_float, c_int, c_long};

/// Opaque `struct synctex_scanner_t`.
#[repr(C)]
pub struct synctex_scanner_s {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque `struct synctex_node_t`.
#[repr(C)]
pub struct synctex_node_s {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `synctex_scanner_p`: owned by the caller, released with [`synctex_scanner_free`].
pub type synctex_scanner_p = *mut synctex_scanner_s;
/// `synctex_node_p`: borrowed from its scanner; valid until the scanner is freed.
pub type synctex_node_p = *mut synctex_node_s;
/// `synctex_status_t` (`long`): a node count, or a negative value on error.
pub type synctex_status_t = c_long;

unsafe extern "C" {
    /// Opens and (with `parse = 1`) parses the synctex file belonging to `output` (the PDF):
    /// the last extension is replaced by `.synctex` or `.synctex.gz`. `build_directory` may be
    /// null. Returns null if the file is missing or cannot be parsed.
    pub fn synctex_scanner_new_with_output_file(
        output: *const c_char,
        build_directory: *const c_char,
        parse: c_int,
    ) -> synctex_scanner_p;

    /// Frees the scanner and every node it owns.
    pub fn synctex_scanner_free(scanner: synctex_scanner_p) -> c_int;

    /// Forward search. `name` must be an input name exactly as TeX recorded it (an unknown name
    /// prints a warning on stdout). `line` is 1-based. Results closer to `page_hint` come first.
    /// Returns the number of results; read them with [`synctex_scanner_next_result`].
    pub fn synctex_display_query(
        scanner: synctex_scanner_p,
        name: *const c_char,
        line: c_int,
        column: c_int,
        page_hint: c_int,
    ) -> synctex_status_t;

    /// Inverse search at `(h, v)` in bp from the top-left corner of the 1-based `page`. Returns
    /// the number of results; read them with [`synctex_scanner_next_result`].
    pub fn synctex_edit_query(
        scanner: synctex_scanner_p,
        page: c_int,
        h: c_float,
        v: c_float,
    ) -> synctex_status_t;

    /// The next result of the last query, or null when there are no more.
    pub fn synctex_scanner_next_result(scanner: synctex_scanner_p) -> synctex_node_p;

    /// The input name recorded for `tag`, or null. Owned by the scanner.
    pub fn synctex_scanner_get_name(scanner: synctex_scanner_p, tag: c_int) -> *const c_char;

    /// The first `Input:` node of the scanner; the others follow via [`synctex_node_sibling`].
    pub fn synctex_scanner_input(scanner: synctex_scanner_p) -> synctex_node_p;

    /// The next sibling of `node`, or null.
    pub fn synctex_node_sibling(node: synctex_node_p) -> synctex_node_p;

    /// The 1-based page on which `node` appears.
    pub fn synctex_node_page(node: synctex_node_p) -> c_int;

    /// The input tag of `node` (see [`synctex_scanner_get_name`]).
    pub fn synctex_node_tag(node: synctex_node_p) -> c_int;

    /// The 1-based source line of `node`.
    pub fn synctex_node_line(node: synctex_node_p) -> c_int;

    /// Left edge of the box enclosing `node`.
    pub fn synctex_node_box_visible_h(node: synctex_node_p) -> c_float;

    /// Baseline of the box enclosing `node`.
    pub fn synctex_node_box_visible_v(node: synctex_node_p) -> c_float;

    /// Width of the box enclosing `node` (may be negative).
    pub fn synctex_node_box_visible_width(node: synctex_node_p) -> c_float;

    /// Height above the baseline of the box enclosing `node`.
    pub fn synctex_node_box_visible_height(node: synctex_node_p) -> c_float;

    /// Depth below the baseline of the box enclosing `node`.
    pub fn synctex_node_box_visible_depth(node: synctex_node_p) -> c_float;
}
