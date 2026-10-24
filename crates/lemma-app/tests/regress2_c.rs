//! Regression tests for verified M2 defects of the agent UI (review round 2, area C) that need no
//! display: the Markdown renderer of agent messages and the toolchain detection. Each test fails
//! until its defect is fixed. GTK behaviour is covered by `gtk_regress2_c.rs`.

use lemma_app::agent::install;
use lemma_app::agent::markdown::{Block, Incremental, render};
use lemma_config::{AgentCfg, Paths};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn temp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress2-c-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn paths(home: &Path) -> Paths {
    let home = home.to_path_buf();
    Paths::from_vars(move |name| (name == "HOME").then(|| home.clone().into_os_string()))
        .expect("paths")
}

/// The text of every block, in order (markup, code, items, quotes, table cells).
fn texts(blocks: &[Block]) -> Vec<String> {
    blocks
        .iter()
        .map(|b| match b {
            Block::Markup(m) | Block::Quote(m) => m.clone(),
            Block::Code { text, .. } => text.clone(),
            Block::Item { marker, markup, .. } => format!("{marker} {markup}"),
            Block::Table { header, rows } => format!("TABLE {header:?} {rows:?}"),
            Block::Rule => "RULE".into(),
        })
        .collect()
}

// ----- C-11: single tildes (LaTeX ties) -----

/// `~` is a LaTeX tie; between punctuation (`$…$~and~$…$`, `}~and~\ref`) pulldown-cmark's
/// single-tilde strikethrough eats the ties and strikes the word through.
#[test]
fn regress2_markdown_ties_stay_verbatim() {
    for text in [
        "So $f$~and~$g$ agree.",
        "By Lemmas~\\ref{a}~and~\\ref{b} the claim holds.",
    ] {
        let blocks = render(text);
        let markup = texts(&blocks).join("\n");
        assert!(
            !markup.contains("<s>") && markup.contains("~and~"),
            "{text:?} rendered as {markup:?}: the ties became strikethrough"
        );
    }
}

// ----- C-12: Incremental ≠ render -----

/// `Incremental` promises the result of `render` on the whole text; it cuts pieces at blank
/// lines, so a loose list numbered `1.` throughout restarts at every item, and an HTML block
/// that spans a blank line is parsed in two halves.
#[test]
fn regress2_incremental_equals_render() {
    let mut differing = Vec::new();
    for text in [
        "1. one\n\n1. two\n\n1. three\n",
        "<pre>\nx\n\ny\n</pre>\n",
        "<!-- a comment\n\nspanning -->\n",
    ] {
        let whole = render(text);
        // At once, and streamed in small chunks as the panel does.
        let mut inc = Incremental::default();
        let at_once = inc.render(text);
        let mut streamed = Incremental::default();
        let mut last = Vec::new();
        let mut end = 0;
        while end < text.len() {
            end = (end + 3).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            last = streamed.render(&text[..end]);
        }
        if at_once != whole || last != whole {
            differing.push(format!(
                "{text:?}: render {whole:?}, Incremental {at_once:?}, streamed {last:?}"
            ));
        }
    }
    assert!(
        differing.is_empty(),
        "Incremental differs from render:\n{}",
        differing.join("\n")
    );
}

// ----- C-13: a table inside a block quote -----

/// The table of a quote is emitted before the quote's text (and outside it), and the
/// paragraphs around it are merged.
#[test]
fn regress2_table_in_quote_keeps_its_place() {
    let text = "> intro\n>\n> | alpha | beta |\n> |---|---|\n> | one | two |\n>\n> outro\n";
    let blocks = render(text);
    // Wherever the table goes (its own block or flattened into the quote), the reading order
    // of the shown text must be intro, table, outro.
    let all = texts(&blocks).join("\n");
    let at = |needle: &str| all.find(needle);
    let (intro, table, outro) = (at("intro"), at("alpha"), at("outro"));
    assert!(
        intro.is_some() && table.is_some() && outro.is_some(),
        "{blocks:?}"
    );
    assert!(
        intro < table && table < outro,
        "order intro → table → outro lost: {blocks:?}"
    );
}

// ----- C-21: U+0000 in agent text -----

/// pulldown-cmark passes NUL through; `escape` hands the text to GLib as a C string, which
/// panics in debug builds (`GStrInteriorNulError`, an abort inside a GTK callback) and cuts the
/// text in release builds.
#[test]
fn regress2_nul_in_agent_text() {
    let text = "The byte at offset 12 is \u{0} here, and the rest of the sentence.";
    let blocks = render(text);
    let markup = texts(&blocks).join("\n");
    assert!(
        markup.contains("and the rest of the sentence"),
        "text after the NUL was lost: {markup:?}"
    );
}

// ----- C-6: claude's version from node's path -----

/// `npm i -g @anthropic-ai/claude-code` under a mise (or asdf) node: the only `x.y.z` in the
/// canonical path is node's version, which is taken as claude's.
#[test]
fn regress2_claude_version_of_npm_claude_under_mise_node() {
    let root = temp("npm-claude");
    let node = root.join("mise/installs/node/22.11.0");
    let cli = node.join("lib/node_modules/@anthropic-ai/claude-code/cli.js");
    script(&cli, "echo '2.0.5 (Claude Code)'");
    std::fs::create_dir_all(node.join("bin")).unwrap();
    let claude = node.join("bin/claude");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "../lib/node_modules/@anthropic-ai/claude-code/cli.js",
        &claude,
    )
    .unwrap();
    let version = install::claude_version(&claude);
    assert_eq!(
        version.as_deref(),
        Some("2.0.5"),
        "node's version was taken as claude's"
    );
    assert!(install::needs_update(version.as_deref().unwrap_or("0.0.0")));
}

// ----- C-8: a symlinked config.toml -----

/// Storing the detected `agent.node`/`agent.claude` renames a temporary file over the config
/// path: a `config.toml` that is a symlink into the user's dotfiles becomes a regular file.
#[test]
fn regress2_store_paths_keeps_a_symlinked_config() {
    let root = temp("symlink-config");
    let home = root.join("home");
    let bin = root.join("bin");
    script(&bin.join("node"), "exit 0");
    script(&bin.join("claude"), "echo '2.1.296 (Claude Code)'");
    let dotfiles = home.join("dotfiles/lemma/config.toml");
    std::fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
    std::fs::write(&dotfiles, "# my settings\n[editor]\nvim = true\n").unwrap();
    let config = home.join(".config/lemma/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dotfiles, &config).unwrap();
    let env = install::Env {
        path: bin.clone().into_os_string(),
        config_file: Some(config.clone()),
        mise_installs: root.join("mise/installs"),
    };
    // The adapter is not installed: detection fails after storing the paths.
    let result = install::detect_in(&AgentCfg::default(), &paths(&home), &env);
    assert!(result.is_err());
    let is_link = std::fs::symlink_metadata(&config)
        .unwrap()
        .file_type()
        .is_symlink();
    let target = std::fs::read_to_string(&dotfiles).unwrap();
    assert!(
        is_link,
        "config.toml is no longer a symlink (the dotfiles copy is cut off)"
    );
    assert!(target.contains("vim = true"));
}
