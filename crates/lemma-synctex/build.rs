fn main() {
    // system libsynctex 2.0.0 (texlive-bin); emits cargo:rustc-link-lib=synctex
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("synctex")
        .expect("libsynctex (pkg-config synctex) not found");
}
