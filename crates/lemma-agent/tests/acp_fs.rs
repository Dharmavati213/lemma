//! WP5.2: ACP `fs/*` routing for adapters other than Claude's (`AcpFlavor::Generic`, Pi), the
//! Claude adapter getting no `fs` capability, and the "authentication required" mark, against
//! `lemma-fake-agent` (its `fs_read`, `fs_write` and `fs_edit` steps play Pi's delegated tools).

mod acp_support;

use acp_support::*;
use lemma_agent::{AcpFlavor, AgentCmd, AgentEvent, EditorRequest, StopReason, ToolReply};
use serde_json::json;
use std::path::PathBuf;

fn generic(sb: &Sandbox, scenario: &serde_json::Value) -> lemma_agent::AgentConfig {
    let path = sb.scenario(scenario);
    let mut cfg = sb.config(&path);
    cfg.flavor = AcpFlavor::Generic;
    cfg
}

/// A fake editor: `ReadText` of `edit.tex` gives its text, of anything else `LIVE\n`; a write
/// outside the project fails like the real one.
fn editor(project: PathBuf) -> impl Fn(&EditorRequest) -> ToolReply + Send + 'static {
    move |req| match req {
        EditorRequest::ReadText { path, .. } if path.ends_with("edit.tex") => {
            text_reply("alpha beta\nalpha\n", false)
        }
        EditorRequest::ReadText { .. } => text_reply("LIVE\n", false),
        EditorRequest::WriteText { path, .. } if !path.starts_with(&project) => {
            text_reply("path outside project root", true)
        }
        EditorRequest::WriteText { .. } => text_reply("Wrote it", false),
        other => text_reply(&format!("unexpected {other:?}"), true),
    }
}

#[test]
fn generic_adapters_read_and_write_through_the_editor() {
    let sb = Sandbox::new("fs-generic");
    let project = sb.project();
    let outside = sb.dir.join("outside.tex");
    let cfg = generic(
        &sb,
        &json!({
            "_options": {"fs": "required", "agent_name": "openma-pi-acp"},
            "default": [
                {"fs_read": {"path": "main.tex", "expect_content": "LIVE\n"}},
                {"fs_read": {"path": "main.tex", "line": 2, "limit": 1}},
                {"fs_write": {"path": "new.tex", "content": "hello\n"}},
                {"fs_write": {"path": outside, "content": "x", "expect_error": "path outside project root"}},
                {"fs_edit": {"path": "edit.tex", "old_string": "beta", "new_string": "gamma"}},
                {"tool": {"name": "list_files", "expect_text": "LIVE"}}
            ]
        }),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let AgentEvent::Ready { agent, .. } = expect_ready(&handle) else {
        unreachable!()
    };
    assert!(agent.starts_with("openma-pi-acp "), "{agent}");
    // Our MCP server is still passed in `session/new`: its calls come on the same channel.
    let answerer = ToolAnswerer::start(handle.tools.clone(), {
        let fs = editor(project.clone());
        move |req| match req {
            EditorRequest::ListFiles => text_reply("LIVE (from MCP)", false),
            other => fs(other),
        }
    });
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    // Pi's tool cards come through as ACP tool calls.
    let titles: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { title, kind, .. } if kind == "read" || kind == "edit" => {
                Some(title.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        titles,
        [
            "read main.tex",
            "read main.tex",
            "write new.tex",
            &format!("write {}", outside.display()),
            "edit edit.tex"
        ]
    );
    shutdown(&handle);
    let recorded = answerer.calls.clone();
    answerer.join();
    let calls = recorded.lock().unwrap().clone();
    let requests: Vec<EditorRequest> = calls.iter().map(|(_, r)| r.clone()).collect();
    assert_eq!(
        requests,
        [
            EditorRequest::ReadText {
                path: project.join("main.tex"),
                line: None,
                limit: None
            },
            EditorRequest::ReadText {
                path: project.join("main.tex"),
                line: Some(2),
                limit: Some(1)
            },
            EditorRequest::WriteText {
                path: project.join("new.tex"),
                content: "hello\n".into()
            },
            EditorRequest::WriteText {
                path: outside.clone(),
                content: "x".into()
            },
            EditorRequest::ReadText {
                path: project.join("edit.tex"),
                line: None,
                limit: None
            },
            EditorRequest::WriteText {
                path: project.join("edit.tex"),
                content: "alpha gamma\nalpha\n".into()
            },
            EditorRequest::ListFiles,
        ]
    );
    // Every call has its own id (they become `Origin::Agent { call }`), none of them 0.
    let mut ids: Vec<u64> = calls.iter().map(|(id, _)| *id).collect();
    assert!(ids.iter().all(|id| *id > 0), "{ids:?}");
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), calls.len());
}

#[test]
fn fs_calls_reach_the_handle_when_the_caller_serves_mcp() {
    let sb = Sandbox::new("fs-callers-server");
    let _server = RecordingMcpServer::start(&sb.socket());
    let cfg = generic(
        &sb,
        &json!({"_options": {"fs": "required"}, "default": [
            {"fs_write": {"path": "main.tex", "content": "new\n"}}
        ]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    assert!(!handle.tools.is_closed(), "fs calls need the channel");
    let answerer = ToolAnswerer::start(handle.tools.clone(), editor(sb.project()));
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    shutdown(&handle);
    let recorded = answerer.calls.clone();
    answerer.join();
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[test]
fn an_unanswered_or_failed_fs_request_is_an_error_for_the_agent() {
    let sb = Sandbox::new("fs-errors");
    let cfg = generic(
        &sb,
        &json!({"_options": {"fs": "required", "mcp": "skip"}, "default": [
            {"fs_read": {"path": "gone.tex", "expect_error": "the editor dropped the request"}}
        ]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    // An editor that drops the call without answering.
    let tools = handle.tools.clone();
    let dropper = std::thread::spawn(move || {
        while let Ok(call) = tools.recv_blocking() {
            drop(call);
        }
    });
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    shutdown(&handle);
    dropper.join().unwrap();
}

#[test]
fn the_claude_adapter_gets_no_fs_capability() {
    let sb = Sandbox::new("fs-claude");
    let scenario = sb.scenario(&json!({"_options": {"fs": "forbidden"}, "default": [
        {"fs_read": {"path": "main.tex", "expect_error": "any"}}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    // `initialize` would have failed had the client advertised `fs`.
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    // The agent cannot use fs/* at all.
    let failed = errors(&events);
    assert!(
        failed
            .iter()
            .any(|e| e.contains("the client did not advertise fs.readTextFile")),
        "{events:#?}"
    );
    shutdown(&handle);
}

#[test]
fn authentication_required_is_marked_as_a_login() {
    let sb = Sandbox::new("fs-auth");
    let cfg = generic(
        &sb,
        &json!({"_options": {"session_new_error": {
            "code": -32000, "message": "Configure an API key or log in with an OAuth provider."
        }}, "default": [{"say": "never"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let events = drain(&handle, SLOW);
    let failed = errors(&events);
    assert!(
        failed
            .iter()
            .any(|e| lemma_agent::needs_login(e) && e.contains("log in with an OAuth provider")),
        "{events:#?}"
    );
    // Other errors are not.
    let sb = Sandbox::new("fs-auth-other");
    let cfg = generic(
        &sb,
        &json!({"_options": {"session_new_error": {"code": -32603, "message": "boom"}},
                "default": [{"say": "never"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let events = drain(&handle, SLOW);
    let failed = errors(&events);
    assert!(failed.iter().any(|e| e.contains("boom")), "{events:#?}");
    assert!(!failed.iter().any(|e| lemma_agent::needs_login(e)));
}

/// The modes a `Ready` offers and the mode it reports.
fn ready_modes(ready: &AgentEvent) -> (Vec<String>, String) {
    let AgentEvent::Ready { modes, mode, .. } = ready else {
        panic!("not Ready: {ready:?}")
    };
    (
        modes.iter().map(|(id, _)| id.clone()).collect(),
        mode.clone(),
    )
}

#[test]
fn other_agents_keep_their_own_modes_but_never_the_elevated_ones() {
    // An agent with Codex's modes: no `default`, starts in `agent`.
    let sb = Sandbox::new("modes-generic");
    let cfg = generic(
        &sb,
        &json!({"_options": {"modes": "codex"}, "default": [{"say": "ok"}],
                "escalate": [{"mode": "agent-full-access"}, {"say": "done"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let (modes, mode) = ready_modes(&expect_ready(&handle));
    assert_eq!(modes, ["read-only", "workspace-write", "agent"]);
    assert_eq!(mode, "agent", "an offered mode is kept");
    // The full-access mode is refused; the others go through.
    send(&handle, AgentCmd::SetMode("agent-full-access".into()));
    send(&handle, AgentCmd::SetMode("read-only".into()));
    let events = events_until(
        &handle,
        SLOW,
        |e| matches!(e, AgentEvent::ModeChanged(m) if m == "read-only"),
    );
    assert!(
        errors(&events)
            .iter()
            .any(|e| e.contains("does not offer the mode \"agent-full-access\"")),
        "{events:#?}"
    );
    // The agent switching itself to it is switched back to its first offered mode.
    send(&handle, AgentCmd::SetMode("workspace-write".into()));
    events_until(
        &handle,
        SLOW,
        |e| matches!(e, AgentEvent::ModeChanged(m) if m == "workspace-write"),
    );
    prompt(&handle, "escalate");
    let events = events_until(
        &handle,
        SLOW,
        |e| matches!(e, AgentEvent::ModeChanged(m) if m == "read-only"),
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
            if n.contains("\"agent-full-access\"") && n.contains("switching back to \"read-only\""))),
        "{events:#?}"
    );
    events_until(&handle, SLOW, is_turn_end);
    shutdown(&handle);

    // Starting in the full-access mode: switched before `Ready`.
    let sb = Sandbox::new("modes-generic-start");
    let cfg = generic(
        &sb,
        &json!({"_options": {"modes": "codex", "initial_mode": "agent-full-access"},
                "default": [{"say": "ok"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let (_, mode) = ready_modes(&expect_ready(&handle));
    assert_eq!(mode, "read-only");
    shutdown(&handle);

    // An agent with Claude Code's mode names: its elevated ones are never offered either.
    let sb = Sandbox::new("modes-generic-adapter");
    let cfg = generic(
        &sb,
        &json!({"_options": {"modes": "adapter"}, "default": [{"say": "ok"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    let (modes, mode) = ready_modes(&expect_ready(&handle));
    assert_eq!(modes, ["default", "plan"]);
    assert_eq!(mode, "default");
    shutdown(&handle);
}

#[test]
fn other_agents_get_the_instructions_in_their_first_prompt() {
    let sb = Sandbox::new("instructions");
    let cfg = generic(
        &sb,
        &json!({"_options": {"mcp": "skip"}, "default": [{"echo_prompt": true}],
                "lemma": [{"say": "the keyword came from the instructions"}]}),
    );
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    let first = |text: &str| -> serde_json::Value {
        prompt(&handle, text);
        let events = events_until(&handle, SLOW, is_turn_end);
        assert!(errors(&events).is_empty(), "{events:#?}");
        serde_json::from_str(&message_text(&events)).expect("the echoed prompt")
    };
    let echoed = first("hello");
    let blocks = echoed["prompt"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "{echoed}");
    assert_eq!(
        blocks[0]["text"],
        format!(
            "{}\nYou are working inside Lemma (test).\n</lemma_instructions>",
            lemma_agent::INSTRUCTIONS_TAG
        )
    );
    assert_eq!(blocks[1]["text"], "hello");
    // Only the first prompt carries them.
    let echoed = first("again");
    assert_eq!(echoed["prompt"].as_array().unwrap().len(), 1, "{echoed}");
    shutdown(&handle);

    // Claude Code gets them in `session/new` only.
    let sb = Sandbox::new("instructions-claude");
    let scenario =
        sb.scenario(&json!({"_options": {"mcp": "skip"}, "default": [{"echo_prompt": true}]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "hello");
    let events = events_until(&handle, SLOW, is_turn_end);
    let echoed: serde_json::Value = serde_json::from_str(&message_text(&events)).unwrap();
    assert_eq!(echoed["prompt"].as_array().unwrap().len(), 1, "{echoed}");
    shutdown(&handle);
}
