//! Links libsynctex 2.0: the system library through pkg-config (Linux: `libsynctex` or
//! `texlive-bin`), or the copy of the SyncTeX parser in `vendor/synctex/` compiled here.
//!
//! The bundled copy is used on macOS, where no package manager ships libsynctex on its own, and
//! wherever `LEMMA_SYNCTEX_BUNDLED=1` is set. It is the parser of TeX Live's
//! `texk/web2c/synctexdir` (API level 2.0.0, the same headers as the system library) and needs
//! only zlib.

use std::env;
use std::path::Path;

const VENDOR: &str = "vendor/synctex";

fn main() {
    println!("cargo:rerun-if-env-changed=LEMMA_SYNCTEX_BUNDLED");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let forced = env::var_os("LEMMA_SYNCTEX_BUNDLED").is_some_and(|v| v != "0" && !v.is_empty());
    if forced || target_os == "macos" {
        build_bundled();
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

/// Compiles the vendored parser into a static `libsynctex.a` and links zlib.
fn build_bundled() {
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
    cc::Build::new()
        .file(dir.join("synctex_parser.c"))
        .file(dir.join("synctex_parser_utils.c"))
        // synctex_parser_utils.c includes its own header with <…>.
        .include(dir)
        // Third-party code: its warnings are not ours to fix.
        .warnings(false)
        .compile("synctex");
    println!("cargo:rustc-link-lib=z");
}
