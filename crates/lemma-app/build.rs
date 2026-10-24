//! Windows: embeds the icon (`packaging/windows/lemma.ico`), the version information and the
//! application manifest (`packaging/windows/lemma.manifest`) into `lemma.exe`. The resource
//! script is compiled with `windres` (`$WINDRES`, else a cross toolchain's
//! `x86_64-w64-mingw32-windres`, MSYS2's `windres` or `llvm-windres`) into an object the linker
//! takes. Without one, `lemma.exe` is built without them, with a warning. Nothing happens on
//! other systems.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let packaging = manifest_dir.join("../../packaging/windows");
    let icon = packaging.join("lemma.ico");
    let manifest = packaging.join("lemma.manifest");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", manifest.display());
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!(
            "cargo:warning=lemma.exe is built without its icon and manifest: only the GNU \
             toolchains (x86_64-pc-windows-gnu, -gnullvm) are supported for them"
        );
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets it"));
    let script = out.join("lemma.rc");
    let object = out.join("lemma-resources.o");
    std::fs::write(&script, resource_script(&icon, &manifest)).expect("write lemma.rc");
    let candidates: Vec<String> = env::var("WINDRES")
        .into_iter()
        .chain(["x86_64-w64-mingw32-windres", "windres", "llvm-windres"].map(String::from))
        .collect();
    for windres in &candidates {
        // llvm-windres needs no preprocessor for this script; GNU windres has no such option.
        for no_preprocess in [true, false] {
            let mut command = Command::new(windres);
            command.arg("--input-format=rc").arg("--output-format=coff");
            if no_preprocess {
                command.arg("--no-preprocess");
            }
            command.arg("-i").arg(&script).arg("-o").arg(&object);
            if command.output().is_ok_and(|o| o.status.success()) && object.is_file() {
                println!("cargo:rustc-link-arg-bin=lemma={}", object.display());
                return;
            }
        }
    }
    println!(
        "cargo:warning=lemma.exe is built without its icon and manifest: no windres was found \
         (tried {})",
        candidates.join(", ")
    );
}

/// The resource script: the icon, the manifest (`RT_MANIFEST`, id 1) and the version.
fn resource_script(icon: &Path, manifest: &Path) -> String {
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets it");
    let numbers: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    let file_version = format!(
        "{},{},{},0",
        numbers.first().unwrap_or(&0),
        numbers.get(1).unwrap_or(&0),
        numbers.get(2).unwrap_or(&0)
    );
    format!(
        r#"1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {file_version}
PRODUCTVERSION {file_version}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Lemma"
      VALUE "FileDescription", "Lemma"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "lemma"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "lemma.exe"
      VALUE "ProductName", "Lemma"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = rc_path(icon),
        manifest = rc_path(manifest),
    )
}

/// `path` for a quoted string in a resource script: forward slashes, which every resource
/// compiler takes, and no backslash escapes.
fn rc_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
