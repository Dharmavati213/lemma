//! The shared scenario fixtures (`tests/fixtures/agent/*.json`, used by the GUI work packages)
//! run end to end against a copy of `tests/fixtures/proj` and a small simulated editor that
//! answers the tools with the texts of ARCHITECTURE §4.2 / §10.4. This proves that every edit of
//! the fixtures applies to the fixture project, and that `errors.json` expects the exact texts.

mod acp_support;

use acp_support::*;
use lemma_agent::{AgentEvent, EditorRequest, StopReason, ToolReply};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// `root`-relative resolution like `ProjectRoot::resolve`, without the file system.
fn resolve(root: &Path, p: &Path) -> Option<PathBuf> {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.starts_with(root).then_some(out)
}

/// The editor's answers to the file tools (§4.2), on disk instead of in buffers.
fn simulated_editor(root: PathBuf) -> impl Fn(&EditorRequest) -> ToolReply + Send + 'static {
    move |req| {
        let outside = || text_reply("path outside project root", true);
        match req {
            EditorRequest::Edit {
                path,
                old_string,
                new_string,
                replace_all,
            } => {
                let Some(full) = resolve(&root, path) else {
                    return outside();
                };
                let Ok(text) = std::fs::read_to_string(&full) else {
                    return text_reply("file not found", true);
                };
                let n = text.matches(old_string.as_str()).count();
                if n == 0 {
                    return text_reply(
                        "old_string not found — the user may have edited this region; call read_file",
                        true,
                    );
                }
                if n > 1 && !replace_all {
                    return text_reply(
                        &format!("old_string matches {n} times; add context or set replace_all"),
                        true,
                    );
                }
                let new = if *replace_all {
                    text.replace(old_string.as_str(), new_string)
                } else {
                    text.replacen(old_string.as_str(), new_string, 1)
                };
                std::fs::write(&full, new).unwrap();
                text_reply(&format!("Edited {}", path.display()), false)
            }
            EditorRequest::Write { path, content } => {
                let Some(full) = resolve(&root, path) else {
                    return outside();
                };
                std::fs::write(&full, content).unwrap();
                text_reply(&format!("Wrote {}", path.display()), false)
            }
            EditorRequest::ReadFile { path, .. } => match resolve(&root, path) {
                Some(full) => text_reply(&std::fs::read_to_string(full).unwrap_or_default(), false),
                None => outside(),
            },
            EditorRequest::Compile => text_reply("Build succeeded: 5 pages, 1 warning.", false),
            _ => text_reply("ok", false),
        }
    }
}

/// Runs one prompt of a fixture scenario; returns the events of the turn and the tool calls.
fn run_fixture(
    sb: &Sandbox,
    fixture: &str,
    prompt_text: &str,
) -> (Vec<AgentEvent>, Vec<EditorRequest>) {
    let scenario = fixtures().join("agent").join(fixture);
    let mut handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let tools = std::mem::replace(&mut handle.tools, async_channel::unbounded().1);
    let answerer = ToolAnswerer::start(tools, simulated_editor(sb.project()));
    expect_ready(&handle);
    prompt(&handle, prompt_text);
    let events = events_until(&handle, SLOW, is_turn_end);
    shutdown(&handle);
    let calls = answerer.calls.clone();
    answerer.join();
    let requests = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, r)| r.clone())
        .collect();
    (events, requests)
}

fn read(sb: &Sandbox, rel: &str) -> String {
    std::fs::read_to_string(sb.project().join(rel)).unwrap()
}

#[test]
fn basic_edits_two_files_and_compiles() {
    let sb = Sandbox::new("fx-basic");
    sb.copy_fixture_project();
    let (events, requests) = run_fixture(&sb, "basic.json", "Polish the notes");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    let edited: Vec<&Path> = requests
        .iter()
        .filter_map(|r| match r {
            EditorRequest::Edit { path, .. } => Some(path.as_path()),
            _ => None,
        })
        .collect();
    assert_eq!(
        edited,
        [
            Path::new("main.tex"),
            Path::new("main.tex"),
            Path::new("chapters/yoneda.tex")
        ]
    );
    assert_eq!(requests.last(), Some(&EditorRequest::Compile));
    assert!(read(&sb, "main.tex").contains("These notes serve as a test document for Lemma."));
    assert!(read(&sb, "main.tex").contains("is treated in detail in~\\cite{Vistoli}."));
    assert!(read(&sb, "chapters/yoneda.tex").contains("Throughout, let $\\cC$"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Plan(p) if p.len() == 4))
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Usage { .. })));
}

#[test]
fn basic_inline_edits_only_the_abstract() {
    for word in ["Inline: tighten this", "Edit only the selected region"] {
        let sb = Sandbox::new("fx-inline");
        sb.copy_fixture_project();
        let original = read(&sb, "main.tex");
        let (events, requests) = run_fixture(&sb, "basic.json", word);
        assert!(errors(&events).is_empty(), "{events:#?}");
        assert_eq!(requests.len(), 1, "{requests:#?}");
        let after = read(&sb, "main.tex");
        let changed: Vec<usize> = original
            .lines()
            .zip(after.lines())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i + 1)
            .collect();
        assert_eq!(changed, [34], "only line 34 (the abstract) changes");
    }
}

#[test]
fn errors_expects_the_exact_refusal_texts() {
    let sb = Sandbox::new("fx-errors");
    sb.copy_fixture_project();
    let (events, requests) = run_fixture(&sb, "errors.json", "try");
    // The fake agent fails the turn unless each reply is an error with the exact text.
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    // Nothing was written.
    assert_eq!(
        read(&sb, "main.tex"),
        std::fs::read_to_string(fixtures().join("proj/main.tex")).unwrap()
    );
}

#[test]
#[cfg(unix)]
fn bypass_writes_files_behind_the_editors_back() {
    let sb = Sandbox::new("fx-bypass");
    sb.copy_fixture_project();
    let inode = |rel: &str| std::fs::metadata(sb.project().join(rel)).unwrap().ino();
    let before = inode("chapters/yoneda.tex");
    let (events, requests) = run_fixture(&sb, "bypass.json", "fix the wording");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert!(requests.is_empty(), "no MCP tool is used");
    assert!(read(&sb, "chapters/yoneda.tex").contains("is fully faithful, hence an embedding."));
    assert!(read(&sb, "main.tex").contains("comparison theorem~\\cite{SGA1}."));
    // Like `sed -i`: a new file renamed over the old one.
    assert_ne!(inode("chapters/yoneda.tex"), before);
    // Shown as Bash-like tool calls.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { kind, .. } if kind == "execute"))
    );

    let sb = Sandbox::new("fx-bypass-inplace");
    sb.copy_fixture_project();
    let before = inode("chapters/yoneda.tex");
    let (events, _) = run_fixture(&sb, "bypass.json", "rewrite it inplace");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert!(read(&sb, "chapters/yoneda.tex").contains("hence an embedding."));
    assert_eq!(inode("chapters/yoneda.tex"), before);
}

#[test]
fn long_streams_200_chunks_and_makes_30_edits() {
    let sb = Sandbox::new("fx-long");
    sb.copy_fixture_project();
    let (events, requests) = run_fixture(&sb, "long.json", "go");
    assert!(errors(&events).is_empty(), "{events:#?}");
    let chunks = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageChunk(_)))
        .count();
    assert_eq!(chunks, 200);
    let edits = requests
        .iter()
        .filter(|r| matches!(r, EditorRequest::Edit { .. }))
        .count();
    assert_eq!(edits, 30);
    assert!(matches!(requests[0], EditorRequest::Write { .. }));
    let text = read(&sb, "long-scenario.tex");
    assert_eq!(text.lines().count(), 30);
    assert!(text.lines().all(|l| l.contains("(revised)")), "{text}");
}

#[test]
fn every_fixture_has_a_default_turn_that_starts() {
    // Each fixture file is a valid scenario and its session starts (cancel.json/permission.json
    // turns are exercised in acp_client.rs).
    for name in [
        "basic.json",
        "permission.json",
        "bypass.json",
        "errors.json",
        "long.json",
        "cancel.json",
    ] {
        let sb = Sandbox::new("fx-start");
        let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent").join(name))).unwrap();
        expect_ready(&handle);
        let rest = shutdown(&handle);
        assert_eq!(
            rest.last(),
            Some(&AgentEvent::Exited(Some(0))),
            "{name}: {rest:?}"
        );
    }
}
