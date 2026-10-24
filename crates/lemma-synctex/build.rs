//! Links libsynctex 2.0: the system library through pkg-config (Linux: `libsynctex` or
//! `texlive-bin`), or the copy of the SyncTeX parser in `vendor/synctex/` compiled here.
//!
//! The bundled copy is used on macOS and Windows, where no package manager ships libsynctex on
//! its own, and wherever `LEMMA_SYNCTEX_BUNDLED=1` is set. It is the parser of TeX Live's
//! `texk/web2c/synctexdir` (API level 2.0.0, the same headers as the system library) and needs
//! only zlib.

use std::env;
use std::path::Path;

const VENDOR: &str = "vendor/synctex";

fn main() {
    println!("cargo:rerun-if-env-changed=LEMMA_SYNCTEX_BUNDLED");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let forced = env::var_os("LEMMA_SYNCTEX_BUNDLED").is_some_and(|v| v != "0" && !v.is_empty());
    if forced || target_os == "macos" || target_os == "windows" {
        build_bundled(&target_os);
        return;
    }
    // system libsynctex 2.0.0 (texlive-bin); emits cargo:rustc-link-lib=synctex
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("synctex")
        .expect(
            "libsynctex (pkg-config synctex) not found; \
             set LEMMA_SYNCTEX_BUNDLED=1 to build the bundled copy instead",
        );
}

/// Compiles the vendored parser into a static `libsynctex.a` and links zlib: on Windows through
/// pkg-config (MSYS2's `zlib.pc` names its headers), elsewhere as plain `-lz`.
fn build_bundled(target_os: &str) {
    let dir = Path::new(VENDOR);
    for file in [
        "synctex_parser.c",
        "synctex_parser_utils.c",
        "synctex_parser.h",
        "synctex_parser_advanced.h",
        "synctex_parser_utils.h",
        "synctex_version.h",
    ] {
        println!("cargo:rerun-if-changed={VENDOR}/{file}");
    }
    let zlib = (target_os == "windows")
        .then(|| pkg_config::Config::new().probe("zlib").ok())
        .flatten();
    let mut build = cc::Build::new();
    for include in zlib.iter().flat_map(|z| &z.include_paths) {
        build.include(include);
    }
    build
        .file(dir.join("synctex_parser.c"))
        .file(dir.join("synctex_parser_utils.c"))
        // synctex_parser_utils.c includes its own header with <…>.
        .include(dir)
        // Third-party code: its warnings are not ours to fix.
        .warnings(false)
        .compile("synctex");
    if zlib.is_none() {
        println!("cargo:rustc-link-lib=z");
    }
    // The parser's path functions on Windows (`PathFindFileNameA`, `PathFindExtensionA`).
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=shlwapi");
    }
}
