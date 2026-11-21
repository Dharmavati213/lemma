//! `cargo run -p xtask -- licenses [--target TRIPLE]`: the licences of the Rust crates compiled
//! into `lemma` and `lemma-mcp`, as one text file on standard output. The packages ship it
//! (`packaging/`): most of the crates' licences (MIT, Apache-2.0, BSD) ask for their copyright
//! notice and text to go with the program.
//!
//! The crates are those `cargo metadata` resolves for the two programs on the target (the
//! host's when none is given): normal dependencies only, since build scripts and tests are not
//! part of the programs. The texts are the licence files of each crate's own sources (the
//! registry's copy, which a build has fetched). Equal texts are printed once.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The programs the packages ship.
const ROOTS: [&str; 2] = ["lemma-app", "lemma-mcp"];

/// A file of a crate's top directory is a licence file when its name starts with one of these
/// (any case): `LICENSE-MIT`, `LICENCE.md`, `COPYING.LESSER`, `NOTICE`, `UNLICENSE`, …
const FILE_PREFIXES: [&str; 6] = [
    "license",
    "licence",
    "copying",
    "copyright",
    "notice",
    "unlicense",
];

/// One third-party crate of the programs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Crate {
    pub name: String,
    pub version: String,
    /// The SPDX expression of its manifest; empty when it has none.
    pub license: String,
    pub repository: String,
    /// Its source directory.
    pub dir: PathBuf,
    /// The manifest's `license-file`, relative to `dir`.
    pub license_file: Option<String>,
}

/// Runs `cargo metadata` in `root` and returns its JSON.
fn metadata(root: &Path, target: Option<&str>) -> Result<Value, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--locked"]);
    if let Some(target) = target {
        command.args(["--filter-platform", target]);
    }
    let out = command
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata's output: {e}"))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The third-party crates the [`ROOTS`] link, from `cargo metadata`'s output: every package
/// reached from them over normal dependencies, without the workspace's own packages.
pub fn crates_of(metadata: &Value) -> Result<Vec<Crate>, String> {
    let packages: BTreeMap<&str, &Value> = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata's output has no packages")?
        .iter()
        .map(|p| (text(p, "id"), p))
        .collect();
    let nodes: BTreeMap<&str, &Value> = metadata
        .pointer("/resolve/nodes")
        .and_then(Value::as_array)
        .ok_or("cargo metadata's output has no dependency graph")?
        .iter()
        .map(|n| (text(n, "id"), n))
        .collect();
    let mut queue: Vec<&str> = Vec::new();
    for root in ROOTS {
        let id = packages
            .iter()
            .find(|(_, p)| text(p, "name") == root && p.get("source").is_some_and(Value::is_null))
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("the workspace has no package {root}"))?;
        queue.push(id);
    }
    let mut seen: BTreeSet<&str> = queue.iter().copied().collect();
    while let Some(id) = queue.pop() {
        let node = nodes
            .get(id)
            .ok_or_else(|| format!("the dependency graph has no {id}"))?;
        for dep in node
            .get("deps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let normal = dep
                .get("dep_kinds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|k| k.get("kind").is_none_or(Value::is_null));
            let dep_id = text(dep, "pkg");
            if normal && seen.insert(dep_id) {
                queue.push(dep_id);
            }
        }
    }
    let mut crates = Vec::new();
    for id in seen {
        let package = packages
            .get(id)
            .ok_or_else(|| format!("cargo metadata lists no package {id}"))?;
        // The workspace's own packages are Lemma's, under Lemma's licence.
        if package.get("source").is_none_or(Value::is_null) {
            continue;
        }
        let manifest = Path::new(text(package, "manifest_path"));
        crates.push(Crate {
            name: text(package, "name").to_owned(),
            version: text(package, "version").to_owned(),
            license: text(package, "license").to_owned(),
            repository: text(package, "repository").to_owned(),
            dir: manifest.parent().unwrap_or(Path::new("")).to_path_buf(),
            license_file: package
                .get("license_file")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    crates.sort();
    Ok(crates)
}

/// Whether a file of this name in a crate's top directory is a licence file.
pub fn is_license_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    FILE_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// The licence files of a crate: (file name, text), by name. Line ends are `\n` and the space
/// at the ends of lines and of the file is dropped, so that copies of one text compare equal.
fn license_files(krate: &Crate) -> Result<Vec<(String, String)>, String> {
    let entries = std::fs::read_dir(&krate.dir).map_err(|e| {
        format!(
            "{} {}: cannot read {} ({e}); build the programs first, which fetches the crates",
            krate.name,
            krate.version,
            krate.dir.display()
        )
    })?;
    let mut names: BTreeSet<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_license_file(name))
        .collect();
    if let Some(file) = &krate.license_file
        && krate.dir.join(file).is_file()
    {
        names.insert(file.clone());
    }
    let mut files = Vec::new();
    for name in names {
        let bytes = std::fs::read(krate.dir.join(&name))
            .map_err(|e| format!("{} {}: {name}: {e}", krate.name, krate.version))?;
        files.push((name, normalized(&String::from_utf8_lossy(&bytes))));
    }
    Ok(files)
}

fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let trimmed = out.trim_matches('\n');
    format!("{trimmed}\n")
}

/// The document: the crates with their licences, then every distinct licence text with the
/// crates it came from. `files` gives a crate's licence files as (name, text).
pub fn document(
    crates: &[Crate],
    files: impl Fn(&Crate) -> Result<Vec<(String, String)>, String>,
) -> Result<String, String> {
    // Text -> the crates' files holding it, in the crates' order.
    let mut texts: Vec<(String, Vec<String>)> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut list = String::new();
    for krate in crates {
        let found = files(krate)?;
        let license = if krate.license.is_empty() {
            "see its licence file"
        } else {
            &krate.license
        };
        let _ = write!(list, "{} {} ({license})", krate.name, krate.version);
        if !krate.repository.is_empty() {
            let _ = write!(list, ", {}", krate.repository);
        }
        if found.is_empty() {
            list.push_str("; its package has no licence file");
        }
        list.push('\n');
        for (name, text) in found {
            let label = format!("{} {} ({name})", krate.name, krate.version);
            match index.get(&text) {
                Some(&i) => texts[i].1.push(label),
                None => {
                    index.insert(text.clone(), texts.len());
                    texts.push((text, vec![label]));
                }
            }
        }
    }
    let rule = "=".repeat(78);
    let mut out = String::new();
    out.push_str(
        "Lemma's programs contain the Rust crates listed below, each under the licence named\n\
         after it. The texts of their licence files follow the list; a text that several crates\n\
         share is printed once, with all of them named above it.\n\n",
    );
    out.push_str(&list);
    for (text, labels) in &texts {
        let _ = write!(out, "\n{rule}\n");
        for label in labels {
            let _ = writeln!(out, "{label}");
        }
        let _ = write!(out, "{rule}\n\n{text}");
    }
    Ok(out)
}

/// The host's target triple, from `rustc -vV`.
fn host() -> Option<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(rustc).arg("-vV").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_owned())
}

/// The whole task: the document for the workspace at `root`, for `target` (the host's without).
pub fn run(root: &Path, target: Option<&str>) -> Result<String, String> {
    // Without a platform, `cargo metadata` lists the crates of every system.
    let host = host();
    let target = target.or(host.as_deref());
    let crates = crates_of(&metadata(root, target)?)?;
    if crates.is_empty() {
        return Err("no third-party crates found".to_owned());
    }
    document(&crates, license_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(name: &str, registry: bool, license: &str) -> Value {
        json!({
            "id": format!("id-{name}"),
            "name": name,
            "version": "1.0.0",
            "license": license,
            "repository": format!("https://example.invalid/{name}"),
            "manifest_path": format!("/src/{name}/Cargo.toml"),
            "source": if registry { json!("registry+https://example.invalid") } else { Value::Null },
        })
    }

    fn node(name: &str, deps: &[(&str, Option<&str>)]) -> Value {
        let deps: Vec<Value> = deps
            .iter()
            .map(|(dep, kind)| {
                json!({ "pkg": format!("id-{dep}"), "dep_kinds": [{ "kind": kind, "target": null }] })
            })
            .collect();
        json!({ "id": format!("id-{name}"), "deps": deps })
    }

    fn sample() -> Value {
        json!({
            "packages": [
                package("lemma-app", false, "GPL-3.0-or-later"),
                package("lemma-mcp", false, "GPL-3.0-or-later"),
                package("lemma-core", false, "GPL-3.0-or-later"),
                package("used", true, "MIT"),
                package("deep", true, "MIT OR Apache-2.0"),
                package("builder", true, "MIT"),
                package("tester", true, "MIT"),
                package("unused", true, "MIT"),
            ],
            "resolve": { "nodes": [
                node("lemma-app", &[
                    ("lemma-core", None), ("builder", Some("build")), ("tester", Some("dev")),
                ]),
                node("lemma-mcp", &[]),
                node("lemma-core", &[("used", None)]),
                node("used", &[("deep", None)]),
                node("deep", &[]),
                node("builder", &[]),
                node("tester", &[]),
                node("unused", &[]),
            ]},
        })
    }

    #[test]
    fn only_what_the_programs_link_is_listed() {
        let crates = crates_of(&sample()).unwrap();
        let names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();
        // Through a workspace crate and a third-party one; not the workspace's own packages,
        // not build or test dependencies, not what nothing reaches.
        assert_eq!(names, ["deep", "used"]);
        assert_eq!(crates[0].dir, Path::new("/src/deep"));
        assert_eq!(crates[0].license, "MIT OR Apache-2.0");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let mut metadata = sample();
        metadata["packages"].as_array_mut().unwrap().remove(1);
        let error = crates_of(&metadata).unwrap_err();
        assert!(error.contains("lemma-mcp"), "{error}");
    }

    #[test]
    fn licence_file_names() {
        for name in [
            "LICENSE",
            "LICENSE-MIT",
            "license.txt",
            "LICENCE.md",
            "COPYING",
            "COPYING.LESSER",
            "COPYRIGHT",
            "NOTICE",
            "UNLICENSE",
        ] {
            assert!(is_license_file(name), "{name}");
        }
        for name in ["Cargo.toml", "README.md", "lib.rs", "build.rs"] {
            assert!(!is_license_file(name), "{name}");
        }
    }

    #[test]
    fn equal_texts_are_printed_once_with_every_crate_named() {
        let crates = crates_of(&sample()).unwrap();
        let out = document(&crates, |krate| {
            Ok(match krate.name.as_str() {
                "deep" => vec![
                    (
                        "LICENSE-APACHE".to_owned(),
                        normalized("Apache text  \r\n\r\n"),
                    ),
                    ("LICENSE-MIT".to_owned(), normalized("MIT, by deep")),
                ],
                _ => vec![("LICENSE".to_owned(), normalized("\nApache text\n"))],
            })
        })
        .unwrap();
        assert!(out.contains("deep 1.0.0 (MIT OR Apache-2.0), https://example.invalid/deep\n"));
        assert_eq!(out.matches("\nApache text\n").count(), 1, "{out}");
        let shared = out.find("deep 1.0.0 (LICENSE-APACHE)\nused 1.0.0 (LICENSE)\n");
        assert!(shared.is_some(), "{out}");
        assert!(out.contains("deep 1.0.0 (LICENSE-MIT)\n"), "{out}");
        assert!(out.ends_with("MIT, by deep\n"), "{out}");
    }

    #[test]
    fn a_crate_without_a_licence_file_says_so() {
        let crates = crates_of(&sample()).unwrap();
        let out = document(&crates, |_| Ok(Vec::new())).unwrap();
        assert!(
            out.contains(
                "used 1.0.0 (MIT), https://example.invalid/used; its package has no licence file\n"
            ),
            "{out}"
        );
    }

    /// The real workspace: the crates everything is built on are there with their texts.
    #[test]
    fn the_workspaces_own_programs() {
        let root = crate::workspace_root();
        let out = run(&root, None).unwrap();
        for name in ["gtk4 ", "glib ", "serde ", "libadwaita "] {
            assert!(
                out.lines().any(|line| line.starts_with(name)),
                "{name} is missing"
            );
        }
        assert!(!out.lines().any(|line| line.starts_with("lemma-core ")));
        assert!(!out.lines().any(|line| line.starts_with("proptest ")));
        // Only this system's crates.
        let other = if cfg!(windows) {
            "nix "
        } else {
            "windows-sys "
        };
        assert!(!out.lines().any(|line| line.starts_with(other)), "{other}");
        assert!(out.contains("Permission is hereby granted, free of charge"));
    }
}
