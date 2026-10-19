//! Lemma's data generation tasks.
//!
//! ```sh
//! cargo run -p xtask -- gen-completion           # regenerate generated.rs and packages.zst
//! cargo run -p xtask -- gen-completion --check   # fail if the committed files are out of date (CI)
//! ```

mod cwl;
mod generate;
mod umath;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: cargo run -p xtask -- gen-completion [--check]";

/// The workspace root (the parent of this crate's directory).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["gen-completion"] => gen_completion(false),
        ["gen-completion", "--check"] => gen_completion(true),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// The generated files: (path relative to the root, contents).
fn outputs(generated: &generate::Generated) -> [(&'static str, &[u8]); 2] {
    [
        (generate::OUTPUT, generated.rust.as_bytes()),
        (generate::PACKED_OUTPUT, &generated.packed),
    ]
}

fn gen_completion(check: bool) -> ExitCode {
    let root = workspace_root();
    let generated = match generate::generate(&root) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("gen-completion: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    for (rel, contents) in outputs(&generated) {
        let path = root.join(rel);
        let current = fs::read(&path).unwrap_or_default();
        if current == contents {
            println!("gen-completion: {rel} is up to date");
        } else if check {
            eprintln!(
                "gen-completion: {rel} is out of date; run `cargo run -p xtask -- gen-completion`"
            );
            failed = true;
        } else if let Err(e) = fs::write(&path, contents) {
            eprintln!("gen-completion: {}: {e}", path.display());
            failed = true;
        } else {
            println!("gen-completion: wrote {rel} ({} bytes)", contents.len());
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed tables match the inputs (what `gen-completion --check` verifies in CI).
    #[test]
    fn generated_files_are_up_to_date() {
        let root = workspace_root();
        let generated = generate::generate(&root).expect("inputs are valid");
        for (rel, contents) in outputs(&generated) {
            let current = fs::read(root.join(rel)).expect("the generated files are committed");
            assert!(
                current == contents,
                "{rel} is out of date; run `cargo run -p xtask -- gen-completion`"
            );
        }
    }

    /// Generating twice gives the same bytes.
    #[test]
    fn generation_is_deterministic() {
        let root = workspace_root();
        let a = generate::generate(&root).expect("inputs are valid");
        let b = generate::generate(&root).expect("inputs are valid");
        assert_eq!(a, b);
    }
}
